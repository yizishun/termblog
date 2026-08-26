//! termblog-web [降权]: axum WS 端点 + 前端静态资源。
//!
//! WS binary message 与 proto 帧一一对应, 网关只做透传, 零协议转换。
//! 开发模式(本文件即如此): 进程内直接内嵌 SessionManager + LocalBackend;
//! 生产模式(M3)把 SessionClient 换成走 Unix socket 的实现即可, 本文件不用动结构。

use std::net::{IpAddr, SocketAddr};

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{ConnectInfo, State, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use futures::{SinkExt, StreamExt};
use termblog_core::{Control, LocalBackend, Quota, SessionManager};
use termblog_proto as proto;
use tokio::sync::broadcast;
use tower_http::services::ServeDir;

#[tokio::main]
async fn main() {
    let mgr = SessionManager::new(LocalBackend, Quota { max_total: 64, max_per_ip: 3 });

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .fallback_service(ServeDir::new("frontend/dist")) // 前端构建产物; 开发可用 vite dev 代理
        .with_state(mgr);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:8080").await.unwrap();
    println!("termblog-web listening on http://127.0.0.1:8080");
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .unwrap();
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(mgr): State<SessionManager>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    ws.on_upgrade(move |sock| handle(sock, mgr, peer.ip()))
}

async fn handle(sock: WebSocket, mgr: SessionManager, peer: IpAddr) {
    let (mut ws_tx, mut ws_rx) = sock.split();

    // 1) 握手: 第一条消息必须是 Open { cols, rows }
    let open = match ws_rx.next().await {
        Some(Ok(Message::Binary(b))) => match proto::decode_one(&b).and_then(|f| f.parse().ok()) {
            Some(o) => o,
            None => return,
        },
        _ => return,
    };
    let open: proto::Open = open;

    // 2) 开会话(配额超限直接回 Closed, 不排队)
    let session = match mgr.create(peer, open.cols, open.rows).await {
        Ok(s) => s,
        Err(e) => {
            let f = proto::Frame::json(proto::CLOSED, &proto::Closed { reason: e.to_string() });
            let _ = ws_tx.send(Message::Binary(proto::encode(&f))).await;
            return;
        }
    };
    let f = proto::Frame::json(proto::OPENED, &proto::Opened { session_id: session.id.clone() });
    if ws_tx.send(Message::Binary(proto::encode(&f))).await.is_err() {
        return;
    }

    // 3) 下行泵: PTY 输出 -> WS(纯透传, 套上 Data 帧头)
    let mut output = session.output; // broadcast::Receiver 移入 task
    let down = tokio::spawn(async move {
        loop {
            let frame = match output.recv().await {
                Ok(bytes) => proto::Frame::data(bytes),
                Err(broadcast::error::RecvError::Lagged(_)) => continue, // 慢消费者丢帧
                Err(broadcast::error::RecvError::Closed) => {
                    // 会话已终结(shell 退出 / 被回收), 告知前端后收尾
                    proto::Frame::json(proto::CLOSED, &proto::Closed { reason: "exit".into() })
                }
            };
            let closed = frame.kind == proto::CLOSED;
            if ws_tx.send(Message::Binary(proto::encode(&frame))).await.is_err() || closed {
                break;
            }
        }
    });

    // 4) 上行泵: WS -> PTY(键入与 resize 的最小翻译)
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
                if session.input.send(f.payload).await.is_err() {
                    break; // 会话已死
                }
            }
            proto::RESIZE => {
                if let Ok(r) = f.parse::<proto::Resize>() {
                    let _ = session.control.send(Control::Resize { cols: r.cols, rows: r.rows }).await;
                }
            }
            _ => {}
        }
    }

    // 5) WS 断开: 停下行泵; drop(session) 触发 core 侧回收
    down.abort();
}
