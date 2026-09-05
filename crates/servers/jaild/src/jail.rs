//! JailBackend: FreeBSD 会话隔离。
//!
//! 每会话一个 ephemeral jail, 生命周期与会话严格绑定:
//!
//! ```text
//! spawn(sid):
//!   1. zfs clone <template> <prefix><sid>        # 毫秒级, 可写层来自 clone
//!   2. mountpoint + mount, 挂 devfs(规则集 4)
//!   3. jail -c(无网络, allow.* 全关, persist) + rctl 限额
//!   4. openpty + fork; 子进程: setsid -> ctty -> jail_attach -> 降权 guest
//!      -> chdir(home) -> exec zsh -l
//!
//! cleanup(sid): umount devfs -> jail -r -> zfs destroy (幂等)
//! sweep(): 启动时回收上次崩溃遗留的 s-* 数据集
//! ```
//!
//! 实现选择: 优先 `jail(8)` / `zfs(8)` / `rctl(8)` 命令行, 接口封装在本文件内;
//! 后续想换 libjail-rs / rctl crate 对外界无感。`jail_attach(2)` 在 fork 后的
//! 子进程里直接 syscall, PTY 从端、uid 切换、attach 的顺序完全可控。

use std::collections::HashSet;
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
use nix::unistd::{fork, ForkResult};
use tracing::{error, info, warn};

use termblog_commentd::{
    protocol::PRIVATE_SYNC, visible_comments, Client as CommentClient, Comment, VisibleReply,
};
use termblog_config::{CommentsConfig, JailConfig};

use crate::pty::{CommentFifo, ShellChild};

#[derive(Clone)]
pub struct JailBackend {
    cfg: JailConfig,
    comments_cfg: CommentsConfig,
    comments: CommentClient,
    /// kern.osreldate(如 1500000 = 15.0): 处理跨版本参数差异
    osreldate: u32,
}

impl JailBackend {
    pub fn new(cfg: JailConfig, comments_cfg: CommentsConfig) -> Self {
        let comments = CommentClient::new(comments_cfg.private_socket.clone());
        Self {
            cfg,
            comments_cfg,
            comments,
            osreldate: sysctl_osreldate().unwrap_or(0),
        }
    }

    /// 启动残留回收: 上次崩溃遗留的 s-* 数据集(jail + devfs + zfs 一并清掉)。
    /// 幂等; zfs 不可用(非 ZFS 机器)时静默跳过, 方便开发环境直接拉起 jaild 调试。
    pub fn sweep(cfg: &JailConfig) -> Result<()> {
        let parent = parent_dataset(&cfg.dataset_prefix);
        let out = Command::new("zfs")
            .args(["list", "-H", "-o", "name", "-r", parent])
            .output();
        let Ok(out) = out else { return Ok(()) };
        if !out.status.success() {
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
        // 旧版本留下的空 mountpoint 目录(/jails/<sid> 或 /jails/s-<sid>)一并清掉。
        // 只删空目录: 非空说明还挂着文件系统, 不能碰。
        if let Ok(entries) = std::fs::read_dir(&cfg.path_prefix) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let sid_like = name.chars().all(|c| c.is_ascii_hexdigit())
                    || name
                        .strip_prefix("s-")
                        .is_some_and(|s| s.chars().all(|c| c.is_ascii_hexdigit()));
                if sid_like {
                    let _ = std::fs::remove_dir(e.path());
                }
            }
        }
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
        let cfg = self.cfg.clone();
        let comments_cfg = self.comments_cfg.clone();
        let sid = sid.to_string();
        let caps = caps.to_vec();
        let osreldate = self.osreldate;
        tokio::task::spawn_blocking(move || {
            spawn_sync(
                &cfg,
                &comments_cfg,
                &snapshot,
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
    snapshot: &[u8],
    sid: &str,
    cols: u16,
    rows: u16,
    caps: &[String],
    osreldate: u32,
) -> Result<ShellChild> {
    let ds = format!("{}{}", cfg.dataset_prefix, sid);
    let path = format!("{}/s-{sid}", cfg.path_prefix);
    let name = format!("s-{sid}");

    // 失败兜底: 任何一步失败都把已建资源全部销毁(幂等), 不留半成品
    let res = spawn_inner(
        cfg,
        comments_cfg,
        snapshot,
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
    snapshot: &[u8],
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

    // 4. rctl 限额(racct 未开启时 fail-closed: 宁可不给会话, 也不跑无配额 jail)
    for limit in [
        format!("memoryuse:deny={}", cfg.memory),
        format!("vmemoryuse:deny={}", cfg.vmemory),
        format!("maxproc:deny={}", cfg.maxproc),
        format!("openfiles:deny={}", cfg.openfiles),
        format!("pcpu:deny={}", cfg.pcpu),
    ] {
        run("rctl", &["-a", &format!("jail:{name}:{limit}")])?;
    }

    // 5. guest 账号(从 jail 内的 passwd 解析 uid/gid/home)
    let (uid, gid, home) = guest_ids(path, &cfg.guest_user)?;

    // 6. shell 启动前固定打开全部 FIFO；随后写完 root-owned 初始快照。
    let targets_rel = comments_cfg
        .targets_file
        .strip_prefix("/")
        .context("comments.targets_file must be an absolute path")?;
    let targets = read_targets(&Path::new(path).join(targets_rel))?;
    let home_root = format!("{path}{home}");
    let comment_fifos = targets
        .iter()
        .map(|(rel, target)| {
            Ok(CommentFifo {
                fd: open_fifo_at(Path::new(&home_root), rel)?,
                target: target.clone(),
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
            })
        }
        ForkResult::Child => unsafe {
            libc::setsid();
            libc::ioctl(slave.as_raw_fd(), libc::TIOCSCTTY, 0);
            for fd in 0..3 {
                libc::dup2(slave.as_raw_fd(), fd);
            }
            if slave.as_raw_fd() > 2 {
                libc::close(slave.as_raw_fd());
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

fn read_targets(path: &Path) -> Result<Vec<(String, String)>> {
    let md = std::fs::symlink_metadata(path)
        .with_context(|| format!("read comments target manifest {}", path.display()))?;
    if !md.file_type().is_file()
        || md.file_type().is_symlink()
        || md.uid() != 0
        || md.mode() & 0o022 != 0
    {
        bail!("comments target manifest must be a root-owned, group/other non-writable regular file");
    }
    parse_targets(&std::fs::read_to_string(path)?)
}

fn parse_targets(text: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    let mut paths = HashSet::new();
    let mut targets = HashSet::new();
    for (idx, line) in text.lines().enumerate() {
        let (rel, target) = line
            .split_once('\t')
            .ok_or_else(|| anyhow::anyhow!("target manifest line {} missing Tab", idx + 1))?;
        if target.contains('\t')
            || rel.is_empty()
            || rel.starts_with('/')
            || rel
                .split('/')
                .any(|p| p.is_empty() || p == "." || p == "..")
        {
            bail!("target manifest line {} invalid relative path", idx + 1);
        }
        let derived = target_for_fifo(rel)
            .ok_or_else(|| anyhow::anyhow!("target manifest line {} path is not a comment device", idx + 1))?;
        if derived != target || !termblog_commentd::valid_target(target) {
            bail!("target manifest line {} mapping inconsistent", idx + 1);
        }
        if !paths.insert(rel.to_string()) || !targets.insert(target.to_string()) {
            bail!("target manifest line {} duplicate", idx + 1);
        }
        out.push((rel.to_string(), target.to_string()));
    }
    Ok(out)
}

fn target_for_fifo(rel: &str) -> Option<String> {
    let directory = if rel == "comment" {
        ""
    } else {
        rel.strip_suffix("/comment")?
    };
    let attachment =
        termblog_content_model::CommentAttachment::from_directory_rel(directory).ok()?;
    (attachment.fifo_rel == rel).then_some(attachment.target)
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
/// -> 移 jail(顺带杀光 jail 里残留进程) -> 毁数据集。
fn cleanup_sync(cfg: &JailConfig, sid: &str) {
    let ds = format!("{}{}", cfg.dataset_prefix, sid);
    let path = format!("{}/s-{sid}", cfg.path_prefix);
    let name = format!("s-{sid}");
    if let Err(e) = run("umount", &["-f", &format!("{path}/dev")]) {
        warn!(sid, %e, "umount devfs failed (can be ignored)");
    }
    if let Err(e) = run("jail", &["-r", &name]) {
        warn!(sid, %e, "jail -r failed (can be ignored)");
    }
    if let Err(e) = run("zfs", &["destroy", "-f", &ds]) {
        warn!(sid, %e, "zfs destroy failed (can be ignored)");
    }
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
                text: "reply".into(),
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
        assert_eq!(
            rows[2],
            ("projects/demo/comment".into(), "/projects/demo/".into())
        );

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
}
