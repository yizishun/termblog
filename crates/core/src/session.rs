//! 会话核心: 会话表 + 配额 + 每会话一个 PTY 读写泵 task。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};
use bytes::Bytes;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use std::os::unix::io::AsRawFd;
use tokio::io::unix::AsyncFd;
use tokio::sync::{broadcast, mpsc};

use crate::backend::ShellBackend;
use crate::pty::{set_winsize, ShellChild};

pub enum Control {
    Resize { cols: u16, rows: u16 },
}

/// 接入层拿到的句柄: 与 backend / 传输方式(web or ssh)无关。
/// drop 掉它(input/control 通道关闭)即表示接入方断开, 会话被回收。
pub struct SessionHandle {
    pub id: String,
    pub input: mpsc::Sender<Bytes>,          // 键入 -> PTY master 写
    pub output: broadcast::Receiver<Bytes>,  // PTY master 读 -> 所有观察者
    pub control: mpsc::Sender<Control>,      // Resize
}

pub struct Quota {
    pub max_total: usize,
    pub max_per_ip: usize,
}

struct Inner {
    backend: Box<dyn ShellBackend>,
    quota: Quota,
    table: Mutex<HashMap<String, IpAddr>>, // sid -> 来源 IP(配额计数用)
}

#[derive(Clone)]
pub struct SessionManager(Arc<Inner>);

impl SessionManager {
    pub fn new(backend: impl ShellBackend + 'static, quota: Quota) -> Self {
        Self(Arc::new(Inner {
            backend: Box::new(backend),
            quota,
            table: Mutex::new(HashMap::new()),
        }))
    }

    /// 开一个会话: 配额检查(入口直接拒绝, 不排队) -> spawn shell -> 启动读写泵
    pub async fn create(&self, peer: IpAddr, cols: u16, rows: u16) -> Result<SessionHandle> {
        let inner = &self.0;

        static NEXT_SID: AtomicU64 = AtomicU64::new(1);
        let sid = format!("{:08x}", NEXT_SID.fetch_add(1, Ordering::Relaxed));

        // 配额检查与占位一次完成(同一把锁), 并发 create 不会超发
        {
            let mut table = inner.table.lock().unwrap();
            if table.len() >= inner.quota.max_total {
                bail!("quota: server full");
            }
            if table.values().filter(|ip| **ip == peer).count() >= inner.quota.max_per_ip {
                bail!("quota: too many sessions from your IP");
            }
            table.insert(sid.clone(), peer);
        }

        let child = match inner.backend.spawn(&sid, cols, rows).await {
            Ok(c) => c,
            Err(e) => {
                inner.table.lock().unwrap().remove(&sid); // 释放占位
                return Err(e);
            }
        };

        let (input_tx, input_rx) = mpsc::channel(64);
        let (ctrl_tx, ctrl_rx) = mpsc::channel(8);
        let (out_tx, out_rx) = broadcast::channel(256); // 慢消费者丢帧, 由 xterm.js 重绘

        tokio::spawn(pump(self.0.clone(), sid.clone(), child, input_rx, ctrl_rx, out_tx));

        Ok(SessionHandle { id: sid, input: input_tx, output: out_rx, control: ctrl_tx })
    }
}

/// 每会话一个 task: 键入 / Resize / PTY 输出 三路的 select 循环。
/// 循环退出(三种情况: 接入层断开、shell 退出、IO 错误) => 回收会话。
async fn pump(
    inner: Arc<Inner>,
    sid: String,
    child: ShellChild,
    mut input: mpsc::Receiver<Bytes>,
    mut ctrl: mpsc::Receiver<Control>,
    out: broadcast::Sender<Bytes>,
) {
    let master = match AsyncFd::new(child.master) {
        Ok(m) => m,
        Err(_) => {
            // pump 未能启动: 收掉已 fork 出的 shell, 释放配额占位
            let _ = kill(Pid::from_raw(-child.pid.as_raw()), Signal::SIGHUP);
            reap(child.pid).await;
            inner.table.lock().unwrap().remove(&sid);
            return;
        }
    };
    let mut buf = [0u8; 8192];

    loop {
        tokio::select! {
            // 键入 -> 写 PTY 主端
            data = input.recv() => match data {
                Some(b) => { let _ = write_all(&master, &b).await; }
                None => break, // 接入层断开(SessionHandle 被 drop)
            },
            // 控制帧; control 通道关闭 => 接入层断开 => 回收
            c = ctrl.recv() => match c {
                Some(Control::Resize { cols, rows }) => {
                    let _ = set_winsize(master.get_ref(), cols, rows);
                    let _ = kill(child.pid, Signal::SIGWINCH);
                }
                None => break,
            },
            // PTY 输出 -> 广播给观察者
            ready = master.readable() => {
                let mut g = match ready { Ok(g) => g, Err(_) => break };
                match nix::unistd::read(master.as_raw_fd(), &mut buf) {
                    Ok(0) => break, // EOF: shell 退出
                    Ok(n) => { let _ = out.send(Bytes::copy_from_slice(&buf[..n])); }
                    Err(nix::errno::Errno::EAGAIN) => g.clear_ready(),
                    Err(_) => break,
                }
            },
        }
    }

    // ---- 回收: 杀进程组(forkpty 里 setsid 过, pid == pgid) -> wait -> 移出会话表 ----
    let _ = kill(Pid::from_raw(-child.pid.as_raw()), Signal::SIGHUP);
    reap(child.pid).await;
    let _ = inner.backend.cleanup(&sid).await;
    inner.table.lock().unwrap().remove(&sid);
    // out(broadcast::Sender) 随本 task 结束被 drop => 观察者收到 Closed, 得知会话终结
}

/// 等 shell 退出并收尸。waitpid 是阻塞调用, 放 blocking 线程;
/// SIGHUP 后宽限 5s 仍不退(shell 可能忽略 HUP)则 SIGKILL 兜底, 不留僵尸/泄漏。
async fn reap(pid: Pid) {
    let wait = |pid| tokio::task::spawn_blocking(move || nix::sys::wait::waitpid(pid, None));
    let grace = std::time::Duration::from_secs(5);
    if tokio::time::timeout(grace, wait(pid)).await.is_err() {
        let _ = kill(Pid::from_raw(-pid.as_raw()), Signal::SIGKILL);
        let _ = wait(pid).await;
    }
}

/// 尽力把一段键入完整写入 PTY(交互输入很小, 几乎不会真的阻塞)
async fn write_all(master: &AsyncFd<std::os::unix::io::OwnedFd>, mut b: &[u8]) -> Result<()> {
    while !b.is_empty() {
        match nix::unistd::write(master.get_ref(), b) {
            Ok(n) => b = &b[n..],
            Err(nix::errno::Errno::EAGAIN) => {
                master.writable().await?.clear_ready();
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
