//! backend 抽象: 核心只关心「给我一个接在 PTY 从端上的 shell 进程」,
//! 对 shell 跑在哪(本机 / jail)完全无感。这是整个架构最关键的扩展缝。

use anyhow::Result;
use async_trait::async_trait;

use crate::pty::{spawn_shell, ShellChild};

#[async_trait]
pub trait ShellBackend: Send + Sync {
    async fn spawn(&self, sid: &str, cols: u16, rows: u16) -> Result<ShellChild>;

    /// 会话结束后的资源销毁(JailBackend: jail -r + zfs destroy; 本机无额外资源)
    async fn cleanup(&self, _sid: &str) -> Result<()> {
        Ok(())
    }
}

/// 开发模式: 直接在本机 forkpty 一个 zsh。无需 root / FreeBSD, macOS/Linux 可跑 M1。
/// 生产模式(M3)换成 JailBackend: zfs clone -> jail -c -> jail_attach -> zsh。
pub struct LocalBackend;

#[async_trait]
impl ShellBackend for LocalBackend {
    async fn spawn(&self, _sid: &str, cols: u16, rows: u16) -> Result<ShellChild> {
        spawn_shell(cols, rows)
    }
}
