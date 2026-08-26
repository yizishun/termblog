//! termblog-ssh [降权]: russh 实现的 SSH 接入层, 独立二进制, 与 web 进程完全分离。
//!
//! 每个 ssh 连接通过 SessionManager::create 开自己的独立会话(PTY + zsh),
//! 与 web 会话互不共享; 两边共享的只是 termblog-core 里的会话核心代码
//! (PTY 泵 / 配额 / 回收逻辑)。接入层各自持有自己的 SessionManager,
//! 互不知晓彼此的存在。
//!
//! 只做 SSH <-> core 的最小翻译:
//!   pty_request          -> 记 winsize
//!   shell_request        -> SessionManager::create + 起下行泵
//!   data                 -> 键入进 PTY
//!   window_change        -> Control::Resize
//!   channel eof/close    -> drop 句柄, 触发 core 侧回收
//!
//! 安全面: 免密但只放行约定用户名(默认 blog); 只实现 shell 会话最小子集,
//! exec / subsystem / port-forward / agent-forward 一律拒绝(靠 russh 默认拒绝,
//! exec/subsystem 显式回 failure 让客户端立刻得到反馈)。

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use bytes::Bytes;
use russh::keys::{Algorithm, PrivateKey};
use russh::server::{Auth, ChannelOpenHandle, Config, Handler, Msg, Server, Session};
use russh::{Channel, ChannelId, Pty};
use termblog_core::{Control, SessionManager};
use tokio::sync::{broadcast, mpsc};

/// SSH 接入层配置
pub struct SshConfig {
    /// 监听地址(默认 0.0.0.0:22, 特权端口需要权限)
    pub listen: SocketAddr,
    /// 允许免密登录的用户名
    pub user: String,
    /// host key 持久化路径, 首次启动自动生成 ed25519
    pub host_key: PathBuf,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], 22)),
            user: "blog".into(),
            host_key: PathBuf::from("ssh_host_ed25519"),
        }
    }
}

/// 拉起 SSH 接入层。mgr 由调用方(本 crate 的 bin)构造, 与 web 进程各自独立。
/// 返回的 Future 即 accept 循环, 调用方 await 即可。
pub async fn run(mgr: SessionManager, cfg: SshConfig) -> Result<()> {
    let key = load_or_create_host_key(&cfg.host_key)?;
    let config = Arc::new(Config {
        keys: vec![key],
        ..Default::default()
    });
    let listener = tokio::net::TcpListener::bind(cfg.listen)
        .await
        .with_context(|| format!("ssh bind {}", cfg.listen))?;
    let user = cfg.user.clone();
    let mut server = SshServer { mgr, user };
    // RunningServer 本身是 Future(accept 循环), 直接 await 到关闭
    server.run_on_socket(config, &listener).await?;
    Ok(())
}

struct SshServer {
    mgr: SessionManager,
    user: String,
}

impl Server for SshServer {
    type Handler = SshHandler;

    fn new_client(&mut self, peer: Option<SocketAddr>) -> SshHandler {
        SshHandler {
            mgr: self.mgr.clone(),
            user: self.user.clone(),
            peer: peer
                .map(|a| a.ip())
                .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
            winsize: (80, 24), // pty_request 之前的兜底
            active: None,
        }
    }
}

/// 开会话后保留在手里的部分(output 已移给下行泵 task)
struct Active {
    input: mpsc::Sender<Bytes>,
    control: mpsc::Sender<Control>,
    /// 下行泵的句柄, 仅持有保活(drop 时 detach, 泵随 broadcast::Closed 自然结束)
    _pump: tokio::task::JoinHandle<()>,
}

struct SshHandler {
    mgr: SessionManager,
    user: String,
    peer: IpAddr,
    winsize: (u16, u16),
    active: Option<Active>,
}

impl SshHandler {
    fn check_user(&self, user: &str) -> Auth {
        if user == self.user {
            Auth::Accept
        } else {
            Auth::reject()
        }
    }

    /// 断开接入 => drop 通道端点, core 泵发现 input 关闭后回收会话。
    /// 注意不 abort 下行泵: 让它自然走到 broadcast::Closed, 由它给客户端
    /// 补发 exit_status + close, 否则客户端(如 stdin 提前 EOF 的 ssh)会干等。
    fn drop_session(&mut self) {
        let _ = self.active.take();
    }
}

impl Handler for SshHandler {
    type Error = anyhow::Error;

    // 免密: OpenSSH 客户端总是先发 "none" 探测可用方法, 直接接受即无感直入;
    // 少数不发 none 的客户端走 password, 同样接受(任意密码)。
    async fn auth_none(&mut self, user: &str) -> Result<Auth> {
        Ok(self.check_user(user))
    }

    async fn auth_password(&mut self, user: &str, _password: &str) -> Result<Auth> {
        Ok(self.check_user(user))
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<()> {
        reply.accept().await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<()> {
        self.winsize = clamp_winsize(col_width, row_height);
        session.channel_success(channel)?;
        Ok(())
    }

    async fn shell_request(&mut self, channel: ChannelId, session: &mut Session) -> Result<()> {
        let (cols, rows) = self.winsize;
        match self.mgr.create(self.peer, cols, rows).await {
            Ok(s) => {
                let termblog_core::SessionHandle {
                    input,
                    mut output,
                    control,
                    ..
                } = s;
                let handle = session.handle();
                // 下行泵: PTY 输出 -> channel data; 会话终结(shell 退出/被回收)
                // -> exit_status + close, 客户端表现与正常退出一致
                let pump = tokio::spawn(async move {
                    loop {
                        match output.recv().await {
                            Ok(bytes) => {
                                if handle.data(channel, bytes).await.is_err() {
                                    break; // channel 已被客户端关闭
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => continue, // 慢消费者丢帧
                            Err(broadcast::error::RecvError::Closed) => {
                                let _ = handle.exit_status_request(channel, 0).await;
                                let _ = handle.close(channel).await;
                                break;
                            }
                        }
                    }
                });
                self.active = Some(Active {
                    input,
                    control,
                    _pump: pump,
                });
                session.channel_success(channel)?;
            }
            Err(e) => {
                // 配额超限等: 告知原因, 拒绝 shell 请求(单一事实来源在 core)
                let handle = session.handle();
                let msg = format!("\x1b[31m[无法创建会话: {e}]\x1b[0m\r\n");
                let _ = handle.data(channel, Bytes::from(msg)).await;
                session.channel_failure(channel)?;
            }
        }
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        _channel: ChannelId,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _session: &mut Session,
    ) -> Result<()> {
        let (cols, rows) = clamp_winsize(col_width, row_height);
        self.winsize = (cols, rows);
        if let Some(a) = &self.active {
            let _ = a.control.send(Control::Resize { cols, rows }).await;
        }
        Ok(())
    }

    async fn data(
        &mut self,
        _channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<()> {
        if let Some(a) = &self.active {
            if a.input.send(Bytes::copy_from_slice(data)).await.is_err() {
                self.drop_session(); // 会话已死
            }
        }
        Ok(())
    }

    async fn channel_eof(&mut self, _channel: ChannelId, _session: &mut Session) -> Result<()> {
        self.drop_session();
        Ok(())
    }

    async fn channel_close(&mut self, _channel: ChannelId, _session: &mut Session) -> Result<()> {
        self.drop_session();
        Ok(())
    }

    // 禁 exec/subsystem: 显式回 failure, 客户端立刻收到反馈而非干等
    async fn exec_request(
        &mut self,
        channel: ChannelId,
        _data: &[u8],
        session: &mut Session,
    ) -> Result<()> {
        session.channel_failure(channel)?;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        _name: &str,
        session: &mut Session,
    ) -> Result<()> {
        session.channel_failure(channel)?;
        Ok(())
    }
}

fn clamp_winsize(cols: u32, rows: u32) -> (u16, u16) {
    (
        cols.clamp(1, u16::MAX as u32) as u16,
        rows.clamp(1, u16::MAX as u32) as u16,
    )
}

/// host key 持久化: 已存在则加载, 否则生成 ed25519 并写入(0600)。
/// 不持久化的话客户端每次连接都会报 host key 变更。
fn load_or_create_host_key(path: &Path) -> Result<PrivateKey> {
    if path.exists() {
        return russh::keys::load_secret_key(path, None)
            .with_context(|| format!("load host key {}", path.display()));
    }
    let key =
        PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).context("generate host key")?;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let openssh = key
        .to_openssh(russh::keys::ssh_key::LineEnding::LF)
        .context("encode host key")?;
    std::fs::write(path, openssh.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(key)
}
