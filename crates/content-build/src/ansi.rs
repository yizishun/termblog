//! ANSI 预渲染: 同一事件流的终端投影。目标宽度 76 显示列(unicode-width
//! 计宽, CJK=2), 输出 UTF-8 文本 + SGR 颜色序列, `less -R` 直接读。
//!
//! 折行: 贪心填充, 断行机会 = 空格之后, 或相邻两字符中任一是宽字符
//! (width==2) 之间; 单个不可断原子超宽时硬切。样式跨行: 断行处行尾发
//! `\x1b[0m`, 新行行首重发活动样式, 保证 less 分屏重绘不串色。

use pulldown_cmark::{Event, Tag, TagEnd};
use unicode_width::UnicodeWidthChar;

/// 目标显示列宽。
const WIDTH: usize = 76;

// SGR 样式位(开启 1/3/2/36, 关闭 22/23/22/39)
const BOLD: u8 = 1;
const ITALIC: u8 = 2;
const DIM: u8 = 4;
const CYAN: u8 = 8;

#[derive(Clone)]
struct Seg {
    s: u8,
    t: String,
}

fn push_seg(segs: &mut Vec<Seg>, s: u8, t: impl Into<String>) {
    let t = t.into();
    if t.is_empty() {
        return;
    }
    if let Some(last) = segs.last_mut() {
        if last.s == s {
            last.t.push_str(&t);
            return;
        }
    }
    segs.push(Seg { s, t });
}

enum Block {
    Paragraph(Vec<Seg>),
    Heading(Vec<Seg>),
    Rule,
    Code(Vec<String>),
    Quote(Vec<Block>),
    List { ordered: bool, start: u64, items: Vec<Vec<Block>> },
    Table { head: Vec<String>, rows: Vec<Vec<String>> },
}

/// 渲染入口: 事件流 → 完整 ANSI 文本(行尾 \n, 文件尾保证一个 \n)。
pub fn render_ansi(events: &[Event<'static>]) -> String {
    let (blocks, _) = parse_blocks(events);
    let mut lines: Vec<String> = vec![];
    let ind = Indent { first: String::new(), cont: String::new(), width: WIDTH };
    render_blocks(&blocks, &mut lines, 0, 0, &ind);
    // 去掉块间渲染出的多余尾部空行, 文件尾保证一个 \n
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

// ── 解析: 事件流 → 块树 ──

fn parse_blocks(events: &[Event<'static>]) -> (Vec<Block>, usize) {
    let mut blocks = vec![];
    let mut i = 0;
    while i < events.len() {
        match &events[i] {
            Event::Start(Tag::Paragraph) => {
                let mut segs = vec![];
                i += 1;
                parse_inline(events, &mut i, 0, &mut segs);
                i += 1; // End(Paragraph)
                blocks.push(Block::Paragraph(segs));
            }
            Event::Start(Tag::Heading { .. }) => {
                let mut segs = vec![];
                i += 1;
                parse_inline(events, &mut i, BOLD, &mut segs);
                i += 1; // End(Heading)
                blocks.push(Block::Heading(segs));
            }
            Event::Start(Tag::CodeBlock(_)) => {
                i += 1;
                let mut text = String::new();
                while i < events.len() {
                    match &events[i] {
                        Event::Text(t) => {
                            text.push_str(t);
                            i += 1;
                        }
                        Event::End(TagEnd::CodeBlock) => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
                let mut lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
                while lines.last().is_some_and(|l| l.is_empty()) {
                    lines.pop();
                }
                blocks.push(Block::Code(lines));
            }
            Event::Start(Tag::BlockQuote(_)) => {
                let (inner, consumed) = parse_blocks(&events[i + 1..]);
                i += consumed + 2; // +1 切片偏移, +1 消费 End(BlockQuote)
                blocks.push(Block::Quote(inner));
            }
            Event::Start(Tag::List(start)) => {
                let ordered = start.is_some();
                let start_num = start.unwrap_or(1);
                i += 1;
                let mut items = vec![];
                while i < events.len() {
                    match &events[i] {
                        Event::Start(Tag::Item) => {
                            let (inner, consumed) = parse_blocks(&events[i + 1..]);
                            i += consumed + 2; // +1 切片偏移, +1 消费 End(Item)
                            items.push(inner);
                        }
                        Event::End(TagEnd::List(_)) => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
                blocks.push(Block::List { ordered, start: start_num, items });
            }
            Event::Start(Tag::Table(_)) => {
                i += 1;
                // 事件结构: Table > TableHead > TableCell*(无 TableRow 包裹) >
                // TableRow > TableCell* > …; 表头行在 TableHead 内
                let mut head: Vec<String> = vec![];
                let mut rows: Vec<Vec<String>> = vec![];
                let mut current: Vec<String> = vec![];
                while i < events.len() {
                    match &events[i] {
                        Event::Start(Tag::TableCell) => {
                            i += 1;
                            let mut segs = vec![];
                            parse_inline(events, &mut i, 0, &mut segs);
                            i += 1; // End(TableCell)
                            current.push(segs.iter().map(|s| s.t.as_str()).collect());
                        }
                        Event::End(TagEnd::TableHead) => {
                            head = std::mem::take(&mut current);
                            i += 1;
                        }
                        Event::End(TagEnd::TableRow) => {
                            rows.push(std::mem::take(&mut current));
                            i += 1;
                        }
                        Event::End(TagEnd::Table) => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
                blocks.push(Block::Table { head, rows });
            }
            Event::End(_) => break,
            Event::Rule => {
                blocks.push(Block::Rule);
                i += 1;
            }
            // 块级 HTML: 站点约定纯 markdown, 跳过
            Event::Html(_) => i += 1,
            // 紧凑列表项没有 Paragraph 包装(裸 inline 事件): 按隐式段落处理
            Event::Text(_)
            | Event::Code(_)
            | Event::SoftBreak
            | Event::HardBreak
            | Event::InlineHtml(_)
            | Event::FootnoteReference(_)
            | Event::TaskListMarker(_)
            | Event::Start(
                Tag::Emphasis
                | Tag::Strong
                | Tag::Strikethrough
                | Tag::Link { .. }
                | Tag::Image { .. },
            ) => {
                let mut segs = vec![];
                parse_inline(events, &mut i, 0, &mut segs);
                blocks.push(Block::Paragraph(segs));
            }
            _ => i += 1,
        }
    }
    (blocks, i)
}

/// inline 解析: 消费 events[*i] 起的内容, 遇到任意 End 事件即返回(不消费)。
/// 由调用方(段落/标题/单元格)消费该 End。
fn parse_inline(events: &[Event<'static>], i: &mut usize, style: u8, segs: &mut Vec<Seg>) {
    loop {
        match &events[*i] {
            Event::End(_) => return,
            Event::Start(Tag::Strong) => {
                *i += 1;
                parse_inline(events, i, style | BOLD, segs);
                *i += 1; // 消费 End(Strong), 继续外层 inline
            }
            Event::Start(Tag::Emphasis) => {
                *i += 1;
                parse_inline(events, i, style | ITALIC, segs);
                *i += 1; // 消费 End(Emphasis)
            }
            Event::Start(Tag::Strikethrough) => {
                *i += 1;
                parse_inline(events, i, style | DIM, segs);
                *i += 1; // 消费 End(Strikethrough)
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                let url = dest_url.to_string();
                *i += 1;
                let mut inner: Vec<Seg> = vec![];
                parse_inline(events, i, style, &mut inner);
                *i += 1; // 消费 End(Link)
                let text: String = inner.iter().map(|s| s.t.as_str()).collect();
                if text.is_empty() || text == url {
                    push_seg(segs, style | CYAN, url);
                } else {
                    for s in inner {
                        push_seg(segs, s.s, s.t);
                    }
                    push_seg(segs, style | CYAN, format!("({url})"));
                }
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                let url = dest_url.to_string();
                *i += 1;
                let mut alt_segs: Vec<Seg> = vec![];
                parse_inline(events, i, style, &mut alt_segs);
                *i += 1; // 消费 End(Image)
                let alt: String = alt_segs.iter().map(|s| s.t.as_str()).collect();
                push_seg(segs, style | DIM, format!("image: {alt} {url}"));
            }
            Event::Text(t) => {
                push_seg(segs, style, t.as_ref());
                *i += 1;
            }
            Event::Code(c) => {
                push_seg(segs, style | CYAN, c.as_ref());
                *i += 1;
            }
            Event::SoftBreak => {
                push_seg(segs, style, " ");
                *i += 1;
            }
            Event::HardBreak => {
                push_seg(segs, style, "\n");
                *i += 1;
            }
            // 内联/块级 HTML 等: 站点约定纯 markdown, 跳过
            Event::InlineHtml(_)
            | Event::Html(_)
            | Event::FootnoteReference(_)
            | Event::TaskListMarker(_) => *i += 1,
            _ => *i += 1,
        }
    }
}

// ── 渲染: 块树 → 输出行 ──

struct Indent {
    /// 首行前缀(已含样式)
    first: String,
    /// 续行前缀(已含样式)
    cont: String,
    /// 内容可用显示列宽
    width: usize,
}

/// 引用前缀: 每级 `> `(dim)。
fn qprefix(q: usize) -> String {
    "\x1b[2m> \x1b[22m".repeat(q)
}

fn render_blocks(blocks: &[Block], lines: &mut Vec<String>, q: usize, level: usize, ind: &Indent) {
    for b in blocks {
        render_block(b, lines, q, level, ind);
        // 块间空一行(Quote 由内部块自带空行, 不再加)
        if !matches!(b, Block::Quote(_)) {
            lines.push(String::new());
        }
    }
}

fn render_block(b: &Block, lines: &mut Vec<String>, q: usize, level: usize, ind: &Indent) {
    match b {
        Block::Paragraph(segs) | Block::Heading(segs) => {
            let wrapped = wrap_segments(segs, ind.width);
            for (i, l) in wrapped.iter().enumerate() {
                let pre = if i == 0 { &ind.first } else { &ind.cont };
                lines.push(format!("{pre}{}", render_line(l)));
            }
        }
        Block::Rule => {
            lines.push(format!("{}\x1b[2m{}\x1b[22m", ind.first, "-".repeat(WIDTH)));
        }
        Block::Code(code_lines) => {
            for (i, l) in code_lines.iter().enumerate() {
                let pre = if i == 0 { &ind.first } else { &ind.cont };
                lines.push(format!("{pre}    {l}"));
            }
        }
        Block::Quote(inner) => {
            let child = Indent {
                first: format!("{}{}", ind.first, qprefix(1)),
                cont: format!("{}{}", ind.cont, qprefix(1)),
                width: ind.width.saturating_sub(2),
            };
            render_blocks(inner, lines, q + 1, level, &child);
        }
        Block::List { ordered, start, items } => {
            let lead = "  ".repeat(level);
            let qp = qprefix(q);
            for (idx, item) in items.iter().enumerate() {
                let item_first = if *ordered {
                    format!("{qp}{lead}{:>3}. ", start + idx as u64)
                } else {
                    format!("{qp}{lead}  • ")
                };
                let item_cont = format!("{qp}{lead}    ");
                let item_width = WIDTH.saturating_sub(2 * q + 2 * level + 4);
                for (bi, blk) in item.iter().enumerate() {
                    let child = if bi == 0 {
                        Indent {
                            first: item_first.clone(),
                            cont: item_cont.clone(),
                            width: item_width,
                        }
                    } else {
                        Indent {
                            first: item_cont.clone(),
                            cont: item_cont.clone(),
                            width: item_width,
                        }
                    };
                    render_block(blk, lines, q, level + 1, &child);
                }
            }
        }
        Block::Table { head, rows } => {
            lines.push(format!("{}{}", ind.first, head.join(" | ")));
            lines.push(format!("{}\x1b[2m{}\x1b[22m", ind.cont, "---- | ----"));
            for r in rows {
                lines.push(format!("{}{}", ind.cont, r.join(" | ")));
            }
        }
    }
}

/// 折行: 贪心填充, 逐字符(原子)累加显示宽度, 超宽即断。
/// 断行机会: 空格之后(空格留在行尾); 或相邻两字符任一是宽字符之间。
fn wrap_segments(segs: &[Seg], width: usize) -> Vec<Vec<Seg>> {
    let mut raw_lines: Vec<Vec<(u8, char)>> = vec![];
    let mut cur: Vec<(u8, char)> = vec![];
    let mut curw = 0usize;
    let mut break_at: Option<usize> = None;

    for seg in segs {
        for c in seg.t.chars() {
            if c == '\n' {
                // 硬换行: 当前行直接收尾
                raw_lines.push(std::mem::take(&mut cur));
                curw = 0;
                break_at = None;
                continue;
            }
            let w = c.width().unwrap_or(0);
            if !cur.is_empty() && curw + w > width {
                // 断行优先级: 紧邻边界可断(相邻两字符任一是宽字符)→ 断在当前原子前;
                // 否则用之前记录的机会(空格之后等); 都没有 → 硬切。
                let at_boundary = matches!(
                    cur.last(),
                    Some(&(_, prev)) if prev.width().unwrap_or(0) == 2 || w == 2
                );
                let bp = if at_boundary { Some(cur.len() - 1) } else { break_at };
                match bp {
                    Some(bp) => {
                        let rest = cur.split_off(bp + 1);
                        let keep = std::mem::replace(&mut cur, rest);
                        raw_lines.push(keep);
                        curw = cur.iter().map(|&(_, ch)| ch.width().unwrap_or(0)).sum();
                        break_at = recompute_break(&cur);
                    }
                    None => {
                        // 无断行机会: 硬切
                        raw_lines.push(std::mem::take(&mut cur));
                        curw = 0;
                        break_at = None;
                    }
                }
            }
            if c == ' ' {
                break_at = Some(cur.len());
            } else if let Some(&(_, prev)) = cur.last() {
                if prev.width().unwrap_or(0) == 2 || w == 2 {
                    break_at = Some(cur.len() - 1);
                }
            }
            cur.push((seg.s, c));
            curw += w;
        }
    }
    if !cur.is_empty() {
        raw_lines.push(cur);
    }

    // 相邻同样式原子并回 Seg
    let mut out = vec![];
    for l in raw_lines {
        let mut line = vec![];
        for (s, c) in l {
            push_seg(&mut line, s, c.to_string());
        }
        out.push(line);
    }
    out
}

/// 重算一行内最后一个断行机会(断行后余下的原子之间可能仍有空格/宽字符)。
fn recompute_break(cur: &[(u8, char)]) -> Option<usize> {
    let mut bp = None;
    for (idx, &(_, c)) in cur.iter().enumerate() {
        if c == ' ' {
            bp = Some(idx);
        } else if idx + 1 < cur.len() {
            let next = cur[idx + 1].1;
            if c.width().unwrap_or(0) == 2 || next.width().unwrap_or(0) == 2 {
                bp = Some(idx);
            }
        }
    }
    bp
}

/// 一行内样式转换成 SGR: 开启/关闭一律成对(1/3/2/36 → 22/23/22/39)。
/// 行尾若有活动样式发 \x1b[0m(断行后新行行首会按段重发)。
fn render_line(segs: &[Seg]) -> String {
    let mut out = String::new();
    let mut active: u8 = 0;
    for seg in segs {
        let off = active & !seg.s;
        let on = seg.s & !active;
        if off & ITALIC != 0 {
            out.push_str("\x1b[23m");
        }
        if off & CYAN != 0 {
            out.push_str("\x1b[39m");
        }
        if off & (BOLD | DIM) != 0 {
            // 22 同时关粗体与 dim: 关掉一个但另一个仍要时重发
            out.push_str("\x1b[22m");
            if seg.s & BOLD != 0 {
                out.push_str("\x1b[1m");
            }
            if seg.s & DIM != 0 {
                out.push_str("\x1b[2m");
            }
        } else {
            if on & BOLD != 0 {
                out.push_str("\x1b[1m");
            }
            if on & DIM != 0 {
                out.push_str("\x1b[2m");
            }
        }
        if on & ITALIC != 0 {
            out.push_str("\x1b[3m");
        }
        if on & CYAN != 0 {
            out.push_str("\x1b[36m");
        }
        out.push_str(&seg.t);
        active = seg.s;
    }
    if active != 0 {
        out.push_str("\x1b[0m");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulldown_cmark::{Options, Parser};

    fn render(md: &str) -> String {
        let parser = Parser::new_ext(md, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH);
        let events: Vec<Event<'static>> = parser.map(|e| e.into_static()).collect();
        render_ansi(&events)
    }

    /// 去掉 SGR 序列后的可见文本(按 \n 分行)。
    fn plain(out: &str) -> Vec<String> {
        out.lines().map(strip_sgr).collect()
    }

    fn strip_sgr(out: &str) -> String {
        let mut s = out.to_string();
        while let Some(i) = s.find('\x1b') {
            let mut j = i + 1;
            while j < s.len() && !s.as_bytes()[j].is_ascii_alphabetic() {
                j += 1;
            }
            s.replace_range(i..=j, "");
        }
        s
    }

    #[test]
    fn wrap_pure_ascii_hard_cut() {
        // 100 个无空格 ASCII: 无断行机会 → 76 硬切 + 24
        let out = render(&format!("{}\n", "a".repeat(100)));
        let lines = plain(&out);
        assert_eq!(lines[0].len(), 76);
        assert_eq!(lines[1], "a".repeat(24));
    }

    #[test]
    fn wrap_cjk_width_two() {
        // CJK 按 2 列计宽: 50 个 → 38 + 12
        let out = render(&format!("{}\n", "中".repeat(50)));
        let lines = plain(&out);
        assert_eq!(lines[0], "中".repeat(38));
        assert_eq!(lines[1], "中".repeat(12));
    }

    #[test]
    fn wrap_at_space() {
        // 8 个 10 字符词: 空格后的断行机会让第 7 个词的第 6 字符超 76 时
        // 断在最后一个空格之后(空格留在行尾): 行 1 = 前 6 词 + 行尾空格
        let words: Vec<String> = (1..=8).map(|i| format!("w{i:09}")).collect();
        let out = render(&format!("{}\n", words.join(" ")));
        let lines = plain(&out);
        assert_eq!(lines[0], format!("{} ", words[..6].join(" ")));
        assert_eq!(lines[1], "w000000007 w000000008");
    }

    #[test]
    fn style_reemission_across_break() {
        // 100 个加粗字符: 断行处行尾 \x1b[0m, 新行行首重发 \x1b[1m
        let out = render(&format!("**{}**\n", "b".repeat(100)));
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].ends_with("\x1b[0m"), "行尾应复位样式: {:?}", lines[0]);
        assert!(lines[1].starts_with("\x1b[1m"), "新行行首应重发粗体: {:?}", lines[1]);
        assert_eq!(strip_sgr(lines[0]).len(), 76);
        assert_eq!(strip_sgr(lines[1]).len(), 24);
    }

    #[test]
    fn code_block_not_wrapped() {
        let long = "c".repeat(100);
        let out = render(&format!("```\n{long}\n```\n"));
        assert!(out.contains(&format!("    {long}")), "代码块应 4 空格缩进且不折行: {out:?}");
    }

    #[test]
    fn list_indent() {
        let out = render("- a\n- b\n");
        let lines = plain(&out);
        assert_eq!(lines[0], "  • a");
        assert_eq!(lines[1], "  • b");
        // 有序列表
        let out = render("1. 一\n2. 二\n");
        let lines = plain(&out);
        assert_eq!(lines[0], "  1. 一");
        assert_eq!(lines[1], "  2. 二");
    }

    #[test]
    fn heading_bold_and_rule_dim() {
        let out = render("# 标题\n\n---\n");
        assert!(out.contains("\x1b[1m标题\x1b[0m"), "标题应加粗(行尾整体复位): {out:?}");
        assert!(out.contains("\x1b[2m----"), "分隔线应 dim: {out:?}");
    }

    #[test]
    fn inline_styles_and_link() {
        let out = render("这是 `code` **粗** *斜* ~~删~~ [文字](https://x.example) 结束\n");
        assert!(out.contains("\x1b[36mcode\x1b[39m"));
        assert!(out.contains("\x1b[1m粗\x1b[22m"));
        assert!(out.contains("\x1b[3m斜\x1b[23m"));
        // 行内样式关闭成对: 链接后还有文本, cyan 以 \x1b[39m 收
        assert!(out.contains("文字\x1b[36m(https://x.example)\x1b[39m"));
        // 链接文字与 url 相同 → 只输出 url
        let out = render("[https://x.example](https://x.example)\n");
        assert!(!out.contains("(https://x.example)"));
    }

    #[test]
    fn blockquote_prefix() {
        let out = render("> 引用一行\n");
        let lines = plain(&out);
        assert_eq!(lines[0], "> 引用一行");
        assert!(out.contains("\x1b[2m> \x1b[22m"));
    }

    #[test]
    fn table_basic() {
        let out = render("| a | b |\n|---|---|\n| 1 | 2 |\n");
        let lines = plain(&out);
        assert_eq!(lines[0], "a | b");
        assert!(lines[1].starts_with("---- | ----"));
        assert_eq!(lines[2], "1 | 2");
    }
}
