//! Stable PTY owner. Application clients may come and go without changing its lifetime.
use std::path::PathBuf;

#[tokio::main(worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
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
    muster_terminal_host::serve(runtime, data).await
}
