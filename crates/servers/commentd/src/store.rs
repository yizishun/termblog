use std::collections::{HashMap, HashSet};
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
    page_limit, valid_target, visible_comments, Comment, ModerateResponse, PageRequest,
    PageResponse, PublicQuery, PublicQueryResponse, SubmitRequest, SubmitResponse,
    MAX_COMMENT_BYTES,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reply_to_id: Option<u64>,
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
            reply_to_id: self.reply_to_id,
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
                    bail!("data directory must be a real directory");
                }
                if md.uid() != 0 || md.gid() != 0 {
                    bail!("data directory must be root:wheel");
                }
                if std::fs::read_dir(dir)?.next().is_some() {
                    bail!("data directory {} is not empty, refusing to initialize", dir.display());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(dir).with_context(|| format!("create {}", dir.display()))?;
            }
            Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
        }
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        validate_dir(dir)?;

        let mut salt = [0u8; SALT_LEN];
        File::open("/dev/urandom")
            .context("open /dev/urandom")?
            .read_exact(&mut salt)
            .context("read random salt")?;
        create_synced(dir, SALT_FILE, &salt)?;
        create_synced(dir, DATA_FILE, b"")?;
        // marker 最后写：缺 marker 的半初始化目录永远不会被当成空库启动。
        create_synced(dir, MARKER_FILE, MARKER)?;
        Ok(())
    }

    pub fn open(dir: &Path) -> Result<Self> {
        validate_dir(dir)?;
        for name in [SALT_FILE, DATA_FILE, MARKER_FILE] {
            validate_file(&dir.join(name)).with_context(|| format!("validate {name}"))?;
        }
        let marker = std::fs::read(dir.join(MARKER_FILE))?;
        if marker != MARKER {
            bail!("invalid initialized marker");
        }
        let salt_vec = std::fs::read(dir.join(SALT_FILE))?;
        let salt: [u8; SALT_LEN] = salt_vec
            .try_into()
            .map_err(|_| anyhow!("salt must be exactly {SALT_LEN} bytes"))?;
        let raw = std::fs::read(dir.join(DATA_FILE))?;
        let text = std::str::from_utf8(&raw).context("comments.jsonl is not UTF-8")?;
        let mut comments = Vec::new();
        let mut last_id = 0;
        for (idx, line) in text.lines().enumerate() {
            if line.is_empty() {
                bail!("comments.jsonl line {} is empty", idx + 1);
            }
            let c: StoredComment = serde_json::from_str(line)
                .with_context(|| format!("comments.jsonl line {} invalid JSON", idx + 1))?;
            validate_stored(&c).with_context(|| format!("comments.jsonl line {}", idx + 1))?;
            if c.id <= last_id {
                bail!("comments.jsonl line {} ID not strictly increasing", idx + 1);
            }
            last_id = c.id;
            comments.push(c);
        }
        validate_relations(&comments)?;
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
            bail!("storage has entered fail-stop state");
        }
        let reject = |msg: &str| SubmitResponse {
            ok: false,
            id: None,
            notice: format!("Comment not submitted: {msg}"),
            error: Some(msg.to_string()),
        };
        if !valid_target(&req.target) {
            return Ok(reject("invalid target"));
        }
        if req.line.len() > MAX_COMMENT_BYTES {
            return Ok(reject("comment exceeds 512 bytes"));
        }
        if req.ip.parse::<std::net::IpAddr>().is_err() {
            return Ok(reject("invalid IP"));
        }
        let parsed = match normalize_line(&req.line) {
            Ok(Some(parsed)) => parsed,
            Ok(None) => return Ok(reject("cleaned body is empty")),
            Err(message) => return Ok(reject(message)),
        };
        let ParsedLine {
            author,
            text,
            reply_number,
        } = parsed;
        if author.len() > 32 {
            return Ok(reject("author name exceeds 32 bytes"));
        }
        if text.len() > MAX_COMMENT_BYTES {
            return Ok(reject("body exceeds 512 bytes"));
        }
        let reply_to_id = match reply_number {
            Some(number) => match resolve_reply_id(&self.comments, &req.target, number) {
                Some(id) => Some(id),
                None => return Ok(reject(&format!("public comment #{number} does not exist in target"))),
            },
            None => None,
        };

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
            return Ok(reject("rate limit exceeded: max 10 comments per hour from same source"));
        }
        if self.comments.iter().any(|c| {
            c.ip_hash == ip_hash
                && c.target == req.target
                && c.text == text
                && c.reply_to_id == reply_to_id
                && recent(c, 300)
        }) {
            return Ok(reject("duplicate content within 5 minutes"));
        }

        let id = self
            .comments
            .last()
            .map_or(Some(1), |c| c.id.checked_add(1))
            .ok_or_else(|| anyhow!("comment IDs exhausted"))?;
        let mut candidate = self.comments.clone();
        candidate.push(StoredComment {
            id,
            target: req.target.clone(),
            author,
            text,
            reply_to_id,
            ip_hash,
            created_at: now.to_rfc3339_opts(SecondsFormat::Secs, true),
            status: Status::Pending,
        });
        self.commit(candidate)?;
        Ok(SubmitResponse {
            ok: true,
            id: Some(id),
            notice: "Comment submitted to moderation queue".into(),
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
            next_after_number: None,
            has_more: false,
            error: Some(message),
        };
        if !valid_target(&req.target) {
            return err("invalid target".into());
        }
        let limit = match page_limit(req.limit) {
            Ok(n) => n,
            Err(e) => return err(e.into()),
        };
        if req.after_number.unwrap_or(0) > 0 && req.revision.is_none() {
            return err("revision is required when after_number>0".into());
        }
        if let Some(r) = &req.revision {
            if r != &self.revision {
                return err("stale_revision".into());
            }
        }
        let private: Vec<_> = self
            .comments
            .iter()
            .filter(|c| c.status == Status::Approved && c.target == req.target)
            .map(StoredComment::view)
            .collect();
        let all = match visible_comments(&private) {
            Ok(comments) => comments,
            Err(_) => return err("comment data unavailable".into()),
        };
        let total = all.len();
        if req.after_number.is_none() {
            let start = total.saturating_sub(limit);
            return PublicQueryResponse {
                ok: true,
                revision: self.revision.clone(),
                total,
                omitted_earlier: start,
                comments: all[start..].to_vec(),
                next_after_number: None,
                has_more: false,
                error: None,
            };
        }
        let after = req.after_number.unwrap_or(0);
        let mut rest = all.into_iter().filter(|c| c.number > after);
        let comments: Vec<_> = rest.by_ref().take(limit).collect();
        let has_more = rest.next().is_some();
        let next_after_number = has_more
            .then(|| comments.last().map(|c| c.number))
            .flatten();
        PublicQueryResponse {
            ok: true,
            revision: self.revision.clone(),
            total,
            omitted_earlier: 0,
            comments,
            next_after_number,
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
            return err("revision is required when after_id>0".into());
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
            bail!("storage has entered fail-stop state");
        }
        if ids.is_empty() {
            return Ok(ModerateResponse {
                ok: false,
                changed: 0,
                error: Some("ids cannot be empty".into()),
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
                Err(e) => return Err(e).with_context(|| format!("create {}", temp.display())),
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
            return Err(anyhow!("directory fsync failed, entering fail-stop: {e}"));
        }
        self.comments = candidate;
        self.revision = new_revision;
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedLine {
    author: String,
    text: String,
    reply_number: Option<u64>,
}

fn normalize_line(line: &str) -> Result<Option<ParsedLine>, &'static str> {
    let clean = normalize_comment_controls(line);
    let clean = clean.trim();
    if clean.is_empty() {
        return Ok(None);
    }

    // A leading #N: applies to the whole (possibly multiline) guest body and
    // must be parsed before the optional author prefix.
    if let Some((reply_number, text)) = reply_prefix(clean)? {
        return Ok(Some(ParsedLine {
            author: "guest".into(),
            text: text.into(),
            reply_number: Some(reply_number),
        }));
    }

    // Only the first physical line may introduce an author. A colon in a later
    // body line must not retroactively turn the preceding text into a name.
    let first_line = clean.split_once('\n').map_or(clean, |(first, _)| first);
    if let Some(colon) = first_line.find(':') {
        let author = clean[..colon].trim();
        let text = clean[colon + 1..].trim();
        if !author.is_empty() && !text.is_empty() {
            let reply = reply_prefix(text)?;
            return Ok(Some(ParsedLine {
                author: author.to_string(),
                text: reply.map_or(text, |(_, body)| body).to_string(),
                reply_number: reply.map(|(number, _)| number),
            }));
        }
    }
    Ok(Some(ParsedLine {
        author: "guest".into(),
        text: clean.to_string(),
        reply_number: None,
    }))
}

fn reply_prefix(text: &str) -> Result<Option<(u64, &str)>, &'static str> {
    let Some(rest) = text.strip_prefix('#') else {
        return Ok(None);
    };
    let digit_len = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digit_len == 0 || rest.as_bytes().get(digit_len) != Some(&b':') {
        return Ok(None);
    }
    let number = rest[..digit_len]
        .parse::<u64>()
        .map_err(|_| "reply number too large")?;
    if number == 0 {
        return Err("reply number must be greater than 0");
    }
    let body = rest[digit_len + 1..].trim();
    if body.is_empty() {
        return Err("reply body is empty");
    }
    Ok(Some((number, body)))
}

fn resolve_reply_id(comments: &[StoredComment], target: &str, wanted: u64) -> Option<u64> {
    let mut number = 0u64;
    for comment in comments
        .iter()
        .filter(|c| c.status == Status::Approved && c.target == target)
    {
        number = number.checked_add(1)?;
        if number == wanted {
            return Some(comment.id);
        }
    }
    None
}

fn validate_relations(comments: &[StoredComment]) -> Result<()> {
    let mut seen: HashMap<u64, (&str, Status)> = HashMap::new();
    for (index, comment) in comments.iter().enumerate() {
        let line = index + 1;
        if let Some(parent_id) = comment.reply_to_id {
            let Some((parent_target, parent_status)) = seen.get(&parent_id) else {
                bail!(
                    "comments.jsonl line {line}: comment ID {} reply_to_id {} does not exist or is not earlier",
                    comment.id,
                    parent_id
                );
            };
            if *parent_target != comment.target {
                bail!(
                    "comments.jsonl line {line}: comment ID {} reply crosses target boundary",
                    comment.id
                );
            }
            if *parent_status != Status::Approved {
                bail!(
                    "comments.jsonl line {line}: parent comment of comment ID {} is not approved",
                    comment.id
                );
            }
        }
        seen.insert(comment.id, (&comment.target, comment.status));
    }
    Ok(())
}

fn validate_stored(c: &StoredComment) -> Result<()> {
    if c.id == 0 {
        bail!("ID must be greater than 0");
    }
    if !valid_target(&c.target) {
        bail!("invalid target");
    }
    if c.author.is_empty()
        || c.author.len() > 32
        || c.text.is_empty()
        || c.text.len() > MAX_COMMENT_BYTES
    {
        bail!("invalid author/text length");
    }
    if normalize_controls(&c.author) != c.author
        || normalize_comment_controls(&c.text) != c.text
    {
        bail!("author/text contains unsupported control characters");
    }
    if c.ip_hash.len() != 64 || !c.ip_hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid ip_hash");
    }
    DateTime::parse_from_rfc3339(&c.created_at).context("created_at not RFC3339")?;
    Ok(())
}

fn normalize_controls(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(*c as u32, 0x00..=0x1f | 0x7f..=0x9f))
        .collect()
}

fn normalize_comment_controls(s: &str) -> String {
    s.chars()
        .filter(|c| *c == '\n' || !matches!(*c as u32, 0x00..=0x1f | 0x7f..=0x9f))
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
        .with_context(|| format!("read data directory {}", dir.display()))?;
    if !md.file_type().is_dir() || md.file_type().is_symlink() {
        bail!("data directory must be a real directory");
    }
    if md.uid() != 0 || md.gid() != 0 {
        bail!("data directory owner must be root");
    }
    if md.mode() & 0o022 != 0 {
        bail!("data directory group/other must not be writable");
    }
    Ok(())
}

fn validate_file(path: &Path) -> Result<()> {
    let md = std::fs::symlink_metadata(path)?;
    if !md.file_type().is_file() || md.file_type().is_symlink() {
        bail!("must be a regular file and not a symlink");
    }
    if md.uid() != 0 || md.gid() != 0 || md.mode() & 0o777 != 0o600 {
        bail!("owner/mode must be root:wheel 0600");
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

    fn submit(s: &mut Store, target: &str, line: &str, ip: &str) -> SubmitResponse {
        s.submit(SubmitRequest {
            target: target.into(),
            line: line.into(),
            ip: ip.into(),
        })
        .unwrap()
    }

    #[test]
    fn prefix_and_controls() {
        assert_eq!(
            normalize_line(" alice: 好\u{1b}文 "),
            Ok(Some(ParsedLine {
                author: "alice".into(),
                text: "好文".into(),
                reply_number: None,
            }))
        );
        assert_eq!(
            normalize_line("无前缀"),
            Ok(Some(ParsedLine {
                author: "guest".into(),
                text: "无前缀".into(),
                reply_number: None,
            }))
        );
        assert_eq!(
            normalize_line("alice: #12: reply"),
            Ok(Some(ParsedLine {
                author: "alice".into(),
                text: "reply".into(),
                reply_number: Some(12),
            }))
        );
        assert_eq!(
            normalize_line("#2: guest reply"),
            Ok(Some(ParsedLine {
                author: "guest".into(),
                text: "guest reply".into(),
                reply_number: Some(2),
            }))
        );
        assert_eq!(normalize_line("\u{7f}\n"), Ok(None));
        assert_eq!(normalize_line("#0: nope"), Err("reply number must be greater than 0"));
        assert_eq!(normalize_line("#1:"), Err("reply body is empty"));
        assert_eq!(
            normalize_line("#rust: ordinary"),
            Ok(Some(ParsedLine {
                author: "#rust".into(),
                text: "ordinary".into(),
                reply_number: None,
            }))
        );
        assert_eq!(
            normalize_line("first line\nsecond: still body"),
            Ok(Some(ParsedLine {
                author: "guest".into(),
                text: "first line\nsecond: still body".into(),
                reply_number: None,
            }))
        );
        assert_eq!(
            normalize_line("bob: #2: first line\nsecond line"),
            Ok(Some(ParsedLine {
                author: "bob".into(),
                text: "first line\nsecond line".into(),
                reply_number: Some(2),
            }))
        );
    }

    #[test]
    fn submit_commit_query_and_moderate() {
        let td = tempfile::tempdir().unwrap();
        let mut s = memory_store(td.path());
        let r = s
            .submit(SubmitRequest {
                target: "/blog/hello/".into(),
                line: "alice: 好文\nsecond: still body".into(),
                ip: "127.0.0.1".into(),
            })
            .unwrap();
        assert!(r.ok);
        assert_eq!(r.notice, "Comment submitted to moderation queue");
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
            after_number: None,
            limit: None,
            revision: None,
        });
        assert_eq!(q.comments[0].number, 1);
        assert_eq!(q.comments[0].author, "alice");
        assert_eq!(q.comments[0].text, "好文\nsecond: still body");
    }

    #[test]
    fn named_guest_and_nested_replies_resolve_to_stable_ids() {
        let td = tempfile::tempdir().unwrap();
        let mut s = memory_store(td.path());

        assert!(submit(&mut s, "/", "alice: root", "127.0.0.1").ok);
        s.moderate(&[1], true).unwrap();
        assert!(submit(&mut s, "/other/", "other", "127.0.0.2").ok);
        s.moderate(&[2], true).unwrap();
        assert!(submit(&mut s, "/", "bob: #1: reply", "127.0.0.3").ok);
        assert_eq!(s.comments[2].reply_to_id, Some(1));
        assert_eq!(s.comments[2].text, "reply");
        s.moderate(&[3], true).unwrap();
        assert!(submit(&mut s, "/", "#2: nested", "127.0.0.4").ok);
        assert_eq!(s.comments[3].reply_to_id, Some(3));
        assert_eq!(s.comments[3].author, "guest");
        s.moderate(&[4], true).unwrap();

        let q = s.public_query(PublicQuery {
            target: "/".into(),
            after_number: None,
            limit: None,
            revision: None,
        });
        assert_eq!(
            q.comments.iter().map(|c| c.number).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(q.comments[1].reply_to.as_ref().unwrap().number, 1);
        assert_eq!(q.comments[1].reply_to.as_ref().unwrap().author, "alice");
        assert_eq!(q.comments[2].reply_to.as_ref().unwrap().number, 2);
        assert_eq!(q.comments[2].reply_to.as_ref().unwrap().author, "bob");
    }

    #[test]
    fn invalid_or_unapproved_reply_is_rejected_without_consuming_id() {
        let td = tempfile::tempdir().unwrap();
        let mut s = memory_store(td.path());
        assert!(submit(&mut s, "/", "pending", "127.0.0.1").ok);

        for (target, line) in [
            ("/", "bob: #1: pending parent"),
            ("/other/", "#1: other target"),
            ("/", "#99: missing"),
            ("/", "#0: invalid"),
            ("/", "#18446744073709551616: overflow"),
            ("/", "#1:"),
        ] {
            let response = submit(&mut s, target, line, "127.0.0.2");
            assert!(!response.ok, "应拒绝 {line:?}");
        }
        assert_eq!(s.comments.len(), 1);

        s.moderate(&[1], true).unwrap();
        let response = submit(&mut s, "/", "#1: accepted", "127.0.0.2");
        assert!(response.ok);
        assert_eq!(response.id, Some(2));
    }

    #[test]
    fn duplicate_text_to_different_parents_is_allowed() {
        let td = tempfile::tempdir().unwrap();
        let mut s = memory_store(td.path());
        assert!(submit(&mut s, "/", "first", "127.0.0.1").ok);
        assert!(submit(&mut s, "/", "second", "127.0.0.2").ok);
        s.moderate(&[1, 2], true).unwrap();

        assert!(submit(&mut s, "/", "#1: same", "127.0.0.3").ok);
        assert!(submit(&mut s, "/", "#2: same", "127.0.0.3").ok);
        assert_eq!(s.comments[2].reply_to_id, Some(1));
        assert_eq!(s.comments[3].reply_to_id, Some(2));
    }

    #[test]
    fn old_records_default_to_root_and_bad_relations_are_rejected() {
        let old: StoredComment = serde_json::from_str(
            r#"{"id":1,"target":"/","author":"a","text":"old","ip_hash":"0000000000000000000000000000000000000000000000000000000000000000","created_at":"2026-09-05T00:00:00Z","status":"approved"}"#,
        )
        .unwrap();
        assert_eq!(old.reply_to_id, None);

        let mut child = old.clone();
        child.id = 2;
        child.reply_to_id = Some(1);
        let mut pending_parent = old.clone();
        pending_parent.status = Status::Pending;
        assert!(validate_relations(&[pending_parent, child.clone()]).is_err());

        child.target = "/other/".into();
        assert!(validate_relations(&[old, child]).is_err());
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
    fn public_pagination_uses_local_numbers_and_relation_survives_renumbering() {
        let td = tempfile::tempdir().unwrap();
        let mut s = memory_store(td.path());

        // ID 1 暂时 pending；ID 2 此时是 target / 中访得到的 #1。
        assert!(submit(&mut s, "/", "older pending", "127.0.0.1").ok);
        assert!(submit(&mut s, "/", "visible root", "127.0.0.2").ok);
        s.moderate(&[2], true).unwrap();
        assert!(submit(&mut s, "/other/", "other target", "127.0.0.3").ok);
        s.moderate(&[3], true).unwrap();
        assert!(submit(&mut s, "/", "second root", "127.0.0.4").ok);
        s.moderate(&[4], true).unwrap();
        assert!(submit(&mut s, "/", "#1: stable reply", "127.0.0.5").ok);
        assert_eq!(s.comments[4].reply_to_id, Some(2));
        s.moderate(&[5], true).unwrap();

        let first = s.public_query(PublicQuery {
            target: "/".into(),
            after_number: Some(0),
            limit: Some(2),
            revision: None,
        });
        assert!(first.ok);
        assert!(first.has_more);
        assert_eq!(first.next_after_number, Some(2));
        assert_eq!(
            first.comments.iter().map(|c| c.number).collect::<Vec<_>>(),
            vec![1, 2]
        );
        let second = s.public_query(PublicQuery {
            target: "/".into(),
            after_number: first.next_after_number,
            limit: Some(2),
            revision: Some(first.revision),
        });
        assert!(second.ok);
        assert!(!second.has_more);
        assert_eq!(
            second.comments.iter().map(|c| c.number).collect::<Vec<_>>(),
            vec![3]
        );
        assert_eq!(second.comments[0].reply_to.as_ref().unwrap().number, 1);

        // 更早的 pending 获批后所有局部编号顺延，但稳定 ID 关系仍指向原父项。
        s.moderate(&[1], true).unwrap();
        let renumbered = s.public_query(PublicQuery {
            target: "/".into(),
            after_number: None,
            limit: None,
            revision: None,
        });
        assert_eq!(
            renumbered
                .comments
                .iter()
                .map(|c| c.number)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        assert_eq!(renumbered.comments[3].reply_to.as_ref().unwrap().number, 2);
        assert_eq!(
            renumbered.comments[3].reply_to.as_ref().unwrap().author,
            "guest"
        );
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
