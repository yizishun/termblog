//! webctl —— 终端↔网页桥(termblog M5)。
//!
//! 提供终端→网页控制序列：私有 OSC 7777 由前端消费并同步 URL；标准 OSC 2
//! 由 xterm/SSH 客户端消费并更新窗口标题。
//!
//! 用法: webctl url /path       (path 必须以 / 开头)
//! 子命令位留给后续(theme 等)。

use std::io::Write;

const MAX_TITLE_BYTES: usize = 512;

/// OSC 7777 序列: `ESC ] 7777 ; url=<path> BEL`
pub fn osc_url(path: &str) -> Vec<u8> {
    let mut b = vec![0x1b];
    b.extend_from_slice(b"]7777;url=");
    b.extend_from_slice(path.as_bytes());
    b.push(0x07);
    b
}

/// 标准 OSC 2 窗口标题序列。拒绝空值、控制字符和超长输入，避免标题提前终止
/// 或向后续终端数据注入额外控制序列。
pub fn osc_title(title: &str) -> Option<Vec<u8>> {
    if title.is_empty() || title.len() > MAX_TITLE_BYTES || title.chars().any(char::is_control) {
        return None;
    }
    let mut b = vec![0x1b];
    b.extend_from_slice(b"]2;");
    b.extend_from_slice(title.as_bytes());
    b.push(0x07);
    Some(b)
}

pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("url") => {
            let Some(p) = args.get(1) else {
                eprintln!("usage: webctl url /path");
                return 2;
            };
            if p.starts_with('/') {
                let mut out = std::io::stdout().lock();
                if out.write_all(&osc_url(p)).and_then(|()| out.flush()).is_err() {
                    return 2;
                }
                0
            } else {
                eprintln!("usage: webctl url /path");
                2
            }
        }
        _ => {
            eprintln!("usage: webctl url /path");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_osc_bytes() {
        assert_eq!(osc_url("/blog/hello/"), b"\x1b]7777;url=/blog/hello/\x07");
        assert_eq!(osc_url("/"), b"\x1b]7777;url=/\x07");
    }

    #[test]
    fn title_osc_bytes_and_validation() {
        assert_eq!(
            osc_title("你好, 世界"),
            Some("\x1b]2;你好, 世界\x07".as_bytes().to_vec())
        );
        assert_eq!(osc_title(""), None);
        assert_eq!(osc_title("bad\x07title"), None);
        assert_eq!(osc_title(&"x".repeat(MAX_TITLE_BYTES + 1)), None);
    }

    #[test]
    fn usage_rejects_bad_args() {
        assert_eq!(run(&[]), 2);
        assert_eq!(run(&["theme".into()]), 2);
        assert_eq!(run(&["url".into()]), 2);
        assert_eq!(run(&["url".into(), "blog/hello/".into()]), 2);
    }
}
