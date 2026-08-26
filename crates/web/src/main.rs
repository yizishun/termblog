//! termblog-web [降权]: axum WS 端点 + 前端静态资源。
//!
//! WS binary message 与 proto 帧一一对应, 网关只做透传, 零协议转换。
//! 开发模式(本文件即如此): 进程内直接内嵌 SessionManager + LocalBackend;
//! 生产模式(M3)把 SessionManager 换成走 Unix socket 的 SessionClient 即可。
//!
//! 刷新重连由 web 层承接, core 保持 M1 语义(连接断 = 杀 shell)完全无感:
//! 浏览器断开 WS 后, web 把会话通道在内存里继续持有 IDLE_GRACE 宽限期;
//! 期间凭 attach_token 刷新页面即接回原会话。attach 时转发 task 会把近期
//! 输出(scrollback 定长缓冲)回放给新连接, 屏幕内容原样恢复; 宽限期耗尽 ->
//! 通道被 drop -> core 按 M1 语义回收 shell。
//! "浏览器连接很脆、需要有人替它捧着会话"是接入层的传输补偿, 不属于
//! 特权进程(M3 的 jaild)的职责, 所以放在这里。

use std::collections::HashMap;
use std::io::Read as _;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{ConnectInfo, State, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use termblog_core::{Control, LocalBackend, Quota, SessionManager};
use termblog_proto as proto;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tower_http::services::ServeDir;

/// 断开后 web 替浏览器持有会话的宽限期
const IDLE_GRACE: Duration = Duration::from_secs(60);
/// 每会话输出广播容量(条数), 与 core 一致: 慢消费者丢帧, 由 xterm.js 重绘
const OUTPUT_CHUNKS: usize = 256;
/// attach 回放的 scrollback 字节上限(近期输出, 超长丢最旧)
const SCROLLBACK_BYTES: usize = 128 * 1024;
/// 回放切片大小: 回放条数 <= SCROLLBACK_BYTES / REPLAY_CHUNK,
/// 保证回放本身不会撑爆广播的 OUTPUT_CHUNKS 容量导致丢回放内容
const REPLAY_CHUNK: usize = 4096;

/// attach 时交给转发 task 的命令。订阅与回放必须在转发 task 内完成:
/// 它是输出广播的唯一写入者, 只有它能把两件事放进同一段无并发窗口里,
/// 保证新订阅者收到「完整回放 + 之后的实时输出」, 不重不漏。
enum FwdCmd {
    Attach { reply: oneshot::Sender<broadcast::Receiver<Bytes>> },
}

/// web 侧持有的会话: core 句柄拆开的通道端 + 转发 task 的命令通道 + 连接计数
struct WebSession {
    sid: String,
    input: mpsc::Sender<Bytes>,
    control: mpsc::Sender<Control>,
    /// 发给转发 task 的命令(attach 时请求「订阅 + 回放」)。转发 task 是
    /// core 输出广播的唯一写入者, 输出订阅只能通过它进行
    fwd_cmd: mpsc::Sender<FwdCmd>,
    /// 当前活跃 WS 连接数; 归零才启动宽限定时器
    connections: usize,
    /// 无连接时的宽限倒计时; 新连接 attach 时取消
    idle_timer: Option<JoinHandle<()>>,
}

#[derive(Clone)]
struct AppState {
    mgr: SessionManager,
    sessions: Arc<Mutex<HashMap<String, WebSession>>>, // attach_token -> 会话
}

/// 一条 WS 连接拿到的会话视图(发送端是 clone, 输出是本次连接的订阅)
struct Connection {
    sid: String,
    token: String,
    attached: bool,
    input: mpsc::Sender<Bytes>,
    control: mpsc::Sender<Control>,
    output: broadcast::Receiver<Bytes>,
}

#[tokio::main]
async fn main() {
    let state = AppState {
        mgr: SessionManager::new(LocalBackend, Quota { max_total: 64, max_per_ip: 3 }),
        sessions: Arc::new(Mutex::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .fallback_service(ServeDir::new("frontend/dist")) // 前端构建产物; 开发可用 vite dev 代理
        .with_state(state);

    // 监听地址可用环境变量覆盖(方便多实例调试), 默认 127.0.0.1:8080
    let addr = std::env::var("TERMBLOG_LISTEN").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    println!("termblog-web listening on http://{addr}");
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .unwrap();
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    ws.on_upgrade(move |sock| handle(sock, state, peer.ip()))
}

/// 凭 token 接回 web 持有的会话: 取消宽限定时器, 连接计数 +1,
/// 并让转发 task 现做「订阅 + scrollback 回放」, 拿到一个从历史输出开始的订阅。
/// 返回 None = token 无效 / 会话已终结, 调用方转为开新会话。
async fn attach(state: &AppState, token: &str) -> Option<Connection> {
    let (sid, input, control, fwd_cmd) = {
        let mut map = state.sessions.lock().unwrap();
        let s = map.get_mut(token)?;
        if let Some(t) = s.idle_timer.take() {
            t.abort();
        }
        s.connections += 1;
        (s.sid.clone(), s.input.clone(), s.control.clone(), s.fwd_cmd.clone())
    };

    // 若发送失败: 转发 task 已退出, 而它退出前必先移除持表条目
    // (见 forwarder 的清理), 所以这里无需回滚计数, 直接回落开新会话即可
    let (reply_tx, reply_rx) = oneshot::channel();
    fwd_cmd.send(FwdCmd::Attach { reply: reply_tx }).await.ok()?;
    let output = reply_rx.await.ok()?;

    Some(Connection {
        sid,
        token: token.to_string(),
        attached: true,
        input,
        control,
        output,
    })
}

/// 开新会话: core 起 shell -> 输出转发 task -> 登记进持有表
async fn create(state: &AppState, peer: IpAddr, cols: u16, rows: u16) -> anyhow::Result<Connection> {
    let session = state.mgr.create(peer, cols, rows).await?; // 配额超限在此被拒

    let token = gen_token();
    let (out_tx, out_rx) = broadcast::channel(OUTPUT_CHUNKS);
    let (cmd_tx, mut cmd_rx) = mpsc::channel(8);

    // 转发 task: core 输出 -> web 侧广播 + scrollback 缓冲; 同时处理 attach
    // 命令(现做订阅 + 回放)。core 会话终结(shell 退出/被回收)时广播关闭,
    // 顺势把会话移出持有表(发送端一并 drop, 幂等)
    {
        let sessions = state.sessions.clone();
        let token = token.clone();
        let mut output = session.output;
        tokio::spawn(async move {
            let mut scrollback: Vec<u8> = Vec::new(); // 近期输出, 超长丢最旧
            loop {
                tokio::select! {
                    // attach: 订阅与回放在同一 task 的同一臂里顺序完成——
                    // 本 task 是广播唯一写入者, 处理本臂期间不会有新输出插进
                    // scrollback 与回放之间, 所以新订阅者不重不漏
                    cmd = cmd_rx.recv() => match cmd {
                        Some(FwdCmd::Attach { reply }) => {
                            let rx = out_tx.subscribe();
                            for chunk in scrollback.chunks(REPLAY_CHUNK) {
                                let _ = out_tx.send(Bytes::copy_from_slice(chunk));
                            }
                            let _ = reply.send(rx);
                        }
                        // 不可达: 持表条目一直握着 cmd_tx, 直到本 task 清理时才移除
                        None => break,
                    },
                    msg = output.recv() => match msg {
                        Ok(b) => {
                            scrollback.extend_from_slice(&b);
                            if scrollback.len() > SCROLLBACK_BYTES {
                                let excess = scrollback.len() - SCROLLBACK_BYTES;
                                scrollback.drain(..excess);
                            }
                            let _ = out_tx.send(b);
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue, // 慢消费者丢帧
                        Err(broadcast::error::RecvError::Closed) => break,       // 会话终结
                    },
                }
            }
            sessions.lock().unwrap().remove(&token);
        });
    }

    state.sessions.lock().unwrap().insert(token.clone(), WebSession {
        sid: session.id.clone(),
        input: session.input.clone(),
        control: session.control.clone(),
        fwd_cmd: cmd_tx,
        connections: 1,
        idle_timer: None,
    });

    Ok(Connection {
        sid: session.id,
        token,
        attached: false,
        input: session.input,
        control: session.control,
        output: out_rx,
    })
}

/// WS 断开: 连接计数 -1; 归零则启动宽限定时器, 到点把会话移出持有表
/// (发送端被 drop => core 收不到输入 => 按 M1 语义回收 shell)
fn detach(state: &AppState, token: &str) {
    let mut map = state.sessions.lock().unwrap();
    let Some(s) = map.get_mut(token) else { return }; // 会话已终结(转发 task 先移除了)
    s.connections -= 1;
    if s.connections > 0 {
        return; // 同 token 还有别的活跃连接(复制标签页等), 不启动倒计时
    }
    let sessions = state.sessions.clone();
    let token = token.to_string();
    s.idle_timer = Some(tokio::spawn(async move {
        tokio::time::sleep(IDLE_GRACE).await;
        sessions.lock().unwrap().remove(&token);
    }));
}

async fn handle(sock: WebSocket, state: AppState, peer: IpAddr) {
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
        conn = attach(&state, token).await;
    }
    let conn = match conn {
        Some(c) => c,
        None => match create(&state, peer, open.cols, open.rows).await {
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
        detach(&state, &conn.token);
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
    detach(&state, &conn.token);
}

/// 128-bit 随机 attach token(取自 /dev/urandom)。token 即重连凭证: 不可猜,
/// 谁持有谁就能 attach, 无需再校验来源 IP(换网络场景合法)。
fn gen_token() -> String {
    let mut b = [0u8; 16];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_err() {
        // 兜底: 时间戳(非加密安全, 仅防 urandom 不可用的极端环境)
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u128)
            .unwrap_or(0);
        b.copy_from_slice(&nanos.to_be_bytes());
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}
