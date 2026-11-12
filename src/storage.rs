//! Authoritative checksummed commit log and disk-backed statement/transaction views.
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

use crate::commit_block::{self, Block, RestoredRow};
use crate::committed_cache::{self, CommittedCache, CommittedView};
use crate::disk_index::{DiskIndex, Entries, Entry};
use crate::position_index::{PositionBudget, PositionEntries, PositionIndex, PositionReader};
use crate::read_batch::{OffsetBatch, WalRead};
use crate::stage::{Budget, Reservation, Stage};
use crate::types::{DataType, Schema, Value};
use crate::vector_codec::{self, HEADER_BYTES, Header as PackedHeader};
use crate::{Error, Result};

pub(crate) type Catalog = BTreeMap<String, Schema>;
const HEADER: &[u8; 8] = b"MVTX0001";
const COMMIT: &[u8; 8] = b"COMMIT01";
const BLOCK_HEADER: &[u8; 8] = b"MVTX0002";
const BLOCK_COMMIT: &[u8; 8] = b"COMMIT02";
const ALIGNED_HEADER: &[u8; 8] = b"MVTX0003";
const ALIGNED_COMMIT: &[u8; 8] = b"COMMIT03";
const WAL_ALIGNMENT: u64 = 4096;
static ZERO_PADDING: [u8; WAL_ALIGNMENT as usize] = [0; WAL_ALIGNMENT as usize];
// Excludes the input, which is either an already charged stage or a separately
// reserved disk read. Includes output, descriptors, dictionaries and stack scratch.
const BLOCK_ENCODE_WORKSPACE: usize = 576 << 10;
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

#[derive(Clone, Copy)]
enum CommitFormat {
    Legacy,
    Compact,
    Aligned,
}

impl CommitFormat {
    fn read(header: &[u8]) -> Result<Self> {
        match header {
            bytes if bytes == HEADER => Ok(Self::Legacy),
            bytes if bytes == BLOCK_HEADER => Ok(Self::Compact),
            bytes if bytes == ALIGNED_HEADER => Ok(Self::Aligned),
            _ => Err(corrupt("invalid commit header")),
        }
    }

    fn header(self) -> &'static [u8; 8] {
        match self {
            Self::Legacy => HEADER,
            Self::Compact => BLOCK_HEADER,
            Self::Aligned => ALIGNED_HEADER,
        }
    }

    fn footer(self) -> &'static [u8; 8] {
        match self {
            Self::Legacy => COMMIT,
            Self::Compact => BLOCK_COMMIT,
            Self::Aligned => ALIGNED_COMMIT,
        }
    }

    fn compact(self) -> bool {
        matches!(self, Self::Compact | Self::Aligned)
    }

    // A version 3 frame is header, original compact payload, zero padding,
    // then footer. The header length still counts only the original payload.
    fn extent(self, start: u64, len: u64) -> Result<(u64, usize)> {
        let end = start
            .checked_add(32)
            .and_then(|pos| pos.checked_add(len))
            .ok_or_else(|| corrupt("commit length overflow"))?;
        if matches!(self, Self::Aligned) {
            let aligned = end
                .checked_next_multiple_of(WAL_ALIGNMENT)
                .ok_or_else(|| corrupt("aligned commit length overflow"))?;
            Ok((aligned, (aligned - end) as usize))
        } else {
            Ok((end, 0))
        }
    }

    fn for_compact(start: u64, len: u64) -> Result<Self> {
        let (_, padding) = Self::Aligned.extent(start, len)?;
        // Cap physical layout overhead for small or incompressible frames.
        if padding != 0 && padding as u64 <= (len + 32) / 8 {
            Ok(Self::Aligned)
        } else {
            Ok(Self::Compact)
        }
    }
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
fn record_limit() -> Error {
    Error::new("resource_limit", "record exceeds format limit")
}

fn number(out: &mut impl Write, n: usize) -> Result<()> {
    out.write_all(&u32::try_from(n).map_err(|_| record_limit())?.to_le_bytes())?;
    Ok(())
}

fn string(out: &mut impl Write, s: &str) -> Result<()> {
    number(out, s.len())?;
    out.write_all(s.as_bytes())?;
    Ok(())
}

struct CountBytes(usize);

impl Write for CountBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.checked_add(bytes.len()).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "schema size overflow")
        })?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
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
struct RecordLayout {
    body_size: u32,
    packed: Option<PackedHeader>,
}

impl RecordLayout {
    fn new(op: &Op) -> Result<Self> {
        let mut packed = None;
        let body_size = match op {
            Op::Schema(schema) => {
                let mut bytes = CountBytes(1);
                serde_json::to_writer(&mut bytes, schema)?;
                bytes.0
            }
            Op::Put { table, row, .. } => {
                let mut bytes = 17 + table.len();
                let mut seen_vector = false;
                for value in row {
                    let field = match value {
                        Value::Int(_) => 9,
                        Value::Text(text) => 5 + text.len(),
                        Value::Vector(vector) => {
                            if seen_vector {
                                return Err(corrupt("row has multiple vector fields"));
                            }
                            seen_vector = true;
                            packed = vector_codec::packed_header(vector);
                            5 + if let Some(header) = packed {
                                HEADER_BYTES + header.payload_len(vector.len())?
                            } else {
                                vector.len() * 4
                            }
                        }
                    };
                    bytes = bytes.checked_add(field).ok_or_else(record_limit)?;
                }
                bytes
            }
            Op::Delete { table, .. } => 13 + table.len(),
        };
        let body_size = u32::try_from(body_size).map_err(|_| record_limit())?;
        Ok(Self { body_size, packed })
    }

    fn size(&self) -> Result<usize> {
        (self.body_size as usize)
            .checked_add(4)
            .ok_or_else(record_limit)
    }

    fn write(&self, op: &Op, out: &mut impl Write) -> Result<()> {
        out.write_all(&self.body_size.to_le_bytes())?;
        match op {
            Op::Schema(schema) => {
                out.write_all(&[0])?;
                serde_json::to_writer(out, schema).map_err(std::io::Error::from)?;
            }
            Op::Put { table, key, row } => {
                out.write_all(&[1])?;
                string(out, table)?;
                out.write_all(&key.to_le_bytes())?;
                number(out, row.len())?;
                for value in row {
                    match value {
                        Value::Int(x) => {
                            out.write_all(&[0])?;
                            out.write_all(&x.to_le_bytes())?;
                        }
                        Value::Text(x) => {
                            out.write_all(&[1])?;
                            string(out, x)?;
                        }
                        Value::Vector(x) => {
                            out.write_all(&[if self.packed.is_some() { 3 } else { 2 }])?;
                            number(out, x.len())?;
                            if let Some(header) = self.packed {
                                vector_codec::write_packed(header, x, out)?;
                            } else {
                                write_raw_vector(x, out)?;
                            }
                        }
                    }
                }
            }
            Op::Delete { table, key } => {
                out.write_all(&[2])?;
                string(out, table)?;
                out.write_all(&key.to_le_bytes())?;
            }
        }
        Ok(())
    }
}

fn write_raw_vector(vector: &[f32], out: &mut impl Write) -> std::io::Result<()> {
    let mut buffer = [0; 1024];
    for values in vector.chunks(buffer.len() / 4) {
        for (slot, value) in buffer.as_chunks_mut::<4>().0.iter_mut().zip(values) {
            *slot = value.to_le_bytes();
        }
        out.write_all(&buffer[..values.len() * 4])?;
    }
    Ok(())
}

#[cfg(test)]
fn encode(op: &Op) -> Result<Vec<u8>> {
    let layout = RecordLayout::new(op)?;
    let mut out = Vec::with_capacity(layout.size()?);
    layout.write(op, &mut out)?;
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

/// Derived index positions keep physical byte offsets for legacy rows. A compact
/// position reserves the high bit and stores a block offset plus a row ordinal.
#[derive(Clone, Copy)]
struct BlockPosition {
    start: u64,
    row: usize,
}

impl BlockPosition {
    const FLAG: u64 = 1 << 63;
    const ROW_BITS: u32 = 16;
    const MAX_START: u64 = (Self::FLAG - 1) >> Self::ROW_BITS;

    fn unpack(offset: u64) -> Option<Self> {
        (offset & Self::FLAG != 0).then_some(Self {
            start: (offset & !Self::FLAG) >> Self::ROW_BITS,
            row: (offset & u64::from(u16::MAX)) as usize,
        })
    }

    fn pack(self) -> Result<u64> {
        if self.start > Self::MAX_START || self.row > usize::from(u16::MAX) {
            return Err(corrupt("compact row position exceeds format limit"));
        }
        Ok(Self::FLAG | (self.start << Self::ROW_BITS) | self.row as u64)
    }

    fn vector_at(self, row: &RestoredRow) -> Option<u64> {
        row.vector_at.map(|at| self.start + 4 + at as u64)
    }
}

struct BlockReader {
    start: Option<u64>,
    body: Vec<u8>,
    expanded: Vec<u8>,
    budget: Budget,
    body_reservation: Option<Reservation>,
    expanded_reservation: Option<Reservation>,
}

impl BlockReader {
    fn new(budget: Budget) -> Self {
        Self {
            start: None,
            body: Vec::new(),
            expanded: Vec::new(),
            budget,
            body_reservation: None,
            expanded_reservation: None,
        }
    }

    fn reserve(&self) -> Result<Reservation> {
        self.budget
            .reserve(commit_block::TARGET_BYTES)
            .ok_or_else(|| corrupt("compact block read workspace exhausted"))
    }

    fn load(&mut self, file: &mut (impl Read + Seek), start: u64) -> Result<()> {
        if self.start != Some(start) {
            self.start = None;
            file.seek(SeekFrom::Start(start))?;
            let size = u32read(file)? as usize;
            if size == 0 || size > commit_block::TARGET_BYTES {
                return Err(corrupt("compact block size exceeds read limit"));
            }
            if self.body_reservation.is_none() {
                self.body_reservation = Some(self.reserve()?);
                self.body = Vec::with_capacity(commit_block::TARGET_BYTES);
            }
            self.body.resize(size, 0);
            file.read_exact(&mut self.body)?;
            self.start = Some(start);
        }
        Ok(())
    }

    fn restore(
        &mut self,
        file: &mut (impl Read + Seek),
        location: BlockPosition,
        out: &mut [u8],
    ) -> Result<RestoredRow> {
        self.load(file, location.start)?;
        Block::parse(&self.body)?.restore(location.row, out)
    }

    fn read(
        &mut self,
        file: &mut (impl Read + Seek),
        location: BlockPosition,
    ) -> Result<(&[u8], RestoredRow)> {
        self.load(file, location.start)?;
        if self.expanded_reservation.is_none() {
            self.expanded_reservation = Some(self.reserve()?);
            self.expanded.resize(commit_block::TARGET_BYTES, 0);
        }
        let row = Block::parse(&self.body)?.restore(location.row, &mut self.expanded)?;
        Ok((&self.expanded[..row.len], row))
    }
}

pub(crate) enum Field<'a> {
    Int(&'a [u8; 8]),
    Text(&'a [u8]),
    Vector(VectorData<'a>),
}
#[derive(Clone, Copy)]
pub(crate) enum VectorData<'a> {
    Raw(&'a [[u8; 4]]),
    Packed {
        header: PackedHeader,
        count: usize,
        payload: &'a [u8],
    },
}
impl VectorData<'_> {
    pub fn len(&self) -> usize {
        match self {
            Self::Raw(bytes) => bytes.len(),
            Self::Packed { count, .. } => *count,
        }
    }
    pub fn append(self, out: &mut Vec<f32>) -> Result<()> {
        match self {
            Self::Raw(bytes) => out.extend(bytes.iter().map(|&b| f32::from_le_bytes(b))),
            Self::Packed {
                header,
                count,
                payload,
            } => header.decode(count, payload, out)?,
        }
        Ok(())
    }
    fn values(self) -> Result<Vec<f32>> {
        let mut out = Vec::with_capacity(self.len());
        self.append(&mut out)?;
        Ok(out)
    }
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
        2 => Ok(Field::Vector(VectorData::Raw(
            sized_bytes(r, 4)?.as_chunks::<4>().0,
        ))),
        3 => {
            let count = u32read(r)? as usize;
            let header = PackedHeader::parse(take(r, HEADER_BYTES)?.first_chunk().unwrap());
            let payload = take(r, header.payload_len(count)?)?;
            Ok(Field::Vector(VectorData::Packed {
                header,
                count,
                payload,
            }))
        }
        _ => Err(corrupt("invalid value tag")),
    }
}
fn put_header<'a>(r: &mut &'a [u8]) -> Result<(&'a [u8], i64, u32)> {
    Ok((sized_bytes(r, 1)?, i64read(r)?, u32read(r)?))
}
fn decode_values(mut r: &[u8], count: u32) -> Result<(Vec<Value>, Option<usize>)> {
    let bytes = r.len();
    let mut row = Vec::new();
    let mut vector_at = None;
    for _ in 0..count {
        let field_at = bytes - r.len();
        row.push(match read_field(&mut r)? {
            Field::Int(b) => Value::Int(i64::from_le_bytes(*b)),
            Field::Text(b) => Value::Text(decode_text(b)?),
            Field::Vector(b) => {
                // Derived index locations include the vector field tag and dimension.
                vector_at = Some(field_at);
                Value::Vector(b.values()?)
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
    stage: Stage,
    indexes: BTreeMap<String, DiskIndex>,
    root: PathBuf,
}
impl Changes {
    fn new(root: PathBuf, catalog: Catalog, budget: Budget) -> Self {
        Self {
            catalog,
            stage: Stage::new(root.with_extension("stage"), budget),
            indexes: BTreeMap::new(),
            root,
        }
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
        let layout = RecordLayout::new(&op)?;
        let offset = self
            .stage
            .append(layout.size()?, |out| layout.write(&op, out))?;
        self.apply(&op, offset)
    }
    pub fn get(&mut self, table: &str, key: i64) -> Result<Option<Entry>> {
        self.indexes
            .get_mut(table)
            .map_or(Ok(None), |index| index.get(key))
    }
    pub fn merge(&mut self, other: Changes) -> Result<()> {
        if other.stage.len() == 0 {
            return Ok(());
        }
        if self.stage.len() == 0 {
            // The file or memory payload, catalog and index offsets move together.
            *self = other;
            return Ok(());
        }
        let mut reader = BufReader::new(other.stage.reader()?);
        while let Some(op) = read_op(&mut reader)? {
            self.push(op)?;
        }
        Ok(())
    }
}
impl Drop for Changes {
    fn drop(&mut self) {
        for index in self.indexes.values() {
            let _ = fs::remove_file(index.path());
        }
    }
}

pub(crate) struct Store {
    pub catalog: Catalog,
    indexes: BTreeMap<String, PositionIndex>,
    vectors: BTreeMap<String, PositionIndex>,
    wal: File,
    committed_end: u64,
    committed_cache: Option<Rc<CommittedCache>>,
    cache_lease: Weak<CommittedCache>,
    committed_limit: usize,
    path: PathBuf,
    scratch: PathBuf,
    sequence: u64,
    stage_budget: Budget,
    position_budget: PositionBudget,
    _lock: Rc<File>,
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
        let stage_budget = Budget::default();
        const MAX_POSITION_BYTES: usize = 192 << 10;
        let position_bytes = std::env::var("MINIVECTORDB_POSITION_BYTES")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(MAX_POSITION_BYTES)
            .min(MAX_POSITION_BYTES);
        let position_budget = PositionBudget::new(stage_budget.clone(), position_bytes);
        let hot_bytes = std::env::var("MINIVECTORDB_HOT_BYTES")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(8 << 20)
            .min(8 << 20);
        let mut store = Self {
            catalog: Catalog::new(),
            indexes: BTreeMap::new(),
            vectors: BTreeMap::new(),
            wal,
            committed_end: 0,
            committed_cache: None,
            cache_lease: Weak::new(),
            committed_limit: std::env::var("MINIVECTORDB_COMMITTED_BYTES")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(committed_cache::MAX_BYTES)
                .min(committed_cache::MAX_BYTES)
                .min(committed_cache::limit_for_hot_budget(hot_bytes)),
            path,
            scratch,
            sequence: 0,
            stage_budget,
            position_budget,
            _lock: Rc::new(lock),
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
        Ok(Changes::new(root, catalog, self.stage_budget.clone()))
    }
    pub fn get(&mut self, table: &str, key: i64) -> Result<Option<Entry>> {
        self.indexes.get_mut(table).map_or(Ok(None), |x| x.get(key))
    }
    fn apply(&mut self, record: &Record, offset: u64, vector_at: Option<u64>) -> Result<()> {
        match &record.op {
            Op::Schema(schema) => {
                if !self.indexes.contains_key(&schema.name) {
                    let path = self.temp("hash");
                    self.indexes.insert(
                        schema.name.clone(),
                        PositionIndex::create(path, self.position_budget.clone())?,
                    );
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
                        vector_at.ok_or_else(|| corrupt("indexed row has no vector"))?
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
        let mut index = PositionIndex::create(self.temp("vectors"), self.position_budget.clone())?;
        let mut log = self.wal_reader()?.buffered();
        let mut blocks = BlockReader::new(self.stage_budget.clone());
        let mut slots = OffsetBatch::new(self.indexes[table].entries()?).ordered();
        while let Some(slot) = slots.next() {
            let slot = slot?;
            if slot.deleted {
                continue;
            }
            let vector_at = if let Some(location) = BlockPosition::unpack(slot.offset) {
                let (_, row) = blocks.read(&mut log, location)?;
                location.vector_at(&row)
            } else {
                log.prefetch(slots.nearby(slot.offset));
                log.seek(SeekFrom::Start(slot.offset))?;
                read_record(&mut log)?
                    .and_then(|record| record.vector_at)
                    .map(|at| slot.offset + at)
            }
            .ok_or_else(|| corrupt("indexed row has no vector"))?;
            index.put(Entry {
                offset: vector_at,
                ..slot
            })?;
        }
        self.vectors.insert(table.to_owned(), index);
        Ok(())
    }
    fn replay(&mut self, start: u64, len: u64, compact: bool) -> Result<()> {
        let mut file = BufReader::new(File::open(&self.path)?);
        file.seek(SeekFrom::Start(start))?;
        let end = start + len;
        let mut offset = start;
        let _compact_reservation = if compact {
            Some(
                self.stage_budget
                    .reserve(commit_block::MAX_INPUT_BYTES + commit_block::TARGET_BYTES)
                    .ok_or_else(|| corrupt("compact replay workspace exhausted"))?,
            )
        } else {
            None
        };
        let mut body = if compact {
            Vec::with_capacity(commit_block::MAX_INPUT_BYTES)
        } else {
            Vec::new()
        };
        let mut expanded = Vec::new();
        while offset < end {
            if end - offset < 4 {
                return Err(corrupt("operation crosses commit boundary"));
            }
            let size = u32read(&mut file)?;
            if u64::from(size) > end - offset - 4 {
                return Err(corrupt("operation crosses commit boundary"));
            }
            let next = offset + 4 + u64::from(size);
            if compact && size as usize > commit_block::MAX_INPUT_BYTES {
                return Err(corrupt("record exceeds compact commit limit"));
            }
            body.resize(size as usize, 0);
            file.read_exact(&mut body)?;
            if compact && body.first() == Some(&commit_block::TAG) {
                let block = Block::parse(&body)?;
                let schema = self
                    .catalog
                    .get(block.table()?)
                    .filter(|schema| block.matches_schema(schema))
                    .ok_or_else(|| corrupt("commit block layout differs from table"))?;
                let vector = schema
                    .columns
                    .iter()
                    .enumerate()
                    .find_map(|(index, column)| {
                        if let DataType::Vector(dim) = column.kind {
                            Some((index, dim))
                        } else {
                            None
                        }
                    });
                expanded.resize(commit_block::TARGET_BYTES, 0);
                let mut expanded_bytes = 0;
                for row in 0..block.len() {
                    let location = BlockPosition { start: offset, row };
                    let decoded = block.restore(row, &mut expanded)?;
                    expanded_bytes += 4 + decoded.len;
                    if expanded_bytes > commit_block::TARGET_BYTES {
                        return Err(corrupt("expanded commit block exceeds limit"));
                    }
                    let record = decode_record(&expanded[..decoded.len])?;
                    if let Some((index, dim)) = vector
                        && let Op::Put { row, .. } = &record.op
                        && !matches!(&row[index], Value::Vector(values)
                            if values.len() == dim && values.iter().all(|value| value.is_finite()))
                    {
                        return Err(corrupt("commit block vector differs from table"));
                    }
                    self.apply(&record, location.pack()?, location.vector_at(&decoded))?;
                }
            } else {
                let record = decode_record(&body)?;
                self.apply(&record, offset, record.vector_at.map(|at| offset + at))?;
            }
            offset = next;
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
            let format = CommitFormat::read(&header[..8])?;
            let len = u64::from_le_bytes(header[8..16].try_into().unwrap());
            let (end, padding) = format.extent(pos, len)?;
            if end > size {
                break;
            }
            if format.compact() && len > commit_block::MAX_INPUT_BYTES as u64 {
                return Err(corrupt("compact commit exceeds format limit"));
            }
            let mut left = len;
            let mut hash = FNV;
            while left > 0 {
                let n = usize::try_from(left.min(65536)).unwrap();
                reader.read_exact(&mut buffer[..n])?;
                hash = checksum(hash, &buffer[..n]);
                left -= u64::try_from(n).unwrap();
            }
            reader.read_exact(&mut buffer[..padding])?;
            if buffer[..padding].iter().any(|&byte| byte != 0) {
                return Err(corrupt("nonzero aligned commit padding"));
            }
            let mut footer = [0; 16];
            reader.read_exact(&mut footer)?;
            if &footer[..8] != format.footer()
                || u64::from_le_bytes(footer[8..].try_into().unwrap()) != hash
            {
                return Err(corrupt("committed frame checksum mismatch"));
            }
            self.replay(pos + 16, len, format.compact())?;
            pos = end;
        }
        if pos != size {
            self.wal.set_len(pos)?;
            self.wal.sync_all()?;
        }
        self.wal.seek(SeekFrom::End(0))?;
        self.committed_end = pos;
        Ok(())
    }
    pub fn commit(&mut self, changes: &Changes) -> Result<()> {
        let mut len = changes.stage.len();
        if len == 0 {
            return Ok(());
        }
        self.discard_cached();
        let pos = self.wal.seek(SeekFrom::End(0))?;
        let staged = changes.stage.bytes();
        let positions = self.position_budget.lend_unused();
        let reservation = if len <= commit_block::MAX_INPUT_BYTES as u64
            && pos <= BlockPosition::MAX_START - (len + 32)
        {
            let input_bytes = if staged.is_some() { 0 } else { len as usize };
            self.stage_budget
                .reserve(BLOCK_ENCODE_WORKSPACE + input_bytes)
        } else {
            None
        };
        let mut format = CommitFormat::Legacy;
        let hash = if let Some(_reservation) = reservation {
            let disk_input;
            let input = if let Some(bytes) = staged {
                bytes
            } else {
                let mut bytes = vec![0; len as usize];
                changes.stage.reader()?.read_exact(&mut bytes)?;
                disk_input = bytes;
                &disk_input
            };
            let payload = commit_block::pack_payload(input, |table| {
                changes.catalog.get(table).map(|schema| schema.primary)
            })?;
            if let Some(payload) = &payload {
                format = CommitFormat::for_compact(pos, payload.len() as u64)?;
            }
            let payload = payload.as_deref().unwrap_or(input);
            len = payload.len() as u64;
            self.wal.write_all(format.header())?;
            self.wal.write_all(&len.to_le_bytes())?;
            self.wal.write_all(payload)?;
            checksum(FNV, payload)
        } else {
            self.wal.write_all(HEADER)?;
            self.wal.write_all(&len.to_le_bytes())?;
            if let Some(bytes) = staged {
                self.wal.write_all(bytes)?;
                checksum(FNV, bytes)
            } else {
                let mut reader = changes.stage.reader()?;
                let mut buffer = [0; 65536];
                let mut hash = FNV;
                let mut remaining = len;
                while remaining > 0 {
                    let count = remaining.min(buffer.len() as u64) as usize;
                    reader.read_exact(&mut buffer[..count])?;
                    hash = checksum(hash, &buffer[..count]);
                    self.wal.write_all(&buffer[..count])?;
                    remaining -= count as u64;
                }
                hash
            }
        };
        drop(positions);
        let (end, padding) = format.extent(pos, len)?;
        self.wal.write_all(&ZERO_PADDING[..padding])?;
        self.wal.write_all(format.footer())?;
        self.wal.write_all(&hash.to_le_bytes())?;
        // This is the only commit acknowledgement gate. Derived indexes need no fsync.
        self.wal.sync_all()?;
        self.replay(pos + 16, len, format.compact())?;
        self.committed_end = end;
        Ok(())
    }

    /// Only one cache generation may consume budget, including detached readers.
    pub fn prepare_cached(&mut self) -> Result<Option<CommittedView>> {
        if self.committed_cache.is_none() && self.cache_lease.strong_count() == 0 {
            self.committed_cache =
                CommittedCache::refresh(None, &self.wal, self.committed_end, self.committed_limit)?;
            if let Some(cache) = &self.committed_cache {
                self.cache_lease = Rc::downgrade(cache);
            }
        }
        Ok(self.cached_committed())
    }

    pub fn discard_cached(&mut self) {
        self.committed_cache = None;
    }

    pub fn release_cached_pages(&self) {
        if self.committed_cache.is_some() {
            committed_cache::release_file_pages(&self.wal, self.committed_end);
        }
    }

    pub fn committed_len(&self) -> u64 {
        self.committed_end
    }

    /// Convert an index location to the earliest physical byte a row needs.
    pub fn committed_row_start(offset: u64) -> u64 {
        BlockPosition::unpack(offset).map_or(offset, |block| block.start)
    }

    /// Hot construction only visits committed rows, after the WAL has synced.
    /// Advice cannot extend into a partial or uncommitted tail.
    pub fn release_consumed_pages(&self, start: u64, end: u64) {
        committed_cache::release_file_range(&self.wal, start, end.min(self.committed_end));
    }

    fn wal_reader(&self) -> Result<WalRead> {
        Ok(match &self.committed_cache {
            Some(cache) => WalRead::cached(File::open(&self.path)?, CommittedCache::view(cache)),
            None => WalRead::Direct(File::open(&self.path)?),
        })
    }

    pub fn cached_bytes(&self) -> usize {
        self.cache_lease
            .upgrade()
            .map_or(0, |cache| cache.allocated())
    }

    pub fn cached_committed(&self) -> Option<CommittedView> {
        self.committed_cache.as_ref().map(CommittedCache::view)
    }

    pub fn view(&self, table: &str, transaction: Option<&Changes>) -> Result<View> {
        let committed = self.indexes.get(table);
        let overlay = transaction.and_then(|tx| tx.indexes.get(table));
        Ok(View {
            base: committed
                .map(PositionIndex::entries)
                .transpose()?
                .map(OffsetBatch::new),
            base_lookup: committed.map(PositionIndex::reader).transpose()?,
            overlay: overlay.map(DiskIndex::reader).transpose()?,
            extra: overlay.map(DiskIndex::entries).transpose()?,
            wal: self.wal_reader()?,
            blocks: BlockReader::new(self.stage_budget.clone()),
            staged: transaction
                .map(|tx| tx.stage.reader().map(WalRead::Stage))
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
        Ok(VectorView {
            slots: OffsetBatch::new(index.entries()?),
            log: self.wal_reader()?,
            dim,
            buffer: Vec::new(),
        })
    }
}

pub(crate) struct VectorView {
    slots: OffsetBatch<PositionEntries>,
    log: WalRead,
    dim: usize,
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
        let mut field = [0; 5];
        self.log.read_exact(&mut field)?;
        let dim = u32::from_le_bytes(field[1..].try_into().unwrap()) as usize;
        if dim != self.dim {
            return Err(corrupt("vector index dimension differs from schema"));
        }
        match field[0] {
            2 => {
                let bytes = dim
                    .checked_mul(4)
                    .ok_or_else(|| corrupt("vector size overflow"))?;
                self.buffer.resize(bytes, 0);
                self.log.read_exact(&mut self.buffer)?;
                VectorData::Raw(self.buffer.as_chunks::<4>().0).values()
            }
            3 => {
                let mut bytes = [0; HEADER_BYTES];
                self.log.read_exact(&mut bytes)?;
                let header = PackedHeader::parse(&bytes);
                self.buffer.resize(header.payload_len(dim)?, 0);
                self.log.read_exact(&mut self.buffer)?;
                VectorData::Packed {
                    header,
                    count: dim,
                    payload: &self.buffer,
                }
                .values()
            }
            _ => Err(corrupt("vector index points to non-vector")),
        }
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
    base: Option<OffsetBatch<PositionEntries>>,
    base_lookup: Option<PositionReader>,
    overlay: Option<DiskIndex>,
    extra: Option<Entries>,
    wal: WalRead,
    blocks: BlockReader,
    staged: Option<WalRead>,
}

#[derive(Debug, Clone, Copy)]
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
    address: FieldAddress,
}

enum FieldAddress {
    Contiguous(u64),
    Block(Option<u64>),
}

impl<'a> RowFields<'a> {
    pub fn count(&self) -> usize {
        self.count as usize
    }

    pub fn next_field(&mut self) -> Result<(Field<'a>, Option<u64>)> {
        let remaining = self.bytes.len();
        let field = read_field(&mut self.bytes)?;
        let offset = match &mut self.address {
            FieldAddress::Contiguous(next) => {
                let offset = *next;
                *next += (remaining - self.bytes.len()) as u64;
                Some(offset)
            }
            FieldAddress::Block(vector) => *vector,
        };
        let vector_offset = if matches!(field, Field::Vector(_)) {
            offset
        } else {
            None
        };
        Ok((field, vector_offset))
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
        let op = if let Position::Committed(offset) = position
            && let Some(location) = BlockPosition::unpack(offset)
        {
            let (body, _) = self.blocks.read(&mut self.wal, location)?;
            Some(decode_record(body)?.op)
        } else {
            read_op(self.seek_row(position)?)?
        };
        match op {
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
        let (body, block_address) = if let Some(location) = BlockPosition::unpack(offset) {
            let decoded = self.blocks.restore(&mut self.wal, location, buffer)?;
            (
                &buffer[..decoded.len],
                Some(FieldAddress::Block(location.vector_at(&decoded))),
            )
        } else {
            let reader = self.seek_row(position)?;
            let mut len = [0; 4];
            reader.read_exact(&mut len)?;
            let Some(body) = buffer.get_mut(..u32::from_le_bytes(len) as usize) else {
                return Ok(HotRead::TooLarge);
            };
            reader.read_exact(body)?;
            (&*body, None)
        };
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
            fields: RowFields {
                bytes,
                count,
                address: block_address.unwrap_or_else(|| {
                    FieldAddress::Contiguous(offset + 4 + (body.len() - bytes.len()) as u64)
                }),
            },
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
            let body = if let Position::Committed(offset) = position
                && let Some(location) = BlockPosition::unpack(offset)
            {
                self.blocks.read(&mut self.wal, location)?.0
            } else {
                if !read_body(self.seek_row(position)?, buffer)? {
                    return Err(corrupt("index points to missing row"));
                }
                buffer.as_slice()
            };
            if let Some(values) = decode_matching_row(body, equalities)? {
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
    fn aligned_commit_restarts_with_old_frames_and_discards_a_torn_footer() {
        let path = std::env::temp_dir().join(format!(
            "mv-aligned-frame-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut store = Store::open(&path).unwrap();
        let mut schema = store.changes(store.catalog.clone()).unwrap();
        schema
            .push(Op::Schema(Schema {
                name: "t".into(),
                primary: 0,
                columns: vec![
                    crate::types::Column {
                        name: "id".into(),
                        kind: DataType::Bigint,
                        primary: true,
                    },
                    crate::types::Column {
                        name: "body".into(),
                        kind: DataType::Varchar(300),
                        primary: false,
                    },
                ],
                indexes: vec![],
            }))
            .unwrap();
        store.commit(&schema).unwrap();
        drop(schema);
        let mut small = store.changes(store.catalog.clone()).unwrap();
        for key in [0, 1] {
            small
                .push(Op::Put {
                    table: "t".into(),
                    key,
                    row: vec![Value::Int(key), Value::Text("tiny".into())],
                })
                .unwrap();
        }
        store.commit(&small).unwrap();
        drop(small);
        let path_wal = path.join("commits.mv1");
        let prefix = std::fs::read(&path_wal).unwrap();
        assert!(prefix.starts_with(b"MVTX0001"));
        assert!(prefix.windows(8).any(|bytes| bytes == b"MVTX0002"));
        let mut wide = store.changes(store.catalog.clone()).unwrap();
        let mut rng = 43_u32;
        for key in 2..130 {
            let body: String = (0..300)
                .map(|_| {
                    rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    char::from(b'a' + ((rng >> 16) % 26) as u8)
                })
                .collect();
            wide.push(Op::Put {
                table: "t".into(),
                key,
                row: vec![Value::Int(key), Value::Text(body)],
            })
            .unwrap();
        }
        store.commit(&wide).unwrap();
        drop(wide);
        drop(store);
        let full = std::fs::read(&path_wal).unwrap();
        assert_eq!(&full[..prefix.len()], &prefix);
        assert_eq!(&full[prefix.len()..prefix.len() + 8], b"MVTX0003");
        assert_eq!(full.len() % 4096, 0);
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store
                .view("t", None)
                .unwrap()
                .collect::<Result<Vec<_>>>()
                .unwrap()
                .len(),
            130
        );
        drop(store);
        OpenOptions::new()
            .write(true)
            .open(&path_wal)
            .unwrap()
            .set_len((full.len() - 1) as u64)
            .unwrap();
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store
                .view("t", None)
                .unwrap()
                .collect::<Result<Vec<_>>>()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(std::fs::read(&path_wal).unwrap(), prefix);
        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn writes_and_recovery_do_not_materialize_a_query_cache() {
        let path = std::env::temp_dir().join(format!(
            "mv-lazy-cache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut store = Store::open(&path).unwrap();
        assert!(store.cached_committed().is_none());
        let mut changes = store.changes(store.catalog.clone()).unwrap();
        changes
            .push(Op::Schema(Schema {
                name: "t".into(),
                primary: 0,
                columns: vec![crate::types::Column {
                    name: "id".into(),
                    kind: DataType::Bigint,
                    primary: true,
                }],
                indexes: vec![],
            }))
            .unwrap();
        changes
            .push(Op::Put {
                table: "t".into(),
                key: 7,
                row: vec![Value::Int(7)],
            })
            .unwrap();
        store.commit(&changes).unwrap();
        assert!(store.cached_committed().is_none());
        drop(changes);
        let old = store.prepare_cached().unwrap().unwrap();
        let original_len = old.bytes().len();
        let mut changes = store.changes(store.catalog.clone()).unwrap();
        changes
            .push(Op::Put {
                table: "t".into(),
                key: 8,
                row: vec![Value::Int(8)],
            })
            .unwrap();
        store.commit(&changes).unwrap();
        assert!(store.cached_committed().is_none());
        assert!(
            store.prepare_cached().unwrap().is_none(),
            "a detached old reader must prevent a second allocation"
        );
        assert!(store.cached_bytes() > original_len);
        assert_eq!(old.bytes().len(), original_len);
        drop(old);
        assert!(store.prepare_cached().unwrap().unwrap().bytes().len() > original_len);
        drop(changes);
        drop(store);
        let store = Store::open(&path).unwrap();
        assert!(store.cached_committed().is_none());
        assert_eq!(
            store.view("t", None).unwrap().next().unwrap().unwrap().key,
            7
        );
        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn encodes_losslessly_and_keeps_raw_fallback_compatible() {
        let vectors = [
            (
                (-64..64).map(|x| (x as f32) / 1024.0).collect::<Vec<_>>(),
                3,
            ),
            (vec![1.0, -0.0, f32::from_bits(1), f32::MAX, f32::MIN], 2),
        ];
        for (vector, expected_tag) in vectors {
            let op = Op::Put {
                table: "docs".into(),
                key: 7,
                row: vec![Value::Int(7), Value::Vector(vector.clone())],
            };
            let bytes = encode(&op).unwrap();
            let mut fields = &bytes[5..];
            let (_, _, count) = put_header(&mut fields).unwrap();
            assert_eq!(count, 2);
            assert!(matches!(read_field(&mut fields).unwrap(), Field::Int(_)));
            assert_eq!(fields[0], expected_tag);
            let record = read_record(&mut &bytes[..]).unwrap().unwrap();
            let Op::Put { row, .. } = record.op else {
                panic!("expected row")
            };
            let Value::Vector(actual) = &row[1] else {
                panic!("expected vector")
            };
            assert!(
                actual
                    .iter()
                    .map(|v| v.to_bits())
                    .eq(vector.iter().map(|v| v.to_bits()))
            );
        }
    }

    #[test]
    fn stage_payload_budget_is_shared_and_survives_reader_and_spill() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("mv-stage-{unique}-{}", std::process::id()));
        let mut store = Store::open(&path).unwrap();
        let mut first = store.changes(Catalog::new()).unwrap();
        let mut second = store.changes(Catalog::new()).unwrap();
        let first_path = first.root.with_extension("stage");
        let second_path = second.root.with_extension("stage");

        let row = |table: &str, key: i64, len: usize| Op::Put {
            table: table.to_owned(),
            key,
            row: vec![
                Value::Int(key),
                Value::Text("z".repeat(len)),
                Value::Vector(vec![0.0, -0.0, f32::from_bits(1)]),
            ],
        };
        let length = 320 << 10;
        first.push(row("first", 1, length)).unwrap();
        let was_memory = !first_path.exists();
        let mut reader = store.view("first", Some(&first)).unwrap();
        second.push(row("second", 2, length)).unwrap();
        let second_spilled = second_path.exists();
        first.push(row("first", 3, 100)).unwrap();
        let first_spilled = first_path.exists();
        let previous = reader.read_at(Position::Staged(0)).unwrap();
        let third_offset = first.get("first", 3).unwrap().unwrap().offset;
        let mut new_reader = store.view("first", Some(&first)).unwrap();
        let current = new_reader.read_at(Position::Staged(third_offset)).unwrap();
        let mut second_reader = store.view("second", Some(&second)).unwrap();
        let other = second_reader.read_at(Position::Staged(0)).unwrap();
        drop((reader, new_reader, second_reader, first, second, store));
        assert!(!first_path.exists() && !second_path.exists());
        fs::remove_dir_all(path).unwrap();

        assert!(was_memory, "small stages must start without a disk file");
        assert!(second_spilled && first_spilled);
        for (values, key, text_len) in
            [(previous, 1, length), (current, 3, 100), (other, 2, length)]
        {
            assert_eq!(values[0].int().unwrap(), key);
            assert!(matches!(&values[1], Value::Text(text) if text.len() == text_len));
            assert!(matches!(&values[2], Value::Vector(values)
                if values.iter().map(|f| f.to_bits()).eq([0, 0x8000_0000, 1])));
        }
    }

    #[test]
    fn held_budget_falls_back_to_legacy_then_compact_commit_recovers() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("mv-stage-pack-{unique}-{}", std::process::id()));
        let mut store = Store::open(&path).unwrap();
        let mut create = store.changes(Catalog::new()).unwrap();
        create
            .push(Op::Schema(Schema {
                name: "t".into(),
                columns: vec![
                    crate::types::Column {
                        name: "id".into(),
                        kind: DataType::Bigint,
                        primary: true,
                    },
                    crate::types::Column {
                        name: "body".into(),
                        kind: DataType::Varchar(256),
                        primary: false,
                    },
                ],
                primary: 0,
                indexes: vec![],
            }))
            .unwrap();
        store.commit(&create).unwrap();
        drop(create);

        let mut changes = store.changes(store.catalog.clone()).unwrap();
        for key in 1..=8 {
            changes
                .push(Op::Put {
                    table: "t".into(),
                    key,
                    row: vec![Value::Int(key), Value::Text("hello".repeat(40))],
                })
                .unwrap();
        }

        let position = store.wal.metadata().unwrap().len() as usize;
        let held = store.stage_budget.reserve(512 << 10).unwrap();
        store.commit(&changes).unwrap();
        let compact_position = store.wal.metadata().unwrap().len() as usize;
        drop(held);
        store.commit(&changes).unwrap();
        drop((changes, store));

        let wal = fs::read(path.join("commits.mv1")).unwrap();
        assert_eq!(&wal[position..position + HEADER.len()], HEADER);
        assert_eq!(
            &wal[compact_position..compact_position + BLOCK_HEADER.len()],
            BLOCK_HEADER
        );
        let store = Store::open(&path).unwrap();
        let view = store.view("t", None).unwrap();
        let mut keys = view.map(|row| row.unwrap().key).collect::<Vec<_>>();
        keys.sort_unstable();
        assert_eq!(keys, (1..=8).collect::<Vec<_>>());
        drop(store);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn first_nonempty_merge_adopts_staging_and_preserves_existing_offsets() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("mv-stage-adopt-{unique}-{}", std::process::id()));
        let mut store = Store::open(&path).unwrap();
        let mut transaction = store.changes(Catalog::new()).unwrap();
        let empty_path = transaction.root.clone();
        transaction
            .merge(store.changes(Catalog::new()).unwrap())
            .unwrap();
        assert_eq!(transaction.root, empty_path);
        let make_op = |key| Op::Put {
            table: "t".into(),
            key,
            row: vec![Value::Int(key)],
        };
        let mut first = store.changes(Catalog::new()).unwrap();
        first.push(make_op(1)).unwrap();
        first.push(make_op(2)).unwrap();
        let expected_path = first.root.clone();
        let expected_offset = first.get("t", 2).unwrap().unwrap().offset;
        transaction.merge(first).unwrap();
        assert_eq!(transaction.root, expected_path);
        assert_eq!(
            transaction.get("t", 2).unwrap().unwrap().offset,
            expected_offset
        );
        let mut later = store.changes(Catalog::new()).unwrap();
        later.push(make_op(3)).unwrap();
        transaction.merge(later).unwrap();
        assert_eq!(transaction.root, expected_path);
        assert_eq!(
            transaction.get("t", 2).unwrap().unwrap().offset,
            expected_offset
        );
        let current_offset = transaction.get("t", 3).unwrap().unwrap().offset;
        assert!(current_offset > expected_offset);
        let mut view = store.view("t", Some(&transaction)).unwrap();
        assert_eq!(
            view.read_at(Position::Staged(current_offset)).unwrap()[0]
                .int()
                .unwrap(),
            3
        );
        drop((view, transaction, store));
        fs::remove_dir_all(path).unwrap();
    }

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
