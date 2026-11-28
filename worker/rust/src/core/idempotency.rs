use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use wasm_bindgen::JsValue;
use worker::{D1Database, D1PreparedStatement, Request, Result};

use crate::{
    core::http::{ApiError, valid_id},
    core::identity::Owner,
    core::storage::integer,
};

const KEY_QUERY: &str = "calling_service = ? AND actor_user_id = ? AND operation = ?
    AND product = ? AND owner_kind = ? AND owner_id = ? AND idempotency_key = ?";

pub struct ReceiptKey<'a> {
    pub calling_service: &'a str,
    /// Use an empty string for a service actor with no user.
    pub actor_user_id: &'a str,
    pub operation: &'a str,
    pub product: &'a str,
    pub owner: &'a Owner,
    pub key: &'a str,
}

#[derive(Debug, Deserialize)]
pub struct Receipt {
    pub request_hash: String,
    pub response_status: u16,
    pub response_json: String,
}

pub struct Success<'a> {
    pub request_hash: &'a str,
    pub response_status: u16,
    pub response: &'a Value,
    pub audit_id: &'a str,
    pub created_at: i64,
    pub expires_at: i64,
}

pub struct Audit<'a> {
    pub id: &'a str,
    pub calling_service: &'a str,
    pub actor_user_id: Option<&'a str>,
    pub operation: &'a str,
    pub target_id: &'a str,
    pub product: &'a str,
    pub owner: &'a Owner,
    pub request_id: &'a str,
    pub details: &'a Value,
    pub created_at: i64,
}

pub fn header(request: &Request) -> std::result::Result<String, ApiError> {
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

    pub fn request_hash(&self, target: &str, parameters: &Value) -> String {
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

    pub fn read(
        &self,
        db: &D1Database,
        now: i64,
    ) -> std::result::Result<D1PreparedStatement, ApiError> {
        let mut values = self.values().to_vec();
        values.push(integer(now).map_err(|_| ApiError::BILLING_UNAVAILABLE)?);
        db.prepare(format!(
            "SELECT request_hash, response_status, response_json
             FROM billing_idempotency_receipt WHERE {KEY_QUERY} AND expires_at > ?"
        ))
        .bind(&values)
        .map_err(|_| ApiError::BILLING_UNAVAILABLE)
    }

    pub async fn lookup(
        &self,
        db: &D1Database,
        now: i64,
        expected_hash: &str,
    ) -> std::result::Result<Option<Receipt>, ApiError> {
        let receipt = self
            .read(db, now)?
            .first(None)
            .await
            .map_err(|_| ApiError::BILLING_UNAVAILABLE)?;
        Receipt::verify(receipt, expected_hash)
    }

    /// Append both statements to the business batch after the corresponding audit insert.
    /// A live receipt with the same key aborts the batch; the caller should then re-read it.
    pub async fn commit(
        &self,
        db: &D1Database,
        mut batch: Vec<D1PreparedStatement>,
        result: Success<'_>,
    ) -> std::result::Result<(), super::storage::CommitFailure> {
        batch.extend(
            self.success(db, result)
                .map_err(|_| super::storage::CommitFailure::Database)?,
        );
        super::storage::commit(db, batch).await.map(|_| ())
    }

    fn success(&self, db: &D1Database, result: Success<'_>) -> Result<[D1PreparedStatement; 2]> {
        let mut values = self.values().to_vec();
        values.push(integer(result.created_at)?);
        let expire = db
            .prepare(format!(
                "DELETE FROM billing_idempotency_receipt WHERE {KEY_QUERY} AND expires_at <= ?"
            ))
            .bind(&values)?;
        let mut values = self.values().to_vec();
        values.extend([
            result.request_hash.into(),
            JsValue::from(result.response_status),
            JsValue::from_str(&result.response.to_string()),
            result.audit_id.into(),
            integer(result.created_at)?,
            integer(result.expires_at)?,
        ]);
        let insert = db.prepare(
            "INSERT INTO billing_idempotency_receipt
             (calling_service, actor_user_id, operation, product, owner_kind, owner_id,
              idempotency_key, request_hash, response_status, response_json, audit_id, created_at, expires_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        ).bind(&values)?;
        Ok([expire, insert])
    }
}

impl Audit<'_> {
    pub fn prepare(&self, db: &D1Database) -> Result<D1PreparedStatement> {
        db.prepare(
            "INSERT INTO billing_audit
             (id, calling_service, actor_user_id, operation, target_id, product, owner_kind, owner_id,
              request_id, details_json, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        ).bind(&[
            self.id.into(), self.calling_service.into(),
            self.actor_user_id.map_or(JsValue::NULL, JsValue::from_str),
            self.operation.into(), self.target_id.into(), self.product.into(),
            self.owner.kind.as_str().into(), self.owner.id.as_str().into(), self.request_id.into(),
            JsValue::from_str(&self.details.to_string()), integer(self.created_at)?,
        ])
    }
}

pub fn digest(value: &Value) -> String {
    let mut canonical = value.clone();
    canonical.sort_all_objects();
    format!("{:x}", Sha256::digest(canonical.to_string().as_bytes()))
}

impl Receipt {
    pub fn verify(
        receipt: Option<Self>,
        expected_hash: &str,
    ) -> std::result::Result<Option<Self>, ApiError> {
        if receipt
            .as_ref()
            .is_some_and(|row| row.request_hash != expected_hash)
        {
            return Err(ApiError::IDEMPOTENCY_CONFLICT);
        }
        Ok(receipt)
    }
}

pub async fn purge_expired(
    db: &D1Database,
    now: i64,
    limit: u32,
) -> std::result::Result<(), ApiError> {
    super::storage::prepare(
        db,
        "DELETE FROM billing_idempotency_receipt WHERE rowid IN (
            SELECT rowid FROM billing_idempotency_receipt WHERE expires_at<=?
            ORDER BY expires_at, rowid LIMIT ?
         )",
        &[super::storage::number(now)?, limit.into()],
    )?
    .run()
    .await
    .map_err(|_| ApiError::BILLING_UNAVAILABLE)?;
    Ok(())
}
