//! webctl —— 终端↔网页桥(termblog M5)。
//!
//! 唯一职责: 把 OSC 7777 序列打到 stdout, 由前端的 osc.ts 消费(replaceState)。
//! ssh 客户端不认识该 OSC, 按 VT 规范吞掉, 无副作用。
//!
//! 用法: webctl url /path       (path 必须以 / 开头)
//! 子命令位留给后续(theme 等)。

use std::io::Write;

/// OSC 7777 序列: `ESC ] 7777 ; url=<path> BEL`
pub fn osc_url(path: &str) -> Vec<u8> {
    let mut b = vec![0x1b];
    b.extend_from_slice(b"]7777;url=");
    b.extend_from_slice(path.as_bytes());
    b.push(0x07);
    b
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
    fn osc_bytes() {
        assert_eq!(osc_url("/blog/hello/"), b"\x1b]7777;url=/blog/hello/\x07");
        assert_eq!(osc_url("/"), b"\x1b]7777;url=/\x07");
    }

    #[test]
    fn usage_rejects_bad_args() {
        assert_eq!(run(&[]), 2);
        assert_eq!(run(&["theme".into()]), 2);
        assert_eq!(run(&["url".into()]), 2);
        assert_eq!(run(&["url".into(), "blog/hello/".into()]), 2);
    }
}
