//! reader —— 带图文章的 TUI 阅读器(图片二期)。
//!
//! 满足**全部**进入条件时 blog 用它替换 less(见 `blog::run` 与 `try_run`):
//!
//! ```text
//! rendered 是机器索引给出的 `~/.rendered/<article-key>`
//! 且 stdin 与 stdout 都是终端
//! 且 env TERMBLOG_IMG == "iterm2"   (严格相等; 未设置/空串/其他值一律走 less)
//! 且 终端 cols ≥ 76                 (窄窗口语义见 §5.6.3)
//! 且 rendered 文件存在且非空
//! 且相邻的 `<basename>.images.json` 存在、解析成功、version == 1、images 非空
//! 且 §5.6.1 全部预检通过
//! ```
//!
//! 任一条件不满足 → 现状 less 路径一字不动(v1 占位框降级)。
//!
//! 视口模型: 1 个模型行 = 1 个预渲染行(文本行不换行); 图像块占据 R 行
//! (§5.6.3 几何)+ 1 行 dim alt。单行滚动用 CSI S/T 移动终端现有 cell
//! (xterm 图片属性随行移动), 只补新露出的一行; 翻页/resize 才全清重画。
//! IIP payload 按 (asset, 几何, 切片窗口) 缓存。
//!
//! 失败降级: 解码/裁剪/重编码失败或切片 payload > 1 MiB → 该块退回显示
//! v1 占位框文本(那段原始 ANSI 行), 其余块不受影响。

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, IsTerminal, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::cursor::MoveTo;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, ScrollDown, ScrollUp,
};
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Terminal;
use unicode_width::UnicodeWidthChar as _;

use super::iip;

/// 进入 TUI 的最低列数(< 此值移交 less, §5.6.3)
const MIN_COLS: u16 = 76;
/// 预排版宽度(与 content-build 的 ansi.rs WIDTH 一致)
const PRE_COLS: usize = 76;
/// cell 尺寸查询超时(§5.6.3)
const CELL_QUERY_TIMEOUT: Duration = Duration::from_millis(300);
/// 未查到 cell 尺寸时的默认值(Maple Mono 15px 的近似, §5.6.3)
pub const DEFAULT_CELL_H: u32 = 20;
pub const DEFAULT_CELL_W: u32 = 9;
/// 单条 IIP payload 运行时硬上限(与前端 iipSizeLimit: 1 MiB 呼应)
const MAX_IIP_PAYLOAD: usize = 1 << 20;
/// 切片/解码缓存 LRU 容量(§5.6.4)
const LRU_CAP: usize = 3;
/// 单行增量切片很小, 多留一些可让来回滚动直接命中、完全跳过编码。
const ROW_SLICE_LRU_CAP: usize = 256;
/// JPEG 切片重编码质量(§5.6.3)
const JPEG_Q: u8 = 80;
/// xterm.js 同步输出模式: 一帧的清屏、文本和异步 IIP 解码全部完成后再一起
/// 显示, 避免逐行滚动时先露出空白屏、随后图片才补上的强烈闪烁。
const SYNC_UPDATE_BEGIN: &[u8] = b"\x1b[?2026h";
const SYNC_UPDATE_END: &[u8] = b"\x1b[?2026l";

// ── manifest ──

#[derive(serde::Deserialize, Debug, Clone)]
struct ImgAnchor {
    block_start: usize,
    block_end: usize,
    #[allow(dead_code)] // 占位框降级的 URL 语义; reader 实际读 asset 字段
    path: String,
    asset: String,
    w: u32,
    h: u32,
    indent_cols: usize,
    display_cols: usize,
    alt: String,
}

#[derive(serde::Deserialize)]
struct ManifestFile {
    version: u32,
    images: Vec<ImgAnchor>,
}

// ── 预检通过的准备工作集 ──

struct Prepared {
    /// 预渲染文本(解析为 span 后; 1 行 = 1 模型行)
    lines: Vec<Line<'static>>,
    /// 原始 ANSI 行(降级渲染与退出重画用)
    raw: Vec<String>,
    images: Vec<ImgAnchor>,
    assets_dir: PathBuf,
    rendered: PathBuf,
}

/// §5.6.1 预检: 任一失败 → Err(调用方回落 less 路径, 不 panic)。
fn preflight(
    text: &str,
    images: Vec<ImgAnchor>,
    assets_dir: &Path,
    rendered: &Path,
) -> Result<Prepared, String> {
    let raw: Vec<String> = text.lines().map(str::to_string).collect();
    if raw.is_empty() {
        return Err("预渲染文本为空".into());
    }
    let n = raw.len();
    let mut prev_end = 0usize;
    for a in &images {
        // 区间合法: 0 ≤ block_start < block_end ≤ 总行数; 相邻区间递增且不重叠
        if !(a.block_start < a.block_end && a.block_end <= n) {
            return Err(format!(
                "图像区间越界: {}..{}(总行数 {n})",
                a.block_start, a.block_end
            ));
        }
        if a.block_start < prev_end {
            return Err(format!(
                "图像区间重叠/乱序: {} 与上一块末行 {prev_end} 交叉",
                a.block_start
            ));
        }
        prev_end = a.block_end;
        // 尺寸合法: w ≥ 1 且 h ≥ 1; indent_cols ≤ display_cols ≤ 76
        if a.w == 0 || a.h == 0 {
            return Err(format!("图像尺寸为零: {} ({}×{})", a.asset, a.w, a.h));
        }
        if a.indent_cols > a.display_cols || a.display_cols > PRE_COLS {
            return Err(format!(
                "缩进/内容宽非法: {} (indent {} > display {} 或 > {PRE_COLS})",
                a.asset, a.indent_cols, a.display_cols
            ));
        }
        validate_asset(&a.asset, assets_dir)?;
    }
    let lines = raw.iter().map(|l| Line::from(parse_line(l))).collect();
    Ok(Prepared {
        lines,
        raw,
        images,
        assets_dir: assets_dir.to_path_buf(),
        rendered: rendered.to_path_buf(),
    })
}

/// asset 安全校验(与 content-build img.rs 的 validate_asset_path 同规则):
/// 字符集限 [a-z0-9/._-], 逐段拒绝空段与 `.`/`..`, 文件存在且非空。
/// 拒绝 `..`/空段后规范化不可能逃逸 `.rendered-assets/` 根。
fn validate_asset(asset: &str, root: &Path) -> Result<(), String> {
    termblog_content_model::validate_resource_rel(asset).map_err(|e| e.to_string())?;
    let p = root.join(asset);
    match std::fs::metadata(&p) {
        Ok(m) if m.is_file() && m.len() > 0 => Ok(()),
        Ok(_) => Err(format!("asset 缺失或为空: {asset}")),
        Err(_) => Err(format!("asset 不存在: {asset}")),
    }
}

// ── ANSI → Span 解析(§5.6.2) ──

/// content-build 的输出只含固定 SGR 子集(1/22 粗体、3/23 斜体、2/22 dim、
/// 36/39 cyan、0 复位)与 OSC 8(`ESC]8;;<url>ST` … `ESC]8;;ST`),调色板封闭。
/// 自写最小解析器: 每行切成 Span + Style; OSC 8 序列剥除(链接语义丢弃,
/// 保留其 SGR 样式 —— 链接文本本就是 cyan; TUI 无鼠标交互, 链接点击是
/// 非目标, 镜像页仍有完整链接)。
pub fn parse_line(line: &str) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = vec![];
    let mut style = Style::default();
    let mut text = String::new();
    let mut rest = line;

    while let Some(c) = rest.chars().next() {
        if c == '\x1b' && rest.as_bytes().get(1) == Some(&b'[') {
            // CSI: 消费到 final byte(0x40..=0x7e)
            let mut end = 2;
            while let Some(&b) = rest.as_bytes().get(end) {
                if (0x40..=0x7e).contains(&b) {
                    end += 1;
                    break;
                }
                end += 1;
            }
            if !text.is_empty() {
                out.push(Span::styled(std::mem::take(&mut text), style));
            }
            apply_sgr(&rest[..end], &mut style);
            rest = &rest[end..];
        } else if c == '\x1b' && rest.as_bytes().get(1) == Some(&b']') {
            // OSC: 消费到 BEL 或 ST(ESC \) —— OSC 8 剥除, 其余 OSC 也剥
            let mut end = 2;
            let bytes = rest.as_bytes();
            while end < bytes.len() {
                if bytes[end] == 0x07 {
                    end += 1;
                    break;
                }
                if bytes[end] == 0x1b && bytes.get(end + 1) == Some(&b'\\') {
                    end += 2;
                    break;
                }
                end += 1;
            }
            if !text.is_empty() {
                out.push(Span::styled(std::mem::take(&mut text), style));
            }
            rest = &rest[end..];
        } else {
            text.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    if !text.is_empty() {
        out.push(Span::styled(text, style));
    }
    out
}

/// 解释一条 SGR 序列(如 `\x1b[1;36m`), 更新样式。未知参数忽略。
fn apply_sgr(seq: &str, style: &mut Style) {
    let params = &seq[2..seq.len().saturating_sub(1)]; // 剥 ESC[ 与 final
    if params.is_empty() {
        *style = Style::default(); // "\x1b[m" = 复位
        return;
    }
    for p in params.split(';') {
        match p {
            "0" | "" => *style = Style::default(),
            "1" => *style = style.add_modifier(Modifier::BOLD),
            "22" => *style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            "2" => *style = style.add_modifier(Modifier::DIM),
            "3" => *style = style.add_modifier(Modifier::ITALIC),
            "23" => *style = style.remove_modifier(Modifier::ITALIC),
            "36" => *style = style.fg(Color::Cyan),
            "39" => *style = style.fg(Color::Reset),
            _ => {}
        }
    }
}

// ── cell 尺寸查询(§5.6.3) ──

/// 发 `CSI 16 t`, 300ms 内解析 `CSI 6 ; h ; w t`(h = 像素行高在前,
/// w = 像素字宽在后; xterm ctlseqs 与 xterm.js typings 均如此)。
/// 超时/解析失败 → 默认行高 ≈20px、字宽 ≈9px(Maple Mono 15px 的近似)。
/// 读响应期间 stdin 需 raw mode; 查询期间把 fd 0 临时设为 O_NONBLOCK,
/// 保证超时逻辑不依赖 termios 的 VMIN 设置。
fn query_cell_size() -> (u32, u32) {
    let stdin = io::stdin();
    let fd = stdin.as_raw_fd();
    let old_flags = fcntl(fd, FcntlArg::F_GETFL).unwrap_or(0);
    let _ = fcntl(
        fd,
        FcntlArg::F_SETFL(OFlag::from_bits_truncate(old_flags) | OFlag::O_NONBLOCK),
    );
    {
        let mut out = io::stdout().lock();
        let _ = out.write_all(b"\x1b[16t");
        let _ = out.flush();
    }
    let deadline = Instant::now() + CELL_QUERY_TIMEOUT;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 128];
    let result = loop {
        let now = Instant::now();
        if now >= deadline {
            break None;
        }
        match nix::unistd::read(fd, &mut chunk) {
            Ok(0) => std::thread::sleep(Duration::from_millis(10)),
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if let Some(cell) = parse_cell_response(&buf) {
                    break Some(cell);
                }
            }
            Err(nix::errno::Errno::EAGAIN) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => break None,
        }
    };
    let _ = fcntl(fd, FcntlArg::F_SETFL(OFlag::from_bits_truncate(old_flags)));
    result.unwrap_or((DEFAULT_CELL_H, DEFAULT_CELL_W))
}

/// 在字节流里找 `CSI 6 ; h ; w t` 响应, 返回 (行高 px, 字宽 px)。
fn parse_cell_response(buf: &[u8]) -> Option<(u32, u32)> {
    let mut i = 0usize;
    while i + 1 < buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b'[' {
            let rest = &buf[i + 2..];
            let Some(end) = rest.iter().position(|&b| b == b't') else {
                return None; // 响应尚未完整, 等下一次 read
            };
            let body = std::str::from_utf8(&rest[..end]).ok()?;
            let params: Vec<&str> = body.split(';').collect();
            if params.len() == 3 && params[0] == "6" {
                let h: u32 = params[1].parse().ok()?;
                let w: u32 = params[2].parse().ok()?;
                if h > 0 && w > 0 {
                    return Some((h, w));
                }
            }
        }
        i += 1;
    }
    None
}

// ── 图像几何(§5.6.3, 纯函数便于单测) ──

/// 图像块几何: (W 列, Wpx, R 行)。
/// W = min(display_cols, cols − indent_cols).max(1) 列;
/// 显示宽 px = W × cell_w; R = ceil(Wpx × h / w / cell_h)。
fn block_geometry(a: &ImgAnchor, cols: u16, cell_w: u32, cell_h: u32) -> (usize, u32, usize) {
    let wcols = a
        .display_cols
        .min((cols as usize).saturating_sub(a.indent_cols))
        .max(1);
    let wpx = (wcols as u32).saturating_mul(cell_w).max(1);
    // R 必须从实际写进 IIP height 的整数值推导, 不能分别对有理数取整,
    // 否则边界尺寸可能多/少预留一行。
    let hpx = display_height(a, wpx);
    let r = (hpx as u64).div_ceil(cell_h as u64);
    (wcols, wpx, r.max(1) as usize)
}

fn display_height(a: &ImgAnchor, wpx: u32) -> u32 {
    (wpx as u64 * a.h as u64 / a.w as u64).max(1) as u32
}

/// 切片窗口使用**显示像素**而不是源图像素。旧实现先把终端行映射回源图
/// 再裁剪: 对 2×2 之类的小图, 多个终端行会落到同一个源像素, 一次本应只
/// 显示 20 行的切片可能重新膨胀成整张 684px 高图片并盖住后文。
/// 现在先把图片缩放到 (wpx, full_hpx), 再按 cell 行精确裁显示像素。
fn slice_window(full_hpx: u32, cell_h: u32, k: usize, vis_rows: usize) -> (u32, u32) {
    let to_display =
        |rows: usize| -> u32 { (rows as u64 * cell_h as u64).min(full_hpx as u64) as u32 };
    let y0 = to_display(k);
    let y1 = to_display(k + vis_rows);
    if y1 <= y0 {
        (y0.saturating_sub(1), y0.max(1).min(full_hpx))
    } else {
        (y0, y1)
    }
}

// ── 缓存(§5.6.4) ──

/// 迷你 LRU(不引 lru crate, 二十行的事)。
struct Lru<K: std::hash::Hash + Eq + Clone, V> {
    map: HashMap<K, V>,
    order: VecDeque<K>,
    cap: usize,
}

impl<K: std::hash::Hash + Eq + Clone, V> Lru<K, V> {
    fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap,
        }
    }
    fn get(&mut self, k: &K) -> Option<&V> {
        if self.map.contains_key(k) {
            self.order.retain(|x| x != k);
            self.order.push_back(k.clone());
            return self.map.get(k);
        }
        None
    }
    fn insert(&mut self, k: K, v: V) {
        if self.map.contains_key(&k) {
            self.order.retain(|x| x != &k);
        } else if self.order.len() >= self.cap {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
        self.order.push_back(k.clone());
        self.map.insert(k, v);
    }
}

impl<K: std::hash::Hash + Eq + Clone, V> Default for Lru<K, V> {
    fn default() -> Self {
        Self::new(LRU_CAP)
    }
}

/// IIP payload 缓存: 完整图按 (asset, 显示宽) 缓存; 整帧切片 LRU ≤ 3;
/// 增量滚动的一行切片 LRU ≤ 256, 来回滚动不重复编码。解码图与按终端
/// 尺寸缩放后的图各 LRU ≤ 3。
struct Caches {
    bytes: HashMap<String, Vec<u8>>,
    full: HashMap<(String, u32), String>,
    slices: Lru<(String, u32, u32, u32), String>,
    row_slices: Lru<(String, u32, usize), String>,
    decoded: Lru<String, image::DynamicImage>,
    scaled: Lru<(String, u32), image::DynamicImage>,
    /// 该 asset 已判定失败(降级为占位框), 不再重试
    failed: HashSet<String>,
}

impl Default for Caches {
    fn default() -> Self {
        Self {
            bytes: HashMap::new(),
            full: HashMap::new(),
            slices: Lru::new(LRU_CAP),
            row_slices: Lru::new(ROW_SLICE_LRU_CAP),
            decoded: Lru::new(LRU_CAP),
            scaled: Lru::new(LRU_CAP),
            failed: HashSet::new(),
        }
    }
}

fn load_bytes(caches: &mut Caches, assets_dir: &Path, asset: &str) -> Result<Vec<u8>, String> {
    if let Some(b) = caches.bytes.get(asset) {
        return Ok(b.clone());
    }
    if caches.failed.contains(asset) {
        return Err("asset 已判定降级".into());
    }
    let b = std::fs::read(assets_dir.join(asset)).map_err(|e| format!("读 asset {asset}: {e}"))?;
    caches.bytes.insert(asset.to_string(), b.clone());
    Ok(b)
}

/// 完整可见图 payload: 直接携带处理后字节, 不解码不重编码(§5.6.4)。
fn full_payload(
    caches: &mut Caches,
    assets_dir: &Path,
    a: &ImgAnchor,
    wpx: u32,
) -> Result<String, String> {
    if caches.failed.contains(&a.asset) {
        return Err("asset 已判定降级".into());
    }
    if let Some(p) = caches.full.get(&(a.asset.clone(), wpx)) {
        return Ok(p.clone());
    }
    let bytes = load_bytes(caches, assets_dir, &a.asset)?;
    let hpx = display_height(a, wpx);
    let s = iip::encode(&a.asset, &bytes, wpx, hpx);
    if s.len() > MAX_IIP_PAYLOAD {
        return Err(format!(
            "完整图 payload {} KiB 超 1 MiB 上限",
            s.len() / 1024
        ));
    }
    caches.full.insert((a.asset.clone(), wpx), s.clone());
    Ok(s)
}

/// 取得按当前终端宽度缩放后的图片。解码与整图缩放各自只做一次。
fn scaled_image(
    caches: &mut Caches,
    assets_dir: &Path,
    a: &ImgAnchor,
    wpx: u32,
) -> Result<image::DynamicImage, String> {
    let scaled_key = (a.asset.clone(), wpx);
    let scaled = match caches.scaled.get(&scaled_key) {
        Some(i) => i.clone(),
        None => {
            let img = match caches.decoded.get(&a.asset) {
                Some(i) => i.clone(),
                None => {
                    let bytes = load_bytes(caches, assets_dir, &a.asset)?;
                    let img = image::load_from_memory(&bytes)
                        .map_err(|e| format!("解码 {}: {e}", a.asset))?;
                    caches.decoded.insert(a.asset.clone(), img.clone());
                    img
                }
            };
            let scaled = img.resize_exact(
                wpx,
                display_height(a, wpx),
                image::imageops::FilterType::Triangle,
            );
            caches.scaled.insert(scaled_key, scaled.clone());
            scaled
        }
    };
    Ok(scaled)
}

/// 切片 payload: 解码 asset → 缩放到终端显示尺寸 → 按显示像素裁剪 → 按源
/// 格式重编码(png 源→png; gif 源→首帧 png; jpeg 源→jpeg q80)。
/// 失败/超 1 MiB → Err(降级)。
fn slice_payload(
    caches: &mut Caches,
    assets_dir: &Path,
    a: &ImgAnchor,
    wpx: u32,
    y0: u32,
    y1: u32,
) -> Result<String, String> {
    let key = (a.asset.clone(), wpx, y0, y1);
    if let Some(p) = caches.slices.get(&key) {
        return Ok(p.clone());
    }
    let scaled = scaled_image(caches, assets_dir, a, wpx)?;
    let sub = scaled.crop_imm(0, y0, scaled.width(), y1 - y0);
    let encoded = reencode_slice(&sub, &a.asset)?;
    let hpx = y1 - y0;
    let s = iip::encode(&a.asset, &encoded, wpx, hpx);
    if s.len() > MAX_IIP_PAYLOAD {
        return Err(format!("切片 payload {} KiB 超 1 MiB 上限", s.len() / 1024));
    }
    caches.slices.insert(key, s.clone());
    Ok(s)
}

/// 单行增量 payload。图像内容仍按显示像素精确裁剪, 但 IIP 目标尺寸使用
/// `W列 × 1行`, 保证浏览器端严格覆盖一个 cell 行。key 使用图片内模型行
/// offset, 反向滚动时可直接复用已编码字节。
fn row_payload(
    caches: &mut Caches,
    assets_dir: &Path,
    a: &ImgAnchor,
    wcols: usize,
    wpx: u32,
    row_offset: usize,
    cell_h: u32,
) -> Result<String, String> {
    let key = (a.asset.clone(), wpx, row_offset);
    if let Some(p) = caches.row_slices.get(&key) {
        return Ok(p.clone());
    }
    let (y0, y1) = slice_window(display_height(a, wpx), cell_h, row_offset, 1);
    let scaled = scaled_image(caches, assets_dir, a, wpx)?;
    let sub = scaled.crop_imm(0, y0, scaled.width(), y1 - y0);
    let encoded = reencode_slice(&sub, &a.asset)?;
    let s = iip::encode_row(&a.asset, &encoded, wcols as u32, y1 - y0, cell_h);
    if s.len() > MAX_IIP_PAYLOAD {
        return Err(format!(
            "单行切片 payload {} KiB 超 1 MiB 上限",
            s.len() / 1024
        ));
    }
    caches.row_slices.insert(key, s.clone());
    Ok(s)
}

/// 按源格式重编码切片: jpg/jpeg → JPEG q80; png/gif(首帧)→ PNG。
fn reencode_slice(img: &image::DynamicImage, asset: &str) -> Result<Vec<u8>, String> {
    use image::ImageEncoder as _;
    let ext = asset.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let mut buf: Vec<u8> = vec![];
    match ext.as_str() {
        "jpg" | "jpeg" => {
            let rgb = img.to_rgb8();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, JPEG_Q)
                .write_image(
                    rgb.as_raw(),
                    rgb.width(),
                    rgb.height(),
                    image::ExtendedColorType::Rgb8,
                )
                .map_err(|e| format!("jpeg 重编码: {e}"))?;
        }
        _ => {
            let rgba = img.to_rgba8();
            image::codecs::png::PngEncoder::new(&mut buf)
                .write_image(
                    rgba.as_raw(),
                    rgba.width(),
                    rgba.height(),
                    image::ExtendedColorType::Rgba8,
                )
                .map_err(|e| format!("png 重编码: {e}"))?;
        }
    }
    Ok(buf)
}

// ── 视口模型(§5.6.2/5.6.3) ──

#[derive(Debug, Clone, Copy, PartialEq)]
enum ItemKind {
    /// 预渲染文本行(line = raw/lines 下标)
    Text { line: usize },
    /// 图像块的一行(img = images 下标; 块共 R 行)
    Image { img: usize },
    /// 图像块下方的 alt 行(img = images 下标)
    Alt { img: usize },
}

struct Model {
    items: Vec<ItemKind>,
    /// 每张图的第一个模型行下标
    block_row: Vec<usize>,
    /// 每张图的图像行数 R(降级块为 0)
    r_rows: Vec<usize>,
}

/// 按当前几何(cols/cell)把文本与图像块拼成视口模型。
/// 完整图 payload 在此就绪(读失败/超限 → 该块降级为占位框原文行,
/// 行数 = 区间长); 切片 payload 在绘制时按需构建。
fn build_model(prep: &Prepared, cols: u16, cell_w: u32, cell_h: u32, caches: &mut Caches) -> Model {
    let mut items: Vec<ItemKind> = vec![];
    let mut block_row = vec![];
    let mut r_rows = vec![];
    let mut cursor = 0usize;
    for (i, a) in prep.images.iter().enumerate() {
        for line in cursor..a.block_start {
            items.push(ItemKind::Text { line });
        }
        let (_, wpx, r) = block_geometry(a, cols, cell_w, cell_h);
        let ok = full_payload(caches, &prep.assets_dir, a, wpx).is_ok();
        if ok {
            block_row.push(items.len());
            r_rows.push(r);
            for _ in 0..r {
                items.push(ItemKind::Image { img: i });
            }
            items.push(ItemKind::Alt { img: i });
        } else {
            // 降级: 该块退回 v1 占位框原文(行数 = 区间长)
            block_row.push(items.len());
            r_rows.push(0);
            for line in a.block_start..a.block_end {
                items.push(ItemKind::Text { line });
            }
        }
        cursor = a.block_end;
    }
    for line in cursor..prep.raw.len() {
        items.push(ItemKind::Text { line });
    }
    Model {
        items,
        block_row,
        r_rows,
    }
}

/// 视口首模型行对应的 rendered 行号(图像块 → 其 block_start)。
fn anchor_line(model: &Model, prep: &Prepared, scroll: usize) -> usize {
    match model.items.get(scroll) {
        Some(ItemKind::Text { line }) => *line,
        Some(ItemKind::Image { img }) | Some(ItemKind::Alt { img }) => {
            prep.images.get(*img).map(|a| a.block_start).unwrap_or(0)
        }
        None => 0,
    }
}

/// 第一个 rendered 行号 ≥ line 的模型行下标(滚动位置锚定用)。
fn scroll_to_line(model: &Model, prep: &Prepared, line: usize) -> usize {
    model
        .items
        .iter()
        .position(|k| match k {
            ItemKind::Text { line: l } => *l >= line,
            ItemKind::Image { img } | ItemKind::Alt { img } => prep
                .images
                .get(*img)
                .map(|a| a.block_start >= line)
                .unwrap_or(true),
        })
        .unwrap_or(model.items.len())
}

// ── 一帧的放置与绘制 ──

/// 一个图像块在本帧的放置结果。
enum Placement {
    /// 不可见(整体在视口之外), 不发字节
    Hidden,
    /// 发 IIP: (payload, 屏幕 x, 屏幕顶行)
    Image { payload: String, x: u16, top: i64 },
    /// 该块降级: 画占位框原文(原始 ANSI 行)
    Degraded,
}

/// 计算放置并取(或构建)payload。top = 图像块首模型行 − scroll。
/// 失败 → Degraded(该块退回 v1 占位框文本, 其余块不受影响)。
#[allow(clippy::too_many_arguments)]
fn place_image(
    prep: &Prepared,
    img: usize,
    top: i64,
    rows: u16,
    cols: u16,
    cell_w: u32,
    cell_h: u32,
    caches: &mut Caches,
) -> Placement {
    let a = &prep.images[img];
    if caches.failed.contains(&a.asset) {
        return Placement::Degraded;
    }
    let (_, wpx, r) = block_geometry(a, cols, cell_w, cell_h);
    let r_i = r as i64;
    if top + r_i <= 0 || top >= rows as i64 {
        return Placement::Hidden; // 整体滚出视口
    }
    // k = 图片顶部滚出视口的模型行数; 视口内可见 vis_rows 行
    let k = (-top).max(0) as usize;
    let vis_rows = (r - k)
        .min((rows as usize).saturating_sub(top.max(0) as usize))
        .max(1);
    // 完整可见(顶与底都在视口内)→ 完整 payload; 否则切片
    let payload = if top >= 0 && top + r_i <= rows as i64 {
        full_payload(caches, &prep.assets_dir, a, wpx)
    } else {
        let (y0, y1) = slice_window(display_height(a, wpx), cell_h, k, vis_rows);
        slice_payload(caches, &prep.assets_dir, a, wpx, y0, y1)
    };
    match payload {
        Ok(p) => Placement::Image {
            payload: p,
            x: a.indent_cols as u16,
            top,
        },
        Err(e) => {
            eprintln!("blog: 图像块 {} 降级: {e}", a.asset);
            caches.failed.insert(a.asset.clone());
            Placement::Degraded
        }
    }
}

// ── 终端守护(§5.6.1 RAII) ──

/// TermGuard: 构造时进入 raw mode + alternate screen + 藏光标;
/// Drop(正常错误返回与 panic 均触发)依次 disable raw mode → leave
/// alternate screen → show cursor。
struct TermGuard {
    active: bool,
}

impl TermGuard {
    fn enter() -> io::Result<TermGuard> {
        let mut stdout = io::stdout();
        terminal::enable_raw_mode()?;
        execute!(stdout, EnterAlternateScreen, crossterm::cursor::Hide)?;
        Ok(TermGuard { active: true })
    }

    fn restore(&mut self) {
        if self.active {
            self.active = false;
            let mut stdout = io::stdout();
            let _ = terminal::disable_raw_mode();
            let _ = execute!(stdout, LeaveAlternateScreen, crossterm::cursor::Show);
        }
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

// ── 键位(§5.6.5) ──

enum Action {
    Quit,
    Down(usize), // 0 = 一屏
    Up(usize),   // 0 = 一屏
    Top,
    Bottom,
    None,
}

fn key_action(code: KeyCode, modifiers: KeyModifiers) -> Action {
    if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
        return Action::Quit;
    }
    match code {
        KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
        KeyCode::Char('j') | KeyCode::Down => Action::Down(1),
        KeyCode::Char('k') | KeyCode::Up => Action::Up(1),
        KeyCode::Char(' ') | KeyCode::PageDown => Action::Down(0),
        KeyCode::Char('b') | KeyCode::PageUp => Action::Up(0),
        KeyCode::Char('g') => Action::Top,
        KeyCode::Char('G') => Action::Bottom,
        _ => Action::None,
    }
}

// ── 入口 ──

/// 满足全部进入条件且 §5.6.1 预检通过 → 进入 TUI 阅读器, 返回退出码。
/// 任一条件不满足 → None(调用方走 less 现状路径, 一字不动)。
fn sidecar_path(rendered: &Path) -> Option<PathBuf> {
    let mut name = rendered.file_name()?.to_os_string();
    name.push(".images.json");
    Some(rendered.with_file_name(name))
}

pub fn try_run(home: &Path, rendered: &Path) -> Option<i32> {
    // TERMBLOG_IMG 严格相等: 未设置/空串/其他值一律走 less
    if std::env::var("TERMBLOG_IMG").as_deref() != Ok("iterm2") {
        return None;
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return None;
    }
    let (cols, _rows) = crossterm::terminal::size().ok()?;
    if cols < MIN_COLS {
        return None;
    }
    let text = std::fs::read_to_string(rendered).ok()?;
    if text.is_empty() {
        return None;
    }
    let manifest_path = sidecar_path(rendered)?;
    let manifest: ManifestFile =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).ok()?).ok()?;
    if manifest.version != 1 || manifest.images.is_empty() {
        return None;
    }
    let assets_dir = home.join(".rendered-assets");
    let prep = match preflight(&text, manifest.images, &assets_dir, rendered) {
        Ok(p) => p,
        Err(_) => return None, // 预检失败 → less 路径
    };

    // 进入终端之后的任何错误由 TermGuard RAII 兜底恢复(不 panic 到裸终端)。
    // panic hook 兜底: workspace 若切 panic=abort, 恢复三步在 abort 前执行。
    let guard = match TermGuard::enter() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("blog: 进入 TUI 失败: {e}");
            return Some(1);
        }
    };
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
        default_hook(info);
    }));
    let result = run_tui(&prep);
    drop(guard); // 恢复(幂等; 与 hook 重复执行无副作用)
    let _ = std::panic::take_hook(); // 卸 hook

    match result {
        Ok(Exit::Quit { view }) => {
            // 主屏重画最后视口(§5.6.6): 清屏后打印退出前可视行对应的
            // 原始 ANSI 行(含占位框文本与 OSC 8)—— 与 less -X 的
            // "退出后留下退出时视口"一致。原计划写的是"cat 全文", 但 cat 后
            // 屏幕最终只留下文章尾部、并非退出时视口 —— 语义不符, 废弃。
            let mut out = io::stdout().lock();
            let _ = write!(out, "\x1b[2J\x1b[H");
            for l in &view {
                let _ = writeln!(out, "{l}");
            }
            let _ = out.flush();
            Some(0)
        }
        Ok(Exit::Handoff { line }) => {
            // 窄窗口移交(§5.6.3): less -RXc +<N> 继续阅读, 阅读位置不丢
            // (N = 视口首模型行对应的 rendered 行号 + 1)
            let _ = std::process::Command::new("less")
                .args(["-RXc", &format!("+{line}")])
                .arg(&prep.rendered)
                .status();
            Some(0)
        }
        Err(e) => {
            eprintln!("blog: TUI 阅读器错误: {e}");
            Some(1)
        }
    }
}

enum Exit {
    Quit { view: Vec<String> },
    Handoff { line: usize },
}

fn run_tui(prep: &Prepared) -> Result<Exit, String> {
    let (cell_h, cell_w) = query_cell_size();
    let stdout = io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|e| format!("初始化终端: {e}"))?;
    terminal.hide_cursor().map_err(|e| format!("藏光标: {e}"))?;

    let (mut cols_now, mut rows_now) =
        crossterm::terminal::size().map_err(|e| format!("读终端尺寸: {e}"))?;
    rows_now = rows_now.max(1);
    if cols_now < MIN_COLS {
        return Ok(Exit::Handoff { line: 1 });
    }
    let mut caches = Caches::default();
    let mut model = build_model(prep, cols_now, cell_w, cell_h, &mut caches);
    let mut scroll: usize = 0;

    draw_frame(
        &mut terminal,
        prep,
        &model,
        scroll,
        cols_now,
        rows_now,
        cell_w,
        cell_h,
        &mut caches,
    )?;

    loop {
        match crossterm::event::read().map_err(|e| format!("读事件: {e}"))? {
            Event::Key(k) if k.kind != KeyEventKind::Release => {
                let max_scroll = model.items.len().saturating_sub(rows_now as usize);
                let new_scroll = match key_action(k.code, k.modifiers) {
                    Action::Quit => {
                        return Ok(Exit::Quit {
                            view: exit_view(prep, &model, scroll, rows_now),
                        })
                    }
                    Action::Down(0) => (scroll + rows_now as usize).min(max_scroll),
                    Action::Down(n) => (scroll + n).min(max_scroll),
                    Action::Up(0) => scroll.saturating_sub(rows_now as usize),
                    Action::Up(n) => scroll.saturating_sub(n),
                    Action::Top => 0,
                    Action::Bottom => max_scroll,
                    Action::None => scroll,
                };
                if new_scroll == scroll {
                    continue;
                }

                if incremental_target(scroll, new_scroll, rows_now).is_some() {
                    match draw_incremental_scroll(
                        &mut terminal,
                        prep,
                        &model,
                        scroll,
                        new_scroll,
                        cols_now,
                        rows_now,
                        cell_w,
                        cell_h,
                        &mut caches,
                    ) {
                        Ok(()) => scroll = new_scroll,
                        Err(IncrementalError::Image { asset, reason }) => {
                            // 预编码发生在物理滚屏前, 可安全把该块降级并用
                            // rendered 行锚重建整帧, 不留下半滚动状态。
                            eprintln!("blog: 图像块 {asset} 增量切片降级: {reason}");
                            caches.failed.insert(asset);
                            let anchor = anchor_line(&model, prep, new_scroll);
                            model = build_model(prep, cols_now, cell_w, cell_h, &mut caches);
                            scroll = scroll_to_line(&model, prep, anchor)
                                .min(model.items.len().saturating_sub(rows_now as usize));
                            draw_frame(
                                &mut terminal,
                                prep,
                                &model,
                                scroll,
                                cols_now,
                                rows_now,
                                cell_w,
                                cell_h,
                                &mut caches,
                            )?;
                        }
                        Err(IncrementalError::Terminal(e)) => return Err(e),
                    }
                } else {
                    draw_frame(
                        &mut terminal,
                        prep,
                        &model,
                        new_scroll,
                        cols_now,
                        rows_now,
                        cell_w,
                        cell_h,
                        &mut caches,
                    )?;
                    scroll = new_scroll;
                }
            }
            Event::Resize(w, h) => {
                // <76 列立即移交 less; ≥76 时宽度变化重建几何, 高度变化只
                // 调整视口。两者都只在 resize 事件上做一次确定性整帧刷新。
                if w < MIN_COLS {
                    let line = anchor_line(&model, prep, scroll);
                    return Ok(Exit::Handoff { line: line + 1 });
                }
                let anchor = anchor_line(&model, prep, scroll);
                if w != cols_now {
                    model = build_model(prep, w, cell_w, cell_h, &mut caches);
                    scroll = scroll_to_line(&model, prep, anchor);
                }
                cols_now = w;
                rows_now = h.max(1);
                scroll = scroll.min(model.items.len().saturating_sub(rows_now as usize));
                draw_frame(
                    &mut terminal,
                    prep,
                    &model,
                    scroll,
                    cols_now,
                    rows_now,
                    cell_w,
                    cell_h,
                    &mut caches,
                )?;
            }
            _ => {}
        }
    }
}

/// 退出前可视模型行对应的原始 ANSI 行(图像块 → 完整占位框区间)。
fn exit_view(prep: &Prepared, model: &Model, scroll: usize, rows: u16) -> Vec<String> {
    let mut view: Vec<String> = vec![];
    let end = (scroll + rows as usize).min(model.items.len());
    let mut i = scroll;
    while i < end {
        match &model.items[i] {
            ItemKind::Text { line } => {
                view.push(prep.raw[*line].clone());
                i += 1;
            }
            ItemKind::Image { img } => {
                let a = &prep.images[*img];
                view.extend(prep.raw[a.block_start..a.block_end].iter().cloned());
                i += model.r_rows[*img].max(1); // 跳过该块全部图像行
            }
            ItemKind::Alt { .. } => i += 1,
        }
    }
    view
}

/// 把一个模型行写入 ratatui buffer。整帧和增量路径共用, 避免样式分叉。
fn fill_model_row(
    buf: &mut Buffer,
    prep: &Prepared,
    kind: &ItemKind,
    y: u16,
    cols: u16,
    cell_w: u32,
    cell_h: u32,
) {
    match kind {
        ItemKind::Text { line } => {
            if let Some(l) = prep.lines.get(*line) {
                let mut x = 0u16;
                for s in &l.spans {
                    if x >= cols {
                        break;
                    }
                    buf.set_span(x, y, s, cols - x);
                    x = x.saturating_add(s.width() as u16);
                }
            }
        }
        ItemKind::Alt { img } => {
            let a = &prep.images[*img];
            let (wcols, _, _) = block_geometry(a, cols, cell_w, cell_h);
            let t = truncate_columns(&a.alt, wcols);
            let span = Span::styled(t, Style::default().add_modifier(Modifier::DIM));
            buf.set_span(
                a.indent_cols as u16,
                y,
                &span,
                cols.saturating_sub(a.indent_cols as u16),
            );
        }
        ItemKind::Image { .. } => {}
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhysicalScroll {
    Up,
    Down,
}

/// 仅相差一行时可走终端原生增量滚动。返回(物理方向, 新露出的模型行,
/// 新露出的屏幕 y)。文章向下读一行时, 屏幕内容物理上移一行。
fn incremental_target(
    old_scroll: usize,
    new_scroll: usize,
    rows: u16,
) -> Option<(PhysicalScroll, usize, u16)> {
    if rows == 0 {
        return None;
    }
    if new_scroll.checked_sub(old_scroll) == Some(1) {
        return Some((
            PhysicalScroll::Up,
            new_scroll.saturating_add(rows as usize - 1),
            rows - 1,
        ));
    }
    if old_scroll.checked_sub(new_scroll) == Some(1) {
        return Some((PhysicalScroll::Down, new_scroll, 0));
    }
    None
}

enum IncrementalError {
    /// 图片行预编码失败; 此时还未动屏幕, 调用方可标记降级后整帧重建。
    Image {
        asset: String,
        reason: String,
    },
    Terminal(String),
}

enum ExposedRow {
    Cells(Buffer),
    Image { payload: String, x: u16 },
}

/// 单行滚动快路径: 先准备新露出的一行, 然后在同步输出事务内用 CSI S/T
/// 移动已有终端行(图片 cell 属性会随行移动), 最后只补这一行。相比整帧
/// 清屏, 不再重发视口内整张图。
#[allow(clippy::too_many_arguments)]
fn draw_incremental_scroll(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    prep: &Prepared,
    model: &Model,
    old_scroll: usize,
    new_scroll: usize,
    cols: u16,
    rows: u16,
    cell_w: u32,
    cell_h: u32,
    caches: &mut Caches,
) -> Result<(), IncrementalError> {
    let (direction, model_row, screen_y) = incremental_target(old_scroll, new_scroll, rows)
        .ok_or_else(|| IncrementalError::Terminal("增量滚动步长不是一行".into()))?;
    let kind = model
        .items
        .get(model_row)
        .ok_or_else(|| IncrementalError::Terminal(format!("增量滚动模型行越界: {model_row}")))?;

    // 图片编码可能失败, 必须在物理滚屏前完成, 才能安全回退整帧降级。
    let exposed = match kind {
        ItemKind::Image { img } => {
            let a = &prep.images[*img];
            let row_offset = model_row.saturating_sub(model.block_row[*img]);
            let (wcols, wpx, _) = block_geometry(a, cols, cell_w, cell_h);
            let payload = row_payload(caches, &prep.assets_dir, a, wcols, wpx, row_offset, cell_h)
                .map_err(|reason| IncrementalError::Image {
                    asset: a.asset.clone(),
                    reason,
                })?;
            ExposedRow::Image {
                payload,
                x: a.indent_cols as u16,
            }
        }
        _ => {
            let mut buf = Buffer::empty(Rect::new(0, screen_y, cols, 1));
            fill_model_row(&mut buf, prep, kind, screen_y, cols, cell_w, cell_h);
            ExposedRow::Cells(buf)
        }
    };

    let mut sync = SyncUpdate::begin()
        .map_err(|e| IncrementalError::Terminal(format!("开启同步滚动: {e}")))?;
    {
        let backend = terminal.backend_mut();
        match direction {
            PhysicalScroll::Up => execute!(backend, ScrollUp(1)),
            PhysicalScroll::Down => execute!(backend, ScrollDown(1)),
        }
        .map_err(|e| IncrementalError::Terminal(format!("滚动终端行: {e}")))?;

        match &exposed {
            ExposedRow::Cells(buf) => {
                // 必须经 BufferDiff 输出: 它会跳过 CJK/emoji 的续占 cell。
                // 直接枚举全部 cell 会在宽字符后多打印一个空格并造成错位。
                let blank = Buffer::empty(buf.area);
                backend
                    .draw(blank.diff_iter(buf))
                    .map_err(|e| IncrementalError::Terminal(format!("绘制增量文本行: {e}")))?;
            }
            ExposedRow::Image { payload, x } => {
                execute!(backend, MoveTo(*x, screen_y))
                    .map_err(|e| IncrementalError::Terminal(format!("移动增量图片光标: {e}")))?;
                backend
                    .write_all(payload.as_bytes())
                    .map_err(|e| IncrementalError::Terminal(format!("写增量图片行: {e}")))?;
            }
        }
        Backend::flush(backend)
            .map_err(|e| IncrementalError::Terminal(format!("提交增量行: {e}")))?;
    }
    sync.finish()
        .map_err(|e| IncrementalError::Terminal(format!("提交同步滚动: {e}")))?;
    Ok(())
}

/// 确定性整帧路径: 全清 → ratatui 重绘文本(图像行留空)→ 直接写 IIP。
/// 初始帧、翻页/首尾跳转与 resize 使用它; 单行滚动走增量路径。
#[allow(clippy::too_many_arguments)]
fn draw_frame(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    prep: &Prepared,
    model: &Model,
    scroll: usize,
    cols: u16,
    rows: u16,
    cell_w: u32,
    cell_h: u32,
    caches: &mut Caches,
) -> Result<(), String> {
    // xterm.js DEC mode 2026: 保留上一帧, 直到本帧的清屏、文本和图片异步
    // 解码全部处理完才原子显示。SyncUpdate 的 Drop 保证中途报错也会关闭。
    let mut sync = SyncUpdate::begin().map_err(|e| format!("开启同步绘制: {e}"))?;
    // 全清(less -c 风格)。刻意不用 ratatui 的 Terminal::clear(): 它为了
    // 保留后端光标位置会发 CSI 6n DSR 查询并等终端应答 —— 每帧一次往返
    // (延迟 + 依赖终端应答, 裸 pty/个别终端会超时失败)。这里自己发
    // Clear(All), 再把 ratatui 的前帧缓冲清空(swap 两次), 下一次 flush
    // 对空缓冲做 diff = 文本全量重绘(物理清屏后必须整帧重画, 否则
    // 位置未变的文本会被清掉却不再重画)。
    terminal.swap_buffers();
    terminal.swap_buffers();
    let mut clr = io::stdout().lock();
    execute!(clr, Clear(ClearType::All)).map_err(|e| format!("清屏: {e}"))?;
    drop(clr);

    terminal
        .draw(|f| {
            let buf = f.buffer_mut();
            let end = (scroll + rows as usize).min(model.items.len());
            for (i, kind) in model.items[scroll..end].iter().enumerate() {
                let y = i as u16;
                fill_model_row(buf, prep, kind, y, cols, cell_w, cell_h);
            }
        })
        .map_err(|e| format!("绘制文本: {e}"))?;

    // IIP: 文本之上放图像。降级块(切片/解码失败)退回占位框原文行。
    let mut out = io::stdout().lock();
    for img in 0..prep.images.len() {
        let top = model.block_row[img] as i64 - scroll as i64;
        let block_h = (model.r_rows[img] + 1) as i64;
        if top + block_h <= 0 || top >= rows as i64 {
            continue; // 整块不可见
        }
        match place_image(prep, img, top, rows, cols, cell_w, cell_h, caches) {
            Placement::Image { payload, x, top } => {
                // 切片帧: 图片顶滚出视口(top < 0)时从屏幕行 0 起画切片
                let y = top.max(0);
                if (y as u16) < rows {
                    execute!(out, MoveTo(x, y as u16)).map_err(|e| format!("移光标: {e}"))?;
                    out.write_all(payload.as_bytes())
                        .map_err(|e| format!("写 IIP: {e}"))?;
                }
            }
            Placement::Degraded => {
                // 该块退回 v1 占位框文本(那段原始 ANSI 行), 其余块不受影响
                let a = &prep.images[img];
                let n = a.block_end - a.block_start;
                let cap = (block_h as usize)
                    .min(n)
                    .min((rows as usize).saturating_sub(top.max(0) as usize));
                for j in 0..cap {
                    let y = (top + j as i64) as u16;
                    execute!(out, MoveTo(0, y)).map_err(|e| format!("移光标: {e}"))?;
                    out.write_all(prep.raw[a.block_start + j].as_bytes())
                        .map_err(|e| format!("写降级行: {e}"))?;
                }
            }
            Placement::Hidden => {}
        }
    }
    out.flush().map_err(|e| format!("flush: {e}"))?;
    drop(out);
    sync.finish().map_err(|e| format!("提交同步绘制: {e}"))?;
    Ok(())
}

/// 同步输出模式守卫。正常帧通过 finish 提交; 任意早退/错误由 Drop 发送结束
/// 序列, 避免终端停留在抑制渲染状态。
struct SyncUpdate {
    active: bool,
}

impl SyncUpdate {
    fn begin() -> io::Result<Self> {
        let mut out = io::stdout().lock();
        out.write_all(SYNC_UPDATE_BEGIN)?;
        out.flush()?;
        Ok(Self { active: true })
    }

    fn finish(&mut self) -> io::Result<()> {
        if self.active {
            self.active = false;
            let mut out = io::stdout().lock();
            out.write_all(SYNC_UPDATE_END)?;
            out.flush()?;
        }
        Ok(())
    }
}

impl Drop for SyncUpdate {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

/// alt 行按显示列截断(超出 W 截断, 不折行, §5.6.3)。
fn truncate_columns(s: &str, cols: usize) -> String {
    let mut out = String::new();
    let mut w = 0usize;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > cols {
            break;
        }
        out.push(c);
        w += cw;
    }
    out
}

// ── 测试钩子: blog --dump-image-frame <article-key> <rows> <cols> <row>(§5.6.6) ──

/// 渲染单帧到 stdout 后退出(不进入交互, 不碰 raw mode/alternate screen),
/// 供 Node e2e 断言 IIP 字节。cell 尺寸用默认值(20×9, 与常量一致)。
pub fn dump_frame(home: &Path, key: &str, rows: u16, cols: u16, row: usize) -> Result<(), String> {
    let rendered = home.join(".rendered").join(key);
    let text = std::fs::read_to_string(&rendered).map_err(|e| format!("读 {key}: {e}"))?;
    if text.is_empty() {
        return Err(format!("{key}: 预渲染文本为空"));
    }
    let manifest_path = sidecar_path(&rendered).ok_or("预渲染路径没有文件名")?;
    let manifest: ManifestFile = serde_json::from_str(
        &std::fs::read_to_string(manifest_path).map_err(|e| format!("读 manifest: {e}"))?,
    )
    .map_err(|e| format!("manifest JSON: {e}"))?;
    if manifest.version != 1 || manifest.images.is_empty() {
        return Err(format!("{key}: manifest 无图或版本不符"));
    }
    let assets_dir = home.join(".rendered-assets");
    let prep = preflight(&text, manifest.images, &assets_dir, &rendered)?;

    let mut caches = Caches::default();
    let model = build_model(&prep, cols, DEFAULT_CELL_W, DEFAULT_CELL_H, &mut caches);
    let scroll = row.min(model.items.len().saturating_sub(rows as usize));

    let mut out = io::stdout().lock();
    out.write_all(SYNC_UPDATE_BEGIN)
        .map_err(|e| e.to_string())?;
    out.write_all(b"\x1b[2J\x1b[H").map_err(|e| e.to_string())?;
    // 文本行(原文 ANSI; 图像块行不发文本, 只发 IIP)
    let end = (scroll + rows as usize).min(model.items.len());
    for kind in &model.items[scroll..end] {
        match kind {
            ItemKind::Text { line } => {
                out.write_all(prep.raw[*line].as_bytes())
                    .map_err(|e| e.to_string())?;
                out.write_all(b"\n").map_err(|e| e.to_string())?;
            }
            ItemKind::Alt { img } => {
                let a = &prep.images[*img];
                let (wcols, _, _) = block_geometry(a, cols, DEFAULT_CELL_W, DEFAULT_CELL_H);
                let t = truncate_columns(&a.alt, wcols);
                out.write_all(format!("\x1b[2m{t}\x1b[22m\n").as_bytes())
                    .map_err(|e| e.to_string())?;
            }
            ItemKind::Image { .. } => {}
        }
    }
    // IIP 序列(与真实帧同款放置计算)
    for img in 0..prep.images.len() {
        let top = model.block_row[img] as i64 - scroll as i64;
        let block_h = (model.r_rows[img] + 1) as i64;
        if top + block_h <= 0 || top >= rows as i64 {
            continue;
        }
        if let Placement::Image { payload, x, top } = place_image(
            &prep,
            img,
            top,
            rows,
            cols,
            DEFAULT_CELL_W,
            DEFAULT_CELL_H,
            &mut caches,
        ) {
            let y = top.max(0);
            if (y as u16) < rows {
                execute!(out, MoveTo(x, y as u16)).map_err(|e| e.to_string())?;
                out.write_all(payload.as_bytes())
                    .map_err(|e| e.to_string())?;
            }
        }
    }
    out.write_all(SYNC_UPDATE_END).map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())?;
    Ok(())
}

/// `blog --dump-image-frame <article-key> <rows> <cols> <row>` 的 CLI 入口。
pub fn dump_cli(args: &[String], home: &Path) -> i32 {
    if args.len() != 4 {
        eprintln!("用法: blog --dump-image-frame <article-key> <rows> <cols> <row>");
        return 2;
    }
    let parse = |i: usize| -> Option<u32> { args[i].parse().ok() };
    let (Some(rows), Some(cols), Some(row)) = (parse(1), parse(2), parse(3)) else {
        eprintln!("blog: --dump-image-frame 参数必须是数字");
        return 2;
    };
    if rows == 0 || cols == 0 || rows > 1000 || cols > 1000 {
        eprintln!("blog: rows/cols 须在 1..=1000");
        return 2;
    }
    match dump_frame(home, &args[0], rows as u16, cols as u16, row as usize) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("blog: dump-image-frame 失败: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use ratatui::style::Modifier;

    fn anchor(
        asset: &str,
        bs: usize,
        be: usize,
        w: u32,
        h: u32,
        ind: usize,
        disp: usize,
    ) -> ImgAnchor {
        ImgAnchor {
            block_start: bs,
            block_end: be,
            path: asset.to_string(),
            asset: asset.to_string(),
            w,
            h,
            indent_cols: ind,
            display_cols: disp,
            alt: format!("alt-{asset}"),
        }
    }

    /// 写一个真实 PNG 到 tmpdir(tag 区分并行测试), 返回 (root, asset 名)。
    fn make_png_asset(tag: &str, name: &str, w: u32, h: u32) -> (PathBuf, String) {
        use image::ImageEncoder as _;
        let dir = std::env::temp_dir().join(format!("tb-reader-test-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([200, 100, 50, 255]));
        let mut buf = vec![];
        image::codecs::png::PngEncoder::new(&mut buf)
            .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgba8)
            .unwrap();
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, buf).unwrap();
        (dir, name.to_string())
    }

    fn cleanup_tmp(tag: &str) {
        let dir = std::env::temp_dir().join(format!("tb-reader-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn termimg_strict_equality() {
        // 进入条件: TERMBLOG_IMG 严格相等(未设置/空串/其他值一律不满足)
        fn ok() -> bool {
            std::env::var("TERMBLOG_IMG").as_deref() == Ok("iterm2")
        }
        std::env::set_var("TERMBLOG_IMG", "iterm2");
        assert!(ok(), "iterm2 应满足严格相等");
        std::env::set_var("TERMBLOG_IMG", "");
        assert!(!ok(), "空串不满足(空环境变量仍存在)");
        std::env::set_var("TERMBLOG_IMG", "foo");
        assert!(!ok(), "其他值不满足");
        std::env::set_var("TERMBLOG_IMG", "iterm2 ");
        assert!(!ok(), "尾随空格不满足");
        std::env::remove_var("TERMBLOG_IMG");
        assert!(!ok(), "未设置不满足");
    }

    #[test]
    fn preflight_rejects_bad_manifests() {
        let (root, asset) = make_png_asset("preflight", "x.png", 10, 10);
        let good = anchor(&asset, 0, 2, 10, 10, 0, 76);
        let text = "行0\n行1\n行2\n行3\n行4\n行5\n行6\n";
        let rp = root.join("t.rendered");
        std::fs::write(&rp, "x").unwrap();
        // 区间越界(block_end > 总行数)
        assert!(preflight(text, vec![anchor(&asset, 0, 9, 10, 10, 0, 76)], &root, &rp).is_err());
        // block_start >= block_end
        assert!(preflight(text, vec![anchor(&asset, 3, 3, 10, 10, 0, 76)], &root, &rp).is_err());
        // 区间重叠
        assert!(preflight(
            text,
            vec![
                anchor(&asset, 0, 4, 10, 10, 0, 76),
                anchor(&asset, 3, 6, 10, 10, 0, 76),
            ],
            &root,
            &rp
        )
        .is_err());
        // 尺寸为零
        assert!(preflight(text, vec![anchor(&asset, 0, 2, 0, 10, 0, 76)], &root, &rp).is_err());
        assert!(preflight(text, vec![anchor(&asset, 0, 2, 10, 0, 0, 76)], &root, &rp).is_err());
        // indent > display
        assert!(preflight(text, vec![anchor(&asset, 0, 2, 10, 10, 10, 8)], &root, &rp).is_err());
        // display > 76
        assert!(preflight(text, vec![anchor(&asset, 0, 2, 10, 10, 0, 80)], &root, &rp).is_err());
        // asset 路径逃逸/非法字符(即便 preflight 先查区间, 这里给合法区间)
        let evil = anchor("../etc/passwd", 0, 2, 10, 10, 0, 76);
        assert!(preflight(text, vec![evil], &root, &rp).is_err());
        let evil = anchor("a/../x.png", 0, 2, 10, 10, 0, 76);
        assert!(preflight(text, vec![evil], &root, &rp).is_err());
        let evil = anchor("a b.png", 0, 2, 10, 10, 0, 76);
        assert!(preflight(text, vec![evil], &root, &rp).is_err());
        let evil = anchor("A.png", 0, 2, 10, 10, 0, 76);
        assert!(preflight(text, vec![evil], &root, &rp).is_err());
        // 合法: 成功
        let ok = preflight(text, vec![good], &root, &rp).unwrap();
        assert_eq!(ok.raw.len(), 7);
        assert_eq!(ok.images.len(), 1);
        cleanup_tmp("preflight");
    }

    #[test]
    fn parse_line_roundtrip_real_output() {
        // content-build 真实输出(逐行断言可见文本 + span 样式 round-trip)
        // 标题粗体: \x1b[1m…\x1b[0m
        let spans = parse_line("\x1b[1m图片管线自检\x1b[0m");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "图片管线自检");
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
        // 占位框顶边 dim: \x1b[2m┌─ 图片 ─…\x1b[22m
        let line = "\x1b[2m┌─ 图片 ─────────────────────\x1b[22m";
        let spans = parse_line(line);
        assert_eq!(spans.len(), 1, "{spans:?}");
        assert_eq!(spans[0].content.as_ref(), "┌─ 图片 ─────────────────────");
        assert!(spans[0].style.add_modifier.contains(Modifier::DIM));
        // URL 行: dim 前缀 + 空格 + cyan 链接(OSC 8 剥除, 样式保留), 行尾复位
        let line = "\x1b[2m│\x1b[22m \x1b[36m\x1b]8;;http://h/blog/x.png\x1b\\http://h/blog/x.png\x1b]8;;\x1b\\\x1b[0m";
        let spans = parse_line(line);
        let visible: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(visible, "│ http://h/blog/x.png", "OSC 8 应剥除: {spans:?}");
        assert!(spans[0].style.add_modifier.contains(Modifier::DIM));
        assert_eq!(
            spans[2].style.fg,
            Some(Color::Cyan),
            "链接样式保留: {spans:?}"
        );
        // 行中复位: \x1b[1m粗\x1b[22m细
        let spans = parse_line("\x1b[1m粗\x1b[22m细");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content.as_ref(), "粗");
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[1].content.as_ref(), "细");
        assert!(!spans[1].style.add_modifier.contains(Modifier::BOLD));
        // 斜体/cyan 开闭
        let spans = parse_line("\x1b[3m斜\x1b[23m\x1b[36m青\x1b[39m");
        assert!(spans[0].style.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(spans[1].style.fg, Some(Color::Cyan));
    }

    #[test]
    fn parse_cell_response_format() {
        assert_eq!(parse_cell_response(b"\x1b[6;21;9t"), Some((21, 9)));
        assert_eq!(parse_cell_response(b"junk\x1b[6;20;9t"), Some((20, 9)));
        assert_eq!(parse_cell_response(b"\x1b[6;0;9t"), None, "零高不合法");
        assert_eq!(parse_cell_response(b"\x1b[6;21"), None, "未完整");
        assert_eq!(parse_cell_response(b"\x1b[5;21;9t"), None, "非 16t 响应");
    }

    #[test]
    fn geometry_wpx_and_rows() {
        // (display 76, indent 0, cols 80, cell 9×20, w=1080, h=607)
        let a = anchor("x.png", 0, 2, 1080, 607, 0, 76);
        let (wcols, wpx, r) = block_geometry(&a, 80, 9, 20);
        assert_eq!(wcols, 76);
        assert_eq!(wpx, 684);
        // R = ceil(684×607 / 1080 / 20) = ceil(19.23) = 20
        assert_eq!(r, 20);
        // 引用块内: indent 2, display 74 → W = min(74, 78) = 74
        let a = anchor("x.png", 0, 2, 1080, 607, 2, 74);
        let (wcols, wpx, r) = block_geometry(&a, 80, 9, 20);
        assert_eq!((wcols, wpx), (74, 666));
        // R = ceil(666×607/1080/20) = ceil(18.72) = 19
        assert_eq!(r, 19);
        // 窄窗口 min(display, cols−indent) 收缩
        let (wcols, _, _) = block_geometry(&a, 76, 9, 20);
        assert_eq!(wcols, 74);
        // 极小图 R ≥ 1
        let a = anchor("x.png", 0, 2, 1, 1, 0, 76);
        let (_, _, r) = block_geometry(&a, 80, 9, 20);
        assert_eq!(r, 35); // ceil(684×1/1/20)
    }

    #[test]
    fn slice_window_mapping() {
        // 显示高 384px, 每模型行严格对应 20 个显示像素。
        assert_eq!(slice_window(384, 20, 5, 10), (100, 300));
        // 到底 clamp 到显示高度。
        assert_eq!(slice_window(384, 20, 19, 10), (380, 384));
        // 2×2 小图放大成 684×684 后也在显示像素空间裁, 不会因源像素量化
        // 把 1 行切片错误地重新膨胀成几百像素高。
        assert_eq!(slice_window(684, 20, 15, 1), (300, 320));
    }

    #[test]
    fn incremental_target_exposes_only_one_edge_row() {
        assert_eq!(
            incremental_target(10, 11, 24),
            Some((PhysicalScroll::Up, 34, 23))
        );
        assert_eq!(
            incremental_target(10, 9, 24),
            Some((PhysicalScroll::Down, 9, 0))
        );
        assert_eq!(incremental_target(10, 12, 24), None);
        assert_eq!(incremental_target(10, 10, 24), None);
        assert_eq!(incremental_target(10, 11, 0), None);
    }

    #[test]
    fn row_payload_is_one_terminal_row_and_cached() {
        let (root, asset) = make_png_asset("row-payload", "photo.png", 160, 90);
        let a = anchor(&asset, 0, 2, 160, 90, 0, 76);
        let mut caches = Caches::default();

        // 显示尺寸 684×384; 中间一行严格裁出 20px, IIP 高度用 1 cell。
        let middle = row_payload(&mut caches, &root, &a, 76, 684, 5, 20).unwrap();
        assert!(middle.contains(";width=76;height=1;preserveAspectRatio=0:"));
        let b64 = middle
            .split_once(':')
            .unwrap()
            .1
            .strip_suffix('\x07')
            .unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        let img = image::load_from_memory(&bytes).unwrap();
        assert_eq!((img.width(), img.height()), (684, 20));

        let again = row_payload(&mut caches, &root, &a, 76, 684, 5, 20).unwrap();
        assert_eq!(again, middle);
        assert_eq!(caches.row_slices.map.len(), 1, "同一行应命中缓存");

        // 最后一行只剩 4px, 保留像素高度, 不把 4px 拉伸为 20px。
        let last = row_payload(&mut caches, &root, &a, 76, 684, 19, 20).unwrap();
        assert!(last.contains(";width=76;height=4px;preserveAspectRatio=0:"));
        let b64 = last
            .split_once(':')
            .unwrap()
            .1
            .strip_suffix('\x07')
            .unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        let img = image::load_from_memory(&bytes).unwrap();
        assert_eq!((img.width(), img.height()), (684, 4));
        cleanup_tmp("row-payload");
    }

    #[test]
    fn build_model_degrade_on_missing_asset() {
        let (root, asset) = make_png_asset("degrade", "y.png", 8, 8);
        let rp = root.join("t.rendered");
        let text = "行0\n行1\n┌框\n行3\n行4\n行5\n行6\n行7\n";
        std::fs::write(&rp, text).unwrap();
        let a = anchor(&asset, 2, 6, 8, 8, 0, 76);
        let prep = preflight(text, vec![a], &root, &rp).unwrap();
        // 删除 asset → build_model 完整 payload 失败 → 降级为占位框原文行
        std::fs::remove_file(root.join(&asset)).unwrap();
        let mut caches = Caches::default();
        let model = build_model(&prep, 80, DEFAULT_CELL_W, DEFAULT_CELL_H, &mut caches);
        assert_eq!(model.r_rows[0], 0, "应降级(无图像行)");
        // 模型 = 原文行: Text(0..2) + Text(2..6) + Text(6..8)
        assert_eq!(model.items.len(), 8);
        assert!(model
            .items
            .iter()
            .all(|k| matches!(k, ItemKind::Text { .. })));
        cleanup_tmp("degrade");
        // 正常情况: 图像块占 R+1 行
        let (root, asset) = make_png_asset("degrade", "y.png", 8, 8);
        let rp = root.join("t.rendered");
        std::fs::write(&rp, text).unwrap();
        let a = anchor(&asset, 2, 6, 8, 8, 0, 76);
        let prep = preflight(text, vec![a], &root, &rp).unwrap();
        let mut caches = Caches::default();
        let model = build_model(&prep, 80, DEFAULT_CELL_W, DEFAULT_CELL_H, &mut caches);
        let r = model.r_rows[0];
        assert!(r >= 1);
        assert_eq!(model.items.len(), 8 - 4 + r + 1, "图像块替换占位框区间");
        assert_eq!(model.items[2], ItemKind::Image { img: 0 });
        assert_eq!(model.items[2 + r], ItemKind::Alt { img: 0 });
        cleanup_tmp("degrade");
    }

    #[test]
    fn truncate_columns_cjk() {
        assert_eq!(truncate_columns("abc中def", 5), "abc中"); // 3 + 2 = 5
        assert_eq!(truncate_columns("abcdef", 4), "abcd");
        assert_eq!(truncate_columns("", 4), "");
    }

    #[test]
    fn exit_view_expands_image_placeholder() {
        let (root, asset) = make_png_asset("exitview", "z.png", 100, 8); // R = ceil(684×8/100/20) = 3
        let rp = root.join("t.rendered");
        let text = "行0\n行1\n┌框\n框2\n框3\n└框\n行6\n行7\n行8\n行9\n";
        std::fs::write(&rp, text).unwrap();
        let a = anchor(&asset, 2, 6, 100, 8, 0, 76);
        let prep = preflight(text, vec![a], &root, &rp).unwrap();
        let mut caches = Caches::default();
        let model = build_model(&prep, 80, DEFAULT_CELL_W, DEFAULT_CELL_H, &mut caches);
        let view = exit_view(&prep, &model, 0, 10);
        // 图像块(2..6)展开为完整占位框 4 行, 其余原文照排
        assert_eq!(
            view,
            vec![
                "行0", "行1", "┌框", "框2", "框3", "└框", "行6", "行7", "行8", "行9",
            ]
        );
        cleanup_tmp("exitview");
    }

    #[test]
    fn scroll_anchor_mapping() {
        let (root, asset) = make_png_asset("anchor", "s.png", 8, 8);
        let rp = root.join("t.rendered");
        let text = "行0\n行1\n行2\n┌框\n框4\n└框\n行6\n行7\n行8\n";
        std::fs::write(&rp, text).unwrap();
        let a = anchor(&asset, 3, 6, 8, 8, 0, 76);
        let prep = preflight(text, vec![a], &root, &rp).unwrap();
        let mut caches = Caches::default();
        let model = build_model(&prep, 80, DEFAULT_CELL_W, DEFAULT_CELL_H, &mut caches);
        let r = model.r_rows[0];
        // 视口首行是图像块 → anchor = block_start = 3
        assert_eq!(anchor_line(&model, &prep, model.block_row[0]), 3);
        // 回到行 6: 第一个 ≥6 的模型行
        let idx = scroll_to_line(&model, &prep, 6);
        assert_eq!(idx, model.block_row[0] + r + 1);
        assert_eq!(anchor_line(&model, &prep, idx), 6);
        cleanup_tmp("anchor");
    }
}
