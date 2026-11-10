use std::io::{self, BufRead, Write};
use std::path::Path;

use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;

use crate::engine::{Engine, Outcome};
use crate::{Error, Result};

#[derive(Deserialize)]
struct Request<'a> {
    #[serde(borrow)]
    id: Option<&'a RawValue>,
    #[serde(borrow)]
    op: Option<&'a RawValue>,
    #[serde(borrow)]
    sql: Option<&'a RawValue>,
    #[serde(borrow)]
    search_mode: Option<&'a RawValue>,
}

fn request_text(field: Option<&RawValue>) -> Option<String> {
    serde_json::from_str(field?.get()).ok()
}

fn respond(out: &mut impl Write, id: Option<&RawValue>, result: Result<Outcome>) -> io::Result<()> {
    out.write_all(b"{\"id\":")?;
    out.write_all(id.map_or("null", RawValue::get).as_bytes())?;
    match result {
        Ok(Outcome::Ok) => out.write_all(b",\"ok\":true")?,
        Ok(Outcome::Affected(n)) => {
            out.write_all(b",\"ok\":true,\"affected_rows\":")?;
            serde_json::to_writer(&mut *out, &n)?;
        }
        Ok(Outcome::Query(query)) => {
            out.write_all(b",\"ok\":true,\"columns\":")?;
            serde_json::to_writer(&mut *out, &query.columns)?;
            out.write_all(b",\"rows\":")?;
            query.write_rows(out)?;
        }
        Err(error) => {
            out.write_all(b",\"ok\":false,\"error\":")?;
            serde_json::to_writer(
                &mut *out,
                &json!({"code":error.code,"message":error.message}),
            )?;
        }
    }
    out.write_all(b"}\n")?;
    out.flush()
}

/// Runs the official JSONL protocol, announcing ready only after recovery.
///
/// # Errors
/// Returns an error on I/O failure or an indeterminate storage failure.
pub fn run(data: &Path) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let mut engine = Engine::open(data)?;
    let mut out = io::BufWriter::new(io::stdout().lock());
    out.write_all(b"{\"type\":\"ready\"}\n")?;
    out.flush()?;
    let mut input = String::new();
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    loop {
        input.clear();
        if stdin.read_line(&mut input)? == 0 {
            engine.rollback();
            break;
        }
        let request: Request<'_> = match serde_json::from_str(&input) {
            Ok(v) => v,
            Err(e) => {
                respond(
                    &mut out,
                    None,
                    Err(Error::new("invalid_request", e.to_string())),
                )?;
                continue;
            }
        };
        let op = request_text(request.op);
        if op.as_deref() == Some("shutdown") {
            engine.rollback();
            respond(&mut out, request.id, Ok(Outcome::Ok))?;
            break;
        }
        let valid_mode = request.search_mode.is_none()
            || matches!(
                request_text(request.search_mode).as_deref(),
                Some("exact" | "ann")
            );
        let result = if op.as_deref() == Some("execute")
            && valid_mode
            && let Some(sql) = request_text(request.sql)
        {
            engine.execute(&sql)
        } else {
            Err(Error::new(
                "invalid_request",
                "execute requires SQL and exact/ann mode",
            ))
        };
        match result {
            // A failed storage operation may have crossed the durability boundary.
            Err(error) if matches!(error.code, "io_error" | "storage_error") => {
                return Err(Box::new(error));
            }
            result => respond(&mut out, request.id, result)?,
        }
    }
    Ok(())
}
