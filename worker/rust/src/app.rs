use serde_json::{Value, json};
use worker::{D1Database, Env, Method, Request, Response, Result};

use crate::core::{
    http::{ApiError, ApiReply, ApiResult, json_response, read_json, request_id, valid_id},
    identity::{UserAccess, UserContext, authorize_user, require_internal_channel},
    logging, storage,
};
use crate::{
    audit, catalog, credit,
    orders::{Orders, webhook},
};

pub(crate) struct RequestContext<'a> {
    pub(crate) env: &'a Env,
    pub(crate) request_id: &'a str,
    pub(crate) now: i64,
    #[cfg(feature = "test-fixture")]
    pub(crate) test: crate::testing::Hooks,
}

impl RequestContext<'_> {
    pub(crate) fn db(&self) -> ApiResult<D1Database> {
        self.env.d1("DB").map_err(|_| ApiError::BILLING_UNAVAILABLE)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Route<'a> {
    Health,
    Readiness,
    Catalog,
    Subscription,
    ListOrders,
    GetOrder(&'a str),
    CreateOrder,
    #[cfg(feature = "mock-payment")]
    MockPay(&'a str),
    Refund(&'a str),
    CreditBalance,
    CreditLedger,
    Entitlement,
    PlatformAudit,
    CreateHold,
    SettleHold(&'a str),
    ReleaseHold(&'a str),
    Consume,
    Webhook(&'a str),
    NotFound,
}

impl Route<'_> {
    fn user_access(&self) -> Option<UserAccess> {
        match self {
            Self::Catalog
            | Self::Subscription
            | Self::CreditBalance
            | Self::CreditLedger
            | Self::Entitlement => Some(UserAccess::Read),
            Self::ListOrders | Self::GetOrder(_) | Self::CreateOrder | Self::Refund(_) => {
                Some(UserAccess::Manage)
            }
            #[cfg(feature = "mock-payment")]
            Self::MockPay(_) => Some(UserAccess::Manage),
            _ => None,
        }
    }

    fn paginated(&self) -> bool {
        matches!(self, Self::ListOrders | Self::CreditLedger)
    }

    fn needs_key(&self) -> bool {
        match self {
            Self::CreateOrder | Self::Refund(_) => true,
            #[cfg(feature = "mock-payment")]
            Self::MockPay(_) => true,
            _ => false,
        }
    }

    fn user_body(&self) -> UserBody {
        match self {
            Self::CreateOrder => UserBody::Json,
            Self::Entitlement | Self::Refund(_) => UserBody::EmptyJson,
            #[cfg(feature = "mock-payment")]
            Self::MockPay(_) => UserBody::EmptyJson,
            _ => UserBody::None,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Health => "/api/v1/health",
            Self::Readiness => "/api/v1/readiness",
            Self::Catalog => "/api/v1/catalog/plans",
            Self::Subscription => "/api/v1/subscriptions",
            Self::ListOrders | Self::CreateOrder => "/api/v1/orders",
            Self::GetOrder(_) => "/api/v1/orders/{id}",
            #[cfg(feature = "mock-payment")]
            Self::MockPay(_) => "/api/v1/orders/{id}/mock-pay",
            Self::Refund(_) => "/api/v1/orders/{id}/refund",
            Self::CreditBalance => "/api/v1/credits/balance",
            Self::CreditLedger => "/api/v1/credits/ledger",
            Self::Entitlement => "/api/v1/entitlements/check",
            Self::PlatformAudit => "/internal/v1/audit-events",
            Self::CreateHold => "/internal/v1/credits/holds",
            Self::SettleHold(_) => "/internal/v1/credits/holds/{id}/settle",
            Self::ReleaseHold(_) => "/internal/v1/credits/holds/{id}/release",
            Self::Consume => "/internal/v1/credits/consume",
            Self::Webhook(_) => "/api/v1/webhooks/{provider}",
            Self::NotFound => "not_found",
        }
    }
}

enum UserBody {
    None,
    EmptyJson,
    Json,
}

fn route(method: Method, path: &str) -> Route<'_> {
    let parts: Vec<_> = path.split('/').collect();
    match (method, parts.as_slice()) {
        (Method::Get, ["", "api", "v1", "health"]) => Route::Health,
        (Method::Get, ["", "api", "v1", "readiness"]) => Route::Readiness,
        (Method::Get, ["", "api", "v1", "catalog", "plans"]) => Route::Catalog,
        (Method::Get, ["", "api", "v1", "subscriptions"]) => Route::Subscription,
        (Method::Get, ["", "api", "v1", "orders"]) => Route::ListOrders,
        (Method::Get, ["", "api", "v1", "orders", id]) if valid_id(id) => Route::GetOrder(id),
        (Method::Post, ["", "api", "v1", "orders"]) => Route::CreateOrder,
        #[cfg(feature = "mock-payment")]
        (Method::Post, ["", "api", "v1", "orders", id, "mock-pay"]) if valid_id(id) => {
            Route::MockPay(id)
        }
        (Method::Post, ["", "api", "v1", "orders", id, "refund"]) if valid_id(id) => {
            Route::Refund(id)
        }
        (Method::Get, ["", "api", "v1", "credits", "balance"]) => Route::CreditBalance,
        (Method::Get, ["", "api", "v1", "credits", "ledger"]) => Route::CreditLedger,
        (Method::Post, ["", "api", "v1", "entitlements", "check"]) => Route::Entitlement,
        (Method::Get, ["", "internal", "v1", "audit-events"]) => Route::PlatformAudit,
        (Method::Post, ["", "internal", "v1", "credits", "holds"]) => Route::CreateHold,
        (Method::Post, ["", "internal", "v1", "credits", "holds", id, "settle"])
            if valid_id(id) =>
        {
            Route::SettleHold(id)
        }
        (Method::Post, ["", "internal", "v1", "credits", "holds", id, "release"])
            if valid_id(id) =>
        {
            Route::ReleaseHold(id)
        }
        (Method::Post, ["", "internal", "v1", "credits", "consume"]) => Route::Consume,
        (Method::Post, ["", "api", "v1", "webhooks", provider]) if valid_id(provider) => {
            Route::Webhook(provider)
        }
        _ => Route::NotFound,
    }
}

pub(crate) async fn handle(request: Request, env: Env) -> Result<Response> {
    let started = storage::now();
    let id = request_id(&request);
    let path = request.path();
    let method = request.inner().method();
    let route = route(request.method(), &path);
    let result = dispatch(&route, &path, request, &env, &id, started).await;
    let (status, error_code, body) = match result {
        Ok(reply) => (reply.status, None, reply.body),
        Err(error) => (error.status, Some(error.code), error.body(&id)),
    };
    if route != Route::Health || !(200..400).contains(&status) {
        let code = if (200..400).contains(&status) {
            "none"
        } else {
            error_code.unwrap_or("http_error")
        };
        logging::event(
            "billing.http.request",
            logging::environment(&env),
            &id,
            [
                ("method", json!(method)),
                ("route", json!(route.label())),
                ("status", json!(status)),
                ("error_code", json!(code)),
                ("duration_ms", json!((storage::now() - started).max(0))),
            ],
            status >= 500,
        );
    }
    json_response(status, &body, &id)
}

async fn dispatch(
    route: &Route<'_>,
    _path: &str,
    request: Request,
    env: &Env,
    id: &str,
    started: i64,
) -> ApiResult<ApiReply> {
    validate_method(&request)?;
    #[cfg(feature = "test-fixture")]
    let test = crate::testing::Hooks::from_request(&request, env)?;
    #[cfg(feature = "test-fixture")]
    let now = test.now.unwrap_or(started);
    #[cfg(not(feature = "test-fixture"))]
    let now = started;
    let context = RequestContext {
        env,
        request_id: id,
        now,
        #[cfg(feature = "test-fixture")]
        test,
    };
    #[cfg(feature = "test-fixture")]
    if let Some(reply) = crate::testing::dispatch(_path, &request, &context).await {
        return reply;
    }

    match route {
        Route::Health => Ok(ApiReply::ok(json!({
            "status": "UP",
            "service": "molis-billing",
            "environment": env.var("ENVIRONMENT_ID").map_err(|_| ApiError::BILLING_UNAVAILABLE)?.to_string(),
        }))),
        Route::Readiness => {
            let ready = match context.db() {
                Ok(db) => {
                    db.prepare(include_str!("core/readiness.sql"))
                        .first::<i32>(Some("ok"))
                        .await
                }
                Err(_) => return Err(ApiError::new(503, "NOT_READY", "Database unavailable")),
            };
            if !matches!(ready, Ok(Some(1))) {
                return Err(ApiError::new(503, "NOT_READY", "Database unavailable"));
            }
            Ok(ApiReply::ok(json!({"status": "READY", "database": "UP"})))
        }
        Route::Webhook(provider) => {
            let db = context.db()?;
            webhook(Orders::new(&db, &context), provider, request, &context).await
        }
        Route::PlatformAudit => audit::handle(&request, &context).await,
        Route::CreateHold | Route::SettleHold(_) | Route::ReleaseHold(_) | Route::Consume => {
            credit::handle_internal(route, request, &context).await
        }
        route => {
            let access =
                route
                    .user_access()
                    .ok_or(ApiError::new(404, "NOT_FOUND", "Route not found"))?;
            handle_user(route, access, request, &context).await
        }
    }
}

fn validate_method(request: &Request) -> ApiResult<()> {
    let actual = request.inner().method();
    if actual == request.method().as_ref() {
        Ok(())
    } else {
        Err(ApiError::new(
            405,
            "METHOD_NOT_ALLOWED",
            "Method not allowed",
        ))
    }
}

async fn handle_user(
    route: &Route<'_>,
    access: UserAccess,
    mut request: Request,
    context: &RequestContext<'_>,
) -> ApiResult<ApiReply> {
    require_internal_channel(&request, context.env)?;
    let subject = UserContext::from_request(&request, route.paginated())?;
    validate_user_request(route, &mut request).await?;
    let user = authorize_user(&request, context.env, subject, access, context.request_id).await?;
    let db = context.db()?;
    match route {
        Route::CreditBalance | Route::CreditLedger => {
            credit::handle_user(route, &request, &db, &user, context).await
        }
        Route::Catalog => {
            let plans = catalog::list_plans(&db, &user.product, user.owner.kind).await?;
            Ok(ApiReply::ok(json!({"items": plans})))
        }
        _ => {
            Orders::new(&db, context)
                .dispatch(route, request, context.env, &user, context.request_id)
                .await
        }
    }
}

async fn validate_user_request(route: &Route<'_>, request: &mut Request) -> ApiResult<()> {
    let key = request
        .headers()
        .get("Idempotency-Key")
        .map_err(|_| ApiError::INVALID_REQUEST)?;
    if (route.needs_key() && key.is_none()) || key.as_deref().is_some_and(|key| !valid_id(key)) {
        return Err(ApiError::INVALID_REQUEST);
    }
    match route.user_body() {
        UserBody::None if request.inner().body().is_some() => Err(ApiError::INVALID_REQUEST),
        UserBody::EmptyJson => {
            let body = read_json::<serde_json::Map<String, Value>>(request).await?;
            if body.is_some_and(|object| !object.is_empty()) {
                return Err(ApiError::INVALID_REQUEST);
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_keep_their_existing_routes() {
        assert_eq!(route(Method::Get, "/api/v1/health"), Route::Health);
        assert_eq!(route(Method::Get, "/api/v1/readiness"), Route::Readiness);
        assert_eq!(route(Method::Post, "/api/v1/health"), Route::NotFound);
    }

    #[test]
    fn shared_route_matching_rejects_wrong_method_and_extra_segments() {
        #[cfg(feature = "mock-payment")]
        assert_eq!(
            route(Method::Post, "/api/v1/orders/o-1/mock-pay"),
            Route::MockPay("o-1")
        );
        #[cfg(not(feature = "mock-payment"))]
        assert_eq!(
            route(Method::Post, "/api/v1/orders/o-1/mock-pay"),
            Route::NotFound
        );
        assert_eq!(
            route(Method::Get, "/api/v1/orders/o-1/mock-pay"),
            Route::NotFound
        );
        assert_eq!(
            route(Method::Post, "/internal/v1/credits/holds/h-1/settle"),
            Route::SettleHold("h-1")
        );
        assert_eq!(
            route(Method::Post, "/internal/v1/credits/holds/h-1/settle/"),
            Route::NotFound
        );
        assert_eq!(route(Method::Get, "/api/v1/unknown"), Route::NotFound);
    }
}
