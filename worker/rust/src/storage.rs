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
    if !(-9_007_199_254_740_991..=9_007_199_254_740_991).contains(&value) {
        return Err("D1 integer exceeds safe Number range".into());
    }
    Ok(JsValue::from_f64(value as f64))
}
