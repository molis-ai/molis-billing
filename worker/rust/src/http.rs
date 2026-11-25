use serde::Serialize;
use serde_json::json;
use worker::{Response, Result};

pub fn json_response(status: u16, body: &impl Serialize) -> Result<Response> {
    let mut response = Response::from_json(body)?.with_status(status);
    let headers = response.headers_mut();
    headers.set("Content-Type", "application/json; charset=utf-8")?;
    headers.set("Cache-Control", "no-store")?;
    headers.set("X-Content-Type-Options", "nosniff")?;
    headers.set("X-Frame-Options", "DENY")?;
    headers.set("Referrer-Policy", "no-referrer")?;
    Ok(response)
}

pub fn error(status: u16, code: &str, message: &str) -> Result<Response> {
    json_response(
        status,
        &json!({ "error": { "code": code, "message": message } }),
    )
}
