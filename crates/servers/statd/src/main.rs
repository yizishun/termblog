use std::path::PathBuf;

use anyhow::{bail, Result};
use nix::unistd::Uid;
use termblog_config::Config;
use termblog_statd::{server, store::Store};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    if !Uid::effective().is_root() {
        bail!("termblog-statd can only be run as root");
    }
    // Cover SQLite journal/temporary files during both explicit initialization
    // and normal service operation; the durable files are also created 0600.
    unsafe { libc::umask(0o077) };
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let config_path = take_option(&mut args, "--config")?.map(PathBuf::from);
    let config_path =
        config_path.or_else(|| std::env::var("TERMBLOG_CONFIG").ok().map(PathBuf::from));
    let cfg = Config::load(config_path.as_deref())?;
    if args.as_slice() == ["--init"] {
        Store::init(&cfg.stats.data_dir)?;
        println!("Initialized {}", cfg.stats.data_dir.display());
        return Ok(());
    }
    if !args.is_empty() {
        bail!("Usage: termblog-statd [--config FILE] [--init]");
    }
    server::run(cfg.stats).await
}

fn take_option(args: &mut Vec<String>, name: &str) -> Result<Option<String>> {
    let Some(position) = args.iter().position(|arg| arg == name) else {
        return Ok(None);
    };
    if position + 1 >= args.len() {
        bail!("{name} missing value");
    }
    let value = args.remove(position + 1);
    args.remove(position);
    Ok(Some(value))
}
