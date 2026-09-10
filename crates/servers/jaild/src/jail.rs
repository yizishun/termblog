//! JailBackend: FreeBSD 会话隔离。
//!
//! 每会话一个 ephemeral jail, 生命周期与会话严格绑定:
//!
//! ```text
//! spawn(sid):
//!   1. zfs clone <template> <prefix><sid>        # 毫秒级, 可写层来自 clone
//!   2. mountpoint + mount, 挂 devfs(规则集 4)
//!   3. jail -c(无网络, allow.* 全关, persist) + jail 聚合 rctl 限额
//!   4. openpty + fork; 子进程: setsid -> ctty -> closefrom(3) -> RLIMIT_NOFILE
//!      -> jail_attach -> 降权 guest -> chdir(home) -> exec zsh -l
//!
//! cleanup(sid): umount devfs -> jail -r -> rctl -r -> zfs destroy
//!               -> remove empty mountpoint -> reclaim unpinned template.old* (幂等)
//! sweep(): 启动时回收上次崩溃遗留的 s-* 数据集和孤儿 RCTL 规则
//! ```
//!
//! 实现选择: 优先 `jail(8)` / `zfs(8)` / `rctl(8)` 命令行, 接口封装在本文件内;
//! 后续想换 libjail-rs / rctl crate 对外界无感。`jail_attach(2)` 在 fork 后的
//! 子进程里直接 syscall, PTY 从端、uid 切换、attach 的顺序完全可控。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::raw::{c_char, c_int};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Context, Result};
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::pty::{openpty, Winsize};
use nix::unistd::{chown, fork, ForkResult, Gid, Uid};
use tracing::{debug, error, info, warn};

use termblog_commentd::{
    protocol::PRIVATE_SYNC, visible_comments, Client as CommentClient, Comment, VisibleReply,
};
use termblog_config::{CommentsConfig, JailConfig, StatsConfig};
use termblog_content_model::{ArticleIndex, ArticlePath, ContentScope};
use termblog_statd::{
    Client as StatsClient, SnapshotRequest, SnapshotResponse, SnapshotTargetRequest,
    TargetSnapshot, MAX_SNAPSHOT_ARTICLES, MAX_SNAPSHOT_TARGETS,
};

use crate::pty::{CommentFifo, ShellChild};
use crate::watcher::{ArticleReadFile, PreparedArticleReads};

#[derive(Clone)]
pub struct JailBackend {
    cfg: JailConfig,
    comments_cfg: CommentsConfig,
    comments: CommentClient,
    stats: StatsClient,
    /// kern.osreldate(如 1500000 = 15.0): 处理跨版本参数差异
    osreldate: u32,
}

impl JailBackend {
    pub fn new(cfg: JailConfig, comments_cfg: CommentsConfig, stats_cfg: StatsConfig) -> Self {
        let comments = CommentClient::new(comments_cfg.private_socket.clone());
        let stats = StatsClient::new(
            stats_cfg.socket.clone(),
            std::time::Duration::from_millis(stats_cfg.request_timeout_ms),
        );
        Self {
            cfg,
            comments_cfg,
            comments,
            stats,
            osreldate: sysctl_osreldate().unwrap_or(0),
        }
    }

    /// 启动残留回收: 上次崩溃遗留的 s-* 数据集(jail + devfs + zfs 一并清掉)
    /// 及孤儿 RCTL。幂等；zfs 不可用时跳过文件系统部分，RCTL 仍独立尝试。
    pub fn sweep(cfg: &JailConfig) -> Result<()> {
        let parent = parent_dataset(&cfg.dataset_prefix);
        let out = Command::new("zfs")
            .args(["list", "-H", "-o", "name", "-r", parent])
            .output();
        let Ok(out) = out else {
            if let Err(e) = cleanup_orphaned_session_rctl_rules() {
                warn!(%e, "orphaned session rctl cleanup failed");
            }
            return Ok(());
        };
        if !out.status.success() {
            if let Err(e) = cleanup_orphaned_session_rctl_rules() {
                warn!(%e, "orphaned session rctl cleanup failed");
            }
            return Ok(());
        }
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let Some(sid) = line.strip_prefix(cfg.dataset_prefix.as_str()) else {
                continue;
            };
            if sid.is_empty() || sid.contains('/') || sid.contains('@') {
                continue;
            }
            warn!(sid, "reclaiming startup residual jail");
            cleanup_sync(cfg, sid);
        }
        // 旧版本留下的 session 及模板构建 staging 空 mountpoint 一并清掉。
        // 只删空目录：非空或仍挂载时 remove_dir 会安全失败。
        let template_leaf = cfg
            .template
            .split_once('@')
            .map(|(dataset, _)| dataset)
            .and_then(|dataset| dataset.rsplit('/').next());
        if let Ok(entries) = std::fs::read_dir(&cfg.path_prefix) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let sid_like = name.chars().all(|c| c.is_ascii_hexdigit())
                    || name
                        .strip_prefix("s-")
                        .is_some_and(|s| s.chars().all(|c| c.is_ascii_hexdigit()));
                let stale_template_mount = template_leaf.is_some_and(|template| {
                    name == format!("{template}.new") || name == format!("{template}-base.new")
                });
                if sid_like || stale_template_mount {
                    let _ = std::fs::remove_dir(e.path());
                }
            }
        }
        // RCTL 的 jail 规则按名称持久化，`jail -r` 不会自动删除。旧版本正常
        // 销毁的会话可能已没有 dataset，所以上面的 ZFS 枚举发现不了；单独从
        // RCTL 数据库枚举，并且只删除已确认没有活 jail 的 s-xxxxxxxx 规则。
        if let Err(e) = cleanup_orphaned_session_rctl_rules() {
            warn!(%e, "orphaned session rctl cleanup failed");
        }
        // 零停机换模板时，旧模板可能曾被旧会话 clone pin 住。最后一个
        // clone 消失后它已经可以销毁，不能一直等到下一次模板构建。
        cleanup_retired_templates(cfg);
        Ok(())
    }
}

impl JailBackend {
    /// 建一个会话 jail 并 fork 出 PTY 上的 shell。重活放 blocking 线程,
    /// 不占 tokio worker。caps: 白名单能力值(决定 TERMBLOG_IMG 环境变量)。
    pub async fn spawn(
        &self,
        sid: &str,
        cols: u16,
        rows: u16,
        caps: &[String],
    ) -> Result<ShellChild> {
        // 首次 approved 同步是会话交付 barrier；失败时不 fork shell。
        let approved = self
            .comments
            .all(PRIVATE_SYNC)
            .await
            .context("first-time comments sync")?;
        let snapshot = snapshot_bytes(&approved)?;
        let comment_counts = approved_counts(&approved)?;
        let cfg = self.cfg.clone();
        let comments_cfg = self.comments_cfg.clone();
        let stats = self.stats.clone();
        let sid = sid.to_string();
        let caps = caps.to_vec();
        let osreldate = self.osreldate;
        tokio::task::spawn_blocking(move || {
            spawn_sync(
                &cfg,
                &comments_cfg,
                &stats,
                &snapshot,
                &comment_counts,
                &sid,
                cols,
                rows,
                &caps,
                osreldate,
            )
        })
        .await
        .context("jail spawn task panicked")?
    }

    pub fn comment_client(&self) -> CommentClient {
        self.comments.clone()
    }

    pub fn stats_client(&self) -> StatsClient {
        self.stats.clone()
    }

    pub fn comment_drain_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.comments_cfg.session_drain_ms)
    }

    /// 会话结束后的资源销毁(jail -r + zfs destroy)。幂等;
    /// 单步失败只告警不中断(资源可能早已不存在)。
    pub async fn cleanup(&self, sid: &str) -> Result<()> {
        let cfg = self.cfg.clone();
        let sid = sid.to_string();
        let _ = tokio::task::spawn_blocking(move || cleanup_sync(&cfg, &sid)).await;
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)] // 生命周期参数在此边界显式传递，避免 fork 前隐藏状态。
fn spawn_sync(
    cfg: &JailConfig,
    comments_cfg: &CommentsConfig,
    stats: &StatsClient,
    snapshot: &[u8],
    comment_counts: &HashMap<String, u64>,
    sid: &str,
    cols: u16,
    rows: u16,
    caps: &[String],
    osreldate: u32,
) -> Result<ShellChild> {
    let ds = format!("{}{}", cfg.dataset_prefix, sid);
    let path = format!("{}/s-{sid}", cfg.path_prefix);
    let name = format!("s-{sid}");

    // 这段 preflight 必须位于下方“失败就 cleanup”边界之外：若发现同名活 jail，
    // 直接返回，绝不能让 cleanup_sync 反过来杀掉它或销毁它的数据集。
    // 会话编号在 jaild 重启后从 1 重新开始，而 FreeBSD 的 jail RCTL 规则会
    // 跨 `jail -r` 保留。确认旧 jail 不存在后清掉同名规则，使旧 prison_racct
    // 完全释放，不把历史 accounting 带进新会话。
    let running_jails = running_jail_names()?;
    if running_jails.contains(&name) {
        bail!("refusing to replace running jail {name}");
    }
    if clear_rctl_rules(&name)? {
        warn!(
            sid,
            jail = name,
            "removed stale rctl rules before jail creation"
        );
    }

    // 失败兜底: 任何一步失败都把已建资源全部销毁(幂等), 不留半成品
    let res = spawn_inner(
        cfg,
        comments_cfg,
        stats,
        snapshot,
        comment_counts,
        sid,
        cols,
        rows,
        caps,
        osreldate,
        &ds,
        &path,
        &name,
    );
    if let Err(e) = &res {
        // 失败原因必须落日志: 否则只剩 socket 上的 Closed 帧, 排障无门
        error!(sid, error = %e, "jail spawn failed");
        warn!(sid, "reclaiming residual resources");
        cleanup_sync(cfg, sid);
    }
    res
}

#[allow(clippy::too_many_arguments)] // spawn_sync 展开后的同步实现，共用同一失败清理边界。
fn spawn_inner(
    cfg: &JailConfig,
    comments_cfg: &CommentsConfig,
    stats: &StatsClient,
    snapshot: &[u8],
    comment_counts: &HashMap<String, u64>,
    sid: &str,
    cols: u16,
    rows: u16,
    caps: &[String],
    osreldate: u32,
    ds: &str,
    path: &str,
    name: &str,
) -> Result<ShellChild> {
    // 1. ZFS clone: 模板只读, 可写层来自 clone(销毁即回收)
    run("zfs", &["clone", &cfg.template, ds])?;
    run("zfs", &["set", "readonly=off", ds])?;
    // 磁盘配额: 每会话可写空间的硬上限。访客可自由写文件, 但写满 quota 后
    // 写操作返回 "Disk quota exceeded", 宿主 zroot 池不受影响(池满 = 宿主
    // 宕机, 这是本 jail 最危险的资源攻击面)。quota 只计 clone 独有的块,
    // 与模板共享的块不计入。fail-closed: 设不上就拒绝开会话, 不跑无配额 jail。
    run("zfs", &["set", &format!("quota={}", cfg.disk_quota), ds])?;
    // `zfs set mountpoint` 在 FreeBSD 上会自动挂载(clone 自模板继承的
    // mountpoint 已处于挂载态, set 时重挂到新位置), 显式 mount 仅作兜底,
    // "already mounted" 视为成功; 最后断言根路径确实可用
    run("zfs", &["set", &format!("mountpoint={path}"), ds])?;
    let _ = run("zfs", &["mount", ds]);
    if !std::fs::metadata(path).is_ok_and(|m| m.is_dir()) {
        bail!("jail root directory {path} not mounted");
    }

    // 2. devfs(默认 jail 规则集 4: 只暴露 null/zero/random 等无害设备)。
    // base.txz 不带 /dev 目录, 先建出来再挂
    let dev = format!("{path}/dev");
    std::fs::create_dir_all(&dev).with_context(|| format!("create {dev}"))?;
    if let Err(e) = run("mount", &["-t", "devfs", "-o", "ruleset=4", "devfs", &dev]) {
        warn!(sid, %e, "devfs mount failed (no /dev inside jail, functionality limited)");
    }

    // 3. 创建 jail: 无网络, allow.* 全关, persist。
    // 注意不能用 `-n` 输出 jid(那是"设置 jail 名"的弃用选项, 会吃掉 name= 参数);
    // 创建成功后再用 jls 查 jid。
    // 跨版本: FreeBSD 15.0 起 UFS quota 移除, 参数 allow.quotactl 改名 allow.quotas。
    // 一个二进制要同时跑 15.x/16 与老版本, 用运行时 kern.osreldate 选参数,
    // 而不是编译期宏(编译目标只有一个 freebsd, 宏区分不了 15 和 16)。
    let quota_param = if osreldate >= 1500000 {
        "allow.quotas=0"
    } else {
        "allow.quotactl=0"
    };
    let out = Command::new("jail")
        .args([
            "-c",
            &format!("name={name}"),
            &format!("path={path}"),
            "host.hostname=blog",
            "persist",
            "ip4=disable",
            "ip6=disable",
            "enforce_statfs=2",
            "children.max=0",
            "allow.raw_sockets=0",
            "allow.chflags=0",
            "allow.mount=0",
            quota_param,
            "allow.socket_af=0",
            "allow.sysvipc=0",
            "allow.mlock=0",
        ])
        .output()
        .context("execute jail -c")?;
    if !out.status.success() {
        bail!("jail -c failed: {}", stderr_of(&out));
    }
    let jid = jail_jid(name).context("query jail jid")?;

    // 4. jail 聚合 rctl 限额(racct 未开启时 fail-closed: 宁可不给会话,
    // 也不跑无配额 jail)。openfiles 不能放在这里: jaild 是长寿命、多线程进程，
    // fork 出来的子进程会继承曾经扩大的 fd 表；即使 closefrom(3) 关掉了实际 fd，
    // jail_attach 后 RACCT_NOFILE 的高水位仍会计入 jail，导致干净的 zsh 也无法
    // fork。文件描述符限制改由 fork 子进程上的 RLIMIT_NOFILE 承担。
    for limit in [
        format!("memoryuse:deny={}", cfg.memory),
        format!("vmemoryuse:deny={}", cfg.vmemory),
        format!("maxproc:deny={}", cfg.maxproc),
        format!("pcpu:deny={}", cfg.pcpu),
    ] {
        run("rctl", &["-a", &format!("jail:{name}:{limit}")])?;
    }

    // 5. guest 账号(从 jail 内的 passwd 解析 uid/gid/home)
    let (uid, gid, home) = guest_ids(path, &cfg.guest_user)?;

    // 6. Before the guest fork, validate both trusted manifests, pin every
    // rendered article inode, register NOTE_READ, and write all immutable
    // per-scope snapshots.  Nothing after fork resolves these guest paths.
    let targets_rel = comments_cfg
        .targets_file
        .strip_prefix("/")
        .context("comments.targets_file must be an absolute path")?;
    let scopes = read_targets(&Path::new(path).join(targets_rel))?;
    let index_path = comments_cfg
        .targets_file
        .with_file_name("article-index.json")
        .strip_prefix("/")
        .context("article index path must be absolute")?
        .to_path_buf();
    let index = read_article_index(&Path::new(path).join(index_path))?;
    let home_root = format!("{path}{home}");
    let articles_by_target = articles_by_target(&index, &scopes)?;
    let article_reads = match articles_by_target
        .iter()
        .flat_map(|(target, articles)| {
            articles.iter().map(|article| {
                Ok(ArticleReadFile {
                    fd: open_rendered_article_at(Path::new(&home_root), article)?,
                    target: target.clone(),
                    article: article.clone(),
                })
            })
        })
        .collect::<Result<Vec<_>>>()
    {
        Ok(article_files) if article_files.is_empty() => None,
        Ok(article_files) => match PreparedArticleReads::new(article_files) {
            Ok(reads) => Some(reads),
            Err(error) => {
                warn!(sid, %error, "failed to register article NOTE_READ watchers; statistics disabled for this session");
                None
            }
        },
        Err(error) => {
            warn!(sid, %error, "failed to pin rendered articles; terminal statistics disabled for this session");
            None
        }
    };
    let snapshot_request = build_snapshot_request(&scopes, &articles_by_target);
    if snapshot_request.is_none() {
        warn!(
            sid,
            "statistics snapshot exceeds protocol limits; writing unavailable status"
        );
    }
    let stats_snapshot = match snapshot_request.as_ref() {
        Some(request) => match stats.snapshot_blocking(request) {
            Ok(response) => match validate_stats_response(request, response) {
                Ok(response) => Some(response),
                Err(error) => {
                    warn!(sid, %error, "statd returned an invalid snapshot; writing unavailable status");
                    None
                }
            },
            Err(error) => {
                warn!(sid, %error, "statd snapshot unavailable; session will continue");
                None
            }
        },
        None => None,
    };
    write_stats_snapshots(
        Path::new(path),
        &scopes,
        &articles_by_target,
        comment_counts,
        stats_snapshot.as_ref(),
    )?;

    let comment_fifos = scopes
        .iter()
        .map(|scope| {
            Ok(CommentFifo {
                fd: open_fifo_at(Path::new(&home_root), &scope.comment_rel)?,
                target: scope.target.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    write_snapshot(path, snapshot)?;

    // 7. openpty + fork; 子进程只做异步信号安全的 syscall
    let ws = Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = openpty(Some(&ws), None).context("openpty")?;
    let (master, slave) = (pty.master, pty.slave);

    // exec 参数与环境在 fork 之前备好(子进程里不分配内存, 规避
    // 多线程进程 fork 后 malloc 的死锁风险)
    let zsh = CString::new(cfg.zsh.clone())?;
    let arg0 = CString::new("-zsh")?; // argv[0] 以 '-' 开头 => login shell
    let argv: [*const c_char; 2] = [arg0.as_ptr(), std::ptr::null()];
    // chdir(2) 要裸路径("HOME=/home/guest" 是环境串, 传它会 ENOENT 且静默
    // 留在 /); 环境串与路径分开备好(子进程里不分配内存)
    let home_dir = CString::new(home.clone())?;
    // 环境变量: fork 前构建 Vec<CString> + Vec<*const c_char>(fork 前分配
    // 是安全的, 现有代码同此模式; 子进程只用指针)。内容见 env_strings。
    let envs: Vec<CString> = env_strings(&home, caps)
        .into_iter()
        .map(|e| CString::new(e).expect("environment string without NUL"))
        .collect();
    let mut envp: Vec<*const c_char> = envs.iter().map(|e| e.as_ptr()).collect();
    envp.push(std::ptr::null());
    // 在 fork 前构造，避免多线程进程 fork 后触碰分配器。软、硬上限一并降低，
    // guest 及其后代都不能自行把 fd 上限抬回宿主值。
    let nofile_limit = child_nofile_rlimit(cfg.openfiles);

    match unsafe { fork() }.context("fork")? {
        ForkResult::Parent { child } => {
            drop(slave);
            // 主端设为非阻塞, 交给 core 的读写泵(AsyncFd)驱动
            let flags = OFlag::from_bits_truncate(fcntl(master.as_raw_fd(), FcntlArg::F_GETFL)?);
            fcntl(
                master.as_raw_fd(),
                FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK),
            )?;
            info!(sid, jid, pid = child.as_raw(), "jail session created");
            Ok(ShellChild {
                master,
                pid: child,
                comment_fifos,
                article_reads,
            })
        }
        ForkResult::Child => unsafe {
            libc::setsid();
            libc::ioctl(slave.as_raw_fd(), libc::TIOCSCTTY, 0);
            install_child_stdio(slave.as_raw_fd());
            if !set_child_nofile_limit(&nofile_limit) {
                let msg = b"setrlimit nofile failed\n";
                libc::write(2, msg.as_ptr().cast(), msg.len());
                libc::_exit(127);
            }
            // 顺序关键: attach 需要权限, 必须在降权之前
            if libc::jail_attach(jid) != 0 {
                let msg = b"jail_attach failed\n";
                libc::write(2, msg.as_ptr().cast(), msg.len());
                libc::_exit(127);
            }
            libc::setgroups(0, std::ptr::null());
            libc::setgid(gid);
            libc::setuid(uid);
            // 失败不致命(shell 落在 / 也比会话打不开强), 但要在 PTY 上留痕
            if libc::chdir(home_dir.as_ptr()) != 0 {
                let msg = b"chdir home failed\n";
                libc::write(2, msg.as_ptr().cast(), msg.len());
            }
            libc::execve(zsh.as_ptr(), argv.as_ptr(), envp.as_ptr());
            let msg = b"exec zsh failed\n";
            libc::write(2, msg.as_ptr().cast(), msg.len());
            libc::_exit(127);
        },
    }
}

/// 把本会话 PTY 安装为标准输入输出，并关闭从多线程 jaild 继承的
/// 其他全部描述符。子进程在 `jail_attach` 前仍处于宿主环境；若不做
/// `closefrom(3)`，新 zsh 会持有其他会话的 PTY master，既破坏隔离，也会把
/// jaild 的描述符状态泄漏给访客进程。
///
/// # Safety
///
/// 只能在 `fork` 后、`execve` 前的子进程中调用；`slave_fd` 必须是有效的
/// PTY slave。函数只执行 async-signal-safe 的系统调用。
unsafe fn install_child_stdio(slave_fd: c_int) {
    for fd in 0..3 {
        libc::dup2(slave_fd, fd);
    }
    if slave_fd > 2 {
        libc::close(slave_fd);
    }
    // FreeBSD closefrom(2) 由一次内核调用关闭所有 fd >= 3，
    // 不遍历 RLIMIT_NOFILE，适合多线程进程 fork 后的受限子进程。
    libc::closefrom(3);
}

fn child_nofile_rlimit(openfiles: u32) -> libc::rlimit {
    let limit = libc::rlim_t::from(openfiles);
    libc::rlimit {
        rlim_cur: limit,
        rlim_max: limit,
    }
}

/// 将每进程文件描述符上限降到配置值；后续 `execve` 会保留该限制。
///
/// # Safety
///
/// 只能在 `fork` 后、`execve` 前的子进程中调用。调用方必须传入 fork 前构造、
/// 当前仍有效的 `rlimit`。
unsafe fn set_child_nofile_limit(limit: &libc::rlimit) -> bool {
    libc::setrlimit(libc::RLIMIT_NOFILE, limit) == 0
}

#[derive(serde::Serialize)]
struct SnapshotLine<'a> {
    number: u64,
    target: &'a str,
    author: &'a str,
    date10: &'a str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to: Option<&'a VisibleReply>,
}

fn snapshot_bytes(comments: &[Comment]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for c in visible_comments(comments).map_err(anyhow::Error::msg)? {
        let date10 = c
            .created_at
            .get(..10)
            .ok_or_else(|| anyhow::anyhow!("commentd created_at too short"))?;
        serde_json::to_writer(
            &mut out,
            &SnapshotLine {
                number: c.number,
                target: &c.target,
                author: &c.author,
                date10,
                text: &c.text,
                reply_to: c.reply_to.as_ref(),
            },
        )?;
        out.push(b'\n');
    }
    Ok(out)
}

fn approved_counts(comments: &[Comment]) -> Result<HashMap<String, u64>> {
    // Reuse the public projection's global-ID/reply validation before trusting
    // target values for either snapshot.
    visible_comments(comments).map_err(anyhow::Error::msg)?;
    let mut counts = HashMap::new();
    for comment in comments {
        let count = counts.entry(comment.target.clone()).or_insert(0u64);
        *count = count
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("approved comment count overflow"))?;
    }
    Ok(counts)
}

fn read_targets(path: &Path) -> Result<Vec<ContentScope>> {
    let md = std::fs::symlink_metadata(path)
        .with_context(|| format!("read scope target manifest {}", path.display()))?;
    if !md.file_type().is_file()
        || md.file_type().is_symlink()
        || md.uid() != 0
        || md.mode() & 0o022 != 0
    {
        bail!("scope target manifest must be a root-owned, group/other non-writable regular file");
    }
    parse_targets(&std::fs::read_to_string(path)?)
}

fn parse_targets(text: &str) -> Result<Vec<ContentScope>> {
    termblog_content_model::parse_scope_manifest(text).map_err(anyhow::Error::msg)
}

fn read_article_index(path: &Path) -> Result<ArticleIndex> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("read article index {}", path.display()))?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != 0
        || metadata.mode() & 0o022 != 0
    {
        bail!("article index must be a root-owned, group/other non-writable regular file");
    }
    let index: ArticleIndex = serde_json::from_slice(&std::fs::read(path)?)
        .with_context(|| format!("parse article index {}", path.display()))?;
    index.validate().map_err(anyhow::Error::msg)?;
    Ok(index)
}

fn articles_by_target(
    index: &ArticleIndex,
    scopes: &[ContentScope],
) -> Result<BTreeMap<String, Vec<String>>> {
    let by_directory: HashMap<&str, &ContentScope> = scopes
        .iter()
        .map(|scope| (scope.directory_rel.as_str(), scope))
        .collect();
    let mut output: BTreeMap<String, Vec<String>> = scopes
        .iter()
        .map(|scope| (scope.target.clone(), Vec::new()))
        .collect();
    for entry in &index.articles {
        let article = ArticlePath::parse(&entry.source_rel).map_err(anyhow::Error::msg)?;
        if let Some(scope) = by_directory.get(article.directory_rel.as_str()) {
            output
                .get_mut(&scope.target)
                .expect("scope target initialized")
                .push(article.key);
        }
    }
    for articles in output.values_mut() {
        articles.sort();
    }
    Ok(output)
}

fn build_snapshot_request(
    scopes: &[ContentScope],
    articles_by_target: &BTreeMap<String, Vec<String>>,
) -> Option<SnapshotRequest> {
    let article_count: usize = articles_by_target.values().map(Vec::len).sum();
    if scopes.len() > MAX_SNAPSHOT_TARGETS || article_count > MAX_SNAPSHOT_ARTICLES {
        return None;
    }
    Some(SnapshotRequest {
        targets: scopes
            .iter()
            .map(|scope| SnapshotTargetRequest {
                target: scope.target.clone(),
                articles: articles_by_target
                    .get(&scope.target)
                    .cloned()
                    .unwrap_or_default(),
            })
            .collect(),
    })
}

fn open_rendered_article_at(home: &Path, article: &str) -> Result<OwnedFd> {
    let parsed = ArticlePath::parse(&format!("{article}.md")).map_err(anyhow::Error::msg)?;
    if parsed.key != article {
        bail!("invalid rendered article key {article}");
    }
    let home_c = CString::new(home.as_os_str().as_bytes())?;
    let raw = unsafe {
        libc::open(
            home_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error())
            .context("open guest home for rendered article");
    }
    let mut dir = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut components = std::iter::once(".rendered")
        .chain(article.split('/'))
        .peekable();
    while let Some(component) = components.next() {
        let component_c = CString::new(component)?;
        let last = components.peek().is_none();
        let flags = if last {
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC
        } else {
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC
        };
        let raw = unsafe { libc::openat(dir.as_raw_fd(), component_c.as_ptr(), flags) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("open rendered article {article}"));
        }
        let opened = unsafe { OwnedFd::from_raw_fd(raw) };
        if last {
            let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
            if unsafe { libc::fstat(opened.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error()).context("fstat rendered article");
            }
            let stat = unsafe { stat.assume_init() };
            if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
                bail!("rendered article {article} is not a regular file");
            }
            return Ok(opened);
        }
        dir = opened;
    }
    bail!("empty rendered article path")
}

fn validate_stats_response(
    request: &SnapshotRequest,
    response: SnapshotResponse,
) -> Result<SnapshotResponse> {
    if !response.ok || response.error.is_some() {
        bail!("statd returned an unsuccessful snapshot");
    }
    chrono::DateTime::parse_from_rfc3339(&response.snapshot_at)
        .context("statd snapshot_at is not RFC3339")?;
    let expected: BTreeMap<&str, BTreeSet<&str>> = request
        .targets
        .iter()
        .map(|target| {
            (
                target.target.as_str(),
                target.articles.iter().map(String::as_str).collect(),
            )
        })
        .collect();
    if expected.len() != request.targets.len() || response.targets.len() != expected.len() {
        bail!("statd snapshot target set differs from request");
    }
    let mut seen = BTreeSet::new();
    for target in &response.targets {
        let Some(expected_articles) = expected.get(target.target.as_str()) else {
            bail!(
                "statd snapshot contains unexpected target {}",
                target.target
            );
        };
        if !seen.insert(target.target.as_str()) {
            bail!("statd snapshot contains duplicate target {}", target.target);
        }
        let actual_articles: BTreeSet<&str> = target
            .articles
            .iter()
            .map(|article| article.article.as_str())
            .collect();
        if actual_articles.len() != target.articles.len() || &actual_articles != expected_articles {
            bail!(
                "statd snapshot article set differs for target {}",
                target.target
            );
        }
    }
    Ok(response)
}

fn write_stats_snapshots(
    jail_root: &Path,
    scopes: &[ContentScope],
    articles_by_target: &BTreeMap<String, Vec<String>>,
    comment_counts: &HashMap<String, u64>,
    stats: Option<&SnapshotResponse>,
) -> Result<()> {
    let fallback_time = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    for scope in scopes {
        let articles = articles_by_target
            .get(&scope.target)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let target_stats = stats.and_then(|snapshot| {
            snapshot
                .targets
                .iter()
                .find(|target| target.target == scope.target)
        });
        let snapshot_at = stats
            .map(|snapshot| snapshot.snapshot_at.as_str())
            .unwrap_or(&fallback_time);
        let bytes = stat_snapshot_bytes(
            scope,
            articles,
            *comment_counts.get(&scope.target).unwrap_or(&0),
            snapshot_at,
            target_stats,
        );
        write_scope_stat(jail_root, scope, &bytes)?;
    }
    Ok(())
}

fn stat_snapshot_bytes(
    scope: &ContentScope,
    articles: &[String],
    comments_approved: u64,
    snapshot_at: &str,
    stats: Option<&TargetSnapshot>,
) -> Vec<u8> {
    let mut output = format!(
        "version 1\ntarget {}\nsnapshot_at {}\nstats_status {}\n",
        scope.target,
        snapshot_at,
        if stats.is_some() { "ok" } else { "unavailable" }
    );
    if let Some(stats) = stats {
        output.push_str(&format!(
            "terminal_read_sessions_total {}\nstatic_requests_total {}\nunique_visitors_approx {}\n",
            stats.terminal_read_sessions_total,
            stats.static_requests_total,
            stats.unique_visitors_approx
        ));
    }
    output.push_str(&format!("comments_approved {comments_approved}\n"));
    if let Some(stats) = stats {
        let by_article: HashMap<&str, _> = stats
            .articles
            .iter()
            .map(|article| (article.article.as_str(), article))
            .collect();
        for key in articles {
            let article = by_article
                .get(key.as_str())
                .expect("validated statd article response");
            output.push_str(&format!(
                "article {key} terminal_read_sessions={} static_requests={}\n",
                article.terminal_read_sessions, article.static_requests
            ));
        }
    }
    output.into_bytes()
}

fn write_scope_stat(jail_root: &Path, scope: &ContentScope, bytes: &[u8]) -> Result<()> {
    let directory = jail_root.join(&scope.proc_rel);
    let metadata = std::fs::symlink_metadata(&directory)
        .with_context(|| format!("validate scope proc directory {}", directory.display()))?;
    if !metadata.file_type().is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != 0
        || metadata.gid() != 0
        || metadata.mode() & 0o7777 != 0o555
    {
        bail!(
            "{} must be a root:wheel 0555 real directory",
            directory.display()
        );
    }
    let final_path = directory.join("stat");
    match std::fs::symlink_metadata(&final_path) {
        Ok(metadata)
            if !metadata.file_type().is_file()
                || metadata.file_type().is_symlink()
                || metadata.uid() != 0
                || metadata.gid() != 0
                || metadata.mode() & 0o7777 != 0o444 =>
        {
            bail!(
                "{} must be a root:wheel 0444 regular file",
                final_path.display()
            );
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect scope stat {}", final_path.display()));
        }
    }
    let sequence = SNAPSHOT_SEQ.fetch_add(1, Ordering::Relaxed);
    let temporary = directory.join(format!(".stat.tmp.{}.{}", std::process::id(), sequence));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o444)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)?;
        file.write_all(bytes)?;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o444))?;
        chown(&temporary, Some(Uid::from_raw(0)), Some(Gid::from_raw(0)))?;
        // Flush both the contents and final ownership/mode before publishing
        // the directory entry. The directory fsync below makes the rename
        // durable as well.
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, &final_path)?;
        File::open(&directory)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// 以 guest home fd 为锚逐级 openat；每层都 O_NOFOLLOW，最终再次 fstat FIFO。
fn open_fifo_at(home: &Path, rel: &str) -> Result<OwnedFd> {
    let home_c = CString::new(home.as_os_str().as_bytes())?;
    let raw = unsafe {
        libc::open(
            home_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error()).context("open guest home");
    }
    let mut dir = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut parts = rel.split('/').peekable();
    while let Some(part) = parts.next() {
        let name = CString::new(part)?;
        let last = parts.peek().is_none();
        let flags = if last {
            libc::O_RDWR | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC
        } else {
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC
        };
        let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("openat comment device {rel}"));
        }
        let opened = unsafe { OwnedFd::from_raw_fd(fd) };
        if last {
            let mut st = std::mem::MaybeUninit::<libc::stat>::zeroed();
            if unsafe { libc::fstat(opened.as_raw_fd(), st.as_mut_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error()).context("fstat comment device");
            }
            let st = unsafe { st.assume_init() };
            if st.st_mode & libc::S_IFMT != libc::S_IFIFO {
                bail!("comment device {rel} is not a FIFO");
            }
            return Ok(opened);
        }
        dir = opened;
    }
    bail!("empty comment device path")
}

static SNAPSHOT_SEQ: AtomicU64 = AtomicU64::new(1);

fn write_snapshot(jail_root: &str, bytes: &[u8]) -> Result<()> {
    let root = Path::new(jail_root);
    for rel in ["var", "var/run"] {
        validate_root_dir(&root.join(rel))?;
    }
    let dir = root.join("var/run/termblog");
    if !dir.exists() {
        std::fs::create_dir(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))?;
    }
    validate_root_dir(&dir)?;
    let final_path = dir.join("comments.jsonl");
    if final_path.exists() {
        let md = std::fs::symlink_metadata(&final_path)?;
        if !md.file_type().is_file() || md.file_type().is_symlink() || md.uid() != 0 {
            bail!("comments snapshot is not a root-owned regular file");
        }
    }
    let seq = SNAPSHOT_SEQ.fetch_add(1, Ordering::Relaxed);
    let temp = dir.join(format!(".comments.tmp.{}.{}", std::process::id(), seq));
    let result = (|| -> Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&temp, &final_path)?;
        File::open(&dir)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn validate_root_dir(path: &Path) -> Result<()> {
    let md = std::fs::symlink_metadata(path)
        .with_context(|| format!("validate root-owned directory {}", path.display()))?;
    if !md.file_type().is_dir()
        || md.file_type().is_symlink()
        || md.uid() != 0
        || md.mode() & 0o022 != 0
    {
        bail!(
            "{} must be a root-owned, group/other non-writable real directory",
            path.display()
        );
    }
    Ok(())
}

/// 会话 shell 的环境变量内容(独立成纯函数便于单测):
/// caps 含 img-iterm2 时含 TERMBLOG_IMG=iterm2(图片二期: 告知 jailbin 的
/// blog 可以走 TUI 阅读器); **否则完全不设置该变量** —— 绝不设空值,
/// 空环境变量仍"存在", 会误导 reader 的严格相等判断。
fn env_strings(home: &str, caps: &[String]) -> Vec<String> {
    let mut e = vec![
        format!("HOME={home}"),
        "TERM=xterm-256color".into(),
        "PATH=/usr/local/bin:/usr/bin:/bin".into(),
    ];
    if caps.iter().any(|c| c == "img-iterm2") {
        e.push("TERMBLOG_IMG=iterm2".into());
    }
    e
}

/// 用 `jls -j <name> jid` 查 jail 的 jid(创建后立刻可用)。
fn jail_jid(name: &str) -> Result<c_int> {
    let out = Command::new("jls")
        .args(["-j", name, "jid"])
        .output()
        .context("execute jls")?;
    if !out.status.success() {
        bail!("jls -j {name} failed: {}", stderr_of(&out));
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .context("parse jail jid")
}

/// FreeBSD 14+ 的 /etc/defaults/devfs.rules 不再自带默认规则集, jail 默认
/// 规则集 4 不存在 => `mount -t devfs -o ruleset=4` 会失败, jail 里没有 /dev。
/// jaild 每次启动时把规则集补进运行期内核表(幂等; 规则只存活到下次重启,
/// 由 jaild 启动流程重建)。策略: 全部隐藏, 只露 null/zero/random 等无害设备。
pub fn ensure_devfs_ruleset() {
    let rules: &[&[&str]] = &[
        &["rule", "-s", "4", "add", "hide"],
        &["rule", "-s", "4", "add", "path", "null", "unhide"],
        &["rule", "-s", "4", "add", "path", "zero", "unhide"],
        &["rule", "-s", "4", "add", "path", "random", "unhide"],
        &["rule", "-s", "4", "add", "path", "urandom", "unhide"],
        &["rule", "-s", "4", "add", "path", "fd", "unhide"],
        &["rule", "-s", "4", "add", "path", "stdin", "unhide"],
        &["rule", "-s", "4", "add", "path", "stdout", "unhide"],
        &["rule", "-s", "4", "add", "path", "stderr", "unhide"],
    ];
    for r in rules {
        if let Err(e) = run("devfs", r) {
            warn!(%e, "configure devfs ruleset 4 failed (non-FreeBSD or insufficient permissions, can be ignored)");
            return;
        }
    }
    info!("devfs ruleset 4 ready (only exposes null/zero/random harmless devices inside jail)");
}

/// 幂等清理。顺序: devfs 先卸(否则 zfs destroy 会因 busy 失败)
/// -> 移 jail(顺带杀光 jail 里残留进程) -> 清 RCTL -> 毁数据集 -> 删空 mountpoint。
/// RCTL 只能在确认 jail 已消失后删除；若 jail 仍活着就解除规则，会产生无配额窗口。
fn cleanup_sync(cfg: &JailConfig, sid: &str) {
    let ds = format!("{}{}", cfg.dataset_prefix, sid);
    let path = format!("{}/s-{sid}", cfg.path_prefix);
    let name = format!("s-{sid}");
    if let Err(e) = run("umount", &["-f", &format!("{path}/dev")]) {
        warn!(sid, %e, "umount devfs failed (can be ignored)");
    }
    if let Err(e) = run("jail", &["-r", &name]) {
        // 幂等清理时，失败可能只是 jail 已被另一轮清理移除。命令结果不能代替
        // 状态检查；下面仍以 jls -d 为准，确认不存在后照样回收遗留规则。
        warn!(sid, %e, "jail -r failed; checking whether jail is already absent");
    }
    match wait_for_jail_removal(&name) {
        Ok(false) => warn!(
            sid,
            jail = name,
            "jail is still alive or dying after cleanup wait; keeping rctl rules"
        ),
        Ok(true) => match clear_rctl_rules(&name) {
            Ok(true) => debug!(sid, jail = name, "session rctl rules removed"),
            Ok(false) => {}
            Err(e) => warn!(sid, jail = name, %e, "remove session rctl rules failed"),
        },
        Err(e) => {
            warn!(sid, jail = name, %e, "cannot confirm jail removal; keeping rctl rules")
        }
    }
    if let Err(e) = run("zfs", &["destroy", "-f", &ds]) {
        warn!(sid, %e, "zfs destroy failed (can be ignored)");
    }
    // FreeBSD ZFS 销毁数据集后可能保留它自动创建的 mountpoint。这里只用
    // remove_dir，目录非空或仍是挂载点时会安全失败，绝不能递归删除。
    match std::fs::remove_dir(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!(sid, path, %e, "remove empty jail mountpoint failed"),
    }

    // 当前 session 可能正是某个 template.old* 的最后一个 clone。此时顺手
    // 回收退役模板；仍被其他 clone 引用时 zfs destroy 会拒绝，后续 session
    // cleanup 或下次启动还会重试。
    cleanup_retired_templates(cfg);
}

/// 回收零停机换面留下、且已不再被 session clone 引用的 template.old*。
///
/// 只匹配当前配置模板的精确退役命名：<template>.old 或
/// <template>.old-<unix timestamp>。使用 zfs destroy -r 而不是 -R：
/// 前者遇到外部 clone 会安全失败，绝不会连仍存活的 session clone 一起删掉。
/// template.old* 清完后，再清理由它们 pin 住的
/// <template>-base.old-<unix timestamp>-<pid>。
fn cleanup_retired_templates(cfg: &JailConfig) {
    let Some((template_dataset, snapshot)) = cfg.template.split_once('@') else {
        warn!(
            template = cfg.template,
            "template has no snapshot; retired template cleanup skipped"
        );
        return;
    };
    if template_dataset.is_empty() || snapshot.is_empty() {
        warn!(
            template = cfg.template,
            "invalid template snapshot; retired template cleanup skipped"
        );
        return;
    }

    let parent = parent_dataset(template_dataset);
    let out = Command::new("zfs")
        .args(["list", "-H", "-o", "name", "-r", parent])
        .output();
    let Ok(out) = out else {
        warn!(
            parent,
            "execute zfs list for retired template cleanup failed"
        );
        return;
    };
    if !out.status.success() {
        warn!(
            parent,
            error = %stderr_of(&out),
            "list retired templates failed"
        );
        return;
    }

    let listed = String::from_utf8_lossy(&out.stdout);
    // 顺序不能反：退役 template 是退役 base snapshot 的 clone。
    for dataset in listed.lines() {
        if !is_retired_template_dataset(template_dataset, dataset) {
            continue;
        }
        match run("zfs", &["destroy", "-r", dataset]) {
            Ok(()) => info!(dataset, "retired jail template storage reclaimed"),
            Err(e) => debug!(
                dataset,
                %e,
                "retired jail template still pinned; keeping it"
            ),
        }
    }
    for dataset in listed.lines() {
        if !is_retired_template_base_dataset(template_dataset, dataset) {
            continue;
        }
        match run("zfs", &["destroy", "-r", dataset]) {
            Ok(()) => info!(dataset, "retired jail template storage reclaimed"),
            Err(e) => debug!(
                dataset,
                %e,
                "retired jail template base still pinned; keeping it"
            ),
        }
    }
}

fn is_retired_template_dataset(template_dataset: &str, candidate: &str) -> bool {
    let Some(suffix) = candidate.strip_prefix(template_dataset) else {
        return false;
    };
    let Some(old_suffix) = suffix.strip_prefix(".old") else {
        return false;
    };
    old_suffix.is_empty()
        || old_suffix.strip_prefix('-').is_some_and(|timestamp| {
            !timestamp.is_empty() && timestamp.chars().all(|c| c.is_ascii_digit())
        })
}

fn is_retired_template_base_dataset(template_dataset: &str, candidate: &str) -> bool {
    let prefix = format!("{template_dataset}-base.old-");
    let Some(serial) = candidate.strip_prefix(&prefix) else {
        return false;
    };
    let mut parts = serial.split('-');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(timestamp), Some(pid), None)
            if !timestamp.is_empty()
                && timestamp.chars().all(|c| c.is_ascii_digit())
                && !pid.is_empty()
                && pid.chars().all(|c| c.is_ascii_digit())
    )
}

/// 列出当前宿主上的 jail 名。查询失败必须向上传递：调用方不能在“不知道 jail
/// 是否仍活着”时解除资源限制。
fn running_jail_names() -> Result<BTreeSet<String>> {
    let out = Command::new("jls")
        // dying jail 仍可能有进程；必须把它视为存活，不能提前解除 RCTL。
        .args(["-d", "name"])
        .output()
        .context("execute jls -d name")?;
    if !out.status.success() {
        bail!("jls -d name failed: {}", stderr_of(&out));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect())
}

/// `jail -r` 可能先把 jail 标为 dying、再异步等里面的进程退出。给它一个短暂
/// 的有界等待；只有 `jls -d` 已看不到该名称，调用方才能安全解除 RCTL 规则。
fn wait_for_jail_removal(name: &str) -> Result<bool> {
    const ATTEMPTS: usize = 40;

    for attempt in 0..ATTEMPTS {
        if !running_jail_names()?.contains(name) {
            return Ok(true);
        }
        if attempt + 1 < ATTEMPTS {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    Ok(false)
}

/// 查询并删除一个 jail subject 的全部 RCTL 规则。先查询是为了让“本来就没有
/// 规则”保持幂等；FreeBSD 的 `rctl -r` 在零匹配时会以 ESRCH 失败。
fn clear_rctl_rules(name: &str) -> Result<bool> {
    let filter = format!("jail:{name}");
    let out = Command::new("rctl")
        .arg(&filter)
        .output()
        .context("execute rctl rule query")?;
    if !out.status.success() {
        bail!("rctl {filter} failed: {}", stderr_of(&out));
    }
    if out.stdout.iter().all(u8::is_ascii_whitespace) {
        return Ok(false);
    }
    run("rctl", &["-r", &filter])?;
    Ok(true)
}

fn cleanup_orphaned_session_rctl_rules() -> Result<()> {
    let running = running_jail_names()?;
    let out = Command::new("rctl")
        .output()
        .context("execute rctl rule listing")?;
    if !out.status.success() {
        bail!("rctl rule listing failed: {}", stderr_of(&out));
    }

    let names = session_rctl_jail_names(&String::from_utf8_lossy(&out.stdout));
    let mut failures = 0usize;
    for name in names {
        if running.contains(&name) {
            warn!(jail = name, "live jail retained during orphan rctl sweep");
            continue;
        }
        // 启动时尚未 accept，但宿主管理员仍可能并发创建 jail；删除前重新
        // 查询一次，宁可暂留规则，也不能给刚出现的同名 jail 解除限额。
        match running_jail_names() {
            Ok(current) if current.contains(&name) => {
                warn!(
                    jail = name,
                    "jail appeared during orphan rctl sweep; retaining rules"
                );
                continue;
            }
            Ok(_) => {}
            Err(e) => {
                failures += 1;
                warn!(jail = name, %e, "cannot confirm orphaned jail; retaining rctl rules");
                continue;
            }
        }
        match clear_rctl_rules(&name) {
            Ok(true) => info!(jail = name, "orphaned session rctl rules reclaimed"),
            Ok(false) => {}
            Err(e) => {
                failures += 1;
                warn!(jail = name, %e, "orphaned session rctl removal failed");
            }
        }
    }
    if failures > 0 {
        bail!("failed to remove orphaned rctl rules for {failures} session jail(s)");
    }
    Ok(())
}

fn session_rctl_jail_names(rules: &str) -> BTreeSet<String> {
    rules
        .lines()
        .filter_map(|line| {
            let (name, _rule) = line.strip_prefix("jail:")?.split_once(':')?;
            is_session_jail_name(name).then(|| name.to_owned())
        })
        .collect()
}

fn is_session_jail_name(name: &str) -> bool {
    name.strip_prefix("s-").is_some_and(|sid| {
        sid.len() == 8
            && sid
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

/// 从 jail 内的 /etc/passwd 解析 guest 的 uid/gid/home。
/// (getpwnam 读的是宿主 passwd, guest 只存在于 jail 里, 所以直接解析文件)
/// 兼容两种格式: FreeBSD 是 7 字段(name:passwd:uid:gid:gecos:home:shell,
/// home 在 [5]); Linux/macOS 是 10 字段(home 在 [8])。
fn guest_ids(jail_root: &str, user: &str) -> Result<(u32, u32, String)> {
    let passwd = std::fs::read_to_string(format!("{jail_root}/etc/passwd"))
        .with_context(|| format!("read {jail_root}/etc/passwd"))?;
    for line in passwd.lines() {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() >= 7 && f[0] == user {
            let uid: u32 = f[2].parse().context("parse guest uid")?;
            let gid: u32 = f[3].parse().context("parse guest gid")?;
            let home = if f.len() >= 9 { f[8] } else { f[5] };
            return Ok((uid, gid, home.to_string()));
        }
    }
    bail!("no {user} user found in jail template")
}

/// kern.osreldate(如 1500000 = 15.0-RELEASE)。读不到时返回 None(调用方兜底)。
fn sysctl_osreldate() -> Option<u32> {
    let out = Command::new("sysctl")
        .args(["-n", "kern.osreldate"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// dataset_prefix "zroot/jails/s-" -> 父数据集 "zroot/jails"
fn parent_dataset(prefix: &str) -> &str {
    let p = prefix.trim_end_matches('/');
    match p.rfind('/') {
        Some(i) => &p[..i],
        None => p,
    }
}

fn run(prog: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(prog)
        .args(args)
        .output()
        .with_context(|| format!("execute {prog}"))?;
    if !out.status.success() {
        bail!("{prog} {} failed: {}", args.join(" "), stderr_of(&out));
    }
    Ok(())
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retired_template_match_is_exact_and_never_selects_sessions() {
        let template = "zroot/jails/template";
        for candidate in [
            "zroot/jails/template.old",
            "zroot/jails/template.old-1725883200",
        ] {
            assert!(is_retired_template_dataset(template, candidate));
        }
        for candidate in [
            "zroot/jails/template",
            "zroot/jails/template@release",
            "zroot/jails/template.old-",
            "zroot/jails/template.old-backup",
            "zroot/jails/template.old/child",
            "zroot/jails/s-00000001",
            "zroot/jails/other.old",
        ] {
            assert!(
                !is_retired_template_dataset(template, candidate),
                "must not reclaim {candidate}"
            );
        }

        for candidate in [
            "zroot/jails/template-base.old-1725883200-1234",
            "zroot/jails/template-base.old-1-9",
        ] {
            assert!(is_retired_template_base_dataset(template, candidate));
        }
        for candidate in [
            "zroot/jails/template-base",
            "zroot/jails/template-base.old",
            "zroot/jails/template-base.old-1725883200",
            "zroot/jails/template-base.old-1725883200-pid",
            "zroot/jails/template-base.old-1725883200-1234-child",
            "zroot/jails/other-base.old-1725883200-1234",
        ] {
            assert!(
                !is_retired_template_base_dataset(template, candidate),
                "must not reclaim {candidate}"
            );
        }
    }

    #[test]
    fn session_rctl_rule_parser_only_selects_owned_jail_names() {
        let rules = concat!(
            "jail:s-00000001:memoryuse:deny=134217728\n",
            "jail:s-00000001:openfiles:deny=256\n",
            "jail:s-00000002:pcpu:deny=25\n",
            "user:guest:openfiles:deny=256\n",
            "jail:production:memoryuse:deny=134217728\n",
            "jail:s-0000000A:maxproc:deny=32\n",
            "jail:s-0000000:maxproc:deny=32\n",
            "jail:s-000000003:maxproc:deny=32\n",
            "jail:s-00000004-child:maxproc:deny=32\n",
            "jail:s-00000005\n",
        );
        assert_eq!(
            session_rctl_jail_names(rules),
            BTreeSet::from(["s-00000001".to_owned(), "s-00000002".to_owned()])
        );
    }

    #[test]
    fn child_stdio_closes_inherited_fds_and_applies_nofile_limit() {
        use std::io::Read as _;

        use nix::sys::wait::{waitpid, WaitStatus};

        let mut pipe_fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
        let leaked_fd = unsafe { libc::fcntl(pipe_fds[1], libc::F_DUPFD, 100) };
        assert!(leaked_fd >= 100);
        let nofile_limit = child_nofile_rlimit(64);

        match unsafe { fork() }.unwrap() {
            ForkResult::Child => unsafe {
                // 用管道写端模拟 PTY slave；helper 会把它装到 0/1/2，随后
                // closefrom(3) 必须同时关闭读端、原始写端和刻意制造的高位 fd。
                install_child_stdio(pipe_fds[1]);
                let closed = libc::fcntl(leaked_fd, libc::F_GETFD) == -1;
                let limited = set_child_nofile_limit(&nofile_limit);
                let mut actual: libc::rlimit = std::mem::zeroed();
                let queried = libc::getrlimit(libc::RLIMIT_NOFILE, &mut actual) == 0;
                let correct_limit = queried
                    && actual.rlim_cur == nofile_limit.rlim_cur
                    && actual.rlim_max == nofile_limit.rlim_max;
                let (message, code): (&[u8], c_int) = if closed && limited && correct_limit {
                    (b"closed-and-limited", 0)
                } else {
                    (b"child-setup-failed", 1)
                };
                libc::write(1, message.as_ptr().cast(), message.len());
                libc::_exit(code);
            },
            ForkResult::Parent { child } => {
                unsafe {
                    libc::close(pipe_fds[1]);
                    libc::close(leaked_fd);
                }
                let mut output = String::new();
                unsafe { File::from_raw_fd(pipe_fds[0]) }
                    .read_to_string(&mut output)
                    .unwrap();
                assert_eq!(output, "closed-and-limited");
                assert_eq!(waitpid(child, None).unwrap(), WaitStatus::Exited(child, 0));
            }
        }
    }

    #[test]
    fn envp_contains_termblog_img_only_with_cap() {
        // caps 含 img-iterm2 → 设置 TERMBLOG_IMG=iterm2
        let envs = env_strings("/home/guest", &["img-iterm2".to_string()]);
        assert!(envs.iter().any(|e| e == "TERMBLOG_IMG=iterm2"), "{envs:?}");
        assert!(envs.iter().any(|e| e == "HOME=/home/guest"));
        // 无 caps / 未知 caps → 完全不设置该变量(不是设空值)
        let envs = env_strings("/home/guest", &[]);
        assert!(
            !envs.iter().any(|e| e.starts_with("TERMBLOG_IMG")),
            "{envs:?}"
        );
        let envs = env_strings("/home/guest", &["img-sixel".to_string()]);
        assert!(
            !envs.iter().any(|e| e.starts_with("TERMBLOG_IMG")),
            "{envs:?}"
        );
    }

    #[test]
    fn guest_snapshot_contains_only_local_numbers_and_reply_summary() {
        let comments = vec![
            Comment {
                id: 40,
                target: "/a/".into(),
                author: "alice".into(),
                text: "root".into(),
                created_at: "2026-09-05T00:00:00Z".into(),
                reply_to_id: None,
            },
            Comment {
                id: 41,
                target: "/b/".into(),
                author: "other".into(),
                text: "elsewhere".into(),
                created_at: "2026-09-05T00:00:01Z".into(),
                reply_to_id: None,
            },
            Comment {
                id: 50,
                target: "/a/".into(),
                author: "bob".into(),
                text: "reply\ncontinued".into(),
                created_at: "2026-09-05T00:00:02Z".into(),
                reply_to_id: Some(40),
            },
        ];
        let bytes = snapshot_bytes(&comments).unwrap();
        let rows: Vec<serde_json::Value> = std::str::from_utf8(&bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        assert_eq!(rows[0]["number"], 1);
        assert_eq!(rows[1]["number"], 1);
        assert_eq!(rows[2]["number"], 2);
        assert_eq!(rows[2]["reply_to"]["number"], 1);
        assert_eq!(rows[2]["reply_to"]["author"], "alice");
        assert_eq!(rows[2]["text"], "reply\ncontinued");
        for row in rows {
            assert!(row.get("id").is_none());
            assert!(row.get("reply_to_id").is_none());
        }
    }

    #[test]
    fn target_manifest_is_strict_and_derives_targets() {
        let rows = parse_targets(
            "comment\t/\nnotes/comment\t/notes/\nprojects/demo/comment\t/projects/demo/\n",
        )
        .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2].comment_rel, "projects/demo/comment");
        assert_eq!(rows[2].target, "/projects/demo/");

        for bad in [
            "/comment\t/\n",
            "notes/../comment\t/notes/../\n",
            "notes/comment\t/wrong/\n",
            "comment\t/\ncomment\t/\n",
            "missing-tab\n",
        ] {
            assert!(parse_targets(bad).is_err(), "应拒绝: {bad:?}");
        }
    }

    #[test]
    fn snapshot_protocol_overflow_degrades_without_rejecting_the_session() {
        let scope = ContentScope::from_directory_rel("").unwrap();
        let mut articles = BTreeMap::from([(scope.target.clone(), Vec::new())]);
        assert!(build_snapshot_request(std::slice::from_ref(&scope), &articles).is_some());

        articles.insert(
            scope.target.clone(),
            vec!["help".to_string(); MAX_SNAPSHOT_ARTICLES + 1],
        );
        assert!(build_snapshot_request(std::slice::from_ref(&scope), &articles).is_none());

        let too_many_scopes = vec![scope; MAX_SNAPSHOT_TARGETS + 1];
        assert!(build_snapshot_request(&too_many_scopes, &BTreeMap::new()).is_none());
    }

    #[test]
    fn stat_snapshot_is_stable_sorted_and_does_not_fake_unavailable_zeroes() {
        let scope = ContentScope::from_directory_rel("tests").unwrap();
        let articles = vec!["tests/a".to_string(), "tests/b".to_string()];
        let stats = TargetSnapshot {
            target: "/tests/".into(),
            terminal_read_sessions_total: 3,
            static_requests_total: 8,
            unique_visitors_approx: 5,
            articles: vec![
                termblog_statd::ArticleSnapshot {
                    article: "tests/a".into(),
                    terminal_read_sessions: 1,
                    static_requests: 2,
                },
                termblog_statd::ArticleSnapshot {
                    article: "tests/b".into(),
                    terminal_read_sessions: 2,
                    static_requests: 6,
                },
            ],
        };
        let text = String::from_utf8(stat_snapshot_bytes(
            &scope,
            &articles,
            4,
            "2026-09-05T12:34:56Z",
            Some(&stats),
        ))
        .unwrap();
        assert_eq!(
            text,
            "version 1\ntarget /tests/\nsnapshot_at 2026-09-05T12:34:56Z\nstats_status ok\nterminal_read_sessions_total 3\nstatic_requests_total 8\nunique_visitors_approx 5\ncomments_approved 4\narticle tests/a terminal_read_sessions=1 static_requests=2\narticle tests/b terminal_read_sessions=2 static_requests=6\n"
        );

        let unavailable = String::from_utf8(stat_snapshot_bytes(
            &scope,
            &articles,
            4,
            "2026-09-05T12:34:56Z",
            None,
        ))
        .unwrap();
        assert!(unavailable.contains("stats_status unavailable\ncomments_approved 4\n"));
        assert!(!unavailable.contains("static_requests_total"));
        assert!(!unavailable.contains("terminal_read_sessions="));
    }
}
