//! Bounded blocks of lossless, schema-compatible PUT records.
use crate::types::{DataType, Schema};
use crate::vector_codec::{HEADER_BYTES, Header};
use crate::{Error, Result};

pub(crate) const TAG: u8 = 4;
pub(crate) const TARGET_BYTES: usize = 64 << 10;
pub(crate) const MAX_INPUT_BYTES: usize = 192 << 10;
const MAX_ROWS: usize = 256;
const MAX_COLUMNS: usize = 64;

// Body: tag, u16 table length, u16 row count, u8 column count, u8 primary
// ordinal, table bytes, u16 cumulative row ends, column descriptors, rows.
// The directory is relative to the row area. Encoded rows keep vector fields
// at their original tag and preserve record order, including repeated keys.
const FIXED_HEADER: usize = 7;
const INT: u8 = 0;
const FOR: u8 = 1;
const TEXT: u8 = 2;
const DICT: u8 = 3;
const AFFIX: u8 = 4;
const RLE: u8 = 5;
const VECTOR: u8 = 6;

fn invalid(reason: &'static str) -> Error {
    Error::new("storage_error", reason)
}

fn take<'a>(input: &mut &'a [u8], len: usize) -> Result<&'a [u8]> {
    let (value, rest) = input
        .split_at_checked(len)
        .ok_or_else(|| invalid("truncated commit block field"))?;
    *input = rest;
    Ok(value)
}

fn fixed<const N: usize>(input: &mut &[u8]) -> Result<[u8; N]> {
    take(input, N)?
        .try_into()
        .map_err(|_| invalid("invalid commit block field length"))
}

fn byte(input: &mut &[u8]) -> Result<u8> {
    Ok(fixed::<1>(input)?[0])
}

fn short(input: &mut &[u8]) -> Result<usize> {
    Ok(usize::from(u16::from_le_bytes(fixed(input)?)))
}

fn length(input: &mut &[u8]) -> Result<usize> {
    usize::try_from(u32::from_le_bytes(fixed(input)?))
        .map_err(|_| invalid("commit block record length out of range"))
}

fn utf8(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes).map_err(|_| invalid("invalid UTF-8 in commit block"))
}

fn le_short(out: &mut Vec<u8>, value: usize) -> Result<()> {
    let value = u16::try_from(value).map_err(|_| invalid("commit block field too long"))?;
    out.extend_from_slice(&value.to_le_bytes());
    Ok(())
}

fn small_byte(value: usize) -> Result<u8> {
    u8::try_from(value).map_err(|_| invalid("commit block count out of range"))
}

#[derive(Clone, Copy)]
enum Field<'a> {
    Int(i64),
    Text(&'a [u8]),
    Vector { encoded: &'a [u8], dimension: usize },
}

impl Field<'_> {
    fn kind(self) -> u8 {
        match self {
            Self::Int(_) => INT,
            Self::Text(_) => TEXT,
            Self::Vector { .. } => VECTOR,
        }
    }
}

fn field<'a>(input: &mut &'a [u8]) -> Result<Field<'a>> {
    let encoded = *input;
    match byte(input)? {
        0 => Ok(Field::Int(i64::from_le_bytes(fixed(input)?))),
        1 => {
            let n = length(input)?;
            Ok(Field::Text(take(input, n)?))
        }
        tag @ (2 | 3) => {
            let dimension = length(input)?;
            if dimension == 0 {
                return Err(invalid("empty commit block vector"));
            }
            let len = if tag == 2 {
                dimension
                    .checked_mul(4)
                    .ok_or_else(|| invalid("commit block vector length overflow"))?
            } else {
                Header::parse(&fixed::<HEADER_BYTES>(input)?).payload_len(dimension)?
            };
            take(input, len)?;
            Ok(Field::Vector {
                encoded: &encoded[..encoded.len() - input.len()],
                dimension,
            })
        }
        _ => Err(invalid("unknown commit block field tag")),
    }
}

fn record(input: &[u8], offset: usize) -> Result<(&[u8], usize)> {
    let mut tail = input
        .get(offset..)
        .ok_or_else(|| invalid("invalid commit block record offset"))?;
    let len = length(&mut tail)?;
    let body = take(&mut tail, len)?;
    if body.is_empty() {
        return Err(invalid("empty commit block record"));
    }
    Ok((body, input.len() - tail.len()))
}

struct Row<'a> {
    table: &'a str,
    fields: &'a [u8],
    primary: usize,
    columns: usize,
    shape: [u8; MAX_COLUMNS],
    dimension: Option<usize>,
}

impl Row<'_> {
    fn matches(&self, other: &Self) -> bool {
        self.table == other.table
            && self.primary == other.primary
            && self.columns == other.columns
            && self.shape == other.shape
            && self.dimension == other.dimension
    }
}

fn legacy_row<'a>(
    mut body: &'a [u8],
    primary: &impl Fn(&str) -> Option<usize>,
) -> Result<Option<Row<'a>>> {
    if body.len() > TARGET_BYTES - 4 || byte(&mut body)? != 1 {
        return Ok(None);
    }
    let table_len = length(&mut body)?;
    let table = utf8(take(&mut body, table_len)?)?;
    let key = i64::from_le_bytes(fixed(&mut body)?);
    let columns = length(&mut body)?;
    let Some(primary) = primary(table) else {
        return Ok(None);
    };
    if columns > MAX_COLUMNS || primary >= columns {
        return Ok(None);
    }
    let fields = body;
    let mut shape = [0; MAX_COLUMNS];
    let mut dimension = None;
    for (ordinal, kind) in shape[..columns].iter_mut().enumerate() {
        let value = field(&mut body)?;
        *kind = value.kind();
        if ordinal == primary && !matches!(value, Field::Int(n) if n == key) {
            return Ok(None);
        }
        match value {
            Field::Text(text) => {
                utf8(text)?;
            }
            Field::Vector {
                dimension: size, ..
            } => {
                if dimension.replace(size).is_some() {
                    return Ok(None);
                }
            }
            Field::Int(_) => {}
        }
    }
    if !body.is_empty() {
        return Err(invalid("trailing legacy PUT bytes in commit block input"));
    }
    Ok(Some(Row {
        table,
        fields,
        primary,
        columns,
        shape,
        dimension,
    }))
}

enum Plan<'a> {
    Int,
    For { base: i64, width: u8 },
    Text,
    Dict(Box<[&'a [u8]]>),
    Affix { prefix: &'a [u8], suffix: &'a [u8] },
    Rle,
    Vector,
}

fn runs(mut value: &[u8]) -> impl Iterator<Item = (u8, u8)> {
    std::iter::from_fn(move || {
        let first = *value.first()?;
        let run = value.iter().take(255).take_while(|&&b| b == first).count();
        value = &value[run..];
        // The count above includes the first byte and never exceeds 255.
        Some((run as u8, first))
    })
}

fn text_plan<'a>(values: &[&'a [u8]]) -> (Plan<'a>, usize) {
    let rows = values.len();
    let total: usize = values.iter().map(|text| text.len()).sum();
    let mut best = (Plan::Text, 1 + 2 * rows + total);
    let first = values[0];
    let mut prefix_len = first.len();
    for text in &values[1..] {
        prefix_len = first[..prefix_len]
            .iter()
            .zip(*text)
            .take_while(|(a, b)| a == b)
            .count();
    }
    let mut suffix_len = first.len() - prefix_len;
    for text in &values[1..] {
        suffix_len = first[first.len() - suffix_len..]
            .iter()
            .rev()
            .zip(text[prefix_len..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
    }
    let affix_len = prefix_len + suffix_len;
    let affix_cost = 1 + 4 + affix_len + 2 * rows + total - affix_len * rows;
    if affix_cost < best.1 {
        best = (
            Plan::Affix {
                prefix: &first[..prefix_len],
                suffix: &first[first.len() - suffix_len..],
            },
            affix_cost,
        );
    }
    let rle_cost = 1
        + 2 * rows
        + 2 * values
            .iter()
            .map(|value| runs(value).count())
            .sum::<usize>();
    if rle_cost < best.1 {
        best = (Plan::Rle, rle_cost);
    }

    let mut sorted = [&[][..]; MAX_ROWS];
    sorted[..rows].copy_from_slice(values);
    sorted[..rows].sort_unstable();
    let mut entries = 1;
    for i in 1..rows {
        if sorted[i] != sorted[entries - 1] {
            sorted[entries] = sorted[i];
            entries += 1;
        }
    }
    let bytes: usize = sorted[..entries].iter().map(|entry| entry.len()).sum();
    let dict_cost = 1 + 1 + 2 * entries + bytes + rows;
    if entries <= usize::from(u8::MAX) && dict_cost < best.1 {
        best = (Plan::Dict(sorted[..entries].into()), dict_cost);
    }
    best
}

fn delta(value: i64, base: i64) -> Result<u64> {
    u64::try_from(i128::from(value) - i128::from(base))
        .map_err(|_| invalid("commit block integer out of range"))
}

fn plan<'a>(rows: &[Row<'a>]) -> Result<(Vec<Plan<'a>>, usize)> {
    let first = &rows[0];
    let mut inputs = [&[][..]; MAX_ROWS];
    for (input, row) in inputs.iter_mut().zip(rows) {
        *input = row.fields;
    }
    let inputs = &mut inputs[..rows.len()];
    let mut plans = Vec::with_capacity(first.columns);
    let mut bytes = FIXED_HEADER + first.table.len() + 2 * rows.len();
    for &kind in &first.shape[..first.columns] {
        let (mode, size) = match kind {
            INT => {
                let mut min = i64::MAX;
                let mut max = i64::MIN;
                for input in &mut *inputs {
                    let Field::Int(value) = field(input)? else {
                        return Err(invalid("commit block integer shape changed"));
                    };
                    min = min.min(value);
                    max = max.max(value);
                }
                let width = (u64::BITS - delta(max, min)?.leading_zeros()).div_ceil(8) as u8;
                let raw_bytes = 1 + 8 * rows.len();
                let for_bytes = 1 + 8 + 1 + usize::from(width) * rows.len();
                if for_bytes < raw_bytes {
                    (Plan::For { base: min, width }, for_bytes)
                } else {
                    (Plan::Int, raw_bytes)
                }
            }
            TEXT => {
                let mut values = [&[][..]; MAX_ROWS];
                for (value, input) in values.iter_mut().zip(&mut *inputs) {
                    let Field::Text(text) = field(input)? else {
                        return Err(invalid("commit block text shape changed"));
                    };
                    *value = text;
                }
                text_plan(&values[..rows.len()])
            }
            VECTOR => {
                let mut total = 1;
                for input in &mut *inputs {
                    let Field::Vector { encoded, .. } = field(input)? else {
                        return Err(invalid("commit block vector shape changed"));
                    };
                    total += encoded.len();
                }
                (Plan::Vector, total)
            }
            _ => return Err(invalid("unknown commit block column kind")),
        };
        plans.push(mode);
        bytes += size;
    }
    Ok((plans, bytes))
}

fn descriptor(out: &mut Vec<u8>, mode: &Plan<'_>) -> Result<()> {
    match mode {
        Plan::Int => out.push(INT),
        Plan::For { base, width } => {
            out.push(FOR);
            out.extend_from_slice(&base.to_le_bytes());
            out.push(*width);
        }
        Plan::Text => out.push(TEXT),
        Plan::Dict(entries) => {
            out.push(DICT);
            out.push(small_byte(entries.len())?);
            let mut end = 0;
            for value in entries {
                end += value.len();
                le_short(out, end)?;
            }
            for value in entries {
                out.extend_from_slice(value);
            }
        }
        Plan::Affix { prefix, suffix } => {
            out.push(AFFIX);
            le_short(out, prefix.len())?;
            le_short(out, suffix.len())?;
            out.extend_from_slice(prefix);
            out.extend_from_slice(suffix);
        }
        Plan::Rle => out.push(RLE),
        Plan::Vector => out.push(VECTOR),
    }
    Ok(())
}

fn encode_row(mut input: &[u8], plans: &[Plan<'_>], out: &mut Vec<u8>) -> Result<()> {
    for mode in plans {
        match (mode, field(&mut input)?) {
            (Plan::Int, Field::Int(value)) => out.extend_from_slice(&value.to_le_bytes()),
            (Plan::For { base, width }, Field::Int(value)) => {
                out.extend_from_slice(&delta(value, *base)?.to_le_bytes()[..usize::from(*width)]);
            }
            (Plan::Text, Field::Text(text)) => {
                le_short(out, text.len())?;
                out.extend_from_slice(text);
            }
            (Plan::Dict(entries), Field::Text(text)) => {
                let index = entries
                    .binary_search(&text)
                    .map_err(|_| invalid("commit block dictionary value missing"))?;
                out.push(small_byte(index)?);
            }
            (Plan::Affix { prefix, suffix }, Field::Text(text)) => {
                let middle = &text[prefix.len()..text.len() - suffix.len()];
                le_short(out, middle.len())?;
                out.extend_from_slice(middle);
            }
            (Plan::Rle, Field::Text(text)) => {
                le_short(out, 2 * runs(text).count())?;
                for (count, value) in runs(text) {
                    out.extend_from_slice(&[count, value]);
                }
            }
            (Plan::Vector, Field::Vector { encoded, .. }) => out.extend_from_slice(encoded),
            _ => return Err(invalid("commit block field disagrees with column plan")),
        }
    }
    if !input.is_empty() {
        return Err(invalid("trailing legacy PUT fields in commit block input"));
    }
    Ok(())
}

fn encode(rows: &[Row<'_>], plans: &[Plan<'_>], body_len: usize, out: &mut Vec<u8>) -> Result<()> {
    let first = &rows[0];
    let start = out.len();
    let len = u32::try_from(body_len).map_err(|_| invalid("commit block length overflow"))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.push(TAG);
    le_short(out, first.table.len())?;
    le_short(out, rows.len())?;
    out.push(small_byte(first.columns)?);
    out.push(small_byte(first.primary)?);
    out.extend_from_slice(first.table.as_bytes());
    let directory = out.len();
    out.resize(directory + 2 * rows.len(), 0);
    for mode in plans {
        descriptor(out, mode)?;
    }
    let row_start = out.len();
    for (i, row) in rows.iter().enumerate() {
        encode_row(row.fields, plans, out)?;
        let end = u16::try_from(out.len() - row_start)
            .map_err(|_| invalid("commit block directory offset overflow"))?;
        out[directory + 2 * i..directory + 2 * i + 2].copy_from_slice(&end.to_le_bytes());
    }
    if out.len() - start != 4 + body_len {
        return Err(invalid("commit block length disagrees with preflight"));
    }
    Ok(())
}

pub(crate) fn pack_payload(
    input: &[u8],
    primary: impl Fn(&str) -> Option<usize>,
) -> Result<Option<Vec<u8>>> {
    if input.len() > MAX_INPUT_BYTES {
        return Ok(None);
    }
    let mut out = None::<Vec<u8>>;
    // Space at peak: input + output <= 384 KiB, 256 Row descriptors,
    // 64 plans with at most 255 borrowed dictionary entries each (255 KiB on
    // x86-64), and a few 256-element slice arrays. Encoding emits directly.
    let mut rows = Vec::with_capacity(MAX_ROWS);
    let mut offset = 0;
    while offset < input.len() {
        rows.clear();
        let (body, mut end) = record(input, offset)?;
        if let Some(row) = legacy_row(body, &primary)? {
            rows.push(row);
            while rows.len() < MAX_ROWS && end < input.len() {
                let (body, next) = record(input, end)?;
                if next - offset > TARGET_BYTES {
                    break;
                }
                let Some(row) = legacy_row(body, &primary)? else {
                    break;
                };
                if !rows[0].matches(&row) {
                    break;
                }
                rows.push(row);
                end = next;
            }
        }

        if rows.len() >= 2 {
            let (plans, bytes) = plan(&rows)?;
            if 4 + bytes < end - offset {
                let output = out.get_or_insert_with(|| {
                    let mut output = Vec::with_capacity(input.len());
                    output.extend_from_slice(&input[..offset]);
                    output
                });
                encode(&rows, &plans, bytes, output)?;
                offset = end;
                continue;
            }
        }
        if let Some(output) = &mut out {
            output.extend_from_slice(&input[offset..end]);
        }
        offset = end;
    }
    Ok(out)
}

#[derive(Clone, Copy)]
enum Column<'a> {
    Int,
    For { base: i64, width: usize },
    Text,
    Dict { ends: &'a [u8], values: &'a [u8] },
    Affix { prefix: &'a [u8], suffix: &'a [u8] },
    Rle,
    Vector,
}

fn end_at(directory: &[u8], index: usize) -> Result<usize> {
    let start = index
        .checked_mul(2)
        .ok_or_else(|| invalid("commit block directory index overflow"))?;
    let mut entry = directory
        .get(start..)
        .ok_or_else(|| invalid("commit block directory index out of range"))?;
    short(&mut entry)
}

fn directory_end(directory: &[u8], rows: usize) -> Result<usize> {
    let mut last = 0;
    for index in 0..rows {
        let end = end_at(directory, index)?;
        if end < last {
            return Err(invalid("unordered commit block directory"));
        }
        last = end;
    }
    Ok(last)
}

fn column<'a>(input: &mut &'a [u8]) -> Result<Column<'a>> {
    match byte(input)? {
        INT => Ok(Column::Int),
        FOR => {
            let base = i64::from_le_bytes(fixed(input)?);
            let width = usize::from(byte(input)?);
            if width >= 8 {
                return Err(invalid("invalid commit block integer width"));
            }
            Ok(Column::For { base, width })
        }
        TEXT => Ok(Column::Text),
        DICT => {
            let n = usize::from(byte(input)?);
            if n == 0 {
                return Err(invalid("empty commit block dictionary"));
            }
            let ends = take(input, 2 * n)?;
            let values = take(input, directory_end(ends, n)?)?;
            Ok(Column::Dict { ends, values })
        }
        AFFIX => {
            let prefix_len = short(input)?;
            let suffix_len = short(input)?;
            Ok(Column::Affix {
                prefix: take(input, prefix_len)?,
                suffix: take(input, suffix_len)?,
            })
        }
        RLE => Ok(Column::Rle),
        VECTOR => Ok(Column::Vector),
        _ => Err(invalid("unknown commit block column mode")),
    }
}

pub(crate) struct Block<'a> {
    table: &'a [u8],
    directory: &'a [u8],
    columns: [Column<'a>; MAX_COLUMNS],
    column_count: usize,
    primary: usize,
    rows: &'a [u8],
    row_offset: usize,
}

pub(crate) struct RestoredRow {
    pub len: usize,
    pub vector_at: Option<usize>,
}

impl<'a> Block<'a> {
    pub fn parse(body: &'a [u8]) -> Result<Self> {
        let mut input = body;
        if body.len() >= TARGET_BYTES || byte(&mut input)? != TAG {
            return Err(invalid("invalid commit block header"));
        }
        let table_len = short(&mut input)?;
        let count = short(&mut input)?;
        let column_count = usize::from(byte(&mut input)?);
        let primary = usize::from(byte(&mut input)?);
        if !(2..=MAX_ROWS).contains(&count) || column_count > MAX_COLUMNS || primary >= column_count
        {
            return Err(invalid("invalid commit block counts"));
        }
        let table = take(&mut input, table_len)?;
        utf8(table)?;
        let directory = take(&mut input, 2 * count)?;
        let mut columns = [Column::Int; MAX_COLUMNS];
        let mut vector_count = 0;
        for mode in &mut columns[..column_count] {
            *mode = column(&mut input)?;
            vector_count += usize::from(matches!(mode, Column::Vector));
        }
        if vector_count > 1 || !matches!(columns[primary], Column::Int | Column::For { .. }) {
            return Err(invalid("invalid commit block primary or vector column"));
        }
        if directory_end(directory, count)? != input.len() {
            return Err(invalid("commit block directory does not cover row bytes"));
        }
        Ok(Self {
            table,
            directory,
            columns,
            column_count,
            primary,
            rows: input,
            row_offset: body.len() - input.len(),
        })
    }

    pub fn len(&self) -> usize {
        self.directory.len() / 2
    }

    pub fn table(&self) -> Result<&str> {
        utf8(self.table)
    }

    pub fn matches_schema(&self, schema: &Schema) -> bool {
        schema.primary == self.primary
            && schema.columns.len() == self.column_count
            && self.columns[..self.column_count]
                .iter()
                .zip(&schema.columns)
                .all(|(mode, column)| {
                    matches!(
                        (mode, &column.kind),
                        (Column::Int | Column::For { .. }, DataType::Bigint)
                            | (
                                Column::Text
                                    | Column::Dict { .. }
                                    | Column::Affix { .. }
                                    | Column::Rle,
                                DataType::Varchar(_)
                            )
                            | (Column::Vector, DataType::Vector(_))
                    )
                })
    }

    pub fn restore(&self, index: usize, out: &mut [u8]) -> Result<RestoredRow> {
        if index >= self.len() {
            return Err(invalid("commit block row index out of range"));
        }
        let start = if index == 0 {
            0
        } else {
            end_at(self.directory, index - 1)?
        };
        let end = end_at(self.directory, index)?;
        let mut input = &self.rows[start..end];
        let cap = out.len().min(TARGET_BYTES);
        let mut output = Output::new(&mut out[..cap]);
        output.push(&[1])?;
        output.length(self.table.len())?;
        output.push(self.table)?;
        let key_at = output.len;
        output.push(&[0; 8])?;
        output.length(self.column_count)?;
        let mut key = None;
        let mut vector_at = None;
        for (i, &mode) in self.columns[..self.column_count].iter().enumerate() {
            match mode {
                Column::Int | Column::For { .. } => {
                    let value = integer(mode, &mut input)?;
                    output.push(&[0])?;
                    output.push(&value.to_le_bytes())?;
                    if i == self.primary {
                        key = Some(value);
                    }
                }
                Column::Text => {
                    let len = short(&mut input)?;
                    output.text(&[take(&mut input, len)?])?;
                }
                Column::Dict { ends, values } => {
                    let index = usize::from(byte(&mut input)?);
                    let start = if index == 0 {
                        0
                    } else {
                        end_at(ends, index - 1)?
                    };
                    let end = end_at(ends, index)?;
                    output.text(&[&values[start..end]])?;
                }
                Column::Affix { prefix, suffix } => {
                    let len = short(&mut input)?;
                    let middle = take(&mut input, len)?;
                    output.text(&[prefix, middle, suffix])?;
                }
                Column::Rle => {
                    let len = short(&mut input)?;
                    output.rle(take(&mut input, len)?)?;
                }
                Column::Vector => {
                    let at = self.row_offset + end - input.len();
                    let Field::Vector { encoded, .. } = field(&mut input)? else {
                        return Err(invalid("invalid commit block vector field"));
                    };
                    vector_at = Some(at);
                    output.push(encoded)?;
                }
            }
        }
        if !input.is_empty() {
            return Err(invalid("trailing bytes in commit block row"));
        }
        let key = key.ok_or_else(|| invalid("missing commit block primary key"))?;
        output.bytes[key_at..key_at + 8].copy_from_slice(&key.to_le_bytes());
        Ok(RestoredRow {
            len: output.len,
            vector_at,
        })
    }
}

fn integer(mode: Column<'_>, input: &mut &[u8]) -> Result<i64> {
    match mode {
        Column::Int => Ok(i64::from_le_bytes(fixed(input)?)),
        Column::For { base, width } => {
            let mut bytes = [0; 8];
            bytes[..width].copy_from_slice(take(input, width)?);
            i64::try_from(i128::from(base) + i128::from(u64::from_le_bytes(bytes)))
                .map_err(|_| invalid("commit block integer delta overflow"))
        }
        _ => Err(invalid("commit block column is not an integer")),
    }
}

struct Output<'a> {
    bytes: &'a mut [u8],
    len: usize,
}

impl<'a> Output<'a> {
    fn new(bytes: &'a mut [u8]) -> Self {
        Self { bytes, len: 0 }
    }

    fn room(&mut self, n: usize) -> Result<&mut [u8]> {
        let end = self
            .len
            .checked_add(n)
            .filter(|&end| end <= self.bytes.len())
            .ok_or_else(|| invalid("restored commit block row too large"))?;
        let out = &mut self.bytes[self.len..end];
        self.len = end;
        Ok(out)
    }

    fn push(&mut self, data: &[u8]) -> Result<()> {
        self.room(data.len())?.copy_from_slice(data);
        Ok(())
    }

    fn length(&mut self, len: usize) -> Result<()> {
        let n = u32::try_from(len).map_err(|_| invalid("commit block field length overflow"))?;
        self.push(&n.to_le_bytes())
    }

    fn text(&mut self, parts: &[&[u8]]) -> Result<()> {
        self.push(&[1])?;
        self.length(parts.iter().map(|part| part.len()).sum())?;
        let start = self.len;
        for part in parts {
            self.push(part)?;
        }
        utf8(&self.bytes[start..self.len])?;
        Ok(())
    }

    fn rle(&mut self, data: &[u8]) -> Result<()> {
        let (runs, tail) = data.as_chunks::<2>();
        if !tail.is_empty() || runs.iter().any(|r| r[0] == 0) {
            return Err(invalid("invalid commit block text run"));
        }
        self.push(&[1])?;
        self.length(runs.iter().map(|r| usize::from(r[0])).sum())?;
        let start = self.len;
        for &[count, byte] in runs {
            self.room(usize::from(count))?.fill(byte);
        }
        utf8(&self.bytes[start..self.len])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(table: &str, id: i64, category: &str, text: &str, vector: &[u8]) -> Vec<u8> {
        let mut row = vec![1];
        row.extend_from_slice(&(table.len() as u32).to_le_bytes());
        row.extend_from_slice(table.as_bytes());
        row.extend_from_slice(&id.to_le_bytes());
        row.extend_from_slice(&5_u32.to_le_bytes());
        row.push(0);
        row.extend_from_slice(&id.to_le_bytes());
        row.push(0);
        row.extend_from_slice(&id.wrapping_mul(19).to_le_bytes());
        for value in [category, text] {
            row.push(1);
            row.extend_from_slice(&(value.len() as u32).to_le_bytes());
            row.extend_from_slice(value.as_bytes());
        }
        row.extend_from_slice(vector);
        let mut record = (row.len() as u32).to_le_bytes().to_vec();
        record.extend_from_slice(&row);
        record
    }

    fn raw_vector() -> Vec<u8> {
        let mut field = vec![2, 8, 0, 0, 0];
        for value in [0.0_f32, -0.0, 1.0, -4.0, 8.25, f32::MAX, f32::MIN, 0.125] {
            field.extend_from_slice(&value.to_le_bytes());
        }
        field
    }

    #[test]
    fn restores_legacy_put_bytes_and_preserves_vector_position() {
        let raw = raw_vector();
        let mut packed = vec![3, 8, 0, 0, 0];
        assert!(crate::vector_codec::try_pack(&[1.0_f32; 8], &mut packed));
        let rows: Vec<_> = (0..24)
            .map(|i| {
                let category = ["red", "green", "blue"][i % 3];
                let vector = if i % 2 == 0 { &raw } else { &packed };
                put(
                    "documents",
                    i64::MIN + i as i64,
                    category,
                    &format!("é_共用_{i}_suffix_ß"),
                    vector,
                )
            })
            .collect();
        let input = rows.concat();
        let output = pack_payload(&input, |_| Some(0)).unwrap().unwrap();
        assert!(output.len() < input.len());
        let block = Block::parse(&output[4..]).unwrap();
        assert_eq!(block.len(), rows.len());
        let mut scratch = [0_u8; TARGET_BYTES];
        for (i, row) in rows.iter().enumerate().rev() {
            let vector = if i % 2 == 0 { &raw } else { &packed };
            let restored = block.restore(i, &mut scratch).unwrap();
            assert_eq!(&scratch[..restored.len], &row[4..]);
            let p = restored.vector_at.unwrap();
            assert_eq!(&output[4 + p..4 + p + vector.len()], vector);
        }
    }

    #[test]
    fn preserves_barriers_oversized_and_unprofitable_records() {
        let raw = raw_vector();
        let a = put("first", -4, "", "category", &raw);
        let b = put("first", 18, "", "category", &raw);
        let c = put("other", 1, "", "category", &raw);
        let d = put("other", 2, "", "category", &raw);
        let wide = put("first", 3, "", &"x".repeat(TARGET_BYTES + 7), &raw);
        let delete = [14, 0, 0, 0, 2, 1, 0, 0, 0, b'a', 1, 0, 0, 0, 0, 0, 0, 0];
        assert!(pack_payload(&wide, |_| Some(0)).unwrap().is_none());
        assert!(pack_payload(&a, |_| Some(0)).unwrap().is_none());
        assert!(
            pack_payload(&[a.clone(), b.clone()].concat(), |_| None)
                .unwrap()
                .is_none()
        );
        let input = [&a[..], &b[..], &delete, &wide, &c[..], &d[..]].concat();
        let result = pack_payload(&input, |_| Some(0)).unwrap().unwrap();
        let n = u32::from_le_bytes(result[..4].try_into().unwrap()) as usize;
        assert_eq!(Block::parse(&result[4..4 + n]).unwrap().len(), 2);
        let at = 4 + n;
        assert_eq!(&result[at..at + delete.len()], &delete);
        let at = at + delete.len();
        assert_eq!(&result[at..at + wide.len()], &wide);
        let at = at + wide.len();
        let n = u32::from_le_bytes(result[at..at + 4].try_into().unwrap()) as usize;
        assert_eq!(Block::parse(&result[at + 4..]).unwrap().len(), 2);
        assert_eq!(result.len(), at + 4 + n);
    }

    #[test]
    fn rejects_truncated_block_and_invalid_row_without_growing_the_output() {
        let raw = raw_vector();
        let input = [
            put("rows", i64::MIN, "same", "keep", &raw),
            put("rows", i64::MAX, "same", "keep", &raw),
        ]
        .concat();
        let output = pack_payload(&input, |_| Some(0)).unwrap().unwrap();
        let body = &output[4..];
        assert!(Block::parse(&body[..body.len() - 1]).is_err());
        let block = Block::parse(body).unwrap();
        assert!(block.restore(0, &mut [0_u8; 8]).is_err());
        assert!(block.restore(2, &mut [0_u8; 256]).is_err());

        let mut scratch = [0_u8; 256];
        let mut bad_row = body.to_vec();
        let p = block.restore(1, &mut scratch).unwrap().vector_at.unwrap();
        bad_row[p] = u8::MAX;
        assert!(
            Block::parse(&bad_row)
                .unwrap()
                .restore(1, &mut scratch)
                .is_err()
        );
    }
}
