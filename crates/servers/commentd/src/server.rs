use std::os::unix::fs::{chown, FileTypeExt, PermissionsExt};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use nix::unistd::Group;
use termblog_config::CommentsConfig;
use termblog_core::{Link, LinkListener};
use termblog_proto::Frame;
use tracing::{error, info, warn};

use crate::protocol::*;
use crate::store::Store;

#[derive(Clone, Copy)]
enum Side {
    Public,
    Private,
}

pub async fn run(cfg: CommentsConfig) -> Result<()> {
    // bind 到 chmod 之间也不能让非 root 抢先 connect private socket；daemon
    // 后续创建的文件都显式给 mode，因此保留严格 umask 不影响功能。
    unsafe { libc::umask(0o077) };

    let store = Arc::new(Mutex::new(Store::open(&cfg.data_dir)?));
    prepare_socket(&cfg.public_socket)?;
    prepare_socket(&cfg.private_socket)?;
    let mut public = LinkListener::bind(&cfg.public_socket)
        .with_context(|| format!("bind {}", cfg.public_socket.display()))?;
    let mut private = LinkListener::bind(&cfg.private_socket)
        .with_context(|| format!("bind {}", cfg.private_socket.display()))?;
    set_socket_mode(&cfg.public_socket, 0o660, "www")?;
    set_socket_mode(&cfg.private_socket, 0o600, "wheel")?;
    info!(public = %cfg.public_socket.display(), private = %cfg.private_socket.display(), "commentd 已就绪");

    let public_store = store.clone();
    let public_loop = async move {
        loop {
            match public.accept().await {
                Ok(link) => {
                    let store = public_store.clone();
                    tokio::spawn(async move { handle(link, Side::Public, store).await });
                }
                Err(e) => error!(%e, "public accept 失败"),
            }
        }
    };
    let private_loop = async move {
        loop {
            match private.accept().await {
                Ok(link) => {
                    let store = store.clone();
                    tokio::spawn(async move { handle(link, Side::Private, store).await });
                }
                Err(e) => error!(%e, "private accept 失败"),
            }
        }
    };
    tokio::join!(public_loop, private_loop);
    Ok(())
}

async fn handle(link: Link, side: Side, store: Arc<Mutex<Store>>) {
    let frame = match link.recv().await {
        Ok(f) => f,
        Err(e) => {
            warn!(%e, "commentd 收帧失败");
            return;
        }
    };
    let kind = frame.kind;
    let response = match side {
        Side::Public if kind == PUBLIC_QUERY => match frame.parse::<PublicQuery>() {
            Ok(req) => serde_json::to_value(store.lock().unwrap().public_query(req)),
            Err(e) => serde_json::to_value(ErrorResponse {
                ok: false,
                error: format!("请求 JSON 非法: {e}"),
            }),
        },
        Side::Public => serde_json::to_value(ErrorResponse {
            ok: false,
            error: "public socket 只允许 approved query".into(),
        }),
        Side::Private => match kind {
            PRIVATE_SUBMIT => match frame.parse::<SubmitRequest>() {
                Ok(req) => {
                    serde_json::to_value(store.lock().unwrap().submit(req).unwrap_or_else(|e| {
                        SubmitResponse {
                            ok: false,
                            id: None,
                            notice: "评论服务持久化失败".into(),
                            error: Some(e.to_string()),
                        }
                    }))
                }
                Err(e) => serde_json::to_value(ErrorResponse {
                    ok: false,
                    error: format!("请求 JSON 非法: {e}"),
                }),
            },
            PRIVATE_SYNC | PRIVATE_QUEUE => match frame.parse::<PageRequest>() {
                Ok(req) => {
                    serde_json::to_value(store.lock().unwrap().page(req, kind == PRIVATE_QUEUE))
                }
                Err(e) => serde_json::to_value(ErrorResponse {
                    ok: false,
                    error: format!("请求 JSON 非法: {e}"),
                }),
            },
            PRIVATE_APPROVE | PRIVATE_REJECT => match frame.parse::<ModerateRequest>() {
                Ok(req) => serde_json::to_value(
                    store
                        .lock()
                        .unwrap()
                        .moderate(&req.ids, kind == PRIVATE_APPROVE)
                        .unwrap_or_else(|e| ModerateResponse {
                            ok: false,
                            changed: 0,
                            error: Some(e.to_string()),
                        }),
                ),
                Err(e) => serde_json::to_value(ErrorResponse {
                    ok: false,
                    error: format!("请求 JSON 非法: {e}"),
                }),
            },
            _ => serde_json::to_value(ErrorResponse {
                ok: false,
                error: "private socket kind 非法".into(),
            }),
        },
    };
    let value =
        response.unwrap_or_else(|e| serde_json::json!({"ok": false, "error": e.to_string()}));
    let _ = link.send(&Frame::json(kind, &value)).await;
    if store.lock().unwrap().is_poisoned() {
        error!("目录 fsync 失败，commentd fail-stop");
        std::process::exit(1);
    }
    // 一连接一请求：发送一帧后直接 drop link。
}

fn prepare_socket(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_socket() => {
            std::fs::remove_file(path).with_context(|| format!("删除旧 socket {}", path.display()))
        }
        Ok(_) => bail!("{} 已存在且不是 socket，拒绝覆盖", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn set_socket_mode(path: &Path, mode: u32, group: &str) -> Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    match Group::from_name(group) {
        Ok(Some(g)) => chown(path, None, Some(g.gid.as_raw()))?,
        Ok(None) => warn!(group, "系统组不存在，socket 保持当前属组"),
        Err(e) => warn!(%e, group, "查询系统组失败"),
    }
    Ok(())
}
