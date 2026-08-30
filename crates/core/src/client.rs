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

use std::net::IpAddr;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use termblog_proto as proto;
use tokio::sync::{broadcast, mpsc};

use crate::handle::{Control, SessionHandle};
use crate::link::Link;

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
    pub async fn open(
        &self,
        peer: IpAddr,
        cols: u16,
        rows: u16,
        attach_token: Option<String>,
    ) -> Result<SessionHandle> {
        let link = Link::connect(&self.socket)
            .await
            .with_context(|| format!("连接 jaild {}", self.socket.display()))?;

        let open = proto::Open { cols, rows, attach_token, fresh: false, peer_ip: Some(peer.to_string()) };
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
        let (out_tx, out_rx) = broadcast::channel(256);

        tokio::spawn(pump(link, input_rx, ctrl_rx, out_tx));

        Ok(SessionHandle { id: session_id, input: input_tx, output: out_rx, control: ctrl_tx })
    }
}

/// 转发泵: 键入/Resize -> 帧写 socket; socket 帧 -> 输出广播。
/// 任何一端断开(接入层 drop 通道 / socket EOF / jaild 发 Closed) => 整体拆解:
/// out_tx 被 drop => 接入层的 output 收到 Closed, 得知会话终结。
async fn pump(
    link: Link,
    mut input: mpsc::Receiver<Bytes>,
    mut ctrl: mpsc::Receiver<Control>,
    out: broadcast::Sender<Bytes>,
) {
    loop {
        tokio::select! {
            data = input.recv() => match data {
                Some(b) => {
                    if link.send(&proto::Frame::data(b)).await.is_err() { break; }
                }
                None => break, // 接入层断开(SessionHandle 被 drop)
            },
            c = ctrl.recv() => match c {
                Some(Control::Resize { cols, rows }) => {
                    let f = proto::Frame::json(proto::RESIZE, &proto::Resize { cols, rows });
                    if link.send(&f).await.is_err() { break; }
                }
                None => break,
            },
            r = link.recv() => match r {
                Ok(f) => match f.kind {
                    proto::DATA => { let _ = out.send(f.payload); }
                    proto::CLOSED => break, // jaild 宣布会话终结
                    _ => {}
                },
                Err(_) => break, // EOF / 协议错误: jaild 已退出或拒绝
            },
        }
    }
    // 本 task 结束 => out(broadcast::Sender) 被 drop => 接入层收到 Closed
}
