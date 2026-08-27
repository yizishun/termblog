//! termblog-ssh [降权]: 独立 SSH 接入层守护进程(与 web 进程完全分离)。
//!
//! 每个 ssh 连接开自己的独立会话, 与 web 会话互不共享; 会话一律经
//! SessionClient 走 Unix socket 连 jaild, 配额单一事实来源在 jaild。
//!
//! 环境变量(覆盖配置文件):
//!   TERMBLOG_CONFIG        配置文件路径, 默认 /usr/local/etc/termblog.toml
//!   TERMBLOG_SSH_LISTEN    监听地址, 默认 0.0.0.0:2222(降权 www 跑不了 22)
//!   TERMBLOG_SSH_HOST_KEY  host key 路径
//!   TERMBLOG_SSH_USER      允许免密登录的用户名, 默认 blog
//!   TERMBLOG_SOCKET        jaild socket 路径

use std::path::Path;
use std::process::ExitCode;

use anyhow::Context;
use termblog_core::{Config, SessionClient};

#[tokio::main]
async fn main() -> ExitCode {
    // 网关日志: 会话创建/拒绝/连接异常都落日志(排障用)
    tracing_subscriber::fmt().init();

    let cfg = match load_cfg() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("termblog-ssh 配置加载失败: {e:#}");
            return ExitCode::FAILURE;
        }
    };

    let listen = match cfg.ssh.listen.parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("ssh.listen {} 不是合法的 host:port: {e}", cfg.ssh.listen);
            return ExitCode::FAILURE;
        }
    };
    let ssh_cfg = termblog_ssh::SshConfig {
        listen,
        user: cfg.ssh.user.clone(),
        host_key: cfg.ssh.host_key.clone(),
    };
    let client = SessionClient::new(&cfg.jail.socket);

    match termblog_ssh::run(client, ssh_cfg).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // 独立守护进程: 绑不上(如无权限绑 22)就报错退出, 不吞错误
            eprintln!("termblog-ssh 启动失败({listen}): {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn load_cfg() -> anyhow::Result<Config> {
    let cfg_path = std::env::var("TERMBLOG_CONFIG").ok();
    let mut cfg = Config::load(cfg_path.as_deref().map(Path::new)).context("加载配置")?;
    if let Ok(v) = std::env::var("TERMBLOG_SSH_LISTEN") {
        cfg.ssh.listen = v;
    }
    if let Ok(v) = std::env::var("TERMBLOG_SSH_HOST_KEY") {
        cfg.ssh.host_key = v.into();
    }
    if let Ok(v) = std::env::var("TERMBLOG_SSH_USER") {
        cfg.ssh.user = v;
    }
    if let Ok(v) = std::env::var("TERMBLOG_SOCKET") {
        cfg.jail.socket = v.into();
    }
    Ok(cfg)
}
