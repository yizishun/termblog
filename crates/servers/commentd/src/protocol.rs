use serde::{Deserialize, Serialize};

pub const PUBLIC_QUERY: u8 = 1;
pub const PRIVATE_SUBMIT: u8 = 1;
pub const PRIVATE_SYNC: u8 = 2;
pub const PRIVATE_QUEUE: u8 = 3;
pub const PRIVATE_APPROVE: u8 = 4;
pub const PRIVATE_REJECT: u8 = 5;

pub const DEFAULT_LIMIT: u16 = 100;
pub const MAX_LIMIT: u16 = 100;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Comment {
    pub id: u64,
    pub target: String,
    pub author: String,
    pub text: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicQuery {
    pub target: String,
    #[serde(default)]
    pub after_id: Option<u64>,
    #[serde(default)]
    pub limit: Option<u16>,
    #[serde(default)]
    pub revision: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicQueryResponse {
    pub ok: bool,
    pub revision: String,
    pub total: usize,
    pub omitted_earlier: usize,
    pub comments: Vec<Comment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_after_id: Option<u64>,
    pub has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitRequest {
    pub target: String,
    pub line: String,
    pub ip: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub notice: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageRequest {
    #[serde(default)]
    pub after_id: u64,
    #[serde(default)]
    pub limit: Option<u16>,
    #[serde(default)]
    pub revision: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageResponse {
    pub ok: bool,
    pub revision: String,
    pub comments: Vec<Comment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_after_id: Option<u64>,
    pub has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModerateRequest {
    pub ids: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModerateResponse {
    pub ok: bool,
    pub changed: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub ok: bool,
    pub error: String,
}

pub fn page_limit(limit: Option<u16>) -> Result<usize, &'static str> {
    let n = limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&n) {
        return Err("limit 必须在 1..=100");
    }
    Ok(n as usize)
}

/// 合法 target: /、/blog/ 或 /blog/<slug>/，slug 与内容编译器完全同型。
pub fn valid_target(target: &str) -> bool {
    if target.len() > 200 {
        return false;
    }
    if target == "/" || target == "/blog/" {
        return true;
    }
    let Some(slug) = target
        .strip_prefix("/blog/")
        .and_then(|s| s.strip_suffix('/'))
    else {
        return false;
    };
    !slug.is_empty()
        && !slug.starts_with('/')
        && !slug.ends_with('/')
        && !slug.contains("//")
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'/' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_validation() {
        for good in ["/", "/blog/", "/blog/hello/", "/blog/a/b-2/"] {
            assert!(valid_target(good), "{good}");
        }
        for bad in [
            "",
            "blog/",
            "/blog/x",
            "/blog//x/",
            "/blog/../x/",
            "/BLOG/x/",
        ] {
            assert!(!valid_target(bad), "{bad}");
        }
    }
}
