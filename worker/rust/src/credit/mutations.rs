#[path = "holds.rs"]
mod holds;
#[path = "settlement.rs"]
mod settlement;
#[path = "writes.rs"]
mod writes;
pub(super) use holds::find_hold;
use holds::{Hold, HoldRow, HoldStatus, read_hold};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use worker::{D1Database, D1PreparedStatement, Request};

use super::{
    CreditScope, Delta, LedgerEntry, Result, UNAVAILABLE, bind, commit_failure, guard, new_id,
    number,
    periods::{self, CreditPeriod},
    sync_credit_periods,
};
use crate::{
    app::Route,
    core::http::{ApiError, read_json, valid_id},
    core::idempotency::{self, Audit, ReceiptKey, Success},
    core::identity::{Owner, OwnerKind},
    core::storage::{self, CommitFailure},
};

const INSUFFICIENT: ApiError = ApiError::INSUFFICIENT_CREDITS;
const INVALID: ApiError = ApiError::INVALID_REQUEST;
use crate::core::storage::MAX_INTEGER;
const RECEIPT_TTL_MS: i64 = 24 * 60 * 60 * 1_000;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Usage {
    calling_service: String,
    actor_user_id: String,
    product: String,
    owner: Owner,
    reference_id: String,
    occurred_at: i64,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HoldRequest {
    #[serde(flatten)]
    usage: Usage,
    amount: i64,
    expires_at: i64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ConsumeRequest {
    #[serde(flatten)]
    usage: Usage,
    amount: i64,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Segment {
    occurred_at: i64,
    amount: i64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SettleRequest {
    #[serde(flatten)]
    usage: Usage,
    segments: Vec<Segment>,
}

impl Usage {
    fn validate(&self, now: i64) -> Result<()> {
        if self.product != "WORK"
            || !valid_id(&self.calling_service)
            || self.calling_service == super::SYSTEM
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

    fn scope(&self) -> CreditScope<'_> {
        CreditScope {
            owner: &self.owner,
            product: &self.product,
        }
    }
}

enum Operation<'a> {
    Hold(&'a HoldRequest),
    Consume(&'a ConsumeRequest),
    Settle(&'a str, Segment),
    Release(&'a str),
}

impl Operation<'_> {
    const fn kind(&self) -> &'static str {
        match self {
            Self::Hold(_) => "billing.credit.hold",
            Self::Consume(_) => "billing.credit.consume",
            Self::Settle(..) => "billing.credit.settle",
            Self::Release(_) => "billing.credit.release",
        }
    }
}

#[derive(Deserialize)]
struct AuditDetails {
    details_json: String,
}

struct Write<'a> {
    db: &'a D1Database,
    usage: &'a Usage,
    request_id: &'a str,
    idempotency_key: &'a str,
    now: i64,
}

struct Mutation {
    statements: Vec<D1PreparedStatement>,
    period_id: Option<String>,
    hold: Option<Hold>,
    ledger_hold: bool,
    kind: &'static str,
    delta: Delta,
    occurred_at: i64,
    amount: Option<i64>,
}

impl Mutation {
    fn debit(period: &CreditPeriod, at: i64, amount: i64, delta: Delta) -> Self {
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

pub(super) async fn handle(
    route: &Route<'_>,
    mut request: Request,
    db: &D1Database,
    now: i64,
    request_id: &str,
) -> Result<(u16, Value)> {
    let idempotency_key = idempotency::header(&request)?;

    let body: Value = read_json(&mut request).await?.ok_or(INVALID)?;
    let make = |usage| Write {
        db,
        usage,
        idempotency_key: &idempotency_key,
        request_id,
        now,
    };
    match route {
        Route::CreateHold => {
            let input: HoldRequest = parse(&body)?;
            input.usage.validate(now)?;
            check_amount(input.amount)?;
            if !(input.usage.occurred_at + 1..=MAX_INTEGER).contains(&input.expires_at) {
                return Err(INVALID);
            }
            make(&input.usage)
                .run(Operation::Hold(&input), &body, None)
                .await
        }
        Route::Consume => {
            let input: ConsumeRequest = parse(&body)?;
            input.usage.validate(now)?;
            check_amount(input.amount)?;
            make(&input.usage)
                .run(Operation::Consume(&input), &body, None)
                .await
        }
        Route::SettleHold(id) => {
            let input: SettleRequest = parse(&body)?;
            input.usage.validate(now)?;
            make(&input.usage).settle(id, &input.segments, &body).await
        }
        Route::ReleaseHold(id) => {
            let usage: Usage = parse(&body)?;
            usage.validate(now)?;
            make(&usage).run(Operation::Release(id), &body, None).await
        }
        _ => Err(ApiError::NOT_FOUND),
    }
}

fn parse<T: serde::de::DeserializeOwned>(body: &Value) -> Result<T> {
    serde_json::from_value(body.clone()).map_err(|_| INVALID)
}

fn check_amount(amount: i64) -> Result<()> {
    if (1..=MAX_INTEGER).contains(&amount) {
        Ok(())
    } else {
        Err(INVALID)
    }
}

impl Write<'_> {
    async fn settle(&self, id: &str, segments: &[Segment], body: &Value) -> Result<(u16, Value)> {
        if segments.len() > 16
            || segments.is_empty()
            || segments
                .windows(2)
                .any(|rows| rows[0].occurred_at > rows[1].occurred_at)
        {
            return Err(INVALID);
        }
        for segment in segments {
            check_amount(segment.amount)?;
            if !(0..=self.now).contains(&segment.occurred_at) {
                return Err(INVALID);
            }
        }
        let mut settled = Vec::with_capacity(segments.len());
        let mut hold = Value::Null;
        for (index, segment) in segments.iter().enumerate() {
            match self
                .run(
                    Operation::Settle(id, *segment),
                    body,
                    Some((index, index + 1 == segments.len())),
                )
                .await
            {
                Ok((_, response)) => {
                    settled.push(response["data"]["settledSegments"][0].clone());
                    hold = response["data"]["hold"].clone();
                }
                Err(error) if error == INSUFFICIENT => {
                    return Err(error
                        .with_data(json!({"settledSegments": settled, "failedSegment": segment})));
                }
                Err(error) => return Err(error),
            }
        }
        Ok((
            200,
            json!({ "data": { "hold": hold, "settledSegments": settled } }),
        ))
    }
}

pub(super) fn update_period(
    db: &D1Database,
    period: &CreditPeriod,
    delta: &Delta,
    now: i64,
) -> Result<[D1PreparedStatement; 2]> {
    Ok([
        guard(db, "EXISTS (SELECT 1 FROM billing_credit_period WHERE id=? AND version=? AND status=?)",
              &[period.id.as_str().into(), number(period.version)?, period.status.as_str().into()])?,
        bind(db, "UPDATE billing_credit_period
                  SET available=available+?, reserved=reserved+?, consumed=consumed+?, expired=expired+?,
                      updated_at=?, version=version+1 WHERE id=? AND version=?",
             &[number(delta.available)?, number(delta.reserved)?, number(delta.consumed)?, number(delta.expired)?,
               number(now)?, period.id.as_str().into(), number(period.version)?])?,
    ])
}
