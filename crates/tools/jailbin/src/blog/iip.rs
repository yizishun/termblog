//! iip.rs —— iTerm2 Inline Images Protocol 编码器(自写, 不引 ratatui-image)。
//!
//! 序列(§5.6.4):
//!
//! ```text
//! ESC]1337;File=name=<base64(asset)>;size=<原始字节数>;inline=1;width=<Wpx>px;height=<Hpx>px;preserveAspectRatio=0:<base64(处理后字节)>BEL
//! ```
//!
//! 直接携带 content-build 的处理后字节(只含 png/jpeg/gif), **不解码、
//! 不重编码** —— v1 构建期预算(单张位图 ≤256 KiB、gif ≤512 KiB、单篇
//! ≤1.5 MiB)因此直接约束线上流量(线上字节 = 处理后字节 × 4/3)。
//! xterm.js 的 @xterm/addon-image 支持 png/jpeg/gif 载荷并按 width/height
//! 属性缩放。

use base64::Engine as _;

/// 编码一条 IIP 序列。IIP 规定 name 是 base64 编码的 UTF-8 文件名;
/// `@xterm/addon-image` 还要求 size 是解码后的原始 payload 字节数, 缺失时
/// 会静默丢弃图片。
fn encode_dims(name: &str, payload: &[u8], width: &str, height: &str) -> String {
    let encoded_name = base64::engine::general_purpose::STANDARD.encode(name.as_bytes());
    let b64 = base64::engine::general_purpose::STANDARD.encode(payload);
    format!(
        "\x1b]1337;File=name={encoded_name};size={};inline=1;width={width};height={height};preserveAspectRatio=0:{b64}\x07",
        payload.len()
    )
}

/// 以像素为单位编码完整图或多行切片。preserveAspectRatio=0, 尺寸由调用方
/// 按原图宽高比算好。
pub fn encode(name: &str, payload: &[u8], width_px: u32, height_px: u32) -> String {
    encode_dims(
        name,
        payload,
        &format!("{width_px}px"),
        &format!("{height_px}px"),
    )
}

/// 以终端 cell 为单位编码增量滚动时新露出的一行。裸数字在 IIP 中表示
/// cell 数; 使用 height=1 可避开浏览器浮点 cell 高度与整数像素取整之间的
/// 误差, 保证切片严格只占一行、不在视口底部触发额外滚屏。
pub fn encode_cells(name: &str, payload: &[u8], width_cols: u32, height_rows: u32) -> String {
    encode_dims(
        name,
        payload,
        &width_cols.to_string(),
        &height_rows.to_string(),
    )
}

/// 增量图片行: 完整 cell 高的切片使用 height=1; 图片末尾不足一行的切片
/// 保留实际像素高度, 避免把最后几个像素拉伸成整行。后者一定小于一个
/// cell, 因此不会因浏览器 cell 高度取整而占到两行。
pub fn encode_row(
    name: &str,
    payload: &[u8],
    width_cols: u32,
    height_px: u32,
    cell_height_px: u32,
) -> String {
    if height_px >= cell_height_px.max(1) {
        return encode_cells(name, payload, width_cols, 1);
    }
    encode_dims(
        name,
        payload,
        &width_cols.to_string(),
        &format!("{}px", height_px.max(1)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_bytes() {
        // 与手拼序列逐字节比对: payload = base64(处理后字节)
        let s = encode("hello/arch.png", b"\x89PNG-payload", 684, 385);
        let expected = "\u{1b}]1337;File=name=aGVsbG8vYXJjaC5wbmc=;size=12;inline=1;width=684px;height=385px;preserveAspectRatio=0:iVBORy1wYXlsb2Fk\u{07}";
        assert_eq!(s, expected);
    }

    #[test]
    fn payload_is_base64_of_bytes() {
        let bytes: Vec<u8> = (0u8..=255).collect();
        let s = encode("x.gif", &bytes, 10, 20);
        // 提取 : 与 BEL 之间的 base64, 解码后与输入逐字节相等
        let b64 = s.split(':').nth(1).unwrap().strip_suffix('\x07').unwrap();
        let dec = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        assert_eq!(dec, bytes);
    }

    #[test]
    fn header_fields() {
        let s = encode("a/b.jpg", b"xx", 1, 2);
        assert!(s.starts_with("\u{1b}]1337;File=name=YS9iLmpwZw==;size=2;inline=1;"));
        assert!(s.contains(";width=1px;height=2px;preserveAspectRatio=0:"));
        assert!(s.ends_with('\x07'));
    }

    #[test]
    fn name_is_base64_utf8_and_size_is_decoded_payload_length() {
        let s = encode("文章/架构图.png", &[0, 1, 2, 3, 4], 10, 20);
        let header = s
            .strip_prefix("\x1b]1337;File=")
            .unwrap()
            .split(':')
            .next()
            .unwrap();
        let fields: Vec<&str> = header.split(';').collect();
        let name = fields[0].strip_prefix("name=").unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(name)
            .unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "文章/架构图.png");
        assert!(fields.contains(&"size=5"));
    }

    #[test]
    fn cell_dimensions_have_no_pixel_suffix() {
        let s = encode_cells("row.png", b"png", 76, 1);
        assert!(s.contains(";width=76;height=1;preserveAspectRatio=0:"));
        assert!(!s.contains("width=76px"));
        assert!(!s.contains("height=1px"));
    }

    #[test]
    fn row_dimension_keeps_partial_last_row_height() {
        let full = encode_row("row.png", b"png", 76, 20, 20);
        assert!(full.contains(";width=76;height=1;"));
        let partial = encode_row("row.png", b"png", 76, 4, 20);
        assert!(partial.contains(";width=76;height=4px;"));
    }
}
