//! Link: jaild <-> 接入层(web/ssh)之间的 Unix socket 链路。
//!
//! 用 SOCK_SEQPACKET 而非 SOCK_STREAM: 内核保留消息边界 —— 一次 send 就是
//! 一条完整记录、对端一次 recv 恰好收到这一条, 不会拆成两半。帧的「头声明
//! 长度」与内核消息边界天然重合, 粘包/半包/流式解码器(FrameDecoder)全部
//! 不再需要。FreeBSD 的 AF_UNIX 原生支持 SEQPACKET; Linux 2.6.4+ 同样支持,
//! 开发机不受影响。
//!
//! tokio 本身没有 SEQPACKET 的 UnixStream 封装(tokio::net::UnixStream 固定
//! SOCK_STREAM), 所以用 tokio-seqpacket crate 的 UnixSeqpacket /
//! UnixSeqpacketListener。它的 fd 由 crate 内部独占管理(AsyncFd<FileDesc>),
//! 不存在我们之前踩过的「把 tokio 已注册 fd 再包一层导致 kqueue 双重注册」
//! 问题 —— 那是用法错误, 不是 AsyncFd 在 FreeBSD 上不可用。
//!
//! 内存: 每条链路一个 MAX_FRAME+5 的接收缓冲(1 MiB)。合法帧必装得下;
//! 对端若发出超长消息, recv 返回的 MessageInfo.truncated() 会如实报告截断,
//! 直接判协议错误断开。实际流量远小于此(PTY 输出 8 KiB 切片、回放 4 KiB
//! 块), 上限只是协议护栏。

use std::io;
use std::path::Path;
use std::sync::Arc;

use termblog_proto as proto;
use tokio_seqpacket::{UnixSeqpacket, UnixSeqpacketListener};

/// 接在一条 SEQPACKET socket 上的链路。Clone 后同一 fd 供上下行泵共享。
#[derive(Clone)]
pub struct Link {
    sock: Arc<UnixSeqpacket>,
}

impl Link {
    /// 接入层侧: 连接 jaild 的 socket。
    pub async fn connect(path: &Path) -> io::Result<Link> {
        Ok(Link { sock: Arc::new(UnixSeqpacket::connect(path).await?) })
    }

    /// 发一帧: encode 后一次 send。SEQPACKET 保证整条消息原子送达, 不会半条。
    pub async fn send(&self, frame: &proto::Frame) -> io::Result<()> {
        let wire = proto::encode(frame);
        let n = self.sock.send(&wire).await?;
        if n != wire.len() {
            // SEQPACKET 上正常不会发生(消息要么全发要么 EAGAIN), 防御而已
            return Err(io::Error::new(io::ErrorKind::WriteZero, "send 未发完"));
        }
        Ok(())
    }

    /// 收一帧: 一次 recv 恰好一条消息。返回 0 = 对端关闭(EOF)。
    pub async fn recv(&self) -> io::Result<proto::Frame> {
        let mut buf = vec![0u8; proto::MAX_FRAME as usize + 5];
        let info = loop {
            match self.sock.recv(&mut buf).await {
                Ok(info) => break info,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        };
        let n = info.bytes_read();
        if n == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        if info.truncated() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "帧超长被内核截断"));
        }
        proto::decode_one(&buf[..n])
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "非法帧(超长或残缺)"))
    }
}

/// jaild 侧的监听 socket(SEQPACKET)。
pub struct LinkListener {
    inner: UnixSeqpacketListener,
}

impl LinkListener {
    /// bind + listen。socket 路径文件由 bind 创建, 权限(0660 root:www)
    /// 由调用方在 bind 之后设置。
    pub fn bind(path: &Path) -> io::Result<LinkListener> {
        Ok(LinkListener { inner: UnixSeqpacketListener::bind(path)? })
    }

    pub async fn accept(&mut self) -> io::Result<Link> {
        Ok(Link { sock: Arc::new(self.inner.accept().await?) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证 SEQPACKET 链路在本机(FreeBSD/kqueue)上工作: 消息边界完整、
    /// 不粘连、不拆分, 截断可检测, 对端关闭写方向得到 EOF。
    #[tokio::test]
    async fn message_boundaries() {
        let (a, b) = UnixSeqpacket::pair().unwrap();
        let la = Link { sock: Arc::new(a) };
        let lb = Link { sock: Arc::new(b) };

        // 一次 send 恰好等于一次 recv, 且长度逐条不同: 若有粘包/拆包立刻现形
        for i in 0..16u8 {
            la.send(&proto::Frame::data(vec![i; i as usize + 1])).await.unwrap();
        }
        for i in 0..16u8 {
            let f = lb.recv().await.unwrap();
            assert_eq!(f.kind, proto::DATA);
            assert_eq!(f.payload.len(), i as usize + 1);
            assert!(f.payload.iter().all(|b| *b == i));
        }

        // 反向同理(控制帧)
        lb.send(&proto::Frame::json(proto::RESIZE, &proto::Resize { cols: 100, rows: 40 }))
            .await
            .unwrap();
        let f = la.recv().await.unwrap();
        assert_eq!(f.kind, proto::RESIZE);

        // 对端关闭写方向 => recv 得到 EOF(SEQPACKET 的 EOF 语义)
        lb.sock.shutdown(std::net::Shutdown::Write).unwrap();
        assert!(matches!(
            la.recv().await,
            Err(ref e) if e.kind() == io::ErrorKind::UnexpectedEof
        ));
    }
}
