use worker::Request;

use crate::core::http::{ApiError, valid_id};

pub struct Page {
    pub limit: u32,
    pub cursor: Option<Cursor>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Cursor {
    pub created_at: i64,
    pub id: String,
}

impl Page {
    pub fn paginate_rows<T>(
        &self,
        rows: &mut Vec<T>,
        cursor: impl FnOnce(&T) -> (i64, &str),
    ) -> Option<String> {
        if rows.len() <= self.limit as usize {
            return None;
        }
        rows.truncate(self.limit as usize);
        rows.last().map(|row| {
            let (created_at, id) = cursor(row);
            Cursor {
                created_at,
                id: id.to_owned(),
            }
            .encode()
        })
    }

    pub fn from_request(request: &Request) -> Result<Self, ApiError> {
        let url = request.url().map_err(|_| ApiError::INVALID_REQUEST)?;
        let mut limit = None;
        let mut cursor = None;
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "limit" if limit.is_none() => {
                    limit = Some(
                        value
                            .parse::<u32>()
                            .ok()
                            .filter(|n| (1..=100).contains(n))
                            .ok_or(ApiError::INVALID_REQUEST)?,
                    );
                }
                "cursor" if cursor.is_none() => cursor = Some(Cursor::decode(&value)?),
                "limit" | "cursor" => return Err(ApiError::INVALID_REQUEST),
                _ => {}
            }
        }
        Ok(Self {
            limit: limit.unwrap_or(20),
            cursor,
        })
    }
}

impl Cursor {
    pub fn encode(&self) -> String {
        format!("{}:{}", self.created_at, self.id)
    }

    pub fn decode(value: &str) -> Result<Self, ApiError> {
        let (timestamp, id) = value.split_once(':').ok_or(ApiError::INVALID_REQUEST)?;
        let created_at = timestamp
            .parse::<i64>()
            .ok()
            .filter(|at| (0..=9_007_199_254_740_991).contains(at))
            .ok_or(ApiError::INVALID_REQUEST)?;
        if !valid_id(id) {
            return Err(ApiError::INVALID_REQUEST);
        }
        Ok(Self {
            created_at,
            id: id.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_roundtrips_and_rejects_invalid_keys() {
        let cursor = Cursor {
            created_at: 1_800_000_000_000,
            id: "credit_1".into(),
        };
        assert_eq!(Cursor::decode(&cursor.encode()), Ok(cursor));
        for raw in ["", "1:", "-1:ok", "1:a:b", "9007199254740992:ok"] {
            assert_eq!(Cursor::decode(raw), Err(ApiError::INVALID_REQUEST));
        }
    }
}
