//! 接入层(web/ssh) 与会话后端(jaild)之间共享的唯一契约: 极简二进制帧。
//!
//! 线上格式: `u8 kind | u32 len(大端) | payload`
//! - `Data` 帧 payload 是裸 PTY 字节(ANSI/OSC 原样穿透, web 与 ssh 画面一致的根本保证)
//! - 其余帧 payload 是 JSON
//!
//! 两条传输各有天然的消息边界, 无需任何粘包处理:
//! - WS: 一个 binary message 恰好承载一个帧(axum 保证消息完整), decode_one 直接解
//! - Unix socket: SOCK_SEQPACKET 按消息收发, 一次 send = 一次 recv(见 core/link.rs)
//! 因此帧解码只有「整条消息 -> 帧」这一步, 流式 Decoder 已删除。

use bytes::{BufMut, Bytes, BytesMut};

pub const OPEN: u8 = 0x01; // -> Open    接入方请求开会话
pub const DATA: u8 = 0x02; // <-> 裸字节  键入 / PTY 输出
pub const RESIZE: u8 = 0x03; // -> Resize  窗口变化
pub const OPENED: u8 = 0x04; // <- Opened  会话已建立
pub const CLOSED: u8 = 0x05; // <- Closed  会话结束(含配额拒绝等原因)

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Open {
    pub cols: u16,
    pub rows: u16,
    /// 断线重连: 带上上次 Opened 下发的 token, 请求 attach 回原会话。
    /// 无此字段(或 token 已失效) => 开新会话。
    #[serde(default)]
    pub attach_token: Option<String>,
    /// true = 本连接要一个全新会话, 跳过 attach(镜像页用它: 每次落地都是
    /// 干净 shell, 无需判断旧 shell 状态)。若同时带了 attach_token, 网关
    /// 会在开新会话前回收该 token 的闲置旧会话(见 web 的 SessionStore::reset)。
    #[serde(default)]
    pub fresh: bool,
    /// 真实对端 IP, 由网关(web/ssh 接入层)填入, 客户端不可信。
    /// jaild 据此做每 IP 配额判定(单一事实来源)。
    #[serde(default)]
    pub peer_ip: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Opened {
    pub session_id: String,
    /// 本次会话的 attach token, 客户端存好(sessionStorage), 重连时凭它回原会话
    pub attach_token: String,
    /// true = 成功 attach 回旧会话(会先发一轮 scrollback 回放, 客户端应先 reset
    /// 屏幕再接收); false = 开了新会话(token 缺失/已过期)
    pub attached: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Resize {
    pub cols: u16,
    pub rows: u16,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Closed {
    pub reason: String,
}

pub struct Frame {
    pub kind: u8,
    pub payload: Bytes,
}

impl Frame {
    /// 构造一个 Data 帧(裸字节)
    pub fn data(b: impl Into<Bytes>) -> Self {
        Self { kind: DATA, payload: b.into() }
    }

    /// 构造一个 JSON 控制帧
    pub fn json(kind: u8, v: &impl serde::Serialize) -> Self {
        let payload = serde_json::to_vec(v).expect("frame json").into();
        Self { kind, payload }
    }

    /// 把 payload 解析为 JSON 控制帧
    pub fn parse<T: serde::de::DeserializeOwned>(&self) -> serde_json::Result<T> {
        serde_json::from_slice(&self.payload)
    }
}

pub fn encode(f: &Frame) -> Bytes {
    let mut b = BytesMut::with_capacity(5 + f.payload.len());
    b.put_u8(f.kind);
    b.put_u32(f.payload.len() as u32);
    b.extend_from_slice(&f.payload);
    b.freeze()
}

/// 解码一条完整的消息(WS binary message / socket 一次 recv 恰为一帧, 不多不少)。
/// 帧头声明的长度超过 MAX_FRAME、或与消息实际长度不符, 都视为协议错误返回 None。
pub fn decode_one(b: &[u8]) -> Option<Frame> {
    if b.len() < 5 {
        return None;
    }
    let len = u32::from_be_bytes(b[1..5].try_into().ok()?) as usize;
    if len > MAX_FRAME as usize || b.len() != 5 + len {
        return None;
    }
    Some(Frame { kind: b[0], payload: Bytes::copy_from_slice(&b[5..]) })
}

/// 单帧 payload 上限(1 MiB)。PTY 输出/键入远小于此; 超限视为协议错误, 断开。
pub const MAX_FRAME: u32 = 1 << 20;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let f = Frame::json(OPEN, &Open { cols: 80, rows: 24, attach_token: None, fresh: false, peer_ip: None });
        let got = decode_one(&encode(&f)).expect("frame");
        assert_eq!(got.kind, OPEN);
        let o: Open = got.parse().unwrap();
        assert_eq!(o.cols, 80);
    }

    #[test]
    fn data_roundtrip() {
        let f = Frame::data(vec![1u8, 2, 3, 4, 5]);
        let got = decode_one(&encode(&f)).expect("frame");
        assert_eq!(&got.payload[..], &[1, 2, 3, 4, 5]);
    }

    #[test]
    fn oversize_rejected() {
        let mut wire = BytesMut::new();
        wire.put_u8(DATA);
        wire.put_u32(MAX_FRAME + 1);
        assert!(decode_one(&wire).is_none());
    }

    #[test]
    fn truncated_rejected() {
        let f = Frame::data(vec![1u8, 2, 3]);
        let wire = encode(&f);
        assert!(decode_one(&wire[..wire.len() - 1]).is_none());
    }
}
