//! MiniVectorDB: a single-client, durable SQL and exact vector database.
mod disk_index;
mod distance;
mod engine;
mod protocol;
mod query;
mod read_batch;
mod sql;
mod storage;
mod types;

pub use protocol::run;

#[derive(Debug)]
struct Error {
    code: &'static str,
    message: String,
}

impl Error {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::new("io_error", e.to_string())
    }
}
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::new("invalid_value", e.to_string())
    }
}
type Result<T> = std::result::Result<T, Error>;
