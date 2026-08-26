//! 接入层(web/ssh) 与会话后端之间共享的唯一契约: 极简二进制帧。
//!
//! 线上格式: `u8 kind | u32 len(大端) | payload`
//! - `Data` 帧 payload 是裸 PTY 字节(ANSI/OSC 原样穿透, web 与 ssh 画面一致的根本保证)
//! - 其余帧 payload 是 JSON
//!
//! WS 约定: 一个 binary message 恰好承载一个帧, 所以网关是纯透传、零协议转换。
//! (M3 接 Unix socket 字节流时, 在这里再加一个处理粘包的流式 Decoder 即可)

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
    // 注意: peer_ip 不放在这里(客户端不可信)。M3 拆进程后由网关在发往 jaild 的
    // Open 里自行填入真实对端 IP, 配额判定才有意义。
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

/// 解码一条完整的消息(WS binary message 恰为一帧, 不多不少)
pub fn decode_one(b: &[u8]) -> Option<Frame> {
    if b.len() < 5 {
        return None;
    }
    let len = u32::from_be_bytes(b[1..5].try_into().ok()?) as usize;
    if b.len() != 5 + len {
        return None;
    }
    Some(Frame { kind: b[0], payload: Bytes::copy_from_slice(&b[5..]) })
}
