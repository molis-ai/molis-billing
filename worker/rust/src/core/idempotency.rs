use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use wasm_bindgen::JsValue;
use worker::{D1Database, D1PreparedStatement, Request};

use crate::{
    core::http::{ApiError, ApiResult, valid_id},
    core::identity::Owner,
    core::storage::{self, CommitFailure, integer, prepare},
};

pub(crate) const RECEIPT_TTL_MS: i64 = 24 * 60 * 60 * 1_000;

const KEY_QUERY: &str = "calling_service = ? AND actor_user_id = ? AND operation = ?
    AND product = ? AND owner_kind = ? AND owner_id = ? AND idempotency_key = ?";

pub(crate) struct ReceiptKey<'a> {
    pub(crate) calling_service: &'a str,
    /// Use an empty string for a service actor with no user.
    pub(crate) actor_user_id: &'a str,
    pub(crate) operation: &'a str,
    pub(crate) product: &'a str,
    pub(crate) owner: &'a Owner,
    pub(crate) key: &'a str,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Receipt {
    pub(crate) request_hash: String,
    pub(crate) response_status: u16,
    pub(crate) response_json: String,
}

pub(crate) struct Success<'a> {
    pub(crate) request_hash: &'a str,
    pub(crate) response_status: u16,
    pub(crate) response: &'a Value,
    pub(crate) audit_id: &'a str,
    pub(crate) created_at: i64,
    pub(crate) expires_at: i64,
}

pub(crate) struct Audit<'a> {
    pub(crate) id: &'a str,
    pub(crate) calling_service: &'a str,
    pub(crate) actor_user_id: Option<&'a str>,
    pub(crate) operation: &'a str,
    pub(crate) target_id: &'a str,
    pub(crate) product: &'a str,
    pub(crate) owner: &'a Owner,
    pub(crate) request_id: &'a str,
    pub(crate) details: &'a Value,
    pub(crate) created_at: i64,
}

pub(crate) fn header(request: &Request) -> ApiResult<String> {
    request
        .headers()
        .get("Idempotency-Key")
        .ok()
        .flatten()
        .filter(|key| valid_id(key))
        .ok_or(ApiError::INVALID_REQUEST)
}

impl ReceiptKey<'_> {
    fn values(&self) -> [JsValue; 7] {
        [
            self.calling_service,
            self.actor_user_id,
            self.operation,
            self.product,
            self.owner.kind.as_str(),
            &self.owner.id,
            self.key,
        ]
        .map(JsValue::from_str)
    }

    pub(crate) fn request_hash(&self, target: &str, parameters: &Value) -> String {
        digest(&json!([
            self.calling_service,
            self.actor_user_id,
            self.operation,
            target,
            self.product,
            self.owner,
            parameters
        ]))
    }

    pub(crate) fn read(&self, db: &D1Database, now: i64) -> ApiResult<D1PreparedStatement> {
        let mut values = self.values().to_vec();
        values.push(integer(now)?);
        db.prepare(format!(
            "SELECT request_hash, response_status, response_json
             FROM billing_idempotency_receipt WHERE {KEY_QUERY} AND expires_at > ?"
        ))
        .bind(&values)
        .map_err(|_| ApiError::BILLING_UNAVAILABLE)
    }

    pub(crate) async fn lookup(
        &self,
        db: &D1Database,
        now: i64,
        expected_hash: &str,
    ) -> ApiResult<Option<Receipt>> {
        let receipt = self
            .read(db, now)?
            .first(None)
            .await
            .map_err(|_| ApiError::BILLING_UNAVAILABLE)?;
        Receipt::verify(receipt, expected_hash)
    }

    /// Append both statements to the business batch after the corresponding audit insert.
    /// A live receipt with the same key aborts the batch; the caller should then re-read it.
    pub(crate) async fn commit(
        &self,
        db: &D1Database,
        mut batch: Vec<D1PreparedStatement>,
        result: Success<'_>,
    ) -> std::result::Result<(), CommitFailure> {
        batch.extend(
            self.success(db, result)
                .map_err(|_| CommitFailure::Database)?,
        );
        storage::commit(db, batch).await.map(|_| ())
    }

    fn success(&self, db: &D1Database, result: Success<'_>) -> ApiResult<[D1PreparedStatement; 2]> {
        let mut values = self.values().to_vec();
        values.push(integer(result.created_at)?);
        let expire = prepare(
            db,
            &format!(
                "DELETE FROM billing_idempotency_receipt WHERE {KEY_QUERY} AND expires_at <= ?"
            ),
            &values,
        )?;
        let mut values = self.values().to_vec();
        values.extend([
            result.request_hash.into(),
            JsValue::from(result.response_status),
            JsValue::from_str(&result.response.to_string()),
            result.audit_id.into(),
            integer(result.created_at)?,
            integer(result.expires_at)?,
        ]);
        let insert = prepare(db,
            "INSERT INTO billing_idempotency_receipt
             (calling_service, actor_user_id, operation, product, owner_kind, owner_id,
              idempotency_key, request_hash, response_status, response_json, audit_id, created_at, expires_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        , &values)?;
        Ok([expire, insert])
    }
}

impl Audit<'_> {
    pub(crate) fn prepare(&self, db: &D1Database) -> ApiResult<D1PreparedStatement> {
        prepare(db,
            "INSERT INTO billing_audit
             (id, calling_service, actor_user_id, operation, target_id, product, owner_kind, owner_id,
              request_id, details_json, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        , &[
            self.id.into(), self.calling_service.into(),
            self.actor_user_id.map_or(JsValue::NULL, JsValue::from_str),
            self.operation.into(), self.target_id.into(), self.product.into(),
            self.owner.kind.as_str().into(), self.owner.id.as_str().into(), self.request_id.into(),
            JsValue::from_str(&self.details.to_string()), integer(self.created_at)?,
        ])
    }
}

pub(crate) fn digest(value: &Value) -> String {
    let mut canonical = value.clone();
    canonical.sort_all_objects();
    format!("{:x}", Sha256::digest(canonical.to_string().as_bytes()))
}

impl Receipt {
    pub(crate) fn verify(receipt: Option<Self>, expected_hash: &str) -> ApiResult<Option<Self>> {
        if receipt
            .as_ref()
            .is_some_and(|row| row.request_hash != expected_hash)
        {
            return Err(ApiError::IDEMPOTENCY_CONFLICT);
        }
        Ok(receipt)
    }
}

pub(crate) async fn purge_expired(db: &D1Database, now: i64, limit: u32) -> ApiResult<()> {
    prepare(
        db,
        "DELETE FROM billing_idempotency_receipt WHERE rowid IN (
            SELECT rowid FROM billing_idempotency_receipt WHERE expires_at<=?
            ORDER BY expires_at, rowid LIMIT ?
         )",
        &[integer(now)?, limit.into()],
    )?
    .run()
    .await
    .map_err(|_| ApiError::BILLING_UNAVAILABLE)?;
    Ok(())
}
