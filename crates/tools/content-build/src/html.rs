//! HTML 投影: 镜像页 / 列表页。

use pulldown_cmark::{Event, HeadingLevel, Tag};

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

/// 正文 HTML: 若事件流的第一个块级元素是 H1(即元数据标题的来源),
/// 从投影中剥掉这个 H1 块(模板自己渲染 <h1>), 其余事件喂 push_html。
/// push_html 对 Text 转义, 但对 InlineHtml/Html 事件是原样透传 ——
/// 所以先自己转义一遍(安全红线: HTML 输出全转义)。
pub fn body_html(events: &[Event<'static>]) -> String {
    let mut out = String::new();
    let mut iter = events.iter().cloned().map(escape_raw_html);
    if matches!(
        events.first(),
        Some(Event::Start(Tag::Heading { level: HeadingLevel::H1, .. }))
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

/// 把内联/块级原始 HTML 事件内容预转义(push_html 对它们原样透传)。
fn escape_raw_html(ev: Event<'static>) -> Event<'static> {
    match ev {
        Event::InlineHtml(s) => Event::InlineHtml(attr_escape(&s).into()),
        Event::Html(s) => Event::Html(attr_escape(&s).into()),
        other => other,
    }
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
        loop {
            let Some(start) = s.find(&open) else { break };
            let Some(rel_end) = s[start..].find("{{/if}}") else { break };
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
  {{#if site_url}}<link rel="canonical" href="{{SITE_URL}}/blog/{{SLUG}}/" />{{/if}}
  <meta name="description" content="{{EXCERPT}}" />
  <meta name="termblog-slug" content="{{SLUG}}" />
  <meta property="og:title" content="{{TITLE}}" />
  {{#if site_url}}<meta property="og:url" content="{{SITE_URL}}/blog/{{SLUG}}/" />{{/if}}
  <meta property="og:type" content="article" />
  <meta property="og:description" content="{{EXCERPT}}" />
  <meta property="og:site_name" content="{{SITE_TITLE}}" />
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
          {{DATE}}{{#if ssh_hint}} · 终端里也可以读: ssh -p 2222 blog@{{HOST}} 然后敲 blog {{SLUG}}{{/if}}
        </footer>
      </article>
    </div>
    <!-- 等待层: JS 用户首屏只看到它(不透明盖住静态正文, 正文不闪现);
         收到 blog 命令的 OSC(内容已在画)后淡出, 露出已就绪的终端 -->
    <div id="mirror-cover">
      <p class="mirror-status">正在接入真实终端…</p>
    </div>
    <!-- 终端层: 与首页同构, 等待层不透明地盖在上面; 需保持正常布局
         (不能 display:none, 否则 FitAddon 量不到尺寸) -->
    <div id="term-host">
      <div id="term-screen"></div>
    </div>
  </div>
  <button id="enter-terminal" type="button" hidden>进入终端 ↵</button>
  <script type="module" src="/assets/{{ENTRY_JS}}"></script>
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

/// 渲染单篇镜像页。
pub fn render_mirror_page(
    a: &Article,
    entry_js: &str,
    entry_css: &str,
    site_url: Option<&str>,
    site_title: &str,
) -> String {
    let body = body_html(&a.events);
    let host = site_url.map(host_of).unwrap_or("");
    let ssh_hint = site_url.is_some();
    fill(
        MIRROR_TEMPLATE,
        &[("site_url", site_url.is_some()), ("ssh_hint", ssh_hint)],
        &[
            ("TITLE", attr_escape(&a.title)),
            ("SITE_TITLE", attr_escape(site_title)),
            ("SITE_URL", attr_escape(site_url.unwrap_or(""))),
            ("SLUG", attr_escape(&a.slug)),
            ("EXCERPT", attr_escape(&a.excerpt)),
            ("DATE", a.date10.clone()),
            ("HOST", attr_escape(host)),
            ("ENTRY_JS", attr_escape(entry_js)),
            ("ENTRY_CSS", attr_escape(entry_css)),
            ("BLOG_CSS", blog_css_href(entry_js)),
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
  <title>文章 — {{SITE_TITLE}}</title>
  <link rel="icon" type="image/svg+xml" href="/favicon.svg" />
  <link rel="stylesheet" href="/style.css" />
  <link rel="stylesheet" href="{{BLOG_CSS}}" />
  {{#if site_url}}<link rel="alternate" type="application/atom+xml" title="{{SITE_TITLE}}" href="/atom.xml" />{{/if}}
</head>

<body>
  <main id="blog-list">
    <h1>文章</h1>
    <ul>
{{ITEMS}}
    </ul>
    <p><a href="/">← 回到终端</a></p>
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
            "      <li><a href=\"/blog/{}/\">{}</a> <span class=\"date\">{}</span></li>\n",
            a.slug,
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

    fn article(slug: &str, title: &str, md: &str) -> Article {
        Article {
            slug: slug.into(),
            title: title.into(),
            excerpt: "摘要".into(),
            date10: "2026-08-29".into(),
            date_rfc3339: "2026-08-29T10:00:00+08:00".into(),
            date_warned: false,
            events: parse(md),
        }
    }

    #[test]
    fn body_strips_first_h1() {
        let b = body_html(&parse("# 标题\n\n正文段落\n"));
        assert!(!b.contains("<h1>"), "首个 H1 应被剥离, 实际: {b}");
        assert!(b.contains("<p>正文段落</p>"));
    }

    #[test]
    fn body_keeps_later_headings() {
        let b = body_html(&parse("正文\n\n# 后出现的 H1\n\n尾段\n"));
        assert!(b.contains("<h1>后出现的 H1</h1>"), "非首块的 H1 应保留: {b}");
    }

    #[test]
    fn body_escapes_html() {
        let b = body_html(&parse("小心 <script>alert(1)</script> 注入\n"));
        assert!(b.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!b.contains("<script>"));
    }

    #[test]
    fn mirror_page_fills_all_placeholders() {
        let a = article("hello", "你好, 世界", "# 你好, 世界\n\n正文\n");
        let page = render_mirror_page(&a, "index-abc123.js", "index-abc123.css", Some("https://blog.example.com"), "~yzs");
        assert!(!page.contains("{{"), "模板残留占位符: {page}");
        assert!(page.contains("<h1>你好, 世界</h1>"));
        assert!(page.contains("name=\"termblog-slug\" content=\"hello\""));
        assert!(page.contains("href=\"https://blog.example.com/blog/hello/\""));
        assert!(page.contains("src=\"/assets/index-abc123.js\""));
        assert!(page.contains("href=\"/assets/index-abc123.css\""), "镜像页必须引打包样式(xterm.css): {page}");
        assert!(page.contains("ssh -p 2222 blog@blog.example.com"));
        // 等待层 + 无 JS 兜底: 正文留在 DOM, cover 由 noscript 对无 JS 隐藏
        assert!(page.contains("id=\"mirror-cover\""));
        assert!(page.contains("正在接入真实终端…"));
        assert!(page.contains("noscript") && page.contains("#mirror-cover{display:none}"));
        assert!(page.contains("id=\"static-view\"") && page.contains("<article>"));
        // 无 site_url: 无 canonical / ssh_hint / og:url
        let page = render_mirror_page(&a, "index-abc123.js", "index-abc123.css", None, "~yzs");
        assert!(!page.contains("canonical"));
        assert!(!page.contains("og:url"));
        assert!(!page.contains("ssh -p 2222"));
    }

    #[test]
    fn mirror_page_escapes_title_attr() {
        let a = article("hello", "含 \"引号\" & <标签>", "# 含 \"引号\" & <标签>\n\n正文\n");
        let page = render_mirror_page(&a, "index-abc123.js", "index-abc123.css", None, "~yzs");
        assert!(page.contains("&quot;引号&quot; &amp; &lt;标签&gt;"));
    }

    #[test]
    fn list_page_content() {
        let a = article("hello", "你好, 世界", "# 你好, 世界\n\n正文\n");
        let page = render_list_page(std::slice::from_ref(&a), Some("https://blog.example.com"), "~yzs", "index-abc123.js");
        assert!(page.contains("<title>文章 — ~yzs</title>"));
        assert!(page.contains("<a href=\"/blog/hello/\">你好, 世界</a>"));
        assert!(page.contains("atom.xml"));
        assert!(!page.contains("assets/"), "列表页不引 entry JS");
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
