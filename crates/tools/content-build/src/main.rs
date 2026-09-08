//! content-build —— 内容编译器(termblog M5)。
//!
//! `jailtpl/content/` 中所有可见 `.md` 文章的两个投影:
//!   HTML 镜像(爬虫读的静态全文页, 进与公开 route 对应的 dist 路径)与
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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use pulldown_cmark::{Event, Options, Parser, Tag};
use serde::Deserialize;
use termblog_content_model::{ArticlePath, CommentAttachment, ContentScope};

const GLOBAL_LIST_DIRECTORY: &str = "blog";
const GLOBAL_LIST_OUTPUT: &str = "blog/index.html";

/// 本地图片在正文中的发布形态。
pub enum ImagePresentation {
    /// 处理后图片可在 Web 和终端中内联显示。
    Inline {
        width: u32,
        height: u32,
        url: String,
    },
    /// 原图仅由 Web 发布，正文显示可点击链接。
    LinkOnly { url: String },
}

/// Web 图片产物：小图用内存中的处理后字节，超限原图从磁盘流式复制。
enum WebAsset {
    Processed(Vec<u8>),
    Original(PathBuf),
}

/// 一篇文章的编译中间态: 元数据 + 解析后的事件流。
pub struct Article {
    pub path: ArticlePath,
    pub title: String,
    pub excerpt: String,
    /// YYYY-MM-DD(sitemap lastmod / 镜像页 footer / .index)
    pub date10: String,
    /// 完整 RFC3339(atom updated / 排序键)
    pub date_rfc3339: String,
    /// 日期走了 mtime fallback(构建告警用)
    pub date_warned: bool,
    pub events: Vec<Event<'static>>,
    /// 本地图 dest_url 原文 → 内联图或原图链接。外链不进表。
    pub image_meta: HashMap<String, ImagePresentation>,
    /// 本地图 dest_url 原文 → (源 rel 路径, 处理后产物 rel 路径)。
    /// 图片二期 manifest 锚点用(webp 转 png 后两个扩展名不同)。
    pub dest_paths: HashMap<String, (String, String)>,
    /// 文章第一张本地图的站点绝对路径(og:image 用)
    pub first_image: Option<String>,
    /// 仅在文章直属目录显式启用评论时存在。
    pub comments: Option<CommentAttachment>,
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
        .ok_or_else(|| anyhow::anyhow!("argument {name} requires a value"))
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
            other => bail!("unknown argument: {other}"),
        }
    }
    Ok(cli)
}

/// 递归扫描 content/HOME 蓝图，收集任意非隐藏目录中的文章与图片资源。
/// 资源分类: 图片扩展名白名单 → assets; .cast → 已知类型静默; 其它非 md → 告警。
fn scan_content(
    root: &Path,
    dir: &Path,
    out: &mut Vec<ArticlePath>,
    assets: &mut Vec<PathBuf>,
    path_errors: &mut Vec<String>,
    warns: &mut Vec<String>,
) -> Result<()> {
    let mut entries = std::fs::read_dir(dir)
        .with_context(|| format!("scan directory {}", dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        let rel_str = rel.to_string_lossy();
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("read directory entry metadata {}", path.display()))?;
        if metadata.file_type().is_symlink() {
            path_errors.push(format!(
                "{}: content does not support symlinks",
                rel.display()
            ));
            continue;
        }
        if termblog_content_model::has_hidden_component(&rel_str) {
            if rel.components().count() == 1
                && !matches!(
                    rel_str.as_ref(),
                    ".termblog.toml"
                        | ".rendered"
                        | ".rendered-assets"
                        | ".comment-targets.tsv"
                        | ".web-outputs.tsv"
                )
            {
                warns.push(format!(
                    "skipping unknown hidden content: {}",
                    rel.display()
                ));
            }
            continue;
        }
        if metadata.is_dir() {
            scan_content(root, &path, out, assets, path_errors, warns)?;
        } else {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext == "md" {
                match ArticlePath::from_source_rel(&rel) {
                    Ok(article_path) => out.push(article_path),
                    Err(e) => path_errors.push(format!("{}: {e}", rel.display())),
                }
            } else if img::is_image_ext(ext) {
                // 资源路径字符集 [a-z0-9/._-](含目录部分), 违规与文章路径错误统一报出
                if let Err(e) = termblog_content_model::validate_resource_rel(&rel_str) {
                    path_errors.push(format!("{}: {e}", rel.display()));
                } else {
                    assets.push(rel);
                }
            } else if ext.eq_ignore_ascii_case("cast") {
                // 已知类型(asciicast 录像): 静默
            } else {
                warns.push(format!(
                    "ordinary HOME content (no Web page generated): {}",
                    rel.display()
                ));
            }
        }
    }
    Ok(())
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ContentConfig {
    /// Preferred shared directory-scope spelling.
    scopes: Option<ContentDirectories>,
    /// Backward-compatible spelling used before statistics shared the scope.
    comments: Option<ContentDirectories>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ContentDirectories {
    directories: Vec<String>,
}

fn load_scopes(content: &Path) -> Result<Vec<ContentScope>> {
    let config_path = content.join(".termblog.toml");
    if !config_path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&config_path)
        .with_context(|| format!("read content config {}", config_path.display()))?;
    let config: ContentConfig = toml::from_str(&text)
        .with_context(|| format!("parse content config {}", config_path.display()))?;
    let directories = match (config.scopes, config.comments) {
        (Some(_), Some(_)) => bail!(
            "{}: configure either [scopes] or legacy [comments], not both",
            config_path.display()
        ),
        (Some(scopes), None) => scopes.directories,
        (None, Some(comments)) => comments.directories,
        (None, None) => Vec::new(),
    };
    let mut seen = BTreeSet::new();
    let mut scopes = Vec::new();
    for directory in directories {
        if !seen.insert(directory.clone()) {
            bail!(
                "{}: configured directories duplicate entry {:?}",
                config_path.display(),
                directory
            );
        }
        let scope = ContentScope::from_directory_rel(&directory).map_err(|e| {
            anyhow::anyhow!(
                "{}: configured directory {:?}: {e}",
                config_path.display(),
                directory
            )
        })?;
        let directory_path = content.join(&scope.directory_rel);
        let metadata = std::fs::symlink_metadata(&directory_path).with_context(|| {
            format!(
                "configured directory does not exist: {}",
                directory_path.display()
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!(
                "configured directory must be a real directory: {} ({:?})",
                directory_path.display(),
                directory
            );
        }
        let reserved = content.join(&scope.comment_rel);
        match std::fs::symlink_metadata(&reserved) {
            Ok(_) => {
                bail!(
                    "configured directory {:?} reserves comment path {}, but it is already taken by source content",
                    directory,
                    reserved.display()
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("check reserved path {}", reserved.display()));
            }
        }
        scopes.push(scope);
    }
    scopes.sort_by(|a, b| a.comment_rel.cmp(&b.comment_rel));
    Ok(scopes)
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

fn validate_output_rel(rel: &str) -> Result<()> {
    if rel.is_empty() || rel.starts_with('/') || rel.ends_with('/') || rel.contains('\\') {
        bail!("Web output manifest path is not a normalized relative path: {rel:?}");
    }
    if rel
        .split('/')
        .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        bail!("Web output manifest path contains invalid components: {rel:?}");
    }
    Ok(())
}

fn read_web_manifest(content: &Path) -> Result<(BTreeMap<String, String>, bool)> {
    let path = content.join(".web-outputs.tsv");
    if !path.is_file() {
        return Ok((BTreeMap::new(), false));
    }
    let mut outputs = BTreeMap::new();
    for (index, line) in std::fs::read_to_string(&path)?.lines().enumerate() {
        let (rel, owner) = line
            .split_once('\t')
            .ok_or_else(|| anyhow::anyhow!("{} line {} missing Tab", path.display(), index + 1))?;
        validate_output_rel(rel)?;
        if owner.is_empty() || outputs.insert(rel.to_owned(), owner.to_owned()).is_some() {
            bail!(
                "{} line {} is empty or duplicated",
                path.display(),
                index + 1
            );
        }
    }
    Ok((outputs, true))
}

fn validate_control_paths(content: &Path) -> Result<()> {
    for (rel, want_directory) in [
        (".termblog.toml", false),
        (".rendered", true),
        (".rendered-assets", true),
        (".comment-targets.tsv", false),
        (".web-outputs.tsv", false),
    ] {
        let path = content.join(rel);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("read control path {}", path.display()));
            }
        };
        if metadata.file_type().is_symlink()
            || (want_directory && !metadata.is_dir())
            || (!want_directory && !metadata.is_file())
        {
            bail!(
                "content control path type error: {} must be {} and cannot be a symlink",
                path.display(),
                if want_directory {
                    "directory"
                } else {
                    "regular file"
                }
            );
        }
    }
    Ok(())
}

fn walk_regular_files(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let mut entries = std::fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            bail!(
                "Web static tree does not allow symlinks: {}",
                path.display()
            );
        }
        if metadata.is_dir() {
            walk_regular_files(root, &path, out)?;
        } else if metadata.is_file() {
            out.push(path.strip_prefix(root)?.to_string_lossy().into_owned());
        }
    }
    Ok(())
}

/// 扫描已有静态树中的文件和目录。目录以尾 `/` 登记，因此只会与“同路径应为文件”
/// 冲突，不会阻止内容在一个已有目录下创建新的子路径。
fn walk_output_nodes(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let mut entries = std::fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            bail!(
                "Web static tree does not allow symlinks: {}",
                path.display()
            );
        }
        let rel = path.strip_prefix(root)?.to_string_lossy();
        if metadata.is_dir() {
            out.push(format!("{rel}/"));
            walk_output_nodes(root, &path, out)?;
        } else if metadata.is_file() {
            out.push(rel.into_owned());
        }
    }
    Ok(())
}

fn claim_output(
    claims: &mut BTreeMap<String, String>,
    rel: &str,
    owner: &str,
    errors: &mut Vec<String>,
) {
    for (claimed, claimed_owner) in claims.iter() {
        if claimed == rel
            || claimed.starts_with(&format!("{rel}/"))
            || rel.starts_with(&format!("{claimed}/"))
        {
            errors.push(format!(
                "Web output conflict: {rel}\n  already claimed by: {claimed_owner} ({claimed})\n  new claim by: {owner}"
            ));
        }
    }
    claims
        .entry(rel.to_owned())
        .or_insert_with(|| owner.to_owned());
}

fn reserved_route_owner(key: &str) -> Option<&'static str> {
    if key == "blog" {
        Some("system article list /blog/")
    } else if key == "ws" {
        Some("WebSocket system route /ws")
    } else if key == "api" || key.starts_with("api/") {
        Some("HTTP API prefix /api/")
    } else if key == "assets" || key.starts_with("assets/") {
        Some("Vite static asset prefix /assets/")
    } else if key == "fonts" || key.starts_with("fonts/") {
        Some("font asset prefix /fonts/")
    } else {
        None
    }
}

fn claim_desired(
    claims: &mut BTreeMap<String, String>,
    desired: &mut BTreeMap<String, String>,
    rel: String,
    owner: String,
    errors: &mut Vec<String>,
) {
    claim_output(claims, &rel, &owner, errors);
    if let Some(first) = desired.insert(rel.clone(), owner.clone()) {
        errors.push(format!(
            "Web output conflict: {rel}\n  already claimed by: {first}\n  new claim by: {owner}"
        ));
    }
}

fn build_web_claims(
    dist: &Path,
    old_outputs: &BTreeMap<String, String>,
    has_manifest: bool,
    arts: &[Article],
    asset_owners: &BTreeMap<String, String>,
    site_url: Option<&str>,
) -> Result<BTreeMap<String, String>> {
    let mut claims = BTreeMap::new();
    let mut errors = Vec::new();
    let mut existing = Vec::new();
    walk_output_nodes(dist, dist, &mut existing)?;
    for rel in existing {
        if old_outputs.contains_key(&rel) || (!has_manifest && rel.starts_with("blog/")) {
            continue;
        }
        claim_output(
            &mut claims,
            &rel,
            &format!("Vite/public static file {rel}"),
            &mut errors,
        );
    }

    let mut desired = BTreeMap::new();
    claim_desired(
        &mut claims,
        &mut desired,
        GLOBAL_LIST_OUTPUT.into(),
        "system article list /blog/".into(),
        &mut errors,
    );
    claim_desired(
        &mut claims,
        &mut desired,
        "robots.txt".into(),
        "system feed artifact robots.txt".into(),
        &mut errors,
    );
    if site_url.is_some() {
        claim_desired(
            &mut claims,
            &mut desired,
            "sitemap.xml".into(),
            "system feed artifact sitemap.xml".into(),
            &mut errors,
        );
        claim_desired(
            &mut claims,
            &mut desired,
            "atom.xml".into(),
            "system feed artifact atom.xml".into(),
            &mut errors,
        );
    }
    for article in arts {
        if let Some(system) = reserved_route_owner(&article.path.key) {
            errors.push(format!(
                "Web path conflict: {}\n  system reservation: {system}\n  content claim: {}",
                article.path.route,
                article.path.source_rel.display()
            ));
        }
        claim_desired(
            &mut claims,
            &mut desired,
            format!("{}/index.html", article.path.key),
            format!("article {}", article.path.source_rel.display()),
            &mut errors,
        );
    }
    for (rel, source) in asset_owners {
        if let Some(first) = rel.split('/').next() {
            if matches!(first, "assets" | "fonts" | "api") {
                errors.push(format!(
                    "Web path conflict: /{rel}\n  system reservation: /{first}/ prefix\n  content claim: image {source}"
                ));
            }
        }
        claim_desired(
            &mut claims,
            &mut desired,
            rel.clone(),
            format!("article image {source}"),
            &mut errors,
        );
    }

    if !errors.is_empty() {
        for error in &errors {
            eprintln!("{error}");
        }
        bail!("Web path/output conflicts: {} issue(s)", errors.len());
    }
    Ok(desired)
}

fn write_stage(stage: &Path, rel: &str, bytes: impl AsRef<[u8]>) -> Result<()> {
    validate_output_rel(rel)?;
    let path = stage.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, bytes)
        .with_context(|| format!("write staging artifact {}", path.display()))
}

fn remove_empty_parents(mut path: PathBuf, stop: &Path) {
    while path != stop {
        if std::fs::remove_dir(&path).is_err() {
            break;
        }
        let Some(parent) = path.parent() else { break };
        path = parent.to_path_buf();
    }
}

fn commit_web_outputs(
    dist: &Path,
    stage: &Path,
    old_outputs: &BTreeMap<String, String>,
    has_manifest: bool,
) -> Result<()> {
    std::fs::create_dir_all(dist)?;
    if !has_manifest {
        let legacy = dist.join(GLOBAL_LIST_DIRECTORY);
        if legacy.exists() {
            std::fs::remove_dir_all(&legacy).with_context(|| {
                format!("clean up legacy content directory {}", legacy.display())
            })?;
        }
    }
    for rel in old_outputs.keys() {
        validate_output_rel(rel)?;
        let path = dist.join(rel);
        if path.is_file() {
            std::fs::remove_file(&path)?;
            if let Some(parent) = path.parent() {
                remove_empty_parents(parent.to_path_buf(), dist);
            }
        }
    }

    let mut staged = Vec::new();
    walk_regular_files(stage, stage, &mut staged)?;
    for rel in staged {
        let source = stage.join(&rel);
        let target = dist.join(&rel);
        let parent = target
            .parent()
            .context("Web artifact missing parent directory")?;
        std::fs::create_dir_all(parent)?;
        let file_name = target
            .file_name()
            .and_then(|v| v.to_str())
            .context("invalid Web artifact filename")?;
        let temp = parent.join(format!(".{file_name}.content-build-{}", std::process::id()));
        std::fs::copy(&source, &temp)?;
        std::fs::rename(&temp, &target)?;
    }
    Ok(())
}

fn replace_tree(staged: &Path, destination: &Path) -> Result<()> {
    let name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .context("invalid generated directory name")?;
    let backup = destination.with_file_name(format!(".{name}.old-{}", std::process::id()));
    if backup.exists() {
        bail!("temporary backup path already exists: {}", backup.display());
    }
    if destination.exists() {
        std::fs::rename(destination, &backup)?;
    }
    if let Err(error) = std::fs::rename(staged, destination) {
        if backup.exists() {
            let _ = std::fs::rename(&backup, destination);
        }
        return Err(error).with_context(|| format!("replace {}", destination.display()));
    }
    if backup.exists() {
        std::fs::remove_dir_all(backup)?;
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: impl AsRef<[u8]>) -> Result<()> {
    let parent = path
        .parent()
        .context("atomic write path missing parent directory")?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("invalid filename")?;
    let temp = parent.join(format!(".{name}.tmp-{}", std::process::id()));
    std::fs::write(&temp, bytes)?;
    std::fs::rename(temp, path)?;
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
    // 终端阅读提示里的 ssh 端口: 取 [ssh] listen 的端口部分(22 时提示省略 -p)
    let ssh_port: u16 = cfg
        .ssh
        .listen
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(22);

    // content 是唯一扫描根；blog/ 只是其中一个普通目录。
    let content_metadata = std::fs::symlink_metadata(&cli.content)
        .with_context(|| format!("read content root {}", cli.content.display()))?;
    if !content_metadata.is_dir() || content_metadata.file_type().is_symlink() {
        bail!(
            "content root does not exist or is not a directory: {}",
            cli.content.display()
        );
    }
    validate_control_paths(&cli.content)?;
    let mut article_paths: Vec<ArticlePath> = Vec::new();
    let mut assets: Vec<PathBuf> = Vec::new();
    let mut path_errors: Vec<String> = Vec::new();
    let mut warns: Vec<String> = Vec::new();
    scan_content(
        &cli.content,
        &cli.content,
        &mut article_paths,
        &mut assets,
        &mut path_errors,
        &mut warns,
    )?;
    if article_paths.is_empty() {
        warns.push(format!("no articles under {}", cli.content.display()));
    }
    if !path_errors.is_empty() {
        for error in &path_errors {
            eprintln!("content path violation: {error}");
        }
        bail!(
            "content path validation failed: {} issue(s)",
            path_errors.len()
        );
    }
    article_paths.sort_by(|a, b| a.key.cmp(&b.key));
    let scopes = load_scopes(&cli.content)?;
    let attachments_by_dir: BTreeMap<String, CommentAttachment> = scopes
        .iter()
        .map(|scope| (scope.directory_rel.clone(), scope.comment_attachment()))
        .collect();

    // 逐篇: 读文件(BOM/CRLF 规范化)→ 解析(两投影共用同一套选项)→ 元数据 → 日期
    // 同循环处理图片: resolve → 存在性校验 → 内联/原图链接决策。
    // 路径、解码和输出冲突仍统一 fail；超预算只降级为链接。
    let mut arts: Vec<Article> = vec![];
    let mut image_errors: Vec<String> = vec![];
    let mut web_assets: HashMap<String, WebAsset> = HashMap::new();
    let mut rendered_asset_bytes: HashMap<String, Vec<u8>> = HashMap::new();
    let mut asset_owners: BTreeMap<String, String> = BTreeMap::new();
    let mut referenced: HashSet<String> = HashSet::new(); // 被引用的源资源 rel 路径
    let mut total_inline_imgs = 0usize;
    let mut total_inline_bytes = 0usize;
    let mut total_link_imgs = 0usize;
    let mut total_link_bytes = 0u64;
    for article_path in &article_paths {
        let path = cli.content.join(&article_path.source_rel);
        let src =
            std::fs::read(&path).with_context(|| format!("read article {}", path.display()))?;
        let mut text = String::from_utf8(src)
            .with_context(|| format!("article is not UTF-8: {}", path.display()))?;
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
        let source_rel = article_path.source_rel.to_string_lossy();
        let key = &article_path.key;
        let stem = key.rsplit('/').next().unwrap_or(key).to_string();
        let title = meta::extract_title(&events, &stem);
        let excerpt = meta::extract_excerpt(&events, &title);
        let (date10, date_rfc3339, date_warned) = meta::article_date(&path);
        if date_warned {
            warns.push(format!(
                "\"{}\" has no git history, date fell back to file mtime",
                article_path.source_rel.display()
            ));
        }

        // 图片引用: 逐张 resolve + 处理, 单篇内联总量预算 1.5 MiB
        let mut image_meta: HashMap<String, ImagePresentation> = HashMap::new();
        let mut dest_paths: HashMap<String, (String, String)> = HashMap::new(); // dest → (源 rel, 产物 rel)
        let mut first_image: Option<String> = None;
        let mut article_bytes = 0usize;
        for dest in collect_image_dests(&events) {
            let (rel_path, suffix) = match img::resolve_image_url(&source_rel, &dest) {
                Err(e) => {
                    image_errors.push(format!("\"{key}\" image {dest}: {e}"));
                    continue;
                }
                Ok(img::ResolvedImage::External(_)) => {
                    if dest.starts_with('/') {
                        warns.push(format!(
                            "\"{key}\" site-absolute image is not managed by pipeline: {dest}"
                        ));
                    }
                    continue;
                }
                Ok(img::ResolvedImage::Local(_)) => {
                    // resolve 成功则 normalize 必然成功
                    let (rel_path, suffix) = img::normalize_local_path(&source_rel, &dest)
                        .expect("resolved local image")
                        .expect("resolved local image");
                    (rel_path, suffix)
                }
            };
            let img_path = cli.content.join(&rel_path);
            if !img_path.is_file() {
                image_errors.push(format!(
                    "image referenced by \"{key}\" does not exist: {rel_path}"
                ));
                continue;
            }
            let source_len = match std::fs::metadata(&img_path) {
                Ok(metadata) => metadata.len(),
                Err(e) => {
                    image_errors.push(format!("stat image {}: {e}", img_path.display()));
                    continue;
                }
            };
            let output = match img::process_image(&img_path, &rel_path, key) {
                Err(e) => {
                    image_errors.push(format!("{e:#}"));
                    continue;
                }
                Ok(img::ImageOutput::Inline(p))
                    if article_bytes.saturating_add(p.bytes.len()) > img::MAX_ARTICLE_BYTES =>
                {
                    img::ImageOutput::LinkOnly {
                        reason: format!(
                            "article inline total would reach {} KiB, exceeding {} KiB",
                            article_bytes.saturating_add(p.bytes.len()) / 1024,
                            img::MAX_ARTICLE_BYTES / 1024
                        ),
                    }
                }
                Ok(output) => output,
            };

            match output {
                img::ImageOutput::Inline(p) => {
                    if let Some(first) = asset_owners.insert(p.dist_rel.clone(), rel_path.clone()) {
                        if first != rel_path {
                            image_errors.push(format!(
                                "Web output conflict: {} generated simultaneously by {} and {}",
                                p.dist_rel, first, rel_path
                            ));
                            continue;
                        }
                    }
                    article_bytes += p.bytes.len();
                    total_inline_imgs += 1;
                    total_inline_bytes += p.bytes.len();
                    referenced.insert(rel_path.clone());
                    web_assets.insert(p.dist_rel.clone(), WebAsset::Processed(p.bytes.clone()));
                    rendered_asset_bytes.insert(p.dist_rel.clone(), p.bytes);
                    // 产物 URL: webp 已转 png, 路径用转换后的扩展名
                    let url = format!("/{}{suffix}", p.dist_rel);
                    image_meta.insert(
                        dest.clone(),
                        ImagePresentation::Inline {
                            width: p.width,
                            height: p.height,
                            url: url.clone(),
                        },
                    );
                    dest_paths.insert(dest.clone(), (rel_path, p.dist_rel));
                    if first_image.is_none() {
                        first_image = Some(url);
                    }
                }
                img::ImageOutput::LinkOnly { reason } => {
                    if let Some(first) = asset_owners.insert(rel_path.clone(), rel_path.clone()) {
                        if first != rel_path {
                            image_errors.push(format!(
                                "Web output conflict: {rel_path} generated simultaneously by {first} and {rel_path}"
                            ));
                            continue;
                        }
                    }
                    total_link_imgs += 1;
                    total_link_bytes = total_link_bytes.saturating_add(source_len);
                    referenced.insert(rel_path.clone());
                    web_assets.insert(rel_path.clone(), WebAsset::Original(img_path));
                    let url = format!("/{rel_path}{suffix}");
                    image_meta.insert(
                        dest.clone(),
                        ImagePresentation::LinkOnly { url: url.clone() },
                    );
                    if first_image.is_none() {
                        first_image = Some(url);
                    }
                    warns.push(format!(
                        "\"{key}\" image {rel_path} published as original link: {reason}"
                    ));
                }
            }
        }
        arts.push(Article {
            path: article_path.clone(),
            title,
            excerpt,
            date10,
            date_rfc3339,
            date_warned,
            events,
            image_meta,
            dest_paths,
            first_image,
            comments: attachments_by_dir.get(&article_path.directory_rel).cloned(),
        });
    }
    if !image_errors.is_empty() {
        for e in &image_errors {
            eprintln!("image error: {e}");
        }
        bail!(
            "image processing failed: {} issue(s), please fix and re-run",
            image_errors.len()
        );
    }
    // 未被引用的资源仅告警(不处理不复制, 如 demo.cast 的先例)
    for a in &assets {
        let rel_str = a.to_string_lossy().to_string();
        if !referenced.contains(&rel_str) {
            warns.push(format!("unreferenced image asset (not copied): {rel_str}"));
        }
    }
    // 排序: 日期倒序, 同日 article key 字典序升序
    arts.sort_by(|a, b| {
        b.date_rfc3339
            .cmp(&a.date_rfc3339)
            .then_with(|| a.path.key.cmp(&b.path.key))
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
                std::fs::read_dir(&assets).with_context(|| format!("scan {}", assets.display()))?
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
                "cannot find frontend entry JS ({}), please run make build-frontend first (vite build before content-build)",
                assets.display()
            ),
            n => bail!("found {n} frontend entry JS files, cannot determine which to use: {found:?}"),
        }
    };
    let entry_css = if arts.is_empty() {
        None
    } else {
        let assets = cli.dist.join("assets");
        let mut found: Vec<String> = vec![];
        if assets.is_dir() {
            for e in
                std::fs::read_dir(&assets).with_context(|| format!("scan {}", assets.display()))?
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
                "cannot find frontend entry CSS ({}), please run make build-frontend first (vite build before content-build)",
                assets.display()
            ),
            n => bail!("found {n} frontend entry CSS files, cannot determine which to use: {found:?}"),
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
            0 => bail!("cannot find standalone comments frontend entry, please run make build-frontend first"),
            n => bail!("found {n} comments frontend entries, cannot determine: {found:?}"),
        }
    };

    // 在动旧产物前完成全部 Web owner/route 冲突检查。
    let (old_outputs, has_web_manifest) = read_web_manifest(&cli.content)?;
    let desired_outputs = build_web_claims(
        &cli.dist,
        &old_outputs,
        has_web_manifest,
        &arts,
        &asset_owners,
        site_url.as_deref(),
    )?;

    // 全部新产物先写入 content 同文件系统的隐藏 staging，失败时 TempDir 自动清理。
    let staging = tempfile::Builder::new()
        .prefix(".content-build-")
        .tempdir_in(&cli.content)?;
    let stage_web = staging.path().join("web");
    let rendered = staging.path().join("rendered");
    let rendered_assets = staging.path().join("rendered-assets");
    std::fs::create_dir_all(&stage_web)?;
    std::fs::create_dir_all(&rendered)?;
    std::fs::create_dir_all(&rendered_assets)?;
    let dist_blog = stage_web.join(GLOBAL_LIST_DIRECTORY);

    // Shared scopes come entirely from explicit content configuration and do
    // not depend on whether a directory currently contains an article.
    let target_rows: Vec<(String, String)> = scopes
        .iter()
        .map(|scope| (scope.comment_rel.clone(), scope.target.clone()))
        .collect();
    let target_manifest: String = target_rows
        .iter()
        .map(|(path, target)| format!("{path}\t{target}\n"))
        .collect();

    // Web 包含所有被引用资源：预算内用处理后字节，超限用无限制原图。
    for (rel_path, asset) in &web_assets {
        let p = stage_web.join(rel_path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        match asset {
            WebAsset::Processed(bytes) => {
                std::fs::write(&p, bytes)
                    .with_context(|| format!("write image {}", p.display()))?;
            }
            WebAsset::Original(source) => {
                std::fs::copy(source, &p).with_context(|| {
                    format!(
                        "copy original image {} -> {}",
                        source.display(),
                        p.display()
                    )
                })?;
            }
        }
    }
    // 终端资产只含预算内图片；链接原图不进入 IIP 载荷。
    for (rel_path, bytes) in &rendered_asset_bytes {
        let q = rendered_assets.join(rel_path);
        if let Some(parent) = q.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::write(&q, bytes)
            .with_context(|| format!("write processed image {}", q.display()))?;
    }

    // ANSI 占位框里 URL 的基址: 有 site_url 拼完整 URL(SSH 用户可直接复制进浏览器),
    // 无则用站点绝对路径(web 端 OSC8 点击相对当前域仍可达)
    let link_base = site_url.as_deref().unwrap_or("");

    // 逐篇产出: HTML 镜像页 + ANSI 预渲染(+ 图片 sidecar manifest)
    for a in &arts {
        // 锚点查表只返回预算内图片。外链/链接原图不产生锚点，
        // 因而 TUI 不读取、不传输它们。
        let lookup = |dest: &str| -> Option<ansi::ImgMeta> {
            let ImagePresentation::Inline {
                width: w,
                height: h,
                ..
            } = a.image_meta.get(dest)?
            else {
                return None;
            };
            let (path, asset) = a
                .dest_paths
                .get(dest)
                .cloned()
                .unwrap_or_else(|| (dest.to_string(), dest.to_string()));
            Some(ansi::ImgMeta {
                path,
                asset,
                w: *w,
                h: *h,
            })
        };
        let (ansi_out, anchors) = ansi::render_ansi(
            &a.events,
            &a.path.source_rel.to_string_lossy(),
            link_base,
            &lookup,
        );
        let rp = rendered.join(&a.path.key);
        if let Some(parent) = rp.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::write(&rp, ansi_out).with_context(|| format!("write {}", rp.display()))?;
        // 有图才写 manifest(无图文章不写文件, reader 据此快速判断无图)
        if !anchors.is_empty() {
            let mp = rendered.join(format!("{}.images.json", a.path.key));
            if let Some(parent) = mp.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            std::fs::write(
                &mp,
                ansi::Manifest {
                    version: 1,
                    images: anchors,
                }
                .to_json(),
            )
            .with_context(|| format!("write {}", mp.display()))?;
        }

        let page = html::render_mirror_page(
            a,
            entry_js.as_deref().unwrap_or_default(),
            entry_css.as_deref().unwrap_or_default(),
            &comments_js,
            site_url.as_deref(),
            ssh_port,
            &site_title,
            a.first_image.as_deref(),
        );
        let hp = stage_web.join(&a.path.key).join("index.html");
        if let Some(parent) = hp.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::write(&hp, page).with_context(|| format!("write {}", hp.display()))?;
    }

    // 列表数据 + 列表页
    std::fs::write(rendered.join(".index"), meta::build_index(&arts))
        .with_context(|| format!("write {}", rendered.join(".index").display()))?;
    let machine_index = meta::build_machine_index(&arts);
    machine_index.validate().map_err(anyhow::Error::msg)?;
    std::fs::write(
        rendered.join(".index.json"),
        serde_json::to_vec_pretty(&machine_index)?,
    )
    .with_context(|| format!("write {}", rendered.join(".index.json").display()))?;
    std::fs::create_dir_all(&dist_blog)
        .with_context(|| format!("create {}", dist_blog.display()))?;
    let entry_js_str = entry_js.as_deref().unwrap_or_default();
    let list = html::render_list_page(&arts, site_url.as_deref(), &site_title, entry_js_str);
    std::fs::write(dist_blog.join("index.html"), list)
        .with_context(|| format!("write {}", dist_blog.join("index.html").display()))?;

    // sitemap / atom(有 site_url 时)与 robots.txt(总是)
    if let Some(u) = &site_url {
        write_stage(&stage_web, "sitemap.xml", feed::sitemap(u, &arts))?;
        let feed_updated = arts
            .first()
            .map(|a| a.date_rfc3339.clone())
            .unwrap_or_else(|| chrono::Local::now().to_rfc3339());
        write_stage(
            &stage_web,
            "atom.xml",
            feed::atom(u, &site_title, &arts, &feed_updated),
        )?;
    } else {
        warns
            .push("site_url not set, skipping sitemap.xml / atom.xml (mirror pages lack canonical/OG:url)".into());
    }
    write_stage(&stage_web, "robots.txt", feed::robots(site_url.as_deref()))?;

    // staging 文件集合必须与预先通过冲突检查的 claim 集合完全一致。
    let mut staged_files = Vec::new();
    walk_regular_files(&stage_web, &stage_web, &mut staged_files)?;
    let staged_set: BTreeSet<String> = staged_files.into_iter().collect();
    let desired_set: BTreeSet<String> = desired_outputs.keys().cloned().collect();
    if staged_set != desired_set {
        bail!(
            "internal error: staging does not match Web claims: staging={staged_set:?}, claims={desired_set:?}"
        );
    }

    let web_manifest: String = desired_outputs
        .iter()
        .map(|(rel, owner)| format!("{rel}\t{owner}\n"))
        .collect();
    commit_web_outputs(&cli.dist, &stage_web, &old_outputs, has_web_manifest)?;
    replace_tree(&rendered, &cli.content.join(".rendered"))?;
    replace_tree(&rendered_assets, &cli.content.join(".rendered-assets"))?;
    atomic_write(
        &cli.content.join(".comment-targets.tsv"),
        target_manifest.as_bytes(),
    )?;
    atomic_write(
        &cli.content.join(".web-outputs.tsv"),
        web_manifest.as_bytes(),
    )?;

    // 摘要
    println!("content-build: {} article(s)", arts.len());
    println!(
        "  mirror pages:   {}/<article-key>/index.html",
        cli.dist.display()
    );
    println!(
        "  global list (fixed URL /blog/): {}/blog/index.html",
        cli.dist.display()
    );
    println!("  ANSI pre-rendered: {}/.rendered/", cli.content.display());
    println!(
        "  inline images: {} file(s), total {} KiB (budget {} KiB/article)",
        total_inline_imgs,
        total_inline_bytes / 1024,
        img::MAX_ARTICLE_BYTES / 1024
    );
    println!(
        "  linked originals: {} file(s), total {} KiB (no size limit)",
        total_link_imgs,
        total_link_bytes / 1024
    );
    println!(
        "  processed images: {}/.rendered-assets/",
        cli.content.display()
    );
    for w in &warns {
        println!("warning: {w}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_routes_cover_system_paths_but_not_ordinary_dirs() {
        // 计划 §6.2 预注册的系统占用: blog 精确为全站列表, ws 精确为 WebSocket,
        // api/assets/fonts 为整个前缀。
        for key in [
            "blog",
            "ws",
            "api",
            "api/comments",
            "assets",
            "assets/x",
            "fonts",
            "fonts/x",
        ] {
            assert!(reserved_route_owner(key).is_some(), "应保留: {key}");
        }
        // blog 作为普通目录仍可用, 仅前缀相似或普通文章不得误伤。
        for key in [
            "help",
            "notes/unix",
            "blog/hello",
            "blog-2026",
            "assets-foo",
            "fonts-serif",
        ] {
            assert!(reserved_route_owner(key).is_none(), "不应保留: {key}");
        }
    }

    #[test]
    fn claim_prefix_logic_distinguishes_dirs_from_child_files() {
        // 目录以尾 / 登记: 与目录内新文件不冲突(文章可以在既有目录下建页面)。
        let mut claims = BTreeMap::new();
        let mut errors = Vec::new();
        claim_output(
            &mut claims,
            "help/",
            "existing directory help/",
            &mut errors,
        );
        claim_output(
            &mut claims,
            "help/index.html",
            "article help.md",
            &mut errors,
        );
        assert!(errors.is_empty(), "目录与子文件不应冲突: {errors:?}");

        // 同一输出的第二个 owner → 精确冲突。
        claim_output(
            &mut claims,
            "help/index.html",
            "another source help/index.html",
            &mut errors,
        );
        assert_eq!(errors.len(), 1, "同路径双 owner 应恰一处冲突: {errors:?}");

        // 文件与同名目录结构冲突(目录登记为 new/index.html/)。
        let mut claims = BTreeMap::new();
        let mut errors = Vec::new();
        claim_output(
            &mut claims,
            "new/index.html/",
            "existing directory new/index.html/",
            &mut errors,
        );
        claim_output(&mut claims, "new/index.html", "article new.md", &mut errors);
        assert_eq!(errors.len(), 1, "文件与同名目录应冲突: {errors:?}");
    }

    #[test]
    fn configured_scopes_map_root_nested_and_empty_directories() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("notes/deep")).unwrap();
        std::fs::create_dir_all(directory.path().join("empty")).unwrap();
        std::fs::create_dir_all(directory.path().join("proc")).unwrap();
        std::fs::write(
            directory.path().join(".termblog.toml"),
            "[scopes]\ndirectories = [\"\", \"notes/deep\", \"empty\", \"proc\"]\n",
        )
        .unwrap();
        let scopes = load_scopes(directory.path()).unwrap();
        assert_eq!(
            scopes
                .iter()
                .map(|scope| scope.stat_rel.as_str())
                .collect::<Vec<_>>(),
            vec![
                "proc/stat",
                "proc/empty/stat",
                "proc/notes/deep/stat",
                "proc/proc/stat",
            ]
        );
    }

    #[test]
    fn configured_scope_rejects_duplicates_and_comment_collisions() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join(".termblog.toml");
        std::fs::write(&config, "[scopes]\ndirectories = [\"\", \"\"]\n").unwrap();
        assert!(load_scopes(directory.path()).is_err());

        std::fs::write(&config, "[scopes]\ndirectories = [\"\"]\n").unwrap();
        std::fs::create_dir(directory.path().join("proc")).unwrap();
        assert!(
            load_scopes(directory.path()).is_ok(),
            "HOME proc is ordinary content"
        );

        std::fs::write(directory.path().join("comment"), b"occupied").unwrap();
        let error = load_scopes(directory.path()).unwrap_err().to_string();
        assert!(error.contains("comment"));

        std::fs::remove_file(directory.path().join("comment")).unwrap();
        std::fs::write(
            &config,
            "[scopes]\ndirectories = []\n[comments]\ndirectories = []\n",
        )
        .unwrap();
        assert!(load_scopes(directory.path()).is_err());

        std::fs::write(&config, "[comments]\ndirectories = [\"\"]\n").unwrap();
        assert_eq!(
            load_scopes(directory.path()).unwrap()[0].stat_rel,
            "proc/stat"
        );
    }
}
