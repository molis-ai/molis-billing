//! Platform audit reads from Billing's append-only audit table.
use serde::{Deserialize, Deserializer, Serialize, de::Error};
use serde_json::{Value, json};
use worker::{D1Database, Request};

use crate::{
    app::RequestContext,
    core::http::{ApiReply, ApiResult},
    core::identity::{OwnerKind, authorize_platform_audit, require_internal_channel},
    core::pagination::Page,
    core::storage,
};

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

pub(crate) async fn handle(request: &Request, context: &RequestContext<'_>) -> ApiResult<ApiReply> {
    require_internal_channel(request, context.env)?;
    authorize_platform_audit(request, context.env, context.request_id).await?;
    let page = Page::from_request(request)?;
    Ok(ApiReply::ok(list(&context.db()?, page).await?))
}

async fn list(db: &D1Database, page: Page) -> ApiResult<Value> {
    let mut sql = String::from(
        "SELECT id, calling_service, actor_user_id, operation, target_id, product, owner_kind,
                owner_id, request_id, details_json, created_at FROM billing_audit",
    );
    let mut values = Vec::new();
    if let Some(cursor) = &page.cursor {
        sql.push_str(" WHERE (created_at, id) < (?, ?)");
        values.push(storage::integer(cursor.created_at)?);
        values.push(cursor.id.as_str().into());
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
    values.push((page.limit + 1).into());

    let result = storage::db_result(storage::prepare(db, &sql, &values)?.all().await)?;
    let mut items: Vec<AuditItem> = storage::rows(&result)?;
    let next_cursor = page.paginate_rows(&mut items, |item| (item.created_at, &item.id));
    Ok(json!({ "items": items, "nextCursor": next_cursor }))
}
