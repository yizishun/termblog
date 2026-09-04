//! Shared, I/O-free path model for termblog content.
//!
//! `jailtpl/content` and the guest's HOME have the same visible layout. This
//! crate is the single place that converts those relative paths into article
//! keys, public routes, and comment attachments.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const MAX_TARGET_LEN: usize = 200;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathModelError(String);

impl fmt::Display for PathModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PathModelError {}

fn err(message: impl Into<String>) -> PathModelError {
    PathModelError(message.into())
}

/// A Markdown source and all of its deterministic projections.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArticlePath {
    /// HOME/content-relative source path including the lowercase `.md` suffix.
    pub source_rel: PathBuf,
    /// Slash-separated article key without `.md`.
    pub key: String,
    /// Canonical public route with leading and trailing slash.
    pub route: String,
    /// Slash-separated immediate parent directory; empty means HOME itself.
    pub directory_rel: String,
}

impl ArticlePath {
    pub fn from_source_rel(path: &Path) -> Result<Self, PathModelError> {
        let source = path.to_str().ok_or_else(|| err("文章路径不是 UTF-8"))?;
        Self::parse(source)
    }

    pub fn parse(source: &str) -> Result<Self, PathModelError> {
        validate_relative(source, false, validate_article_source_segment, "文章路径")?;
        let key = source
            .strip_suffix(".md")
            .ok_or_else(|| err("文章扩展名必须精确为小写 .md"))?;
        if key.is_empty() || key.ends_with('/') {
            return Err(err("文章文件名为空"));
        }
        for segment in key.split('/') {
            validate_article_segment(segment)?;
        }
        let directory_rel = key.rsplit_once('/').map_or("", |(parent, _)| parent);
        Ok(Self {
            source_rel: PathBuf::from(source),
            key: key.to_owned(),
            route: format!("/{key}/"),
            directory_rel: directory_rel.to_owned(),
        })
    }
}

/// A configured comment FIFO and its API/storage target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommentAttachment {
    pub directory_rel: String,
    pub fifo_rel: String,
    pub target: String,
}

impl CommentAttachment {
    pub fn from_directory_rel(directory: &str) -> Result<Self, PathModelError> {
        if !directory.is_empty() {
            validate_relative(directory, false, validate_article_segment, "评论目录")?;
        }
        let target = if directory.is_empty() {
            "/".to_owned()
        } else {
            format!("/{directory}/")
        };
        validate_target(&target)?;
        Ok(Self {
            directory_rel: directory.to_owned(),
            fifo_rel: if directory.is_empty() {
                "comment".to_owned()
            } else {
                format!("{directory}/comment")
            },
            target,
        })
    }
}

/// Machine-readable article index installed under `~/.rendered/.index.json`.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ArticleIndex {
    pub version: u32,
    pub articles: Vec<ArticleIndexEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ArticleIndexEntry {
    pub date10: String,
    pub source_rel: String,
    pub key: String,
    pub route: String,
    pub title: String,
}

impl ArticleIndex {
    pub fn validate(&self) -> Result<(), PathModelError> {
        if self.version != 1 {
            return Err(err(format!("不支持的文章索引版本: {}", self.version)));
        }
        let mut sources = BTreeSet::new();
        let mut keys = BTreeSet::new();
        let mut routes = BTreeSet::new();
        for entry in &self.articles {
            let path = ArticlePath::parse(&entry.source_rel)?;
            if path.key != entry.key || path.route != entry.route {
                return Err(err(format!(
                    "文章索引映射不一致: {} 应为 key={} route={}",
                    entry.source_rel, path.key, path.route
                )));
            }
            if entry.date10.len() != 10
                || !entry.date10.bytes().enumerate().all(|(i, b)| {
                    if matches!(i, 4 | 7) {
                        b == b'-'
                    } else {
                        b.is_ascii_digit()
                    }
                })
            {
                return Err(err(format!("文章索引日期不合法: {}", entry.date10)));
            }
            if entry.title.chars().any(char::is_control) {
                return Err(err(format!("文章索引标题含控制字符: {}", entry.source_rel)));
            }
            if !sources.insert(&entry.source_rel)
                || !keys.insert(&entry.key)
                || !routes.insert(&entry.route)
            {
                return Err(err(format!("文章索引含重复映射: {}", entry.source_rel)));
            }
        }
        Ok(())
    }
}

/// True when any slash-separated component begins with `.`.
pub fn has_hidden_component(path: &str) -> bool {
    path.split('/').any(|segment| segment.starts_with('.'))
}

/// Validate a public comment target (`/` or `/a/b/`).
pub fn validate_target(target: &str) -> Result<(), PathModelError> {
    if target == "/" {
        return Ok(());
    }
    if target.len() > MAX_TARGET_LEN {
        return Err(err("评论 target 过长"));
    }
    let inner = target
        .strip_prefix('/')
        .and_then(|s| s.strip_suffix('/'))
        .ok_or_else(|| err("评论 target 必须以 / 开头并以 / 结尾"))?;
    validate_relative(inner, false, validate_article_segment, "评论 target")
}

/// Validate a visible HOME-relative resource path.
pub fn validate_resource_rel(path: &str) -> Result<(), PathModelError> {
    validate_relative(path, false, validate_resource_segment, "资源路径")
}

fn validate_relative(
    path: &str,
    allow_empty: bool,
    segment_validator: fn(&str) -> Result<(), PathModelError>,
    kind: &str,
) -> Result<(), PathModelError> {
    if path.is_empty() {
        return if allow_empty {
            Ok(())
        } else {
            Err(err(format!("{kind}为空")))
        };
    }
    if path.starts_with('/') || path.ends_with('/') {
        return Err(err(format!("{kind}必须是无首尾斜杠的相对路径")));
    }
    if path.contains('\\') {
        return Err(err(format!("{kind}不能含反斜杠")));
    }
    for segment in path.split('/') {
        if segment.is_empty() {
            return Err(err(format!("{kind}含空组件")));
        }
        if matches!(segment, "." | "..") {
            return Err(err(format!("{kind}不能含 {segment} 组件")));
        }
        if segment.starts_with('.') {
            return Err(err(format!("{kind}不能含隐藏组件 {segment}")));
        }
        segment_validator(segment)?;
    }
    Ok(())
}

fn validate_article_source_segment(segment: &str) -> Result<(), PathModelError> {
    if let Some(stem) = segment.strip_suffix(".md") {
        validate_article_segment(stem)
    } else {
        validate_article_segment(segment)
    }
}

fn validate_article_segment(segment: &str) -> Result<(), PathModelError> {
    if segment.is_empty() {
        return Err(err("路径组件为空"));
    }
    if let Some(ch) = segment
        .chars()
        .find(|ch| !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || *ch == '-'))
    {
        return Err(err(format!("含非法字符 '{ch}'（仅允许 [a-z0-9-]）")));
    }
    Ok(())
}

fn validate_resource_segment(segment: &str) -> Result<(), PathModelError> {
    if let Some(ch) = segment.chars().find(|ch| {
        !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '_' | '-'))
    }) {
        return Err(err(format!("含非法字符 '{ch}'（仅允许 [a-z0-9._-]）")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn article_mappings_are_content_relative() {
        assert_eq!(
            ArticlePath::parse("help.md").unwrap(),
            ArticlePath {
                source_rel: "help.md".into(),
                key: "help".into(),
                route: "/help/".into(),
                directory_rel: "".into(),
            }
        );
        let nested = ArticlePath::parse("notes/unix.md").unwrap();
        assert_eq!(nested.key, "notes/unix");
        assert_eq!(nested.route, "/notes/unix/");
        assert_eq!(nested.directory_rel, "notes");
        assert_eq!(ArticlePath::parse("blog/a.md").unwrap().route, "/blog/a/");
    }

    #[test]
    fn article_paths_are_strict() {
        for bad in [
            "",
            "/a.md",
            "a.md/",
            "a//b.md",
            "./a.md",
            "../a.md",
            "a\\b.md",
            "A.md",
            "a_b.md",
            "a b.md",
            "你好.md",
            ".hidden/a.md",
            "a.MD",
        ] {
            assert!(ArticlePath::parse(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn hidden_components_are_detected() {
        assert!(has_hidden_component(".rendered/a"));
        assert!(has_hidden_component("notes/.draft/a.md"));
        assert!(!has_hidden_component("notes/a.md"));
    }

    #[test]
    fn attachments_map_directories() {
        assert_eq!(
            CommentAttachment::from_directory_rel("").unwrap(),
            CommentAttachment {
                directory_rel: "".into(),
                fifo_rel: "comment".into(),
                target: "/".into(),
            }
        );
        assert_eq!(
            CommentAttachment::from_directory_rel("projects/demo").unwrap(),
            CommentAttachment {
                directory_rel: "projects/demo".into(),
                fifo_rel: "projects/demo/comment".into(),
                target: "/projects/demo/".into(),
            }
        );
        for bad in ["/notes", "notes/", "notes//demo", ".hidden", "a_b"] {
            assert!(CommentAttachment::from_directory_rel(bad).is_err());
        }
        assert!(CommentAttachment::from_directory_rel(&"a".repeat(MAX_TARGET_LEN)).is_err());
    }

    #[test]
    fn targets_and_resources_are_strict() {
        for good in ["/", "/notes/", "/projects/demo/"] {
            assert!(validate_target(good).is_ok(), "rejected {good:?}");
        }
        for bad in ["", "notes/", "/notes", "//", "/a//b/", "/a_b/"] {
            assert!(validate_target(bad).is_err(), "accepted {bad:?}");
        }
        for good in ["pixel.png", "notes/shared/a_b-2.webp"] {
            assert!(validate_resource_rel(good).is_ok(), "rejected {good:?}");
        }
        for bad in [
            "../pixel.png",
            ".hidden/a.png",
            "A.png",
            "a b.png",
            "a\\b.png",
        ] {
            assert!(validate_resource_rel(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn machine_index_rejects_duplicate_mapping() {
        let entry = ArticleIndexEntry {
            date10: "2026-09-04".into(),
            source_rel: "notes/a.md".into(),
            key: "notes/a".into(),
            route: "/notes/a/".into(),
            title: "A".into(),
        };
        let index = ArticleIndex {
            version: 1,
            articles: vec![entry.clone(), entry],
        };
        assert!(index.validate().is_err());
    }
}
