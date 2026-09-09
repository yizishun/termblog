//! 会话核心：配额、PTY 泵与固定 FIFO 投稿。

use std::collections::HashMap;
use std::net::IpAddr;
#[cfg(target_os = "freebsd")]
use std::os::fd::AsFd;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Result};
use bytes::Bytes;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use termblog_commentd::{Client as CommentClient, SubmitRequest, MAX_COMMENT_BYTES};
use termblog_core::{Control, SessionHandle};
use termblog_statd::{RecordEvent, Source, MAX_BATCH_EVENTS};
use tokio::io::unix::AsyncFd;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::jail::JailBackend;
use crate::pty::{CommentFifo, ShellChild};
use crate::watcher::{ArticleRead, RunningArticleReads};

const OUTPUT_CHUNKS: usize = 128;
const COMMENT_QUEUE: usize = 16;
const ACK_QUEUE: usize = 32;
const MAX_SESSION_COMMENTS: usize = 8;

struct IdleDeadline {
    timeout: Duration,
    deadline: Instant,
}

impl IdleDeadline {
    fn new(timeout: Duration, now: Instant) -> Self {
        Self {
            timeout,
            deadline: now + timeout,
        }
    }

    fn refresh_on_input(&mut self, bytes: &[u8], now: Instant) {
        if !bytes.is_empty() {
            self.deadline = now + self.timeout;
        }
    }
}

pub struct Quota {
    pub max_total: usize,
    pub max_per_ip: usize,
}

struct Inner {
    backend: Arc<JailBackend>,
    quota: Quota,
    idle_timeout: Duration,
    table: Mutex<HashMap<String, IpAddr>>,
}

#[derive(Clone)]
pub struct SessionManager(Arc<Inner>);

impl SessionManager {
    pub fn new(backend: JailBackend, quota: Quota, idle_timeout: Duration) -> Self {
        Self(Arc::new(Inner {
            backend: Arc::new(backend),
            quota,
            idle_timeout,
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
        article_reads,
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
            tracing::error!(sid, %e, "failed to register comment FIFO");
            let _ = kill(Pid::from_raw(-pid.as_raw()), Signal::SIGHUP);
            reap(pid).await;
            let _ = inner.backend.cleanup(&sid).await;
            inner.table.lock().unwrap().remove(&sid);
            return;
        }
    };

    // NOTE_READ registrations were installed before fork.  Starting their
    // delivery task here cannot miss an early guest read because kqueue keeps
    // the one-shot event pending.
    let (article_tx, article_rx) = mpsc::channel(MAX_BATCH_EVENTS * 2);
    let running_article_reads: Option<RunningArticleReads> =
        article_reads.map(|reads| reads.start(article_tx));
    let mut article_stats_worker = tokio::spawn(article_stats_worker(
        inner.backend.stats_client(),
        sid.clone(),
        peer,
        article_rx,
    ));

    let (submit_tx, submit_rx) = mpsc::channel(COMMENT_QUEUE);
    let (ack_tx, mut ack_rx) = mpsc::channel::<Bytes>(ACK_QUEUE);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let accepted = Arc::new(AtomicUsize::new(0));
    let mut readers = Vec::new();
    for fifo in fifos {
        readers.push(tokio::spawn(fifo_reader(
            fifo,
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
    let mut idle = IdleDeadline::new(inner.idle_timeout, Instant::now());
    let why: &str;
    loop {
        tokio::select! {
            data = input.recv() => match data {
                Some(b) => {
                    idle.refresh_on_input(&b, Instant::now());
                    if tokio::time::timeout_at(idle.deadline, write_all(&master, &b))
                        .await
                        .is_err()
                    {
                        why = "idle timeout (no user input)";
                        break;
                    }
                }
                None => { why = "input channel closed (access layer disconnected)"; break; }
            },
            c = ctrl.recv() => match c {
                Some(Control::Resize { cols, rows }) => {
                    let _ = crate::pty::set_winsize(master.get_ref(), cols, rows);
                    let _ = kill(pid, Signal::SIGWINCH);
                }
                None => { why = "control channel closed (access layer disconnected)"; break; }
            },
            ack = ack_rx.recv(), if ack_open => match ack {
                Some(bytes) => {
                    // ack 只进已有输出队列；绝不写 PTY master（否则等价于模拟键盘）。
                    match tokio::time::timeout_at(idle.deadline, out.send(bytes)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(_)) => {
                            why = "output channel closed (access layer disconnected)";
                            break;
                        }
                        Err(_) => {
                            why = "idle timeout (no user input)";
                            break;
                        }
                    }
                    // zsh 可能已在异步 ack 之前画好下一个 prompt。用默认为
                    // ignore 的专用信号让 .zshrc 中的 ZLE trap 重画当前编辑行；
                    // 仍不向 PTY 输入缓冲区注入任何字节。
                    let _ = kill(pid, Signal::SIGURG);
                }
                None => ack_open = false,
            },
            ready = master.readable() => {
                let mut g = match ready { Ok(g) => g, Err(_) => { why = "master readable error"; break; } };
                match nix::unistd::read(master.as_raw_fd(), &mut buf) {
                    Ok(0) => { why = "PTY EOF (shell exited)"; break; }
                    Ok(n) => {
                        match tokio::time::timeout_at(
                            idle.deadline,
                            out.send(Bytes::copy_from_slice(&buf[..n])),
                        )
                        .await
                        {
                            Ok(Ok(())) => {}
                            Ok(Err(_)) => {
                                why = "output channel closed (access layer disconnected)";
                                break;
                            }
                            Err(_) => {
                                why = "idle timeout (no user input)";
                                break;
                            }
                        }
                    }
                    Err(nix::errno::Errno::EAGAIN) => g.clear_ready(),
                    Err(_) => { why = "PTY read error"; break; }
                }
            },
            _ = tokio::time::sleep_until(idle.deadline) => {
                why = "idle timeout (no user input)";
                break;
            },
        }
    }

    tracing::info!(
        sid,
        why,
        "pump exited, stopping submissions and performing final drain"
    );
    // 先停 writer，再让每个已打开 FIFO 读到 EAGAIN；路径从不重开。
    let _ = kill(Pid::from_raw(-pid.as_raw()), Signal::SIGHUP);
    if let Some(reads) = running_article_reads {
        reads.stop().await;
    }
    // Statistics must never lengthen jail reclamation.  Events already
    // committed remain durable; queued best-effort events may be dropped.
    article_stats_worker.abort();
    let _ = (&mut article_stats_worker).await;
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
                    tracing::warn!(sid, %e, "comment worker exited abnormally");
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
    tracing::info!(sid, "shell reaped, backend cleanup started");
    let _ = inner.backend.cleanup(&sid).await;
    inner.table.lock().unwrap().remove(&sid);
}

async fn article_stats_worker(
    client: termblog_statd::Client,
    sid: String,
    peer: IpAddr,
    mut reads: mpsc::Receiver<ArticleRead>,
) {
    while let Some(first) = reads.recv().await {
        let mut events = vec![RecordEvent {
            source: Source::TerminalReadSession,
            target: first.target,
            article: first.article,
            ip: peer.to_string(),
        }];
        while events.len() < MAX_BATCH_EVENTS {
            let next = match reads.try_recv() {
                Ok(next) => next,
                Err(_) => break,
            };
            events.push(RecordEvent {
                source: Source::TerminalReadSession,
                target: next.target,
                article: next.article,
                ip: peer.to_string(),
            });
        }
        if let Err(error) = client.record_batch(events).await {
            tracing::warn!(sid, %error, "failed to record terminal article reads");
        }
    }
}

struct PreparedCommentFifo {
    fifo: AsyncFd<OwnedFd>,
    target: String,
    closes: FifoCloseEvents,
}

#[cfg(target_os = "freebsd")]
struct PollableKqueue(nix::sys::event::Kqueue);

#[cfg(target_os = "freebsd")]
impl AsRawFd for PollableKqueue {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.0.as_fd().as_raw_fd()
    }
}

struct FifoCloseEvents {
    #[cfg(target_os = "freebsd")]
    queue: AsyncFd<PollableKqueue>,
}

impl FifoCloseEvents {
    #[cfg(target_os = "freebsd")]
    fn new(fd: &OwnedFd) -> Result<Self> {
        use nix::fcntl::{fcntl, FcntlArg, FdFlag};
        use nix::sys::event::{EventFilter, EventFlag, FilterFlag, KEvent, Kqueue};
        use tokio::io::Interest;

        let queue = Kqueue::new()?;
        fcntl(
            queue.as_fd().as_raw_fd(),
            FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC),
        )?;
        let change = KEvent::new(
            fd.as_raw_fd() as usize,
            EventFilter::EVFILT_VNODE,
            EventFlag::EV_ADD | EventFlag::EV_ENABLE | EventFlag::EV_CLEAR | EventFlag::EV_RECEIPT,
            FilterFlag::from_bits_retain(libc::NOTE_CLOSE_WRITE),
            0,
            0,
        );
        let mut receipt = [KEvent::new(
            0,
            EventFilter::EVFILT_VNODE,
            EventFlag::empty(),
            FilterFlag::empty(),
            0,
            0,
        )];
        let timeout = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let received = queue.kevent(&[change], &mut receipt, Some(timeout))?;
        if received != 1
            || !receipt[0].flags().contains(EventFlag::EV_ERROR)
            || receipt[0].data() != 0
        {
            bail!("kqueue rejected comment FIFO NOTE_CLOSE_WRITE registration");
        }
        Ok(Self {
            queue: AsyncFd::with_interest(PollableKqueue(queue), Interest::READABLE)?,
        })
    }

    #[cfg(not(target_os = "freebsd"))]
    fn new(_fd: &OwnedFd) -> Result<Self> {
        Ok(Self {})
    }

    #[cfg(target_os = "freebsd")]
    async fn wait(&self) -> Result<()> {
        use nix::sys::event::{EventFilter, EventFlag, FilterFlag, KEvent};

        loop {
            let mut ready = self.queue.readable().await?;
            let mut event = [KEvent::new(
                0,
                EventFilter::EVFILT_VNODE,
                EventFlag::empty(),
                FilterFlag::empty(),
                0,
                0,
            )];
            let timeout = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            let received = self
                .queue
                .get_ref()
                .0
                .kevent(&[], &mut event, Some(timeout))?;
            ready.clear_ready();
            if received == 0 {
                continue;
            }
            if event[0].flags().contains(EventFlag::EV_ERROR) {
                bail!("comment FIFO vnode watcher failed: {}", event[0].data());
            }
            return Ok(());
        }
    }

    #[cfg(not(target_os = "freebsd"))]
    async fn wait(&self) -> Result<()> {
        std::future::pending::<Result<()>>().await
    }
}

fn prepare_fifos(fifos: Vec<CommentFifo>) -> Result<Vec<PreparedCommentFifo>> {
    fifos
        .into_iter()
        .map(|f| {
            let closes = FifoCloseEvents::new(&f.fd)?;
            Ok(PreparedCommentFifo {
                fifo: AsyncFd::new(f.fd)?,
                target: f.target,
                closes,
            })
        })
        .collect()
}

async fn fifo_reader(
    prepared: PreparedCommentFifo,
    tx: mpsc::Sender<Submission>,
    ack: mpsc::Sender<Bytes>,
    mut shutdown: watch::Receiver<bool>,
    accepted: Arc<AtomicUsize>,
) {
    let PreparedCommentFifo {
        fifo,
        target,
        closes,
    } = prepared;
    // A normal echo/cat appends LF (or CRLF). Keep room for that terminator
    // without reducing the documented 512-byte comment limit.
    let mut payload = Vec::with_capacity(MAX_COMMENT_BYTES + 2);
    let mut overflow = false;
    let mut buf = [0u8; 1024];
    loop {
        tokio::select! {
            ready = fifo.readable() => {
                let mut guard = match ready { Ok(g) => g, Err(_) => break };
                if !drain_fifo(&fifo, &mut buf, &mut payload, &mut overflow) {
                    break;
                }
                guard.clear_ready();
            }
            closed = closes.wait() => {
                if closed.is_err()
                    || !drain_fifo(&fifo, &mut buf, &mut payload, &mut overflow)
                {
                    break;
                }
                if !finalize_fifo_write(
                    &target,
                    &tx,
                    &ack,
                    &accepted,
                    &mut payload,
                    &mut overflow,
                ).await {
                    break;
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    let _ = drain_fifo(&fifo, &mut buf, &mut payload, &mut overflow);
                    let _ = finalize_fifo_write(
                        &target,
                        &tx,
                        &ack,
                        &accepted,
                        &mut payload,
                        &mut overflow,
                    ).await;
                    break;
                }
            }
        }
    }
}

fn drain_fifo(
    fifo: &AsyncFd<OwnedFd>,
    buf: &mut [u8],
    payload: &mut Vec<u8>,
    overflow: &mut bool,
) -> bool {
    loop {
        match nix::unistd::read(fifo.as_raw_fd(), buf) {
            Ok(0) | Err(nix::errno::Errno::EAGAIN) => return true,
            Ok(n) => buffer_fifo_bytes(&buf[..n], payload, overflow),
            Err(_) => return false,
        }
    }
}

fn buffer_fifo_bytes(bytes: &[u8], payload: &mut Vec<u8>, overflow: &mut bool) {
    if *overflow {
        return;
    }
    let max_raw = MAX_COMMENT_BYTES + 2;
    if payload
        .len()
        .checked_add(bytes.len())
        .is_none_or(|len| len > max_raw)
    {
        payload.clear();
        *overflow = true;
    } else {
        payload.extend_from_slice(bytes);
    }
}

async fn finalize_fifo_write(
    target: &str,
    tx: &mpsc::Sender<Submission>,
    ack: &mpsc::Sender<Bytes>,
    accepted: &AtomicUsize,
    payload: &mut Vec<u8>,
    overflow: &mut bool,
) -> bool {
    if std::mem::take(overflow) {
        payload.clear();
        send_ack(ack, "Comment not submitted: comment exceeds 512 bytes").await;
        return true;
    }
    if payload.is_empty() {
        return true;
    }

    let mut raw = std::mem::take(payload);
    if raw.last() == Some(&b'\n') {
        raw.pop();
        if raw.last() == Some(&b'\r') {
            raw.pop();
        }
    }
    if raw.len() > MAX_COMMENT_BYTES {
        send_ack(ack, "Comment not submitted: comment exceeds 512 bytes").await;
        return true;
    }
    let text = match String::from_utf8(raw) {
        Ok(text) => text,
        Err(_) => {
            send_ack(ack, "Comment not submitted: input is not valid UTF-8").await;
            return true;
        }
    };
    let slot = accepted.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
        (n < MAX_SESSION_COMMENTS).then_some(n + 1)
    });
    if slot.is_err() {
        send_ack(ack, "Comment not submitted: maximum 8 comments per session").await;
        return true;
    }
    tx.send(Submission {
        target: target.to_string(),
        line: text,
    })
    .await
    .is_ok()
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
            Err(_) => "Comment not submitted: comments service temporarily unavailable".into(),
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

    #[test]
    fn idle_deadline_refreshes_only_for_nonempty_user_input() {
        let timeout = Duration::from_secs(15 * 60);
        let started = Instant::now();
        let mut idle = IdleDeadline::new(timeout, started);
        let original_deadline = idle.deadline;

        idle.refresh_on_input(&[], started + Duration::from_secs(60));
        assert_eq!(idle.deadline, original_deadline);

        let input_at = started + Duration::from_secs(60);
        idle.refresh_on_input(b"x", input_at);
        assert_eq!(idle.deadline, input_at + timeout);
    }

    #[tokio::test]
    async fn one_fifo_write_preserves_internal_newlines_and_is_bounded() {
        let (tx, mut rx) = mpsc::channel(16);
        let (ack, mut ack_rx) = mpsc::channel(16);
        let accepted = AtomicUsize::new(0);
        let mut payload = Vec::new();
        let mut overflow = false;

        buffer_fifo_bytes(b"alice: first\nsecond\n", &mut payload, &mut overflow);
        assert!(
            finalize_fifo_write(
                "/",
                &tx,
                &ack,
                &accepted,
                &mut payload,
                &mut overflow,
            )
            .await
        );
        assert_eq!(rx.recv().await.unwrap().line, "alice: first\nsecond");
        assert!(rx.try_recv().is_err());
        assert!(ack_rx.try_recv().is_err());
        assert_eq!(accepted.load(Ordering::Relaxed), 1);

        buffer_fifo_bytes(&[0xff, b'\n'], &mut payload, &mut overflow);
        finalize_fifo_write(
            "/",
            &tx,
            &ack,
            &accepted,
            &mut payload,
            &mut overflow,
        )
        .await;
        assert!(String::from_utf8_lossy(&ack_rx.recv().await.unwrap()).contains("UTF-8"));

        buffer_fifo_bytes(
            &vec![b'x'; MAX_COMMENT_BYTES + 1],
            &mut payload,
            &mut overflow,
        );
        finalize_fifo_write(
            "/",
            &tx,
            &ack,
            &accepted,
            &mut payload,
            &mut overflow,
        )
        .await;
        assert!(String::from_utf8_lossy(&ack_rx.recv().await.unwrap()).contains("512"));
        assert_eq!(accepted.load(Ordering::Relaxed), 1);
    }

    #[cfg(target_os = "freebsd")]
    #[tokio::test]
    async fn close_write_event_frames_each_cat_as_one_submission() {
        use std::ffi::CString;
        use std::io::Write;
        use std::os::fd::FromRawFd;
        use std::os::unix::ffi::OsStrExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("comment");
        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let raw_fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        assert!(raw_fd >= 0);
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        let prepared = prepare_fifos(vec![CommentFifo {
            fd,
            target: "/".into(),
        }])
        .unwrap()
        .pop()
        .unwrap();

        let (tx, mut rx) = mpsc::channel(4);
        let (ack, _ack_rx) = mpsc::channel(4);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let accepted = Arc::new(AtomicUsize::new(0));
        let reader = tokio::spawn(fifo_reader(
            prepared,
            tx,
            ack,
            shutdown_rx,
            accepted.clone(),
        ));

        {
            let mut writer = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            writer.write_all(b"bob: first\nsecond\n").unwrap();
        }

        let submission = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(submission.line, "bob: first\nsecond");
        assert_eq!(accepted.load(Ordering::Relaxed), 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), rx.recv())
                .await
                .is_err(),
            "one cat invocation produced more than one submission"
        );

        {
            let mut writer = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            writer.write_all(b"carol: separate write\n").unwrap();
        }
        let second = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.line, "carol: separate write");
        assert_eq!(accepted.load(Ordering::Relaxed), 2);

        shutdown_tx.send(true).unwrap();
        reader.await.unwrap();
    }
}
