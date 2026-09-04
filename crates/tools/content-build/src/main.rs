//! content-build —— 内容编译器(termblog M5)。
//!
//! 唯一内容源 `jailtpl/content/blog/*.md` 的两个投影:
//!   HTML 镜像(爬虫读的静态全文页, 进 `frontend/dist/blog/`)与
//!   ANSI 预渲染(终端里 `less -R` 读的排版文本, 进 `jailtpl/content/.rendered/`)。
//! 同时产出文章列表页、sitemap.xml / atom.xml / robots.txt。
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
mod img;
mod meta;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use pulldown_cmark::{Event, Options, Parser, Tag};

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
    /// 本地图 dest_url 原文 → (宽, 高, 重写后 /blog/ 路径); html.rs 查表用。
    /// 外链不进表(html.rs 查不到 = External, 省略宽高)。
    pub image_meta: HashMap<String, (u32, u32, String)>,
    /// 本地图 dest_url 原文 → (源 rel 路径, 处理后产物 rel 路径)。
    /// 图片二期 manifest 锚点用(webp 转 png 后两个扩展名不同)。
    pub dest_paths: HashMap<String, (String, String)>,
    /// 文章第一张本地图的 /blog/ 路径(og:image 用)
    pub first_image: Option<String>,
}

/// 评论绑定文章的直属目录，而不是文章文件本身。同目录文章共享一个 target/FIFO。
pub(crate) fn comment_location_for_slug(slug: &str) -> (String, String) {
    let directory = slug.rsplit_once('/').map(|(parent, _)| parent).unwrap_or("");
    if directory.is_empty() {
        ("blog/comment".into(), "/blog/".into())
    } else {
        (
            format!("blog/{directory}/comment"),
            format!("/blog/{directory}/"),
        )
    }
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
    args.next()
        .ok_or_else(|| anyhow::anyhow!("参数 {name} 需要一个值"))
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

/// 递归扫描 content/blog/, 收集 *.md 的相对路径(相对 blog 根)与图片资源,
/// 同时收集 slug 违规与非 md 文件告警。
/// 资源分类: 图片扩展名白名单 → assets; .cast → 已知类型静默; 其它非 md → 告警。
fn scan_blog(
    root: &Path,
    dir: &Path,
    out: &mut Vec<PathBuf>,
    assets: &mut Vec<PathBuf>,
    slug_errors: &mut Vec<String>,
    warns: &mut Vec<String>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("扫描目录 {}", dir.display()))?
    {
        let entry = entry.with_context(|| format!("读目录项 {}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            scan_blog(root, &path, out, assets, slug_errors, warns)?;
        } else {
            let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext == "md" {
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
            } else if img::is_image_ext(ext) {
                // 资源路径字符集 [a-z0-9/._-](含目录部分), 违规与 slug 违规统一报出
                let rel_str = rel.to_string_lossy();
                if let Err(e) = img::validate_asset_path(&rel_str) {
                    slug_errors.push(format!("{}: {e}", rel.display()));
                } else {
                    assets.push(rel);
                }
            } else if ext.eq_ignore_ascii_case("cast") {
                // 已知类型(asciicast 录像): 静默
            } else {
                warns.push(format!("忽略非 markdown 文件: {}", rel.display()));
            }
        }
    }
    Ok(())
}

/// 收集事件流里所有图片的 dest_url 原文(文档序, 含重复)。
fn collect_image_dests(events: &[Event<'static>]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Start(Tag::Image { dest_url, .. }) => Some(dest_url.to_string()),
            _ => None,
        })
        .collect()
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
    let mut assets: Vec<PathBuf> = vec![];
    let mut slug_errors: Vec<String> = vec![];
    let mut warns: Vec<String> = vec![];
    if blog_dir.is_dir() {
        scan_blog(
            &blog_dir,
            &blog_dir,
            &mut rel_paths,
            &mut assets,
            &mut slug_errors,
            &mut warns,
        )?;
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
        bail!(
            "slug 校验失败: {} 个文件违规, 请改名后重跑",
            slug_errors.len()
        );
    }
    rel_paths.sort();

    // 逐篇: 读文件(BOM/CRLF 规范化)→ 解析(两投影共用同一套选项)→ 元数据 → 日期
    // 同循环处理图片: resolve → 存在性校验 → 缩放/预算 → 记录 image_meta。
    // 所有图片问题收集后统一 fail(宁构建失败, 不线上 404)。
    let mut arts: Vec<Article> = vec![];
    let mut image_errors: Vec<String> = vec![];
    let mut asset_bytes: HashMap<String, Vec<u8>> = HashMap::new(); // rel → 处理后字节
    let mut referenced: HashSet<String> = HashSet::new(); // 被引用的资源 rel 路径
    let mut total_imgs = 0usize;
    let mut total_bytes = 0usize;
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
        let parser = Parser::new_ext(
            &text,
            Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH,
        );
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
            warns.push(format!(
                "「{}」无 git 历史, 日期回退到文件 mtime",
                rel.display()
            ));
        }

        // 图片引用: 逐张 resolve + 处理, 单篇总量预算 1.5 MiB
        let mut image_meta: HashMap<String, (u32, u32, String)> = HashMap::new();
        let mut dest_paths: HashMap<String, (String, String)> = HashMap::new(); // dest → (源 rel, 产物 rel)
        let mut first_image: Option<String> = None;
        let mut article_bytes = 0usize;
        for dest in collect_image_dests(&events) {
            let (rel_path, suffix) = match img::resolve_image_url(&slug, &dest) {
                Err(e) => {
                    image_errors.push(format!("「{slug}」图片 {dest}: {e}"));
                    continue;
                }
                Ok(img::ResolvedImage::External(_)) => {
                    if dest.starts_with('/') {
                        warns.push(format!("「{slug}」站点绝对路径图片不受管线管理: {dest}"));
                    }
                    continue;
                }
                Ok(img::ResolvedImage::Local(_)) => {
                    // resolve 成功则 normalize 必然成功
                    let (rel_path, suffix) = img::normalize_local_path(&slug, &dest)
                        .expect("已 resolve 的本地图")
                        .expect("已 resolve 的本地图");
                    (rel_path, suffix)
                }
            };
            let img_path = blog_dir.join(&rel_path);
            if !img_path.is_file() {
                image_errors.push(format!("「{slug}」引用的图片不存在: {rel_path}"));
                continue;
            }
            match img::process_image(&img_path, &rel_path, &slug) {
                Err(e) => image_errors.push(format!("{e:#}")),
                Ok(p) => {
                    article_bytes += p.bytes.len();
                    if article_bytes > img::MAX_ARTICLE_BYTES {
                        image_errors.push(format!(
                            "「{slug}」图片总量超过单篇预算: 已累计 {} KiB(上限 {} KiB, 超出来自 {rel_path}); 请压缩/减少图片",
                            article_bytes / 1024,
                            img::MAX_ARTICLE_BYTES / 1024
                        ));
                        continue;
                    }
                    total_imgs += 1;
                    total_bytes += p.bytes.len();
                    referenced.insert(p.dist_rel.clone());
                    asset_bytes
                        .entry(p.dist_rel.clone())
                        .or_insert_with(|| p.bytes.clone());
                    // 产物 URL: webp 已转 png, 路径用转换后的扩展名
                    let url = format!("/blog/{}{suffix}", p.dist_rel);
                    image_meta.insert(dest.clone(), (p.width, p.height, url.clone()));
                    dest_paths.insert(dest.clone(), (rel_path, p.dist_rel));
                    if first_image.is_none() {
                        first_image = Some(url);
                    }
                }
            }
        }
        arts.push(Article {
            slug,
            title,
            excerpt,
            date10,
            date_rfc3339,
            date_warned,
            events,
            image_meta,
            dest_paths,
            first_image,
        });
    }
    if !image_errors.is_empty() {
        for e in &image_errors {
            eprintln!("图片错误: {e}");
        }
        bail!("图片处理失败: {} 处问题, 请修复后重跑", image_errors.len());
    }
    // 未被引用的资源仅告警(不处理不复制, 如 demo.cast 的先例)
    for a in &assets {
        let rel_str = a.to_string_lossy().to_string();
        if !referenced.contains(&rel_str) {
            warns.push(format!("未被引用的图片资源(不复制): {rel_str}"));
        }
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
            for e in
                std::fs::read_dir(&assets).with_context(|| format!("扫描 {}", assets.display()))?
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
            for e in
                std::fs::read_dir(&assets).with_context(|| format!("扫描 {}", assets.display()))?
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

    // 独立 comments 入口：文章镜像页不通过 main.ts 间接启动。
    let comments_js = {
        let assets = cli.dist.join("assets");
        let mut found = Vec::new();
        if assets.is_dir() {
            for entry in std::fs::read_dir(&assets)? {
                let name = entry?.file_name().to_string_lossy().to_string();
                if name.starts_with("comments-") && name.ends_with(".js") {
                    found.push(name);
                }
            }
        }
        match found.len() {
            1 => found.pop().unwrap(),
            0 => bail!("找不到独立 comments 前端入口，请先 make build-frontend"),
            n => bail!("找到 {n} 个 comments 前端入口，无法确定: {found:?}"),
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
        std::fs::remove_dir_all(&rendered)
            .with_context(|| format!("清理 {}", rendered.display()))?;
    }
    std::fs::create_dir_all(&rendered).with_context(|| format!("创建 {}", rendered.display()))?;
    // 处理后图片的第二投影(jail 内 TUI 阅读器的读取源): 与 dist/blog 同字节、
    // 同路径; 目录与 dist/blog 一样每轮先清后写, 僵尸资源天然清理。
    let rendered_assets = cli.content.join(".rendered-assets");
    if rendered_assets.exists() {
        std::fs::remove_dir_all(&rendered_assets)
            .with_context(|| format!("清理 {}", rendered_assets.display()))?;
    }
    std::fs::create_dir_all(&rendered_assets)
        .with_context(|| format!("创建 {}", rendered_assets.display()))?;

    // 评论绑定文章直属目录；同目录多篇文章共享一行，纯资源目录不创建 FIFO。
    let mut target_rows = vec![("comment".to_string(), "/".to_string())];
    target_rows.extend(
        arts.iter()
            .map(|a| comment_location_for_slug(&a.slug)),
    );
    target_rows.sort_by(|a, b| a.0.cmp(&b.0));
    target_rows.dedup();
    let target_manifest: String = target_rows
        .iter()
        .map(|(path, target)| format!("{path}\t{target}\n"))
        .collect();
    std::fs::write(cli.content.join(".comment-targets.tsv"), target_manifest)
        .with_context(|| format!("写 {}", cli.content.join(".comment-targets.tsv").display()))?;

    // 复制被引用的图片资源进 dist/blog/(dist/blog 已整体先清, 僵尸资源天然清理)
    // 与 .rendered-assets/(与 dist/blog 同字节、同路径)
    for (rel_path, bytes) in &asset_bytes {
        let p = dist_blog.join(rel_path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建 {}", parent.display()))?;
        }
        std::fs::write(&p, bytes).with_context(|| format!("写图片 {}", p.display()))?;
        let q = rendered_assets.join(rel_path);
        if let Some(parent) = q.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建 {}", parent.display()))?;
        }
        std::fs::write(&q, bytes).with_context(|| format!("写处理后图片 {}", q.display()))?;
    }

    // ANSI 占位框里 URL 的基址: 有 site_url 拼完整 URL(SSH 用户可直接复制进浏览器),
    // 无则用 /blog/... 站点路径(web 端 OSC8 点击相对当前域仍可达)
    let link_base = site_url.as_deref().unwrap_or("");

    // 逐篇产出: HTML 镜像页 + ANSI 预渲染(+ 图片 sidecar manifest)
    for a in &arts {
        // 锚点查表: dest 原文 → (源 rel, 产物 rel, 宽, 高)。外链/行内图查不到
        // → 不产锚点, 占位框保持纯文本形态。
        let lookup = |dest: &str| -> Option<ansi::ImgMeta> {
            let (w, h, _) = *a.image_meta.get(dest)?;
            let (path, asset) = a
                .dest_paths
                .get(dest)
                .cloned()
                .unwrap_or_else(|| (dest.to_string(), dest.to_string()));
            Some(ansi::ImgMeta { path, asset, w, h })
        };
        let (ansi_out, anchors) = ansi::render_ansi(&a.events, &a.slug, link_base, &lookup);
        let rp = rendered.join(&a.slug);
        if let Some(parent) = rp.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建 {}", parent.display()))?;
        }
        std::fs::write(&rp, ansi_out).with_context(|| format!("写 {}", rp.display()))?;
        // 有图才写 manifest(无图文章不写文件, reader 据此快速判断无图)
        if !anchors.is_empty() {
            let mp = rendered.join(format!("{}.images.json", a.slug));
            if let Some(parent) = mp.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("创建 {}", parent.display()))?;
            }
            std::fs::write(
                &mp,
                ansi::Manifest {
                    version: 1,
                    images: anchors,
                }
                .to_json(),
            )
            .with_context(|| format!("写 {}", mp.display()))?;
        }

        let page = html::render_mirror_page(
            a,
            entry_js.as_deref().unwrap_or_default(),
            entry_css.as_deref().unwrap_or_default(),
            &comments_js,
            site_url.as_deref(),
            &site_title,
            a.first_image.as_deref(),
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
    let list = html::render_list_page(
        &arts,
        site_url.as_deref(),
        &site_title,
        entry_js_str,
    );
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
        std::fs::write(
            cli.dist.join("atom.xml"),
            feed::atom(u, &site_title, &arts, &feed_updated),
        )
        .with_context(|| format!("写 {}", cli.dist.join("atom.xml").display()))?;
    } else {
        warns
            .push("未设置 site_url, 跳过 sitemap.xml / atom.xml(镜像页无 canonical/OG:url)".into());
    }
    std::fs::write(
        cli.dist.join("robots.txt"),
        feed::robots(site_url.as_deref()),
    )
    .with_context(|| format!("写 {}", cli.dist.join("robots.txt").display()))?;

    // 摘要
    println!("content-build: {} 篇文章", arts.len());
    println!("  镜像页:   {}/blog/<slug>/index.html", cli.dist.display());
    println!("  列表页:   {}/blog/index.html", cli.dist.display());
    println!("  ANSI 预渲染: {}/.rendered/", cli.content.display());
    println!(
        "  图片: {} 张, 共 {} KiB(预算 {} KiB/篇)",
        total_imgs,
        total_bytes / 1024,
        img::MAX_ARTICLE_BYTES / 1024
    );
    println!("  处理后图片: {}/.rendered-assets/", cli.content.display());
    for w in &warns {
        println!("警告: {w}");
    }
    Ok(())
}

#[cfg(test)]
mod comment_location_tests {
    use super::*;

    #[test]
    fn comments_attach_to_immediate_article_directory() {
        assert_eq!(
            comment_location_for_slug("hello"),
            ("blog/comment".into(), "/blog/".into())
        );
        assert_eq!(
            comment_location_for_slug("topic/one"),
            ("blog/topic/comment".into(), "/blog/topic/".into())
        );
        assert_eq!(
            comment_location_for_slug("topic/deep/two"),
            (
                "blog/topic/deep/comment".into(),
                "/blog/topic/deep/".into(),
            )
        );
    }
}
