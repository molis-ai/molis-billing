use super::*;
use crate::core::calendar::{Interval, purchase_period};
use crate::core::storage;
use serde::Deserialize;
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Payment {
    order_id: String,
    owner: Owner,
    plan_id: String,
    monthly_credits: i64,
    interval: Interval,
    reject_after_grant: bool,
}

pub(crate) async fn pay(mut request: Request, ctx: &RequestContext<'_>) -> Result<ApiReply> {
    require_internal_channel(&request, ctx.env)?;
    let now = ctx.now;
    let request_id = ctx.request_id;
    let payment: Payment = request
        .json()
        .await
        .map_err(|_| ApiError::INVALID_REQUEST)?;
    let db = ctx.db()?;
    let scope = CreditScope {
        owner: &payment.owner,
        product: "WORK",
    };
    let (anchor, period) = purchase_period(now, "Etc/UTC", payment.interval, None)?;
    let id = &payment.order_id;
    let subscription_id = format!("subscription-{id}");
    let mut batch = vec![
        storage::guard(&db, "EXISTS(SELECT 1 FROM billing_order WHERE id=? AND status='PENDING_PAYMENT' AND payment_expires_at>?)",
            &[id.as_str().into(), number(now)?]).map_err(|_| UNAVAILABLE)?,
        bind(&db, "UPDATE billing_order SET status='PAID', paid_at=?, payment_provider='fixture', provider_payment_id=? WHERE id=?",
            &[number(now)?, id.as_str().into(), id.as_str().into()])?,
        bind(&db, "INSERT INTO billing_subscription (id,product,owner_kind,owner_id,plan_id,billing_time_zone,anchor_at)
                   VALUES (?,'WORK',?,?,?,?,?)",
            &[subscription_id.as_str().into(), payment.owner.kind.as_str().into(), payment.owner.id.as_str().into(),
                payment.plan_id.as_str().into(), anchor.time_zone.as_str().into(), number(anchor.at)?])?,
        bind(&db, "INSERT INTO billing_order_service_period
                   (order_id,subscription_id,product,owner_kind,owner_id,plan_id,month_offset,month_count,starts_at,ends_at)
                   VALUES (?,?,'WORK',?,?,?,0,?,?,?)",
            &[id.as_str().into(), subscription_id.as_str().into(), payment.owner.kind.as_str().into(), payment.owner.id.as_str().into(),
                payment.plan_id.as_str().into(), payment.interval.months().into(), number(period.starts_at)?, number(period.ends_at)?])?,
    ];
    batch.extend(prepare_paid_first_month(
        &db,
        &scope,
        FirstMonthGrant {
            order_id: id,
            anchor: &anchor,
            period: &period,
            monthly_credits: payment.monthly_credits,
            paid_at: now,
            request_id,
        },
    )?);
    if payment.reject_after_grant {
        batch.push(storage::guard(&db, "0", &[]).map_err(|_| UNAVAILABLE)?);
    }
    let result = storage::commit(&db, batch).await;
    if let Err(error) = result {
        let paid = db
            .prepare("SELECT status FROM billing_order WHERE id=?")
            .bind(&[id.as_str().into()])
            .map_err(|_| UNAVAILABLE)?
            .first::<String>(Some("status"))
            .await
            .map_err(|_| UNAVAILABLE)?;
        if paid.as_deref() != Some("PAID") {
            return Err(super::commit_failure(error));
        }
    }
    Ok(ApiReply::ok(
        json!({"orderId": id, "endsAt": period.ends_at}),
    ))
}
