//! 元数据提取: slug 校验 / 标题 / 摘要 / 日期 / .index 生成。

use std::path::Path;
use std::process::Command;

use pulldown_cmark::{Event, HeadingLevel, Tag, TagEnd};

use crate::Article;

/// slug 白名单校验: `^[a-z0-9/-]+$`(小写字母、数字、连字符、路径分隔),
/// 另加防御性检查(空、`//`、以 `/` 结尾)。违规返回带原因的 Err。
pub fn validate_slug(slug: &str) -> Result<(), String> {
    if slug.is_empty() {
        return Err("slug 为空".into());
    }
    if let Some(c) = slug.chars().find(|c| {
        !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-' || *c == '/')
    }) {
        return Err(format!("含非法字符 '{c}' (仅允许 [a-z0-9/-])"));
    }
    if slug.contains("//") {
        return Err("含连续分隔符 //".into());
    }
    if slug.ends_with('/') {
        return Err("以 / 结尾".into());
    }
    Ok(())
}

/// 收集一段 inline 内容(从 events[*i] 起)的纯文本, 直到遇到 `stop` 结束事件。
/// 嵌套 inline 容器(strong/em/link…)的结束事件跳过, 代码取字面、链接取文字、
/// 图片取 alt、换行按空格。停在 `stop` 处不消费它。
fn collect_inline_text(events: &[Event<'static>], i: &mut usize, out: &mut String, stop: TagEnd) {
    loop {
        match &events[*i] {
            Event::End(t) => {
                if *t == stop {
                    return;
                }
                *i += 1;
            }
            Event::Start(Tag::Image { .. }) => {
                *i += 1;
                let mut alt = String::new();
                collect_inline_text(events, i, &mut alt, TagEnd::Image);
                out.push_str(alt.trim());
                *i += 1; // End(Image)
            }
            Event::Start(_) => *i += 1,
            Event::Text(t) => {
                out.push_str(t);
                *i += 1;
            }
            Event::Code(c) => {
                out.push_str(c);
                *i += 1;
            }
            Event::SoftBreak | Event::HardBreak => {
                out.push(' ');
                *i += 1;
            }
            Event::Html(_)
            | Event::InlineHtml(_)
            | Event::FootnoteReference(_)
            | Event::TaskListMarker(_) => *i += 1,
            _ => *i += 1,
        }
    }
}

/// 标题: 文中第一个 H1 的全部 inline 文本(无 H1 用文件名 stem)。
/// Tab 替换为空格。
pub fn extract_title(events: &[Event<'static>], fallback: &str) -> String {
    let mut i = 0;
    while i < events.len() {
        if let Event::Start(Tag::Heading { level: HeadingLevel::H1, .. }) = events[i] {
            i += 1;
            let mut text = String::new();
            collect_inline_text(events, &mut i, &mut text, TagEnd::Heading(HeadingLevel::H1));
            let t = text.replace('\t', " ").trim().to_string();
            if !t.is_empty() {
                return t;
            }
            break;
        }
        i += 1;
    }
    fallback.to_string()
}

/// 摘要: 第一个段落(Paragraph 块)的纯文本, 折叠空白, 超 160 字符截断加 …。
/// 无(非空)段落时用标题。
pub fn extract_excerpt(events: &[Event<'static>], fallback: &str) -> String {
    let mut i = 0;
    while i < events.len() {
        if let Event::Start(Tag::Paragraph) = events[i] {
            i += 1;
            let mut text = String::new();
            collect_inline_text(events, &mut i, &mut text, TagEnd::Paragraph);
            i += 1; // End(Paragraph)
            let t = collapse_ws(&text);
            if !t.is_empty() {
                return truncate_chars(&t, 160);
            }
            continue;
        }
        i += 1;
    }
    fallback.to_string()
}

/// 折叠空白: 连续空白(含 Tab/换行)压成单个空格, 首尾去空。
pub fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 按字符数截断, 超长以 … 结尾。
pub fn truncate_chars(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        let mut t: String = s.chars().take(n).collect();
        t.push('…');
        t
    } else {
        s.to_string()
    }
}

/// 文章日期: git 最后提交时间(commit date, ISO8601); 无历史时回退文件 mtime。
/// 返回 (YYYY-MM-DD, 完整 RFC3339, 是否走了 fallback)。
pub fn article_date(path: &Path) -> (String, String, bool) {
    if let Ok(out) = Command::new("git")
        .args(["log", "-1", "--format=%cI", "--"])
        .arg(path)
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            let s = s.trim();
            if !s.is_empty() {
                let date10 = s.chars().take(10).collect::<String>();
                return (date10, s.to_string(), false);
            }
        }
    }
    let secs = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dt = chrono::DateTime::from_timestamp(secs as i64, 0)
        .unwrap_or_default()
        .with_timezone(&chrono::Local);
    let rfc = dt.to_rfc3339();
    let date10 = rfc.chars().take(10).collect::<String>();
    (date10, rfc, true)
}

/// .rendered/.index: 一行一篇, TSV = 日期 \t slug \t 标题。
/// 日期倒序, 同日 slug 字典序升序。
pub fn build_index(arts: &[Article]) -> String {
    let mut sorted: Vec<&Article> = arts.iter().collect();
    sorted.sort_by(|a, b| {
        b.date_rfc3339
            .cmp(&a.date_rfc3339)
            .then_with(|| a.slug.cmp(&b.slug))
    });
    let mut out = String::new();
    for a in sorted {
        out.push_str(&a.date10);
        out.push('\t');
        out.push_str(&a.slug);
        out.push('\t');
        out.push_str(&a.title);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulldown_cmark::{Options, Parser};

    fn parse(md: &str) -> Vec<Event<'static>> {
        Parser::new_ext(md, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH)
            .map(|e| e.into_static())
            .collect()
    }

    fn article(slug: &str, date10: &str, date_rfc3339: &str, title: &str) -> Article {
        Article {
            slug: slug.into(),
            title: title.into(),
            excerpt: String::new(),
            date10: date10.into(),
            date_rfc3339: date_rfc3339.into(),
            date_warned: false,
            events: vec![],
        }
    }

    #[test]
    fn slug_validation() {
        assert!(validate_slug("hello").is_ok());
        assert!(validate_slug("2026/notes").is_ok());
        assert!(validate_slug("why-a-terminal-blog").is_ok());
        assert!(validate_slug("Hello").is_err()); // 大写
        assert!(validate_slug("my_post").is_err()); // 下划线
        assert!(validate_slug("你好").is_err()); // 中文
        assert!(validate_slug("a..b").is_err()); // 点
        assert!(validate_slug("a b").is_err()); // 空格
        assert!(validate_slug("").is_err()); // 空
        assert!(validate_slug("a//b").is_err()); // 连续分隔符
        assert!(validate_slug("a/").is_err()); // 尾部斜杠
    }

    #[test]
    fn title_extraction() {
        // 有 H1
        assert_eq!(extract_title(&parse("# 你好, 世界\n\n正文"), "fallback"), "你好, 世界");
        // 无 H1 → 文件名 stem
        assert_eq!(extract_title(&parse("只有正文, 没有标题\n"), "my-post"), "my-post");
        // H1 带行内代码与链接
        assert_eq!(
            extract_title(&parse("# `less -R` 与 [链接](https://x.example) 标题"), "fb"),
            "less -R 与 链接 标题"
        );
        // Tab 换空格
        assert_eq!(extract_title(&parse("# 你好\t世界"), "fb"), "你好 世界");
    }

    #[test]
    fn excerpt_extraction() {
        // 首段纯文本 + 空白折叠
        assert_eq!(
            extract_excerpt(&parse("第一  段\n有多行\t与空白。\n\n第二段"), "fb"),
            "第一 段 有多行 与空白。"
        );
        // 160 字符截断, 以 … 结尾
        let long = "字".repeat(200);
        let ex = extract_excerpt(&parse(&format!("{long}\n\n尾段")), "fb");
        assert_eq!(ex.chars().count(), 161);
        assert_eq!(ex.chars().take(160).collect::<String>(), "字".repeat(160));
        assert!(ex.ends_with('…'));
        // 无段落 → fallback 标题
        assert_eq!(extract_excerpt(&parse("# 只有标题"), "标题"), "标题");
    }

    #[test]
    fn index_build() {
        let a = article("hello", "2026-08-29", "2026-08-29T10:00:00+08:00", "你好, 世界");
        let b = article(
            "why-a-terminal-blog",
            "2026-08-29",
            "2026-08-29T11:00:00+08:00",
            "为什么把博客做成一个终端",
        );
        let c = article("2026/notes", "2026-08-28", "2026-08-28T09:00:00+08:00", "笔记");
        // 同日(08-29): 按完整时间倒序(b 11:00 在前), 跨日按日期倒序
        assert_eq!(
            build_index(&[c, a, b]),
            "2026-08-29\twhy-a-terminal-blog\t为什么把博客做成一个终端\n\
             2026-08-29\thello\t你好, 世界\n\
             2026-08-28\t2026/notes\t笔记\n"
        );
        // 标题里 Tab 已被替换(标题提取保证, 这里直接构造含 Tab 的标题验证 build_index 不额外处理)
        let d = article("t", "2026-08-01", "2026-08-01T00:00:00+08:00", "含\tTab");
        assert_eq!(build_index(&[d]), "2026-08-01\tt\t含\tTab\n");
    }

    #[test]
    fn collapse_and_truncate() {
        assert_eq!(collapse_ws("  a \n\t b  c "), "a b c");
        assert_eq!(truncate_chars("abc", 5), "abc");
        assert_eq!(truncate_chars("abcdef", 3), "abc…");
    }
}
