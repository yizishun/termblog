//! jaild [root]: 特权守护进程。整个系统唯一携带 root 的进程。
//!
//! 职责(见 plan §1/§8):
//!   - 监听 Unix socket `/var/run/termblog.sock`(0660 root:www, SEQPACKET)
//!   - 收 proto 帧, 调 SessionManager(JailBackend) 开会话/配额
//!   - 每连接一个 task: 上行帧 -> 键入/Resize, 下行输出 -> 帧写 socket
//!   - socket 断开 = 会话立即回收(jail -r + zfs destroy), 无宽限期;
//!     刷新重连是 web 层的职责(web 侧 60s 宽限 + scrollback 回放),
//!     jaild 不参与恢复机制
//!   - 启动时扫描回收上次崩溃遗留的 s-* 残留
//!
//! 接入方(web/ssh, 降权 www)只经 socket 触达这里; 会话(PTY + jail)的
//! 生命周期完全由本进程持有。

mod jail;
mod pty;
mod session;
mod watcher;

use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::fs::{chown, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use nix::unistd::Group;
use termblog_config::Config;
use termblog_core::link::{Link, LinkListener};
use termblog_core::Control;
use termblog_proto as proto;
use tracing::{error, info, warn};

use crate::jail::{ensure_devfs_ruleset, JailBackend};
use crate::session::{Quota, SessionManager};

/// 握手超时: 首帧必须是 Open
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// 能力白名单(图片二期): 已知值只有 img-iterm2(终端链路支持
/// iTerm2 Inline Images Protocol)。未知值丢弃。
///
/// 安全论证: caps 来自客户端、不可信, 但它只影响该访客自己会话的一个
/// 提示性环境变量(TERMBLOG_IMG=iterm2), 最坏后果是自己的终端收到图像
/// 字节; 无横向风险。白名单过滤是为了防止环境变量值被注入奇怪字符串
/// (jaild 把 cap 值原样写进 execve 的 envp)。
fn filter_caps(caps: &[String]) -> Vec<String> {
    const KNOWN: &[&str] = &["img-iterm2"];
    caps.iter()
        .filter(|c| {
            let known = KNOWN.contains(&c.as_str());
            if !known {
                warn!(cap = %c, "dropping unknown capability announcement (not in whitelist)");
            }
            known
        })
        .cloned()
        .collect()
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();

    let cfg_path = std::env::var("TERMBLOG_CONFIG").ok();
    let cfg = Config::load(cfg_path.as_deref().map(Path::new))?;

    // 先绑 socket 再做耗时启动步骤: 残留会话的 sweep(jail -r 杀进程 +
    // zfs destroy)可达 10s+(现场: 14:52:47 重启, 14:52:58 才就绪), 若
    // socket 晚于 sweep 才绑, 重启窗口里的连接会被直接拒绝。现在连接
    // 只在 backlog 里排队, accept 循环在所有启动步骤完成后才开始,
    // 排队的连接随后照常握手——sweep 期间没有任何新会话, 零竞态。
    let socket = cfg.jail.socket.clone();
    let _ = std::fs::remove_file(&socket);
    let mut listener =
        LinkListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;
    // 权限受控边界: 0660 root:www —— 只有降权网关可连。
    // chown 失败只告警(开发/非 root 场景), 生产 rc 以 root 启动自然得到 root:www
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o660))?;
    match Group::from_name("www") {
        Ok(Some(g)) => {
            if let Err(e) = chown(&socket, None, Some(g.gid.as_raw())) {
                warn!(%e, socket = %socket.display(), "chown root:www failed (not started as root?)");
            }
        }
        Ok(None) => warn!("system has no www group, socket keeping root group"),
        Err(e) => warn!(%e, "failed to query www group"),
    }
    info!(socket = %socket.display(), "socket listening (startup steps pending, connections will queue)");

    // 启动残留回收: 上次崩溃遗留的 s-* 全部销毁(幂等)
    JailBackend::sweep(&cfg.jail)?;
    // devfs 规则集 4(FreeBSD 14+ 不自带, 缺了 jail 里没有 /dev)
    ensure_devfs_ruleset();

    let mgr = SessionManager::new(
        JailBackend::new(cfg.jail.clone(), cfg.comments.clone(), cfg.stats.clone()),
        Quota {
            max_total: cfg.session.max_total,
            max_per_ip: cfg.session.max_per_ip,
        },
        Duration::from_secs(cfg.session.idle_timeout_secs),
    );
    info!(
        socket = %socket.display(),
        idle_timeout_secs = cfg.session.idle_timeout_secs,
        "jaild ready, starting accept"
    );

    loop {
        match listener.accept().await {
            Ok(link) => {
                tokio::spawn(handle_conn(mgr.clone(), link));
            }
            Err(e) => error!(%e, "accept failed"),
        }
    }
}

/// 一条连接 = 一个 task: 握手 Open -> 开会话(配额在 SessionManager 判定)
/// -> 收发循环。连接断开或会话终结 => 本 task 退出, SessionHandle 通道
/// 随之关闭 => SessionManager 泵立即回收 shell 并销毁 jail(没有宽限期)。
async fn handle_conn(mgr: SessionManager, link: Link) {
    // 1) 握手: 第一条帧必须是 Open(限时), 网关已填好真实 peer_ip。
    //    Open.attach_token 已废弃(刷新重连由 web 层自己处理), 收到即忽略。
    let open: proto::Open = match tokio::time::timeout(HANDSHAKE_TIMEOUT, link.recv()).await {
        Ok(Ok(f)) if f.kind == proto::OPEN => match f.parse() {
            Ok(o) => o,
            Err(e) => {
                warn!(%e, "Open frame JSON parse failed");
                return;
            }
        },
        Ok(Ok(f)) => {
            warn!(kind = f.kind, "first frame is not Open");
            return;
        }
        Ok(Err(e)) => {
            warn!(%e, "read Open frame failed");
            return;
        }
        Err(_) => {
            warn!("read Open frame timed out");
            return;
        }
    };
    let peer = open
        .peer_ip
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));

    // 2) 开会话: 配额超限等失败在此被拒(Closed 帧告知原因)。
    //    caps 经白名单过滤(只认已知能力值), 映射为该会话 jail 里的
    //    提示性环境变量 TERMBLOG_IMG(见 jail.rs)。
    let caps = filter_caps(&open.caps);
    let session = match mgr.create(peer, open.cols, open.rows, caps).await {
        Ok(s) => s,
        Err(e) => {
            let f = proto::Frame::json(
                proto::CLOSED,
                &proto::Closed {
                    reason: e.to_string(),
                },
            );
            let _ = link.send(&f).await;
            return;
        }
    };

    // 初始尺寸已由 Open 帧经 SessionManager::create 在 openpty 时生效,
    // 这里无需再推一遍(刷新接回时的新尺寸由 web 侧推送)。

    let f = proto::Frame::json(
        proto::OPENED,
        &proto::Opened {
            session_id: session.id.clone(),
            attach_token: String::new(), // 已废弃: 恢复机制只在 web 层
            attached: false,
        },
    );
    if link.send(&f).await.is_err() {
        return; // session 随本函数退出被 drop => 会话立即回收
    }

    // 3) 收发循环: 上行帧 -> 键入/Resize; 下行输出 -> 帧写 socket
    let mut output = session.output;
    loop {
        tokio::select! {
            r = link.recv() => match r {
                Ok(f) => match f.kind {
                    proto::DATA => {
                        if session.input.send(f.payload).await.is_err() {
                            break; // 会话已死(泵退出, 通道关闭)
                        }
                    }
                    proto::RESIZE => {
                        if let Ok(r) = f.parse::<proto::Resize>() {
                            let _ = session
                                .control
                                .send(Control::Resize { cols: r.cols, rows: r.rows })
                                .await;
                        }
                    }
                    _ => {}
                },
                Err(_) => break, // socket EOF / 协议错误: 接入层断开
            },
            msg = output.recv() => match msg {
                Some(bytes) => {
                    if link.send(&proto::Frame::data(bytes)).await.is_err() {
                        break; // 对端已断开
                    }
                }
                None => {
                    // shell 已退出/会话被回收: 告知接入层后收尾
                    let f = proto::Frame::json(proto::CLOSED, &proto::Closed { reason: "exit".into() });
                    let _ = link.send(&f).await;
                    break;
                }
            },
        }
    }
    tracing::info!(sid = %session.id, "connection closed, session returned (immediate reclamation)");
    // session(input/control/output) 随本函数结束被 drop => 泵回收 shell 并销毁 jail
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_whitelist_filter() {
        // 已知值保留, 未知值丢弃(顺序保留)
        let caps = vec![
            "img-iterm2".to_string(),
            "evil; rm -rf".to_string(),
            "IMG_ITERM2".to_string(),
        ];
        assert_eq!(filter_caps(&caps), vec!["img-iterm2".to_string()]);
        assert!(filter_caps(&[]).is_empty());
        assert!(filter_caps(&["img-sixel".to_string()]).is_empty());
    }
}
