//! content-build —— 内容编译器(termblog M5)。
//!
//! 唯一内容源 `jailtpl/content/blog/*.md` 的两个投影:
//!   HTML 镜像(爬虫读的静态全文页, 进 `frontend/dist/blog/`)与
//!   ANSI 预渲染(终端里 `less -R` 读的排版文本, 进 `jailtpl/content/.rendered/`)。
//! 同时产出文章列表页、sitemap.xml / atom.xml / robots.txt 与首页文章列表注入。
//!
//! 用法(从仓库根):
//!   content-build [--content jailtpl/content] [--dist frontend/dist]
//!                 [--config <file>] [--site-url <url>] [--site-title <title>]
//!
//! site_url 优先级: --site-url > 配置文件 web.site_url > 无(无则跳过 sitemap/atom)。
//! site_title 优先级: --site-title > 配置文件 web.site_title > 内置默认。

mod ansi;
mod feed;
mod html;
mod meta;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use pulldown_cmark::{Event, Options, Parser};

/// 一篇文章的编译中间态: 元数据 + 解析后的事件流。
pub struct Article {
    pub slug: String,
    pub title: String,
    pub excerpt: String,
    /// YYYY-MM-DD(sitemap lastmod / 镜像页 footer / .index)
    pub date10: String,
    /// 完整 RFC3339(atom updated / 排序键)
    pub date_rfc3339: String,
    /// 日期走了 mtime fallback(构建告警用)
    pub date_warned: bool,
    pub events: Vec<Event<'static>>,
}

#[derive(Debug)]
struct Cli {
    content: PathBuf,
    dist: PathBuf,
    config: Option<PathBuf>,
    site_url_arg: Option<String>,
    site_title: Option<String>,
}

fn next_val(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    args.next().ok_or_else(|| anyhow::anyhow!("参数 {name} 需要一个值"))
}

fn parse_cli() -> Result<Cli> {
    let mut cli = Cli {
        content: PathBuf::from("jailtpl/content"),
        dist: PathBuf::from("frontend/dist"),
        config: None,
        site_url_arg: None,
        site_title: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--content" => cli.content = PathBuf::from(next_val(&mut args, "--content")?),
            "--dist" => cli.dist = PathBuf::from(next_val(&mut args, "--dist")?),
            "--config" => cli.config = Some(PathBuf::from(next_val(&mut args, "--config")?)),
            "--site-url" => cli.site_url_arg = Some(next_val(&mut args, "--site-url")?),
            "--site-title" => cli.site_title = Some(next_val(&mut args, "--site-title")?),
            other => bail!("未知参数: {other}"),
        }
    }
    Ok(cli)
}

/// 递归扫描 content/blog/, 收集 *.md 的相对路径(相对 blog 根),
/// 同时收集 slug 违规与非 md 文件告警。
fn scan_blog(
    root: &Path,
    dir: &Path,
    out: &mut Vec<PathBuf>,
    slug_errors: &mut Vec<String>,
    warns: &mut Vec<String>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("扫描目录 {}", dir.display()))? {
        let entry = entry.with_context(|| format!("读目录项 {}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            scan_blog(root, &path, out, slug_errors, warns)?;
        } else {
            let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                let slug = rel
                    .to_string_lossy()
                    .strip_suffix(".md")
                    .unwrap_or_default()
                    .to_string();
                if let Err(e) = meta::validate_slug(&slug) {
                    slug_errors.push(format!("{}: {e}", rel.display()));
                } else {
                    out.push(rel);
                }
            } else {
                warns.push(format!("忽略非 markdown 文件: {}", rel.display()));
            }
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = parse_cli()?;

    // site_url: --site-url > 配置文件 web.site_url > 无
    // site_title: --site-title > 配置文件 web.site_title > 内置默认
    // 配置文件查找复用 termblog_config::Config::load 的机制:
    // --config 指定文件 > $TERMBLOG_CONFIG > /usr/local/etc/termblog.toml(不存在则全默认)。
    let cfg_path = match &cli.config {
        Some(p) => Some(p.clone()),
        None => std::env::var("TERMBLOG_CONFIG").ok().map(PathBuf::from),
    };
    let cfg = termblog_config::Config::load(cfg_path.as_deref())?;
    let site_url = cli
        .site_url_arg
        .or(cfg.web.site_url)
        .map(|u| u.trim_end_matches('/').to_string());
    let site_title = cli.site_title.clone().unwrap_or(cfg.web.site_title.clone());

    // 扫描 + slug 校验(违规全部列出后统一失败)
    let blog_dir = cli.content.join("blog");
    let mut rel_paths: Vec<PathBuf> = vec![];
    let mut slug_errors: Vec<String> = vec![];
    let mut warns: Vec<String> = vec![];
    if blog_dir.is_dir() {
        scan_blog(&blog_dir, &blog_dir, &mut rel_paths, &mut slug_errors, &mut warns)?;
        if rel_paths.is_empty() {
            warns.push(format!("{} 下没有文章", blog_dir.display()));
        }
    } else {
        warns.push(format!("{} 不存在, 没有文章", blog_dir.display()));
    }
    if !slug_errors.is_empty() {
        for e in &slug_errors {
            eprintln!("slug 违规: {e}");
        }
        bail!("slug 校验失败: {} 个文件违规, 请改名后重跑", slug_errors.len());
    }
    rel_paths.sort();

    // 逐篇: 读文件(BOM/CRLF 规范化)→ 解析(两投影共用同一套选项)→ 元数据 → 日期
    let mut arts: Vec<Article> = vec![];
    for rel in &rel_paths {
        let path = blog_dir.join(rel);
        let src = std::fs::read(&path).with_context(|| format!("读文章 {}", path.display()))?;
        let mut text = String::from_utf8(src)
            .with_context(|| format!("文章不是 UTF-8: {}", path.display()))?;
        if let Some(stripped) = text.strip_prefix('\u{feff}') {
            text = stripped.to_string();
        }
        if text.contains('\r') {
            text = text.replace("\r\n", "\n").replace('\r', "\n");
        }
        let parser = Parser::new_ext(&text, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH);
        let events: Vec<Event<'static>> = parser.map(|e| e.into_static()).collect();
        let slug = rel
            .to_string_lossy()
            .strip_suffix(".md")
            .unwrap_or_default()
            .to_string();
        let stem = slug.rsplit('/').next().unwrap_or(&slug).to_string();
        let title = meta::extract_title(&events, &stem);
        let excerpt = meta::extract_excerpt(&events, &title);
        let (date10, date_rfc3339, date_warned) = meta::article_date(&path);
        if date_warned {
            warns.push(format!("「{}」无 git 历史, 日期回退到文件 mtime", rel.display()));
        }
        arts.push(Article { slug, title, excerpt, date10, date_rfc3339, date_warned, events });
    }
    // 排序: 日期倒序, 同日 slug 字典序升序
    arts.sort_by(|a, b| {
        b.date_rfc3339
            .cmp(&a.date_rfc3339)
            .then_with(|| a.slug.cmp(&b.slug))
    });

    // 前端入口产物: 镜像页需要引用带 hash 的文件名(dist/assets/index-*.js /
    // index-*.css, 各恰 1 个)。CSS 含 xterm.css 等打包样式, 缺失会让镜像页的
    // 终端 DOM 裸奔(裸 textarea「输入框」+ 无样式文本层「乱码」), 必须同 JS 一样强制。
    let entry_js = if arts.is_empty() {
        None
    } else {
        let assets = cli.dist.join("assets");
        let mut found: Vec<String> = vec![];
        if assets.is_dir() {
            for e in std::fs::read_dir(&assets)
                .with_context(|| format!("扫描 {}", assets.display()))?
            {
                let name = e?.file_name().to_string_lossy().to_string();
                if name.starts_with("index-") && name.ends_with(".js") {
                    found.push(name);
                }
            }
        }
        match found.len() {
            1 => Some(found.pop().unwrap()),
            0 => bail!(
                "找不到前端入口 JS ({}), 请先 make build-frontend(vite build 先于 content-build)",
                assets.display()
            ),
            n => bail!("找到 {n} 个前端入口 JS, 无法确定用哪个: {found:?}"),
        }
    };
    let entry_css = if arts.is_empty() {
        None
    } else {
        let assets = cli.dist.join("assets");
        let mut found: Vec<String> = vec![];
        if assets.is_dir() {
            for e in std::fs::read_dir(&assets)
                .with_context(|| format!("扫描 {}", assets.display()))?
            {
                let name = e?.file_name().to_string_lossy().to_string();
                if name.starts_with("index-") && name.ends_with(".css") {
                    found.push(name);
                }
            }
        }
        match found.len() {
            1 => Some(found.pop().unwrap()),
            0 => bail!(
                "找不到前端入口 CSS ({}), 请先 make build-frontend(vite build 先于 content-build)",
                assets.display()
            ),
            n => bail!("找到 {n} 个前端入口 CSS, 无法确定用哪个: {found:?}"),
        }
    };

    // 清理旧产物(幂等: 删文后不留僵尸)
    let dist_blog = cli.dist.join("blog");
    if dist_blog.exists() {
        std::fs::remove_dir_all(&dist_blog)
            .with_context(|| format!("清理 {}", dist_blog.display()))?;
    }
    for f in ["sitemap.xml", "atom.xml", "robots.txt"] {
        let p = cli.dist.join(f);
        if p.exists() {
            std::fs::remove_file(&p).with_context(|| format!("清理 {}", p.display()))?;
        }
    }
    let rendered = cli.content.join(".rendered");
    if rendered.exists() {
        std::fs::remove_dir_all(&rendered).with_context(|| format!("清理 {}", rendered.display()))?;
    }
    std::fs::create_dir_all(&rendered)
        .with_context(|| format!("创建 {}", rendered.display()))?;

    // 逐篇产出: HTML 镜像页 + ANSI 预渲染
    for a in &arts {
        let ansi_out = ansi::render_ansi(&a.events);
        let rp = rendered.join(&a.slug);
        if let Some(parent) = rp.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建 {}", parent.display()))?;
        }
        std::fs::write(&rp, ansi_out).with_context(|| format!("写 {}", rp.display()))?;

        let page = html::render_mirror_page(
            a,
            entry_js.as_deref().unwrap_or_default(),
            entry_css.as_deref().unwrap_or_default(),
            site_url.as_deref(),
            &site_title,
        );
        let hp = dist_blog.join(&a.slug).join("index.html");
        if let Some(parent) = hp.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建 {}", parent.display()))?;
        }
        std::fs::write(&hp, page).with_context(|| format!("写 {}", hp.display()))?;
    }

    // 列表数据 + 列表页
    std::fs::write(rendered.join(".index"), meta::build_index(&arts))
        .with_context(|| format!("写 {}", rendered.join(".index").display()))?;
    std::fs::create_dir_all(&dist_blog).with_context(|| format!("创建 {}", dist_blog.display()))?;
    let entry_js_str = entry_js.as_deref().unwrap_or_default();
    let list = html::render_list_page(&arts, site_url.as_deref(), &site_title, entry_js_str);
    std::fs::write(dist_blog.join("index.html"), list)
        .with_context(|| format!("写 {}", dist_blog.join("index.html").display()))?;

    // sitemap / atom(有 site_url 时)与 robots.txt(总是)
    if let Some(u) = &site_url {
        std::fs::write(cli.dist.join("sitemap.xml"), feed::sitemap(u, &arts))
            .with_context(|| format!("写 {}", cli.dist.join("sitemap.xml").display()))?;
        let feed_updated = arts
            .first()
            .map(|a| a.date_rfc3339.clone())
            .unwrap_or_else(|| chrono::Local::now().to_rfc3339());
        std::fs::write(cli.dist.join("atom.xml"), feed::atom(u, &site_title, &arts, &feed_updated))
            .with_context(|| format!("写 {}", cli.dist.join("atom.xml").display()))?;
    } else {
        warns.push("未设置 site_url, 跳过 sitemap.xml / atom.xml(镜像页无 canonical/OG:url)".into());
    }
    std::fs::write(cli.dist.join("robots.txt"), feed::robots(site_url.as_deref()))
        .with_context(|| format!("写 {}", cli.dist.join("robots.txt").display()))?;

    // 首页注入(幂等)
    if arts.is_empty() {
        warns.push("无文章, 跳过首页文章列表注入".into());
    } else {
        let idx_path = cli.dist.join("index.html");
        let src = std::fs::read_to_string(&idx_path)
            .with_context(|| format!("读首页 {}", idx_path.display()))?;
        let new = html::inject_homepage(&src, &arts, entry_js.as_deref().unwrap_or_default())
            .ok_or_else(|| anyhow::anyhow!("首页 {} 缺少 </body>, 无法注入", idx_path.display()))?;
        std::fs::write(&idx_path, new).with_context(|| format!("写 {}", idx_path.display()))?;
    }

    // 摘要
    println!("content-build: {} 篇文章", arts.len());
    println!("  镜像页:   {}/blog/<slug>/index.html", cli.dist.display());
    println!("  列表页:   {}/blog/index.html", cli.dist.display());
    println!("  ANSI 预渲染: {}/.rendered/", cli.content.display());
    for w in &warns {
        println!("警告: {w}");
    }
    Ok(())
}
