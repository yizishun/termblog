//! termblog-core: 协议运行时 + 接入层共享库。
//!
//! 本 crate 只放「两侧都要用」的代码:
//!   - `proto`(独立 crate): 帧格式(encode/decode)
//!   - `link`: SEQPACKET Unix socket 传输(jaild <-> web/ssh)
//!   - `handle`: 会话句柄与控制消息(两侧同型)
//!   - `client`: SessionClient, 接入层(web/ssh)开会话的客户端
//!   - `config`: 三个二进制共用的 TOML 配置
//!
//! jaild 专属的特权代码(JailBackend / PTY / SessionManager + 配额)不在
//! 本 crate, 而在 crates/jaild; jaild 也依赖本 crate 的 link/config/handle。

pub mod client;
pub mod config;
pub mod handle;
pub mod link;

pub use client::SessionClient;
pub use config::{Config, JailConfig, SessionConfig, SshConfig, WebConfig};
pub use handle::{Control, SessionHandle};
pub use link::{Link, LinkListener};
