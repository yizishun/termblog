//! termblog-ssh [降权]: 独立 SSH 接入层守护进程(与 web 进程完全分离)。
//!
//! 每个 ssh 连接开自己的独立会话, 与 web 会话互不共享; 共享的只有
//! termblog-core 里的会话核心代码。M3 之后两边的 SessionManager 都换成
//! 走 Unix socket 的 SessionClient, 配额单一事实来源统一到 jaild。
//!
//! 环境变量:
//!   TERMBLOG_SSH_LISTEN    监听地址, 默认 0.0.0.0:22(特权端口需要权限)
//!   TERMBLOG_SSH_HOST_KEY  host key 路径, 默认 ./ssh_host_ed25519
//!   TERMBLOG_SSH_USER      允许免密登录的用户名, 默认 blog

use std::process::ExitCode;

use termblog_core::{LocalBackend, Quota, SessionManager};

#[tokio::main]
async fn main() -> ExitCode {
    let mgr = SessionManager::new(
        LocalBackend,
        Quota {
            max_total: 64,
            max_per_ip: 3,
        },
    );

    let mut cfg = termblog_ssh::SshConfig::default();
    if let Ok(v) = std::env::var("TERMBLOG_SSH_LISTEN") {
        cfg.listen = match v.parse() {
            Ok(a) => a,
            Err(e) => {
                eprintln!("TERMBLOG_SSH_LISTEN 不是合法的 host:port: {e}");
                return ExitCode::FAILURE;
            }
        };
    }
    if let Ok(v) = std::env::var("TERMBLOG_SSH_HOST_KEY") {
        cfg.host_key = v.into();
    }
    if let Ok(v) = std::env::var("TERMBLOG_SSH_USER") {
        cfg.user = v;
    }

    let listen = cfg.listen;
    match termblog_ssh::run(mgr, cfg).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // 独立守护进程: 绑不上(如无权限绑 22)就报错退出, 不吞错误
            eprintln!("termblog-ssh 启动失败({listen}): {e:#}");
            ExitCode::FAILURE
        }
    }
}
