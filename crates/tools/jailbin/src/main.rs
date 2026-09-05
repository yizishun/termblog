//! jailbin —— 装进 jail 模板的访客命令多合一二进制(busybox 式)。
//!
//! 单二进制按 argv[0] 分发: `/usr/local/bin/blog` 与 `/usr/local/bin/webctl`
//! 是指向 jailbin 的符号链接(模板构建时建立), 访客视角与 shell 版完全一致。
//! 未来新命令 = 新模块(文件或目录, 如 blog/ 带自己的私有子模块)+ 模板里
//! 多一条 symlink。

mod blog;
mod play;
mod webctl;

use std::ffi::OsString;
use std::path::Path;

fn main() {
    let mut args = std::env::args_os();
    let argv0 = args.next().unwrap_or_else(|| OsString::from("jailbin"));
    let rest: Vec<String> = args.map(|a| a.to_string_lossy().into_owned()).collect();
    let prog = Path::new(&argv0)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "jailbin".into());
    let code = match prog.as_str() {
        "blog" => blog::run(&rest),
        "play" => play::run(&rest),
        "webctl" => webctl::run(&rest),
        _ => {
            eprintln!("jailbin: Usage: blog [file] | play [options] [cast] | webctl url /path");
            2
        }
    };
    if code != 0 {
        std::process::exit(code);
    }
}
