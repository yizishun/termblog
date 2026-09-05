//! Pre-fork article vnode watchers.
//!
//! FreeBSD's `NOTE_READ` is attached to the already-opened rendered inode, not
//! to a pathname.  A guest rename therefore cannot make an event drift to a
//! different article.  Every registration is EV_ONESHOT, which implements the
//! one-session/one-article counting rule without persisting session IDs.

use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;

pub struct ArticleReadFile {
    pub fd: OwnedFd,
    pub target: String,
    pub article: String,
}

#[derive(Debug)]
pub struct ArticleRead {
    pub target: String,
    pub article: String,
}

pub struct PreparedArticleReads {
    #[cfg(target_os = "freebsd")]
    queue: nix::sys::event::Kqueue,
    files: Vec<ArticleReadFile>,
}

impl PreparedArticleReads {
    #[cfg(target_os = "freebsd")]
    pub fn new(files: Vec<ArticleReadFile>) -> Result<Self> {
        use nix::fcntl::{fcntl, FcntlArg, FdFlag};
        use nix::sys::event::{EventFilter, EventFlag, FilterFlag, KEvent, Kqueue};
        use std::os::fd::{AsFd, AsRawFd};

        let queue = Kqueue::new()?;
        // kqueue(2), unlike the article open(2) calls, has no CLOEXEC flag.
        // Do not leak this host-side monitor into the guest shell after fork.
        fcntl(
            queue.as_fd().as_raw_fd(),
            FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC),
        )?;
        if !files.is_empty() {
            let note_read = FilterFlag::from_bits_retain(libc::NOTE_READ);
            let changes: Vec<KEvent> = files
                .iter()
                .enumerate()
                .map(|(index, file)| {
                    KEvent::new(
                        file.fd.as_raw_fd() as usize,
                        EventFilter::EVFILT_VNODE,
                        EventFlag::EV_ADD
                            | EventFlag::EV_ENABLE
                            | EventFlag::EV_ONESHOT
                            | EventFlag::EV_RECEIPT,
                        note_read,
                        0,
                        index as isize,
                    )
                })
                .collect();
            let mut receipts = vec![
                KEvent::new(
                    0,
                    EventFilter::EVFILT_VNODE,
                    EventFlag::empty(),
                    FilterFlag::empty(),
                    0,
                    0,
                );
                changes.len()
            ];
            let timeout = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            let received = queue.kevent(&changes, &mut receipts, Some(timeout))?;
            if received != changes.len()
                || receipts[..received]
                    .iter()
                    .any(|event| !event.flags().contains(EventFlag::EV_ERROR) || event.data() != 0)
            {
                anyhow::bail!("kqueue rejected one or more article NOTE_READ registrations");
            }
        }
        Ok(Self { queue, files })
    }

    #[cfg(not(target_os = "freebsd"))]
    pub fn new(files: Vec<ArticleReadFile>) -> Result<Self> {
        Ok(Self { files })
    }

    pub fn start(self, tx: mpsc::Sender<ArticleRead>) -> RunningArticleReads {
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = shutdown.clone();
        let worker = tokio::task::spawn_blocking(move || self.run(tx, worker_shutdown));
        RunningArticleReads { shutdown, worker }
    }

    #[cfg(target_os = "freebsd")]
    fn run(self, tx: mpsc::Sender<ArticleRead>, shutdown: Arc<AtomicBool>) {
        use nix::sys::event::{EventFilter, EventFlag, FilterFlag, KEvent};
        use tokio::sync::mpsc::error::TrySendError;

        let Self { queue, files } = self;
        let mut files: Vec<Option<ArticleReadFile>> = files.into_iter().map(Some).collect();
        let mut remaining = files.len();
        let mut queue_warning_emitted = false;
        let mut events = vec![
            KEvent::new(
                0,
                EventFilter::EVFILT_VNODE,
                EventFlag::empty(),
                FilterFlag::empty(),
                0,
                0,
            );
            remaining.max(1)
        ];
        let timeout = libc::timespec {
            tv_sec: 0,
            tv_nsec: 250_000_000,
        };
        while remaining > 0 && !shutdown.load(Ordering::Relaxed) {
            let count = match queue.kevent(&[], &mut events, Some(timeout)) {
                Ok(count) => count,
                Err(error) => {
                    tracing::warn!(%error, "article kqueue wait failed; disabling session watchers");
                    break;
                }
            };
            for event in &events[..count] {
                let index = event.udata() as usize;
                if event.flags().contains(EventFlag::EV_ERROR) {
                    tracing::warn!(index, error = event.data(), "article vnode watcher failed");
                    if index < files.len() && files[index].take().is_some() {
                        remaining -= 1;
                    }
                    continue;
                }
                let Some(file) = files.get_mut(index).and_then(Option::take) else {
                    continue;
                };
                remaining -= 1;
                // Dropping the pre-opened descriptor after the one-shot event
                // is deliberate; no pathname is consulted again.
                let ArticleReadFile {
                    fd: _,
                    target,
                    article,
                } = file;
                match tx.try_send(ArticleRead { target, article }) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        if !queue_warning_emitted {
                            tracing::warn!(
                                "terminal statistics queue full; dropping article read events"
                            );
                            queue_warning_emitted = true;
                        }
                    }
                    Err(TrySendError::Closed(_)) => return,
                }
            }
        }
    }

    #[cfg(not(target_os = "freebsd"))]
    fn run(self, _tx: mpsc::Sender<ArticleRead>, _shutdown: Arc<AtomicBool>) {
        // Production is FreeBSD.  Keeping the descriptors until this worker
        // exits preserves ownership semantics in cross-platform builds while
        // making the unsupported monitor a harmless no-op.
        drop(self);
    }
}

pub struct RunningArticleReads {
    shutdown: Arc<AtomicBool>,
    worker: tokio::task::JoinHandle<()>,
}

impl RunningArticleReads {
    pub async fn stop(self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Err(error) = self.worker.await {
            tracing::warn!(%error, "article watcher worker exited abnormally");
        }
    }
}

#[cfg(all(test, target_os = "freebsd"))]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::io::{Read, Seek, Write};
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;

    fn open_read(path: &std::path::Path) -> OwnedFd {
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        assert!(fd >= 0);
        unsafe { OwnedFd::from_raw_fd(fd) }
    }

    #[tokio::test]
    async fn note_read_is_one_shot_and_stays_with_renamed_inode() {
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("article");
        let renamed = directory.path().join("renamed");
        std::fs::File::create(&original)
            .unwrap()
            .write_all(b"body")
            .unwrap();
        let watched = open_read(&original);
        let prepared = PreparedArticleReads::new(vec![ArticleReadFile {
            fd: watched,
            target: "/".into(),
            article: "help".into(),
        }])
        .unwrap();
        std::fs::rename(&original, &renamed).unwrap();

        let (tx, mut rx) = mpsc::channel(1);
        let running = prepared.start(tx);
        let mut reader = std::fs::File::open(&renamed).unwrap();
        let mut body = String::new();
        reader.read_to_string(&mut body).unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.article, "help");
        reader.rewind().unwrap();
        reader.read_to_string(&mut String::new()).unwrap();
        let second = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
        assert!(
            matches!(second, Err(_) | Ok(None)),
            "unexpected second read event"
        );
        running.stop().await;
    }
    #[tokio::test]
    async fn articles_and_new_sessions_count_independently() {
        let directory = tempfile::tempdir().unwrap();
        let a = directory.path().join("a");
        let b = directory.path().join("b");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();

        let prepared = PreparedArticleReads::new(vec![
            ArticleReadFile {
                fd: open_read(&a),
                target: "/tests/".into(),
                article: "tests/a".into(),
            },
            ArticleReadFile {
                fd: open_read(&b),
                target: "/tests/".into(),
                article: "tests/b".into(),
            },
        ])
        .unwrap();
        let (tx, mut rx) = mpsc::channel(4);
        let running = prepared.start(tx);
        std::fs::read(&a).unwrap();
        std::fs::read(&a).unwrap();
        std::fs::read(&b).unwrap();

        let mut articles = BTreeSet::new();
        for _ in 0..2 {
            let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
            articles.insert(event.article);
        }
        assert_eq!(
            articles,
            BTreeSet::from(["tests/a".into(), "tests/b".into()])
        );
        let third = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
        assert!(matches!(third, Err(_) | Ok(None)));
        running.stop().await;

        let next_session = PreparedArticleReads::new(vec![ArticleReadFile {
            fd: open_read(&a),
            target: "/tests/".into(),
            article: "tests/a".into(),
        }])
        .unwrap();
        let (tx, mut rx) = mpsc::channel(1);
        let running = next_session.start(tx);
        std::fs::read(&a).unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.article, "tests/a");
        running.stop().await;
    }
}
