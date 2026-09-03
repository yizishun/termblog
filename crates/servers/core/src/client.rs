//! SessionClient: 走 Unix socket 的会话客户端(web/ssh 接入层使用)。
//!
//! 设计: **每条会话一条 socket 连接**(而非复用连接多路复用), 因此 proto 帧
//! 无需携带 session_id, 接入层拿到的东西与 jaild 内会话完全同构
//! (同型的 SessionHandle)。连接关闭 = 会话交还, jaild 立即回收会话
//! (jail 销毁, 无宽限期; 刷新重连是 web 层自己的逻辑)。
//!
//! 注意: peer_ip 由本客户端填入(接入层从 TCP 握手拿到的真实对端 IP),
//! 客户端不可信, jaild 据此做配额判定的单一事实来源。
//!
//! 传输: SEQPACKET Unix socket(见 link.rs)。消息边界 = 帧边界,
//! 无粘包处理, 一次 send / 一次 recv 各对应一条完整帧。
//!
//! 输出通道: 有界 mpsc(单消费者)。终端字节流是有状态协议(ANSI/IIP 无帧界),
//! 任意丢一块都会破坏后续序列——所以从 jaild PTY 读缓冲到 web WS 的全链路
//! 都是「有界队列 + 背压」, 绝不静默丢帧: 队列满时 pump 停读/停收, 背压沿
//! 链路传导回子进程的 write(2)。

use std::net::IpAddr;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use termblog_proto as proto;
use tokio::sync::mpsc;

use crate::handle::{Control, SessionHandle};
use crate::link::Link;

/// 输出队列容量(条; PTY 读块 ≤ 8192B, ≈ 1 MiB)。队列只被背压瞬间填满,
/// 正常情况远小于此; 容量同时保证单条完整 IIP 序列(≤683 KiB + 头)不
/// 被队列容量截断。
const OUTPUT_CHUNKS: usize = 128;

#[derive(Clone)]
pub struct SessionClient {
    socket: PathBuf,
}

impl SessionClient {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self { socket: socket.into() }
    }

    /// 开一个会话: 连 socket -> 发 Open -> 读 Opened(成功) / Closed(配额拒绝等)
    /// -> 起转发泵。返回的 SessionHandle 与 jaild 内部会话同型。
    /// caps: 客户端能力通告(白名单字符串, 由 jaild 过滤; 空 = 无增强能力)。
    pub async fn open(
        &self,
        peer: IpAddr,
        cols: u16,
        rows: u16,
        attach_token: Option<String>,
        caps: Vec<String>,
    ) -> Result<SessionHandle> {
        let link = Link::connect(&self.socket)
            .await
            .with_context(|| format!("连接 jaild {}", self.socket.display()))?;
        Self::open_on(link, peer, cols, rows, attach_token, caps).await
    }

    /// 在已建立的链路上开会话(open 的实做; 测试用 pair() 注入链路)。
    async fn open_on(
        link: Link,
        peer: IpAddr,
        cols: u16,
        rows: u16,
        attach_token: Option<String>,
        caps: Vec<String>,
    ) -> Result<SessionHandle> {
        let open = proto::Open {
            cols,
            rows,
            attach_token,
            fresh: false,
            peer_ip: Some(peer.to_string()),
            caps,
        };
        link.send(&proto::Frame::json(proto::OPEN, &open)).await.context("发送 Open")?;

        // 第一条回帧: Opened(成功) 或 Closed(配额拒绝等原因)
        let first = link.recv().await.context("读取 Opened")?;
        let session_id = match first.kind {
            proto::OPENED => first.parse::<proto::Opened>()?.session_id,
            proto::CLOSED => bail!("{}", first.parse::<proto::Closed>()?.reason),
            other => bail!("意外的首帧 0x{other:02x}"),
        };

        let (input_tx, input_rx) = mpsc::channel(64);
        let (ctrl_tx, ctrl_rx) = mpsc::channel(8);
        let (out_tx, out_rx) = mpsc::channel(OUTPUT_CHUNKS);

        tokio::spawn(pump(link, input_rx, ctrl_rx, out_tx));

        Ok(SessionHandle { id: session_id, input: input_tx, output: out_rx, control: ctrl_tx })
    }
}

/// 转发泵: 键入/Resize -> 帧写 socket; socket 帧 -> 输出队列(背压)。
/// 任何一端断开(接入层 drop 通道 / socket EOF / jaild 发 Closed) => 整体拆解:
/// out_tx 被 drop => 接入层的 output 收到 None, 得知会话终结。
async fn pump(
    link: Link,
    mut input: mpsc::Receiver<Bytes>,
    mut ctrl: mpsc::Receiver<Control>,
    out: mpsc::Sender<Bytes>,
) {
    #[allow(unused_assignments)]
    let why;
    loop {
        tokio::select! {
            data = input.recv() => match data {
                Some(b) => {
                    if link.send(&proto::Frame::data(b)).await.is_err() { why = "send 输入"; break; }
                }
                None => { why = "input 关闭"; break; } // 接入层断开(SessionHandle 被 drop)
            },
            c = ctrl.recv() => match c {
                Some(Control::Resize { cols, rows }) => {
                    let f = proto::Frame::json(proto::RESIZE, &proto::Resize { cols, rows });
                    if link.send(&f).await.is_err() { why = "send resize"; break; }
                }
                None => { why = "ctrl 关闭"; break; }
            },
            r = link.recv() => match r {
                Ok(f) => match f.kind {
                    proto::DATA => {
                        // 队列满 = 下游消费不及: 停收 socket 即背压 ——
                        // socket 不读, jaild 侧的写最终阻塞, 一路传导到
                        // 子进程 write(2)。绝不丢字节。
                        if out.send(f.payload).await.is_err() {
                            why = "输出通道关闭";
                            break; // 接入层断开
                        }
                    }
                    proto::CLOSED => { why = "jaild Closed"; break; } // jaild 宣布会话终结
                    _ => {}
                },
                Err(_) => { why = "链路 EOF"; break; } // EOF / 协议错误: jaild 已退出或拒绝
            },
        }
    }
    eprintln!("DEBUG pump exit: {why}");
    // 本 task 结束 => out(mpsc::Sender) 被 drop => 接入层收到 None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_seqpacket::UnixSeqpacket;

    /// §5.8 背压链路: 输出队列满时 pump 停读而非丢字节。压入远超队列容量
    /// (≈1 MiB)的伪随机字节流, 收端逐字节比对 —— 任何一处丢帧(Lagged)
    /// 都会现形; mpsc + 背压下必须逐字节相等。
    #[tokio::test]
    async fn output_bytes_not_lost_under_backpressure() {
        let (a, b) = UnixSeqpacket::pair().unwrap();
        let client_link = Link::from_sock(a);
        let server_link = Link::from_sock(b);

        const CHUNKS: usize = 300;
        const CHUNK: usize = 8192;
        let pattern: Vec<u8> =
            (0..CHUNKS * CHUNK).map(|i| ((i * 31 + i / 7) & 0xff) as u8).collect();

        let server_pattern = pattern.clone();
        let server = tokio::spawn(async move {
            let f = server_link.recv().await.unwrap();
            assert_eq!(f.kind, proto::OPEN, "首帧必须是 Open");
            server_link
                .send(&proto::Frame::json(
                    proto::OPENED,
                    &proto::Opened {
                        session_id: "t".into(),
                        attach_token: String::new(),
                        attached: false,
                    },
                ))
                .await
                .unwrap();
            for chunk in server_pattern.chunks(CHUNK) {
                // 客户端队列满时本 send 阻塞 = 端到端背压, 无丢字节
                server_link.send(&proto::Frame::data(chunk.to_vec())).await.unwrap();
            }
            // 发完即断(socket drop = EOF), 客户端以 output None 收尾
        });

        let peer = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
        let mut h = SessionClient::open_on(client_link, peer, 80, 24, None, vec![])
            .await
            .unwrap();
        let mut got: Vec<u8> = Vec::new();
        while let Some(b) = h.output.recv().await {
            got.extend_from_slice(&b);
        }
        server.await.unwrap();
        assert_eq!(got.len(), pattern.len(), "字节流总长应一致(丢帧现形)");
        assert_eq!(got, pattern, "字节流应逐字节相等(丢帧现形)");
    }
}
