//! `blog` reads ordinary files, and uses the build index for published articles.
//!
//! The guest HOME mirrors `jailtpl/content`. No directory name is special:
//! published Markdown is identified only by `~/.rendered/.index.json`.

mod comments;
mod iip;
mod reader;

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use termblog_content_model::{ArticleIndex, ArticleIndexEntry, ArticlePath, CommentAttachment};

use crate::webctl::osc_url;

const TARGETS_FILE: &str = "/usr/local/share/termblog/comment-targets.tsv";

pub fn run(args: &[String]) -> i32 {
    let home = match std::env::var_os("HOME") {
        Some(value) => PathBuf::from(value),
        None => {
            eprintln!("blog: HOME 未设置");
            return 1;
        }
    };
    let rendered_dir = home.join(".rendered");

    if args.first().map(String::as_str) == Some("--dump-image-frame") {
        return self::reader::dump_cli(&args[1..], &home);
    }

    if args.is_empty() || (args.len() == 1 && args[0].is_empty()) {
        let index = rendered_dir.join(".index");
        if index.is_file() {
            if cat(&index).is_err() {
                return 1;
            }
        } else {
            println!("暂无文章（模板里没有 .rendered/.index，请重建模板）");
        }
        return 0;
    }

    let operands = if args.first().map(String::as_str) == Some("--") {
        &args[1..]
    } else {
        if args.first().is_some_and(|arg| arg.starts_with('-')) {
            eprintln!("blog: 未知选项 {}（路径以 - 开头时请先写 --）", args[0]);
            return 2;
        }
        args
    };
    if operands.len() != 1 {
        eprintln!("用法: blog [--] <HOME 相对文章键或文件路径>");
        return 2;
    }

    let arg = &operands[0];
    let Some(file) = resolve_file(arg, &home) else {
        eprintln!("blog: 没有这个文件: {arg}（敲 blog 看列表）");
        return 1;
    };
    let source_rel = home_relative_file(&file, &home);
    let index_result = load_machine_index(&rendered_dir.join(".index.json"));
    let article = index_result
        .as_ref()
        .ok()
        .and_then(|entries| source_rel.as_deref().and_then(|source| entries.get(source)));

    if article.is_none() {
        if let Err(error) = &index_result {
            eprintln!("blog: 文章映射不可用（{error}），按普通文件显示");
        }
        return show_file(&file);
    }
    let article = article.expect("checked above");

    emit_osc(&article.route);
    let rendered = rendered_dir.join(&article.key);
    if rendered.is_file() {
        if let Some(code) = self::reader::try_run(&home, &rendered) {
            render_article_comments(article);
            emit_osc("/");
            return code;
        }
    }

    let source = if rendered.is_file() {
        rendered
    } else {
        eprintln!(
            "blog: 「{}」无预渲染产物，显示原始 markdown（重建模板后即排版）",
            article.key
        );
        file
    };
    let code = show_file(&source);
    render_article_comments(article);
    emit_osc("/");
    code
}

fn resolve_file(arg: &str, home: &Path) -> Option<PathBuf> {
    [
        PathBuf::from(arg),
        home.join(arg),
        home.join(format!("{arg}.md")),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
}

fn home_relative_file(file: &Path, home: &Path) -> Option<String> {
    let absolute_file = std::fs::canonicalize(file).ok()?;
    let absolute_home = std::fs::canonicalize(home).ok()?;
    absolute_file
        .strip_prefix(absolute_home)
        .ok()?
        .to_str()
        .map(str::to_owned)
}

fn load_machine_index(path: &Path) -> Result<HashMap<String, ArticleIndexEntry>, String> {
    let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let index: ArticleIndex =
        serde_json::from_str(&text).map_err(|error| format!("索引 JSON: {error}"))?;
    index.validate().map_err(|error| error.to_string())?;
    let mut entries = HashMap::new();
    for entry in index.articles {
        if entries.insert(entry.source_rel.clone(), entry).is_some() {
            return Err("文章索引含重复 source_rel".into());
        }
    }
    Ok(entries)
}

fn parse_attachments(text: &str) -> Result<HashMap<String, CommentAttachment>, String> {
    let mut attachments = HashMap::new();
    for (index, line) in text.lines().enumerate() {
        let (fifo, target) = line
            .split_once('\t')
            .ok_or_else(|| format!("评论清单第 {} 行缺 Tab", index + 1))?;
        if target.contains('\t') {
            return Err(format!("评论清单第 {} 行字段过多", index + 1));
        }
        let directory = if fifo == "comment" {
            ""
        } else {
            fifo.strip_suffix("/comment")
                .ok_or_else(|| format!("评论清单第 {} 行 FIFO 非法", index + 1))?
        };
        let attachment = CommentAttachment::from_directory_rel(directory)
            .map_err(|error| format!("评论清单第 {} 行: {error}", index + 1))?;
        if attachment.fifo_rel != fifo || attachment.target != target {
            return Err(format!("评论清单第 {} 行映射不一致", index + 1));
        }
        if attachments
            .insert(attachment.directory_rel.clone(), attachment)
            .is_some()
        {
            return Err(format!("评论清单第 {} 行目录重复", index + 1));
        }
    }
    Ok(attachments)
}

fn render_article_comments(article: &ArticleIndexEntry) {
    let Ok(path) = ArticlePath::parse(&article.source_rel) else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(TARGETS_FILE) else {
        return;
    };
    let Ok(attachments) = parse_attachments(&text) else {
        return;
    };
    let Some(attachment) = attachments.get(&path.directory_rel) else {
        return;
    };
    let noun = if attachment.target == "/" {
        "留言"
    } else {
        "评论"
    };
    comments::render(
        &attachment.target,
        &format!(
            "暂无{noun} —— echo 'alice: 好文' > ~/{} 写第一条",
            attachment.fifo_rel
        ),
    );
}

fn show_file(path: &Path) -> i32 {
    if std::io::stdin().is_terminal() {
        let _ = Command::new("less").arg("-RXc").arg(path).status();
        0
    } else if cat(path).is_err() {
        1
    } else {
        0
    }
}

fn emit_osc(path: &str) {
    let mut out = std::io::stdout().lock();
    if out.write_all(&osc_url(path)).is_ok() {
        let _ = out.flush();
    }
}

fn cat(path: &Path) -> std::io::Result<()> {
    let mut file = std::fs::File::open(path)?;
    let mut out = std::io::stdout().lock();
    std::io::copy(&mut file, &mut out)?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_manifest_accepts_general_directories() {
        let attachments = parse_attachments(
            "comment\t/\nnotes/comment\t/notes/\nprojects/demo/comment\t/projects/demo/\n",
        )
        .unwrap();
        assert_eq!(attachments[""].target, "/");
        assert_eq!(attachments["notes"].fifo_rel, "notes/comment");
        assert_eq!(attachments["projects/demo"].target, "/projects/demo/");
        assert!(parse_attachments("notes/comment\t/blog/\n").is_err());
        assert!(parse_attachments("notes/../comment\t/notes/../\n").is_err());
    }

    #[test]
    fn machine_index_rejects_inconsistent_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("index.json");
        std::fs::write(
            &file,
            r#"{"version":1,"articles":[{"date10":"2026-09-04","source_rel":"notes/a.md","key":"wrong","route":"/notes/a/","title":"A"}]}"#,
        )
        .unwrap();
        assert!(load_machine_index(&file).is_err());
    }

    #[test]
    fn file_resolution_uses_home_root_without_special_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("notes")).unwrap();
        std::fs::write(dir.path().join("notes/unix.md"), "# Unix").unwrap();
        assert_eq!(
            resolve_file("notes/unix", dir.path()),
            Some(dir.path().join("notes/unix.md"))
        );
        assert_eq!(
            home_relative_file(&dir.path().join("notes/unix.md"), dir.path()).as_deref(),
            Some("notes/unix.md")
        );
    }
}
