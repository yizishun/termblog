//! PTY 细节: forkpty + winsize。core 里唯一碰操作系统终端接口的地方。

use std::ffi::CString;
use std::os::unix::io::{AsRawFd, OwnedFd};

use anyhow::Result;
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::pty::{forkpty, ForkptyResult, Winsize};
use nix::unistd::{execvp, Pid};

pub struct ShellChild {
    pub master: OwnedFd, // PTY 主端: core 的读写泵操作它
    pub pid: Pid,        // 子进程(PTY 会话首进程): 用于发信号 / 回收
}

/// fork 出一个 stdin/stdout/stderr 都接在 PTY 从端上的登录 shell(zsh)
pub fn spawn_shell(cols: u16, rows: u16) -> Result<ShellChild> {
    let ws = winsize(cols, rows);
    match unsafe { forkpty(Some(&ws), None) }? {
        ForkptyResult::Parent { child, master } => {
            // 主端设为非阻塞, 交给 tokio AsyncFd 驱动
            let flags = OFlag::from_bits_truncate(fcntl(master.as_raw_fd(), FcntlArg::F_GETFL)?);
            fcntl(master.as_raw_fd(), FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
            Ok(ShellChild { master, pid: child })
        }
        ForkptyResult::Child => {
            // forkpty 已做好 setsid + 从端设为控制终端, 直接 exec
            let path = CString::new("/bin/zsh").unwrap();
            let arg0 = CString::new("-zsh").unwrap(); // argv[0] 以 '-' 开头 => login shell
            let _ = execvp(&path, &[&arg0]); // 成功则不返回
            // exec 失败: fork 后的子进程不能跑 exit(3)(会执行父进程的 atexit/析构), 用 _exit 直接走
            unsafe { libc::_exit(127) }
        }
    }
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
    Winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 }
}
