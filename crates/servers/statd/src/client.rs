use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use termblog_core::Link;
use termblog_proto::Frame;

use crate::protocol::*;

#[derive(Debug, Clone)]
pub struct Client {
    socket: PathBuf,
    timeout: Duration,
}

impl Client {
    pub fn new(socket: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            socket: socket.into(),
            timeout,
        }
    }

    pub fn path(&self) -> &Path {
        &self.socket
    }

    async fn request<T: Serialize, R: DeserializeOwned>(&self, kind: u8, req: &T) -> Result<R> {
        let operation = async {
            let link = Link::connect(&self.socket)
                .await
                .with_context(|| format!("connect {}", self.socket.display()))?;
            link.send(&Frame::json(kind, req)).await?;
            let frame = link.recv().await?;
            if frame.kind != kind {
                bail!("statd response kind mismatch: {} != {kind}", frame.kind);
            }
            frame.parse().context("parse statd response")
        };
        tokio::time::timeout(self.timeout, operation)
            .await
            .context("statd request timed out")?
    }

    pub async fn record_batch(&self, events: Vec<RecordEvent>) -> Result<usize> {
        let response: RecordBatchResponse = self
            .request(RECORD_BATCH, &RecordBatchRequest { events })
            .await?;
        if !response.ok {
            bail!(
                "statd record failed: {}",
                response.error.unwrap_or_else(|| "unknown error".into())
            );
        }
        Ok(response.accepted)
    }

    pub async fn snapshot(&self, request: &SnapshotRequest) -> Result<SnapshotResponse> {
        let response: SnapshotResponse = self.request(SNAPSHOT, request).await?;
        if !response.ok {
            bail!(
                "statd snapshot failed: {}",
                response.error.unwrap_or_else(|| "unknown error".into())
            );
        }
        Ok(response)
    }

    /// The jail preparation path is already on a dedicated blocking thread.
    /// A short-lived single-thread runtime lets it issue the one pre-fork
    /// snapshot request without ever carrying a Tokio runtime across fork(2).
    pub fn snapshot_blocking(&self, request: &SnapshotRequest) -> Result<SnapshotResponse> {
        tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .context("build statd snapshot runtime")?
            .block_on(self.snapshot(request))
    }
}
