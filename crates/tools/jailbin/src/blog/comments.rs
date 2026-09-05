use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;

use serde::Deserialize;

const SNAPSHOT: &str = "/var/run/termblog/comments.jsonl";
const MAX_SHOWN: usize = 100;

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SnapshotReply {
    number: u64,
    author: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SnapshotComment {
    number: u64,
    target: String,
    author: String,
    date10: String,
    text: String,
    #[serde(default)]
    reply_to: Option<SnapshotReply>,
}

pub fn render(target: &str, empty_hint: &str) {
    let mut out = io::stdout().lock();
    match load(Path::new(SNAPSHOT), target) {
        Ok(comments) => {
            let total = comments.len();
            let start = total.saturating_sub(MAX_SHOWN);
            let _ = writeln!(out, "\n── Comments ({total}) ───────────────────────────");
            if start > 0 {
                let _ = writeln!(out, "… {start} earlier comments omitted\n");
            }
            for c in &comments[start..] {
                let author = strip_controls(&c.author);
                let text = display_text(c);
                let _ = writeln!(
                    out,
                    "\x1b[2m#{}\x1b[0m  \x1b[1m{}\x1b[0m \x1b[2m· {}\x1b[0m\n    {}\n",
                    c.number, author, c.date10, text
                );
            }
            if comments.is_empty() {
                let _ = writeln!(out, "({empty_hint})");
            }
        }
        Err(_) => {
            let _ = writeln!(
                out,
                "\n── Comments ───────────────────────────────\n(Comments temporarily unavailable)"
            );
        }
    }
    let _ = out.flush();
}

fn load(path: &Path, target: &str) -> Result<Vec<SnapshotComment>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut numbers: HashMap<String, u64> = HashMap::new();
    let mut seen: HashMap<(String, u64), String> = HashMap::new();
    for (idx, line) in text.lines().enumerate() {
        if line.is_empty() {
            return Err(format!("line {} is empty", idx + 1));
        }
        let c: SnapshotComment =
            serde_json::from_str(line).map_err(|e| format!("line {}: {e}", idx + 1))?;
        if !valid_target(&c.target)
            || c.author.is_empty()
            || c.author.len() > 32
            || c.text.is_empty()
            || c.text.len() > 512
            || !valid_date10(&c.date10)
        {
            return Err(format!("line {} has invalid fields", idx + 1));
        }
        let next = numbers.entry(c.target.clone()).or_default();
        *next = next
            .checked_add(1)
            .ok_or_else(|| format!("line {} local number overflow", idx + 1))?;
        if c.number != *next {
            return Err(format!(
                "line {} local number not contiguous: expected {}, got {}",
                idx + 1,
                *next,
                c.number
            ));
        }
        if let Some(parent) = &c.reply_to {
            if parent.number == 0 || parent.author.is_empty() || parent.author.len() > 32 {
                return Err(format!("line {} reply field invalid", idx + 1));
            }
            let key = (c.target.clone(), parent.number);
            let Some(author) = seen.get(&key) else {
                return Err(format!("line {} reply references non-existent or later comment", idx + 1));
            };
            if author != &parent.author {
                return Err(format!("line {} reply author mismatch", idx + 1));
            }
        }
        seen.insert((c.target.clone(), c.number), c.author.clone());
        if c.target == target {
            out.push(c);
        }
    }
    Ok(out)
}

fn display_text(comment: &SnapshotComment) -> String {
    let text = strip_controls(&comment.text);
    match &comment.reply_to {
        Some(parent) => format!(
            "(In reply to {} from comment #{}):\n    {}",
            strip_controls(&parent.author),
            parent.number,
            text
        ),
        None => text,
    }
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
mod tests {
    use super::*;

    fn comment(number: u64, author: &str, parent: Option<(u64, &str)>) -> SnapshotComment {
        SnapshotComment {
            number,
            target: "/".into(),
            author: author.into(),
            date10: "2026-09-05".into(),
            text: format!("text {number}"),
            reply_to: parent.map(|(number, author)| SnapshotReply {
                number,
                author: author.into(),
            }),
        }
    }

    #[test]
    fn whole_snapshot_fails_on_bad_line() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("comments.jsonl");
        std::fs::write(
            &path,
            "{\"number\":1,\"target\":\"/\",\"author\":\"a\",\"date10\":\"2026-09-01\",\"text\":\"ok\"}\nnot json\n",
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
            "{\"number\":1,\"target\":\"/blog/\",\"author\":\"a\\u001b\",\"date10\":\"2026-09-01\",\"text\":\"ok\"}\n{\"number\":1,\"target\":\"/\",\"author\":\"b\",\"date10\":\"2026-09-02\",\"text\":\"root\"}\n",
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
                "{\"number\":1,\"target\":\"/blog/a/\",\"author\":\"a\",\"date10\":\"2026-09-01\",\"text\":\"first\"}\n",
                "{\"number\":1,\"target\":\"/blog/b/\",\"author\":\"b\",\"date10\":\"2026-09-01\",\"text\":\"other article\"}\n",
                "{\"number\":2,\"target\":\"/blog/a/\",\"author\":\"c\",\"date10\":\"2026-09-02\",\"text\":\"second\"}\n",
            ),
        )
        .unwrap();

        let comments = load(&path, "/blog/a/").unwrap();
        assert_eq!(
            comments
                .iter()
                .map(|comment| comment.number)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn nested_replies_show_the_direct_parent_on_its_own_line() {
        let reply = comment(3, "bob", Some((1, "alice")));
        let nested_reply = comment(4, "carol", Some((3, "bob")));
        assert_eq!(
            display_text(&reply),
            "(In reply to alice from comment #1):\n    text 3"
        );
        assert_eq!(
            display_text(&nested_reply),
            "(In reply to bob from comment #3):\n    text 4"
        );
    }

    #[test]
    fn reply_keeps_its_label_when_parent_is_outside_latest_window() {
        let reply = comment(101, "reply", Some((1, "old")));
        assert_eq!(
            display_text(&reply),
            "(In reply to old from comment #1):\n    text 101"
        );
    }

    #[test]
    fn snapshot_rejects_global_ids_gaps_and_bad_parent_summary() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("comments.jsonl");
        for bad in [
            "{\"id\":1,\"target\":\"/\",\"author\":\"a\",\"date10\":\"2026-09-01\",\"text\":\"old schema\"}\n",
            "{\"number\":2,\"target\":\"/\",\"author\":\"a\",\"date10\":\"2026-09-01\",\"text\":\"gap\"}\n",
            "{\"number\":1,\"target\":\"/\",\"author\":\"a\",\"date10\":\"2026-09-01\",\"text\":\"root\"}\n{\"number\":2,\"target\":\"/\",\"author\":\"b\",\"date10\":\"2026-09-02\",\"text\":\"reply\",\"reply_to\":{\"number\":1,\"author\":\"wrong\"}}\n",
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(load(&path, "/").is_err(), "应拒绝 {bad:?}");
        }
    }
}
