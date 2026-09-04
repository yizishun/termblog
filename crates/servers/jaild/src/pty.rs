//! PTY 细节(jaild 私有): ShellChild 与 winsize 设置。
//! fork 出 shell 的那半在 jail.rs(jail 内 exec)。

use std::os::unix::io::{AsRawFd, OwnedFd};

use anyhow::Result;
use nix::pty::Winsize;
use nix::unistd::Pid;

pub struct CommentFifo {
    pub fd: OwnedFd,
    /// 启动时从 root-owned 清单固定；运行期移动 inode 不改变归属。
    pub target: String,
}

pub struct ShellChild {
    pub master: OwnedFd, // PTY 主端: 读写泵操作它
    pub pid: Pid,        // 子进程(PTY 会话首进程): 用于发信号 / 回收
    pub comment_fifos: Vec<CommentFifo>,
}

/// TIOCSWINSZ: 通知 PTY 窗口尺寸变了(配合给子进程发 SIGWINCH)
pub fn set_winsize(master: &OwnedFd, cols: u16, rows: u16) -> Result<()> {
    let ws = winsize(cols, rows);
    if unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &ws) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn winsize(cols: u16, rows: u16) -> Winsize {
    Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}
