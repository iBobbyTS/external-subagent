use std::fmt;

#[derive(Debug)]
pub enum StoreError {
    Sqlite(rusqlite::Error),
    LegacySchemaUnsupported,
    InvalidState(String),
    Conflict(String),
}

impl StoreError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::LegacySchemaUnsupported => "STORE_SCHEMA_VERSION_UNSUPPORTED",
            Self::Sqlite(_) => "PERSISTENCE_ERROR",
            Self::InvalidState(_) => "INVALID_STATE",
            Self::Conflict(_) => "CONFLICT",
        }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "SQLite store error: {error}"),
            Self::LegacySchemaUnsupported => formatter.write_str(
                "STORE_SCHEMA_VERSION_UNSUPPORTED: existing Store must be backed up and recreated",
            ),
            Self::InvalidState(message) => write!(formatter, "invalid store state: {message}"),
            Self::Conflict(message) => write!(formatter, "store conflict: {message}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sqlite(value)
    }
}

pub type StoreResult<T> = Result<T, StoreError>;
