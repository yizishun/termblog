use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::net::IpAddr;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{SecondsFormat, Utc};
use hmac::{Hmac, Mac};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Transaction};
use sha2::Sha256;
use termblog_content_model::{ArticlePath, ContentScope};

use crate::protocol::*;

const DATABASE_FILE: &str = "stats.sqlite3";
const SECRET_FILE: &str = "secret";
const SECRET_LEN: usize = 32;
const SCHEMA_VERSION: i64 = 1;

type HmacSha256 = Hmac<Sha256>;

pub struct Store {
    connection: Connection,
    secret: [u8; SECRET_LEN],
}

impl Store {
    pub fn init(dir: &Path) -> Result<()> {
        prepare_empty_root_dir(dir)?;

        let mut secret = [0u8; SECRET_LEN];
        File::open("/dev/urandom")
            .context("open /dev/urandom")?
            .read_exact(&mut secret)
            .context("read statd secret")?;
        create_synced(dir, SECRET_FILE, &secret)?;

        let database = dir.join(DATABASE_FILE);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&database)
            .context("create stats database")?;
        file.sync_all()?;
        drop(file);

        let connection = open_connection(&database)?;
        create_schema(&connection)?;
        connection.close().map_err(|(_, error)| error)?;
        File::open(&database)?.sync_all()?;
        File::open(dir)?.sync_all()?;
        Ok(())
    }

    pub fn open(dir: &Path) -> Result<Self> {
        validate_root_dir(dir)?;
        validate_root_file(&dir.join(SECRET_FILE)).context("validate statd secret")?;
        validate_root_file(&dir.join(DATABASE_FILE)).context("validate stats database")?;

        let secret_vec = std::fs::read(dir.join(SECRET_FILE))?;
        let secret: [u8; SECRET_LEN] = secret_vec
            .try_into()
            .map_err(|_| anyhow!("statd secret must be exactly {SECRET_LEN} bytes"))?;
        let connection = open_connection(&dir.join(DATABASE_FILE))?;
        validate_database(&connection)?;
        Ok(Self { connection, secret })
    }

    pub fn record_batch(&mut self, request: RecordBatchRequest) -> Result<usize> {
        if request.events.is_empty() || request.events.len() > MAX_BATCH_EVENTS {
            bail!("record batch must contain between 1 and {MAX_BATCH_EVENTS} events");
        }
        let events = request
            .events
            .into_iter()
            .map(validate_event)
            .collect::<Result<Vec<_>>>()?;

        let secret = self.secret;
        let transaction = self.connection.transaction()?;
        for event in &events {
            increment_target(&transaction, &event.target, event.source)?;
            increment_article(&transaction, &event.target, &event.article, event.source)?;
            let visitor = visitor_hash(&secret, &event.target, &event.ip);
            transaction.execute(
                "INSERT OR IGNORE INTO target_visitors(target, visitor_hash) VALUES (?1, ?2)",
                params![event.target, visitor.as_slice()],
            )?;
        }
        transaction.commit()?;
        Ok(events.len())
    }

    pub fn snapshot(&mut self, request: SnapshotRequest) -> Result<SnapshotResponse> {
        let targets = validate_snapshot_request(request)?;
        let transaction = self.connection.transaction()?;
        let mut snapshots = Vec::with_capacity(targets.len());
        for requested in targets {
            let totals: Option<(i64, i64)> = transaction
                .query_row(
                    "SELECT terminal_read_sessions, static_requests FROM target_totals WHERE target=?1",
                    [&requested.target],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let (terminal, static_requests) = totals.unwrap_or((0, 0));
            let visitors: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM target_visitors WHERE target=?1",
                [&requested.target],
                |row| row.get(0),
            )?;

            let mut article_statement = transaction.prepare(
                "SELECT source, total FROM article_totals WHERE target=?1 AND article=?2",
            )?;
            let mut articles = Vec::with_capacity(requested.articles.len());
            for article in requested.articles {
                let rows = article_statement
                    .query_map(params![&requested.target, &article], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                    })?;
                let mut terminal_read_sessions = 0;
                let mut article_static_requests = 0;
                for row in rows {
                    let (source, count) = row?;
                    match source.as_str() {
                        "terminal_read_session" => terminal_read_sessions = count,
                        "static_request" => article_static_requests = count,
                        _ => bail!("database contains an unknown article source"),
                    }
                }
                articles.push(ArticleSnapshot {
                    article,
                    terminal_read_sessions: as_u64(terminal_read_sessions)?,
                    static_requests: as_u64(article_static_requests)?,
                });
            }
            drop(article_statement);
            snapshots.push(TargetSnapshot {
                target: requested.target,
                terminal_read_sessions_total: as_u64(terminal)?,
                static_requests_total: as_u64(static_requests)?,
                unique_visitors_approx: as_u64(visitors)?,
                articles,
            });
        }
        transaction.commit()?;
        Ok(SnapshotResponse {
            ok: true,
            snapshot_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
            targets: snapshots,
            error: None,
        })
    }
}

#[derive(Debug)]
struct ValidEvent {
    source: Source,
    target: String,
    article: String,
    ip: String,
}

fn validate_event(event: RecordEvent) -> Result<ValidEvent> {
    if event.article.is_empty() || event.article.len() > MAX_ARTICLE_KEY_LEN {
        bail!("article key length is invalid");
    }
    let article = ArticlePath::parse(&format!("{}.md", event.article))?;
    if article.key != event.article {
        bail!("article key mapping is invalid");
    }
    let scope = ContentScope::from_target(&event.target)?;
    if !scope.contains_article(&article) {
        bail!(
            "article {} does not belong to target {}",
            event.article,
            event.target
        );
    }
    let ip = canonical_ip(&event.ip)?;
    Ok(ValidEvent {
        source: event.source,
        target: event.target,
        article: event.article,
        ip,
    })
}

fn validate_snapshot_request(request: SnapshotRequest) -> Result<Vec<SnapshotTargetRequest>> {
    if request.targets.len() > MAX_SNAPSHOT_TARGETS {
        bail!("snapshot has more than {MAX_SNAPSHOT_TARGETS} targets");
    }
    let mut seen_targets = BTreeSet::new();
    let mut article_count = 0usize;
    let mut targets = request.targets;
    for target in &mut targets {
        let scope = ContentScope::from_target(&target.target)?;
        if !seen_targets.insert(target.target.clone()) {
            bail!("duplicate snapshot target {}", target.target);
        }
        let mut seen_articles = BTreeSet::new();
        for key in &target.articles {
            article_count = article_count
                .checked_add(1)
                .ok_or_else(|| anyhow!("snapshot article count overflow"))?;
            if article_count > MAX_SNAPSHOT_ARTICLES {
                bail!("snapshot has more than {MAX_SNAPSHOT_ARTICLES} articles");
            }
            if key.is_empty() || key.len() > MAX_ARTICLE_KEY_LEN {
                bail!("snapshot article key length is invalid");
            }
            let article = ArticlePath::parse(&format!("{key}.md"))?;
            if article.key != *key || !scope.contains_article(&article) {
                bail!(
                    "snapshot article {key} does not belong to target {}",
                    target.target
                );
            }
            if !seen_articles.insert(key.clone()) {
                bail!("duplicate snapshot article {key}");
            }
        }
        target.articles.sort();
    }
    targets.sort_by(|a, b| a.target.cmp(&b.target));
    Ok(targets)
}

fn increment_target(transaction: &Transaction<'_>, target: &str, source: Source) -> Result<()> {
    let existing: Option<(i64, i64)> = transaction
        .query_row(
            "SELECT terminal_read_sessions, static_requests FROM target_totals WHERE target=?1",
            [target],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (terminal, static_requests) = existing.unwrap_or((0, 0));
    let (terminal, static_requests) = match source {
        Source::TerminalReadSession => (
            terminal
                .checked_add(1)
                .context("terminal target counter overflow")?,
            static_requests,
        ),
        Source::StaticRequest => (
            terminal,
            static_requests
                .checked_add(1)
                .context("static target counter overflow")?,
        ),
    };
    transaction.execute(
        "INSERT INTO target_totals(target, terminal_read_sessions, static_requests)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(target) DO UPDATE SET
           terminal_read_sessions=excluded.terminal_read_sessions,
           static_requests=excluded.static_requests",
        params![target, terminal, static_requests],
    )?;
    Ok(())
}

fn increment_article(
    transaction: &Transaction<'_>,
    target: &str,
    article: &str,
    source: Source,
) -> Result<()> {
    let source = match source {
        Source::TerminalReadSession => "terminal_read_session",
        Source::StaticRequest => "static_request",
    };
    let current: Option<i64> = transaction
        .query_row(
            "SELECT total FROM article_totals WHERE target=?1 AND article=?2 AND source=?3",
            params![target, article, source],
            |row| row.get(0),
        )
        .optional()?;
    let next = current
        .unwrap_or(0)
        .checked_add(1)
        .context("article counter overflow")?;
    transaction.execute(
        "INSERT INTO article_totals(target, article, source, total) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(target, article, source) DO UPDATE SET total=excluded.total",
        params![target, article, source, next],
    )?;
    Ok(())
}

fn canonical_ip(raw: &str) -> Result<String> {
    match raw
        .parse::<IpAddr>()
        .context("event IP is not a valid IP address")?
    {
        IpAddr::V4(ip) => Ok(ip.to_string()),
        IpAddr::V6(ip) => Ok(ip
            .to_ipv4_mapped()
            .map_or_else(|| ip.to_string(), |ip| ip.to_string())),
    }
}

fn visitor_hash(secret: &[u8; SECRET_LEN], target: &str, canonical_ip: &str) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(target.as_bytes());
    mac.update(&[0]);
    mac.update(canonical_ip.as_bytes());
    mac.finalize().into_bytes().into()
}

fn as_u64(value: i64) -> Result<u64> {
    value
        .try_into()
        .context("database contains a negative counter")
}

fn open_connection(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("open SQLite database {}", path.display()))?;
    connection.busy_timeout(std::time::Duration::from_millis(250))?;
    connection.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;")?;
    Ok(connection)
}

fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "BEGIN EXCLUSIVE;
         CREATE TABLE target_totals (
           target TEXT PRIMARY KEY NOT NULL,
           terminal_read_sessions INTEGER NOT NULL CHECK(terminal_read_sessions >= 0),
           static_requests INTEGER NOT NULL CHECK(static_requests >= 0)
         ) STRICT;
         CREATE TABLE article_totals (
           target TEXT NOT NULL,
           article TEXT NOT NULL,
           source TEXT NOT NULL CHECK(source IN ('terminal_read_session', 'static_request')),
           total INTEGER NOT NULL CHECK(total >= 0),
           PRIMARY KEY(target, article, source)
         ) STRICT;
         CREATE TABLE target_visitors (
           target TEXT NOT NULL,
           visitor_hash BLOB NOT NULL CHECK(length(visitor_hash) = 32),
           PRIMARY KEY(target, visitor_hash)
         ) STRICT;
         PRAGMA user_version=1;
         COMMIT;",
    )?;
    Ok(())
}

fn validate_database(connection: &Connection) -> Result<()> {
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != SCHEMA_VERSION {
        bail!("unsupported statd schema version: {version}");
    }
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        bail!("stats database integrity check failed: {integrity}");
    }
    for table in ["target_totals", "article_totals", "target_visitors"] {
        let exists: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            [table],
            |row| row.get(0),
        )?;
        if exists != 1 {
            bail!("stats database missing table {table}");
        }
    }
    for query in [
        "SELECT target, terminal_read_sessions, static_requests FROM target_totals LIMIT 0",
        "SELECT target, article, source, total FROM article_totals LIMIT 0",
        "SELECT target, visitor_hash FROM target_visitors LIMIT 0",
    ] {
        connection
            .prepare(query)
            .context("stats database schema does not match version 1")?;
    }
    Ok(())
}

fn prepare_empty_root_dir(dir: &Path) -> Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(metadata) => {
            if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                bail!("statd data directory must be a real directory");
            }
            if std::fs::read_dir(dir)?.next().is_some() {
                bail!(
                    "data directory {} is not empty, refusing to initialize",
                    dir.display()
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        Err(error) => return Err(error).with_context(|| format!("read {}", dir.display())),
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    validate_root_dir(dir)
}

fn validate_root_dir(dir: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(dir)
        .with_context(|| format!("read data directory {}", dir.display()))?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        bail!("statd data directory must be a real directory");
    }
    if metadata.uid() != 0 || metadata.gid() != 0 || metadata.mode() & 0o7777 != 0o700 {
        bail!("statd data directory must be root:wheel 0700");
    }
    Ok(())
}

fn validate_root_file(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != 0
        || metadata.gid() != 0
        || metadata.mode() & 0o7777 != 0o600
    {
        bail!("{} must be a root:wheel 0600 regular file", path.display());
    }
    Ok(())
}

fn create_synced(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let path = dir.join(name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_store() -> Store {
        let connection = Connection::open_in_memory().unwrap();
        create_schema(&connection).unwrap();
        Store {
            connection,
            secret: [7; SECRET_LEN],
        }
    }

    fn event(source: Source, target: &str, article: &str, ip: &str) -> RecordEvent {
        RecordEvent {
            source,
            target: target.into(),
            article: article.into(),
            ip: ip.into(),
        }
    }

    fn request(target: &str, articles: &[&str]) -> SnapshotRequest {
        SnapshotRequest {
            targets: vec![SnapshotTargetRequest {
                target: target.into(),
                articles: articles.iter().map(|value| (*value).into()).collect(),
            }],
        }
    }

    #[test]
    fn sources_accumulate_separately_and_visitors_are_a_union() {
        let mut store = memory_store();
        store
            .record_batch(RecordBatchRequest {
                events: vec![
                    event(
                        Source::TerminalReadSession,
                        "/tests/",
                        "tests/a",
                        "192.0.2.1",
                    ),
                    event(Source::StaticRequest, "/tests/", "tests/a", "192.0.2.1"),
                    event(Source::StaticRequest, "/tests/", "tests/b", "192.0.2.2"),
                ],
            })
            .unwrap();
        let snapshot = store
            .snapshot(request("/tests/", &["tests/b", "tests/a"]))
            .unwrap();
        let target = &snapshot.targets[0];
        assert_eq!(target.terminal_read_sessions_total, 1);
        assert_eq!(target.static_requests_total, 2);
        assert_eq!(target.unique_visitors_approx, 2);
        assert_eq!(target.articles[0].article, "tests/a");
        assert_eq!(target.articles[0].terminal_read_sessions, 1);
        assert_eq!(target.articles[0].static_requests, 1);
    }

    #[test]
    fn targets_are_isolated_and_mapped_strictly() {
        let mut store = memory_store();
        store
            .record_batch(RecordBatchRequest {
                events: vec![
                    event(Source::StaticRequest, "/", "help", "::ffff:192.0.2.1"),
                    event(Source::StaticRequest, "/", "help", "192.0.2.1"),
                    event(Source::StaticRequest, "/notes/", "notes/a", "192.0.2.1"),
                ],
            })
            .unwrap();
        assert_eq!(
            store.snapshot(request("/", &["help"])).unwrap().targets[0].unique_visitors_approx,
            1
        );
        assert_eq!(
            store
                .snapshot(request("/notes/", &["notes/a"]))
                .unwrap()
                .targets[0]
                .unique_visitors_approx,
            1
        );

        let before = store.snapshot(request("/", &["help"])).unwrap();
        assert!(store
            .record_batch(RecordBatchRequest {
                events: vec![
                    event(Source::StaticRequest, "/", "help", "192.0.2.2"),
                    event(Source::StaticRequest, "/", "notes/a", "192.0.2.2"),
                ],
            })
            .is_err());
        let after = store.snapshot(request("/", &["help"])).unwrap();
        assert_eq!(
            before.targets[0].static_requests_total,
            after.targets[0].static_requests_total
        );
    }

    #[test]
    fn empty_and_oversized_batches_are_rejected() {
        let mut store = memory_store();
        assert!(store
            .record_batch(RecordBatchRequest { events: Vec::new() })
            .is_err());
        let events = (0..=MAX_BATCH_EVENTS)
            .map(|_| event(Source::StaticRequest, "/", "help", "127.0.0.1"))
            .collect();
        assert!(store.record_batch(RecordBatchRequest { events }).is_err());
    }

    #[test]
    fn overflow_rolls_back_the_whole_batch() {
        let mut store = memory_store();
        store
            .connection
            .execute("INSERT INTO target_totals VALUES ('/', ?1, 0)", [i64::MAX])
            .unwrap();
        assert!(store
            .record_batch(RecordBatchRequest {
                events: vec![event(Source::TerminalReadSession, "/", "help", "127.0.0.1",)],
            })
            .is_err());
        let visitors: i64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM target_visitors", [], |row| row.get(0))
            .unwrap();
        assert_eq!(visitors, 0);
    }

    #[test]
    fn counters_survive_reopening_the_database() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("stats.sqlite3");
        {
            let connection = Connection::open(&database).unwrap();
            create_schema(&connection).unwrap();
            let mut store = Store {
                connection,
                secret: [9; SECRET_LEN],
            };
            store
                .record_batch(RecordBatchRequest {
                    events: vec![event(
                        Source::StaticRequest,
                        "/notes/",
                        "notes/a",
                        "2001:db8::1",
                    )],
                })
                .unwrap();
        }

        let connection = open_connection(&database).unwrap();
        validate_database(&connection).unwrap();
        let mut reopened = Store {
            connection,
            secret: [9; SECRET_LEN],
        };
        let snapshot = reopened.snapshot(request("/notes/", &["notes/a"])).unwrap();
        assert_eq!(snapshot.targets[0].static_requests_total, 1);
        assert_eq!(snapshot.targets[0].unique_visitors_approx, 1);
    }

    #[test]
    fn unknown_schema_and_corrupt_database_are_rejected() {
        let connection = Connection::open_in_memory().unwrap();
        create_schema(&connection).unwrap();
        connection.pragma_update(None, "user_version", 99).unwrap();
        assert!(validate_database(&connection).is_err());

        let wrong_columns = Connection::open_in_memory().unwrap();
        create_schema(&wrong_columns).unwrap();
        wrong_columns
            .execute_batch("ALTER TABLE target_totals RENAME COLUMN static_requests TO broken")
            .unwrap();
        assert!(validate_database(&wrong_columns).is_err());

        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("broken.sqlite3");
        std::fs::write(&database, b"not a sqlite database").unwrap();
        if let Ok(connection) = open_connection(&database) {
            assert!(validate_database(&connection).is_err());
        }
    }
}
