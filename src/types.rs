use std::io::Write;

use serde::de::{Deserializer, SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::value::RawValue;

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
            Self::Vector(x) => Self::write_vector_json(x, out)?,
        }
        Ok(())
    }
    pub fn write_vector_json(vector: &[f32], out: &mut impl Write) -> Result<()> {
        let mut serializer = serde_json::Serializer::new(out);
        let mut seq = serializer.serialize_seq(Some(vector.len()))?;
        for value in vector {
            seq.serialize_element(&f64::from(*value))?;
        }
        seq.end()?;
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
    let invalid_array = |_| {
        Error::new(
            "invalid_vector",
            "vector must be a finite numeric JSON array",
        )
    };
    let mut input = serde_json::Deserializer::from_str(s);
    let values = input
        .deserialize_seq(VectorElements(dim))
        .map_err(invalid_array)?;
    input.end().map_err(invalid_array)?;
    values
}

struct VectorElements(usize);

impl<'de> Visitor<'de> for VectorElements {
    type Value = Result<Vec<f32>>;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a numeric JSON array")
    }

    fn visit_seq<A: SeqAccess<'de>>(
        self,
        mut seq: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut count = 0;
        let mut values = Ok(Vec::with_capacity(self.0.min(256)));
        while let Some(raw) = seq.next_element::<&RawValue>()? {
            if count < self.0
                && let Ok(vector) = &mut values
            {
                match vector_number(raw.get()) {
                    Ok(number) => vector.push(number),
                    Err(error) => values = Err(error),
                }
            }
            count += 1;
        }
        // Complete syntax validation and count before exposing an element error.
        Ok(if count == self.0 {
            values
        } else {
            Err(Error::new("dimension_mismatch", "vector dimension differs"))
        })
    }
}

fn vector_number(raw: &str) -> Result<f32> {
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
    // Parse the original decimal directly so there is only one rounding.
    let number = raw
        .parse::<f32>()
        .map_err(|_| Error::new("invalid_vector", "invalid float32 number"))?;
    if number.is_finite() {
        Ok(number)
    } else {
        Err(Error::new(
            "invalid_vector",
            "value cannot be stored as finite float32",
        ))
    }
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

#[cfg(test)]
mod tests {
    use super::vector;

    #[test]
    fn vector_preserves_error_order_and_float32_rounding() {
        for (input, dim, code, message) in [
            (
                "[false,1]",
                1,
                "dimension_mismatch",
                "vector dimension differs",
            ),
            (
                "[false,1,]",
                1,
                "invalid_vector",
                "vector must be a finite numeric JSON array",
            ),
            (
                "[false] 0",
                1,
                "invalid_vector",
                "vector must be a finite numeric JSON array",
            ),
            (
                "[false]",
                1,
                "invalid_vector",
                "vector elements must be JSON numbers",
            ),
            (
                "[3.5e38]",
                1,
                "invalid_vector",
                "value cannot be stored as finite float32",
            ),
        ] {
            let error = vector(input, dim).unwrap_err();
            assert_eq!(
                (error.code, error.message.as_str()),
                (code, message),
                "{input}"
            );
        }
        let numbers = vector(
            "[-0,1.401298464324817e-45,1.000000059604644775390625,1.000000059604644775390626]",
            4,
        )
        .unwrap();
        assert_eq!(
            numbers.iter().map(|f| f.to_bits()).collect::<Vec<_>>(),
            [0x8000_0000, 1, 0x3f80_0000, 0x3f80_0001]
        );
    }
}
