use serde_json::json;
use worker::{Env, Method, Request, Response, Result};

use crate::http::{error, json_response};

#[derive(Debug, PartialEq, Eq)]
enum Route {
    Health,
    Readiness,
    NotFound,
}

fn route(method: Method, path: &str) -> Route {
    match (method, path) {
        (Method::Get, "/api/v1/health") => Route::Health,
        (Method::Get, "/api/v1/readiness") => Route::Readiness,
        _ => Route::NotFound,
    }
}

pub async fn handle(request: Request, env: Env) -> Result<Response> {
    match route(request.method(), &request.path()) {
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
        Route::NotFound => error(404, "NOT_FOUND", "Route not found"),
    }
}

async fn database_ready(env: &Env) -> bool {
    let Ok(database) = env.d1("DB") else {
        return false;
    };
    matches!(
        database
            .prepare("SELECT 1 AS ok")
            .first::<i32>(Some("ok"))
            .await,
        Ok(Some(1))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_routes_to_probes() {
        assert_eq!(route(Method::Get, "/api/v1/health"), Route::Health);
        assert_eq!(route(Method::Get, "/api/v1/readiness"), Route::Readiness);
    }

    #[test]
    fn unknown_routes_and_non_get_methods_are_not_found() {
        assert_eq!(route(Method::Get, "/api/v1/unknown"), Route::NotFound);
        assert_eq!(route(Method::Post, "/api/v1/health"), Route::NotFound);
    }
}
