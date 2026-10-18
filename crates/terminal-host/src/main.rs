//! Stable PTY owner. Application clients may come and go without changing its lifetime.
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    // Before creating a runtime or logger: the terminal bootstrap only waits
    // for its ownership checkpoint and execs the requested program.
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments.first().is_some_and(|arg| arg == "--owned-child") {
        anyhow::ensure!(arguments.len() >= 5, "invalid owned-child arguments");
        let parent: u32 = arguments[1]
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("invalid parent PID"))?
            .parse()?;
        return muster_terminal_host::launch_owned_child(
            parent,
            &PathBuf::from(&arguments[2]),
            &PathBuf::from(&arguments[3]),
            &arguments[4],
            &arguments[5..],
        );
    }
    tracing_subscriber::fmt()
        .with_env_filter("muster_terminal_host=info")
        .init();
    let mut args = std::env::args_os().skip(1);
    let (mut runtime, mut data) = (None, None);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--runtime-dir") => runtime = args.next().map(PathBuf::from),
            Some("--data-dir") => data = args.next().map(PathBuf::from),
            _ => anyhow::bail!("usage: muster-terminal-host --runtime-dir DIR --data-dir DIR"),
        }
    }
    let runtime = runtime.ok_or_else(|| anyhow::anyhow!("--runtime-dir is required"))?;
    let data = data.ok_or_else(|| anyhow::anyhow!("--data-dir is required"))?;
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(muster_terminal_host::serve(runtime, data))
}
