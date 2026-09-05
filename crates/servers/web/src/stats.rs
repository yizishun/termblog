use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use termblog_config::StatsConfig;
use termblog_content_model::{parse_scope_manifest, ArticleIndex, ArticlePath};
use termblog_statd::{Client, RecordEvent, Source, MAX_BATCH_EVENTS};
use tokio::sync::mpsc;

const QUEUE_CAPACITY: usize = 2048;
const BATCH_DELAY: Duration = Duration::from_millis(25);
const WARNING_INTERVAL_SECS: u64 = 60;

#[derive(Clone)]
pub struct Recorder {
    routes: Arc<RwLock<HashMap<String, RouteBinding>>>,
    sender: mpsc::Sender<RecordEvent>,
    queue_warning: Arc<RateLimit>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RouteBinding {
    target: String,
    article: String,
}

impl Recorder {
    pub fn start(config: &StatsConfig, targets_file: &Path) -> Self {
        let article_index = targets_file.with_file_name("article-index.json");
        let routes = match load_routes(&article_index, targets_file) {
            Ok(routes) => routes,
            Err(error) => {
                tracing::warn!(
                    %error,
                    article_index = %article_index.display(),
                    targets = %targets_file.display(),
                    "static article statistics mapping unavailable"
                );
                HashMap::new()
            }
        };
        let routes = Arc::new(RwLock::new(routes));
        tokio::spawn(route_reloader(
            routes.clone(),
            article_index,
            targets_file.to_path_buf(),
        ));
        let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
        let client = Client::new(
            config.socket.clone(),
            Duration::from_millis(config.request_timeout_ms),
        );
        tokio::spawn(batch_worker(client, receiver));
        Self {
            routes,
            sender,
            queue_warning: Arc::new(RateLimit::default()),
        }
    }

    fn record(&self, method: &Method, path: &str, status: StatusCode, ip: IpAddr) {
        let binding = self
            .routes
            .read()
            .ok()
            .and_then(|routes| qualifying_binding(&routes, method, path, status).cloned());
        let Some(binding) = binding else {
            return;
        };
        let event = RecordEvent {
            source: Source::StaticRequest,
            target: binding.target.clone(),
            article: binding.article.clone(),
            ip: ip.to_string(),
        };
        if self.sender.try_send(event).is_err() && self.queue_warning.should_log() {
            tracing::warn!("static statistics queue full or closed; dropping events");
        }
    }
}

pub async fn track_request(
    State(recorder): State<Recorder>,
    request: Request,
    next: Next,
) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|connect| connect.0.ip());
    let response = next.run(request).await;
    if let Some(peer) = peer {
        recorder.record(&method, &path, response.status(), peer);
    }
    response
}

fn qualifying_binding<'a>(
    routes: &'a HashMap<String, RouteBinding>,
    method: &Method,
    path: &str,
    status: StatusCode,
) -> Option<&'a RouteBinding> {
    if method != Method::GET || !matches!(status, StatusCode::OK | StatusCode::NOT_MODIFIED) {
        return None;
    }
    routes.get(path)
}

fn load_routes(article_index: &Path, targets_file: &Path) -> Result<HashMap<String, RouteBinding>> {
    reject_symlink(article_index)?;
    reject_symlink(targets_file)?;
    let index: ArticleIndex = serde_json::from_slice(&std::fs::read(article_index)?)
        .with_context(|| format!("parse {}", article_index.display()))?;
    index.validate().map_err(anyhow::Error::msg)?;
    let scopes = parse_scope_manifest(&std::fs::read_to_string(targets_file)?)
        .map_err(anyhow::Error::msg)?;
    let by_directory: HashMap<_, _> = scopes
        .iter()
        .map(|scope| (scope.directory_rel.as_str(), scope))
        .collect();
    let mut routes = HashMap::new();
    for entry in index.articles {
        let article = ArticlePath::parse(&entry.source_rel).map_err(anyhow::Error::msg)?;
        let Some(scope) = by_directory.get(article.directory_rel.as_str()) else {
            continue;
        };
        if routes
            .insert(
                entry.route,
                RouteBinding {
                    target: scope.target.clone(),
                    article: article.key,
                },
            )
            .is_some()
        {
            anyhow::bail!("duplicate static article route");
        }
    }
    Ok(routes)
}

fn reject_symlink(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("read metadata for {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!(
            "{} must be a regular file and not a symlink",
            path.display()
        );
    }
    Ok(())
}

async fn route_reloader(
    routes: Arc<RwLock<HashMap<String, RouteBinding>>>,
    article_index: PathBuf,
    targets_file: PathBuf,
) {
    let warning = RateLimit::default();
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.tick().await;
    loop {
        interval.tick().await;
        match load_routes(&article_index, &targets_file) {
            Ok(updated) => match routes.write() {
                Ok(mut current) => *current = updated,
                Err(_) => return,
            },
            Err(error) if warning.should_log() => {
                tracing::warn!(%error, "failed to refresh static statistics mapping; retaining previous index");
            }
            Err(_) => {}
        }
    }
}

async fn batch_worker(client: Client, mut receiver: mpsc::Receiver<RecordEvent>) {
    let warning = RateLimit::default();
    while let Some(first) = receiver.recv().await {
        let mut events = Vec::with_capacity(MAX_BATCH_EVENTS);
        events.push(first);
        tokio::time::sleep(BATCH_DELAY).await;
        while events.len() < MAX_BATCH_EVENTS {
            match receiver.try_recv() {
                Ok(event) => events.push(event),
                Err(_) => break,
            }
        }
        if let Err(error) = client.record_batch(events).await {
            if warning.should_log() {
                tracing::warn!(%error, "statd unavailable; dropping static statistics batch");
            }
        }
    }
}

#[derive(Default)]
struct RateLimit {
    last: AtomicU64,
}

impl RateLimit {
    fn should_log(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(1);
        let previous = self.last.load(Ordering::Relaxed);
        if previous != 0 && now.saturating_sub(previous) < WARNING_INTERVAL_SECS {
            return false;
        }
        self.last
            .compare_exchange(previous, now.max(1), Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termblog_content_model::{ArticleIndexEntry, ContentScope};

    fn routes() -> HashMap<String, RouteBinding> {
        HashMap::from([(
            "/tests/hello/".into(),
            RouteBinding {
                target: "/tests/".into(),
                article: "tests/hello".into(),
            },
        )])
    }

    #[test]
    fn only_canonical_successful_gets_qualify() {
        let routes = routes();
        assert!(
            qualifying_binding(&routes, &Method::GET, "/tests/hello/", StatusCode::OK).is_some()
        );
        assert!(qualifying_binding(
            &routes,
            &Method::GET,
            "/tests/hello/",
            StatusCode::NOT_MODIFIED
        )
        .is_some());
        for (method, path, status) in [
            (Method::HEAD, "/tests/hello/", StatusCode::OK),
            (Method::GET, "/tests/hello", StatusCode::OK),
            (Method::GET, "/tests/missing/", StatusCode::NOT_FOUND),
            (Method::GET, "/", StatusCode::OK),
            (Method::GET, "/blog/", StatusCode::OK),
            (Method::GET, "/api/comments", StatusCode::OK),
            (Method::GET, "/pixel.png", StatusCode::OK),
        ] {
            assert!(qualifying_binding(&routes, &method, path, status).is_none());
        }
    }

    #[tokio::test]
    async fn a_full_queue_drops_without_delaying_the_response_path() {
        let (sender, mut receiver) = mpsc::channel(1);
        let recorder = Recorder {
            routes: Arc::new(RwLock::new(routes())),
            sender,
            queue_warning: Arc::new(RateLimit::default()),
        };
        let ip = "192.0.2.1".parse().unwrap();
        recorder.record(&Method::GET, "/tests/hello/", StatusCode::OK, ip);
        recorder.record(&Method::GET, "/tests/hello/", StatusCode::OK, ip);

        let event = receiver.try_recv().unwrap();
        assert_eq!(event.article, "tests/hello");
        assert!(matches!(
            receiver.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn manifest_mapping_includes_only_configured_directories() {
        let directory = tempfile::tempdir().unwrap();
        let index_path = directory.path().join("article-index.json");
        let targets_path = directory.path().join("comment-targets.tsv");
        let entries = [
            ("help.md", "help", "/help/"),
            ("tests/hello.md", "tests/hello", "/tests/hello/"),
            ("other/a.md", "other/a", "/other/a/"),
        ]
        .into_iter()
        .map(|(source_rel, key, route)| ArticleIndexEntry {
            date10: "2026-09-05".into(),
            source_rel: source_rel.into(),
            key: key.into(),
            route: route.into(),
            title: key.into(),
        })
        .collect();
        std::fs::write(
            &index_path,
            serde_json::to_vec(&ArticleIndex {
                version: 1,
                articles: entries,
            })
            .unwrap(),
        )
        .unwrap();
        let scopes = [
            ContentScope::from_directory_rel("").unwrap(),
            ContentScope::from_directory_rel("tests").unwrap(),
        ];
        std::fs::write(
            &targets_path,
            scopes
                .iter()
                .map(|scope| format!("{}\t{}\n", scope.comment_rel, scope.target))
                .collect::<String>(),
        )
        .unwrap();
        let routes = load_routes(&index_path, &targets_path).unwrap();
        assert_eq!(routes.len(), 2);
        assert_eq!(routes["/tests/hello/"].target, "/tests/");
        assert!(!routes.contains_key("/other/a/"));
    }
}
