use std::collections::HashMap;

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VisibleReply {
    pub number: u64,
    pub author: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VisibleComment {
    pub number: u64,
    pub target: String,
    pub author: String,
    pub text: String,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<VisibleReply>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicQuery {
    pub target: String,
    #[serde(default)]
    pub after_number: Option<u64>,
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
    pub comments: Vec<VisibleComment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_after_number: Option<u64>,
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
        return Err("limit must be between 1 and 100");
    }
    Ok(n as usize)
}

/// 合法 target: `/` 或任意规范的 content/HOME 相对目录 route。
pub fn valid_target(target: &str) -> bool {
    termblog_content_model::validate_target(target).is_ok()
}

/// 将私有、全局 ID 模型投影为公开的 target 内局部编号模型。
///
/// 输入必须按全局 ID 严格递增；reply 只能指向同 target 的更早评论。
pub fn visible_comments(comments: &[Comment]) -> Result<Vec<VisibleComment>, String> {
    let mut last_id = 0;
    let mut next_number: HashMap<&str, u64> = HashMap::new();
    let mut seen: HashMap<u64, (&str, VisibleReply)> = HashMap::new();
    let mut out = Vec::with_capacity(comments.len());

    for comment in comments {
        if comment.id == 0 || comment.id <= last_id {
            return Err("comment IDs not strictly increasing".into());
        }
        last_id = comment.id;

        let counter = next_number.entry(&comment.target).or_default();
        *counter = counter
            .checked_add(1)
            .ok_or_else(|| "local comment numbers exhausted".to_string())?;
        let number = *counter;

        let reply_to = match comment.reply_to_id {
            Some(parent_id) => {
                let Some((parent_target, parent)) = seen.get(&parent_id) else {
                    return Err(format!("reply points to nonexistent or later comment ID {parent_id}"));
                };
                if *parent_target != comment.target.as_str() {
                    return Err(format!("reply crosses target boundary: ID {parent_id}"));
                }
                Some(parent.clone())
            }
            None => None,
        };
        let own_ref = VisibleReply {
            number,
            author: comment.author.clone(),
        };
        seen.insert(comment.id, (&comment.target, own_ref));
        out.push(VisibleComment {
            number,
            target: comment.target.clone(),
            author: comment.author.clone(),
            text: comment.text.clone(),
            created_at: comment.created_at.clone(),
            reply_to,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(id: u64, target: &str, author: &str, reply_to_id: Option<u64>) -> Comment {
        Comment {
            id,
            target: target.into(),
            author: author.into(),
            text: format!("text {id}"),
            created_at: "2026-09-05T00:00:00Z".into(),
            reply_to_id,
        }
    }

    #[test]
    fn projection_uses_per_target_numbers_and_direct_parent() {
        let visible = visible_comments(&[
            comment(1, "/a/", "alice", None),
            comment(2, "/b/", "elsewhere", None),
            comment(3, "/a/", "bob", Some(1)),
            comment(4, "/a/", "carol", Some(3)),
        ])
        .unwrap();

        assert_eq!(visible[0].number, 1);
        assert_eq!(visible[1].number, 1);
        assert_eq!(visible[2].number, 2);
        assert_eq!(
            visible[2].reply_to,
            Some(VisibleReply {
                number: 1,
                author: "alice".into()
            })
        );
        assert_eq!(
            visible[3].reply_to,
            Some(VisibleReply {
                number: 2,
                author: "bob".into()
            })
        );
    }

    #[test]
    fn projection_rejects_missing_or_cross_target_parent() {
        assert!(visible_comments(&[comment(2, "/", "a", Some(1))]).is_err());
        assert!(visible_comments(&[
            comment(1, "/a/", "a", None),
            comment(2, "/b/", "b", Some(1)),
        ])
        .is_err());
    }

    #[test]
    fn public_json_has_no_global_id_and_old_cursor_is_rejected() {
        let response = PublicQueryResponse {
            ok: true,
            revision: "r".into(),
            total: 1,
            omitted_earlier: 0,
            comments: visible_comments(&[comment(99, "/", "alice", None)]).unwrap(),
            next_after_number: None,
            has_more: false,
            error: None,
        };
        let value = serde_json::to_value(response).unwrap();
        let first = &value["comments"][0];
        assert_eq!(first["number"], 1);
        assert!(first.get("id").is_none());
        assert!(first.get("reply_to_id").is_none());

        assert!(serde_json::from_value::<PublicQuery>(serde_json::json!({
            "target": "/",
            "after_id": 1,
            "revision": "r"
        }))
        .is_err());
    }

    #[test]
    fn target_validation() {
        for good in ["/", "/blog/", "/notes/", "/projects/demo/", "/blog/a/b-2/"] {
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
