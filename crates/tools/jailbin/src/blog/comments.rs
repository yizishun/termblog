use std::io::{self, Write};
use std::path::Path;

use serde::Deserialize;

const SNAPSHOT: &str = "/var/run/termblog/comments.jsonl";
const MAX_SHOWN: usize = 100;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotComment {
    id: u64,
    target: String,
    author: String,
    date10: String,
    text: String,
}

pub fn render(target: &str, empty_hint: &str) {
    let mut out = io::stdout().lock();
    match load(Path::new(SNAPSHOT), target) {
        Ok(comments) => {
            let total = comments.len();
            let start = total.saturating_sub(MAX_SHOWN);
            let _ = writeln!(out, "\n── 评论 ({total}) ───────────────────────────");
            if start > 0 {
                let _ = writeln!(out, "… 还有 {start} 条更早评论\n");
            }
            for (offset, c) in comments[start..].iter().enumerate() {
                let author = strip_controls(&c.author);
                let text = strip_controls(&c.text);
                let ordinal = start + offset + 1;
                let _ = writeln!(
                    out,
                    "\x1b[2m#{}\x1b[0m  \x1b[1m{}\x1b[0m \x1b[2m· {}\x1b[0m\n    {}\n",
                    ordinal, author, c.date10, text
                );
            }
            if comments.is_empty() {
                let _ = writeln!(out, "({empty_hint})");
            }
        }
        Err(_) => {
            let _ = writeln!(
                out,
                "\n── 评论 ───────────────────────────────\n(评论暂不可用)"
            );
        }
    }
    let _ = out.flush();
}

fn load(path: &Path, target: &str) -> Result<Vec<SnapshotComment>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut last_id = 0;
    for (idx, line) in text.lines().enumerate() {
        if line.is_empty() {
            return Err(format!("第 {} 行为空", idx + 1));
        }
        let c: SnapshotComment =
            serde_json::from_str(line).map_err(|e| format!("第 {} 行: {e}", idx + 1))?;
        if c.id == 0 || c.id <= last_id {
            return Err(format!("第 {} 行 ID 非严格递增", idx + 1));
        }
        if !valid_target(&c.target)
            || c.author.is_empty()
            || c.author.len() > 32
            || c.text.is_empty()
            || c.text.len() > 512
            || !valid_date10(&c.date10)
        {
            return Err(format!("第 {} 行字段非法", idx + 1));
        }
        last_id = c.id;
        if c.target == target {
            out.push(c);
        }
    }
    Ok(out)
}

fn valid_target(target: &str) -> bool {
    termblog_content_model::validate_target(target).is_ok()
}

fn valid_date10(s: &str) -> bool {
    s.len() == 10
        && s.as_bytes()[4] == b'-'
        && s.as_bytes()[7] == b'-'
        && s.bytes()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
}

fn strip_controls(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(*c as u32, 0x00..=0x1f | 0x7f..=0x9f))
        .collect()
}

#[cfg(test)]
fn display_ordinal(omitted_earlier: usize, offset: usize) -> usize {
    omitted_earlier + offset + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_snapshot_fails_on_bad_line() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("comments.jsonl");
        std::fs::write(
            &path,
            "{\"id\":1,\"target\":\"/\",\"author\":\"a\",\"date10\":\"2026-09-01\",\"text\":\"ok\"}\nnot json\n",
        )
        .unwrap();
        assert!(load(&path, "/").is_err());
    }

    #[test]
    fn filters_target_and_controls_at_output_boundary() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("comments.jsonl");
        std::fs::write(
            &path,
            "{\"id\":1,\"target\":\"/blog/\",\"author\":\"a\\u001b\",\"date10\":\"2026-09-01\",\"text\":\"ok\"}\n{\"id\":2,\"target\":\"/\",\"author\":\"b\",\"date10\":\"2026-09-02\",\"text\":\"root\"}\n",
        )
        .unwrap();
        let got = load(&path, "/blog/").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(strip_controls(&got[0].author), "a");
    }

    #[test]
    fn visible_numbers_are_contiguous_within_target() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("comments.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"id\":1,\"target\":\"/blog/a/\",\"author\":\"a\",\"date10\":\"2026-09-01\",\"text\":\"first\"}\n",
                "{\"id\":2,\"target\":\"/blog/b/\",\"author\":\"b\",\"date10\":\"2026-09-01\",\"text\":\"other article\"}\n",
                "{\"id\":3,\"target\":\"/blog/a/\",\"author\":\"c\",\"date10\":\"2026-09-02\",\"text\":\"second\"}\n",
            ),
        )
        .unwrap();

        let comments = load(&path, "/blog/a/").unwrap();
        let shown: Vec<_> = comments
            .iter()
            .enumerate()
            .map(|(offset, comment)| (display_ordinal(0, offset), comment.id))
            .collect();
        assert_eq!(shown, vec![(1, 1), (2, 3)]);
    }
}
