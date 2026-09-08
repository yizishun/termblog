//! 图片管线: 白名单 / URL 重写 / 缩放重编码 / 内联预算。
//!
//! 图片以 Markdown 所在目录为基准解析，规范化后必须仍在 content/HOME 根内；
//! 本地引用在构建期改写为 content-relative 的站点绝对路径。
//!
//! 内联预算: 单张位图 ≤ 256 KiB, gif ≤ 512 KiB(gif 不重编码,
//! 保动画), 单篇文章内联图片总量 ≤ 1.5 MiB。原图或处理后产物
//! 超限时不再报错：Web 原样发布无尺寸限制的原图，正文降级为链接，
//! 终端不传输图像载荷。只有内联位图宽度 > 1080 px 时才缩小到 1080。
//! 外部图片(http(s):// 等绝对 URL)原样透传, 不校验、不复制、不计预算。

use std::path::Path;

use anyhow::{bail, Context, Result};

/// 位图宽度上限: 超过则缩小到此宽度。
pub const MAX_WIDTH: u32 = 1080;
/// 单张位图处理后字节预算。
pub const MAX_BITMAP_BYTES: usize = 256 * 1024;
/// 单张 gif 字节预算(原样采用, 不缩放不重编码)。
pub const MAX_GIF_BYTES: usize = 512 * 1024;
/// 单篇文章内联图片总量预算(1.5 MiB)。
pub const MAX_ARTICLE_BYTES: usize = 1536 * 1024;

/// 扩展名白名单(大小写不敏感)。svg 明确拒绝: 同域直接打开会执行其中脚本。
pub fn is_image_ext(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "webp" | "gif"
    )
}

/// 图片 URL 的解析结果。
pub enum ResolvedImage {
    /// 本站图片: content-relative 的站点绝对路径(含 ?/# 后缀)。
    Local(String),
    /// 外部图片(http: https: mailto: data: 等带 scheme, 或站点绝对路径): 原样。
    External(String),
}

/// 剥掉 dest 的 `?`/`#` 后缀, 返回 (路径部分, 后缀)。
fn split_suffix(dest: &str) -> (&str, &str) {
    match dest.find(['?', '#']) {
        Some(idx) => (&dest[..idx], &dest[idx..]),
        None => (dest, ""),
    }
}

/// scheme 检测: `^[a-zA-Z][a-zA-Z0-9+.-]*:`(http: https: mailto: data: …)。
fn has_scheme(dest: &str) -> bool {
    match dest.find(':') {
        Some(colon) => {
            let scheme = &dest[..colon];
            !scheme.is_empty()
                && scheme
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
        }
        None => false,
    }
}

/// 把 md 里的相对 dest 规范化为 content/HOME-relative 路径。
/// 返回 Ok(Some((rel, suffix))) = 本地图; Ok(None) = 外链/站点绝对路径; Err = 违规。
pub fn normalize_local_path(source_rel: &str, dest: &str) -> Result<Option<(String, String)>> {
    if has_scheme(dest) || dest.starts_with('/') {
        return Ok(None);
    }
    let (path_part, suffix) = split_suffix(dest);
    if path_part.contains('%') {
        bail!("image path contains '%'(please use [a-z0-9/._-] filename charset): {dest}");
    }
    // base = Markdown 源文件的直属目录（相对 content/HOME）。
    let base = match source_rel.rfind('/') {
        Some(idx) => &source_rel[..idx],
        None => "",
    };
    let joined = if base.is_empty() {
        path_part.to_string()
    } else {
        format!("{base}/{path_part}")
    };
    // 按 / 分段规范化: "." 丢弃, ".." 弹栈, 栈空则逃逸 content 根
    let mut stack: Vec<&str> = vec![];
    for seg in joined.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if stack.pop().is_none() {
                    bail!("image path escapes content root: {dest}");
                }
            }
            s => stack.push(s),
        }
    }
    if stack.is_empty() {
        bail!("image path does not point to any file: {dest}");
    }
    Ok(Some((stack.join("/"), suffix.to_string())))
}

/// source_rel 是含 `.md` 的 content-relative 路径；dest 是 Markdown 原始 URL。
/// 返回 content-relative 的站点绝对路径；外链原样返回。
pub fn resolve_image_url(source_rel: &str, dest: &str) -> Result<ResolvedImage> {
    match normalize_local_path(source_rel, dest)? {
        Some((rel, suffix)) => Ok(ResolvedImage::Local(format!("/{rel}{suffix}"))),
        None => Ok(ResolvedImage::External(dest.to_string())),
    }
}

/// 一张处理完成的图片: 最终字节 + 真实尺寸(来自解码, 不解析文件头手写)。
#[derive(Debug)]
pub struct ProcessedImage {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// 最终产物文件名(相对 content)。webp 产物统一转 png, 此字段会换扩展名;
    /// 其余格式与输入 rel 相同。
    pub dist_rel: String,
}

/// 图片在两种发布形态之间的构建期决策。
#[derive(Debug)]
pub enum ImageOutput {
    /// 处理后图像可进入终端内联管线。
    Inline(ProcessedImage),
    /// 超出单图内联预算；原文件仅发布到 Web，正文显示链接。
    LinkOnly { reason: String },
}

/// 处理一张本地图: 读原字节 → gif 原样采用(只解码拿尺寸);
/// 位图宽 > 1080 则缩小重编码(重编码变大则回退原字节, 此时尺寸按原图报告);
/// **webp 产物(含回退原字节分支)统一转 png** —— 构建期格式保证: 处理后产物
/// 只含 png/jpeg/gif(收窄 addon-image 载荷兼容面; webp 只出现在 lossless
/// 场景, 转 png 字节级等价)。预算按转换后字节核算。
/// 原图或处理后产物超出单图预算时返回 `LinkOnly`，不压缩原图。
/// 解码错误等非预算问题仍然报错。
pub fn process_image(path: &Path, rel: &str, article_key: &str) -> Result<ImageOutput> {
    use image::ImageEncoder;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let source_budget = if ext == "gif" {
        MAX_GIF_BYTES
    } else {
        MAX_BITMAP_BYTES
    };
    let source_len = std::fs::metadata(path)
        .with_context(|| format!("stat image {}", path.display()))?
        .len();
    if source_len > source_budget as u64 {
        // 只读头部/尺寸以拒绝伪图片，不将无限制原图读入内存。
        image::ImageReader::open(path)
            .with_context(|| format!("open image of \"{article_key}\": {rel}"))?
            .with_guessed_format()
            .with_context(|| format!("detect image format of \"{article_key}\": {rel}"))?
            .into_dimensions()
            .with_context(|| {
                format!("failed to read image dimensions of \"{article_key}\": {rel}")
            })?;
        return Ok(ImageOutput::LinkOnly {
            reason: format!(
                "original {} KiB exceeds per-image inline budget {} KiB",
                source_len / 1024,
                source_budget / 1024
            ),
        });
    }
    let raw = std::fs::read(path).with_context(|| format!("read image {}", path.display()))?;

    if ext == "gif" {
        // gif: 已通过原图预算，原样采用，不缩放(会丢动画)。
        let img = image::load_from_memory_with_format(&raw, image::ImageFormat::Gif)
            .with_context(|| format!("failed to decode gif of \"{article_key}\": {rel}"))?;
        return Ok(ImageOutput::Inline(ProcessedImage {
            bytes: raw,
            width: img.width(),
            height: img.height(),
            dist_rel: rel.to_string(),
        }));
    }

    let img = image::load_from_memory(&raw)
        .with_context(|| format!("failed to decode image of \"{article_key}\": {rel}"))?;
    let (w, h) = (img.width(), img.height());

    let (mut bytes, mut fw, mut fh) = if w > MAX_WIDTH {
        let nh = ((h as u64 * MAX_WIDTH as u64) / w as u64).max(1) as u32;
        let resized =
            image::imageops::resize(&img, MAX_WIDTH, nh, image::imageops::FilterType::Triangle);
        let encoded = reencode(&resized, &ext)
            .with_context(|| format!("failed to re-encode image of \"{article_key}\": {rel}"))?;
        if encoded.len() > raw.len() {
            // 重编码后更大(如 WebP 只有 lossless): 回退用原字节, 尺寸按原图
            (raw.clone(), w, h)
        } else {
            (encoded, MAX_WIDTH, nh)
        }
    } else {
        // 未触发缩放: 直接用原字节, 不重编码
        (raw.clone(), w, h)
    };

    // webp → png: 处理后产物只含 png/jpeg/gif(见文件头注释)。重解码拿像素
    // 再编码 PNG, 尺寸从 PNG 解码结果取(与 reader 读到的字节同源)。
    let dist_rel = if ext == "webp" {
        let img = image::load_from_memory(&bytes)
            .with_context(|| format!("failed to decode webp artifact of \"{article_key}\" (before png conversion): {rel}"))?;
        let rgba = img.to_rgba8();
        let mut png_buf: Vec<u8> = vec![];
        image::codecs::png::PngEncoder::new(&mut png_buf).write_image(
            rgba.as_raw(),
            rgba.width(),
            rgba.height(),
            image::ExtendedColorType::Rgba8,
        )?;
        bytes = png_buf;
        fw = rgba.width();
        fh = rgba.height();
        with_png_ext(rel)
    } else {
        rel.to_string()
    };

    if bytes.len() > MAX_BITMAP_BYTES {
        return Ok(ImageOutput::LinkOnly {
            reason: format!(
                "processed {} KiB exceeds per-image inline budget {} KiB",
                bytes.len() / 1024,
                MAX_BITMAP_BYTES / 1024
            ),
        });
    }
    Ok(ImageOutput::Inline(ProcessedImage {
        bytes,
        width: fw,
        height: fh,
        dist_rel,
    }))
}

/// rel 的扩展名换成 .png(webp 产物转 png 后的最终文件名)。
fn with_png_ext(rel: &str) -> String {
    match rel.rfind('.') {
        Some(i) if i > rel.rfind('/').unwrap_or(0) => format!("{}.png", &rel[..i]),
        _ => format!("{rel}.png"),
    }
}

/// 按扩展名重编码: jpg/jpeg → JPEG q80; png → PNG 默认; webp → WebP(lossless,
/// image 0.25 没有 lossy 编码器, 可能变大, 调用方负责回退)。
fn reencode(img: &image::RgbaImage, ext: &str) -> Result<Vec<u8>> {
    use image::ImageEncoder;
    let mut buf: Vec<u8> = vec![];
    match ext {
        "jpg" | "jpeg" => {
            // JPEG 无 alpha: 转 RGB(透明部分落白底)
            let rgb = image::DynamicImage::ImageRgba8(img.clone()).to_rgb8();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 80).write_image(
                rgb.as_raw(),
                rgb.width(),
                rgb.height(),
                image::ExtendedColorType::Rgb8,
            )?;
        }
        "webp" => {
            image::codecs::webp::WebPEncoder::new_lossless(&mut buf).write_image(
                img.as_raw(),
                img.width(),
                img.height(),
                image::ExtendedColorType::Rgba8,
            )?;
        }
        _ => {
            image::codecs::png::PngEncoder::new(&mut buf).write_image(
                img.as_raw(),
                img.width(),
                img.height(),
                image::ExtendedColorType::Rgba8,
            )?;
        }
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(key: &str, dest: &str) -> Result<ResolvedImage> {
        resolve_image_url(&format!("{key}.md"), dest)
    }

    fn local(r: ResolvedImage) -> String {
        match r {
            ResolvedImage::Local(u) => u,
            ResolvedImage::External(u) => panic!("应为 Local, 得到 External({u})"),
        }
    }

    fn external(r: ResolvedImage) -> String {
        match r {
            ResolvedImage::External(u) => u,
            ResolvedImage::Local(u) => panic!("应为 External, 得到 Local({u})"),
        }
    }

    #[test]
    fn resolve_nested_source() {
        // 嵌套源路径: base = Markdown 所在目录
        let u = local(resolve("a/b/c", "img.png").unwrap());
        assert_eq!(u, "/a/b/img.png");
    }

    #[test]
    fn resolve_asset_dir() {
        // 同名资源目录: hello + hello/x.png → /hello/x.png
        let u = local(resolve("hello", "hello/x.png").unwrap());
        assert_eq!(u, "/hello/x.png");
    }

    #[test]
    fn resolve_dot_segments() {
        // 顶层文章 base = "": ./x.png → /x.png
        assert_eq!(local(resolve("hello", "./x.png").unwrap()), "/x.png");
        // `./` 相对 Markdown 所在目录
        assert_eq!(local(resolve("a/b", "./x.png").unwrap()), "/a/x.png");
        // source "sub/post.md": base "sub", ../shared/x.png → shared/x.png
        let u = local(resolve("sub/post", "../shared/x.png").unwrap());
        assert_eq!(u, "/shared/x.png");
        // 逃逸 content 根 → fail
        assert!(resolve("hello", "../x.png").is_err());
        assert!(resolve("a/b", "../../etc").is_err());
    }

    #[test]
    fn resolve_suffix_preserved() {
        assert_eq!(
            local(resolve("hello", "hello/x.png?v=1").unwrap()),
            "/hello/x.png?v=1"
        );
        assert_eq!(
            local(resolve("hello", "hello/x.png#frag").unwrap()),
            "/hello/x.png#frag"
        );
    }

    #[test]
    fn resolve_external_passthrough() {
        assert_eq!(
            external(resolve("hello", "https://cdn.example.com/a.png").unwrap()),
            "https://cdn.example.com/a.png"
        );
        assert_eq!(
            external(resolve("hello", "http://example.com/a.png").unwrap()),
            "http://example.com/a.png"
        );
        assert_eq!(
            external(resolve("hello", "data:image/png;base64,xx").unwrap()),
            "data:image/png;base64,xx"
        );
        // 站点绝对路径 → 按 External 原样(调用方告警)
        assert_eq!(
            external(resolve("hello", "/blog/other/x.png").unwrap()),
            "/blog/other/x.png"
        );
    }

    #[test]
    fn resolve_rejects_percent() {
        assert!(resolve("hello", "a%20b.png").is_err());
    }

    #[test]
    fn asset_path_charset() {
        assert!(termblog_content_model::validate_resource_rel("hello/arch.png").is_ok());
        assert!(termblog_content_model::validate_resource_rel("hello/arch_v2.min-v2.webp").is_ok());
        assert!(termblog_content_model::validate_resource_rel("中文.png").is_err());
        assert!(termblog_content_model::validate_resource_rel("a b.png").is_err());
        assert!(termblog_content_model::validate_resource_rel("A.png").is_err());
        assert!(termblog_content_model::validate_resource_rel("").is_err());
    }

    /// 构造内存 PNG(纯色 w×h)。
    fn make_png(w: u32, h: u32) -> Vec<u8> {
        use image::ImageEncoder;
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([200, 100, 50, 255]));
        let mut buf = vec![];
        image::codecs::png::PngEncoder::new(&mut buf)
            .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgba8)
            .unwrap();
        buf
    }

    fn write_tmp(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn expect_inline(output: ImageOutput) -> ProcessedImage {
        match output {
            ImageOutput::Inline(image) => image,
            ImageOutput::LinkOnly { reason } => panic!("expected inline image: {reason}"),
        }
    }

    #[test]
    fn process_wide_png_resized() {
        let dir = std::env::temp_dir().join(format!("tb-img-test-{}-wide", std::process::id()));
        // 2000×100 纯色 PNG: 宽 > 1080 → 应缩小到 1080×54
        let p = write_tmp(&dir, "wide.png", &make_png(2000, 100));
        let r = expect_inline(process_image(&p, "t/wide.png", "t").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(r.width, 1080);
        assert_eq!(r.height, 54);
        // 缩小后的字节必须能解码且尺寸一致
        let img = image::load_from_memory(&r.bytes).unwrap();
        assert_eq!((img.width(), img.height()), (1080, 54));
    }

    #[test]
    fn process_small_png_passthrough() {
        let dir = std::env::temp_dir().join(format!("tb-img-test-{}-small", std::process::id()));
        let raw = make_png(100, 50);
        let p = write_tmp(&dir, "small.png", &raw);
        let r = expect_inline(process_image(&p, "t/small.png", "t").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!((r.width, r.height), (100, 50));
        assert_eq!(r.bytes, raw, "未触发缩放应原样采用原字节");
    }

    #[test]
    fn process_oversize_bitmap_becomes_original_link() {
        let dir = std::env::temp_dir().join(format!("tb-img-test-{}-big", std::process::id()));
        // 噪点 PNG: 原图超 256 KiB，应不解码/压缩，直接发布原图链接。
        let mut img = image::RgbaImage::new(1000, 1000);
        let mut x: u32 = 12345;
        for px in img.pixels_mut() {
            // xorshift 伪随机
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            *px = image::Rgba([
                (x & 0xff) as u8,
                ((x >> 8) & 0xff) as u8,
                ((x >> 16) & 0xff) as u8,
                255,
            ]);
        }
        use image::ImageEncoder;
        let mut buf = vec![];
        image::codecs::png::PngEncoder::new(&mut buf)
            .write_image(img.as_raw(), 1000, 1000, image::ExtendedColorType::Rgba8)
            .unwrap();
        assert!(buf.len() > MAX_BITMAP_BYTES, "测试前提: 噪点图应超预算");
        let p = write_tmp(&dir, "big.png", &buf);
        let output = process_image(&p, "t/big.png", "my-post").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        match output {
            ImageOutput::LinkOnly { reason } => {
                assert!(reason.contains("256"), "降级原因须包含预算: {reason}");
            }
            ImageOutput::Inline(_) => panic!("超限图片不应内联"),
        }
    }

    #[test]
    fn process_oversize_fake_image_still_fails_validation() {
        let dir = std::env::temp_dir().join(format!("tb-img-test-{}-fake-big", std::process::id()));
        let fake = vec![b'x'; MAX_BITMAP_BYTES + 1];
        let p = write_tmp(&dir, "fake.png", &fake);
        let error = process_image(&p, "t/fake.png", "my-post").unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
        let message = format!("{error:#}");
        assert!(message.contains("my-post"), "报错须含文章 key: {message}");
        assert!(message.contains("t/fake.png"), "报错须含路径: {message}");
    }

    #[test]
    fn process_gif_passthrough_and_budget() {
        let dir = std::env::temp_dir().join(format!("tb-img-test-{}-gif", std::process::id()));
        // 最小合法 gif(1×1, 透明)
        let gif1x1: &[u8] = &[
            0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0x00,
            0x00, 0x00, 0xff, 0xff, 0xff, 0x21, 0xf9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2c,
            0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00,
            0x3b,
        ];
        let p = write_tmp(&dir, "a.gif", gif1x1);
        let r = expect_inline(process_image(&p, "t/a.gif", "t").unwrap());
        assert_eq!((r.width, r.height), (1, 1));
        assert_eq!(r.bytes, gif1x1, "gif 应原样采用");
        // 超 512 KiB 的 gif 原样发布为链接，不进入解码/内联管线。
        let mut big = gif1x1.to_vec();
        big.resize(MAX_GIF_BYTES + 1, 0);
        let p2 = write_tmp(&dir, "big.gif", &big);
        let output = process_image(&p2, "t/big.gif", "t").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        match output {
            ImageOutput::LinkOnly { reason } => {
                assert!(reason.contains("512"), "gif 降级原因: {reason}");
            }
            ImageOutput::Inline(_) => panic!("超限 gif 不应内联"),
        }
    }

    #[test]
    fn process_webp_converts_to_png() {
        use image::ImageEncoder;
        let dir = std::env::temp_dir().join(format!("tb-img-test-{}-webp", std::process::id()));
        // 用与 reencode 相同的编码器生成一张 2×2 lossless webp(保证本 crate 可解码)。
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([200, 50, 50, 255]));
        let mut buf = vec![];
        image::codecs::webp::WebPEncoder::new_lossless(&mut buf)
            .write_image(img.as_raw(), 2, 2, image::ExtendedColorType::Rgba8)
            .unwrap();
        let p = write_tmp(&dir, "a.webp", &buf);
        let r = expect_inline(process_image(&p, "t/a.webp", "t").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(r.dist_rel, "t/a.png", "webp 产物应统一转 png 扩展名");
        assert_eq!((r.width, r.height), (2, 2));
        // 产物字节必须是合法 PNG(reader/浏览器按 png 解码)。
        let decoded = image::load_from_memory(&r.bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (2, 2));
    }
}
