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

use std::net::{IpAddr, SocketAddr};
use std::path::Path;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{ConnectInfo, State, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use futures::{SinkExt, StreamExt};
use termblog_core::{Config, Control, SessionClient};
use termblog_proto as proto;
use tokio::sync::broadcast;
use tower_http::services::ServeDir;

use sessions::SessionStore;

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
    if let Ok(v) = std::env::var("TERMBLOG_SOCKET") {
        cfg.jail.socket = v.into();
    }

    let state = SessionStore::new(SessionClient::new(&cfg.jail.socket));

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .fallback_service(ServeDir::new(&cfg.web.static_dir)) // 前端构建产物; 开发可用 vite dev 代理
        .with_state(state);

    // 默认 0.0.0.0:8080(生产直连), TERMBLOG_LISTEN 可覆盖
    let listener = tokio::net::TcpListener::bind(&cfg.web.listen).await?;
    println!(
        "termblog-web listening on http://{} (jaild socket {})",
        cfg.web.listen,
        cfg.jail.socket.display()
    );
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<SessionStore>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    ws.on_upgrade(move |sock| handle(sock, state, peer.ip()))
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

    // 2) attach 优先; token 缺失/失效(会话已回收)则开新会话,
    //    由 Opened.attached 告知前端是恢复还是新开(前端据此重置屏幕)
    let mut conn = None;
    if let Some(token) = open.attach_token.as_deref() {
        conn = store.attach(token).await;
    }
    let conn = match conn {
        Some(c) => c,
        None => match store.create(peer, open.cols, open.rows, open.attach_token.clone()).await {
            Ok(c) => c,
            Err(e) => {
                let f = proto::Frame::json(proto::CLOSED, &proto::Closed { reason: e.to_string() });
                let _ = ws_tx.send(Message::Binary(proto::encode(&f))).await;
                return;
            }
        },
    };

    // 3) 同步窗口尺寸: attach 回来的连接尺寸可能变了; 顺带触发 SIGWINCH
    //    让 vim/less 等前台程序在回放内容之上重绘到最新画面
    let _ = conn.control.send(Control::Resize { cols: open.cols, rows: open.rows }).await;

    let f = proto::Frame::json(proto::OPENED, &proto::Opened {
        session_id: conn.sid.clone(),
        attach_token: conn.token.clone(),
        attached: conn.attached,
    });
    if ws_tx.send(Message::Binary(proto::encode(&f))).await.is_err() {
        store.detach(&conn.token);
        return;
    }

    // 4) 下行泵: 会话输出 -> WS(纯透传, 套上 Data 帧头)
    let mut output = conn.output;
    let down = tokio::spawn(async move {
        loop {
            let frame = match output.recv().await {
                Ok(bytes) => proto::Frame::data(bytes),
                Err(broadcast::error::RecvError::Lagged(_)) => continue, // 慢消费者丢帧
                Err(broadcast::error::RecvError::Closed) => {
                    // 会话已终结(shell 退出 / 宽限期耗尽被回收), 告知前端后收尾
                    proto::Frame::json(proto::CLOSED, &proto::Closed { reason: "exit".into() })
                }
            };
            let closed = frame.kind == proto::CLOSED;
            if ws_tx.send(Message::Binary(proto::encode(&frame))).await.is_err() || closed {
                break;
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
        let Some(f) = proto::decode_one(&b) else { continue };
        match f.kind {
            proto::DATA => {
                if conn.input.send(f.payload).await.is_err() {
                    break; // 会话已死
                }
            }
            proto::RESIZE => {
                if let Ok(r) = f.parse::<proto::Resize>() {
                    let _ = conn.control.send(Control::Resize { cols: r.cols, rows: r.rows }).await;
                }
            }
            _ => {}
        }
    }

    // 6) WS 断开: 停下行泵; 会话不杀, 由 detach 的宽限定时器兜底回收
    down.abort();
    store.detach(&conn.token);
}
