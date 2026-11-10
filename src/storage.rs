//! Authoritative checksummed commit log and disk-backed statement/transaction views.
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::disk_index::{DiskIndex, Entries, Entry};
use crate::read_batch::{OffsetBatch, WalRead};
use crate::types::{DataType, Schema, Value};
use crate::{Error, Result};

pub(crate) type Catalog = BTreeMap<String, Schema>;
const HEADER: &[u8; 8] = b"MVTX0001";
const COMMIT: &[u8; 8] = b"COMMIT01";
const FNV: u64 = 0xcbf2_9ce4_8422_2325;
fn checksum(mut hash: u64, buf: &[u8]) -> u64 {
    for b in buf {
        hash = (hash ^ u64::from(*b)).wrapping_mul(0x100_0000_01b3);
    }
    hash
}
fn corrupt(message: &str) -> Error {
    Error::new("storage_error", message)
}

#[derive(Debug)]
pub(crate) enum Op {
    Schema(Schema),
    Put {
        table: String,
        key: i64,
        row: Vec<Value>,
    },
    Delete {
        table: String,
        key: i64,
    },
}
fn number(out: &mut Vec<u8>, n: usize) -> Result<()> {
    out.extend_from_slice(
        &u32::try_from(n)
            .map_err(|_| Error::new("resource_limit", "record exceeds format limit"))?
            .to_le_bytes(),
    );
    Ok(())
}
fn string(out: &mut Vec<u8>, s: &str) -> Result<()> {
    number(out, s.len())?;
    out.extend_from_slice(s.as_bytes());
    Ok(())
}
fn u32read(r: &mut impl Read) -> Result<u32> {
    let mut b = [0; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}
fn i64read(r: &mut impl Read) -> Result<i64> {
    let mut b = [0; 8];
    r.read_exact(&mut b)?;
    Ok(i64::from_le_bytes(b))
}
fn take<'a>(r: &mut &'a [u8], bytes: usize) -> Result<&'a [u8]> {
    let (field, rest) = r
        .split_at_checked(bytes)
        .ok_or_else(|| corrupt("truncated field"))?;
    *r = rest;
    Ok(field)
}
fn sized_bytes<'a>(r: &mut &'a [u8], item_bytes: usize) -> Result<&'a [u8]> {
    let count = usize::try_from(u32read(r)?).map_err(|_| corrupt("invalid field length"))?;
    let bytes = count
        .checked_mul(item_bytes)
        .ok_or_else(|| corrupt("field size overflow"))?;
    take(r, bytes)
}
fn decode_text(bytes: &[u8]) -> Result<String> {
    String::from_utf8(bytes.to_vec()).map_err(|_| corrupt("invalid UTF-8"))
}
fn encode(op: &Op) -> Result<Vec<u8>> {
    let mut b = Vec::new();
    match op {
        Op::Schema(schema) => {
            b.push(0);
            b.extend(serde_json::to_vec(schema)?);
        }
        Op::Put { table, key, row } => {
            b.push(1);
            string(&mut b, table)?;
            b.extend_from_slice(&key.to_le_bytes());
            number(&mut b, row.len())?;
            for value in row {
                match value {
                    Value::Int(x) => {
                        b.push(0);
                        b.extend_from_slice(&x.to_le_bytes());
                    }
                    Value::Text(x) => {
                        b.push(1);
                        string(&mut b, x)?;
                    }
                    Value::Vector(x) => {
                        b.push(2);
                        number(&mut b, x.len())?;
                        for f in x {
                            b.extend_from_slice(&f.to_le_bytes());
                        }
                    }
                }
            }
        }
        Op::Delete { table, key } => {
            b.push(2);
            string(&mut b, table)?;
            b.extend_from_slice(&key.to_le_bytes());
        }
    }
    let mut out = Vec::with_capacity(b.len() + 4);
    number(&mut out, b.len())?;
    out.extend(b);
    Ok(out)
}
struct Record {
    op: Op,
    vector_at: Option<u64>,
}

fn read_body(r: &mut impl Read, data: &mut Vec<u8>) -> Result<bool> {
    let mut length = [0; 4];
    let read = r.read(&mut length[..1])?;
    if read == 0 {
        return Ok(false);
    }
    r.read_exact(&mut length[1..])?;
    let len = usize::try_from(u32::from_le_bytes(length))
        .map_err(|_| corrupt("invalid record length"))?;
    // One row may exceed a sort run, but no decoder allocates the entire log.
    data.resize(len, 0);
    r.read_exact(data)?;
    Ok(true)
}

pub(crate) enum Field<'a> {
    Int(&'a [u8; 8]),
    Text(&'a [u8]),
    Vector(&'a [[u8; 4]]),
}
impl Field<'_> {
    fn equals(&self, value: &Value) -> bool {
        match (self, value) {
            (Self::Int(bytes), Value::Int(n)) => i64::from_le_bytes(**bytes) == *n,
            (Self::Text(bytes), Value::Text(s)) => *bytes == s.as_bytes(),
            _ => false,
        }
    }
}

fn read_field<'a>(r: &mut &'a [u8]) -> Result<Field<'a>> {
    let [tag] = *take(r, 1)? else {
        unreachable!("read exactly one tag byte");
    };
    match tag {
        0 => Ok(Field::Int(take(r, 8)?.first_chunk().unwrap())),
        1 => Ok(Field::Text(sized_bytes(r, 1)?)),
        2 => Ok(Field::Vector(sized_bytes(r, 4)?.as_chunks::<4>().0)),
        _ => Err(corrupt("invalid value tag")),
    }
}
fn put_header<'a>(r: &mut &'a [u8]) -> Result<(&'a [u8], i64, u32)> {
    Ok((sized_bytes(r, 1)?, i64read(r)?, u32read(r)?))
}
fn decode_vector(bytes: &[[u8; 4]]) -> Vec<f32> {
    bytes.iter().map(|&b| f32::from_le_bytes(b)).collect()
}
fn decode_values(mut r: &[u8], count: u32) -> Result<(Vec<Value>, Option<usize>)> {
    let bytes = r.len();
    let mut row = Vec::new();
    let mut vector_at = None;
    for _ in 0..count {
        row.push(match read_field(&mut r)? {
            Field::Int(b) => Value::Int(i64::from_le_bytes(*b)),
            Field::Text(b) => Value::Text(decode_text(b)?),
            Field::Vector(b) => {
                vector_at = Some(bytes - r.len() - std::mem::size_of_val(b));
                Value::Vector(decode_vector(b))
            }
        });
    }
    if !r.is_empty() {
        return Err(corrupt("record has trailing bytes"));
    }
    Ok((row, vector_at))
}

fn decode_record(data: &[u8]) -> Result<Record> {
    let (&tag, mut r) = data.split_first().ok_or_else(|| corrupt("empty record"))?;
    let mut vector_at = None;
    let op = match tag {
        0 => Op::Schema(serde_json::from_slice(r).map_err(|_| corrupt("invalid schema record"))?),
        1 => {
            let (table, key, count) = put_header(&mut r)?;
            let table = decode_text(table)?;
            let (row, offset) = decode_values(r, count)?;
            // Include the length prefix outside the record body.
            vector_at = offset.map(|i| 4 + (data.len() - r.len() + i) as u64);
            Op::Put { table, key, row }
        }
        2 => {
            let table = decode_text(sized_bytes(&mut r, 1)?)?;
            let key = i64read(&mut r)?;
            if !r.is_empty() {
                return Err(corrupt("record has trailing bytes"));
            }
            Op::Delete { table, key }
        }
        _ => return Err(corrupt("invalid operation tag")),
    };
    Ok(Record { op, vector_at })
}

fn read_record(r: &mut impl Read) -> Result<Option<Record>> {
    let mut data = Vec::new();
    if !read_body(r, &mut data)? {
        return Ok(None);
    }
    decode_record(&data).map(Some)
}

fn read_op(r: &mut impl Read) -> Result<Option<Op>> {
    Ok(read_record(r)?.map(|record| record.op))
}

fn decode_matching_row(data: &[u8], equalities: &[(usize, &Value)]) -> Result<Option<Vec<Value>>> {
    let mut fields = data
        .strip_prefix(&[1])
        .ok_or_else(|| corrupt("index points to non-row"))?;
    let (_, _, count) = put_header(&mut fields)?;
    let mut rest = fields;
    let mut tests = equalities.iter().peekable();
    // Tests are in column order. Skipping a field inspects only its tag and length.
    for column in 0..count as usize {
        let field = read_field(&mut rest)?;
        while let Some(&&(index, value)) = tests.peek()
            && index == column
        {
            if !field.equals(value) {
                return Ok(None);
            }
            tests.next();
        }
        if tests.peek().is_none() {
            return decode_values(fields, count).map(|(values, _)| Some(values));
        }
    }
    Err(corrupt("filter column is outside row"))
}

pub(crate) struct Changes {
    pub catalog: Catalog,
    path: PathBuf,
    file: File,
    indexes: BTreeMap<String, DiskIndex>,
    root: PathBuf,
}
impl Changes {
    fn new(root: PathBuf, catalog: Catalog) -> Result<Self> {
        let path = root.with_extension("stage");
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)?;
        Ok(Self {
            catalog,
            path,
            file,
            indexes: BTreeMap::new(),
            root,
        })
    }
    fn apply(&mut self, op: &Op, offset: u64) -> Result<()> {
        match op {
            Op::Schema(schema) => {
                self.catalog.insert(schema.name.clone(), schema.clone());
            }
            Op::Put { table, key, .. } | Op::Delete { table, key } => {
                if !self.indexes.contains_key(table) {
                    let path = self
                        .root
                        .with_extension(format!("{}.hash", self.indexes.len()));
                    self.indexes.insert(table.clone(), DiskIndex::create(path)?);
                }
                self.indexes.get_mut(table).unwrap().put(Entry {
                    key: *key,
                    offset,
                    deleted: matches!(op, Op::Delete { .. }),
                })?;
            }
        }
        Ok(())
    }
    pub fn push(&mut self, op: Op) -> Result<()> {
        let offset = self.file.stream_position()?;
        self.file.write_all(&encode(&op)?)?;
        self.apply(&op, offset)
    }
    pub fn get(&mut self, table: &str, key: i64) -> Result<Option<Entry>> {
        self.indexes
            .get_mut(table)
            .map_or(Ok(None), |index| index.get(key))
    }
    pub fn merge(&mut self, other: &Changes) -> Result<()> {
        let mut reader = BufReader::new(File::open(&other.path)?);
        while let Some(op) = read_op(&mut reader)? {
            self.push(op)?;
        }
        Ok(())
    }
}
impl Drop for Changes {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        for index in self.indexes.values() {
            let _ = fs::remove_file(index.path());
        }
    }
}

pub(crate) struct Store {
    pub catalog: Catalog,
    indexes: BTreeMap<String, DiskIndex>,
    vectors: BTreeMap<String, DiskIndex>,
    wal: File,
    path: PathBuf,
    scratch: PathBuf,
    sequence: u64,
    _lock: File,
}
impl Store {
    pub fn open(data: &Path) -> Result<Self> {
        fs::create_dir_all(data)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(data.join("minivectordb.lock"))?;
        lock.try_lock().map_err(|error| {
            Error::new(
                "storage_error",
                format!("cannot lock data directory: {error}"),
            )
        })?;
        let scratch = data.join("scratch");
        fs::create_dir_all(&scratch)?;
        // scratch is derived state exclusively owned by this format, never user data.
        for file in fs::read_dir(&scratch)? {
            let file = file?;
            if file.file_name().to_string_lossy().starts_with("mv-") && file.file_type()?.is_file()
            {
                fs::remove_file(file.path())?;
            }
        }
        let path = data.join("commits.mv1");
        let wal = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        #[cfg(unix)]
        File::open(data)?.sync_all()?;
        let mut store = Self {
            catalog: Catalog::new(),
            indexes: BTreeMap::new(),
            vectors: BTreeMap::new(),
            wal,
            path,
            scratch,
            sequence: 0,
            _lock: lock,
        };
        store.recover()?;
        Ok(store)
    }
    pub fn temp(&mut self, suffix: &str) -> PathBuf {
        self.sequence += 1;
        self.scratch.join(format!("mv-{}.{suffix}", self.sequence))
    }
    pub fn changes(&mut self, catalog: Catalog) -> Result<Changes> {
        let root = self.temp("base");
        Changes::new(root, catalog)
    }
    pub fn get(&mut self, table: &str, key: i64) -> Result<Option<Entry>> {
        self.indexes.get_mut(table).map_or(Ok(None), |x| x.get(key))
    }
    fn apply(&mut self, record: &Record, offset: u64) -> Result<()> {
        match &record.op {
            Op::Schema(schema) => {
                if !self.indexes.contains_key(&schema.name) {
                    let path = self.temp("hash");
                    self.indexes
                        .insert(schema.name.clone(), DiskIndex::create(path)?);
                }
                if !schema.indexes.is_empty() && !self.vectors.contains_key(&schema.name) {
                    self.build_vector_index(&schema.name)?;
                }
                self.catalog.insert(schema.name.clone(), schema.clone());
            }
            Op::Put { table, key, .. } | Op::Delete { table, key } => {
                let deleted = matches!(record.op, Op::Delete { .. });
                self.indexes
                    .get_mut(table)
                    .ok_or_else(|| corrupt("row table missing"))?
                    .put(Entry {
                        key: *key,
                        offset,
                        deleted,
                    })?;
                if let Some(index) = self.vectors.get_mut(table) {
                    let vector = if deleted {
                        0
                    } else {
                        offset
                            + record
                                .vector_at
                                .ok_or_else(|| corrupt("indexed row has no vector"))?
                    };
                    index.put(Entry {
                        key: *key,
                        offset: vector,
                        deleted,
                    })?;
                }
            }
        }
        Ok(())
    }
    fn build_vector_index(&mut self, table: &str) -> Result<()> {
        // A flat index points directly into committed binary vector payloads.
        // It can be rebuilt from the same row versions as the primary index.
        let mut index = DiskIndex::create(self.temp("vectors"))?;
        let mut log = File::open(&self.path)?;
        for slot in self.indexes[table].entries()? {
            let slot = slot?;
            if slot.deleted {
                continue;
            }
            log.seek(SeekFrom::Start(slot.offset))?;
            let vector_at = read_record(&mut log)?
                .and_then(|record| record.vector_at)
                .ok_or_else(|| corrupt("indexed row has no vector"))?;
            index.put(Entry {
                offset: slot.offset + vector_at,
                ..slot
            })?;
        }
        self.vectors.insert(table.to_owned(), index);
        Ok(())
    }
    fn replay(&mut self, start: u64, len: u64) -> Result<()> {
        let mut file = BufReader::new(File::open(&self.path)?);
        file.seek(SeekFrom::Start(start))?;
        let end = start + len;
        while file.stream_position()? < end {
            let offset = file.stream_position()?;
            let record = read_record(&mut file)?.ok_or_else(|| corrupt("missing operation"))?;
            if file.stream_position()? > end {
                return Err(corrupt("operation crosses commit boundary"));
            }
            self.apply(&record, offset)?;
        }
        Ok(())
    }
    fn recover(&mut self) -> Result<()> {
        let size = self.wal.metadata()?.len();
        let mut pos = 0;
        let mut reader = BufReader::new(File::open(&self.path)?);
        let mut buffer = [0; 65536];
        while size - pos >= 16 {
            reader.seek(SeekFrom::Start(pos))?;
            let mut header = [0; 16];
            reader.read_exact(&mut header)?;
            if &header[..8] != HEADER {
                return Err(corrupt("invalid commit header"));
            }
            let len = u64::from_le_bytes(header[8..16].try_into().unwrap());
            let Some(end) = pos.checked_add(32).and_then(|p| p.checked_add(len)) else {
                return Err(corrupt("commit length overflow"));
            };
            if end > size {
                break;
            }
            let mut left = len;
            let mut hash = FNV;
            while left > 0 {
                let n = usize::try_from(left.min(65536)).unwrap();
                reader.read_exact(&mut buffer[..n])?;
                hash = checksum(hash, &buffer[..n]);
                left -= u64::try_from(n).unwrap();
            }
            let mut footer = [0; 16];
            reader.read_exact(&mut footer)?;
            if &footer[..8] != COMMIT || u64::from_le_bytes(footer[8..].try_into().unwrap()) != hash
            {
                return Err(corrupt("committed frame checksum mismatch"));
            }
            self.replay(pos + 16, len)?;
            pos = end;
        }
        if pos != size {
            self.wal.set_len(pos)?;
            self.wal.sync_all()?;
        }
        self.wal.seek(SeekFrom::End(0))?;
        Ok(())
    }
    pub fn commit(&mut self, changes: &Changes) -> Result<()> {
        let len = changes.file.metadata()?.len();
        if len == 0 {
            return Ok(());
        }
        let pos = self.wal.seek(SeekFrom::End(0))?;
        self.wal.write_all(HEADER)?;
        self.wal.write_all(&len.to_le_bytes())?;
        let mut reader = File::open(&changes.path)?;
        let mut buffer = [0; 65536];
        let mut hash = FNV;
        loop {
            let n = reader.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hash = checksum(hash, &buffer[..n]);
            self.wal.write_all(&buffer[..n])?;
        }
        self.wal.write_all(COMMIT)?;
        self.wal.write_all(&hash.to_le_bytes())?;
        // This is the only commit acknowledgement gate. Derived indexes need no fsync.
        self.wal.sync_all()?;
        self.replay(pos + 16, len)
    }
    pub fn view(&self, table: &str, transaction: Option<&Changes>) -> Result<View> {
        let committed = self.indexes.get(table);
        let overlay = transaction.and_then(|tx| tx.indexes.get(table));
        Ok(View {
            base: committed
                .map(DiskIndex::entries)
                .transpose()?
                .map(OffsetBatch::new),
            base_lookup: committed.map(DiskIndex::reader).transpose()?,
            overlay: overlay.map(DiskIndex::reader).transpose()?,
            extra: overlay.map(DiskIndex::entries).transpose()?,
            wal: WalRead::Direct(File::open(&self.path)?),
            staged: transaction
                .map(|tx| File::open(&tx.path).map(WalRead::Direct))
                .transpose()?,
        })
    }
    pub fn vector_view(&self, schema: &Schema, column: usize) -> Result<VectorView> {
        let DataType::Vector(dim) = schema.columns[column].kind else {
            return Err(corrupt("vector index does not refer to a vector column"));
        };
        let index = self
            .vectors
            .get(&schema.name)
            .ok_or_else(|| corrupt("vector index missing"))?;
        let bytes = dim
            .checked_mul(4)
            .ok_or_else(|| corrupt("vector size overflow"))?;
        Ok(VectorView {
            slots: OffsetBatch::new(index.entries()?),
            log: WalRead::Direct(File::open(&self.path)?),
            bytes,
            buffer: Vec::new(),
        })
    }
}

pub(crate) struct VectorView {
    slots: OffsetBatch<Entries>,
    log: WalRead,
    bytes: usize,
    buffer: Vec<u8>,
}

impl VectorView {
    pub fn batched(mut self) -> Self {
        self.slots = self.slots.ordered();
        self.log = self.log.buffered();
        self
    }

    pub fn read_at(&mut self, offset: u64) -> Result<Vec<f32>> {
        self.log.seek(SeekFrom::Start(offset))?;
        self.buffer.resize(self.bytes, 0);
        self.log.read_exact(&mut self.buffer)?;
        Ok(decode_vector(self.buffer.as_chunks::<4>().0))
    }
}

impl Iterator for VectorView {
    type Item = Result<(i64, u64, Vec<f32>)>;

    fn next(&mut self) -> Option<Self::Item> {
        let entry = loop {
            match self.slots.next()? {
                Ok(entry) if !entry.deleted => break entry,
                Ok(_) => continue,
                Err(error) => return Some(Err(error)),
            }
        };
        self.log.prefetch(self.slots.nearby(entry.offset));
        Some(
            self.read_at(entry.offset)
                .map(|values| (entry.key, entry.offset, values)),
        )
    }
}

pub(crate) struct View {
    base: Option<OffsetBatch<Entries>>,
    base_lookup: Option<DiskIndex>,
    overlay: Option<DiskIndex>,
    extra: Option<Entries>,
    wal: WalRead,
    staged: Option<WalRead>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) enum Position {
    Committed(u64),
    Staged(u64),
    Vector(u64),
}

pub(crate) struct LocatedRow {
    pub key: i64,
    pub values: Vec<Value>,
    pub position: Position,
}

pub(crate) enum HotRead<'a> {
    End,
    TooLarge,
    Row {
        key: i64,
        offset: u64,
        fields: RowFields<'a>,
    },
}

pub(crate) struct RowFields<'a> {
    bytes: &'a [u8],
    count: u32,
}

impl<'a> RowFields<'a> {
    pub fn count(&self) -> usize {
        self.count as usize
    }

    pub fn next_field(&mut self) -> Result<Field<'a>> {
        read_field(&mut self.bytes)
    }
}

impl Iterator for View {
    type Item = Result<LocatedRow>;
    fn next(&mut self) -> Option<Self::Item> {
        self.next_row().transpose()
    }
}
impl View {
    pub fn batched(mut self) -> Self {
        debug_assert!(
            self.staged.is_none(),
            "batching requires committed positions"
        );
        self.base = self.base.map(OffsetBatch::ordered);
        self.wal = self.wal.buffered();
        self
    }

    fn seek_row(&mut self, position: Position) -> Result<&mut WalRead> {
        let (file, offset) = match position {
            Position::Committed(offset) => (&mut self.wal, offset),
            Position::Staged(offset) => (
                self.staged
                    .as_mut()
                    .ok_or_else(|| corrupt("staged file missing"))?,
                offset,
            ),
            Position::Vector(_) => return Err(corrupt("vector location requested from row view")),
        };
        file.seek(SeekFrom::Start(offset))?;
        Ok(file)
    }
    pub fn read_at(&mut self, position: Position) -> Result<Vec<Value>> {
        match read_op(self.seek_row(position)?)? {
            Some(Op::Put { row, .. }) => Ok(row),
            _ => Err(corrupt("index points to non-row")),
        }
    }

    // Called only on a committed view. The caller owns a fixed-size body buffer.
    pub fn next_hot<'a>(&mut self, buffer: &'a mut [u8]) -> Result<HotRead<'a>> {
        let Some((key, position)) = self.next_position()? else {
            return Ok(HotRead::End);
        };
        let Position::Committed(offset) = position else {
            return Err(corrupt("hot scan requires committed positions"));
        };
        let reader = self.seek_row(position)?;
        let mut len = [0; 4];
        reader.read_exact(&mut len)?;
        let Some(body) = buffer.get_mut(..u32::from_le_bytes(len) as usize) else {
            return Ok(HotRead::TooLarge);
        };
        reader.read_exact(body)?;
        let mut bytes = body
            .strip_prefix(&[1])
            .ok_or_else(|| corrupt("index points to non-row"))?;
        let (_, row_key, count) = put_header(&mut bytes)?;
        if row_key != key {
            return Err(corrupt("index points to wrong row"));
        }
        Ok(HotRead::Row {
            key,
            offset,
            fields: RowFields { bytes, count },
        })
    }

    fn next_row(&mut self) -> Result<Option<LocatedRow>> {
        let Some((key, position)) = self.next_position()? else {
            return Ok(None);
        };
        Ok(Some(LocatedRow {
            key,
            position,
            values: self.read_at(position)?,
        }))
    }

    pub fn next_equal(
        &mut self,
        equalities: &[(usize, &Value)],
        buffer: &mut Vec<u8>,
    ) -> Result<Option<LocatedRow>> {
        while let Some((key, position)) = self.next_position()? {
            if !read_body(self.seek_row(position)?, buffer)? {
                return Err(corrupt("index points to missing row"));
            }
            if let Some(values) = decode_matching_row(buffer, equalities)? {
                return Ok(Some(LocatedRow {
                    key,
                    position,
                    values,
                }));
            }
        }
        Ok(None)
    }

    fn next_position(&mut self) -> Result<Option<(i64, Position)>> {
        if let Some(base) = &mut self.base {
            while let Some(entry) = base.next() {
                let mut e = entry?;
                let mut staged = false;
                if let Some(overlay) = &mut self.overlay
                    && let Some(new) = overlay.get(e.key)?
                {
                    e = new;
                    staged = true;
                }
                if e.deleted {
                    continue;
                }
                let position = if staged {
                    Position::Staged(e.offset)
                } else {
                    Position::Committed(e.offset)
                };
                if !staged {
                    self.wal.prefetch(base.nearby(e.offset));
                }
                return Ok(Some((e.key, position)));
            }
            self.base = None;
        }
        if let Some(extra) = &mut self.extra {
            for entry in extra.by_ref() {
                let e = entry?;
                if e.deleted {
                    continue;
                }
                // A committed tombstone still has a slot, but the overlay row replaces it.
                let old = self
                    .base_lookup
                    .as_mut()
                    .map(|b| b.get(e.key))
                    .transpose()?
                    .flatten();
                if old.is_some() {
                    continue;
                }
                return Ok(Some((e.key, Position::Staged(e.offset))));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejected_equality_row_does_not_decode_its_vector() {
        let mut record = encode(&Op::Put {
            table: "docs".into(),
            key: 7,
            row: vec![
                Value::Int(7),
                Value::Text("keep".into()),
                Value::Vector(vec![1.0]),
            ],
        })
        .unwrap();
        record.pop(); // A vector decode would encounter a truncated component.
        let body = &record[4..];

        assert!(
            decode_matching_row(body, &[(1, &Value::Text("other".into()))])
                .unwrap()
                .is_none()
        );
        assert!(decode_matching_row(body, &[(1, &Value::Text("keep".into()))]).is_err());
    }
}
