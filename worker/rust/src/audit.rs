//! Platform audit reads from Billing's append-only audit table.
use serde::{Deserialize, Deserializer, Serialize, de::Error};
use serde_json::{Value, json};
use worker::{D1Database, Env, Request, Response, Result};

use crate::{
    http::{ApiError, json_response, request_id},
    identity::{OwnerKind, authorize_platform_audit, require_internal_channel},
    pagination::{Cursor, Page},
    storage,
};

const UNAVAILABLE: ApiError = ApiError::BILLING_UNAVAILABLE;

#[derive(Deserialize, Serialize)]
#[serde(rename_all(serialize = "camelCase"))]
struct AuditItem {
    id: String,
    calling_service: String,
    actor_user_id: Option<String>,
    operation: String,
    target_id: String,
    product: String,
    owner_kind: OwnerKind,
    owner_id: String,
    request_id: String,
    #[serde(
        rename(deserialize = "details_json"),
        deserialize_with = "deserialize_details"
    )]
    details: Value,
    created_at: i64,
}

fn deserialize_details<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Value, D::Error> {
    let details = String::deserialize(deserializer)?;
    serde_json::from_str(&details).map_err(D::Error::custom)
}

pub(crate) async fn handle(request: &Request, env: &Env) -> Result<Response> {
    match read(request, env).await {
        Ok(data) => json_response(200, &json!({ "data": data })),
        Err(failure) => failure.response(),
    }
}

async fn read(request: &Request, env: &Env) -> std::result::Result<Value, ApiError> {
    require_internal_channel(request, env).await?;
    authorize_platform_audit(request, env, &request_id(request)).await?;
    let page = Page::from_request(request)?;
    let db = env.d1("DB").map_err(|_| UNAVAILABLE)?;
    list(&db, page).await
}

async fn list(db: &D1Database, page: Page) -> std::result::Result<Value, ApiError> {
    let mut sql = String::from(
        "SELECT id, calling_service, actor_user_id, operation, target_id, product, owner_kind,
                owner_id, request_id, details_json, created_at FROM billing_audit",
    );
    let mut values = Vec::new();
    if let Some(cursor) = page.cursor {
        sql.push_str(" WHERE (created_at, id) < (?, ?)");
        values.push(storage::integer(cursor.created_at).map_err(|_| UNAVAILABLE)?);
        values.push(cursor.id.into());
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
    values.push((page.limit + 1).into());

    let mut items: Vec<AuditItem> = db
        .prepare(sql)
        .bind(&values)
        .map_err(|_| UNAVAILABLE)?
        .all()
        .await
        .map_err(|_| UNAVAILABLE)?
        .results()
        .map_err(|_| UNAVAILABLE)?;
    let next_cursor = if items.len() > page.limit as usize {
        items.truncate(page.limit as usize);
        items.last().map(|item| {
            Cursor {
                created_at: item.created_at,
                id: item.id.clone(),
            }
            .encode()
        })
    } else {
        None
    };
    Ok(json!({ "items": items, "nextCursor": next_cursor }))
}
