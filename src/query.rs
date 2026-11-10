//! Typed plans, filtered exact distances, and bounded external sorting.
use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeSet, BinaryHeap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

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
    fn read_at(&mut self, candidate: &Candidate) -> Result<QueryRow> {
        match (self, candidate.position) {
            (Self::Table { source, .. }, position) => Ok(QueryRow::Full(source.read_at(position)?)),
            (
                Self::Vector {
                    source,
                    primary,
                    embedding,
                },
                Position::Vector(offset),
            ) => Ok(indexed_row(
                candidate.primary,
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
#[derive(Clone, Debug, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Serialize, Deserialize)]
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
}
impl Runs {
    fn new(base: PathBuf) -> Self {
        Self {
            base,
            sequence: 0,
            levels: Vec::new(),
            artifacts: BTreeSet::new(),
        }
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
        if self.levels[level].len() == 16 {
            let paths = std::mem::take(&mut self.levels[level]);
            let path = self.path();
            let mut out = BufWriter::new(File::create(&path)?);
            merge(&paths, |c| {
                serde_json::to_writer(&mut out, &c)?;
                out.write_all(b"\n")?;
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
        buffer.sort_unstable();
        let path = self.path();
        let mut out = BufWriter::new(File::create(&path)?);
        for c in buffer.drain(..) {
            serde_json::to_writer(&mut out, &c)?;
            out.write_all(b"\n")?;
        }
        out.flush()?;
        drop(out);
        self.insert(path, 0)
    }
    fn final_paths(&mut self) -> Result<Vec<PathBuf>> {
        let mut paths: Vec<_> = self.levels.iter_mut().flat_map(std::mem::take).collect();
        while paths.len() > 16 {
            let mut next = Vec::new();
            for batch in paths.chunks(16) {
                let path = self.path();
                let mut out = BufWriter::new(File::create(&path)?);
                merge(batch, |c| {
                    serde_json::to_writer(&mut out, &c)?;
                    out.write_all(b"\n")?;
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
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        Ok(None)
    } else {
        Ok(Some(serde_json::from_str(&line)?))
    }
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
    store: &Store,
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
    let Some(table) = cache.get(store, schema, *embedding, &fields[..count])? else {
        return Ok(false);
    };
    let hits = table.select(q, k, |slot| plan.predicates.matches_hot(table, slot));
    let covered = plan.projections.iter().all(|expr| {
        let column = expr.column_index();
        column == schema.primary || column == *embedding
    });
    let mut source = if covered || hits.is_empty() {
        None
    } else {
        Some(store.view(&schema.name, None)?)
    };
    for hit in hits {
        let row = table.row(hit.slot);
        if let Some(source) = &mut source {
            let projected = QueryRow::Full(source.read_at(Position::Committed(row.offset))?);
            writer.row(&plan.projections, &projected)?;
        } else {
            let vector = table.vector(hit.slot);
            writer.row_with(&plan.projections, |projection, out| {
                match projection {
                    BoundExpr::Column(i) if *i == schema.primary => {
                        serde_json::to_writer(out, &row.key)?;
                    }
                    BoundExpr::Column(_) => Value::write_vector_json(vector, out)?,
                    BoundExpr::Distance(_, query) => {
                        let distance = if query == q {
                            hit.distance()
                        } else {
                            SquaredL2::between(vector, query).distance()
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
            writer.row(&plan.projections, &view.read_at(&c)?)?;
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
            writer.row(&plan.projections, &view.read_at(&c)?)?;
            Ok(true)
        })?;
    }
    writer.finish()?;
    Ok(result)
}
