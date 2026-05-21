//! Helpers for converting between domain enums and SQLite TEXT columns.
//!
//! We piggy-back on the `#[serde(rename_all = "snake_case")]` attributes on
//! domain enums so the wire format and the column format are guaranteed to
//! stay in lockstep.

use ports::{PortError, PortResult};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub fn enum_to_str<T: Serialize>(t: &T) -> String {
    serde_json::to_value(t)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

pub fn enum_from_str<T: DeserializeOwned>(field: &'static str, value: &str) -> PortResult<T> {
    serde_json::from_value(serde_json::Value::String(value.to_string())).map_err(|e| {
        PortError::Backend(format!("decode {field}={value:?}: {e}"))
    })
}

pub fn json_to_string<T: Serialize>(t: &T) -> PortResult<String> {
    serde_json::to_string(t).map_err(|e| PortError::Backend(format!("serialize json: {e}")))
}

pub fn json_from_string<T: DeserializeOwned>(field: &'static str, raw: &str) -> PortResult<T> {
    serde_json::from_str(raw).map_err(|e| PortError::Backend(format!("decode {field}: {e}")))
}

pub fn parse_uuid<T: std::str::FromStr>(field: &'static str, value: &str) -> PortResult<T>
where
    <T as std::str::FromStr>::Err: std::fmt::Display,
{
    value
        .parse::<T>()
        .map_err(|e| PortError::Backend(format!("parse {field}={value:?}: {e}")))
}

pub fn map_sqlx_err(e: sqlx::Error) -> PortError {
    match &e {
        sqlx::Error::RowNotFound => PortError::NotFound("row not found".into()),
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            PortError::Conflict(db.message().to_string())
        }
        _ => PortError::Backend(e.to_string()),
    }
}
