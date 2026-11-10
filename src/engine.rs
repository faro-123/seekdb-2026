use std::collections::BTreeSet;
use std::path::Path;

use crate::query::{self, Plan, Predicates, QueryFile};
use crate::sql::{self, Statement};
use crate::storage::{Catalog, Changes, LocatedRow, Op, Store};
use crate::types::{DataType, Schema, coerce};
use crate::{Error, Result};

pub(crate) enum Outcome {
    Ok,
    Affected(u64),
    Query(QueryFile),
}
pub(crate) struct Engine {
    store: Store,
    transaction: Option<Changes>,
}
impl Engine {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            store: Store::open(path)?,
            transaction: None,
        })
    }
    fn catalog(&self) -> &Catalog {
        self.transaction
            .as_ref()
            .map_or(&self.store.catalog, |tx| &tx.catalog)
    }
    fn schema(&self, table: &str) -> Result<Schema> {
        self.catalog()
            .get(table)
            .cloned()
            .ok_or_else(|| Error::new("unknown_table", table))
    }
    fn exists(&mut self, table: &str, key: i64) -> Result<bool> {
        if let Some(tx) = &mut self.transaction
            && let Some(e) = tx.get(table, key)?
        {
            return Ok(!e.deleted);
        }
        Ok(self.store.get(table, key)?.is_some_and(|e| !e.deleted))
    }
    pub fn rollback(&mut self) {
        self.transaction = None;
    }
    pub fn execute(&mut self, input: &str) -> Result<Outcome> {
        let statement = sql::parse(input)?;
        match statement {
            Statement::Begin => {
                if self.transaction.is_some() {
                    return Err(Error::new(
                        "transaction_active",
                        "nested transactions unsupported",
                    ));
                }
                self.transaction = Some(self.store.changes(self.catalog().clone())?);
                return Ok(Outcome::Ok);
            }
            Statement::Commit => {
                if let Some(tx) = self.transaction.take() {
                    self.store.commit(&tx)?;
                }
                return Ok(Outcome::Ok);
            }
            Statement::Rollback => {
                self.rollback();
                return Ok(Outcome::Ok);
            }
            Statement::Select(select) => {
                let schema = self.schema(&select.table)?;
                let plan = Plan::bind(&schema, &select)?;
                return query::execute(plan, &schema, self.transaction.as_ref(), &mut self.store)
                    .map(Outcome::Query);
            }
            _ => {}
        }
        let mut changes = self.store.changes(self.catalog().clone())?;
        let outcome = match statement {
            Statement::Create(schema) => {
                if self.catalog().contains_key(&schema.name) {
                    return Err(Error::new("table_exists", "table already exists"));
                }
                changes.push(Op::Schema(schema))?;
                Outcome::Ok
            }
            Statement::Index {
                name,
                table,
                column,
            } => {
                let mut schema = self.schema(&table)?;
                let i = schema.column(&column)?;
                if !matches!(schema.columns[i].kind, DataType::Vector(_)) {
                    return Err(Error::new(
                        "type_error",
                        "vector index requires VECTOR column",
                    ));
                }
                if self
                    .catalog()
                    .values()
                    .any(|s| s.indexes.iter().any(|(n, _)| *n == name))
                {
                    return Err(Error::new("index_exists", "index name already exists"));
                }
                // Replay builds the flat index only when this DDL becomes durable.
                schema.indexes.push((name, i));
                changes.push(Op::Schema(schema))?;
                Outcome::Ok
            }
            Statement::Insert {
                table,
                columns,
                rows,
            } => {
                let schema = self.schema(&table)?;
                let mapping = if let Some(cols) = columns {
                    let ids: Vec<_> = cols
                        .iter()
                        .map(|n| schema.column(n))
                        .collect::<Result<_>>()?;
                    if ids.len() != schema.columns.len()
                        || ids.iter().copied().collect::<BTreeSet<_>>().len() != ids.len()
                    {
                        return Err(Error::new(
                            "column_count",
                            "all columns exactly once required",
                        ));
                    }
                    ids
                } else {
                    (0..schema.columns.len()).collect()
                };
                let mut count = 0;
                for literals in rows {
                    if literals.len() != mapping.len() {
                        return Err(Error::new(
                            "column_count",
                            "value count differs from column count",
                        ));
                    }
                    let mut row = Vec::with_capacity(schema.columns.len());
                    for (i, col) in schema.columns.iter().enumerate() {
                        let source = mapping.iter().position(|n| *n == i).unwrap();
                        row.push(coerce(&literals[source], &col.kind)?);
                    }
                    let key = row[schema.primary].int()?;
                    if self.exists(&table, key)?
                        || changes.get(&table, key)?.is_some_and(|e| !e.deleted)
                    {
                        return Err(Error::new("duplicate_key", "duplicate primary key"));
                    }
                    changes.push(Op::Put {
                        table: table.clone(),
                        key,
                        row,
                    })?;
                    count += 1;
                }
                Outcome::Affected(count)
            }
            Statement::Update {
                table,
                assignments,
                conditions,
            } => {
                let schema = self.schema(&table)?;
                let predicates = Predicates::bind(&schema, &conditions)?;
                let mut bound = Vec::new();
                let mut used = BTreeSet::new();
                for (name, lit) in assignments {
                    let i = schema.column(&name)?;
                    if i == schema.primary {
                        return Err(Error::new(
                            "unsupported_sql",
                            "primary key updates unsupported",
                        ));
                    }
                    if !used.insert(i) {
                        return Err(Error::new("unsupported_sql", "duplicate assignment"));
                    }
                    bound.push((i, coerce(&lit, &schema.columns[i].kind)?));
                }
                let view = self.store.view(&table, self.transaction.as_ref())?;
                let mut count = 0;
                for record in view {
                    let LocatedRow {
                        key,
                        values: mut row,
                        ..
                    } = record?;
                    if predicates.matches(&row) {
                        for (i, v) in &bound {
                            row[*i] = v.clone();
                        }
                        changes.push(Op::Put {
                            table: table.clone(),
                            key,
                            row,
                        })?;
                        count += 1;
                    }
                }
                Outcome::Affected(count)
            }
            Statement::Delete { table, conditions } => {
                let schema = self.schema(&table)?;
                let predicates = Predicates::bind(&schema, &conditions)?;
                let view = self.store.view(&table, self.transaction.as_ref())?;
                let mut count = 0;
                for record in view {
                    let LocatedRow {
                        key, values: row, ..
                    } = record?;
                    if predicates.matches(&row) {
                        changes.push(Op::Delete {
                            table: table.clone(),
                            key,
                        })?;
                        count += 1;
                    }
                }
                Outcome::Affected(count)
            }
            _ => unreachable!("control and query statements handled before staging"),
        };
        if let Some(tx) = &mut self.transaction {
            tx.merge(&changes)?;
        } else {
            self.store.commit(&changes)?;
        }
        Ok(outcome)
    }
}
