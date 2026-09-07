//! termblog-web [降权]: axum WS 端点 + 前端静态资源。
//!
//! WS binary message 与 proto 帧一一对应, 网关只做透传, 零协议转换。
//! 会话一律经 Unix socket 的 SessionClient 连 jaild(唯一特权进程), 本进程
//! 无本地会话。
//!
//! 本文件只做「一条 WS 连接的一生」:
//!   握手(首帧必须是 Open) → 拿到会话(SessionStore: attach 优先, 否则 create)
//!   → 回 Opened → 下行泵(会话输出 → WS) + 上行循环(WS → 键入/Resize)
//!   → 断开交还(SessionStore::detach)。
//! 「浏览器刷新不丢会话」的持有/宽限/scrollback 回放逻辑在 sessions.rs。

mod sessions;
mod stats;

use std::net::{IpAddr, SocketAddr};
use std::path::Path;

use anyhow::Context;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{ConnectInfo, OriginalUri, Query, State, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Json;
use axum::Router;
use futures::{SinkExt, StreamExt};
use rustls_acme::caches::DirCache;
use rustls_acme::{AcmeConfig, UseChallenge};
use termblog_commentd::{page_limit, valid_target, Client as CommentClient, PublicQuery};
use termblog_config::Config;
use termblog_core::{Control, SessionClient};
use termblog_proto as proto;
use tower_http::services::ServeDir;

use sessions::{CloseReason, OutItem, SessionStore};

#[derive(Clone)]
struct AppState {
    sessions: SessionStore,
    comments: CommentClient,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CommentsParams {
    target: Option<String>,
    after_number: Option<u64>,
    limit: Option<u16>,
    revision: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 网关日志(排障用)
    tracing_subscriber::fmt().init();

    // 配置: TERMBLOG_CONFIG 指定文件, 否则 /usr/local/etc/termblog.toml(缺省用默认值)
    let cfg_path = std::env::var("TERMBLOG_CONFIG").ok();
    let mut cfg = Config::load(cfg_path.as_deref().map(Path::new))?;
    // 环境变量覆盖(方便多实例调试)
    if let Ok(v) = std::env::var("TERMBLOG_LISTEN") {
        cfg.web.listen = v;
    }
    if let Ok(v) = std::env::var("TERMBLOG_TLS_LISTEN") {
        cfg.web.tls.listen = v;
    }
    if let Ok(v) = std::env::var("TERMBLOG_SOCKET") {
        cfg.jail.socket = v.into();
    }

    let state = AppState {
        sessions: SessionStore::new(SessionClient::new(&cfg.jail.socket)),
        comments: CommentClient::new(cfg.comments.public_socket.clone()),
    };
    let stats = stats::Recorder::start(&cfg.stats, &cfg.comments.targets_file);

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/api/comments", get(comments_handler))
        .fallback_service(ServeDir::new(&cfg.web.static_dir)) // 前端构建产物; 开发可用 vite dev 代理
        .layer(middleware::from_fn_with_state(stats, stats::track_request))
        .with_state(state);

    if cfg.web.tls.enabled {
        serve_https(&cfg, app).await?;
    } else {
        serve_http(&cfg, app).await?;
    }
    Ok(())
}

async fn serve_http(cfg: &Config, app: Router) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(&cfg.web.listen)
        .await
        .with_context(|| format!("bind HTTP listener {}", cfg.web.listen))?;
    println!(
        "termblog-web listening on http://{} (jaild socket {})",
        cfg.web.listen,
        cfg.jail.socket.display()
    );
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .context("serve HTTP")?;
    Ok(())
}

async fn serve_https(cfg: &Config, app: Router) -> anyhow::Result<()> {
    let tls = &cfg.web.tls;
    let https_addr: SocketAddr = tls
        .listen
        .parse()
        .with_context(|| format!("parse HTTPS listen address {}", tls.listen))?;
    let site_url = cfg
        .web
        .site_url
        .clone()
        .context("TLS requires web.site_url")?;

    let mut acme_state = AcmeConfig::new(tls.domains.clone())
        .contact(tls.contacts.clone())
        .cache(DirCache::new(tls.cache_dir.clone()))
        .directory_lets_encrypt(tls.production)
        .challenge_type(UseChallenge::Http01)
        .state();
    let tls_acceptor = acme_state.axum_acceptor(acme_state.default_rustls_config());
    let http01 = acme_state.http01_challenge_tower_service();

    // AcmeState must be polled continuously for initial issuance and renewal.
    tokio::spawn(async move {
        while let Some(event) = acme_state.next().await {
            match event {
                Ok(event) => tracing::info!(?event, "ACME event"),
                Err(error) => tracing::error!(?error, "ACME error"),
            }
        }
        tracing::error!("ACME state stream ended");
    });

    // HTTP-01 is served before the fallback. Every other port-80 request gets
    // a permanent redirect preserving its path and query string.
    let redirect_app = Router::new()
        .route_service("/.well-known/acme-challenge/{challenge_token}", http01)
        .fallback(redirect_to_https)
        .with_state(site_url.clone());
    let http_listener = tokio::net::TcpListener::bind(&cfg.web.listen)
        .await
        .with_context(|| format!("bind HTTP redirect listener {}", cfg.web.listen))?;

    println!(
        "termblog-web listening on {} at {} (HTTP redirect/ACME on {})",
        site_url, tls.listen, cfg.web.listen
    );
    let https_server = axum_server::bind(https_addr)
        .acceptor(tls_acceptor)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>());
    let http_server = axum::serve(http_listener, redirect_app);
    tokio::try_join!(async { https_server.await.context("serve HTTPS") }, async {
        http_server.await.context("serve HTTP redirect/ACME")
    },)?;
    Ok(())
}

async fn redirect_to_https(
    State(site_url): State<String>,
    OriginalUri(uri): OriginalUri,
) -> Redirect {
    Redirect::permanent(&https_location(&site_url, &uri))
}

fn https_location(site_url: &str, uri: &axum::http::Uri) -> String {
    format!(
        "{}{}",
        site_url.trim_end_matches('/'),
        uri.path_and_query().map_or("/", |value| value.as_str())
    )
}

async fn comments_handler(
    State(state): State<AppState>,
    Query(params): Query<CommentsParams>,
) -> impl IntoResponse {
    let Some(target) = params.target else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "target is required"})),
        );
    };
    if !valid_target(&target)
        || page_limit(params.limit).is_err()
        || params.after_number.unwrap_or(0) > 0 && params.revision.is_none()
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid query parameters"})),
        );
    }
    match state
        .comments
        .public_query(&PublicQuery {
            target,
            after_number: params.after_number,
            limit: params.limit,
            revision: params.revision,
        })
        .await
    {
        Ok(res) if res.ok => (
            StatusCode::OK,
            Json(serde_json::json!({
                "revision": res.revision,
                "total": res.total,
                "omitted_earlier": res.omitted_earlier,
                "comments": res.comments,
                "next_after_number": res.next_after_number,
                "has_more": res.has_more,
            })),
        ),
        Ok(res) if res.error.as_deref() == Some("stale_revision") => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "stale_revision"})),
        ),
        Ok(res) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": res.error.unwrap_or_else(|| "query failed".into())})),
        ),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "comments service temporarily unavailable"})),
        ),
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    ws.on_upgrade(move |sock| handle(sock, state.sessions, peer.ip()))
}

async fn handle(sock: WebSocket, store: SessionStore, peer: IpAddr) {
    let (mut ws_tx, mut ws_rx) = sock.split();

    // 1) 握手: 第一条消息必须是 Open { cols, rows, attach_token? }
    let open = match ws_rx.next().await {
        Some(Ok(Message::Binary(b))) => match proto::decode_one(&b).and_then(|f| f.parse().ok()) {
            Some(o) => o,
            None => return,
        },
        _ => return,
    };
    let open: proto::Open = open;

    // 2) 会话获取。fresh(镜像页)一律开新会话: 每次落地都是干净 shell,
    //    浏览器无需判断旧 shell 状态(在文章里/别的分页器/提示符)——状态判断
    //    与恢复按键整块删除。开新会话前回收旧 token 的闲置会话, 配额即时释放。
    //    非 fresh 走 attach 优先; token 缺失/失效(会话已回收)则开新会话,
    //    由 Opened.attached 告知前端是恢复还是新开(前端据此重置屏幕)。
    let mut conn = None;
    if open.fresh {
        if let Some(token) = open.attach_token.as_deref() {
            store.reset(token);
        }
    } else if let Some(token) = open.attach_token.as_deref() {
        conn = store.attach(token).await;
    }
    let conn = match conn {
        Some(c) => c,
        None => match store
            .create(
                peer,
                open.cols,
                open.rows,
                open.attach_token.clone(),
                open.caps,
            )
            .await
        {
            Ok(c) => c,
            Err(e) => {
                let f = proto::Frame::json(
                    proto::CLOSED,
                    &proto::Closed {
                        reason: e.to_string(),
                    },
                );
                let _ = ws_tx.send(Message::Binary(proto::encode(&f))).await;
                return;
            }
        },
    };

    // 3) 同步窗口尺寸: attach 回来的连接尺寸可能变了; 顺带触发 SIGWINCH
    //    让 vim/less 等前台程序在回放内容之上重绘到最新画面
    let _ = conn
        .control
        .send(Control::Resize {
            cols: open.cols,
            rows: open.rows,
        })
        .await;

    let f = proto::Frame::json(
        proto::OPENED,
        &proto::Opened {
            session_id: conn.sid.clone(),
            attach_token: conn.token.clone(),
            attached: conn.attached,
        },
    );
    if ws_tx
        .send(Message::Binary(proto::encode(&f)))
        .await
        .is_err()
    {
        store.detach(&conn.token, conn.id);
        return;
    }

    // 4) 下行泵: 会话输出 -> WS。新 attach 接管或慢消费者时,
    //    forward task 会通知本 WS 主动关闭。
    let mut output = conn.output;
    let mut close_rx = conn.close_rx;
    let mut close_rx_open = true;
    let down = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                signal = &mut close_rx, if close_rx_open => {
                    match signal {
                        Ok(reason) => {
                            let message = reason.message();
                            // slow consumer 是会话级错误, 前端应丢掉 token;
                            // Replaced 只是刷新接管, 不发 CLOSED, 避免旧页面误删 token。
                            let code = match reason {
                                CloseReason::Replaced => 1000,
                                CloseReason::SlowConsumer => {
                                    let f = proto::Frame::json(
                                        proto::CLOSED,
                                        &proto::Closed { reason: message.into() },
                                    );
                                    let _ = ws_tx
                                        .send(Message::Binary(proto::encode(&f)))
                                        .await;
                                    1008 // policy violation
                                }
                            };
                            let _ = ws_tx
                                .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                                    code,
                                    reason: message.into(),
                                })))
                                .await;
                            break;
                        }
                        // 会话终结时 sender 与输出队列一起 drop。关闭本分支,
                        // 由 output=None 走正常 exit 路径。
                        Err(_) => close_rx_open = false,
                    }
                }
                msg = output.recv() => {
                    let frame = match msg {
                        Some(item) => match item {
                            OutItem::Data(bytes) => proto::Frame::data(bytes),
                            OutItem::ReplayEnd => proto::Frame::replay_end(),
                        },
                        None => {
                            // 会话已终结(shell 退出 / 宽限期耗尽被回收), 告知前端后收尾
                            proto::Frame::json(proto::CLOSED, &proto::Closed { reason: "exit".into() })
                        }
                    };
                    let closed = frame.kind == proto::CLOSED;
                    if ws_tx.send(Message::Binary(proto::encode(&frame))).await.is_err() || closed {
                        break;
                    }
                }
            }
        }
    });

    // 5) 上行泵: WS -> PTY(键入与 resize 的最小翻译)
    while let Some(Ok(msg)) = ws_rx.next().await {
        let Message::Binary(b) = msg else {
            match msg {
                Message::Close(_) => break,
                _ => continue, // Ping/Pong 由 axum 自动应答, Text 忽略
            }
        };
        let Some(f) = proto::decode_one(&b) else {
            continue;
        };
        match f.kind {
            proto::DATA => {
                if conn.input.send(f.payload).await.is_err() {
                    break; // 会话已死
                }
            }
            proto::RESIZE => {
                if let Ok(r) = f.parse::<proto::Resize>() {
                    let _ = conn
                        .control
                        .send(Control::Resize {
                            cols: r.cols,
                            rows: r.rows,
                        })
                        .await;
                }
            }
            _ => {}
        }
    }

    // 6) WS 断开: 停下行泵; 会话不杀, 由 detach 的宽限定时器兜底回收
    down.abort();
    store.detach(&conn.token, conn.id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_redirect_preserves_path_and_query() {
        let uri: axum::http::Uri = "/freebsd/empty/?from=http".parse().unwrap();
        assert_eq!(
            https_location("https://www.yizishun.com", &uri),
            "https://www.yizishun.com/freebsd/empty/?from=http"
        );
    }

    #[test]
    fn https_redirect_root_has_one_slash() {
        let uri: axum::http::Uri = "/".parse().unwrap();
        assert_eq!(
            https_location("https://www.yizishun.com/", &uri),
            "https://www.yizishun.com/"
        );
    }
}
