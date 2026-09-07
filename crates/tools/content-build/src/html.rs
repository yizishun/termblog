//! HTML 投影: 镜像页 / 列表页。

use std::collections::HashMap;

use pulldown_cmark::{Event, HeadingLevel, Tag, TagEnd};

use crate::Article;

/// HTML 属性转义(& < > ")。所有进模板占位符的值先过这里,
/// 正文 HTML 则由 pulldown-cmark 的 push_html 保证转义。
pub fn attr_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// 正文 HTML: 两遍处理 ——
/// 1. 预处理: 原始 Html/InlineHtml 事件照旧转义(安全红线: HTML 输出全转义);
///    Image 事件(Start..End)消费掉, 换成手写的 <img> InlineHtml(可信生成物,
///    不再过转义, 否则会被转义成文本)。
/// 2. 若第一个块级元素是 H1(即元数据标题的来源)则整块剥掉, 其余喂 push_html。
///
/// image_meta: 本地图 dest_url 原文 → (宽, 高, 站点绝对路径); 查不到 = 外链(原样, 无宽高)。
pub fn body_html(
    events: &[Event<'static>],
    image_meta: &HashMap<String, (u32, u32, String)>,
) -> String {
    let mut out = String::new();
    let pre = preprocess_events(events, image_meta);
    let mut iter = pre.into_iter();
    if matches!(
        events.first(),
        Some(Event::Start(Tag::Heading {
            level: HeadingLevel::H1,
            ..
        }))
    ) {
        // 剥掉首块 H1: 从 Start(H1) 到与之配对的 End(H1) 整块丢弃
        let mut depth = 0usize;
        for ev in iter.by_ref() {
            match &ev {
                Event::Start(_) => depth += 1,
                Event::End(_) => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    pulldown_cmark::html::push_html(&mut out, iter);
    out
}

/// 预处理事件流: 转义原始 HTML 事件; Image 事件换成手写 <img> 的 InlineHtml。
fn preprocess_events(
    events: &[Event<'static>],
    image_meta: &HashMap<String, (u32, u32, String)>,
) -> Vec<Event<'static>> {
    let mut out = Vec::with_capacity(events.len());
    let mut i = 0;
    while i < events.len() {
        match &events[i] {
            Event::Start(Tag::Image {
                dest_url, title, ..
            }) => {
                // 收集 alt 纯文本(Text/Code 拼接, 换行按空格; 嵌套样式容器拍平)
                let mut alt = String::new();
                let mut j = i + 1;
                while j < events.len() {
                    match &events[j] {
                        Event::End(TagEnd::Image) => break,
                        Event::Text(t) => alt.push_str(t),
                        Event::Code(c) => alt.push_str(c),
                        Event::SoftBreak | Event::HardBreak => alt.push(' '),
                        _ => {}
                    }
                    j += 1;
                }
                let tag = build_img_tag(dest_url, title, alt.trim(), image_meta);
                out.push(Event::InlineHtml(tag.into()));
                i = if j < events.len() { j + 1 } else { j }; // 跳过 End(Image)
            }
            Event::InlineHtml(s) => {
                out.push(Event::InlineHtml(attr_escape(s).into()));
                i += 1;
            }
            Event::Html(s) => {
                out.push(Event::Html(attr_escape(s).into()));
                i += 1;
            }
            other => {
                out.push(other.clone());
                i += 1;
            }
        }
    }
    out
}

/// 手写 <img>: src 用重写后的 content-relative 站点路径(外链原样); 本地图带真实宽高;
/// 所有属性值过 attr_escape。
fn build_img_tag(
    dest_url: &str,
    title: &str,
    alt: &str,
    image_meta: &HashMap<String, (u32, u32, String)>,
) -> String {
    let (src, dims) = match image_meta.get(dest_url) {
        Some((w, h, url)) => (url.as_str(), Some((*w, *h))),
        None => (dest_url, None), // 外链: 不进表, 原样输出, 省略宽高
    };
    let mut tag = format!(
        "<img src=\"{}\" alt=\"{}\"",
        attr_escape(src),
        attr_escape(alt)
    );
    if let Some((w, h)) = dims {
        tag.push_str(&format!(" width=\"{w}\" height=\"{h}\""));
    }
    if !title.is_empty() {
        tag.push_str(&format!(" title=\"{}\"", attr_escape(title)));
    }
    tag.push_str(" loading=\"lazy\" decoding=\"async\" />");
    tag
}

/// site_url(https://host[:port], 无尾斜杠)的 host 部分(去 scheme、路径与端口)。
fn host_of(site_url: &str) -> &str {
    let rest = site_url.split("://").nth(1).unwrap_or(site_url);
    let rest = rest.split('/').next().unwrap_or(rest);
    match rest.rfind(':') {
        Some(idx) => &rest[..idx],
        None => rest,
    }
}

/// 模板填充: 先处理 `{{#if NAME}}…{{/if}}` 条件块(关闭时整块删除),
/// 再替换 `{{KEY}}` 占位符。占位符值不再二次扫描(正文里的字面 `{{` 安全)。
fn fill(tpl: &str, conds: &[(&str, bool)], vars: &[(&str, String)]) -> String {
    let mut s = tpl.to_string();
    for (name, on) in conds {
        let open = format!("{{{{#if {name}}}}}");
        while let Some(start) = s.find(&open) {
            let Some(rel_end) = s[start..].find("{{/if}}") else {
                break;
            };
            let end = start + rel_end;
            if *on {
                let inner = s[start + open.len()..end].to_string();
                s.replace_range(start..end + "{{/if}}".len(), &inner);
            } else {
                s.replace_range(start..end + "{{/if}}".len(), "");
            }
        }
    }
    for (k, v) in vars {
        s = s.replace(&format!("{{{{{k}}}}}"), v);
    }
    s
}

/// 镜像页模板(§8.4 定案基础上按人类需求调整: JS 用户先看等待层而非静态正文)。
/// 占位符 {{…}}, {{#if site_url}}/{{#if ssh_hint}} 条件块。
///
/// 布局: 静态全文(爬虫 / 无 JS 访客直接可读)→ `#mirror-cover` 不透明等待层
/// 盖在正文之上(JS 用户首屏只看到"正在接入真实终端…", 正文不闪现)→ 终端就绪
/// 后等待层淡出、正文层隐藏、终端接管; 终端失败时等待层撤掉露出正文(降级可读)。
/// 所有 UA 拿到同一份 HTML(无 cloaking); 正文始终留在 DOM 里。
const MIRROR_TEMPLATE: &str = r#"<!doctype html>
<html lang="zh-CN">

<head>
  <meta charset="UTF-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1.0" />
  <title>{{TITLE}} — {{SITE_TITLE}}</title>
  <link rel="icon" type="image/svg+xml" href="/favicon.svg" />
  <link rel="stylesheet" href="/style.css" />
  <!-- 打包样式(xterm.css 等, vite 产物, 带 hash): 缺了它镜像页的终端 DOM 会
       裸奔 —— 裸露可拉大的 textarea 与无样式文本层 -->
  <link rel="stylesheet" href="/assets/{{ENTRY_CSS}}" />
  <link rel="stylesheet" href="{{BLOG_CSS}}" />
  <!-- 无 JS: 不显示等待层, 静态正文即全部视图 -->
  <noscript><style>#mirror-cover{display:none}</style></noscript>
  {{#if site_url}}<link rel="canonical" href="{{SITE_URL}}{{ROUTE}}" />{{/if}}
  <meta name="description" content="{{EXCERPT}}" />
  <meta name="termblog-source" content="{{SOURCE}}" />
  <meta name="termblog-route" content="{{ROUTE}}" />
  <meta property="og:title" content="{{TITLE}}" />
  {{#if site_url}}<meta property="og:url" content="{{SITE_URL}}{{ROUTE}}" />{{/if}}
  <meta property="og:type" content="article" />
  <meta property="og:description" content="{{EXCERPT}}" />
  <meta property="og:site_name" content="{{SITE_TITLE}}" />
  {{#if og_image}}<meta property="og:image" content="{{SITE_URL}}{{OG_IMAGE}}" />{{/if}}
  {{#if site_url}}<link rel="alternate" type="application/atom+xml" title="{{SITE_TITLE}}" href="/atom.xml" />{{/if}}
</head>

<body>
  <!-- CRT 滤镜 defs: 与 frontend/index.html 中的完全一致(style.css 对
       #term-screen 应用 url(#crt-smudge), 缺 defs 会导致滤镜引用失效) -->
  <svg class="defs-only" aria-hidden="true" focusable="false">
    <defs>
      <filter id="crt-smudge" color-interpolation-filters="sRGB"
              x="0" y="0" width="100%" height="100%">
        <feTurbulence type="fractalNoise" baseFrequency="0.01" numOctaves="1" seed="5" result="noise"></feTurbulence>
        <feDisplacementMap in="SourceGraphic" in2="noise" scale="4"
                           xChannelSelector="R" yChannelSelector="G"></feDisplacementMap>
      </filter>
    </defs>
  </svg>
  <div id="app">
    <!-- 静态正文层: 爬虫与无 JS 访客的完整视图; JS 用户由 #mirror-cover 盖住,
         终端就绪后隐藏本层由终端接管(需保持正常布局, 不能 display:none 初始,
         否则 FitAddon 量不到尺寸) -->
    <div id="static-view">
      <article>
        <h1>{{TITLE}}</h1>
        {{BODY_HTML}}
        <footer class="post-meta">
          {{DATE}}{{#if ssh_hint}} · Also readable in terminal: {{SSH_CMD}} then run blog {{KEY}}{{/if}}
        </footer>
        {{#if comments}}<section class="comments" data-comments-target="{{COMMENT_TARGET}}" data-comments-fifo="~/{{COMMENT_FIFO}}">
          <h2>Comments</h2>
          <p class="comments-status">Loading…</p>
          <ol class="comment-list"></ol>
        </section>{{/if}}
      </article>
    </div>
    <!-- 等待层: JS 用户首屏只看到它(不透明盖住静态正文, 正文不闪现);
         收到 blog 命令的 OSC(内容已在画)后淡出, 露出已就绪的终端 -->
    <div id="mirror-cover">
      <p class="mirror-status">Connecting to real terminal…</p>
    </div>
    <!-- 终端层: 与首页同构, 等待层不透明地盖在上面; 需保持正常布局
         (不能 display:none, 否则 FitAddon 量不到尺寸) -->
    <div id="term-host">
      <div id="term-screen"></div>
    </div>
  </div>
  <script type="module" src="/assets/{{ENTRY_JS}}"></script>
  {{#if comments}}<script type="module" src="/assets/{{COMMENTS_JS}}"></script>{{/if}}
</body>

</html>
"#;

/// blog.css 引用: 带 entry JS 的 hash 做版本参数, 前端重新构建后
/// 浏览器不会再用缓存的旧样式(等待层等新样式依赖它)。
fn blog_css_href(entry_js: &str) -> String {
    if entry_js.is_empty() {
        "/blog.css".into()
    } else {
        format!("/blog.css?v={entry_js}")
    }
}

/// 渲染单篇镜像页。first_image: 文章第一张本地图的站点绝对路径(og:image 用,
/// 需同时有 site_url 才输出)。ssh_port: termblog-ssh 监听端口(22 是 ssh 默认
/// 端口, 提示里省略 -p, 访客命令最简)。
pub fn render_mirror_page(
    a: &Article,
    entry_js: &str,
    entry_css: &str,
    comments_js: &str,
    site_url: Option<&str>,
    ssh_port: u16,
    site_title: &str,
    first_image: Option<&str>,
) -> String {
    let body = body_html(&a.events, &a.image_meta);
    let host = site_url.map(host_of).unwrap_or("");
    let ssh_hint = site_url.is_some();
    // 终端阅读提示的 ssh 命令(端口来自 [ssh] listen, 不再写死)
    let ssh_cmd = if ssh_port == 22 {
        format!("ssh blog@{host}")
    } else {
        format!("ssh -p {ssh_port} blog@{host}")
    };
    let og_image = site_url.is_some() && first_image.is_some();
    fill(
        MIRROR_TEMPLATE,
        &[
            ("site_url", site_url.is_some()),
            ("ssh_hint", ssh_hint),
            ("og_image", og_image),
            ("comments", a.comments.is_some()),
        ],
        &[
            ("TITLE", attr_escape(&a.title)),
            ("SITE_TITLE", attr_escape(site_title)),
            ("SITE_URL", attr_escape(site_url.unwrap_or(""))),
            ("SOURCE", attr_escape(&a.path.source_rel.to_string_lossy())),
            ("ROUTE", attr_escape(&a.path.route)),
            ("KEY", attr_escape(&a.path.key)),
            (
                "COMMENT_TARGET",
                attr_escape(a.comments.as_ref().map_or("", |c| c.target.as_str())),
            ),
            (
                "COMMENT_FIFO",
                attr_escape(a.comments.as_ref().map_or("", |c| c.fifo_rel.as_str())),
            ),
            ("EXCERPT", attr_escape(&a.excerpt)),
            ("DATE", a.date10.clone()),
            ("SSH_CMD", attr_escape(&ssh_cmd)),
            ("ENTRY_JS", attr_escape(entry_js)),
            ("ENTRY_CSS", attr_escape(entry_css)),
            ("COMMENTS_JS", attr_escape(comments_js)),
            ("BLOG_CSS", blog_css_href(entry_js)),
            ("OG_IMAGE", attr_escape(first_image.unwrap_or(""))),
            ("BODY_HTML", body),
        ],
    )
}

/// 列表页模板(§8.7): 纯静态, 不引导终端(不引 entry JS)。
const LIST_TEMPLATE: &str = r#"<!doctype html>
<html lang="zh-CN">

<head>
  <meta charset="UTF-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1.0" />
  <title>Articles — {{SITE_TITLE}}</title>
  <link rel="icon" type="image/svg+xml" href="/favicon.svg" />
  <link rel="stylesheet" href="/style.css" />
  <link rel="stylesheet" href="{{BLOG_CSS}}" />
  {{#if site_url}}<link rel="alternate" type="application/atom+xml" title="{{SITE_TITLE}}" href="/atom.xml" />{{/if}}
</head>

<body>
  <main id="blog-list">
    <h1>Articles</h1>
    <ul>
{{ITEMS}}
    </ul>
    <p><a href="/">← Back to terminal</a></p>
  </main>
</body>

</html>
"#;

/// 渲染 /blog/ 列表页。
pub fn render_list_page(
    arts: &[Article],
    site_url: Option<&str>,
    site_title: &str,
    entry_js: &str,
) -> String {
    let mut items = String::new();
    for a in arts {
        items.push_str(&format!(
            "      <li><a href=\"{}\">{}</a> <span class=\"date\">{}</span></li>\n",
            a.path.route,
            attr_escape(&a.title),
            a.date10
        ));
    }
    fill(
        LIST_TEMPLATE,
        &[("site_url", site_url.is_some())],
        &[
            ("SITE_TITLE", attr_escape(site_title)),
            ("ITEMS", items),
            ("BLOG_CSS", blog_css_href(entry_js)),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulldown_cmark::{Options, Parser};

    fn parse(md: &str) -> Vec<Event<'static>> {
        Parser::new_ext(md, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH)
            .map(|e| e.into_static())
            .collect()
    }

    fn article(key: &str, title: &str, md: &str) -> Article {
        let path = termblog_content_model::ArticlePath::parse(&format!("{key}.md")).unwrap();
        let comments =
            termblog_content_model::CommentAttachment::from_directory_rel(&path.directory_rel).ok();
        Article {
            path,
            title: title.into(),
            excerpt: "摘要".into(),
            date10: "2026-08-29".into(),
            date_rfc3339: "2026-08-29T10:00:00+08:00".into(),
            date_warned: false,
            events: parse(md),
            image_meta: HashMap::new(),
            dest_paths: Default::default(),
            first_image: None,
            comments,
        }
    }

    #[test]
    fn body_strips_first_h1() {
        let b = body_html(&parse("# 标题\n\n正文段落\n"), &HashMap::new());
        assert!(!b.contains("<h1>"), "首个 H1 应被剥离, 实际: {b}");
        assert!(b.contains("<p>正文段落</p>"));
    }

    #[test]
    fn body_keeps_later_headings() {
        let b = body_html(&parse("正文\n\n# 后出现的 H1\n\n尾段\n"), &HashMap::new());
        assert!(
            b.contains("<h1>后出现的 H1</h1>"),
            "非首块的 H1 应保留: {b}"
        );
    }

    #[test]
    fn body_escapes_html() {
        let b = body_html(
            &parse("小心 <script>alert(1)</script> 注入\n"),
            &HashMap::new(),
        );
        assert!(b.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!b.contains("<script>"));
    }

    #[test]
    fn body_image_rewritten_with_dims() {
        // 本地图: src 重写为 content-relative 站点绝对路径, 带宽高/lazy/decoding
        let mut meta = HashMap::new();
        meta.insert(
            "hello/arch.png".to_string(),
            (1080u32, 640u32, "/blog/hello/arch.png".to_string()),
        );
        let b = body_html(&parse("![架构图](hello/arch.png)\n"), &meta);
        assert!(
            b.contains(
                "<img src=\"/blog/hello/arch.png\" alt=\"架构图\" width=\"1080\" height=\"640\" loading=\"lazy\" decoding=\"async\" />"
            ),
            "本地图应输出完整 <img>: {b}"
        );
    }

    #[test]
    fn body_image_alt_escaped() {
        let mut meta = HashMap::new();
        meta.insert(
            "x.png".to_string(),
            (2u32, 2u32, "/blog/t/x.png".to_string()),
        );
        let b = body_html(&parse("![含 \"引号\" & <标签>](x.png)\n"), &meta);
        assert!(
            b.contains("alt=\"含 &quot;引号&quot; &amp; &lt;标签&gt;\""),
            "alt 应转义: {b}"
        );
        // title 同理
        let b = body_html(&parse("![a](x.png \"ti<b>tle\")\n"), &meta);
        assert!(b.contains("title=\"ti&lt;b&gt;tle\""), "title 应转义: {b}");
    }

    #[test]
    fn body_external_image_no_dims() {
        let b = body_html(
            &parse("![外链](https://cdn.example.com/a.png)\n"),
            &HashMap::new(),
        );
        assert!(
            b.contains("<img src=\"https://cdn.example.com/a.png\" alt=\"外链\""),
            "外链原样: {b}"
        );
        assert!(!b.contains("width="), "外链无宽高属性: {b}");
        // alt 里的嵌套样式被拍平
        let b = body_html(
            &parse("![**粗** `码`](https://x.example/a.png)\n"),
            &HashMap::new(),
        );
        assert!(b.contains("alt=\"粗 码\""), "alt 嵌套应拍平: {b}");
    }

    #[test]
    fn mirror_page_og_image() {
        let mut a = article("hello", "你好", "# 你好\n\n![图](hello/x.png)\n");
        a.image_meta
            .insert("hello/x.png".into(), (2, 2, "/blog/hello/x.png".into()));
        a.first_image = Some("/blog/hello/x.png".into());
        let page = render_mirror_page(
            &a,
            "index-abc123.js",
            "index-abc123.css",
            "comments-abc123.js",
            Some("https://blog.example.com"),
            2222,
            "~yzs",
            a.first_image.as_deref(),
        );
        assert!(
            page.contains("<meta property=\"og:image\" content=\"https://blog.example.com/blog/hello/x.png\" />"),
            "有 site_url 且有首图应出 og:image: {page}"
        );
        // 无 site_url → 不出 og:image
        let page = render_mirror_page(
            &a,
            "index-abc123.js",
            "index-abc123.css",
            "comments-abc123.js",
            None,
            2222,
            "~yzs",
            a.first_image.as_deref(),
        );
        assert!(
            !page.contains("og:image"),
            "无 site_url 不出 og:image: {page}"
        );
        // 有 site_url 但无首图 → 不出
        let b = article("plain", "纯文本", "# 纯文本\n\n正文\n");
        let page = render_mirror_page(
            &b,
            "index-abc123.js",
            "index-abc123.css",
            "comments-abc123.js",
            Some("https://blog.example.com"),
            2222,
            "~yzs",
            None,
        );
        assert!(!page.contains("og:image"), "无首图不出 og:image: {page}");
    }

    #[test]
    fn mirror_page_fills_all_placeholders() {
        let a = article("hello", "你好, 世界", "# 你好, 世界\n\n正文\n");
        let page = render_mirror_page(
            &a,
            "index-abc123.js",
            "index-abc123.css",
            "comments-abc123.js",
            Some("https://blog.example.com"),
            2222,
            "~yzs",
            None,
        );
        assert!(!page.contains("{{"), "模板残留占位符: {page}");
        assert!(page.contains("<h1>你好, 世界</h1>"));
        assert!(page.contains("name=\"termblog-source\" content=\"hello.md\""));
        assert!(page.contains("name=\"termblog-route\" content=\"/hello/\""));
        assert!(page.contains("href=\"https://blog.example.com/hello/\""));
        assert!(page.contains("data-comments-target=\"/\""));
        assert!(page.contains("data-comments-fifo=\"~/comment\""));
        assert!(page.contains("src=\"/assets/index-abc123.js\""));
        assert!(
            page.contains("href=\"/assets/index-abc123.css\""),
            "镜像页必须引打包样式(xterm.css): {page}"
        );
        assert!(page.contains("ssh -p 2222 blog@blog.example.com"));
        // 等待层 + 无 JS 兜底: 正文留在 DOM, cover 由 noscript 对无 JS 隐藏
        assert!(page.contains("id=\"mirror-cover\""));
        assert!(page.contains("Connecting to real terminal…"));
        assert!(page.contains("noscript") && page.contains("#mirror-cover{display:none}"));
        assert!(page.contains("id=\"static-view\"") && page.contains("<article>"));
        assert!(
            !page.contains("enter-terminal"),
            "镜像页不应包含文章/终端切换按钮: {page}"
        );
        // 无 site_url: 无 canonical / ssh_hint / og:url
        let page = render_mirror_page(
            &a,
            "index-abc123.js",
            "index-abc123.css",
            "comments-abc123.js",
            None,
            2222,
            "~yzs",
            None,
        );
        assert!(!page.contains("canonical"));
        assert!(!page.contains("og:url"));
        assert!(!page.contains("ssh -p 2222"));
    }

    #[test]
    fn mirror_page_ssh_hint_port_22() {
        let a = article("hello", "你好", "# 你好\n\n正文\n");
        let page = render_mirror_page(
            &a,
            "index-abc123.js",
            "index-abc123.css",
            "comments-abc123.js",
            Some("http://blog.example.com"),
            22,
            "~yzs",
            None,
        );
        assert!(
            page.contains("ssh blog@blog.example.com"),
            "22 端口提示应省略 -p: {page}"
        );
        assert!(!page.contains("-p 22"), "22 端口不应出现 -p 参数: {page}");
    }

    #[test]
    fn mirror_page_escapes_title_attr() {
        let a = article(
            "hello",
            "含 \"引号\" & <标签>",
            "# 含 \"引号\" & <标签>\n\n正文\n",
        );
        let page = render_mirror_page(
            &a,
            "index-abc123.js",
            "index-abc123.css",
            "comments-abc123.js",
            None,
            2222,
            "~yzs",
            None,
        );
        assert!(page.contains("&quot;引号&quot; &amp; &lt;标签&gt;"));
    }

    #[test]
    fn list_page_content() {
        let a = article("hello", "你好, 世界", "# 你好, 世界\n\n正文\n");
        let page = render_list_page(
            std::slice::from_ref(&a),
            Some("https://blog.example.com"),
            "~yzs",
            "index-abc123.js",
        );
        assert!(page.contains("<title>Articles — ~yzs</title>"));
        assert!(page.contains("<a href=\"/hello/\">你好, 世界</a>"));
        assert!(page.contains("atom.xml"));
        assert!(
            !page.contains("src=\"/assets/index-abc123.js\""),
            "列表页不引终端 entry JS"
        );
        assert!(!page.contains("comments-abc123.js"));
        assert!(!page.contains("data-comments-target"));
        let page = render_list_page(std::slice::from_ref(&a), None, "~yzs", "index-abc123.js");
        assert!(!page.contains("atom.xml"));
    }

    #[test]
    fn host_extraction() {
        assert_eq!(host_of("https://blog.example.com"), "blog.example.com");
        assert_eq!(host_of("https://example.com:8080"), "example.com");
        assert_eq!(host_of("http://example.com"), "example.com");
    }
}
