use std::io::Write;

use serde::ser::SerializeSeq;
use serde::{Deserialize, Serialize, Serializer};

use crate::{Error, Result};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum DataType {
    Bigint,
    Varchar(usize),
    Vector(usize),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Column {
    pub name: String,
    pub kind: DataType,
    pub primary: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Schema {
    pub name: String,
    pub columns: Vec<Column>,
    pub primary: usize,
    pub indexes: Vec<(String, usize)>,
}
impl Schema {
    pub fn column(&self, name: &str) -> Result<usize> {
        self.columns
            .iter()
            .position(|c| c.name == name)
            .ok_or_else(|| Error::new("unknown_column", name))
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Literal {
    Number(String),
    Text(String),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum Value {
    Int(i64),
    Text(String),
    Vector(Vec<f32>),
}
impl Value {
    pub fn write_json(&self, out: &mut impl Write) -> Result<()> {
        match self {
            Self::Int(x) => serde_json::to_writer(out, x)?,
            Self::Text(x) => serde_json::to_writer(out, x)?,
            Self::Vector(x) => {
                let mut serializer = serde_json::Serializer::new(out);
                let mut seq = serializer.serialize_seq(Some(x.len()))?;
                for value in x {
                    seq.serialize_element(&f64::from(*value))?;
                }
                seq.end()?;
            }
        }
        Ok(())
    }
    pub fn int(&self) -> Result<i64> {
        if let Self::Int(v) = self {
            Ok(*v)
        } else {
            Err(Error::new("type_error", "BIGINT required"))
        }
    }
}

pub(crate) fn vector(s: &str, dim: usize) -> Result<Vec<f32>> {
    let input: Vec<&serde_json::value::RawValue> = serde_json::from_str(s).map_err(|_| {
        Error::new(
            "invalid_vector",
            "vector must be a finite numeric JSON array",
        )
    })?;
    if input.len() != dim {
        return Err(Error::new("dimension_mismatch", "vector dimension differs"));
    }
    input
        .into_iter()
        .map(|x| {
            // Preserve decimal digits until the single rounding to float32.
            let raw = x.get();
            if !raw
                .as_bytes()
                .first()
                .is_some_and(|b| b.is_ascii_digit() || *b == b'-')
            {
                return Err(Error::new(
                    "invalid_vector",
                    "vector elements must be JSON numbers",
                ));
            }
            let f = raw
                .parse::<f32>()
                .map_err(|_| Error::new("invalid_vector", "invalid float32 number"))?;
            if f.is_finite() {
                Ok(f)
            } else {
                Err(Error::new(
                    "invalid_vector",
                    "value cannot be stored as finite float32",
                ))
            }
        })
        .collect()
}

pub(crate) fn coerce(lit: &Literal, kind: &DataType) -> Result<Value> {
    match (lit, kind) {
        (Literal::Number(s), DataType::Bigint) => s.parse::<i64>().map(Value::Int).map_err(|_| {
            Error::new(
                "invalid_bigint",
                "integer outside signed 64-bit range or non-integer syntax",
            )
        }),
        (Literal::Text(s), DataType::Varchar(n)) if s.chars().count() <= *n => {
            Ok(Value::Text(s.clone()))
        }
        (Literal::Text(_), DataType::Varchar(_)) => Err(Error::new(
            "varchar_too_long",
            "Unicode character limit exceeded",
        )),
        (Literal::Text(s), DataType::Vector(n)) => vector(s, *n).map(Value::Vector),
        _ => Err(Error::new(
            "type_error",
            "literal type differs from column type",
        )),
    }
}
