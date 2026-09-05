use std::os::unix::fs::{chown, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use nix::unistd::Group;
use termblog_config::StatsConfig;
use termblog_core::{Link, LinkListener};
use termblog_proto::Frame;
use tracing::{error, info, warn};

use crate::protocol::*;
use crate::store::Store;

pub async fn run(cfg: StatsConfig) -> Result<()> {
    unsafe { libc::umask(0o077) };
    let store = Arc::new(Mutex::new(Store::open(&cfg.data_dir)?));
    prepare_socket(&cfg.socket)?;
    let mut listener = LinkListener::bind(&cfg.socket)
        .with_context(|| format!("bind {}", cfg.socket.display()))?;
    set_socket_mode(&cfg.socket)?;
    info!(socket = %cfg.socket.display(), "termblog-statd ready");

    loop {
        match listener.accept().await {
            Ok(link) => {
                let store = store.clone();
                tokio::spawn(async move { handle(link, store).await });
            }
            Err(error) => error!(%error, "statd accept failed"),
        }
    }
}

async fn handle(link: Link, store: Arc<Mutex<Store>>) {
    let frame = match link.recv().await {
        Ok(frame) => frame,
        Err(error) => {
            warn!(%error, "statd receive failed");
            return;
        }
    };
    let kind = frame.kind;
    let response = match kind {
        RECORD_BATCH => match frame.parse::<RecordBatchRequest>() {
            Ok(request) => match store.lock().unwrap().record_batch(request) {
                Ok(accepted) => serde_json::to_value(RecordBatchResponse {
                    ok: true,
                    accepted,
                    error: None,
                }),
                Err(error) => serde_json::to_value(RecordBatchResponse {
                    ok: false,
                    accepted: 0,
                    error: Some(error.to_string()),
                }),
            },
            Err(error) => serde_json::to_value(RecordBatchResponse {
                ok: false,
                accepted: 0,
                error: Some(format!("invalid request JSON: {error}")),
            }),
        },
        SNAPSHOT => match frame.parse::<SnapshotRequest>() {
            Ok(request) => match store.lock().unwrap().snapshot(request) {
                Ok(response) => serde_json::to_value(response),
                Err(error) => serde_json::to_value(SnapshotResponse {
                    ok: false,
                    snapshot_at: String::new(),
                    targets: Vec::new(),
                    error: Some(error.to_string()),
                }),
            },
            Err(error) => serde_json::to_value(SnapshotResponse {
                ok: false,
                snapshot_at: String::new(),
                targets: Vec::new(),
                error: Some(format!("invalid request JSON: {error}")),
            }),
        },
        _ => serde_json::to_value(ErrorResponse {
            ok: false,
            error: format!("unknown statd request kind {kind}"),
        }),
    };
    let Ok(response) = response else {
        return;
    };
    if let Err(error) = link.send(&Frame::json(kind, &response)).await {
        warn!(%error, "statd response failed");
    }
}

fn prepare_socket(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_socket()
                || metadata.file_type().is_symlink()
                || metadata.uid() != 0
            {
                bail!("refusing to replace non-root socket at {}", path.display());
            }
            std::fs::remove_file(path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    }
    Ok(())
}

fn set_socket_mode(path: &Path) -> Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    match Group::from_name("www") {
        Ok(Some(group)) => chown(path, None, Some(group.gid.as_raw()))?,
        Ok(None) => warn!("system has no www group; statd socket keeps root group"),
        Err(error) => return Err(error).context("query www group"),
    }
    Ok(())
}
