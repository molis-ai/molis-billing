use serde_json::json;
use worker::{Env, Method, Request, Response, Result};

use crate::{
    audit, catalog, credit,
    http::{ApiError, error, json_response, request_id, valid_id},
    identity::{UserAccess, UserContext, authorize_user, require_internal_channel},
    orders::{Orders, append_first_credit, webhook},
};

#[derive(Debug, PartialEq, Eq)]
pub enum Route<'a> {
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
    pub fn user_access(&self) -> Option<UserAccess> {
        use Route::*;
        match self {
            Catalog | Subscription | CreditBalance | CreditLedger | Entitlement => {
                Some(UserAccess::Read)
            }
            ListOrders | GetOrder(_) | CreateOrder | Refund(_) => Some(UserAccess::Manage),
            #[cfg(feature = "mock-payment")]
            MockPay(_) => Some(UserAccess::Manage),
            _ => None,
        }
    }

    pub fn is_internal_credit(&self) -> bool {
        matches!(
            self,
            Self::CreateHold | Self::SettleHold(_) | Self::ReleaseHold(_) | Self::Consume
        )
    }
}

pub fn route(method: Method, path: &str) -> Route<'_> {
    use Route::*;
    let parts: Vec<_> = path.split('/').collect();
    match (method, parts.as_slice()) {
        (Method::Get, ["", "api", "v1", "health"]) => Health,
        (Method::Get, ["", "api", "v1", "readiness"]) => Readiness,
        (Method::Get, ["", "api", "v1", "catalog", "plans"]) => Catalog,
        (Method::Get, ["", "api", "v1", "subscriptions"]) => Subscription,
        (Method::Get, ["", "api", "v1", "orders"]) => ListOrders,
        (Method::Get, ["", "api", "v1", "orders", id]) if valid_id(id) => GetOrder(id),
        (Method::Post, ["", "api", "v1", "orders"]) => CreateOrder,
        #[cfg(feature = "mock-payment")]
        (Method::Post, ["", "api", "v1", "orders", id, "mock-pay"]) if valid_id(id) => MockPay(id),
        (Method::Post, ["", "api", "v1", "orders", id, "refund"]) if valid_id(id) => Refund(id),
        (Method::Get, ["", "api", "v1", "credits", "balance"]) => CreditBalance,
        (Method::Get, ["", "api", "v1", "credits", "ledger"]) => CreditLedger,
        (Method::Post, ["", "api", "v1", "entitlements", "check"]) => Entitlement,
        (Method::Get, ["", "internal", "v1", "audit-events"]) => PlatformAudit,
        (Method::Post, ["", "internal", "v1", "credits", "holds"]) => CreateHold,
        (Method::Post, ["", "internal", "v1", "credits", "holds", id, "settle"])
            if valid_id(id) =>
        {
            SettleHold(id)
        }
        (Method::Post, ["", "internal", "v1", "credits", "holds", id, "release"])
            if valid_id(id) =>
        {
            ReleaseHold(id)
        }
        (Method::Post, ["", "internal", "v1", "credits", "consume"]) => Consume,
        (Method::Post, ["", "api", "v1", "webhooks", provider]) if valid_id(provider) => {
            Webhook(provider)
        }
        _ => NotFound,
    }
}

pub async fn handle(request: Request, env: Env) -> Result<Response> {
    let path = request.path();
    let route = route(request.method(), &path);
    match route {
        Route::Health => json_response(
            200,
            &json!({
                "data": {
                    "status": "UP",
                    "service": "molis-billing",
                    "environment": env.var("ENVIRONMENT_ID")?.to_string(),
                }
            }),
        ),
        Route::Readiness => {
            if database_ready(&env).await {
                json_response(
                    200,
                    &json!({ "data": { "status": "READY", "database": "UP" } }),
                )
            } else {
                error(503, "NOT_READY", "Database unavailable")
            }
        }
        #[cfg(feature = "order-fixture")]
        Route::Webhook(provider) if crate::orders::fixture::enabled(&request, &env) => {
            crate::orders::fixture::webhook(provider, request, &env).await
        }
        Route::Webhook(provider) => {
            let Ok(db) = env.d1("DB") else {
                return ApiError::BILLING_UNAVAILABLE.response();
            };
            webhook(
                Orders::new(&db, append_first_credit),
                provider,
                request,
                &env,
            )
            .await
        }
        Route::PlatformAudit => audit::handle(&request, &env).await,
        route if route.is_internal_credit() => credit::handle_internal(&route, request, &env).await,
        route => {
            let Some(access) = route.user_access() else {
                return error(404, "NOT_FOUND", "Route not found");
            };
            handle_user(route, access, request, &env).await
        }
    }
}

async fn handle_user(
    route: Route<'_>,
    access: UserAccess,
    request: Request,
    env: &Env,
) -> Result<Response> {
    if let Err(failure) = require_internal_channel(&request, env).await {
        return failure.response();
    }
    let request_id = request_id(&request);
    let subject = match UserContext::from_request(&request) {
        Ok(subject) => subject,
        Err(failure) => return failure.response(),
    };
    let user = match authorize_user(&request, env, subject, access, &request_id).await {
        Ok(user) => user,
        Err(failure) => return failure.response(),
    };
    if matches!(route, Route::CreditBalance | Route::CreditLedger) {
        return credit::handle_user(&route, &request, env, &user, &request_id).await;
    }

    let Ok(db) = env.d1("DB") else {
        return ApiError::BILLING_UNAVAILABLE.response();
    };
    if route == Route::Catalog {
        return match catalog::list_plans(&db, &user.product, user.owner.kind).await {
            Ok(plans) => json_response(200, &json!({ "data": { "items": plans } })),
            Err(failure) => failure.response(),
        };
    }
    #[cfg(feature = "order-fixture")]
    if crate::orders::fixture::enabled(&request, env) {
        return crate::orders::fixture::user(route, request, env, &user, &request_id).await;
    }
    Orders::new(&db, append_first_credit)
        .dispatch(&route, request, env, &user, &request_id)
        .await
        .unwrap_or_else(|| ApiError::NOT_FOUND.response())
}

async fn database_ready(env: &Env) -> bool {
    let Ok(database) = env.d1("DB") else {
        return false;
    };
    matches!(
        database
            .prepare(include_str!("schema-readiness.sql"))
            .first::<i32>(Some("ok"))
            .await,
        Ok(Some(1))
    )
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
