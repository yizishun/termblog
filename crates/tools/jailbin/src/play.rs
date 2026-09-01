//! play —— asciicast 终端录像播放器(自包含, 不依赖 asciinema 二进制)。
//!
//! 解析 asciicast v1/v2/v3, 按事件时间戳把输出字节回放进当前终端。
//! 录像随博文走: 每篇博文的资源目录放 .cast, 如 ~/blog/hello/demo.cast。
//!
//!   play                    列出 ~/blog 下可播的 .cast
//!   play hello/demo         = play ~/blog/hello/demo(.cast 可省)
//!   play 任意路径/文件.cast  相对 cwd / 绝对路径原样找
//!   play -s 2 -i 1.5 xxx    2 倍速 + 空闲压缩到 1.5s
//!
//! 交互: 空格 暂停/继续, `.` 逐帧(暂停时), q 或 Ctrl-C 退出。
//! stdout 非终端(如管道)退化为无延时倾倒(等价 asciinema cat), 不读按键。
//! zstd 压缩的 .zst 暂不支持, 报错建议先转成明文。
//!
//! 设计参考 asciinema 3.x 的 player.rs / asciicast.rs(格式、键位、idle 算法),
//! 代码自写: asciinema CLI 是录制/上传/直播全家桶, 播放只需其中很小的子集。

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

/// 输出事件(只保留 "o" 事件; 时间为绝对秒, 数据为原样字节)。
#[derive(Debug, Clone, PartialEq)]
pub struct OutEvent {
    pub t: f64,
    pub data: Vec<u8>,
}

/// 解析结果(header 只留播放所需字段)。
#[derive(Debug)]
// cols/rows/title/version 暂不参与回放(不做自动缩放), 留着供后续扩展
#[allow(dead_code)]
pub struct Cast {
    pub version: u64,
    pub cols: u16,
    pub rows: u16,
    pub title: Option<String>,
    pub idle_time_limit: Option<f64>,
    pub events: Vec<OutEvent>,
}

// ── 解析 ──

/// 解析 asciicast 文本(v1 单 JSON / v2、v3 JSONL)。错误信息带行号, 直接面向用户。
pub fn parse_cast(text: &str) -> Result<Cast, String> {
    // v1: 整个文件是一个 JSON 对象
    if let Ok(doc) = serde_json::from_str::<Value>(text) {
        if doc.get("version").and_then(Value::as_u64) == Some(1) {
            return parse_v1(&doc);
        }
    }

    // v2/v3: 首行 header JSON, 其余每行一个 [time, code, data] 事件
    let mut lines = text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty());
    let (ln, first) = lines.next().ok_or_else(|| "空文件".to_string())?;
    let header: Value = serde_json::from_str(first)
        .map_err(|e| format!("第 {} 行: 不是合法的 header JSON: {e}", ln + 1))?;
    let version = header
        .get("version")
        .and_then(Value::as_u64)
        .ok_or("header 缺 version")?;
    let cast = match version {
        2 | 3 => parse_v23(version, &header)?,
        v => return Err(format!("不支持的 asciicast 版本: v{v} (支持 v1/v2/v3)")),
    };

    let mut events = cast.events;
    let mut prev_t = 0.0f64;
    for (ln, line) in lines {
        let line = line.trim();
        if line.starts_with('#') {
            continue; // v3 允许注释行
        }
        let ev: (f64, String, String) = serde_json::from_str(line)
            .map_err(|e| format!("第 {} 行: 不是合法事件 [时间, 类型, 数据]: {e}", ln + 1))?;
        // 时间语义: v2 绝对秒; v3 相对上一事件的增量(累加成绝对时间)。
        // 只回放输出, i/r/m/x 等忽略。
        let t = if version == 3 {
            prev_t + ev.0.max(0.0)
        } else {
            ev.0.max(prev_t) // v2 防御性单调化
        };
        prev_t = t;
        if ev.1 == "o" {
            events.push(OutEvent { t, data: ev.2.into_bytes() });
        }
    }
    Ok(Cast { events, ..cast })
}

fn parse_v1(doc: &Value) -> Result<Cast, String> {
    let cols = get_u16(doc, "width")?;
    let rows = get_u16(doc, "height")?;
    let stdout = doc
        .get("stdout")
        .and_then(Value::as_array)
        .ok_or("v1: 缺 stdout 数组")?;
    let mut events = Vec::with_capacity(stdout.len());
    let mut acc = 0.0f64;
    for (i, item) in stdout.iter().enumerate() {
        let pair: (f64, String) = serde_json::from_value(item.clone())
            .map_err(|_| format!("stdout[{i}]: 不是 [延迟, 数据] 二元组"))?;
        acc += pair.0.max(0.0); // v1 是相对延迟, 累加成绝对时间
        events.push(OutEvent { t: acc, data: pair.1.into_bytes() });
    }
    Ok(Cast {
        version: 1,
        cols,
        rows,
        title: doc.get("title").and_then(Value::as_str).map(String::from),
        idle_time_limit: None, // v1 格式无此字段
        events,
    })
}

fn parse_v23(version: u64, header: &Value) -> Result<Cast, String> {
    // v2: 顶层 width/height; v3: term.cols/term.rows
    let (cols, rows) = if version == 2 {
        (get_u16(header, "width")?, get_u16(header, "height")?)
    } else {
        let term = header.get("term").ok_or("v3: 缺 term 对象")?;
        (get_u16(term, "cols")?, get_u16(term, "rows")?)
    };
    Ok(Cast {
        version,
        cols,
        rows,
        title: header.get("title").and_then(Value::as_str).map(String::from),
        idle_time_limit: header.get("idle_time_limit").and_then(Value::as_f64),
        events: Vec::new(),
    })
}

fn get_u16(v: &Value, key: &str) -> Result<u16, String> {
    v.get(key)
        .and_then(Value::as_u64)
        .and_then(|n| u16::try_from(n).ok())
        .ok_or_else(|| format!("header 缺 {key} 或不是正整数"))
}

// ── 时间轴变换(纯函数, 可单测) ──

/// 空闲压缩: 相邻事件间隔超过 limit 的部分扣掉(算法同 asciinema)。
pub fn apply_idle_limit(events: &mut [OutEvent], limit: f64) {
    if !(limit > 0.0) {
        return;
    }
    let mut prev = 0.0f64;
    let mut offset = 0.0f64;
    for ev in events.iter_mut() {
        let delay = ev.t - prev;
        if delay > limit {
            offset += delay - limit;
        }
        prev = ev.t;
        ev.t -= offset;
    }
}

/// 倍速: 全部时间除以 speed。
pub fn apply_speed(events: &mut [OutEvent], speed: f64) {
    for ev in events.iter_mut() {
        ev.t /= speed;
    }
}

// ── 回放 ──

fn emit(out: &mut std::io::StdoutLock<'_>, data: &[u8]) -> bool {
    // 每事件即写即 flush: PTY 上按录像节奏出帧。写失败(会话被回收等)→ 停播。
    out.write_all(data).is_ok() && out.flush().is_ok()
}

/// 交互回放。返回进程退出码(正常结束与主动退出都是 0)。
fn play_interactive(events: &[OutEvent]) -> i32 {
    let _guard = TtyGuard::enter(); // 进 raw 模式(失败则无按键, 照常放完)
    let mut stdin_open = std::io::stdin().is_terminal();
    let mut out = std::io::stdout().lock();

    let mut epoch = Instant::now();
    let mut paused: Option<Instant> = None; // 暂停开始时刻
    let mut i = 0;
    while i < events.len() {
        if let Some(pause_start) = paused {
            // 暂停态: 死等按键
            match wait_key(&mut stdin_open, None) {
                Some(b' ') => {
                    epoch += pause_start.elapsed(); // 时间线平移, 恢复后连续
                    paused = None;
                }
                Some(b'.') => {
                    if !emit(&mut out, &events[i].data) {
                        return 0;
                    }
                    i += 1; // 逐帧: 画当前帧, 保持暂停
                }
                Some(b'q') | Some(0x03) => {
                    let _ = out.write_all(b"\r\n").and_then(|()| out.flush());
                    return 0;
                }
                _ => {}
            }
            continue;
        }

        // 播放态: 等到点, 等待期间响应按键
        let target = epoch + Duration::from_secs_f64(events[i].t.max(0.0));
        let mut due = false;
        while !due {
            let now = Instant::now();
            if now >= target {
                due = true;
                break;
            }
            match wait_key(&mut stdin_open, Some(target - now)) {
                Some(b' ') => {
                    paused = Some(Instant::now());
                    break;
                }
                Some(b'q') | Some(0x03) => {
                    let _ = out.write_all(b"\r\n").and_then(|()| out.flush());
                    return 0;
                }
                Some(_) => {} // 其它键忽略, 继续等
                None => {
                    if !stdin_open {
                        due = true; // stdin 没了就不再等待, 直接放完
                    }
                }
            }
        }
        if paused.is_none() && due {
            if !emit(&mut out, &events[i].data) {
                return 0;
            }
            i += 1;
        }
    }
    0
}

/// 等一个按键。`timeout = None` 死等; Some(d) 最多等 d(到点返回 None)。
/// stdin 不可用/关闭时把 `stdin_open` 置 false, 之后恒返回 None(退化为无交互)。
fn wait_key(stdin_open: &mut bool, timeout: Option<Duration>) -> Option<u8> {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::os::fd::{AsRawFd, BorrowedFd};

    if !*stdin_open {
        return None;
    }
    let fd = unsafe { BorrowedFd::borrow_raw(0) };
    let mut fds = [PollFd::new(fd, PollFlags::POLLIN)];
    // None = 死等; Some(d) 向上取整到毫秒, 避免截断成 0 导致忙轮询
    let to: PollTimeout = match timeout {
        None => PollTimeout::NONE,
        Some(d) => {
            let ms = (d.as_nanos() + 999_999) / 1_000_000;
            PollTimeout::try_from(ms).unwrap_or(PollTimeout::MAX)
        }
    };
    if poll(&mut fds, to).unwrap_or(-1) <= 0 {
        return None;
    }
    let mut buf = [0u8; 32];
    match nix::unistd::read(fd.as_raw_fd(), &mut buf) {
        Ok(0) | Err(_) => {
            *stdin_open = false; // EOF/错误: 之后不再轮询
            None
        }
        Ok(_n) => {
            // ESC 开头的多字节序列(方向键等)整段吞掉, 不当作按键
            let first = buf[0];
            if first == 0x1b {
                return None;
            }
            Some(first)
        }
    }
}

/// raw 模式守卫: 进入时把 stdin 切到 raw(按键立即可见), Drop 时无条件恢复。
struct TtyGuard {
    orig: nix::sys::termios::Termios,
}

impl TtyGuard {
    fn enter() -> Option<Self> {
        use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg};
        use std::os::fd::BorrowedFd;

        if !std::io::stdin().is_terminal() {
            return None;
        }
        let fd = unsafe { BorrowedFd::borrow_raw(0) };
        let orig = tcgetattr(&fd).ok()?;
        let mut raw = orig.clone();
        cfmakeraw(&mut raw);
        tcsetattr(&fd, SetArg::TCSANOW, &raw).ok()?;
        Some(Self { orig })
    }
}

impl Drop for TtyGuard {
    fn drop(&mut self) {
        use nix::sys::termios::{tcsetattr, SetArg};
        use std::os::fd::BorrowedFd;

        let fd = unsafe { BorrowedFd::borrow_raw(0) };
        let _ = tcsetattr(&fd, SetArg::TCSANOW, &self.orig);
    }
}

// ── 入口 ──

pub fn run(args: &[String]) -> i32 {
    let mut speed: f64 = 1.0;
    let mut idle_override: Option<f64> = None;
    let mut file: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "-h" | "--help" => {
                print!("{USAGE}");
                return 0;
            }
            "-s" | "--speed" => {
                i += 1;
                match args.get(i).and_then(|s| s.parse::<f64>().ok()) {
                    Some(v) if v > 0.0 && v.is_finite() => speed = v,
                    _ => {
                        eprintln!("play: --speed 需要一个正数");
                        return 2;
                    }
                }
            }
            "-i" | "--idle-limit" => {
                i += 1;
                match args.get(i).and_then(|s| s.parse::<f64>().ok()) {
                    Some(v) if v > 0.0 && v.is_finite() => idle_override = Some(v),
                    _ => {
                        eprintln!("play: --idle-limit 需要一个正数(秒)");
                        return 2;
                    }
                }
            }
            s if let Some(v) = s.strip_prefix("--speed=") => match v.parse::<f64>() {
                Ok(v) if v > 0.0 && v.is_finite() => speed = v,
                _ => {
                    eprintln!("play: --speed 需要一个正数");
                    return 2;
                }
            },
            s if let Some(v) = s.strip_prefix("--idle-limit=") => match v.parse::<f64>() {
                Ok(v) if v > 0.0 && v.is_finite() => idle_override = Some(v),
                _ => {
                    eprintln!("play: --idle-limit 需要一个正数(秒)");
                    return 2;
                }
            },
            s if s.starts_with('-') && s.len() > 1 => {
                eprintln!("play: 未知选项 {s} (-h 看用法)");
                return 2;
            }
            _ => {
                if file.is_some() {
                    eprintln!("play: 只支持一个录像文件");
                    return 2;
                }
                file = Some(args[i].clone());
            }
        }
        i += 1;
    }

    let Some(arg) = file else {
        return list_casts();
    };

    let Some(path) = resolve(&arg) else {
        eprintln!("play: 找不到录像: {arg} (敲 play 看列表; .cast 后缀可省)");
        return 1;
    };

    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("play: 读不了 {}: {e}", path.display());
            return 1;
        }
    };
    if bytes.starts_with(&ZSTD_MAGIC) {
        eprintln!("play: 这个录像是 zstd 压缩的, 暂不支持; 先用 asciinema convert 转成明文 .cast");
        return 1;
    }
    let text = match String::from_utf8(bytes) {
        Ok(t) => t,
        Err(_) => {
            eprintln!("play: 文件不是 UTF-8 文本, 不像 asciicast");
            return 1;
        }
    };
    let mut cast = match parse_cast(&text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("play: 解析失败 ({}): {e}", path.display());
            return 1;
        }
    };

    if let Some(limit) = idle_override.or(cast.idle_time_limit) {
        apply_idle_limit(&mut cast.events, limit);
    }
    apply_speed(&mut cast.events, speed);

    if std::io::stdout().is_terminal() {
        play_interactive(&cast.events)
    } else {
        // 管道 / 重定向: 无延时倾倒(等价 asciinema cat), 不读按键
        let mut out = std::io::stdout().lock();
        for ev in &cast.events {
            if out.write_all(&ev.data).is_err() {
                break;
            }
        }
        let _ = out.flush();
        0
    }
}

/// 文件解析顺序(与 blog 同款): 原样(相对 cwd / 绝对) → ~/blog/<arg> → ~/blog/<arg>.cast
fn resolve(arg: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let blog = home.join("blog");
    for cand in [
        PathBuf::from(arg),
        blog.join(arg),
        blog.join(format!("{arg}.cast")),
    ] {
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// 裸 play: 列出 ~/blog 下的 .cast(相对路径, 去后缀, 直接可当参数用)。
fn list_casts() -> i32 {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        eprintln!("play: HOME 未设置");
        return 1;
    };
    let blog = home.join("blog");
    let mut found: Vec<String> = vec![];
    walk_casts(&blog, &blog, &mut found);
    if found.is_empty() {
        println!("还没有录像(.cast)。把录像放进 ~/blog/<博文目录>/ 再敲 play。");
    } else {
        found.sort();
        println!("可播录像(~/blog 下):");
        for f in &found {
            println!("  {f}");
        }
        println!("播放: play <名字>  (如 play {})", found[0]);
    }
    0
}

fn walk_casts(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_casts(root, &path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("cast") {
            if let Ok(rel) = path.strip_prefix(root) {
                let s = rel.to_string_lossy();
                out.push(s.strip_suffix(".cast").unwrap_or(&s).to_string());
            }
        }
    }
}

const USAGE: &str = "\
用法: play [选项] <录像>

播放 asciicast 终端录像(v1/v2/v3)。录像随博文存放:
~/blog/<博文目录>/<名字>.cast, 如 ~/blog/hello/demo.cast → play hello/demo

查找顺序: 原样路径(相对 cwd / 绝对) → ~/blog/<录像> → ~/blog/<录像>.cast
不带参数时列出 ~/blog 下全部可播录像。

选项:
  -s, --speed N        倍速播放 (默认 1.0)
  -i, --idle-limit N   空闲上限秒数, 长停顿压缩 (默认用录像头部值)
  -h, --help           本帮助

键位: 空格 暂停/继续    . 逐帧(暂停时)    q / Ctrl-C 退出
";

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(t: f64, s: &str) -> OutEvent {
        OutEvent { t, data: s.as_bytes().to_vec() }
    }

    #[test]
    fn parse_v2_minimal() {
        let text = r#"{"version": 2, "width": 100, "height": 50}
[1.23, "o", "hello"]
[2.0, "i", "x"]
[3.0, "o", "world"]
"#;
        let cast = parse_cast(text).unwrap();
        assert_eq!(cast.version, 2);
        assert_eq!((cast.cols, cast.rows), (100, 50));
        assert_eq!(cast.events.len(), 2); // "i" 被忽略
        assert_eq!(cast.events[0], ev(1.23, "hello"));
        assert_eq!(cast.events[1], ev(3.0, "world"));
    }

    #[test]
    fn parse_v2_escapes_and_meta() {
        let text = r#"{"version": 2, "width": 80, "height": 24, "idle_time_limit": 1.5, "title": "演示"}
[0.5, "o", "\u001b[32mok\u001b[0m\r\n"]
"#;
        let cast = parse_cast(text).unwrap();
        assert_eq!(cast.idle_time_limit, Some(1.5));
        assert_eq!(cast.title.as_deref(), Some("演示"));
        assert_eq!(cast.events[0].data, b"\x1b[32mok\x1b[0m\r\n");
    }

    #[test]
    fn parse_v3_header() {
        let text = r#"{"version": 3, "term": {"cols": 72, "rows": 30, "type": "xterm-256color"}}
[0.1, "o", "hi"]
[0.2, "r", "80x24"]
"#;
        let cast = parse_cast(text).unwrap();
        assert_eq!(cast.version, 3);
        assert_eq!((cast.cols, cast.rows), (72, 30));
        assert_eq!(cast.events.len(), 1); // resize 被忽略
    }

    #[test]
    fn parse_v3_relative_deltas() {
        // v3 时间是相对上一事件的增量(对齐 asciinema 官方夹具的期望值)
        let text = r#"{"version": 3, "term": {"cols": 100, "rows": 50}}
[0.000001, "o", "a"]
[1.0, "o", "b"]
[0.3, "i", "\n"]
[1.600001, "r", "80x40"]
[10.5, "o", "c"]
# 注释行应被跳过
"#;
        let cast = parse_cast(text).unwrap();
        assert_eq!(cast.version, 3);
        let ts: Vec<f64> = cast.events.iter().map(|e| e.t).collect();
        // 0.000001 → +1.0 → (+0.3 输入, 时间轴照走) → (+1.600001 缩放) → +10.5
        assert!(ts[0].abs() - 0.000001 < 1e-9);
        assert!((ts[1] - 1.000001).abs() < 1e-9);
        assert!((ts[2] - 13.400002).abs() < 1e-9);
        let datas: Vec<&[u8]> = cast.events.iter().map(|e| e.data.as_slice()).collect();
        assert_eq!(datas, vec![b"a".as_slice(), b"b".as_slice(), b"c".as_slice()]);
    }

    #[test]
    fn parse_v1_relative_delays() {
        let text = r#"{"version": 1, "width": 80, "height": 24,
"stdout": [[0.5, "a"], [1.25, "b"], [0.25, "c"]]}"#;
        let cast = parse_cast(text).unwrap();
        assert_eq!(cast.version, 1);
        let ts: Vec<f64> = cast.events.iter().map(|e| e.t).collect();
        assert_eq!(ts, vec![0.5, 1.75, 2.0]); // 相对延迟累加
        let datas: Vec<&[u8]> = cast.events.iter().map(|e| e.data.as_slice()).collect();
        assert_eq!(datas, vec![b"a".as_slice(), b"b".as_slice(), b"c".as_slice()]);
    }

    #[test]
    fn reject_bad_input() {
        assert!(parse_cast("").is_err());
        assert!(parse_cast("not json").is_err());
        assert!(parse_cast(r#"{"version": 9}"#).is_err());
        assert!(parse_cast(r#"{"version": 2}"#).is_err()); // 缺 width
        assert!(parse_cast("{\"version\": 2, \"width\": 80, \"height\": 24}\n[1, 2]").is_err()); // 事件不是三元组
    }

    #[test]
    fn idle_limit_matches_asciinema() {
        // 与 asciinema limit_idle_time 单测同一组数据
        let mut events = vec![ev(0.0, "a"), ev(1.0, "b"), ev(3.5, "c"), ev(4.0, "d"), ev(7.5, "e")];
        apply_idle_limit(&mut events, 2.0);
        let ts: Vec<f64> = events.iter().map(|e| e.t).collect();
        assert_eq!(ts, vec![0.0, 1.0, 3.0, 3.5, 5.5]);
    }

    #[test]
    fn idle_limit_zero_is_noop() {
        let mut events = vec![ev(0.0, "a"), ev(10.0, "b")];
        apply_idle_limit(&mut events, 0.0);
        assert_eq!(events[1].t, 10.0);
    }

    #[test]
    fn speed_divides_timeline() {
        let mut events = vec![ev(0.0, "a"), ev(2.0, "b"), ev(5.0, "c")];
        apply_speed(&mut events, 2.0);
        let ts: Vec<f64> = events.iter().map(|e| e.t).collect();
        assert_eq!(ts, vec![0.0, 1.0, 2.5]);
    }

    #[test]
    fn zstd_magic_const() {
        // 与 asciinema ZSTD_MAGIC 一致
        assert_eq!(ZSTD_MAGIC, [0x28, 0xb5, 0x2f, 0xfd]);
    }
}
