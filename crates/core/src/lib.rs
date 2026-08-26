//! 会话核心(生产上跑在特权进程 jaild 内; 开发模式内嵌进 web 进程)。
//!
//! 三层: `pty`(fork 出 PTY 上的 shell) -> `backend`(shell 跑在哪的抽象)
//! -> `session`(会话表、配额、每会话一个读写泵 task)。

pub mod backend;
pub mod pty;
pub mod session;

pub use backend::{LocalBackend, ShellBackend};
pub use pty::ShellChild;
pub use session::{Control, Quota, SessionHandle, SessionManager};
