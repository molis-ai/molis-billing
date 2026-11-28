use serde::{Deserialize, Serialize};
use worker::{D1Database, D1PreparedStatement};

use crate::core::{
    http::{ApiError, ApiResult, valid_id},
    identity::{Owner, OwnerKind},
    storage::{guard, integer, prepare},
};

use super::super::model::{CreditPeriod, CreditScope, Delta, SYSTEM};
use super::holds::Hold;

pub(super) const INSUFFICIENT: ApiError = ApiError::INSUFFICIENT_CREDITS;
pub(super) const INVALID: ApiError = ApiError::INVALID_REQUEST;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Usage {
    pub(super) calling_service: String,
    pub(super) actor_user_id: String,
    pub(super) product: String,
    pub(super) owner: Owner,
    pub(super) reference_id: String,
    pub(super) occurred_at: i64,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct HoldRequest {
    #[serde(flatten)]
    pub(super) usage: Usage,
    pub(super) amount: i64,
    pub(super) expires_at: i64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ConsumeRequest {
    #[serde(flatten)]
    pub(super) usage: Usage,
    pub(super) amount: i64,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Segment {
    pub(super) occurred_at: i64,
    pub(super) amount: i64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SettleRequest {
    #[serde(flatten)]
    pub(super) usage: Usage,
    pub(super) segments: Vec<Segment>,
}

impl Usage {
    pub(super) fn validate(&self, now: i64) -> ApiResult<()> {
        if self.product != "WORK"
            || !valid_id(&self.calling_service)
            || self.calling_service == SYSTEM
            || !valid_id(&self.actor_user_id)
            || !valid_id(&self.owner.id)
            || !valid_id(&self.reference_id)
            || !(0..=now).contains(&self.occurred_at)
        {
            return Err(INVALID);
        }
        if self.owner.kind == OwnerKind::User && self.owner.id != self.actor_user_id {
            return Err(ApiError::FORBIDDEN);
        }
        Ok(())
    }

    pub(super) fn scope(&self) -> CreditScope<'_> {
        CreditScope {
            owner: &self.owner,
            product: &self.product,
        }
    }
}

pub(super) enum Operation<'a> {
    Hold(&'a HoldRequest),
    Consume(&'a ConsumeRequest),
    Settle(&'a str, Segment),
    Release(&'a str),
}

impl Operation<'_> {
    pub(super) const fn kind(&self) -> &'static str {
        match self {
            Self::Hold(_) => "billing.credit.hold",
            Self::Consume(_) => "billing.credit.consume",
            Self::Settle(..) => "billing.credit.settle",
            Self::Release(_) => "billing.credit.release",
        }
    }
}

pub(super) struct Mutation {
    pub(super) statements: Vec<D1PreparedStatement>,
    pub(super) period_id: Option<String>,
    pub(super) hold: Option<Hold>,
    pub(super) ledger_hold: bool,
    pub(super) kind: &'static str,
    pub(super) delta: Delta,
    pub(super) occurred_at: i64,
    pub(super) amount: Option<i64>,
}

impl Mutation {
    pub(super) fn debit(period: &CreditPeriod, at: i64, amount: i64, delta: Delta) -> Self {
        Self {
            statements: Vec::new(),
            period_id: Some(period.id.clone()),
            hold: None,
            ledger_hold: false,
            kind: "CONSUME",
            delta,
            occurred_at: at,
            amount: Some(amount),
        }
    }
}

pub(in crate::credit) fn update_period(
    db: &D1Database,
    period: &CreditPeriod,
    delta: &Delta,
    now: i64,
) -> ApiResult<[D1PreparedStatement; 2]> {
    Ok([
        guard(db, "EXISTS (SELECT 1 FROM billing_credit_period WHERE id=? AND version=? AND status=?)",
              &[period.id.as_str().into(), integer(period.version)?, period.status.as_str().into()])?,
        prepare(db, "UPDATE billing_credit_period
                  SET available=available+?, reserved=reserved+?, consumed=consumed+?, expired=expired+?,
                      updated_at=?, version=version+1 WHERE id=? AND version=?",
             &[integer(delta.available)?, integer(delta.reserved)?, integer(delta.consumed)?, integer(delta.expired)?,
               integer(now)?, period.id.as_str().into(), integer(period.version)?])?,
    ])
}
