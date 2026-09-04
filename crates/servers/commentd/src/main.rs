use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use nix::unistd::Uid;
use termblog_commentd::protocol::*;
use termblog_commentd::{server, store::Store, Client};
use termblog_config::Config;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    if !Uid::effective().is_root() {
        bail!("commentd/commentctl 只能以 root 运行");
    }
    let argv0 = std::env::args().next().unwrap_or_else(|| "commentd".into());
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let forced_ctl = args.first().is_some_and(|a| a == "commentctl");
    if forced_ctl {
        args.remove(0);
    }
    let config_path = take_option(&mut args, "--config")?.map(PathBuf::from);
    let cfg_path = config_path.or_else(|| std::env::var("TERMBLOG_CONFIG").ok().map(PathBuf::from));
    let cfg = Config::load(cfg_path.as_deref())?;
    let prog = Path::new(&argv0)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("commentd");
    if prog == "commentctl" || forced_ctl {
        return ctl(&cfg, &args).await;
    }
    if args.as_slice() == ["--init"] {
        Store::init(&cfg.comments.data_dir)?;
        println!("已初始化 {}", cfg.comments.data_dir.display());
        return Ok(());
    }
    if !args.is_empty() {
        bail!("用法: commentd [--config FILE] [--init]");
    }
    server::run(cfg.comments).await
}

async fn ctl(cfg: &Config, args: &[String]) -> Result<()> {
    let Some(cmd) = args.first().map(String::as_str) else {
        bail!("用法: commentctl queue [--after-id N] [--limit N] | approve <id…>|--all | reject <id…>");
    };
    let client = Client::new(&cfg.comments.private_socket);
    match cmd {
        "queue" => queue(&client, &args[1..]).await,
        "approve" => {
            let ids = if args.get(1).map(String::as_str) == Some("--all") {
                if args.len() != 2 {
                    bail!("--all 不能与 ID 混用");
                }
                client
                    .all(PRIVATE_QUEUE)
                    .await?
                    .into_iter()
                    .map(|c| c.id)
                    .collect()
            } else {
                parse_ids(&args[1..])?
            };
            if ids.is_empty() {
                println!("changed=0");
                return Ok(());
            }
            let res = client.moderate(PRIVATE_APPROVE, ids).await?;
            print_moderate(res)
        }
        "reject" => {
            let res = client
                .moderate(PRIVATE_REJECT, parse_ids(&args[1..])?)
                .await?;
            print_moderate(res)
        }
        _ => bail!("未知 commentctl 命令: {cmd}"),
    }
}

async fn queue(client: &Client, args: &[String]) -> Result<()> {
    const MAX_STALE_RETRIES: usize = 5;
    let mut args = args.to_vec();
    let after = take_option(&mut args, "--after-id")?
        .map(|s| s.parse::<u64>().context("--after-id 必须是整数"))
        .transpose()?
        .unwrap_or(0);
    let limit = take_option(&mut args, "--limit")?
        .map(|s| s.parse::<u16>().context("--limit 必须是整数"))
        .transpose()?
        .unwrap_or(DEFAULT_LIMIT);
    page_limit(Some(limit)).map_err(anyhow::Error::msg)?;
    if !args.is_empty() {
        bail!("queue 参数非法: {args:?}");
    }
    let mut cursor = after;
    let mut revision = if after > 0 {
        Some(queue_revision(client).await?)
    } else {
        None
    };
    let mut rows = Vec::new();
    let mut stale_retries = 0;
    loop {
        let res = client
            .page(
                PRIVATE_QUEUE,
                &PageRequest {
                    after_id: cursor,
                    limit: Some(limit),
                    revision: revision.clone(),
                },
            )
            .await?;
        if !res.ok {
            if res.error.as_deref() == Some("stale_revision") {
                stale_retries += 1;
                if stale_retries >= MAX_STALE_RETRIES {
                    bail!("commentd 数据持续变化，无法取得一致 queue");
                }
                rows.clear();
                cursor = after;
                revision = if after > 0 {
                    Some(queue_revision(client).await?)
                } else {
                    None
                };
                continue;
            }
            bail!("queue 失败: {}", res.error.unwrap_or_default());
        }
        revision.get_or_insert(res.revision.clone());
        rows.extend(res.comments);
        if !res.has_more {
            for c in rows {
                println!(
                    "#{}\t{}\t{}\t{}\t{}",
                    c.id, c.target, c.author, c.created_at, c.text
                );
            }
            break;
        }
        cursor = res.next_after_id.context("has_more 缺 next_after_id")?;
    }
    Ok(())
}

async fn queue_revision(client: &Client) -> Result<String> {
    let res = client
        .page(
            PRIVATE_QUEUE,
            &PageRequest {
                after_id: 0,
                limit: Some(1),
                revision: None,
            },
        )
        .await?;
    if !res.ok {
        bail!(
            "queue 失败: {}",
            res.error.unwrap_or_else(|| "未知错误".into())
        );
    }
    Ok(res.revision)
}
fn parse_ids(args: &[String]) -> Result<Vec<u64>> {
    if args.is_empty() {
        bail!("至少需要一个评论 ID");
    }
    args.iter()
        .map(|s| s.parse::<u64>().with_context(|| format!("非法 ID: {s}")))
        .collect()
}

fn print_moderate(res: ModerateResponse) -> Result<()> {
    if !res.ok {
        bail!("审核失败: {}", res.error.unwrap_or_default());
    }
    println!("changed={}", res.changed);
    Ok(())
}

fn take_option(args: &mut Vec<String>, name: &str) -> Result<Option<String>> {
    let Some(pos) = args.iter().position(|a| a == name) else {
        return Ok(None);
    };
    if pos + 1 >= args.len() {
        bail!("{name} 缺少值");
    }
    let value = args.remove(pos + 1);
    args.remove(pos);
    Ok(Some(value))
}
