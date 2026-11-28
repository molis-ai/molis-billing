use serde::Deserialize;
use serde_json::json;
use worker::{D1Database, D1PreparedStatement};

use crate::core::{
    calendar::{Anchor, Period, boundary_at},
    http::{ApiError, ApiResult, generated_id as new_id},
    idempotency::Audit,
    storage::{self, CommitFailure, guard, integer, prepare},
};

use super::{
    cleanup,
    model::{CreditPeriod, CreditScope, Delta, LedgerEntry, SYSTEM},
};

pub(crate) struct FirstMonthGrant<'a> {
    pub(crate) order_id: &'a str,
    pub(crate) anchor: &'a Anchor,
    pub(crate) period: &'a Period,
    pub(crate) monthly_credits: i64,
    pub(crate) paid_at: i64,
    pub(crate) request_id: &'a str,
}

#[derive(Deserialize)]
struct PaidPeriod {
    order_id: String,
    month_offset: u32,
    month_count: u32,
    starts_at: i64,
    ends_at: i64,
    anchor_at: i64,
    billing_time_zone: String,
    monthly_credits: i64,
    issued: u32,
}

struct Grant<'a> {
    order_id: &'a str,
    month_index: u32,
    last: bool,
    starts_at: i64,
    expires_at: i64,
    amount: i64,
}

/// Append after marking the order PAID and inserting its service period, before committing the
/// payment batch. The caller owns payment idempotency and concurrency guards for the whole batch.
/// A paid future renewal is picked up by synchronization at its start time.
pub(crate) fn prepare_paid_first_month(
    db: &D1Database,
    scope: &CreditScope<'_>,
    payment: FirstMonthGrant<'_>,
) -> ApiResult<Vec<D1PreparedStatement>> {
    if payment.period.starts_at > payment.paid_at {
        return Ok(Vec::new());
    }
    if payment.period.starts_at != payment.paid_at {
        return Err(ApiError::BILLING_UNAVAILABLE);
    }
    let expires_at = if payment.period.month_count == 1 {
        payment.period.ends_at
    } else {
        boundary_at(payment.anchor, next_month(payment.period.month_offset)?)?
    };
    let grant = Grant {
        order_id: payment.order_id,
        month_index: 0,
        last: payment.period.month_count == 1,
        starts_at: payment.paid_at,
        expires_at,
        amount: payment.monthly_credits,
    };
    grant.prepare(db, scope, payment.paid_at, payment.request_id)
}

pub(crate) async fn sync_credit_periods(
    db: &D1Database,
    scope: &CreditScope<'_>,
    now: i64,
    request_id: &str,
) -> ApiResult<()> {
    sync_until(db, scope, now, request_id, i64::MAX).await
}

pub(super) async fn sync_until(
    db: &D1Database,
    scope: &CreditScope<'_>,
    now: i64,
    request_id: &str,
    deadline: i64,
) -> ApiResult<()> {
    // Apply hold deadlines that fell before the month ended before archiving its buckets.
    cleanup::release_due_holds(db, scope, now, request_id, deadline).await?;
    if storage::now() >= deadline {
        return Ok(());
    }
    archive_due(db, scope, now, request_id, deadline).await?;
    if storage::now() >= deadline {
        return Ok(());
    }

    let mut values = scope.values().to_vec();
    values.push(integer(now)?);
    let paid: Vec<PaidPeriod> = prepare(
        db,
        "SELECT p.order_id, p.month_offset, p.month_count, p.starts_at, p.ends_at,
                s.anchor_at, s.billing_time_zone, o.monthly_credits,
                COALESCE((SELECT SUM(1 << c.month_index) FROM billing_credit_period c
                          WHERE c.order_id=p.order_id), 0) AS issued
         FROM billing_order_service_period p
         JOIN billing_order o ON o.id=p.order_id
         JOIN billing_subscription s ON s.id=p.subscription_id
         WHERE p.product=? AND p.owner_kind=? AND p.owner_id=?
           AND p.revoked_at IS NULL AND p.credits_complete=0 AND o.status='PAID' AND p.starts_at<=?
           AND NOT EXISTS (SELECT 1 FROM billing_credit_period c WHERE c.order_id=p.order_id AND c.starts_at<=?4 AND c.expires_at>?4)
         ORDER BY p.starts_at, p.order_id",
        &values,
    )?
    .all()
    .await
    .map_err(|_| ApiError::BILLING_UNAVAILABLE)?
    .results()
    .map_err(|_| ApiError::BILLING_UNAVAILABLE)?;
    let mut archive_late_grants = false;
    for row in paid {
        let anchor = Anchor {
            at: row.anchor_at,
            time_zone: row.billing_time_zone,
        };
        for index in 0..row.month_count {
            if storage::now() >= deadline {
                return Ok(());
            }
            if row.issued & (1 << index) != 0 {
                continue;
            }
            let offset = row
                .month_offset
                .checked_add(index)
                .ok_or(ApiError::BILLING_UNAVAILABLE)?;
            let starts_at = if index == 0 {
                row.starts_at
            } else {
                boundary_at(&anchor, offset)?
            };
            if starts_at > now {
                break;
            }
            let expires_at = if index + 1 == row.month_count {
                row.ends_at
            } else {
                boundary_at(&anchor, next_month(offset)?)?
            };
            Grant {
                order_id: &row.order_id,
                month_index: index,
                last: index + 1 == row.month_count,
                starts_at,
                expires_at,
                amount: row.monthly_credits,
            }
            .commit(db, scope, now, request_id)
            .await?;
            archive_late_grants |= expires_at <= now;
        }
    }
    if archive_late_grants {
        archive_due(db, scope, now, request_id, deadline).await?;
    }
    Ok(())
}

fn next_month(offset: u32) -> ApiResult<u32> {
    offset.checked_add(1).ok_or(ApiError::BILLING_UNAVAILABLE)
}

impl Grant<'_> {
    fn prepare(
        &self,
        db: &D1Database,
        scope: &CreditScope<'_>,
        now: i64,
        request_id: &str,
    ) -> ApiResult<Vec<D1PreparedStatement>> {
        let id = new_id();
        let audit_id = new_id();
        let detail = json!({"orderId": self.order_id, "monthIndex": self.month_index, "amount": self.amount});
        let audit = Audit {
            id: &audit_id,
            calling_service: SYSTEM,
            actor_user_id: None,
            operation: "billing.credit.grant",
            target_id: &id,
            product: scope.product,
            owner: scope.owner,
            request_id,
            details: &detail,
            created_at: now,
        };
        let mut paid = scope.values().to_vec();
        paid.extend([
            self.order_id.into(),
            self.month_index.into(),
            integer(self.starts_at)?,
            integer(self.expires_at)?,
            integer(self.amount)?,
            self.order_id.into(),
            self.month_index.into(),
        ]);
        let mut row = vec![
            id.as_str().into(),
            self.order_id.into(),
            self.month_index.into(),
        ];
        row.extend(scope.values());
        row.extend([
            integer(self.starts_at)?,
            integer(self.expires_at)?,
            integer(self.amount)?,
            integer(self.amount)?,
            integer(now)?,
            integer(now)?,
        ]);
        let mut batch = vec![
            guard(db,
                  "EXISTS (SELECT 1 FROM billing_order_service_period p JOIN billing_order o ON o.id=p.order_id
                   WHERE p.product=? AND p.owner_kind=? AND p.owner_id=? AND p.order_id=?
                     AND p.revoked_at IS NULL AND o.status='PAID' AND ?<p.month_count
                     AND p.starts_at<=? AND p.ends_at>=? AND o.monthly_credits=?)
                   AND NOT EXISTS (SELECT 1 FROM billing_credit_period WHERE order_id=? AND month_index=?)",
                  &paid)?,
            prepare(db,
                 "INSERT INTO billing_credit_period
                  (id, order_id, month_index, product, owner_kind, owner_id, starts_at, expires_at,
                   status, granted, available, reserved, consumed, expired, granted_at, updated_at)
                  VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'ACTIVE', ?, ?, 0, 0, 0, ?, ?)",
                 &row)?,
            audit.prepare(db)?,
            LedgerEntry {
                id: &new_id(), period_id: &id, hold_id: None, kind: "GRANT",
                delta: Delta { granted: self.amount, available: self.amount, ..Delta::default() },
                reference: Some(self.order_id), actor: None, occurred_at: self.starts_at,
            }.prepare(db, &audit)?,
        ];
        if self.last {
            batch.push(prepare(
                db,
                "UPDATE billing_order_service_period SET credits_complete=1 WHERE order_id=?",
                &[self.order_id.into()],
            )?);
        }
        Ok(batch)
    }

    async fn commit(
        &self,
        db: &D1Database,
        scope: &CreditScope<'_>,
        now: i64,
        request_id: &str,
    ) -> ApiResult<()> {
        match storage::commit(db, self.prepare(db, scope, now, request_id)?).await {
            Ok(_) => Ok(()),
            Err(CommitFailure::GuardRejected) => {
                let completed = prepare(
                    db,
                    "SELECT 1 AS ok FROM billing_order_service_period p
                     JOIN billing_order o ON o.id=p.order_id
                     WHERE p.order_id=? AND (p.revoked_at IS NOT NULL OR o.status!='PAID'
                       OR EXISTS (SELECT 1 FROM billing_credit_period c
                                  WHERE c.order_id=p.order_id AND c.month_index=?))",
                    &[self.order_id.into(), self.month_index.into()],
                )?
                .first::<i32>(Some("ok"))
                .await
                .map_err(|_| ApiError::BILLING_UNAVAILABLE)?;
                if completed == Some(1) {
                    Ok(())
                } else {
                    Err(ApiError::VERSION_CONFLICT)
                }
            }
            Err(error) => Err(error.into_api_error()),
        }
    }
}

pub(super) async fn period_at(
    db: &D1Database,
    scope: &CreditScope<'_>,
    at: i64,
) -> ApiResult<Option<CreditPeriod>> {
    read_period(db, scope, at)?
        .first(None)
        .await
        .map_err(|_| ApiError::BILLING_UNAVAILABLE)
}

pub(super) fn read_period(
    db: &D1Database,
    scope: &CreditScope<'_>,
    at: i64,
) -> ApiResult<D1PreparedStatement> {
    let mut values = scope.values().to_vec();
    values.extend([integer(at)?, integer(at)?]);
    prepare(db, include_str!("sql/period_at.sql"), &values)
}

async fn archive_due(
    db: &D1Database,
    scope: &CreditScope<'_>,
    now: i64,
    request_id: &str,
    deadline: i64,
) -> ApiResult<()> {
    let mut values = scope.values().to_vec();
    values.push(integer(now)?);
    let rows: Vec<CreditPeriod> = prepare(db, include_str!("sql/archive_due.sql"), &values)?
        .all()
        .await
        .map_err(|_| ApiError::BILLING_UNAVAILABLE)?
        .results()
        .map_err(|_| ApiError::BILLING_UNAVAILABLE)?;
    for mut row in rows {
        for attempt in 0..4 {
            if storage::now() >= deadline {
                return Ok(());
            }
            let batch = archive_batch(db, scope, &row, now, request_id)?;
            match storage::commit(db, batch).await {
                Ok(_) => break,
                Err(CommitFailure::GuardRejected) => {
                    cleanup::release_due_holds(db, scope, now, request_id, deadline).await?;
                    let updated: Option<CreditPeriod> = prepare(
                        db,
                        "SELECT id, starts_at, expires_at, granted, available, reserved, consumed, version, status
                         FROM billing_credit_period WHERE id=? AND status='ACTIVE'",
                        &[row.id.as_str().into()],
                    )?
                    .first(None)
                    .await
                    .map_err(|_| ApiError::BILLING_UNAVAILABLE)?;
                    let Some(updated) = updated else { break };
                    if attempt == 3 {
                        return Err(ApiError::VERSION_CONFLICT);
                    }
                    row = updated;
                }
                Err(error) => return Err(error.into_api_error()),
            }
        }
    }
    Ok(())
}

fn archive_batch(
    db: &D1Database,
    scope: &CreditScope<'_>,
    row: &CreditPeriod,
    now: i64,
    request_id: &str,
) -> ApiResult<Vec<D1PreparedStatement>> {
    let audit_id = new_id();
    let expired = row.available + row.reserved;
    let detail = json!({"amount": expired});
    let audit = Audit {
        id: &audit_id,
        calling_service: SYSTEM,
        actor_user_id: None,
        operation: "billing.credit.expire",
        target_id: &row.id,
        product: scope.product,
        owner: scope.owner,
        request_id,
        details: &detail,
        created_at: now,
    };
    let condition = "id=? AND version=? AND status='ACTIVE' AND expires_at<=?";
    let version = [row.id.as_str().into(), integer(row.version)?, integer(now)?];
    Ok(vec![
        guard(
            db,
            &format!(
                "EXISTS (
            SELECT 1 FROM billing_credit_period WHERE {condition}
              AND NOT EXISTS (SELECT 1 FROM billing_credit_hold h
                WHERE h.credit_period_id=billing_credit_period.id AND h.status='OPEN'
                  AND h.expires_at<billing_credit_period.expires_at)
        )"
            ),
            &version,
        )?,
        prepare(
            db,
            include_str!("sql/archive_holds.sql"),
            &[integer(now)?, row.id.as_str().into()],
        )?,
        prepare(
            db,
            &format!(
                "UPDATE billing_credit_period
              SET expired=expired+available+reserved, available=0, reserved=0,
                  status='EXPIRED', updated_at=?, version=version+1 WHERE {condition}"
            ),
            &[
                integer(now)?,
                version[0].clone(),
                version[1].clone(),
                version[2].clone(),
            ],
        )?,
        audit.prepare(db)?,
        LedgerEntry {
            id: &new_id(),
            period_id: &row.id,
            hold_id: None,
            kind: "EXPIRE",
            delta: Delta {
                available: -row.available,
                reserved: -row.reserved,
                expired,
                ..Delta::default()
            },
            reference: None,
            actor: None,
            occurred_at: row.expires_at,
        }
        .prepare(db, &audit)?,
    ])
}
