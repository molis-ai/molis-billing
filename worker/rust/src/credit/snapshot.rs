use super::periods::{CreditPeriod, period_at, read_period};
use super::*;
use crate::core::storage;
pub(super) fn probes(
    db: &D1Database,
    scope: &CreditScope<'_>,
    at: i64,
) -> Result<[D1PreparedStatement; 3]> {
    let mut values = scope.values().to_vec();
    values.push(number(at)?);
    Ok([
        bind(db,"SELECT h.id FROM billing_credit_hold h JOIN billing_credit_period p ON p.id=h.credit_period_id
          WHERE h.product=? AND h.owner_kind=? AND h.owner_id=? AND h.status='OPEN' AND h.expires_at<=? AND h.expires_at<p.expires_at LIMIT 1",&values)?,
        bind(db,"SELECT id FROM billing_credit_period WHERE product=? AND owner_kind=? AND owner_id=? AND status='ACTIVE' AND expires_at<=? LIMIT 1",&values)?,
        bind(db,"SELECT p.order_id FROM billing_order_service_period p
          WHERE p.product=? AND p.owner_kind=? AND p.owner_id=? AND p.revoked_at IS NULL AND p.credits_complete=0 AND p.starts_at<=?
          AND NOT EXISTS (SELECT 1 FROM billing_credit_period c WHERE c.order_id=p.order_id AND c.starts_at<=?4 AND c.expires_at>?4) LIMIT 1",&values)?,
    ])
}

pub(super) fn due(results: [&worker::D1Result; 3]) -> Result<bool> {
    for result in results {
        if storage::first_row::<serde_json::Value>(result)?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) async fn balance(
    db: &D1Database,
    scope: &CreditScope<'_>,
    now: i64,
    id: &str,
) -> Result<Option<CreditPeriod>> {
    let [holds, archive, grant] = probes(db, scope, now)?;
    let [holds, archive, grant, period] =
        storage::batch_read(db, [holds, archive, grant, read_period(db, scope, now)?]).await?;
    if due([&holds, &archive, &grant])? {
        sync_credit_periods(db, scope, now, id).await?;
        period_at(db, scope, now).await
    } else {
        storage::first_row(&period)
    }
}
