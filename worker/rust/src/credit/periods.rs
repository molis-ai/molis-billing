use serde::Deserialize;
use serde_json::json;
use worker::{D1Database, D1PreparedStatement};

use super::{
    CreditScope, Delta, LedgerEntry, Result, SYSTEM, UNAVAILABLE, bind, cleanup, commit_failure,
    guard, new_id, number,
};
use crate::{
    core::calendar::{Anchor, Period, boundary_at},
    core::http::ApiError,
    core::idempotency::Audit,
    core::storage::{self, CommitFailure},
};

#[derive(Deserialize)]
pub(super) struct CreditPeriod {
    pub id: String,
    pub starts_at: i64,
    pub expires_at: i64,
    pub granted: i64,
    pub available: i64,
    pub reserved: i64,
    pub consumed: i64,
    pub version: i64,
    pub status: String,
}

pub struct FirstMonthGrant<'a> {
    pub order_id: &'a str,
    pub anchor: &'a Anchor,
    pub period: &'a Period,
    pub monthly_credits: i64,
    pub paid_at: i64,
    pub request_id: &'a str,
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
pub fn prepare_paid_first_month(
    db: &D1Database,
    scope: &CreditScope<'_>,
    payment: FirstMonthGrant<'_>,
) -> Result<Vec<D1PreparedStatement>> {
    if payment.period.starts_at > payment.paid_at {
        return Ok(Vec::new());
    }
    if payment.period.starts_at != payment.paid_at {
        return Err(UNAVAILABLE);
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

pub async fn sync_credit_periods(
    db: &D1Database,
    scope: &CreditScope<'_>,
    now: i64,
    request_id: &str,
) -> Result<()> {
    sync_until(db, scope, now, request_id, i64::MAX).await
}

pub(super) async fn sync_until(
    db: &D1Database,
    scope: &CreditScope<'_>,
    now: i64,
    request_id: &str,
    deadline: i64,
) -> Result<()> {
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
    values.push(number(now)?);
    let paid: Vec<PaidPeriod> = bind(
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
    .map_err(|_| UNAVAILABLE)?
    .results()
    .map_err(|_| UNAVAILABLE)?;
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
            let offset = row.month_offset.checked_add(index).ok_or(UNAVAILABLE)?;
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

fn next_month(offset: u32) -> Result<u32> {
    offset.checked_add(1).ok_or(UNAVAILABLE)
}

impl Grant<'_> {
    fn prepare(
        &self,
        db: &D1Database,
        scope: &CreditScope<'_>,
        now: i64,
        request_id: &str,
    ) -> Result<Vec<D1PreparedStatement>> {
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
            number(self.starts_at)?,
            number(self.expires_at)?,
            number(self.amount)?,
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
            number(self.starts_at)?,
            number(self.expires_at)?,
            number(self.amount)?,
            number(self.amount)?,
            number(now)?,
            number(now)?,
        ]);
        let mut batch = vec![
            guard(db,
                  "EXISTS (SELECT 1 FROM billing_order_service_period p JOIN billing_order o ON o.id=p.order_id
                   WHERE p.product=? AND p.owner_kind=? AND p.owner_id=? AND p.order_id=?
                     AND p.revoked_at IS NULL AND o.status='PAID' AND ?<p.month_count
                     AND p.starts_at<=? AND p.ends_at>=? AND o.monthly_credits=?)
                   AND NOT EXISTS (SELECT 1 FROM billing_credit_period WHERE order_id=? AND month_index=?)",
                  &paid)?,
            bind(db,
                 "INSERT INTO billing_credit_period
                  (id, order_id, month_index, product, owner_kind, owner_id, starts_at, expires_at,
                   status, granted, available, reserved, consumed, expired, granted_at, updated_at)
                  VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'ACTIVE', ?, ?, 0, 0, 0, ?, ?)",
                 &row)?,
            audit.prepare(db).map_err(|_| UNAVAILABLE)?,
            LedgerEntry {
                id: &new_id(), period_id: &id, hold_id: None, kind: "GRANT",
                delta: Delta { granted: self.amount, available: self.amount, ..Delta::default() },
                reference: Some(self.order_id), actor: None, occurred_at: self.starts_at,
            }.prepare(db, &audit)?,
        ];
        if self.last {
            batch.push(bind(
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
    ) -> Result<()> {
        match storage::commit(db, self.prepare(db, scope, now, request_id)?).await {
            Ok(_) => Ok(()),
            Err(CommitFailure::GuardRejected) => {
                let completed = bind(
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
                .map_err(|_| UNAVAILABLE)?;
                if completed == Some(1) {
                    Ok(())
                } else {
                    Err(ApiError::VERSION_CONFLICT)
                }
            }
            Err(error) => Err(commit_failure(error)),
        }
    }
}

pub(super) async fn period_at(
    db: &D1Database,
    scope: &CreditScope<'_>,
    at: i64,
) -> Result<Option<CreditPeriod>> {
    read_period(db, scope, at)?
        .first(None)
        .await
        .map_err(|_| UNAVAILABLE)
}

pub(super) fn read_period(
    db: &D1Database,
    scope: &CreditScope<'_>,
    at: i64,
) -> Result<D1PreparedStatement> {
    let mut values = scope.values().to_vec();
    values.extend([number(at)?, number(at)?]);
    bind(db, include_str!("sql/period_at.sql"), &values)
}

async fn archive_due(
    db: &D1Database,
    scope: &CreditScope<'_>,
    now: i64,
    request_id: &str,
    deadline: i64,
) -> Result<()> {
    let mut values = scope.values().to_vec();
    values.push(number(now)?);
    let rows: Vec<CreditPeriod> = bind(db, include_str!("sql/archive_due.sql"), &values)?
        .all()
        .await
        .map_err(|_| UNAVAILABLE)?
        .results()
        .map_err(|_| UNAVAILABLE)?;
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
                    let updated: Option<CreditPeriod> = bind(
                        db,
                        "SELECT id, starts_at, expires_at, granted, available, reserved, consumed, version, status
                         FROM billing_credit_period WHERE id=? AND status='ACTIVE'",
                        &[row.id.as_str().into()],
                    )?
                    .first(None)
                    .await
                    .map_err(|_| UNAVAILABLE)?;
                    let Some(updated) = updated else { break };
                    if attempt == 3 {
                        return Err(ApiError::VERSION_CONFLICT);
                    }
                    row = updated;
                }
                Err(error) => return Err(commit_failure(error)),
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
) -> Result<Vec<D1PreparedStatement>> {
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
    let version = [row.id.as_str().into(), number(row.version)?, number(now)?];
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
        bind(
            db,
            include_str!("sql/archive_holds.sql"),
            &[number(now)?, row.id.as_str().into()],
        )?,
        bind(
            db,
            &format!(
                "UPDATE billing_credit_period
              SET expired=expired+available+reserved, available=0, reserved=0,
                  status='EXPIRED', updated_at=?, version=version+1 WHERE {condition}"
            ),
            &[
                number(now)?,
                version[0].clone(),
                version[1].clone(),
                version[2].clone(),
            ],
        )?,
        audit.prepare(db).map_err(|_| UNAVAILABLE)?,
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
