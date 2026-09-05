//! 会话核心：配额、PTY 泵与固定 FIFO 投稿。

use std::collections::HashMap;
use std::net::IpAddr;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Result};
use bytes::Bytes;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use termblog_commentd::{Client as CommentClient, SubmitRequest};
use termblog_core::{Control, SessionHandle};
use tokio::io::unix::AsyncFd;
use tokio::sync::{mpsc, watch};

use crate::jail::JailBackend;
use crate::pty::{CommentFifo, ShellChild};

const OUTPUT_CHUNKS: usize = 128;
const COMMENT_QUEUE: usize = 16;
const ACK_QUEUE: usize = 32;
const MAX_COMMENT_LINE: usize = 512;
const MAX_SESSION_COMMENTS: usize = 8;

pub struct Quota {
    pub max_total: usize,
    pub max_per_ip: usize,
}

struct Inner {
    backend: Arc<JailBackend>,
    quota: Quota,
    table: Mutex<HashMap<String, IpAddr>>,
}

#[derive(Clone)]
pub struct SessionManager(Arc<Inner>);

impl SessionManager {
    pub fn new(backend: JailBackend, quota: Quota) -> Self {
        Self(Arc::new(Inner {
            backend: Arc::new(backend),
            quota,
            table: Mutex::new(HashMap::new()),
        }))
    }

    pub async fn create(
        &self,
        peer: IpAddr,
        cols: u16,
        rows: u16,
        caps: Vec<String>,
    ) -> Result<SessionHandle> {
        let inner = &self.0;
        static NEXT_SID: AtomicU64 = AtomicU64::new(1);
        let sid = format!("{:08x}", NEXT_SID.fetch_add(1, Ordering::Relaxed));
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

        let child = match inner.backend.spawn(&sid, cols, rows, &caps).await {
            Ok(c) => c,
            Err(e) => {
                inner.table.lock().unwrap().remove(&sid);
                return Err(e);
            }
        };
        let (input_tx, input_rx) = mpsc::channel(64);
        let (ctrl_tx, ctrl_rx) = mpsc::channel(8);
        let (out_tx, out_rx) = mpsc::channel(OUTPUT_CHUNKS);
        tokio::spawn(pump(
            self.0.clone(),
            sid.clone(),
            peer,
            child,
            input_rx,
            ctrl_rx,
            out_tx,
        ));
        Ok(SessionHandle {
            id: sid,
            input: input_tx,
            output: out_rx,
            control: ctrl_tx,
        })
    }
}

struct Submission {
    target: String,
    line: String,
}

async fn pump(
    inner: Arc<Inner>,
    sid: String,
    peer: IpAddr,
    child: ShellChild,
    mut input: mpsc::Receiver<Bytes>,
    mut ctrl: mpsc::Receiver<Control>,
    out: mpsc::Sender<Bytes>,
) {
    let ShellChild {
        master: master_fd,
        pid,
        comment_fifos,
    } = child;
    let master = match AsyncFd::new(master_fd) {
        Ok(m) => m,
        Err(_) => {
            let _ = kill(Pid::from_raw(-pid.as_raw()), Signal::SIGHUP);
            reap(pid).await;
            let _ = inner.backend.cleanup(&sid).await;
            inner.table.lock().unwrap().remove(&sid);
            return;
        }
    };
    let fifos = match prepare_fifos(comment_fifos) {
        Ok(f) => f,
        Err(e) => {
            tracing::error!(sid, %e, "评论 FIFO 注册失败");
            let _ = kill(Pid::from_raw(-pid.as_raw()), Signal::SIGHUP);
            reap(pid).await;
            let _ = inner.backend.cleanup(&sid).await;
            inner.table.lock().unwrap().remove(&sid);
            return;
        }
    };

    let (submit_tx, submit_rx) = mpsc::channel(COMMENT_QUEUE);
    let (ack_tx, mut ack_rx) = mpsc::channel::<Bytes>(ACK_QUEUE);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let accepted = Arc::new(AtomicUsize::new(0));
    let mut readers = Vec::new();
    for (fifo, target) in fifos {
        readers.push(tokio::spawn(fifo_reader(
            fifo,
            target,
            submit_tx.clone(),
            ack_tx.clone(),
            shutdown_rx.clone(),
            accepted.clone(),
        )));
    }
    drop(submit_tx);

    let mut worker = tokio::spawn(comment_worker(
        inner.backend.comment_client(),
        peer.to_string(),
        submit_rx,
        ack_tx,
    ));
    let mut buf = [0u8; 8192];
    let mut ack_open = true;
    let why: &str;
    loop {
        tokio::select! {
            data = input.recv() => match data {
                Some(b) => { let _ = write_all(&master, &b).await; }
                None => { why = "input 通道关闭(接入层断开)"; break; }
            },
            c = ctrl.recv() => match c {
                Some(Control::Resize { cols, rows }) => {
                    let _ = crate::pty::set_winsize(master.get_ref(), cols, rows);
                    let _ = kill(pid, Signal::SIGWINCH);
                }
                None => { why = "control 通道关闭(接入层断开)"; break; }
            },
            ack = ack_rx.recv(), if ack_open => match ack {
                Some(bytes) => {
                    // ack 只进已有输出队列；绝不写 PTY master（否则等价于模拟键盘）。
                    if out.send(bytes).await.is_err() {
                        why = "输出通道关闭(接入层断开)";
                        break;
                    }
                    // zsh 可能已在异步 ack 之前画好下一个 prompt。用默认为
                    // ignore 的专用信号让 .zshrc 中的 ZLE trap 重画当前编辑行；
                    // 仍不向 PTY 输入缓冲区注入任何字节。
                    let _ = kill(pid, Signal::SIGURG);
                }
                None => ack_open = false,
            },
            ready = master.readable() => {
                let mut g = match ready { Ok(g) => g, Err(_) => { why = "master readable 错误"; break; } };
                match nix::unistd::read(master.as_raw_fd(), &mut buf) {
                    Ok(0) => { why = "PTY EOF(shell 退出)"; break; }
                    Ok(n) => {
                        if out.send(Bytes::copy_from_slice(&buf[..n])).await.is_err() {
                            why = "输出通道关闭(接入层断开)";
                            break;
                        }
                    }
                    Err(nix::errno::Errno::EAGAIN) => g.clear_ready(),
                    Err(_) => { why = "PTY 读错误"; break; }
                }
            },
        }
    }

    tracing::info!(sid, why, "pump 退出，停止投稿并做最终 drain");
    // 先停 writer，再让每个已打开 FIFO 读到 EAGAIN；路径从不重开。
    let _ = kill(Pid::from_raw(-pid.as_raw()), Signal::SIGHUP);
    let _ = shutdown_tx.send(true);
    let drain = inner.backend.comment_drain_timeout();
    let deadline = tokio::time::Instant::now() + drain;
    let mut readers = readers;
    if tokio::time::timeout_at(deadline, async {
        for reader in &mut readers {
            let _ = reader.await;
        }
    })
    .await
    .is_err()
    {
        for reader in &readers {
            reader.abort();
        }
    }
    // PTY 已结束但接入层可能仍连着；在同一个 drain deadline 内继续转发
    // 已持久化投稿的 ack。ack 队列有界，慢客户端不会撑大 jaild 内存。
    let mut worker_done = false;
    let mut ack_done = false;
    let mut deliver_acks = true;
    'drain: while !(worker_done && ack_done) {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                if !worker_done {
                    worker.abort();
                }
                break;
            }
            result = &mut worker, if !worker_done => {
                if let Err(e) = result {
                    tracing::warn!(sid, %e, "评论 worker 异常结束");
                }
                worker_done = true;
            }
            ack = ack_rx.recv(), if !ack_done => match ack {
                Some(bytes) if deliver_acks => {
                    match tokio::time::timeout_at(deadline, out.send(bytes)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(_)) => deliver_acks = false,
                        Err(_) => {
                            if !worker_done {
                                worker.abort();
                            }
                            break 'drain;
                        }
                    }
                }
                Some(_) => {}
                None => ack_done = true,
            }
        }
    }

    reap(pid).await;
    tracing::info!(sid, "shell 已收尸, backend 清理开始");
    let _ = inner.backend.cleanup(&sid).await;
    inner.table.lock().unwrap().remove(&sid);
}

fn prepare_fifos(fifos: Vec<CommentFifo>) -> Result<Vec<(AsyncFd<OwnedFd>, String)>> {
    fifos
        .into_iter()
        .map(|f| Ok((AsyncFd::new(f.fd)?, f.target)))
        .collect()
}

async fn fifo_reader(
    fifo: AsyncFd<OwnedFd>,
    target: String,
    tx: mpsc::Sender<Submission>,
    ack: mpsc::Sender<Bytes>,
    mut shutdown: watch::Receiver<bool>,
    accepted: Arc<AtomicUsize>,
) {
    let mut line = Vec::with_capacity(MAX_COMMENT_LINE);
    let mut discard = false;
    let mut buf = [0u8; 1024];
    loop {
        tokio::select! {
            ready = fifo.readable() => {
                let mut guard = match ready { Ok(g) => g, Err(_) => break };
                match nix::unistd::read(fifo.as_raw_fd(), &mut buf) {
                    Ok(0) => guard.clear_ready(), // O_RDWR 主方案不应 EOF；清 readiness 防空转。
                    Ok(n) => consume_fifo_bytes(&buf[..n], &target, &tx, &ack, &accepted, &mut line, &mut discard).await,
                    Err(nix::errno::Errno::EAGAIN) => guard.clear_ready(),
                    Err(_) => break,
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    loop {
                        match nix::unistd::read(fifo.as_raw_fd(), &mut buf) {
                            Ok(0) | Err(nix::errno::Errno::EAGAIN) => break,
                            Ok(n) => consume_fifo_bytes(&buf[..n], &target, &tx, &ack, &accepted, &mut line, &mut discard).await,
                            Err(_) => break,
                        }
                    }
                    break;
                }
            }
        }
    }
}

async fn consume_fifo_bytes(
    bytes: &[u8],
    target: &str,
    tx: &mpsc::Sender<Submission>,
    ack: &mpsc::Sender<Bytes>,
    accepted: &AtomicUsize,
    line: &mut Vec<u8>,
    discard: &mut bool,
) {
    for &byte in bytes {
        if *discard {
            if byte == b'\n' {
                *discard = false;
            }
            continue;
        }
        if byte == b'\n' {
            let raw = std::mem::take(line);
            let text = match String::from_utf8(raw) {
                Ok(s) => s,
                Err(_) => {
                    send_ack(ack, "评论未提交：输入不是合法 UTF-8").await;
                    continue;
                }
            };
            let slot = accepted.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < MAX_SESSION_COMMENTS).then_some(n + 1)
            });
            if slot.is_err() {
                send_ack(ack, "评论未提交：每个会话最多 8 条").await;
                continue;
            }
            if tx
                .send(Submission {
                    target: target.to_string(),
                    line: text,
                })
                .await
                .is_err()
            {
                return;
            }
        } else if line.len() == MAX_COMMENT_LINE {
            line.clear();
            *discard = true;
            send_ack(ack, "评论未提交：单行超过 512 字节").await;
        } else {
            line.push(byte);
        }
    }
}

async fn comment_worker(
    client: CommentClient,
    ip: String,
    mut rx: mpsc::Receiver<Submission>,
    ack: mpsc::Sender<Bytes>,
) {
    while let Some(item) = rx.recv().await {
        let notice = match client
            .submit(&SubmitRequest {
                target: item.target,
                line: item.line,
                ip: ip.clone(),
            })
            .await
        {
            Ok(res) => res.notice,
            Err(_) => "评论未提交：评论服务暂不可用".into(),
        };
        send_ack(&ack, &notice).await;
    }
}

async fn send_ack(tx: &mpsc::Sender<Bytes>, notice: &str) {
    let _ = tx.send(Bytes::from(format!("\r\n{notice}\r\n"))).await;
}

async fn reap(pid: Pid) {
    let wait = |pid| tokio::task::spawn_blocking(move || nix::sys::wait::waitpid(pid, None));
    let grace = Duration::from_secs(5);
    if tokio::time::timeout(grace, wait(pid)).await.is_err() {
        let _ = kill(Pid::from_raw(-pid.as_raw()), Signal::SIGKILL);
        let _ = wait(pid).await;
    }
}

async fn write_all(master: &AsyncFd<OwnedFd>, mut bytes: &[u8]) -> Result<()> {
    while !bytes.is_empty() {
        match nix::unistd::write(master.get_ref(), bytes) {
            Ok(n) => bytes = &bytes[n..],
            Err(nix::errno::Errno::EAGAIN) => master.writable().await?.clear_ready(),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fifo_lines_are_strict_utf8_bounded_and_session_limited() {
        let (tx, mut rx) = mpsc::channel(16);
        let (ack, mut ack_rx) = mpsc::channel(16);
        let accepted = AtomicUsize::new(0);
        let mut line = Vec::new();
        let mut discard = false;
        consume_fifo_bytes(
            b"alice: ok\n",
            "/",
            &tx,
            &ack,
            &accepted,
            &mut line,
            &mut discard,
        )
        .await;
        assert_eq!(rx.recv().await.unwrap().line, "alice: ok");
        consume_fifo_bytes(
            &[0xff, b'\n'],
            "/",
            &tx,
            &ack,
            &accepted,
            &mut line,
            &mut discard,
        )
        .await;
        assert!(String::from_utf8_lossy(&ack_rx.recv().await.unwrap()).contains("UTF-8"));
        let mut over = vec![b'x'; 513];
        over.push(b'\n');
        consume_fifo_bytes(&over, "/", &tx, &ack, &accepted, &mut line, &mut discard).await;
        assert!(String::from_utf8_lossy(&ack_rx.recv().await.unwrap()).contains("512"));
    }
}
