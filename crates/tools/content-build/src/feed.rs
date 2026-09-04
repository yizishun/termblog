//! sitemap.xml / atom.xml / robots.txt。URL 一律尾斜杠(canonical 形态)。

use crate::Article;

/// XML 转义: `& < > " '` → 实体。所有进 XML 文本/属性的值先过这里,
/// 保证 `xmllint --noout` 通过。
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// 剔除 XML 1.0 非法的控制字符(摘要等自由文本里可能出现)。
fn strip_control(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// sitemap.xml: 首页 + 列表页(无 lastmod)+ 每篇文章(带 lastmod)。
pub fn sitemap(site_url: &str, arts: &[Article]) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n",
    );
    out.push_str(&format!(
        "  <url><loc>{}/</loc></url>\n",
        xml_escape(site_url)
    ));
    out.push_str(&format!(
        "  <url><loc>{}/blog/</loc></url>\n",
        xml_escape(site_url)
    ));
    for a in arts {
        out.push_str(&format!(
            "  <url><loc>{}{}</loc><lastmod>{}</lastmod></url>\n",
            xml_escape(site_url),
            xml_escape(&a.path.route),
            a.date10
        ));
    }
    out.push_str("</urlset>\n");
    out
}

/// atom.xml: feed 元数据 + 每篇一个 entry(content = 转义后的完整 HTML)。
pub fn atom(site_url: &str, site_title: &str, arts: &[Article], feed_updated: &str) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<feed xmlns=\"http://www.w3.org/2005/Atom\">\n",
    );
    out.push_str(&format!("  <title>{}</title>\n", xml_escape(site_title)));
    out.push_str(&format!("  <id>{}/</id>\n", xml_escape(site_url)));
    out.push_str(&format!(
        "  <updated>{}</updated>\n",
        xml_escape(feed_updated)
    ));
    out.push_str(&format!("  <link href=\"{}/\" />\n", xml_escape(site_url)));
    out.push_str(&format!(
        "  <link href=\"{}/atom.xml\" rel=\"self\" />\n",
        xml_escape(site_url)
    ));
    out.push_str(&format!(
        "  <author><name>{}</name></author>\n",
        xml_escape(site_title)
    ));
    for a in arts {
        let body = crate::html::body_html(&a.events, &a.image_meta);
        out.push_str("  <entry>\n");
        out.push_str(&format!("    <title>{}</title>\n", xml_escape(&a.title)));
        out.push_str(&format!(
            "    <id>{}{}</id>\n",
            xml_escape(site_url),
            xml_escape(&a.path.route)
        ));
        out.push_str(&format!(
            "    <link href=\"{}{}\" />\n",
            xml_escape(site_url),
            xml_escape(&a.path.route)
        ));
        out.push_str(&format!(
            "    <updated>{}</updated>\n",
            xml_escape(&a.date_rfc3339)
        ));
        out.push_str(&format!(
            "    <summary>{}</summary>\n",
            xml_escape(&strip_control(&a.excerpt))
        ));
        out.push_str(&format!(
            "    <content type=\"html\">{}</content>\n",
            xml_escape(&body)
        ));
        out.push_str("  </entry>\n");
    }
    out.push_str("</feed>\n");
    out
}

/// robots.txt: 总是产出; 有 site_url 时附 Sitemap 行。
pub fn robots(site_url: Option<&str>) -> String {
    let mut out = String::from("User-agent: *\nAllow: /\n");
    if let Some(u) = site_url {
        out.push_str(&format!("Sitemap: {u}/sitemap.xml\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulldown_cmark::{Options, Parser};

    fn article(key: &str, title: &str, excerpt: &str, md: &str) -> Article {
        let events = Parser::new_ext(md, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH)
            .map(|e| e.into_static())
            .collect();
        crate::Article {
            path: termblog_content_model::ArticlePath::parse(&format!("{key}.md")).unwrap(),
            title: title.into(),
            excerpt: excerpt.into(),
            date10: "2026-08-29".into(),
            date_rfc3339: "2026-08-29T10:00:00+08:00".into(),
            date_warned: false,
            events,
            image_meta: Default::default(),
            dest_paths: Default::default(),
            first_image: None,
            comments: None,
        }
    }

    #[test]
    fn xml_escape_all() {
        assert_eq!(xml_escape("&<>\"'"), "&amp;&lt;&gt;&quot;&apos;");
        assert_eq!(
            xml_escape("<a href=\"x\">'y'</a>&"),
            "&lt;a href=&quot;x&quot;&gt;&apos;y&apos;&lt;/a&gt;&amp;"
        );
    }

    #[test]
    fn sitemap_trailing_slash_urls() {
        let a = article("hello", "你好", "摘", "# 你好\n");
        let s = sitemap("https://blog.example.com", &[a]);
        assert!(s.contains("<loc>https://blog.example.com/</loc>"));
        assert!(s.contains("<loc>https://blog.example.com/blog/</loc>"));
        assert!(
            s.contains("<loc>https://blog.example.com/hello/</loc><lastmod>2026-08-29</lastmod>"),
            "文章 URL 应带尾斜杠: {s}"
        );
    }

    #[test]
    fn atom_content_escaped_html() {
        let a = article("hello", "你好", "摘", "# 你好\n\n正文 <b>加粗</b>\n");
        let s = atom(
            "https://blog.example.com",
            "~yzs",
            &[a],
            "2026-08-29T10:00:00+08:00",
        );
        assert!(s.contains("<entry>"));
        assert!(s.contains("<id>https://blog.example.com/hello/</id>"));
        assert!(s.contains(
            "<content type=\"html\">&lt;p&gt;正文 &amp;lt;b&amp;gt;加粗&amp;lt;/b&amp;gt;&lt;/p&gt;\n</content>"
        ));
        assert!(s.contains("<updated>2026-08-29T10:00:00+08:00</updated>"));
    }

    #[test]
    fn atom_strips_control_chars_in_summary() {
        let a = article("hello", "你好", "摘\u{07}要", "# 你好\n");
        let s = atom(
            "https://blog.example.com",
            "~yzs",
            &[a],
            "2026-08-29T10:00:00+08:00",
        );
        assert!(s.contains("<summary>摘要</summary>"));
    }

    #[test]
    fn robots_with_and_without_site_url() {
        assert_eq!(
            robots(Some("https://blog.example.com")),
            "User-agent: *\nAllow: /\nSitemap: https://blog.example.com/sitemap.xml\n"
        );
        assert_eq!(robots(None), "User-agent: *\nAllow: /\n");
    }
}
