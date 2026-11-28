//! Reusable D1 primitives for conditional atomic batches.
use wasm_bindgen::JsValue;
use worker::{D1Database, D1PreparedStatement, D1Result, Error, Result};

const GUARD_FAILURE: &str = "CHECK constraint failed: billing_write_guard_satisfied";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitFailure {
    GuardRejected,
    Database,
}

/// condition must be trusted SQL from the caller, with all dynamic values bound.
pub fn guard(db: &D1Database, condition: &str, values: &[JsValue]) -> Result<D1PreparedStatement> {
    db.prepare(format!(
        "INSERT INTO billing_write_guard (failure)
         SELECT 1 WHERE CASE WHEN ({condition}) THEN 0 ELSE 1 END = 1"
    ))
    .bind(values)
}

pub async fn commit(
    db: &D1Database,
    statements: Vec<D1PreparedStatement>,
) -> std::result::Result<Vec<D1Result>, CommitFailure> {
    let results = db.batch(statements).await.map_err(classify_error)?;
    if let Some(failed) = results.iter().find(|result| !result.success()) {
        return Err(classify_message(failed.error().as_deref().unwrap_or("")));
    }
    Ok(results)
}

fn classify_error(error: Error) -> CommitFailure {
    classify_message(&error.to_string())
}

fn classify_message(message: &str) -> CommitFailure {
    if message.contains(GUARD_FAILURE) {
        CommitFailure::GuardRejected
    } else {
        CommitFailure::Database
    }
}

/// D1 binds integers via JavaScript Numbers. Fail before losing integer precision.
pub fn integer(value: i64) -> Result<JsValue> {
    if !(-MAX_INTEGER..=MAX_INTEGER).contains(&value) {
        return Err("D1 integer exceeds safe Number range".into());
    }
    Ok(JsValue::from_f64(value as f64))
}

use super::http::ApiError;
use serde::de::DeserializeOwned;

pub const MAX_INTEGER: i64 = 9_007_199_254_740_991;
pub type ApiResult<T> = std::result::Result<T, ApiError>;

pub fn now() -> i64 {
    // Cloudflare supplies an epoch timestamp in milliseconds.
    worker::Date::now().as_millis() as i64
}

pub fn db_result<T>(result: Result<T>) -> ApiResult<T> {
    result.map_err(|_| ApiError::BILLING_UNAVAILABLE)
}

pub fn number(value: i64) -> ApiResult<JsValue> {
    db_result(integer(value))
}

pub fn prepare(db: &D1Database, sql: &str, values: &[JsValue]) -> ApiResult<D1PreparedStatement> {
    db_result(db.prepare(sql).bind(values))
}

pub fn checked_guard(
    db: &D1Database,
    sql: &str,
    values: &[JsValue],
) -> ApiResult<D1PreparedStatement> {
    db_result(guard(db, sql, values))
}

pub async fn batch_read<const N: usize>(
    db: &D1Database,
    statements: [D1PreparedStatement; N],
) -> ApiResult<[D1Result; N]> {
    let results = db_result(db.batch(statements.into()).await)?;
    if !results.iter().all(D1Result::success) {
        return Err(ApiError::BILLING_UNAVAILABLE);
    }
    results
        .try_into()
        .map_err(|_| ApiError::BILLING_UNAVAILABLE)
}

pub fn rows<T: DeserializeOwned>(result: &D1Result) -> ApiResult<Vec<T>> {
    db_result(result.results())
}

pub fn first_row<T: DeserializeOwned>(result: &D1Result) -> ApiResult<Option<T>> {
    Ok(rows(result)?.into_iter().next())
}
