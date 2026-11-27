//! Local integration fixture. The deployed build never selects this feature.
use super::*;

pub fn enabled(request: &Request, env: &Env) -> bool {
    env.var("ENVIRONMENT_ID")
        .is_ok_and(|value| value.to_string() == "billing-isolated-test")
        && request
            .headers()
            .get("X-Order-Fixture")
            .ok()
            .flatten()
            .as_deref()
            == Some("1")
}

pub async fn user(
    route: Route<'_>,
    request: Request,
    env: &Env,
    user: &VerifiedUser,
    request_id: &str,
) -> worker::Result<Response> {
    let Ok(db) = env.d1("DB") else {
        return ApiError::BILLING_UNAVAILABLE.response();
    };
    let orders = service(&db, &request);
    orders
        .dispatch(&route, request, env, user, request_id)
        .await
        .unwrap_or_else(|| ApiError::NOT_FOUND.response())
}

pub async fn webhook(provider: &str, request: Request, env: &Env) -> worker::Result<Response> {
    let Ok(db) = env.d1("DB") else {
        return ApiError::BILLING_UNAVAILABLE.response();
    };
    if provider == "order-fixture-expire" {
        let orders = service(&db, &request);
        let result = orders
            .expire_pending(&request_id(&request), 50)
            .await
            .map(|count| Reply::ok(json!({ "expired": count })));
        return respond(result);
    }
    super::webhook(service(&db, &request), provider, request, env).await
}

fn service<'a>(db: &'a D1Database, request: &Request) -> Orders<'a> {
    let mut orders = Orders::new(db, append_credit);
    orders.fixed_time = request
        .headers()
        .get("X-Order-Fixture-Time")
        .ok()
        .flatten()
        .and_then(|value| value.parse().ok());
    orders.fixed_commit_time = request
        .headers()
        .get("X-Order-Fixture-Commit-Time")
        .ok()
        .flatten()
        .and_then(|value| value.parse().ok());
    orders
}

fn append_credit(
    db: &D1Database,
    first: &FirstCredit<'_>,
    batch: &mut Vec<D1PreparedStatement>,
) -> ApiResult<()> {
    append_first_credit(db, first, batch)?;
    if first.request_id == "fixture-fail-credit" {
        // Exercise D1 rollback by writing to the append-only ledger after the grant.
        batch.push(prepare(
            db,
            "UPDATE billing_credit_ledger SET created_at=created_at
             WHERE credit_period_id IN (SELECT id FROM billing_credit_period WHERE order_id=?)",
            &[first.order_id.into()],
        )?);
    }
    Ok(())
}
