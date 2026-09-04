use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::protocol::{
    page_limit, valid_target, Comment, ModerateResponse, PageRequest, PageResponse, PublicQuery,
    PublicQueryResponse, SubmitRequest, SubmitResponse,
};

const DATA_FILE: &str = "comments.jsonl";
const SALT_FILE: &str = "salt";
const MARKER_FILE: &str = "initialized";
const MARKER: &[u8] = b"termblog-commentd-v1\n";
const SALT_LEN: usize = 32;
static TEMP_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Status {
    Pending,
    Approved,
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredComment {
    id: u64,
    target: String,
    author: String,
    text: String,
    ip_hash: String,
    created_at: String,
    status: Status,
}

impl StoredComment {
    fn view(&self) -> Comment {
        Comment {
            id: self.id,
            target: self.target.clone(),
            author: self.author.clone(),
            text: self.text.clone(),
            created_at: self.created_at.clone(),
        }
    }
}

/// commentd 是唯一写者；所有修改在候选 Vec 上完成，持久化成功才替换这里。
pub struct Store {
    dir: PathBuf,
    salt: [u8; SALT_LEN],
    comments: Vec<StoredComment>,
    revision: String,
    poisoned: bool,
}

impl Store {
    pub fn init(dir: &Path) -> Result<()> {
        match std::fs::symlink_metadata(dir) {
            Ok(md) => {
                if !md.file_type().is_dir() || md.file_type().is_symlink() {
                    bail!("数据目录必须是真实目录");
                }
                if md.uid() != 0 || md.gid() != 0 {
                    bail!("数据目录必须是 root:wheel");
                }
                if std::fs::read_dir(dir)?.next().is_some() {
                    bail!("数据目录 {} 非空，拒绝初始化", dir.display());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(dir).with_context(|| format!("创建 {}", dir.display()))?;
            }
            Err(e) => return Err(e).with_context(|| format!("读取 {}", dir.display())),
        }
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        validate_dir(dir)?;

        let mut salt = [0u8; SALT_LEN];
        File::open("/dev/urandom")
            .context("打开 /dev/urandom")?
            .read_exact(&mut salt)
            .context("读取随机 salt")?;
        create_synced(dir, SALT_FILE, &salt)?;
        create_synced(dir, DATA_FILE, b"")?;
        // marker 最后写：缺 marker 的半初始化目录永远不会被当成空库启动。
        create_synced(dir, MARKER_FILE, MARKER)?;
        Ok(())
    }

    pub fn open(dir: &Path) -> Result<Self> {
        validate_dir(dir)?;
        for name in [SALT_FILE, DATA_FILE, MARKER_FILE] {
            validate_file(&dir.join(name)).with_context(|| format!("校验 {name}"))?;
        }
        let marker = std::fs::read(dir.join(MARKER_FILE))?;
        if marker != MARKER {
            bail!("initialized 标记非法");
        }
        let salt_vec = std::fs::read(dir.join(SALT_FILE))?;
        let salt: [u8; SALT_LEN] = salt_vec
            .try_into()
            .map_err(|_| anyhow!("salt 必须恰为 {SALT_LEN} 字节"))?;
        let raw = std::fs::read(dir.join(DATA_FILE))?;
        let text = std::str::from_utf8(&raw).context("comments.jsonl 不是 UTF-8")?;
        let mut comments = Vec::new();
        let mut last_id = 0;
        for (idx, line) in text.lines().enumerate() {
            if line.is_empty() {
                bail!("comments.jsonl 第 {} 行为空", idx + 1);
            }
            let c: StoredComment = serde_json::from_str(line)
                .with_context(|| format!("comments.jsonl 第 {} 行 JSON 非法", idx + 1))?;
            validate_stored(&c).with_context(|| format!("comments.jsonl 第 {} 行", idx + 1))?;
            if c.id <= last_id {
                bail!("comments.jsonl 第 {} 行 ID 未严格递增", idx + 1);
            }
            last_id = c.id;
            comments.push(c);
        }
        let canonical = encode(&comments)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            salt,
            comments,
            revision: revision(&canonical),
            poisoned: false,
        })
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    pub fn submit(&mut self, req: SubmitRequest) -> Result<SubmitResponse> {
        if self.poisoned {
            bail!("存储已进入 fail-stop 状态");
        }
        let reject = |msg: &str| SubmitResponse {
            ok: false,
            id: None,
            notice: format!("评论未提交：{msg}"),
            error: Some(msg.to_string()),
        };
        if !valid_target(&req.target) {
            return Ok(reject("target 非法"));
        }
        if req.line.len() > 512 {
            return Ok(reject("单行超过 512 字节"));
        }
        if req.ip.parse::<std::net::IpAddr>().is_err() {
            return Ok(reject("IP 非法"));
        }
        let Some((author, text)) = normalize_line(&req.line) else {
            return Ok(reject("清洗后正文为空"));
        };
        if author.len() > 32 {
            return Ok(reject("名字超过 32 字节"));
        }
        if text.len() > 512 {
            return Ok(reject("正文超过 512 字节"));
        }

        let ip_hash = hash_ip(&self.salt, &req.ip);
        let now = Utc::now();
        let recent = |c: &StoredComment, seconds: i64| {
            DateTime::parse_from_rfc3339(&c.created_at)
                .ok()
                .map(|t| {
                    now.signed_duration_since(t.with_timezone(&Utc))
                        .num_seconds()
                        <= seconds
                })
                .unwrap_or(false)
        };
        if self
            .comments
            .iter()
            .filter(|c| c.ip_hash == ip_hash && recent(c, 3600))
            .count()
            >= 10
        {
            return Ok(reject("同一来源每小时最多 10 条"));
        }
        if self.comments.iter().any(|c| {
            c.ip_hash == ip_hash && c.target == req.target && c.text == text && recent(c, 300)
        }) {
            return Ok(reject("5 分钟内请勿重复提交相同内容"));
        }

        let id = self
            .comments
            .last()
            .map_or(Some(1), |c| c.id.checked_add(1))
            .ok_or_else(|| anyhow!("评论 ID 已耗尽"))?;
        let mut candidate = self.comments.clone();
        candidate.push(StoredComment {
            id,
            target: req.target.clone(),
            author,
            text,
            ip_hash,
            created_at: now.to_rfc3339_opts(SecondsFormat::Secs, true),
            status: Status::Pending,
        });
        self.commit(candidate)?;
        Ok(SubmitResponse {
            ok: true,
            id: Some(id),
            notice: format!("[#{id}] 已投入待审队列，归属 {}", req.target),
            error: None,
        })
    }

    pub fn public_query(&self, req: PublicQuery) -> PublicQueryResponse {
        let err = |message: String| PublicQueryResponse {
            ok: false,
            revision: self.revision.clone(),
            total: 0,
            omitted_earlier: 0,
            comments: vec![],
            next_after_id: None,
            has_more: false,
            error: Some(message),
        };
        if !valid_target(&req.target) {
            return err("target 非法".into());
        }
        let limit = match page_limit(req.limit) {
            Ok(n) => n,
            Err(e) => return err(e.into()),
        };
        if req.after_id.unwrap_or(0) > 0 && req.revision.is_none() {
            return err("after_id>0 时 revision 必填".into());
        }
        if let Some(r) = &req.revision {
            if r != &self.revision {
                return err("stale_revision".into());
            }
        }
        let all: Vec<_> = self
            .comments
            .iter()
            .filter(|c| c.status == Status::Approved && c.target == req.target)
            .map(StoredComment::view)
            .collect();
        let total = all.len();
        if req.after_id.is_none() {
            let start = total.saturating_sub(limit);
            return PublicQueryResponse {
                ok: true,
                revision: self.revision.clone(),
                total,
                omitted_earlier: start,
                comments: all[start..].to_vec(),
                next_after_id: None,
                has_more: false,
                error: None,
            };
        }
        let after = req.after_id.unwrap_or(0);
        let mut rest = all.into_iter().filter(|c| c.id > after);
        let comments: Vec<_> = rest.by_ref().take(limit).collect();
        let has_more = rest.next().is_some();
        let next_after_id = has_more.then(|| comments.last().map(|c| c.id)).flatten();
        PublicQueryResponse {
            ok: true,
            revision: self.revision.clone(),
            total,
            omitted_earlier: 0,
            comments,
            next_after_id,
            has_more,
            error: None,
        }
    }

    pub fn page(&self, req: PageRequest, pending: bool) -> PageResponse {
        let err = |message: String| PageResponse {
            ok: false,
            revision: self.revision.clone(),
            comments: vec![],
            next_after_id: None,
            has_more: false,
            error: Some(message),
        };
        let limit = match page_limit(req.limit) {
            Ok(n) => n,
            Err(e) => return err(e.into()),
        };
        if req.after_id > 0 && req.revision.is_none() {
            return err("after_id>0 时 revision 必填".into());
        }
        if let Some(r) = &req.revision {
            if r != &self.revision {
                return err("stale_revision".into());
            }
        }
        let wanted = if pending {
            Status::Pending
        } else {
            Status::Approved
        };
        let mut iter = self
            .comments
            .iter()
            .filter(|c| c.status == wanted && c.id > req.after_id)
            .map(StoredComment::view);
        let comments: Vec<_> = iter.by_ref().take(limit).collect();
        let has_more = iter.next().is_some();
        let next_after_id = has_more.then(|| comments.last().map(|c| c.id)).flatten();
        PageResponse {
            ok: true,
            revision: self.revision.clone(),
            comments,
            next_after_id,
            has_more,
            error: None,
        }
    }

    pub fn moderate(&mut self, ids: &[u64], approve: bool) -> Result<ModerateResponse> {
        if self.poisoned {
            bail!("存储已进入 fail-stop 状态");
        }
        if ids.is_empty() {
            return Ok(ModerateResponse {
                ok: false,
                changed: 0,
                error: Some("ids 不能为空".into()),
            });
        }
        let ids: HashSet<u64> = ids.iter().copied().collect();
        let mut candidate = self.comments.clone();
        let mut changed = 0;
        for c in &mut candidate {
            if c.status == Status::Pending && ids.contains(&c.id) {
                c.status = if approve {
                    Status::Approved
                } else {
                    Status::Deleted
                };
                changed += 1;
            }
        }
        if changed > 0 {
            self.commit(candidate)?;
        }
        Ok(ModerateResponse {
            ok: true,
            changed,
            error: None,
        })
    }

    fn commit(&mut self, candidate: Vec<StoredComment>) -> Result<()> {
        let bytes = encode(&candidate)?;
        let new_revision = revision(&bytes);
        // 崩溃遗留的临时文件与复用 PID 不得让后续提交永久失败。
        let (temp, mut f) = loop {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let temp = self
                .dir
                .join(format!(".comments.tmp.{}.{}", std::process::id(), seq));
            let mut opts = OpenOptions::new();
            opts.write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
            match opts.open(&temp) {
                Ok(f) => break (temp, f),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).with_context(|| format!("创建 {}", temp.display())),
            }
        };
        let pre_rename = (|| -> Result<()> {
            f.write_all(&bytes)?;
            f.sync_all()?;
            std::fs::rename(&temp, self.dir.join(DATA_FILE))?;
            Ok(())
        })();
        if let Err(e) = pre_rename {
            let _ = std::fs::remove_file(&temp);
            return Err(e);
        }
        // rename 已发生；目录 fsync 失败后不能继续以旧内存提供服务。
        if let Err(e) = File::open(&self.dir).and_then(|f| f.sync_all()) {
            self.poisoned = true;
            return Err(anyhow!("目录 fsync 失败，进入 fail-stop: {e}"));
        }
        self.comments = candidate;
        self.revision = new_revision;
        Ok(())
    }
}

fn normalize_line(line: &str) -> Option<(String, String)> {
    let clean: String = line
        .chars()
        .filter(|c| !matches!(*c as u32, 0x00..=0x1f | 0x7f..=0x9f))
        .collect();
    let clean = clean.trim();
    if clean.is_empty() {
        return None;
    }
    if let Some((left, right)) = clean.split_once(':') {
        let author = left.trim();
        let text = right.trim();
        if !author.is_empty() && author.len() <= 32 && !text.is_empty() {
            return Some((author.to_string(), text.to_string()));
        }
    }
    Some(("guest".into(), clean.to_string()))
}

fn validate_stored(c: &StoredComment) -> Result<()> {
    if c.id == 0 {
        bail!("ID 必须大于 0");
    }
    if !valid_target(&c.target) {
        bail!("target 非法");
    }
    if c.author.is_empty() || c.author.len() > 32 || c.text.is_empty() || c.text.len() > 512 {
        bail!("author/text 长度非法");
    }
    if normalize_controls(&c.author) != c.author || normalize_controls(&c.text) != c.text {
        bail!("author/text 含控制字符");
    }
    if c.ip_hash.len() != 64 || !c.ip_hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("ip_hash 非法");
    }
    DateTime::parse_from_rfc3339(&c.created_at).context("created_at 非 RFC3339")?;
    Ok(())
}

fn normalize_controls(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(*c as u32, 0x00..=0x1f | 0x7f..=0x9f))
        .collect()
}

fn encode(comments: &[StoredComment]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for c in comments {
        serde_json::to_writer(&mut out, c)?;
        out.push(b'\n');
    }
    Ok(out)
}

fn revision(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn hash_ip(salt: &[u8; SALT_LEN], ip: &str) -> String {
    let mut h = Sha256::new();
    h.update(salt);
    h.update(ip.as_bytes());
    format!("{:x}", h.finalize())
}

fn validate_dir(dir: &Path) -> Result<()> {
    let md = std::fs::symlink_metadata(dir)
        .with_context(|| format!("读取数据目录 {}", dir.display()))?;
    if !md.file_type().is_dir() || md.file_type().is_symlink() {
        bail!("数据目录必须是真实目录");
    }
    if md.uid() != 0 || md.gid() != 0 {
        bail!("数据目录 owner 必须是 root");
    }
    if md.mode() & 0o022 != 0 {
        bail!("数据目录 group/other 不得可写");
    }
    Ok(())
}

fn validate_file(path: &Path) -> Result<()> {
    let md = std::fs::symlink_metadata(path)?;
    if !md.file_type().is_file() || md.file_type().is_symlink() {
        bail!("必须是普通文件且不能是 symlink");
    }
    if md.uid() != 0 || md.gid() != 0 || md.mode() & 0o777 != 0o600 {
        bail!("owner/mode 必须是 root:wheel 0600");
    }
    Ok(())
}

fn create_synced(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let path = dir.join(name);
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_store(dir: &Path) -> Store {
        Store {
            dir: dir.to_path_buf(),
            salt: [7; SALT_LEN],
            comments: vec![],
            revision: revision(b""),
            poisoned: false,
        }
    }

    #[test]
    fn prefix_and_controls() {
        assert_eq!(
            normalize_line(" alice: 好\u{1b}文 "),
            Some(("alice".into(), "好文".into()))
        );
        assert_eq!(
            normalize_line("无前缀"),
            Some(("guest".into(), "无前缀".into()))
        );
        assert_eq!(normalize_line("\u{7f}\n"), None);
    }

    #[test]
    fn submit_commit_query_and_moderate() {
        let td = tempfile::tempdir().unwrap();
        let mut s = memory_store(td.path());
        let r = s
            .submit(SubmitRequest {
                target: "/blog/hello/".into(),
                line: "alice: 好文".into(),
                ip: "127.0.0.1".into(),
            })
            .unwrap();
        assert!(r.ok);
        assert!(td.path().join(DATA_FILE).is_file());
        assert_eq!(
            s.page(
                PageRequest {
                    after_id: 0,
                    limit: None,
                    revision: None
                },
                true
            )
            .comments
            .len(),
            1
        );
        assert_eq!(s.moderate(&[1], true).unwrap().changed, 1);
        let q = s.public_query(PublicQuery {
            target: "/blog/hello/".into(),
            after_id: None,
            limit: None,
            revision: None,
        });
        assert_eq!(q.comments[0].author, "alice");
        assert_eq!(q.comments[0].text, "好文");
    }

    #[test]
    fn duplicate_and_limit_rejected_without_id_consumption() {
        let td = tempfile::tempdir().unwrap();
        let mut s = memory_store(td.path());
        let req = SubmitRequest {
            target: "/".into(),
            line: "hello".into(),
            ip: "127.0.0.1".into(),
        };
        assert!(s.submit(req.clone()).unwrap().ok);
        assert!(!s.submit(req).unwrap().ok);
        assert_eq!(s.comments.len(), 1);
        for n in 1..10 {
            assert!(
                s.submit(SubmitRequest {
                    target: "/".into(),
                    line: format!("line {n}"),
                    ip: "127.0.0.1".into()
                })
                .unwrap()
                .ok
            );
        }
        assert!(
            !s.submit(SubmitRequest {
                target: "/".into(),
                line: "eleven".into(),
                ip: "127.0.0.1".into()
            })
            .unwrap()
            .ok
        );
        assert_eq!(s.comments.last().unwrap().id, 10);
    }

    #[test]
    fn pagination_is_revision_bound() {
        let td = tempfile::tempdir().unwrap();
        let mut s = memory_store(td.path());
        for n in 0..3 {
            s.submit(SubmitRequest {
                target: "/".into(),
                line: format!("line {n}"),
                ip: format!("127.0.0.{}", n + 1),
            })
            .unwrap();
            s.moderate(&[(n + 1) as u64], true).unwrap();
        }
        let first = s.page(
            PageRequest {
                after_id: 0,
                limit: Some(2),
                revision: None,
            },
            false,
        );
        assert!(first.has_more);
        let stale = s.page(
            PageRequest {
                after_id: 2,
                limit: Some(2),
                revision: Some("old".into()),
            },
            false,
        );
        assert_eq!(stale.error.as_deref(), Some("stale_revision"));
    }
}
