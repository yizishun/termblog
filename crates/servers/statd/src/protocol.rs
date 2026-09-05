use serde::{Deserialize, Serialize};

pub const RECORD_BATCH: u8 = 1;
pub const SNAPSHOT: u8 = 2;

pub const MAX_BATCH_EVENTS: usize = 256;
pub const MAX_SNAPSHOT_TARGETS: usize = 256;
pub const MAX_SNAPSHOT_ARTICLES: usize = 1024;
pub const MAX_ARTICLE_KEY_LEN: usize = 512;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    TerminalReadSession,
    StaticRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecordEvent {
    pub source: Source,
    pub target: String,
    pub article: String,
    pub ip: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecordBatchRequest {
    pub events: Vec<RecordEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecordBatchResponse {
    pub ok: bool,
    pub accepted: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SnapshotRequest {
    pub targets: Vec<SnapshotTargetRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SnapshotTargetRequest {
    pub target: String,
    pub articles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotResponse {
    pub ok: bool,
    pub snapshot_at: String,
    pub targets: Vec<TargetSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TargetSnapshot {
    pub target: String,
    pub terminal_read_sessions_total: u64,
    pub static_requests_total: u64,
    pub unique_visitors_approx: u64,
    pub articles: Vec<ArticleSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArticleSnapshot {
    pub article: String,
    pub terminal_read_sessions: u64,
    pub static_requests: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorResponse {
    pub ok: bool,
    pub error: String,
}
