//! 全局配置(TOML)。web / ssh / jaild / commentd / statd 与 content-build 共用同一份配置文件。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// 默认配置路径(不存在时退回内置默认值, 方便开发)。
pub const DEFAULT_CONFIG: &str = "/usr/local/etc/termblog.toml";

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub web: WebConfig,
    pub ssh: SshConfig,
    pub session: SessionConfig,
    pub jail: JailConfig,
    pub comments: CommentsConfig,
    pub stats: StatsConfig,
}

impl Config {
    /// 加载配置: `Some(path)` 读指定文件; `None` 时读 DEFAULT_CONFIG,
    /// 文件不存在则全部用默认值。
    pub fn load(path: Option<&Path>) -> Result<Config> {
        let p = match path {
            Some(p) => p.to_path_buf(),
            None => PathBuf::from(DEFAULT_CONFIG),
        };
        let cfg = if p.exists() {
            let s = std::fs::read_to_string(&p)
                .with_context(|| format!("read config {}", p.display()))?;
            toml::from_str(&s).with_context(|| format!("parse config {}", p.display()))
        } else {
            Ok(Config::default())
        }?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        let c = &self.comments;
        if !c.public_socket.is_absolute()
            || !c.private_socket.is_absolute()
            || !c.data_dir.is_absolute()
            || !c.targets_file.is_absolute()
        {
            anyhow::bail!("comments socket, data_dir, and targets_file must be absolute paths");
        }
        if c.public_socket == c.private_socket {
            anyhow::bail!("comments public and private sockets cannot be identical");
        }
        if c.public_socket == self.jail.socket || c.private_socket == self.jail.socket {
            anyhow::bail!("comments socket cannot be identical to jail socket");
        }
        if c.session_drain_ms == 0 {
            anyhow::bail!("comments drain timeout must be greater than 0");
        }
        let s = &self.stats;
        if !s.socket.is_absolute() || !s.data_dir.is_absolute() {
            anyhow::bail!("stats socket and data_dir must be absolute paths");
        }
        if [
            &self.jail.socket,
            &self.comments.public_socket,
            &self.comments.private_socket,
        ]
        .contains(&&s.socket)
        {
            anyhow::bail!("stats socket cannot be identical to another service socket");
        }
        if s.data_dir == self.comments.data_dir {
            anyhow::bail!("stats and comments data directories cannot be identical");
        }
        if s.request_timeout_ms == 0 {
            anyhow::bail!("stats request timeout must be greater than 0");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct StatsConfig {
    /// Shared SEQPACKET socket used by jaild and the unprivileged Web process.
    pub socket: PathBuf,
    /// Root-only SQLite database and visitor-HMAC secret directory.
    pub data_dir: PathBuf,
    /// End-to-end timeout for a best-effort RecordBatch or Snapshot request.
    pub request_timeout_ms: u64,
}

impl Default for StatsConfig {
    fn default() -> Self {
        Self {
            socket: PathBuf::from("/var/run/termblog-statd.sock"),
            data_dir: PathBuf::from("/var/db/termblog-statd"),
            request_timeout_ms: 250,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CommentsConfig {
    pub public_socket: PathBuf,
    pub private_socket: PathBuf,
    pub data_dir: PathBuf,
    pub targets_file: PathBuf,
    pub session_drain_ms: u64,
}

impl Default for CommentsConfig {
    fn default() -> Self {
        Self {
            public_socket: PathBuf::from("/var/run/commentd-public.sock"),
            private_socket: PathBuf::from("/var/run/commentd-private.sock"),
            data_dir: PathBuf::from("/var/db/termblog-commentd"),
            targets_file: PathBuf::from("/usr/local/share/termblog/comment-targets.tsv"),
            session_drain_ms: 1000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WebConfig {
    /// axum 监听地址(生产直连 0.0.0.0:8080)
    pub listen: String,
    /// 前端静态资源目录(安装布局下为绝对路径)
    pub static_dir: String,
    /// 站点对外绝对地址(https://host[:port], 尾部不带 /)。
    /// canonical / sitemap / atom / OG 的前缀, 仅 content-build 消费。
    pub site_url: Option<String>,
    /// 站点标题: 镜像页 <title> / og:site_name / atom feed 标题, content-build 消费。
    pub site_title: String,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8080".into(),
            static_dir: "frontend/dist".into(),
            site_url: None,
            site_title: "~yzs".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SshConfig {
    /// russh 监听地址。降权 www 跑不了 22 等特权端口, 生产用 2222
    pub listen: String,
    /// 允许免密登录的用户名
    pub user: String,
    /// host key 持久化路径
    pub host_key: PathBuf,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:2222".into(),
            user: "blog".into(),
            host_key: PathBuf::from("/var/db/termblog/ssh_host_ed25519"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    pub max_total: usize,
    pub max_per_ip: usize,
    /// 硬寿命上限(M4 落地, 先占位)
    pub hard_lifetime_secs: u64,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            max_total: 64,
            max_per_ip: 3,
            hard_lifetime_secs: 7200,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct JailConfig {
    /// jaild 的 Unix socket: 接入层(web/ssh)连这里, jaild 绑这里。
    /// 这是降权网关与特权 jaild 之间的唯一通道。
    pub socket: PathBuf,
    /// 只读模板快照: clone 的源
    pub template: String,
    /// 会话数据集前缀: <prefix><sid>
    pub dataset_prefix: String,
    /// 每会话可写空间上限(ZFS quota; 模板共享块不计入)。
    /// 防访客写爆宿主 zroot 池。
    pub disk_quota: String,
    /// 会话 jail 挂载路径前缀: <prefix>/<sid>
    pub path_prefix: String,
    pub memory: String,
    pub vmemory: String,
    pub maxproc: u32,
    pub openfiles: u32,
    pub pcpu: u32,
    /// jail 内登录 shell 路径(模板里由 pkg 装到 /usr/local/bin)
    pub zsh: String,
    /// jail 内降权运行的用户
    pub guest_user: String,
}

impl Default for JailConfig {
    fn default() -> Self {
        Self {
            socket: PathBuf::from("/var/run/termblog.sock"),
            template: "zroot/jails/template@release".into(),
            dataset_prefix: "zroot/jails/s-".into(),
            disk_quota: "4M".into(),
            path_prefix: "/jails".into(),
            memory: "128M".into(),
            vmemory: "512M".into(),
            maxproc: 32,
            openfiles: 256,
            pcpu: 25,
            zsh: "/usr/local/bin/zsh".into(),
            guest_user: "guest".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_paths_and_timeout_are_validated() {
        let mut cfg = Config::default();
        cfg.stats.socket = "relative.sock".into();
        assert!(cfg.validate().is_err());

        let defaults = Config::default();
        for socket in [
            defaults.jail.socket,
            defaults.comments.public_socket,
            defaults.comments.private_socket,
        ] {
            let mut cfg = Config::default();
            cfg.stats.socket = socket;
            assert!(cfg.validate().is_err());
        }

        let mut cfg = Config::default();
        cfg.stats.data_dir = "relative-data".into();
        assert!(cfg.validate().is_err());

        let mut cfg = Config::default();
        cfg.stats.data_dir = cfg.comments.data_dir.clone();
        assert!(cfg.validate().is_err());

        let mut cfg = Config::default();
        cfg.stats.request_timeout_ms = 0;
        assert!(cfg.validate().is_err());
    }
}
