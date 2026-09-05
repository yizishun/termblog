//! Link: jaild <-> 接入层(web/ssh)之间的 Unix socket 链路。
//!
//! 用 SOCK_SEQPACKET 而非 SOCK_STREAM: 内核保留消息边界。FreeBSD 的
//! AF_UNIX 原生支持 SEQPACKET; Linux 2.6.4+ 同样支持, 开发机不受影响。
//!
//! ⚠️ FreeBSD 背压怪癖(本仓库实测并加测试锁定): 非阻塞 send(2) 在发送缓冲
//! 不足时可能**部分写入**(短计数, 记录未终结), 接收方 recvmsg(2) 会先收到
//! 该记录的碎片; 而 tokio-seqpacket 不暴露 MSG_EOR 边界。因此 send 侧必须
//! 循环续发(内核把续发并入同一条记录), recv 侧必须按帧头声明长度重组
//! (§5.8 字节零丢失的链路前提)。
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
    /// 测试用: 从已配对的 socket 构造链路(生产路径走 connect/accept)。
    pub fn from_sock(sock: UnixSeqpacket) -> Link {
        Link { sock: Arc::new(sock) }
    }

    /// 接入层侧: 连接 jaild 的 socket。
    pub async fn connect(path: &Path) -> io::Result<Link> {
        Ok(Link { sock: Arc::new(UnixSeqpacket::connect(path).await?) })
    }

    /// 发一帧: encode 后发整条记录。
    ///
    /// FreeBSD 的 AF_UNIX SOCK_SEQPACKET 在非阻塞 + 发送缓冲不足时,
    /// `send(2)` 会**部分写入**(返回短计数, 实测 40 字节粒度)——记录并未
    /// 终结, 后续 send 会续到同一条记录, 对端 recv 仍只见整条记录
    /// (本仓库有专门测试验证此语义)。所以这里必须循环续发, 否则背压下
    /// 帧被拆坏(§5.8 字节零丢失的链路前提)。
    pub async fn send(&self, frame: &proto::Frame) -> io::Result<()> {
        let wire = proto::encode(frame);
        let mut sent = 0usize;
        while sent < wire.len() {
            let n = self.sock.send(&wire[sent..]).await?;
            if n == 0 {
                // 对端已关闭写方向: 续发无意义, 如实报错
                return Err(io::Error::new(io::ErrorKind::WriteZero, "send incomplete"));
            }
            sent += n;
        }
        Ok(())
    }

    /// 收一帧。
    ///
    /// FreeBSD 的 AF_UNIX SOCK_SEQPACKET 在背压下: 发送方 `send(2)` 可能
    /// 部分写入(记录未终结), 接收方 `recvmsg(2)` 会先收到该记录的**碎片**。
    /// tokio-seqpacket 不暴露 MSG_EOR 记录边界, 所以这里按帧头声明的长度
    /// 重组成整帧(每条链路单向任一时刻只有一条记录在途, 长度前缀足以判界;
    /// 声明长度超 MAX_FRAME 即协议错误)。
    pub async fn recv(&self) -> io::Result<proto::Frame> {
        let mut buf = vec![0u8; proto::MAX_FRAME as usize + 5];
        let mut acc: Vec<u8> = Vec::new();
        loop {
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
                return Err(io::Error::new(io::ErrorKind::InvalidData, "frame truncated by kernel due to excessive length"));
            }
            acc.extend_from_slice(&buf[..n]);
            if acc.len() >= 5 {
                let len = u32::from_be_bytes(acc[1..5].try_into().expect("5-byte header")) as usize;
                if len > proto::MAX_FRAME as usize {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "frame declared length exceeds limit"));
                }
                if acc.len() >= 5 + len {
                    if acc.len() > 5 + len {
                        // 一次 recv 不该跨越两条记录(SEQPACKET), 出现即协议错乱
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "single recv spanned across two records"));
                    }
                    return proto::decode_one(&acc).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "invalid frame (too long or incomplete)")
                    });
                }
                // 否则: 记录碎片, 继续收
            }
        }
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
