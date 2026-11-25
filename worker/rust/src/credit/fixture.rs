//! Local WASM/D1 acceptance adapter. Excluded from normal and deployment builds.
use serde::Deserialize;
use serde_json::json;
use worker::{Env, Request, Response};

use super::{
    CreditScope, FirstMonthGrant, Result, UNAVAILABLE, bind, cleanup_due, handle_internal_at,
    handle_user_at, number, prepare_paid_first_month, render,
};
use crate::{
    app::{self, Route},
    calendar::{Interval, purchase_period},
    http::{ApiError, request_id},
    identity::{Owner, UserAccess, UserContext, authorize_user, require_internal_channel},
    storage,
};

pub(crate) fn enabled(env: &Env) -> bool {
    env.var("ENVIRONMENT_ID")
        .ok()
        .is_some_and(|value| value.to_string() == "billing-isolated-test")
        && env
            .var("BILLING_CREDIT_FIXTURE")
            .ok()
            .is_some_and(|value| value.to_string() == "1")
}

pub(crate) async fn handle(request: Request, env: Env) -> worker::Result<Response> {
    let path = request.path();
    let route = app::route(request.method(), &path);
    let clock = || clock(&request);
    let response = match route {
        Route::CreditBalance | Route::CreditLedger => read(&route, &request, &env, clock()).await,
        route if route.is_internal_credit() => {
            let at = clock();
            handle_internal_at(&route, request, &env, at).await
        }
        _ if path == "/fixture/credits/cleanup" => cleanup(&request, &env).await,
        _ if path == "/fixture/credits/pay" => pay(request, &env).await,
        _ => return app::handle(request, env).await,
    };
    render(response)
}

fn clock(request: &Request) -> Result<i64> {
    request
        .headers()
        .get("X-Fixture-Now")
        .ok()
        .flatten()
        .and_then(|value| value.parse().ok())
        .filter(|at| (0..=8_640_000_000_000_000).contains(at))
        .ok_or(ApiError::INVALID_REQUEST)
}

async fn read(
    route: &Route<'_>,
    request: &Request,
    env: &Env,
    clock: Result<i64>,
) -> Result<(u16, serde_json::Value)> {
    require_internal_channel(request, env).await?;
    let subject = UserContext::from_request(request)?;
    let request_id = request_id(request);
    let user = authorize_user(request, env, subject, UserAccess::Read, &request_id).await?;
    handle_user_at(route, request, env, &user, &request_id, clock).await
}

async fn cleanup(request: &Request, env: &Env) -> Result<(u16, serde_json::Value)> {
    require_internal_channel(request, env).await?;
    let db = env.d1("DB").map_err(|_| UNAVAILABLE)?;
    cleanup_due(&db, clock(request)?, &request_id(request)).await?;
    Ok((200, json!({"data": {}})))
}

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

async fn pay(mut request: Request, env: &Env) -> Result<(u16, serde_json::Value)> {
    require_internal_channel(&request, env).await?;
    let now = clock(&request)?;
    let request_id = request_id(&request);
    let payment: Payment = request
        .json()
        .await
        .map_err(|_| ApiError::INVALID_REQUEST)?;
    let db = env.d1("DB").map_err(|_| UNAVAILABLE)?;
    let scope = CreditScope {
        owner: &payment.owner,
        product: "WORK",
    };
    let (anchor, period) = purchase_period(now, "Etc/UTC", payment.interval, None)?;
    let id = &payment.order_id;
    let subscription_id = format!("subscription-{id}");
    let mut batch = vec![
        storage::guard(&db, "EXISTS(SELECT 1 FROM billing_order WHERE id=? AND status='PENDING_PAYMENT' AND payment_expires_at>?)",
            &[id.as_str().into(), number(now)]).map_err(|_| UNAVAILABLE)?,
        bind(&db, "UPDATE billing_order SET status='PAID', paid_at=?, payment_provider='fixture', provider_payment_id=? WHERE id=?",
            &[number(now), id.as_str().into(), id.as_str().into()])?,
        bind(&db, "INSERT INTO billing_subscription (id,product,owner_kind,owner_id,plan_id,billing_time_zone,anchor_at)
                   VALUES (?,'WORK',?,?,?,?,?)",
            &[subscription_id.as_str().into(), payment.owner.kind.as_str().into(), payment.owner.id.as_str().into(),
                payment.plan_id.as_str().into(), anchor.time_zone.as_str().into(), number(anchor.at)])?,
        bind(&db, "INSERT INTO billing_order_service_period
                   (order_id,subscription_id,product,owner_kind,owner_id,plan_id,month_offset,month_count,starts_at,ends_at)
                   VALUES (?,?,'WORK',?,?,?,0,?,?,?)",
            &[id.as_str().into(), subscription_id.as_str().into(), payment.owner.kind.as_str().into(), payment.owner.id.as_str().into(),
                payment.plan_id.as_str().into(), payment.interval.months().into(), number(period.starts_at), number(period.ends_at)])?,
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
            request_id: &request_id,
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
    Ok((
        200,
        json!({"data": { "orderId": id, "endsAt": period.ends_at }}),
    ))
}
