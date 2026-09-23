mod agents;
mod detect;
mod hookcmd;
mod hub;
mod inject;
mod mcp;
mod model;
mod notify;
mod worktree;

fn main() {
    // OWNER: core implementer (CLI: gui/daemon/hook/status/stop/url/open).
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("hook") {
        std::process::exit(hookcmd::run(
            args.get(2).map(String::as_str).unwrap_or(""),
            args.get(3).map(String::as_str).unwrap_or(""),
            args.get(4).cloned(),
        ));
    }
}
