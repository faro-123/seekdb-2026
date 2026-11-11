//! Bounded committed snapshots shared by exact TopK queries.
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::mem::size_of;

use crate::distance::{DistanceBounds, SquaredL2};
use crate::half;
use crate::storage::VectorData;
use crate::storage::{Field, HotRead, Store};
use crate::types::{DataType, Schema, Value};
use crate::{Error, Result};

const WORK_BYTES: usize = 1 << 20;
const MAX_BYTES: usize = 8 << 20;
// Leave room for two copies of a final projected row, the scan buffers, and hits.
const MAX_BODY: usize = 384 << 10;
const MAX_COLUMNS: usize = 256;
pub(crate) const MAX_FIELDS: usize = 8;
pub(crate) const MAX_K: usize = 1024;

pub(crate) struct HotRow {
    pub key: i64,
    pub offset: u64,
}

enum Scalar {
    Int(Vec<i64>),
    Text { offsets: Vec<usize>, bytes: Vec<u8> },
}

struct HotColumn {
    index: usize,
    values: Scalar,
}

enum HotVector<'a> {
    Raw(&'a [f32]),
    Half(&'a [u16]),
}

struct HotVectors {
    dim: usize,
    half: Vec<u16>,
    raw: Vec<f32>,
    // Only mixed snapshots need per-row positions. The high bit selects raw.
    slots: Vec<u32>,
}

impl HotVectors {
    const RAW: u32 = 1 << 31;

    fn new(dim: usize, rows: usize, packed: usize) -> Self {
        Self {
            dim,
            half: Vec::with_capacity(packed * dim),
            raw: Vec::with_capacity((rows - packed) * dim),
            slots: Vec::with_capacity(if packed == 0 || packed == rows {
                0
            } else {
                rows
            }),
        }
    }

    fn push(&mut self, row: &[f32]) -> Result<()> {
        let start = self.half.len();
        // No temporary packed vector. Undo partial appends if a value needs raw.
        let position = if self.half.len() + self.dim <= self.half.capacity()
            && row.iter().all(|&value| {
                half::pack(value).is_some_and(|bits| {
                    self.half.push(bits);
                    true
                })
            }) {
            start as u32
        } else {
            self.half.truncate(start);
            if self.raw.len() + self.dim > self.raw.capacity() {
                return Err(Error::new("storage_error", "hot raw vector count changed"));
            }
            let offset = self.raw.len() as u32;
            self.raw.extend_from_slice(row);
            Self::RAW | offset
        };
        if self.slots.capacity() != 0 {
            self.slots.push(position);
        }
        Ok(())
    }

    fn at(&self, slot: usize) -> HotVector<'_> {
        let position = if !self.slots.is_empty() {
            self.slots[slot]
        } else if self.half.is_empty() {
            Self::RAW | (slot * self.dim) as u32
        } else {
            (slot * self.dim) as u32
        };
        let start = (position & !Self::RAW) as usize;
        if position & Self::RAW == 0 {
            HotVector::Half(&self.half[start..][..self.dim])
        } else {
            HotVector::Raw(&self.raw[start..][..self.dim])
        }
    }

    fn expanded<'a>(&'a self, slot: usize, scratch: &'a mut Vec<f32>) -> &'a [f32] {
        match self.at(slot) {
            HotVector::Raw(row) => row,
            HotVector::Half(row) => {
                scratch.clear();
                scratch.extend(row.iter().copied().map(half::expand));
                scratch
            }
        }
    }
}

pub(crate) struct HotTable {
    name: String,
    embedding: usize,
    dim: usize,
    primary: usize,
    vectors: HotVectors,
    rows: Vec<HotRow>,
    columns: Vec<HotColumn>,
}

#[derive(Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct Hit {
    distance: SquaredL2,
    key: i64,
    pub slot: usize,
}

impl Hit {
    pub fn distance(&self) -> f64 {
        self.distance.distance()
    }
}

#[derive(Clone, Copy)]
struct Estimate(f64);

impl PartialEq for Estimate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Estimate {}
impl PartialOrd for Estimate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Estimate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

fn retain_smallest<T: Ord>(heap: &mut BinaryHeap<T>, k: usize, value: T) {
    if heap.len() < k {
        heap.push(value);
    } else if heap.peek().is_some_and(|worst| value < *worst) {
        *heap.peek_mut().unwrap() = value;
    }
}

impl HotTable {
    pub fn row(&self, slot: usize) -> &HotRow {
        &self.rows[slot]
    }

    pub fn vector<'a>(&'a self, slot: usize, scratch: &'a mut Vec<f32>) -> &'a [f32] {
        self.vectors.expanded(slot, scratch)
    }

    pub fn compare(&self, slot: usize, column: usize, wanted: &Value) -> Ordering {
        if column == self.primary {
            let Value::Int(n) = wanted else {
                unreachable!("typed primary key comparison");
            };
            return self.rows[slot].key.cmp(n);
        }
        let col = self
            .columns
            .iter()
            .find(|c| c.index == column)
            .expect("cached predicate");
        match (&col.values, wanted) {
            (Scalar::Int(values), Value::Int(n)) => values[slot].cmp(n),
            (Scalar::Text { offsets, bytes }, Value::Text(s)) => {
                bytes[offsets[slot]..offsets[slot + 1]].cmp(s.as_bytes())
            }
            _ => unreachable!("typed scalar comparison"),
        }
    }

    fn offer(&self, hits: &mut BinaryHeap<Hit>, k: usize, slot: usize, q: &[f32], vector: &[f32]) {
        let hit = Hit {
            distance: SquaredL2::between(vector, q),
            key: self.rows[slot].key,
            slot,
        };
        retain_smallest(hits, k, hit);
    }

    pub fn select(&self, q: &[f32], k: usize, matches: impl Fn(usize) -> bool) -> Vec<Hit> {
        if let Some(hits) = self.screened(q, k, &matches) {
            return hits;
        }
        let mut hits = BinaryHeap::with_capacity(k.min(self.rows.len()));
        let mut scratch = Vec::new();
        for slot in 0..self.rows.len() {
            if matches(slot) {
                self.offer(&mut hits, k, slot, q, self.vector(slot, &mut scratch));
            }
        }
        hits.into_sorted_vec()
    }

    fn screened(&self, q: &[f32], k: usize, matches: &impl Fn(usize) -> bool) -> Option<Vec<Hit>> {
        let bounds = DistanceBounds::new(self.dim)?;
        let slots = self.rows.len();
        let heap_capacity = k.min(slots);
        // Estimates and both heaps can coexist. Leave room for query bookkeeping.
        if slots * size_of::<f64>()
            + heap_capacity * (size_of::<Hit>() + size_of::<Estimate>())
            + self.dim * size_of::<f32>()
            > WORK_BYTES - (64 << 10)
        {
            return None;
        }
        let mut estimates = Vec::with_capacity(slots);
        let mut upper = BinaryHeap::with_capacity(heap_capacity);
        let mut scratch = Vec::new();
        for slot in 0..slots {
            if !matches(slot) {
                estimates.push(f64::INFINITY);
                continue;
            }
            let estimate = match self.vectors.at(slot) {
                HotVector::Raw(row) => bounds.estimate(row, q),
                HotVector::Half(row) => bounds.estimate_half(row, q, &mut scratch),
            };
            if !estimate.is_finite() {
                return None;
            }
            estimates.push(estimate);
            retain_smallest(&mut upper, k, Estimate(estimate));
        }
        let Some(largest) = upper.peek() else {
            return Some(Vec::new());
        };
        // upper(estimate) is monotone for this query. Using estimates in the
        // heap gives the same Kth upper bound without widening each entry.
        let threshold = bounds.upper(largest.0);
        let mut hits = BinaryHeap::with_capacity(heap_capacity);
        for (slot, estimate) in estimates.into_iter().enumerate() {
            // At least K exact distances are <= threshold if K rows match.
            // Include equality so even unresolved ties reach the exact key.
            if bounds.lower(estimate) <= threshold {
                self.offer(&mut hits, k, slot, q, self.vector(slot, &mut scratch));
            }
        }
        Some(hits.into_sorted_vec())
    }

    fn build(
        store: &Store,
        schema: &Schema,
        embedding: usize,
        fields: &[usize],
        budget: usize,
    ) -> Result<Option<Self>> {
        let DataType::Vector(dim) = schema.columns[embedding].kind else {
            unreachable!("hot scan requires a bound vector");
        };
        // The row decoder used for final projections also needs space for its Value array.
        if dim == 0 || dim > MAX_BODY / size_of::<f32>() || schema.columns.len() > MAX_COLUMNS {
            return Ok(None);
        }
        let mut row_width = size_of::<HotRow>();
        // Include table name, column descriptors, and each text column's initial offset.
        let mut bytes = size_of::<Self>()
            + size_of::<HotCache>()
            + schema.name.len()
            + fields.len() * (size_of::<HotColumn>() + size_of::<usize>());
        for &index in fields {
            row_width += match schema.columns[index].kind {
                DataType::Bigint => size_of::<i64>(),
                DataType::Varchar(_) => size_of::<usize>(),
                DataType::Vector(_) => unreachable!("vector predicates are rejected at binding"),
            };
        }
        if bytes > budget {
            return Ok(None);
        }

        // First count actual bytes. VARCHAR declarations can greatly exceed the stored values.
        // Use the same fixed body buffer for both passes and borrow fields from it.
        let mut buffer = vec![0; MAX_BODY];
        let mut text_bytes = [0; MAX_FIELDS];
        let mut rows = 0;
        let mut packed = 0;
        let mut expanded = Vec::with_capacity(dim);
        if !scan_rows(store, schema, embedding, fields, &mut buffer, |cell| {
            match cell {
                Cell::Row(..) => {
                    bytes += row_width;
                    rows += 1;
                }
                Cell::Embedding(values) => {
                    if values.len() != dim {
                        return Err(Error::new(
                            "storage_error",
                            "hot vector dimension differs from schema",
                        ));
                    }
                    expanded.clear();
                    values.append(&mut expanded)?;
                    if expanded.iter().all(|&value| half::pack(value).is_some()) {
                        packed += 1;
                        bytes += dim * size_of::<u16>();
                    } else {
                        bytes += dim * size_of::<f32>();
                    }
                }
                Cell::Scalar(col, Field::Text(value)) => {
                    bytes += value.len();
                    text_bytes[col] += value.len();
                }
                _ => {}
            }
            Ok(bytes <= budget)
        })? {
            return Ok(None);
        }

        if packed != 0 && packed != rows && bytes + rows * size_of::<u32>() > budget {
            return Ok(None);
        }
        let mut table = Self {
            name: schema.name.clone(),
            embedding,
            dim,
            primary: schema.primary,
            vectors: HotVectors::new(dim, rows, packed),
            rows: Vec::with_capacity(rows),
            columns: fields
                .iter()
                .enumerate()
                .map(|(i, &index)| HotColumn {
                    index,
                    values: match schema.columns[index].kind {
                        DataType::Bigint => Scalar::Int(Vec::with_capacity(rows)),
                        DataType::Varchar(_) => {
                            let mut offsets = Vec::with_capacity(rows + 1);
                            offsets.push(0);
                            Scalar::Text {
                                offsets,
                                bytes: Vec::with_capacity(text_bytes[i]),
                            }
                        }
                        DataType::Vector(_) => unreachable!("validated scalar column"),
                    },
                })
                .collect(),
        };
        let complete = scan_rows(store, schema, embedding, fields, &mut buffer, |cell| {
            match cell {
                Cell::Row(key, offset) if table.rows.len() < rows => {
                    table.rows.push(HotRow { key, offset });
                }
                Cell::Embedding(values) if values.len() == dim => {
                    expanded.clear();
                    values.append(&mut expanded)?;
                    table.vectors.push(&expanded)?;
                }
                Cell::Scalar(col, value) => match (&mut table.columns[col].values, value) {
                    (Scalar::Int(values), Field::Int(b)) if values.len() < rows => {
                        values.push(i64::from_le_bytes(*b));
                    }
                    (Scalar::Text { offsets, bytes }, Field::Text(value))
                        if offsets.len() <= rows
                            && value.len() <= text_bytes[col] - bytes.len() =>
                    {
                        bytes.extend_from_slice(value);
                        offsets.push(bytes.len());
                    }
                    _ => return Ok(false),
                },
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        if !complete
            || table.rows.len() != rows
            || table.vectors.half.len() != packed * dim
            || table.vectors.raw.len() != (rows - packed) * dim
        {
            return Err(Error::new(
                "storage_error",
                "committed rows changed during hot scan build",
            ));
        }
        Ok(Some(table))
    }
}

enum Cell<'a> {
    Row(i64, u64),
    Embedding(VectorData<'a>),
    Scalar(usize, Field<'a>),
}

// The single client cannot modify the committed view between the two passes.
fn scan_rows(
    store: &Store,
    schema: &Schema,
    embedding: usize,
    selected: &[usize],
    buffer: &mut [u8],
    mut visit: impl FnMut(Cell<'_>) -> Result<bool>,
) -> Result<bool> {
    let mut view = store.view(&schema.name, None)?.batched();
    loop {
        let (key, offset, mut fields) = match view.next_hot(buffer)? {
            HotRead::End => return Ok(true),
            HotRead::TooLarge => return Ok(false),
            HotRead::Row {
                key,
                offset,
                fields,
            } => (key, offset, fields),
        };
        if fields.count() != schema.columns.len() {
            return Err(Error::new(
                "storage_error",
                "hot row column count differs from schema",
            ));
        }
        if !visit(Cell::Row(key, offset))? {
            return Ok(false);
        }
        let mut wanted = selected.iter().enumerate().peekable();
        for index in 0..fields.count() {
            let field = fields.next_field()?;
            let cell = if index == embedding {
                let Field::Vector(values) = field else {
                    return Err(Error::new("storage_error", "hot row vector is missing"));
                };
                Cell::Embedding(values)
            } else if wanted.peek().is_some_and(|&(_, &column)| column == index) {
                Cell::Scalar(wanted.next().unwrap().0, field)
            } else {
                continue;
            };
            if !visit(cell)? {
                return Ok(false);
            }
        }
    }
}

enum Snapshot {
    Empty,
    Denied(String),
    Ready(HotTable),
}

pub(crate) struct HotCache {
    budget: usize,
    snapshot: Snapshot,
}

impl HotCache {
    pub fn new() -> Self {
        let budget = std::env::var("MINIVECTORDB_HOT_BYTES")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(MAX_BYTES)
            .min(MAX_BYTES);
        Self::with_budget(budget)
    }

    fn with_budget(budget: usize) -> Self {
        Self {
            budget,
            snapshot: Snapshot::Empty,
        }
    }

    pub fn invalidate(&mut self) {
        self.snapshot = Snapshot::Empty;
    }

    fn table(&self) -> Option<&HotTable> {
        match &self.snapshot {
            Snapshot::Ready(table) => Some(table),
            _ => None,
        }
    }

    pub fn get(
        &mut self,
        store: &Store,
        schema: &Schema,
        vector: usize,
        fields: &[usize],
    ) -> Result<Option<&HotTable>> {
        if schema.name.len() > 4096 {
            return Ok(None);
        }
        let Some(budget) = self.budget.checked_sub(WORK_BYTES).filter(|n| *n > 0) else {
            return Ok(None);
        };
        // Small fixed workspace even for SQL with many duplicate predicates.
        let mut union = [0; MAX_FIELDS];
        let mut count = fields.len();
        union[..count].copy_from_slice(fields);
        match &self.snapshot {
            Snapshot::Denied(name) if *name == schema.name => return Ok(None),
            Snapshot::Ready(table) if table.name == schema.name && table.embedding == vector => {
                if fields
                    .iter()
                    .all(|i| table.columns.iter().any(|col| col.index == *i))
                {
                    return Ok(self.table());
                }
                for column in &table.columns {
                    if !union[..count].contains(&column.index) {
                        if count == MAX_FIELDS {
                            return Ok(None);
                        }
                        union[count] = column.index;
                        count += 1;
                    }
                }
            }
            _ => {}
        }
        // Evict before counting or allocating the next table, including column expansion.
        self.invalidate();
        union[..count].sort_unstable();
        self.snapshot = match HotTable::build(store, schema, vector, &union[..count], budget)? {
            Some(table) => Snapshot::Ready(table),
            None => Snapshot::Denied(schema.name.clone()),
        };
        Ok(self.table())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Op;
    use crate::types::{Column, DataType, Value};
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn mixed_snapshot_preserves_half_bits_and_raw_values() {
        let inputs = [
            [1.0, -0.0, f32::from_bits(0x33800000)],
            [f32::from_bits(1), f32::MAX, 0.33333334],
            [2.0, -1.0, 65504.0],
        ];
        let mut vectors = HotVectors::new(3, 3, 2);
        for row in &inputs {
            vectors.push(row).unwrap();
        }
        assert_eq!(vectors.half.len(), 6);
        assert_eq!(vectors.raw.len(), 3);
        let mut scratch = Vec::new();
        for (i, row) in inputs.iter().enumerate() {
            assert!(
                vectors
                    .expanded(i, &mut scratch)
                    .iter()
                    .map(|v| v.to_bits())
                    .eq(row.iter().map(|v| v.to_bits()))
            );
        }
    }

    #[test]
    fn snapshot_reuses_contiguous_vectors_and_declines_low_budget() {
        let path = std::env::temp_dir().join(format!(
            "mv-hot-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let schema = Schema {
            name: "t".into(),
            primary: 0,
            columns: vec![
                Column {
                    name: "id".into(),
                    kind: DataType::Bigint,
                    primary: true,
                },
                Column {
                    name: "category".into(),
                    kind: DataType::Varchar(1_000_000),
                    primary: false,
                },
                Column {
                    name: "v".into(),
                    kind: DataType::Vector(2),
                    primary: false,
                },
            ],
            indexes: vec![("vi".into(), 2)],
        };
        let mut store = Store::open(&path).unwrap();
        let mut changes = store.changes(store.catalog.clone()).unwrap();
        changes.push(Op::Schema(schema.clone())).unwrap();
        changes
            .push(Op::Put {
                table: "t".into(),
                key: 7,
                row: vec![
                    Value::Int(7),
                    Value::Text("a".into()),
                    Value::Vector(vec![1.0, 2.0]),
                ],
            })
            .unwrap();
        store.commit(&changes).unwrap();
        let mut cache = HotCache::with_budget(MAX_BYTES);
        let table = cache
            .get(&store, &schema, 2, &[1])
            .unwrap()
            .expect("committed row must become hot");
        assert_eq!(table.vector(0, &mut Vec::new()), [1.0, 2.0]);
        let address = table.vectors.half.as_ptr();
        assert_eq!(
            cache
                .get(&store, &schema, 2, &[])
                .unwrap()
                .unwrap()
                .vectors
                .half
                .as_ptr(),
            address,
            "subset predicates must reuse the same allocation"
        );
        assert!(
            HotCache::with_budget(WORK_BYTES)
                .get(&store, &schema, 2, &[1])
                .unwrap()
                .is_none()
        );
        drop(changes);
        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }
}
