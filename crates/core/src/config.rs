//! 全局配置(TOML)。web / ssh / jaild 三个二进制共用同一份配置文件。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// 默认配置路径(不存在时退回内置默认值, 方便开发)。
pub const DEFAULT_CONFIG: &str = "/usr/local/etc/termblog.toml";

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub web: WebConfig,
    pub ssh: SshConfig,
    pub session: SessionConfig,
    pub jail: JailConfig,
}

impl Config {
    /// 加载配置: `Some(path)` 读指定文件; `None` 时读 DEFAULT_CONFIG,
    /// 文件不存在则全部用默认值。
    pub fn load(path: Option<&Path>) -> Result<Config> {
        let p = match path {
            Some(p) => p.to_path_buf(),
            None => PathBuf::from(DEFAULT_CONFIG),
        };
        if p.exists() {
            let s = std::fs::read_to_string(&p).with_context(|| format!("读配置 {}", p.display()))?;
            toml::from_str(&s).with_context(|| format!("解析配置 {}", p.display()))
        } else {
            Ok(Config::default())
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            web: WebConfig::default(),
            ssh: SshConfig::default(),
            session: SessionConfig::default(),
            jail: JailConfig::default(),
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
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8080".into(),
            static_dir: "frontend/dist".into(),
            site_url: None,
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
        Self { max_total: 64, max_per_ip: 3, hard_lifetime_secs: 7200 }
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

