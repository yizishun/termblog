//! web 侧会话持有: 「浏览器刷新不丢会话」的全部逻辑。
//!
//! 分层理由: WS 连接很脆(切标签、刷新、网络抖动), 会话(真实 jail + shell)
//! 却贵, 所以接入层替浏览器在断开后继续捧着会话一段时间。这份「捧」的逻辑
//! 与「一条 WS 连接怎么收发帧」无关, 单独放这个模块:
//!
//!   - `WebSession`: 持有表条目(句柄通道端 + 转发 task 命令通道 + 连接计数 + 宽限定时器)
//!   - `attach`: 凭 token 接回(刷新路径), 取消宽限、计数 +1、订阅 + scrollback 回放
//!   - `create`: `client.open` 开新会话 → 起转发 task(scrollback 缓冲 + attach 回放) → 登记
//!   - `detach`: 计数 -1, 归零起 60s 宽限定时器, 到点移除条目(通道 drop → jaild 回收 shell)
//!
//! jaild 侧没有持有表(socket 断开即回收会话), 刷新重连的宽限与回放
//! 只在本模块; web 发给前端的 token 由本进程生成, 只在本进程内存里有效。

use std::collections::HashMap;
use std::io::Read as _;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use termblog_core::{Control, SessionClient};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

/// 断开后 web 替浏览器持有会话的宽限期
const IDLE_GRACE: Duration = Duration::from_secs(60);
/// 每会话输出广播容量(条数): 慢消费者丢帧, 由 xterm.js 重绘
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

/// web 侧持有的会话: 句柄拆开的通道端 + 转发 task 的命令通道 + 连接计数
struct WebSession {
    sid: String,
    input: mpsc::Sender<Bytes>,
    control: mpsc::Sender<Control>,
    /// 发给转发 task 的命令(attach 时请求「订阅 + 回放」)
    fwd_cmd: mpsc::Sender<FwdCmd>,
    /// 当前活跃 WS 连接数; 归零才启动宽限定时器
    connections: usize,
    /// 无连接时的宽限倒计时; 新连接 attach 时取消
    idle_timer: Option<JoinHandle<()>>,
}

/// 一条 WS 连接拿到的会话视图(发送端是 clone, 输出是本次连接的订阅)
pub struct Connection {
    pub sid: String,
    pub token: String,
    pub attached: bool,
    pub input: mpsc::Sender<Bytes>,
    pub control: mpsc::Sender<Control>,
    pub output: broadcast::Receiver<Bytes>,
}

/// 会话持有者: main.rs 只经它开会话/接回/交还, 不碰持有表细节。
#[derive(Clone)]
pub struct SessionStore {
    client: SessionClient,
    table: Arc<Mutex<HashMap<String, WebSession>>>, // attach_token -> 会话
}

impl SessionStore {
    pub fn new(client: SessionClient) -> Self {
        Self { client, table: Arc::new(Mutex::new(HashMap::new())) }
    }

    /// 凭 token 接回 web 持有的会话: 取消宽限定时器, 连接计数 +1,
    /// 并让转发 task 现做「订阅 + scrollback 回放」, 拿到一个从历史输出开始的订阅。
    /// 返回 None = token 无效 / 会话已终结, 调用方转为开新会话。
    pub async fn attach(&self, token: &str) -> Option<Connection> {
        let (sid, input, control, fwd_cmd) = {
            let mut table = self.table.lock().unwrap();
            let s = table.get_mut(token)?;
            if let Some(t) = s.idle_timer.take() {
                t.abort();
            }
            s.connections += 1;
            (s.sid.clone(), s.input.clone(), s.control.clone(), s.fwd_cmd.clone())
        };

        // 若发送失败: 转发 task 已退出, 而它退出前必先移除持有表条目
        // (见转发 task 的清理), 所以这里无需回滚计数, 直接回落开新会话即可
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

    /// 开新会话: jaild 起 jail shell -> 输出转发 task -> 登记进持有表。
    /// attach_token 仍随 Open 透传(协议字段兼容), jaild 已不再使用。
    pub async fn create(
        &self,
        peer: IpAddr,
        cols: u16,
        rows: u16,
        attach_token: Option<String>,
    ) -> Result<Connection> {
        let session = self.client.open(peer, cols, rows, attach_token).await?; // 配额超限在此被拒

        let token = gen_token();
        let (out_tx, out_rx) = broadcast::channel(OUTPUT_CHUNKS);
        let (cmd_tx, mut cmd_rx) = mpsc::channel(8);

        // 转发 task: 会话输出 -> web 侧广播 + scrollback 缓冲; 同时处理 attach
        // 命令(现做订阅 + 回放)。会话终结(shell 退出/被回收)时广播关闭,
        // 顺势把会话移出持有表(发送端一并 drop, 幂等)
        {
            let table = self.table.clone();
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
                            // 不可达: 持有表条目一直握着 cmd_tx, 直到本 task 清理时才移除
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
                table.lock().unwrap().remove(&token);
            });
        }

        self.table.lock().unwrap().insert(token.clone(), WebSession {
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
    /// (发送端被 drop => jaild 侧 socket 断开 => 会话立即回收)
    pub fn detach(&self, token: &str) {
        let mut table = self.table.lock().unwrap();
        let Some(s) = table.get_mut(token) else { return }; // 会话已终结(转发 task 先移除了)
        s.connections -= 1;
        if s.connections > 0 {
            return; // 同 token 还有别的活跃连接(复制标签页等), 不启动倒计时
        }
        let table = self.table.clone();
        let token = token.to_string();
        s.idle_timer = Some(tokio::spawn(async move {
            tokio::time::sleep(IDLE_GRACE).await;
            table.lock().unwrap().remove(&token);
        }));
    }
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
