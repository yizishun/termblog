//! 会话句柄与控制消息: jaild 与接入层(web/ssh)共享的 API 类型。
//!
//! SessionHandle 与传输方式无关 —— jaild 的 SessionManager(见 crates/servers/jaild)
//! 创建会话后返回它, 接入层的 SessionClient 开会话后也返回同型句柄,
//! 因此两侧代码可以共用这一份类型定义。

use bytes::Bytes;
use tokio::sync::mpsc;

pub enum Control {
    Resize { cols: u16, rows: u16 },
}

/// 接入层拿到的句柄: 与传输方式(web or ssh)无关。
/// drop 掉它(input/control 通道关闭)即表示接入方断开, 会话被回收。
pub struct SessionHandle {
    pub id: String,
    pub input: mpsc::Sender<Bytes>,          // 键入 -> PTY master 写
    pub output: mpsc::Receiver<Bytes>,       // PTY master 读 -> 唯一消费者(单播)
    pub control: mpsc::Sender<Control>,      // Resize
}
