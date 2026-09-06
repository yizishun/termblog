//! web 侧会话持有: 「浏览器刷新不丢会话」的全部逻辑。
//!
//! 分层理由: WS 连接很脆(切标签、刷新、网络抖动), 会话(真实 jail + shell)
//! 却贵, 所以接入层替浏览器在断开后继续捧着会话一段时间。这份「捧」的逻辑
//! 与「一条 WS 连接怎么收发帧」无关, 单独放这个模块:
//!
//!   - `WebSession`: 持有表条目(句柄通道端 + 转发 task 命令通道 + 当前连接 + 宽限定时器)
//!   - `attach`: 凭 token 接回(刷新路径), 取消宽限、接管旧连接 + scrollback 回放
//!   - `create`: `client.open` 开新会话 → 起转发 task(scrollback 缓冲 + attach 回放) → 登记
//!   - `detach`: 当前连接断开后起 60s 宽限定时器, 到点移除条目
//!     (通道 drop → jaild 回收 shell); 被接管旧连接的迟到 detach 无效
//!
//! jaild 侧没有持有表(socket 断开即回收会话), 刷新重连的宽限与回放
//! 只在本模块; web 发给前端的 token 由本进程生成, 只在本进程内存里有效。
//!
//! 输出链路(图片二期, 字节零丢失): 终端字节流是有状态协议, 丢一块即破坏
//! 后续 ANSI/IIP 序列且不可自愈, 所以每会话只保留一条活跃 WS 的有界 mpsc:
//!   - forward task 是 core output 的唯一消费者, 消费 → scrollback 环(128 KiB)
//!     → try_send 到当前连接;
//!   - 队列满 → 关闭慢连接, 绝不静默丢帧;
//!   - 新 attach 原子替换当前连接, 刷新时新旧 WS 短暂重叠也不会扇出;
//!   - 队列 128 条(约 1 MiB), 足以容纳单条完整 IIP 序列(≤683 KiB + 头)。
//!
//! 内存上限: 每会话一条输出队列 × session.max_total。

use std::collections::HashMap;
use std::io::Read as _;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use termblog_core::{Control, SessionClient};
use tokio::sync::{mpsc, oneshot};

/// 断开后 web 替浏览器持有会话的宽限期
const IDLE_GRACE: Duration = Duration::from_secs(60);
/// 每连接输出私有队列容量(条; 单条 ≤ 8 KiB PTY 读块, ≈ 1 MiB)
const OUTPUT_CHUNKS: usize = 128;
/// attach 回放的 scrollback 字节上限(近期输出, 超长丢最旧)
const SCROLLBACK_BYTES: usize = 128 * 1024;
/// 回放切片大小: 回放条数 <= SCROLLBACK_BYTES / REPLAY_CHUNK (=32),
/// 保证回放本身不会撑爆私有队列的 OUTPUT_CHUNKS 容量导致回放丢内容
const REPLAY_CHUNK: usize = 4096;

/// attach 时交给转发 task 的命令。回放与 owner 切换必须在唯一输出消费者内完成,
/// 才能保证新连接收到「完整回放 + 之后的实时输出」, 不重不漏。
enum FwdCmd {
    Attach {
        id: u64,
        subscriber: Subscriber,
        reply: oneshot::Sender<bool>,
    },
}

/// 服务端主动关闭 WS 的原因。
#[derive(Clone, Copy)]
pub enum CloseReason {
    /// 同 token 的新 WS 已接管会话。
    Replaced,
    /// WS 消费赶不上 PTY 输出。
    SlowConsumer,
}

impl CloseReason {
    pub fn message(self) -> &'static str {
        match self {
            Self::Replaced => "replaced by reconnect",
            Self::SlowConsumer => "slow consumer",
        }
    }
}

/// 转发 task 维护的唯一活跃 WS 连接。
struct Subscriber {
    id: u64,
    queue: mpsc::Sender<OutItem>,
    close: oneshot::Sender<CloseReason>,
}

/// 输出队列条目: 裸 PTY 字节或回放结束标记。
/// ReplayEnd 必须在「回放块之后、实时输出之前」进入同一队列, 前端据此在
/// Opened 与 ReplayEnd 之间关闭 xterm stdin(回放里的终端查询不能触发回答上行)。
#[derive(Debug, PartialEq, Eq)]
pub enum OutItem {
    Data(Bytes),
    ReplayEnd,
}

/// web 侧持有的会话: 句柄通道端 + 转发 task 命令通道 + 当前 owner
struct WebSession {
    sid: String,
    input: mpsc::Sender<Bytes>,
    control: mpsc::Sender<Control>,
    fwd_cmd: mpsc::Sender<FwdCmd>,
    /// 当前活跃 WS; None = 已断开、正在宽限期。
    owner: Option<u64>,
    /// 无连接时的宽限倒计时; 新连接 attach 时取消
    idle_timer: Option<tokio::task::JoinHandle<()>>,
}

/// 一条 WS 连接拿到的会话视图。
pub struct Connection {
    pub id: u64,
    pub sid: String,
    pub token: String,
    pub attached: bool,
    pub input: mpsc::Sender<Bytes>,
    pub control: mpsc::Sender<Control>,
    pub output: mpsc::Receiver<OutItem>,
    /// 新 attach 接管或慢消费者时, forward task 通知 WS 写循环关闭。
    pub close_rx: oneshot::Receiver<CloseReason>,
}

/// 会话持有者: main.rs 只经它开会话/接回/交还, 不碰持有表细节。
#[derive(Clone)]
pub struct SessionStore {
    client: SessionClient,
    table: Arc<Mutex<HashMap<String, WebSession>>>, // attach_token -> 会话
}

impl SessionStore {
    pub fn new(client: SessionClient) -> Self {
        Self {
            client,
            table: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 凭 token 接回 web 持有的会话: 取消宽限定时器,
    /// 并让转发 task 原子执行「scrollback 回放 + 接管旧连接」。
    /// 返回 None = token 无效 / 会话已终结, 调用方转为开新会话。
    pub async fn attach(&self, token: &str) -> Option<Connection> {
        let (sid, input, control, fwd_cmd) = {
            let mut table = self.table.lock().unwrap();
            let s = table.get_mut(token)?;
            if let Some(t) = s.idle_timer.take() {
                t.abort();
            }
            (
                s.sid.clone(),
                s.input.clone(),
                s.control.clone(),
                s.fwd_cmd.clone(),
            )
        };

        let (queue_tx, queue_rx) = mpsc::channel(OUTPUT_CHUNKS);
        let (close_tx, close_rx) = oneshot::channel();
        let id = next_conn_id();
        let (reply_tx, reply_rx) = oneshot::channel();
        fwd_cmd
            .send(FwdCmd::Attach {
                id,
                subscriber: Subscriber {
                    id,
                    queue: queue_tx,
                    close: close_tx,
                },
                reply: reply_tx,
            })
            .await
            .ok()?;
        if !reply_rx.await.ok()? {
            return None;
        }

        Some(Connection {
            id,
            sid,
            token: token.to_string(),
            attached: true,
            input,
            control,
            output: queue_rx,
            close_rx,
        })
    }

    /// 开新会话: jaild 起 jail shell -> 输出转发 task -> 登记进持有表。
    /// attach_token 仍随 Open 透传(协议字段兼容), jaild 已不再使用。
    /// caps: 客户端能力通告(如 ["img-iterm2"]), 随 Open 透传给 jaild。
    /// attach 路径不重新开会话, caps 只在 create 时生效(attach 回来的会话
    /// 按创建时能力运行; 镜像页永远 fresh, 主路径无此问题)。
    pub async fn create(
        &self,
        peer: IpAddr,
        cols: u16,
        rows: u16,
        attach_token: Option<String>,
        caps: Vec<String>,
    ) -> Result<Connection> {
        let session = self
            .client
            .open(peer, cols, rows, attach_token, caps)
            .await?; // 配额超限在此被拒

        let token = gen_token();
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let (queue_tx, queue_rx) = mpsc::channel(OUTPUT_CHUNKS);
        let (close_tx, close_rx) = oneshot::channel();
        let id = next_conn_id();

        // 先登记再起 task: task 退出时的 remove 不会与登记顺序竞态。
        self.table.lock().unwrap().insert(
            token.clone(),
            WebSession {
                sid: session.id.clone(),
                input: session.input.clone(),
                control: session.control.clone(),
                fwd_cmd: cmd_tx,
                owner: Some(id),
                idle_timer: None,
            },
        );

        tokio::spawn(forward_output(
            self.table.clone(),
            token.clone(),
            session.id.clone(),
            session.output,
            cmd_rx,
            Subscriber {
                id,
                queue: queue_tx,
                close: close_tx,
            },
        ));

        Ok(Connection {
            id,
            sid: session.id,
            token,
            attached: false,
            input: session.input,
            control: session.control,
            output: queue_rx,
            close_rx,
        })
    }

    /// fresh 开局前的旧会话回收(镜像页 Open.fresh=1 时由网关调用):
    /// token 对应的会话若无活跃连接(owner=None), 立即移出持有表 ——
    /// 条目 drop → 转发 task 清理 → socket 断开 → jaild 回收 jail, 配额即时释放。
    /// 有活跃连接则保留给它; token 不存在则无操作。
    pub fn reset(&self, token: &str) {
        let mut table = self.table.lock().unwrap();
        let idle = table.get(token).map(|s| s.owner.is_none()).unwrap_or(false);
        if idle {
            table.remove(token);
        }
    }

    /// 当前 WS 断开: 启动宽限定时器, 到点把会话移出持有表。
    /// 被新 attach 接管的旧 WS 会带旧 id 迟到此处, 必须忽略它。
    pub fn detach(&self, token: &str, id: u64) {
        let mut table = self.table.lock().unwrap();
        let Some(s) = table.get_mut(token) else {
            return;
        }; // 会话已终结
        if s.owner != Some(id) {
            return;
        }
        s.owner = None;
        let table = self.table.clone();
        let token = token.to_string();
        s.idle_timer = Some(tokio::spawn(async move {
            tokio::time::sleep(IDLE_GRACE).await;
            table.lock().unwrap().remove(&token);
        }));
    }
}

/// core output 的唯一消费者: 维护 scrollback, 并只向当前 WS 转发。
async fn forward_output(
    table: Arc<Mutex<HashMap<String, WebSession>>>,
    token: String,
    sid: String,
    mut output: mpsc::Receiver<Bytes>,
    mut commands: mpsc::Receiver<FwdCmd>,
    initial: Subscriber,
) {
    let mut active = Some(initial);
    let mut scrollback: Vec<u8> = Vec::new();

    // 新会话没有 scrollback 回放, 但 ReplayEnd 仍必须先于实时输出入队:
    // 前端在 Opened 后禁用了 xterm stdin, 没有这帧就永远不恢复。
    // 队列容量 128, 此时必为空, 发送不会失败; 失败只可能是连接已死, 直接返回。
    if let Some(first) = &active {
        if first.queue.send(OutItem::ReplayEnd).await.is_err() {
            table.lock().unwrap().remove(&token);
            return;
        }
    }

    loop {
        tokio::select! {
            // 回放期间不会处理实时输出, 因此回放与后续输出不重不漏。
            cmd = commands.recv() => match cmd {
                Some(FwdCmd::Attach { id, subscriber, reply }) => {
                    let mut replayed = true;
                    for chunk in scrollback.chunks(REPLAY_CHUNK) {
                        // 回放 ≤32 条 < 队列容量 128, 新队列必为空。
                        if subscriber
                            .queue
                            .send(OutItem::Data(Bytes::copy_from_slice(chunk)))
                            .await
                            .is_err()
                        {
                            replayed = false;
                            break;
                        }
                    }
                    // 无论回放是否完整, ReplayEnd 都必须入队(且排在实时输出之前):
                    // 前端只在收到它之后恢复 stdin, 缺失会让终端输入永久禁用。
                    let _ = subscriber.queue.send(OutItem::ReplayEnd).await;
                    if !replayed {
                        let _ = reply.send(false);
                        continue;
                    }

                    // owner 在本 task 内切换, 并发 attach 也严格串行。先更新 id,
                    // 旧连接随后的 detach 才不会启动宽限定时器。
                    let installed = {
                        let mut table = table.lock().unwrap();
                        if let Some(s) = table.get_mut(&token) {
                            if let Some(t) = s.idle_timer.take() {
                                t.abort();
                            }
                            s.owner = Some(id);
                            true
                        } else {
                            false
                        }
                    };
                    if installed {
                        if let Some(old) = active.replace(subscriber) {
                            let _ = old.close.send(CloseReason::Replaced);
                        }
                    }
                    let _ = reply.send(installed);
                }
                None => break,
            },
            msg = output.recv() => match msg {
                Some(b) => {
                    scrollback.extend_from_slice(&b);
                    if scrollback.len() > SCROLLBACK_BYTES {
                        let excess = scrollback.len() - SCROLLBACK_BYTES;
                        scrollback.drain(..excess);
                    }
                    if let Some(s) = active.take() {
                        match s.queue.try_send(OutItem::Data(b)) {
                            Ok(()) => active = Some(s),
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                tracing::warn!(
                                    sid = %sid,
                                    conn = s.id,
                                    "slow consumer: output queue full, closing WS connection"
                                );
                                let _ = s.close.send(CloseReason::SlowConsumer);
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {}
                        }
                    }
                }
                None => break, // 会话终结
            },
        }
    }
    table.lock().unwrap().remove(&token);
}

/// owner generation: 区分当前连接与被接管连接的迟到 detach。
fn next_conn_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// 128-bit 随机 attach token(取自 /dev/urandom)。token 即重连凭证: 不可猜,
/// 谁持有谁就能 attach, 无需再校验来源 IP(换网络场景合法)。
fn gen_token() -> String {
    let mut b = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .is_err()
    {
        // 兜底: 时间戳(非加密安全, 仅防 urandom 不可用的极端环境)
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        b.copy_from_slice(&nanos.to_be_bytes());
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn attach_replaces_old_connection_and_keeps_scrollback() {
        let store = SessionStore::new(SessionClient::new("/unused"));
        let token = "test-token".to_string();
        let old_id = next_conn_id();

        let (input, _input_rx) = mpsc::channel(1);
        let (control, _control_rx) = mpsc::channel(1);
        let (commands, command_rx) = mpsc::channel(8);
        let (pty_output, output_rx) = mpsc::channel(8);
        let (old_queue, mut old_output) = mpsc::channel(8);
        let (old_close, old_close_rx) = oneshot::channel();

        store.table.lock().unwrap().insert(
            token.clone(),
            WebSession {
                sid: "sid".into(),
                input,
                control,
                fwd_cmd: commands,
                owner: Some(old_id),
                idle_timer: None,
            },
        );
        let task = tokio::spawn(forward_output(
            store.table.clone(),
            token.clone(),
            "sid".into(),
            output_rx,
            command_rx,
            Subscriber {
                id: old_id,
                queue: old_queue,
                close: old_close,
            },
        ));

        // 新会话的 initial subscriber 先收到 ReplayEnd, 之后才是实时输出
        assert_eq!(
            old_output.recv().await.unwrap(),
            OutItem::ReplayEnd,
            "新会话必须先发 ReplayEnd(空回放)"
        );
        pty_output
            .send(Bytes::from_static(b"before"))
            .await
            .unwrap();
        assert_eq!(
            old_output.recv().await.unwrap(),
            OutItem::Data(Bytes::from_static(b"before"))
        );

        let mut new = store.attach(&token).await.expect("attach 应成功");
        assert!(matches!(old_close_rx.await, Ok(CloseReason::Replaced)));
        assert_eq!(
            new.output.recv().await.unwrap(),
            OutItem::Data(Bytes::from_static(b"before")),
            "attach 先回放 scrollback"
        );
        assert_eq!(
            new.output.recv().await.unwrap(),
            OutItem::ReplayEnd,
            "回放块之后必须紧跟 ReplayEnd"
        );
        assert!(old_output.recv().await.is_none(), "旧连接的输出队列应关闭");

        pty_output.send(Bytes::from_static(b"after")).await.unwrap();
        assert_eq!(
            new.output.recv().await.unwrap(),
            OutItem::Data(Bytes::from_static(b"after")),
            "ReplayEnd 之后的实时输出仍照常送达"
        );

        // 被接管连接的迟到 detach 不能让当前会话进入宽限期。
        store.detach(&token, old_id);
        {
            let table = store.table.lock().unwrap();
            let session = table.get(&token).unwrap();
            assert_eq!(session.owner, Some(new.id));
            assert!(session.idle_timer.is_none());
        }

        store.detach(&token, new.id);
        {
            let mut table = store.table.lock().unwrap();
            let session = table.get_mut(&token).unwrap();
            assert_eq!(session.owner, None);
            session.idle_timer.take().unwrap().abort();
        }

        drop(pty_output);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn failed_replay_still_attempts_replay_end_and_keeps_session() {
        let store = SessionStore::new(SessionClient::new("/unused"));
        let token = "test-token".to_string();
        let old_id = next_conn_id();

        let (input, _input_rx) = mpsc::channel(1);
        let (control, _control_rx) = mpsc::channel(1);
        let (commands, command_rx) = mpsc::channel(8);
        let (pty_output, output_rx) = mpsc::channel(8);
        let (old_queue, mut old_output) = mpsc::channel(8);
        let (old_close, _old_close_rx) = oneshot::channel();

        store.table.lock().unwrap().insert(
            token.clone(),
            WebSession {
                sid: "sid".into(),
                input,
                control,
                fwd_cmd: commands.clone(),
                owner: Some(old_id),
                idle_timer: None,
            },
        );
        let task = tokio::spawn(forward_output(
            store.table.clone(),
            token.clone(),
            "sid".into(),
            output_rx,
            command_rx,
            Subscriber {
                id: old_id,
                queue: old_queue,
                close: old_close,
            },
        ));

        // 新会话的 ReplayEnd 先行; 然后积累一段 scrollback
        assert_eq!(old_output.recv().await.unwrap(), OutItem::ReplayEnd);
        pty_output
            .send(Bytes::from_static(b"history"))
            .await
            .unwrap();
        assert_eq!(
            old_output.recv().await.unwrap(),
            OutItem::Data(Bytes::from_static(b"history"))
        );

        // 新订阅者的接收端立刻 drop → 回放中途失败: reply=false, 会话不因此终结
        let (dead_queue, dead_rx) = mpsc::channel(8);
        drop(dead_rx);
        let (dead_close, _) = oneshot::channel();
        let (reply_tx, reply_rx) = oneshot::channel();
        commands
            .send(FwdCmd::Attach {
                id: next_conn_id(),
                subscriber: Subscriber {
                    id: next_conn_id(),
                    queue: dead_queue,
                    close: dead_close,
                },
                reply: reply_tx,
            })
            .await
            .unwrap();
        assert_eq!(
            reply_rx.await.unwrap(),
            false,
            "回放失败必须如实上报(attach 转开新会话)"
        );

        // 会话仍存活, 旧连接继续收实时输出
        pty_output
            .send(Bytes::from_static(b"still-alive"))
            .await
            .unwrap();
        assert_eq!(
            old_output.recv().await.unwrap(),
            OutItem::Data(Bytes::from_static(b"still-alive"))
        );

        drop(pty_output);
        task.await.unwrap();
    }
}
