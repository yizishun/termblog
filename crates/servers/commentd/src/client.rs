use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use termblog_core::Link;
use termblog_proto::Frame;

use crate::protocol::*;

#[derive(Debug, Clone)]
pub struct Client {
    socket: PathBuf,
}

impl Client {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.socket
    }

    async fn request<T: Serialize, R: DeserializeOwned>(&self, kind: u8, req: &T) -> Result<R> {
        let link = Link::connect(&self.socket)
            .await
            .with_context(|| format!("连接 {}", self.socket.display()))?;
        link.send(&Frame::json(kind, req)).await?;
        let frame = link.recv().await?;
        if frame.kind != kind {
            bail!("commentd 响应 kind 不匹配: {} != {kind}", frame.kind);
        }
        frame.parse().context("解析 commentd 响应")
    }

    pub async fn public_query(&self, req: &PublicQuery) -> Result<PublicQueryResponse> {
        self.request(PUBLIC_QUERY, req).await
    }

    pub async fn submit(&self, req: &SubmitRequest) -> Result<SubmitResponse> {
        self.request(PRIVATE_SUBMIT, req).await
    }

    pub async fn page(&self, kind: u8, req: &PageRequest) -> Result<PageResponse> {
        self.request(kind, req).await
    }

    pub async fn moderate(&self, kind: u8, ids: Vec<u64>) -> Result<ModerateResponse> {
        self.request(kind, &ModerateRequest { ids }).await
    }

    /// 拉取固定 revision 的完整 approved/pending 快照；翻页遇变更便整轮重来。
    pub async fn all(&self, kind: u8) -> Result<Vec<Comment>> {
        for _ in 0..5 {
            let mut out = Vec::new();
            let mut after_id = 0;
            let mut revision: Option<String> = None;
            loop {
                let res = self
                    .page(
                        kind,
                        &PageRequest {
                            after_id,
                            limit: Some(MAX_LIMIT),
                            revision: revision.clone(),
                        },
                    )
                    .await?;
                if !res.ok {
                    if res.error.as_deref() == Some("stale_revision") {
                        break;
                    }
                    bail!(
                        "commentd 查询失败: {}",
                        res.error.unwrap_or_else(|| "未知错误".into())
                    );
                }
                if revision.is_none() {
                    revision = Some(res.revision.clone());
                }
                out.extend(res.comments);
                if !res.has_more {
                    return Ok(out);
                }
                after_id = res
                    .next_after_id
                    .ok_or_else(|| anyhow::anyhow!("commentd has_more 缺 next_after_id"))?;
            }
        }
        bail!("commentd 数据持续变化，无法取得一致快照")
    }
}
