use serde::Deserialize;
use wasm_bindgen::JsValue;
use worker::{D1Database, D1PreparedStatement};

use crate::core::{
    http::ApiResult,
    idempotency::Audit,
    identity::Owner,
    storage::{integer, prepare},
};

pub(super) const SYSTEM: &str = "billing";

pub(crate) struct CreditScope<'a> {
    pub(crate) owner: &'a Owner,
    pub(crate) product: &'a str,
}

impl CreditScope<'_> {
    pub(super) fn values(&self) -> [JsValue; 3] {
        [self.product, self.owner.kind.as_str(), &self.owner.id].map(JsValue::from_str)
    }
}

pub(super) fn optional(value: Option<&str>) -> JsValue {
    value.map_or(JsValue::NULL, JsValue::from_str)
}

#[derive(Default)]
pub(super) struct Delta {
    pub(super) granted: i64,
    pub(super) available: i64,
    pub(super) reserved: i64,
    pub(super) consumed: i64,
    pub(super) expired: i64,
}

pub(super) struct LedgerEntry<'a> {
    pub(super) id: &'a str,
    pub(super) period_id: &'a str,
    pub(super) hold_id: Option<&'a str>,
    pub(super) kind: &'static str,
    pub(super) delta: Delta,
    pub(super) reference: Option<&'a str>,
    pub(super) actor: Option<&'a str>,
    pub(super) occurred_at: i64,
}

impl LedgerEntry<'_> {
    pub(super) fn prepare(
        &self,
        db: &D1Database,
        audit: &Audit<'_>,
    ) -> ApiResult<D1PreparedStatement> {
        prepare(
            db,
            "INSERT INTO billing_credit_ledger
             (id, credit_period_id, hold_id, product, owner_kind, owner_id, kind,
              delta_granted, delta_available, delta_reserved, delta_consumed, delta_expired,
              reference_id, actor_user_id, calling_service, request_id, audit_id, occurred_at, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                self.id.into(), self.period_id.into(), optional(self.hold_id),
                audit.product.into(), audit.owner.kind.as_str().into(), audit.owner.id.as_str().into(),
                self.kind.into(), integer(self.delta.granted)?, integer(self.delta.available)?,
                integer(self.delta.reserved)?, integer(self.delta.consumed)?, integer(self.delta.expired)?,
                optional(self.reference), optional(self.actor), audit.calling_service.into(),
                audit.request_id.into(), audit.id.into(), integer(self.occurred_at)?,
                integer(audit.created_at)?,
            ],
        )
    }
}

#[derive(Deserialize)]
pub(super) struct CreditPeriod {
    pub(crate) id: String,
    pub(crate) starts_at: i64,
    pub(crate) expires_at: i64,
    pub(crate) granted: i64,
    pub(crate) available: i64,
    pub(crate) reserved: i64,
    pub(crate) consumed: i64,
    pub(crate) version: i64,
    pub(crate) status: String,
}
