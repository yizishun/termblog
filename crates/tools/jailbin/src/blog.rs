//! blog —— 文章阅读入口(termblog M5)。
//!
//! 接口像 cat: 任意路径的 md 都能读, 任意目录层级(相对 cwd / 相对 ~/blog / 绝对)。
//! 内容上更聪明: ~/blog/ 下的文章读预渲染排版 ~/.rendered/<slug>(76 列折行、
//! 标题加粗/表格对齐等由 content-build 构建期算好); 没有产物的才退回原始 md。
//! cat/less 任何时候读的都是原始未渲染的 markdown。
//!
//!   blog                列出文章(~/.rendered/.index)
//!   blog hello          = blog ~/blog/hello.md, 读预渲染排版
//!   blog <其他文件.md>   像 cat 一样读原始内容(不渲染、不同步地址栏)
//!
//! 读 ~/blog/ 下文章时: 进入同步地址栏 /blog/<slug>/(尾斜杠 = canonical 形态),
//! 退出分页器后复位 /。交互终端用 less -RXc 分页(-R 解释颜色/粗体, -X 不进
//! 备用屏不闪屏, -c 清屏重画, attach 回放不叠旧画面), 非交互管道直接 cat。

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::webctl::osc_url;

pub fn run(args: &[String]) -> i32 {
    let home = match std::env::var_os("HOME") {
        Some(h) => PathBuf::from(h),
        None => {
            eprintln!("blog: HOME 未设置");
            return 1;
        }
    };
    let blog_dir = home.join("blog");
    let rendered_dir = home.join(".rendered");

    // 裸 blog(含 shell 版 blog "" 的等价行为)列出文章
    if args.is_empty() || (args.len() == 1 && args[0].is_empty()) {
        let index = rendered_dir.join(".index");
        if index.is_file() {
            if cat(&index).is_err() {
                return 1;
            }
        } else {
            println!("暂无文章(模板里没有 .rendered/.index, 重建模板试试)");
        }
        return 0;
    }

    let arg = &args[0];
    // 解析文件: 原样(相对 cwd / 绝对)→ 相对 ~/blog → ~/blog/<名>.md(blog hello 简写)
    let mut file: Option<PathBuf> = None;
    for cand in [
        PathBuf::from(arg),
        blog_dir.join(arg),
        blog_dir.join(format!("{arg}.md")),
    ] {
        if cand.is_file() {
            file = Some(cand);
            break;
        }
    }
    let Some(mut file) = file else {
        eprintln!("blog: 没有这个文件: {arg} (敲 blog 看列表)");
        return 1;
    };
    // 规范化成绝对路径(相对 cwd 的补 current_dir), 便于按 ~/blog/ 前缀算 slug
    if !file.is_absolute() {
        if let Ok(cwd) = std::env::current_dir() {
            file = cwd.join(file);
        }
    }

    // 文件在 ~/blog/ 下 → 算 slug 并同步地址栏(带尾斜杠 = canonical 形态)
    // slug 白名单 [a-z0-9/-] 与内容编译器一致, 违规则不同步(仅阅读)
    let slug = slug_of(&file, &blog_dir);

    if !slug.is_empty() {
        emit_osc(&format!("/blog/{slug}/"));
    }

    // 内容源: 文章优先读预渲染排版; 无产物(模板建成后才加的 md)退回原始文件
    let src = if !slug.is_empty() {
        let rp = rendered_dir.join(&slug);
        if rp.is_file() {
            rp
        } else {
            eprintln!("blog: 「{slug}」无预渲染产物, 显示原始 markdown(重建模板后即排版)");
            file
        }
    } else {
        file
    };

    if std::io::stdin().is_terminal() {
        // less 失败也继续复位(与 shell 版 `|| true` 一致)
        let _ = Command::new("less").arg("-RXc").arg(&src).status();
    } else if cat(&src).is_err() {
        return 1;
    }

    if !slug.is_empty() {
        emit_osc("/");
    }
    0
}

/// 文件在 ~/blog/ 下且以 .md 结尾 → 相对 slug; 白名单 [a-z0-9/-] 校验
/// (空串 / 以 / 结尾 / 含 // / 其它字符 → 空串, 不同步地址栏)。
fn slug_of(file: &Path, blog_dir: &Path) -> String {
    let Ok(rel) = file.strip_prefix(blog_dir) else {
        return String::new();
    };
    let s = rel.to_string_lossy();
    let Some(stem) = s.strip_suffix(".md") else {
        return String::new();
    };
    let valid = !stem.is_empty()
        && !stem.ends_with('/')
        && !stem.contains("//")
        && stem
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '/' || c == '-');
    if valid { stem.to_string() } else { String::new() }
}

fn emit_osc(path: &str) {
    // OSC 必须立即 flush: Rust stdout 是行缓冲, OSC 无换行, 若不 flush 会滞留
    // 缓冲区, 会话被回收(SIGKILL)时丢失 —— shell 版 printf 是无缓冲直写。
    let mut out = std::io::stdout().lock();
    if out.write_all(&osc_url(path)).is_ok() {
        let _ = out.flush();
    }
}

fn cat(p: &Path) -> std::io::Result<()> {
    let mut f = std::fs::File::open(p)?;
    let mut out = std::io::stdout().lock();
    std::io::copy(&mut f, &mut out)?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_whitelist() {
        let bd = Path::new("/home/guest/blog");
        assert_eq!(slug_of(Path::new("/home/guest/blog/hello.md"), bd), "hello");
        assert_eq!(slug_of(Path::new("/home/guest/blog/a/b/c.md"), bd), "a/b/c");
        assert_eq!(slug_of(Path::new("/home/guest/blog/a-b/9x.md"), bd), "a-b/9x");
        assert_eq!(slug_of(Path::new("/home/guest/blog/x.md.md"), bd), ""); // 残留 . 不在白名单
        // 违规: 大写/非白名单字符/双斜杠/目录外/非 .md
        assert_eq!(slug_of(Path::new("/home/guest/blog/HeLLo.md"), bd), "");
        assert_eq!(slug_of(Path::new("/home/guest/blog/中文.md"), bd), "");
        assert_eq!(slug_of(Path::new("/home/guest/blog/a//b.md"), bd), "");
        assert_eq!(slug_of(Path::new("/home/guest/other/hello.md"), bd), "");
        assert_eq!(slug_of(Path::new("/home/guest/blog/hello.txt"), bd), "");
        assert_eq!(slug_of(Path::new("/home/guest/blog/hello"), bd), "");
    }
}
