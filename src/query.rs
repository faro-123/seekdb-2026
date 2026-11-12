//! Typed plans, filtered exact distances, and bounded external sorting.
use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeSet, BinaryHeap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::PathBuf;

use crate::distance::SquaredL2;
use crate::hot::{HotCache, HotTable, MAX_FIELDS, MAX_K};
use crate::sql::{Condition, Expr, Select};
use crate::storage::{Changes, Position, Store, VectorView, View};
use crate::types::{DataType, Schema, Value, coerce, vector};
use crate::{Error, Result};

pub(crate) struct Predicates(Vec<(usize, String, Value)>);
impl Predicates {
    pub fn bind(schema: &Schema, conditions: &[Condition]) -> Result<Self> {
        let mut result = Vec::new();
        for cond in conditions {
            let i = schema.column(&cond.column)?;
            if matches!(schema.columns[i].kind, DataType::Vector(_)) {
                return Err(Error::new("type_error", "vector comparisons unsupported"));
            }
            let value = match (&cond.value, &schema.columns[i].kind) {
                // A comparison literal need not fit in the column's storage length.
                (crate::types::Literal::Text(s), DataType::Varchar(_)) => Value::Text(s.clone()),
                _ => coerce(&cond.value, &schema.columns[i].kind)?,
            };
            result.push((i, cond.op.clone(), value));
        }
        Ok(Self(result))
    }
    pub fn matches(&self, row: &[Value]) -> bool {
        self.0.iter().all(|(i, op, wanted)| {
            let cmp = match (&row[*i], wanted) {
                (Value::Int(a), Value::Int(b)) => a.cmp(b),
                (Value::Text(a), Value::Text(b)) => a.cmp(b),
                _ => return false,
            };
            compare(op, cmp)
        })
    }

    fn matches_hot(&self, table: &HotTable, slot: usize) -> bool {
        self.0
            .iter()
            .all(|(column, op, wanted)| compare(op, table.compare(slot, *column, wanted)))
    }

    fn equalities(&self) -> Option<Vec<(usize, &Value)>> {
        if self.0.is_empty() || self.0.iter().any(|(_, op, _)| op != "=") {
            return None;
        }
        let mut equalities: Vec<_> = self.0.iter().map(|(i, _, value)| (*i, value)).collect();
        equalities.sort_unstable_by_key(|&(i, _)| i);
        Some(equalities)
    }
}

fn compare(op: &str, cmp: Ordering) -> bool {
    match op {
        "=" => cmp == Ordering::Equal,
        "<" => cmp == Ordering::Less,
        "<=" => cmp != Ordering::Greater,
        ">" => cmp == Ordering::Greater,
        ">=" => cmp != Ordering::Less,
        _ => false,
    }
}

enum QueryRow {
    Full(Vec<Value>),
    Indexed {
        primary: (usize, Value),
        embedding: (usize, Value),
    },
}

impl QueryRow {
    fn column(&self, index: usize) -> &Value {
        match self {
            Self::Full(row) => &row[index],
            Self::Indexed { primary, .. } if primary.0 == index => &primary.1,
            Self::Indexed { embedding, .. } if embedding.0 == index => &embedding.1,
            Self::Indexed { .. } => unreachable!("flat scan selected only for covered columns"),
        }
    }
}

enum TableFilter<'a> {
    Full(&'a Predicates),
    Equality {
        tests: Vec<(usize, &'a Value)>,
        buffer: Vec<u8>,
    },
}

enum Scan<'a> {
    Table {
        source: Box<View>,
        filter: TableFilter<'a>,
    },
    Vector {
        source: VectorView,
        primary: usize,
        embedding: usize,
    },
}

fn indexed_row(key: i64, vector: Vec<f32>, primary: usize, embedding: usize) -> QueryRow {
    QueryRow::Indexed {
        primary: (primary, Value::Int(key)),
        embedding: (embedding, Value::Vector(vector)),
    }
}

impl Scan<'_> {
    fn read_at(&mut self, candidate: Candidate) -> Result<QueryRow> {
        let position = candidate.position;
        let key = candidate.primary;
        // Sorting is finished for this hit. Release wide keys before decoding
        // the original row for projection.
        drop(candidate);
        match (self, position) {
            (Self::Table { source, .. }, position) => Ok(QueryRow::Full(source.read_at(position)?)),
            (
                Self::Vector {
                    source,
                    primary,
                    embedding,
                },
                Position::Vector(offset),
            ) => Ok(indexed_row(
                key,
                source.read_at(offset)?,
                *primary,
                *embedding,
            )),
            _ => Err(Error::new("storage_error", "invalid vector scan position")),
        }
    }
}

impl Iterator for Scan<'_> {
    type Item = Result<(i64, Position, QueryRow)>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Table { source, filter } => {
                let record = match filter {
                    TableFilter::Full(predicates) => source.find(|record| {
                        record
                            .as_ref()
                            .map_or(true, |row| predicates.matches(&row.values))
                    }),
                    TableFilter::Equality { tests, buffer } => {
                        source.next_equal(tests, buffer).transpose()
                    }
                }?;
                Some(record.map(|row| (row.key, row.position, QueryRow::Full(row.values))))
            }
            Self::Vector {
                source,
                primary,
                embedding,
            } => source.next().map(|record| {
                record.map(|(key, offset, vector)| {
                    (
                        key,
                        Position::Vector(offset),
                        indexed_row(key, vector, *primary, *embedding),
                    )
                })
            }),
        }
    }
}

#[derive(Clone)]
enum BoundExpr {
    Column(usize),
    Distance(usize, Vec<f32>),
}
impl BoundExpr {
    fn bind(schema: &Schema, expr: &Expr) -> Result<Self> {
        match expr {
            Expr::Column(name) => Ok(Self::Column(schema.column(name)?)),
            Expr::Distance(name, text) => {
                let i = schema.column(name)?;
                let DataType::Vector(dim) = schema.columns[i].kind else {
                    return Err(Error::new(
                        "type_error",
                        "distance requires a VECTOR column",
                    ));
                };
                Ok(Self::Distance(i, vector(text, dim)?))
            }
        }
    }
    fn distance(row: &QueryRow, i: usize, q: &[f32]) -> SquaredL2 {
        let Value::Vector(v) = row.column(i) else {
            unreachable!("typed vector row")
        };
        SquaredL2::between(v, q)
    }
    fn column_index(&self) -> usize {
        match self {
            Self::Column(i) | Self::Distance(i, _) => *i,
        }
    }
    fn write_json(&self, row: &QueryRow, out: &mut impl Write) -> Result<()> {
        match self {
            Self::Column(i) => row.column(*i).write_json(out)?,
            Self::Distance(i, q) => {
                serde_json::to_writer(out, &Self::distance(row, *i, q).distance())?
            }
        }
        Ok(())
    }
    fn sort_value(&self, row: &QueryRow) -> SortValue {
        match self {
            Self::Column(i) => match row.column(*i) {
                Value::Int(n) => SortValue::Int(*n),
                Value::Text(s) => SortValue::Text(s.clone()),
                Value::Vector(_) => unreachable!("vector order rejected during bind"),
            },
            Self::Distance(i, q) => SortValue::Distance(Self::distance(row, *i, q)),
        }
    }
}
pub(crate) struct Plan {
    pub columns: Vec<String>,
    projections: Vec<BoundExpr>,
    predicates: Predicates,
    order: Vec<(BoundExpr, bool)>,
    limit: Option<usize>,
}
impl Plan {
    pub fn bind(schema: &Schema, select: &Select) -> Result<Self> {
        let mut columns = Vec::new();
        let mut projections = Vec::new();
        if select.star {
            for (i, col) in schema.columns.iter().enumerate() {
                columns.push(col.name.clone());
                projections.push(BoundExpr::Column(i));
            }
        } else {
            for proj in &select.projections {
                let default = match &proj.expr {
                    Expr::Column(n) => n.clone(),
                    Expr::Distance(_, _) => "l2_distance".into(),
                };
                columns.push(proj.alias.clone().unwrap_or(default));
                projections.push(BoundExpr::bind(schema, &proj.expr)?);
            }
        }
        let predicates = Predicates::bind(schema, &select.conditions)?;
        let mut order = Vec::new();
        for (expr, desc) in &select.order {
            let bound = if let Expr::Column(name) = expr {
                if let Some(i) = columns.iter().position(|c| c == name) {
                    projections[i].clone()
                } else {
                    BoundExpr::bind(schema, expr)?
                }
            } else {
                BoundExpr::bind(schema, expr)?
            };
            if let BoundExpr::Column(i) = bound
                && matches!(schema.columns[i].kind, DataType::Vector(_))
            {
                return Err(Error::new(
                    "type_error",
                    "VECTOR cannot be ordered directly",
                ));
            }
            order.push((bound, *desc));
        }
        Ok(Self {
            columns,
            projections,
            predicates,
            order,
            limit: select.limit,
        })
    }

    fn covered_vector(&self, schema: &Schema) -> Option<usize> {
        let &(_, vector) = schema.indexes.first()?;
        let expressions = || {
            self.projections
                .iter()
                .chain(self.order.iter().map(|(e, _)| e))
        };
        let covered = self.predicates.0.is_empty()
            && expressions().any(|e| e.column_index() == vector)
            && expressions()
                .all(|e| matches!(e.column_index(), i if i == schema.primary || i == vector));
        covered.then_some(vector)
    }

    fn inline_capacity(&self, schema: &Schema, limit: usize) -> Option<usize> {
        // Bound the JSON for small numeric projections before allocating.
        // A finite f64 or i64, including its delimiter, fits in 64 bytes.
        const INLINE_BYTES: usize = 32 << 10;
        if !self.projections.iter().all(|expr| match expr {
            BoundExpr::Distance(..) => true,
            BoundExpr::Column(i) => matches!(schema.columns[*i].kind, DataType::Bigint),
        }) {
            return None;
        }
        let row = self.projections.len().checked_mul(64)?.checked_add(3)?;
        let bytes = row.checked_mul(limit)?.checked_add(2)?;
        (bytes <= INLINE_BYTES).then_some(bytes)
    }

    fn table_filter(&self) -> TableFilter<'_> {
        if self.limit.is_some()
            && self
                .order
                .iter()
                .any(|(expr, _)| matches!(expr, BoundExpr::Distance(..)))
            && let Some(tests) = self.predicates.equalities()
        {
            return TableFilter::Equality {
                tests,
                buffer: Vec::new(),
            };
        }
        TableFilter::Full(&self.predicates)
    }
}
#[derive(Clone, Debug)]
enum SortValue {
    Int(i64),
    Text(String),
    Distance(SquaredL2),
}
impl PartialEq for SortValue {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for SortValue {}
impl PartialOrd for SortValue {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for SortValue {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Int(a), Self::Int(b)) => a.cmp(b),
            (Self::Text(a), Self::Text(b)) => a.cmp(b),
            (Self::Distance(a), Self::Distance(b)) => a.cmp(b),
            _ => unreachable!("sort type fixed by plan"),
        }
    }
}
#[derive(Clone, Debug)]
struct Candidate {
    keys: Vec<(SortValue, bool)>,
    primary: i64,
    position: Position,
}
impl Candidate {
    fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.keys.capacity() * std::mem::size_of::<(SortValue, bool)>()
            + self
                .keys
                .iter()
                .map(|(value, _)| {
                    if let SortValue::Text(s) = value {
                        s.capacity()
                    } else {
                        0
                    }
                })
                .sum::<usize>()
    }

    fn write(&self, out: &mut impl Write) -> Result<()> {
        let (tag, offset) = match self.position {
            Position::Committed(offset) => (0, offset),
            Position::Staged(offset) => (1, offset),
            Position::Vector(offset) => (2, offset),
        };
        out.write_all(&[tag])?;
        out.write_all(&offset.to_le_bytes())?;
        out.write_all(&self.primary.to_le_bytes())?;
        write_run_length(out, self.keys.len())?;
        for (key, descending) in &self.keys {
            let tag = match key {
                SortValue::Int(_) => 0,
                SortValue::Text(_) => 1,
                SortValue::Distance(_) => 2,
            };
            out.write_all(&[tag | u8::from(*descending) << 2])?;
            match key {
                SortValue::Int(key) => out.write_all(&key.to_le_bytes())?,
                SortValue::Text(key) => {
                    write_run_length(out, key.len())?;
                    out.write_all(key.as_bytes())?;
                }
                SortValue::Distance(key) => key.write_run(out)?,
            }
        }
        Ok(())
    }
}

fn write_run_length(out: &mut impl Write, length: usize) -> Result<()> {
    let length = u32::try_from(length)
        .map_err(|_| Error::new("resource_limit", "sort run field too large"))?;
    out.write_all(&length.to_le_bytes())?;
    Ok(())
}

fn read_run_bytes<const N: usize>(reader: &mut impl Read) -> Result<[u8; N]> {
    let mut bytes = [0; N];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}
impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Candidate {}
impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        for ((a, desc), (b, _)) in self.keys.iter().zip(&other.keys) {
            let cmp = if *desc { b.cmp(a) } else { a.cmp(b) };
            if cmp != Ordering::Equal {
                return cmp;
            }
        }
        self.primary.cmp(&other.primary)
    }
}
struct Runs {
    base: PathBuf,
    sequence: usize,
    levels: Vec<Vec<PathBuf>>,
    artifacts: BTreeSet<PathBuf>,
    max_candidate_bytes: usize,
    head_budget: usize,
}
impl Runs {
    fn new(base: PathBuf) -> Self {
        Self {
            base,
            sequence: 0,
            levels: Vec::new(),
            artifacts: BTreeSet::new(),
            max_candidate_bytes: 0,
            head_budget: 16 << 20,
        }
    }
    fn fan_in(&self) -> usize {
        // Bound retained decoded heads separately from the run-generation
        // budget. Binary run I/O reads each key into its final allocation.
        // At least two heads are necessary to make progress for oversized keys.
        (self.head_budget / self.max_candidate_bytes.max(1)).clamp(2, 16)
    }
    fn path(&mut self) -> PathBuf {
        self.sequence += 1;
        let p = self.base.with_extension(format!("run{}", self.sequence));
        self.artifacts.insert(p.clone());
        p
    }
    fn remove(&mut self, path: PathBuf) -> Result<()> {
        fs::remove_file(&path)?;
        self.artifacts.remove(&path);
        Ok(())
    }
    fn insert(&mut self, path: PathBuf, level: usize) -> Result<()> {
        if self.levels.len() <= level {
            self.levels.resize_with(level + 1, Vec::new);
        }
        self.levels[level].push(path);
        // A later wide key can lower the fan-in while a level already contains
        // many narrow-key runs. Consume bounded batches even after that change.
        while self.levels[level].len() >= self.fan_in() {
            let fan_in = self.fan_in();
            let paths: Vec<_> = self.levels[level].drain(..fan_in).collect();
            let path = self.path();
            let mut out = BufWriter::new(File::create(&path)?);
            merge(&paths, |c| {
                c.write(&mut out)?;
                Ok(true)
            })?;
            out.flush()?;
            drop(out);
            for p in paths {
                self.remove(p)?;
            }
            self.insert(path, level + 1)?;
        }
        Ok(())
    }
    fn spill(&mut self, buffer: &mut Vec<Candidate>) -> Result<()> {
        for c in buffer.iter() {
            self.max_candidate_bytes = self.max_candidate_bytes.max(c.bytes());
        }
        buffer.sort_unstable();
        let path = self.path();
        let mut out = BufWriter::new(File::create(&path)?);
        for c in buffer.drain(..) {
            c.write(&mut out)?;
        }
        out.flush()?;
        drop(out);
        self.insert(path, 0)
    }
    fn final_paths(&mut self) -> Result<Vec<PathBuf>> {
        let mut paths: Vec<_> = self.levels.iter_mut().flat_map(std::mem::take).collect();
        let fan_in = self.fan_in();
        while paths.len() > fan_in {
            let mut next = Vec::new();
            for batch in paths.chunks(fan_in) {
                let path = self.path();
                let mut out = BufWriter::new(File::create(&path)?);
                merge(batch, |c| {
                    c.write(&mut out)?;
                    Ok(true)
                })?;
                out.flush()?;
                next.push(path);
            }
            for p in paths {
                self.remove(p)?;
            }
            paths = next;
        }
        Ok(paths)
    }
}
impl Drop for Runs {
    fn drop(&mut self) {
        for p in &self.artifacts {
            let _ = fs::remove_file(p);
        }
    }
}
fn read_candidate(reader: &mut BufReader<File>) -> Result<Option<Candidate>> {
    if reader.fill_buf()?.is_empty() {
        return Ok(None);
    }
    let [tag] = read_run_bytes(reader)?;
    let offset = u64::from_le_bytes(read_run_bytes(reader)?);
    let invalid = || Error::new("storage_error", "invalid sort run record");
    let position = match tag {
        0 => Position::Committed(offset),
        1 => Position::Staged(offset),
        2 => Position::Vector(offset),
        _ => return Err(invalid()),
    };
    let primary = i64::from_le_bytes(read_run_bytes(reader)?);
    let count = u32::from_le_bytes(read_run_bytes(reader)?);
    let mut keys = Vec::new();
    for _ in 0..count {
        let [tag] = read_run_bytes(reader)?;
        let value = match tag & !4 {
            0 => SortValue::Int(i64::from_le_bytes(read_run_bytes(reader)?)),
            1 => {
                let length = u32::from_le_bytes(read_run_bytes(reader)?) as usize;
                let mut bytes = vec![0; length];
                reader.read_exact(&mut bytes)?;
                SortValue::Text(String::from_utf8(bytes).map_err(|_| invalid())?)
            }
            2 => SortValue::Distance(SquaredL2::read_run(reader)?),
            _ => return Err(invalid()),
        };
        keys.push((value, tag & 4 != 0));
    }
    Ok(Some(Candidate {
        keys,
        primary,
        position,
    }))
}
fn merge(paths: &[PathBuf], mut emit: impl FnMut(Candidate) -> Result<bool>) -> Result<()> {
    let mut readers: Vec<_> = paths
        .iter()
        .map(|p| File::open(p).map(BufReader::new))
        .collect::<std::io::Result<_>>()?;
    let mut heads = BinaryHeap::new();
    for (i, reader) in readers.iter_mut().enumerate() {
        if let Some(c) = read_candidate(reader)? {
            heads.push(Reverse((c, i)));
        }
    }
    while let Some(Reverse((c, i))) = heads.pop() {
        if !emit(c)? {
            break;
        }
        if let Some(c) = read_candidate(&mut readers[i])? {
            heads.push(Reverse((c, i)));
        }
    }
    Ok(())
}
enum Rows {
    File(PathBuf),
    Inline(Vec<u8>),
}

pub(crate) struct QueryResult {
    pub columns: Vec<String>,
    rows: Rows,
}
impl QueryResult {
    pub fn write_rows(&self, out: &mut impl Write) -> std::io::Result<()> {
        match &self.rows {
            Rows::File(path) => std::io::copy(&mut File::open(path)?, out).map(|_| ()),
            Rows::Inline(bytes) => out.write_all(bytes),
        }
    }
}
impl Drop for QueryResult {
    fn drop(&mut self) {
        if let Rows::File(path) = &self.rows {
            let _ = fs::remove_file(path);
        }
    }
}
struct RowWriter<W> {
    out: W,
    count: usize,
}
impl<W: Write> RowWriter<W> {
    fn new(mut out: W) -> Result<Self> {
        out.write_all(b"[")?;
        Ok(Self { out, count: 0 })
    }
    fn row(&mut self, projections: &[BoundExpr], row: &QueryRow) -> Result<()> {
        self.row_with(projections, |expr, out| expr.write_json(row, out))
    }
    fn row_with(
        &mut self,
        projections: &[BoundExpr],
        mut cell: impl FnMut(&BoundExpr, &mut W) -> Result<()>,
    ) -> Result<()> {
        if self.count > 0 {
            self.out.write_all(b",")?;
        }
        self.out.write_all(b"[")?;
        for (i, projection) in projections.iter().enumerate() {
            if i > 0 {
                self.out.write_all(b",")?;
            }
            cell(projection, &mut self.out)?;
        }
        self.out.write_all(b"]")?;
        self.count += 1;
        Ok(())
    }
    fn finish(mut self) -> Result<W> {
        self.out.write_all(b"]")?;
        self.out.flush()?;
        Ok(self.out)
    }
}

fn try_hot(
    plan: &Plan,
    schema: &Schema,
    store: &mut Store,
    cache: &mut HotCache,
    writer: &mut RowWriter<impl Write>,
) -> Result<bool> {
    let [(BoundExpr::Distance(embedding, q), false)] = &plan.order[..] else {
        return Ok(false);
    };
    let Some(k) = plan.limit.filter(|&k| k <= MAX_K) else {
        return Ok(false);
    };
    if !schema.indexes.iter().any(|(_, i)| i == embedding) {
        return Ok(false);
    }
    let mut fields = [0; MAX_FIELDS];
    let mut count = 0;
    for &(i, _, _) in &plan.predicates.0 {
        if i != schema.primary && !fields[..count].contains(&i) {
            if count == MAX_FIELDS {
                return Ok(false);
            }
            fields[count] = i;
            count += 1;
        }
    }
    let Some(table) = cache.get(store, schema, *embedding, &fields[..count], Some(q))? else {
        return Ok(false);
    };
    let hits = table.select(q, k, |slot| plan.predicates.matches_hot(table, slot))?;
    let covered = plan.projections.iter().all(|expr| {
        let column = expr.column_index();
        column == schema.primary || column == *embedding
    });
    let mut source = if covered || hits.is_empty() {
        None
    } else {
        Some(store.view(&schema.name, None)?)
    };
    let mut expanded = Vec::new();
    for hit in hits {
        let row = table.row(hit.slot);
        if let Some(source) = &mut source {
            let projected = QueryRow::Full(source.read_at(Position::Committed(row.offset))?);
            writer.row(&plan.projections, &projected)?;
        } else {
            writer.row_with(&plan.projections, |projection, out| {
                match projection {
                    BoundExpr::Column(i) if *i == schema.primary => {
                        serde_json::to_writer(out, &row.key)?;
                    }
                    BoundExpr::Column(_) => {
                        Value::write_vector_json(table.vector(hit.slot, &mut expanded)?, out)?
                    }
                    BoundExpr::Distance(_, query) => {
                        let distance = if query == q {
                            hit.distance()
                        } else {
                            SquaredL2::between(table.vector(hit.slot, &mut expanded)?, query)
                                .distance()
                        };
                        serde_json::to_writer(out, &distance)?;
                    }
                }
                Ok(())
            })?;
        }
    }
    Ok(true)
}

pub(crate) fn execute(
    plan: Plan,
    schema: &Schema,
    transaction: Option<&Changes>,
    store: &mut Store,
    cache: &mut HotCache,
) -> Result<QueryResult> {
    let limit = plan.limit.unwrap_or(usize::MAX);
    if limit == 0 {
        return Ok(QueryResult {
            columns: plan.columns,
            rows: Rows::Inline(b"[]".to_vec()),
        });
    }
    let mut tried_hot = false;
    if transaction.is_none()
        && let Some(capacity) = plan.inline_capacity(schema, limit)
    {
        let mut writer = RowWriter::new(Vec::with_capacity(capacity))?;
        if try_hot(&plan, schema, store, cache, &mut writer)? {
            return Ok(QueryResult {
                columns: plan.columns,
                rows: Rows::Inline(writer.finish()?),
            });
        }
        tried_hot = true;
    }
    let path = store.temp("response");
    let file = File::create(&path)?;
    let result = QueryResult {
        columns: plan.columns.clone(),
        rows: Rows::File(path),
    };
    let mut writer = RowWriter::new(BufWriter::new(file))?;
    if transaction.is_none() && !tried_hot && try_hot(&plan, schema, store, cache, &mut writer)? {
        writer.finish()?;
        return Ok(result);
    }
    let batch_reads = transaction.is_none()
        && plan.limit.is_some()
        && plan
            .order
            .iter()
            .any(|(expr, _)| matches!(expr, BoundExpr::Distance(..)));
    let mut view = if let Some(embedding) = plan
        .covered_vector(schema)
        .filter(|_| transaction.is_none())
    {
        let source = store.vector_view(schema, embedding)?;
        Scan::Vector {
            source: if batch_reads {
                source.batched()
            } else {
                source
            },
            primary: schema.primary,
            embedding,
        }
    } else {
        let source = store.view(&schema.name, transaction)?;
        Scan::Table {
            source: Box::new(if batch_reads {
                source.batched()
            } else {
                source
            }),
            filter: plan.table_filter(),
        }
    };
    if plan.order.is_empty() {
        for record in view {
            let (_, _, row) = record?;
            writer.row(&plan.projections, &row)?;
            if writer.count >= limit {
                break;
            }
        }
        writer.finish()?;
        return Ok(result);
    }
    let budget = std::env::var("MINIVECTORDB_SORT_BYTES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1_048_576)
        .max(4096);
    let mut runs = Runs::new(store.temp("sort"));
    let mut buffer = Vec::new();
    let mut heap = BinaryHeap::<Candidate>::new();
    let mut bytes = 0;
    let mut heap_mode = plan.limit.is_some_and(|n| n <= 10000);
    for record in view.by_ref() {
        let (primary, position, row) = record?;
        let c = Candidate {
            keys: plan
                .order
                .iter()
                .map(|(e, d)| (e.sort_value(&row), *d))
                .collect(),
            primary,
            position,
        };
        let size = c.bytes();
        // A spill may recursively merge many runs; the scan row is no longer
        // needed once its keys and storage position have been captured.
        drop(row);
        if heap_mode {
            if heap.len() < limit {
                bytes += size;
                heap.push(c);
            } else if heap.peek().is_some_and(|worst| c < *worst) {
                let old = heap.pop().unwrap();
                bytes = bytes - old.bytes() + size;
                heap.push(c);
            }
            if bytes >= budget {
                buffer = heap.drain().collect();
                runs.spill(&mut buffer)?;
                bytes = 0;
                heap_mode = false;
            }
        } else {
            bytes += size;
            buffer.push(c);
            if bytes >= budget {
                runs.spill(&mut buffer)?;
                bytes = 0;
            }
        }
    }
    if heap_mode {
        for c in heap.into_sorted_vec() {
            writer.row(&plan.projections, &view.read_at(c)?)?;
        }
    } else {
        if !buffer.is_empty() {
            runs.spill(&mut buffer)?;
        }
        let paths = runs.final_paths()?;
        merge(&paths, |c| {
            if writer.count >= limit {
                return Ok(false);
            }
            writer.row(&plan.projections, &view.read_at(c)?)?;
            Ok(true)
        })?;
    }
    writer.finish()?;
    Ok(result)
}

#[cfg(test)]
mod sort_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn binary_runs_preserve_text_exact_distance_and_high_position_bits() {
        let root = std::env::temp_dir().join(format!(
            "mv-merge-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, AtomicOrdering::Relaxed),
        ));
        fs::create_dir(&root).unwrap();
        let mut runs = Runs::new(root.join("sort"));
        let keys = vec![
            (SortValue::Text("你\0\n\"\\".repeat(1024)), true),
            (SortValue::Int(i64::MIN), false),
            (
                SortValue::Distance(SquaredL2::between(
                    &[f32::MAX, f32::from_bits(1)],
                    &[-f32::MAX, 0.0],
                )),
                true,
            ),
        ];
        let offset = (1 << 63) + 7;
        let mut rows = vec![Candidate {
            keys: keys.clone(),
            primary: i64::MAX,
            position: Position::Committed(offset),
        }];
        runs.spill(&mut rows).unwrap();
        let paths = runs.final_paths().unwrap();
        let mut seen = 0;
        merge(&paths, |candidate| {
            assert_eq!(candidate.keys, keys);
            assert_eq!(candidate.primary, i64::MAX);
            assert!(matches!(candidate.position, Position::Committed(n) if n == offset));
            seen += 1;
            Ok(true)
        })
        .unwrap();
        assert_eq!(seen, 1);
        drop(runs);
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn shrinking_fan_in_merges_pending_narrow_runs_in_bounded_batches() {
        let root = std::env::temp_dir().join(format!(
            "mv-merge-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, AtomicOrdering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let mut runs = Runs::new(root.join("sort"));
        // Scale the same production algorithm down to make this regression cheap.
        runs.head_budget = 4096;
        let mut expected = Vec::new();
        for key in 0..33 {
            let text = if key < 15 {
                format!("a{key:02}")
            } else {
                format!(
                    "{}{}",
                    "z".repeat(4096),
                    if key % 2 == 0 { 'a' } else { 'b' }
                )
            };
            expected.push((text.clone(), key));
            let mut buffer = vec![Candidate {
                keys: vec![(SortValue::Text(text), false)],
                primary: key,
                position: Position::Committed(0),
            }];
            runs.spill(&mut buffer).unwrap();
            if key == 14 {
                assert_eq!(runs.levels[0].len(), 15);
            }
        }
        assert_eq!(runs.fan_in(), 2);
        let paths = runs.final_paths().unwrap();
        assert!(paths.len() <= 2);
        let mut actual = Vec::new();
        merge(&paths, |c| {
            let (SortValue::Text(text), false) = c.keys.into_iter().next().unwrap() else {
                panic!("unexpected sort key")
            };
            actual.push((text, c.primary));
            Ok(true)
        })
        .unwrap();
        expected.sort();
        assert_eq!(actual, expected);
        drop(runs);
        assert_eq!(
            fs::read_dir(&root).unwrap().count(),
            0,
            "all intermediate runs must be reclaimed"
        );
        fs::remove_dir(root).unwrap();
    }
}
