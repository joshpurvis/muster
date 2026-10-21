//! `muster pr list` and `muster pr snapshot` (docs/CLI.md, "PR snapshots"): read-only GitHub
//! fetches for the PR review flow (skills/muster-pr-review). One command replaces the agent's
//! many sequential `gh` calls: independent calls run in parallel here, without the model, and
//! the JSON fits the `pr_overview` and `pr_decision` panels.
//!
//! Nothing here writes to GitHub. `gh` and `curl` run as child processes with an argument
//! vector, never through a shell; only the validated OWNER/NAME, the PR number and SHAs read
//! from GitHub reach their command lines. PR text (titles, bodies, branch names, file names,
//! URLs) is only ever data in the output. Works without a running daemon.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// Default total diff budget (`--max-diff-bytes`).
pub const DEFAULT_MAX_DIFF: usize = 400_000;
/// One file's diff never exceeds this (a `pr_decision` change's `diff` limit).
pub const MAX_FILE_DIFF: usize = 100_000;
/// Each downloaded picture: at most this many bytes and this long.
pub const MAX_IMAGE_BYTES: usize = 1_500_000;
const IMAGE_TIMEOUT_SECS: u64 = 20;
/// At most this many pictures are downloaded (4 × 1.5 MB stays under pr_decision's 6 MiB).
pub const MAX_IMAGES: usize = 4;
/// pr_decision shows at most this many checks.
const MAX_CHECK_ITEMS: usize = 200;
const GH_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_GH_OUTPUT: usize = 64 << 20;
const MAX_LIST: u64 = 200;
/// At most this many requests to GitHub at once, per command (`--parallel` may lower it).
pub const MAX_PARALLEL: u8 = 4;
/// Retries after GitHub's secondary rate limit, and the longest wait it may ask for.
const MAX_RETRIES: u32 = 3;
const MAX_RETRY_WAIT_SECS: u64 = 120;

#[derive(clap::Subcommand)]
pub enum PrCmd {
    /// Print a repo's open PRs as JSON, newest first (fits the pr_overview panel's cards).
    #[command(visible_alias = "ls", after_help = "Exit codes: 0 ok, 2 bad arguments, 5 gh failed (JSON {error, detail}).")]
    List {
        /// The repository, OWNER/NAME.
        #[arg(long, value_name = "OWNER/NAME")]
        repo: String,
        /// At most this many PRs (1 to 200).
        #[arg(long, value_name = "N", default_value_t = MAX_LIST)]
        limit: u64,
        /// At most N requests to GitHub at once (1 to 4).
        #[arg(long, value_name = "N", default_value_t = MAX_PARALLEL, value_parser = clap::value_parser!(u8).range(1..=4))]
        parallel: u8,
        /// Print each GitHub call's time to stderr.
        #[arg(long)]
        verbose: bool,
    },
    /// Print one PR as one immutable snapshot (diff, checks, reviews, pictures) as JSON.
    #[command(after_help = "Everything describes the head commit read first; the PR is read again at the end.\n\
Exit codes: 0 ok, 2 bad arguments, 3 the head moved or the base branch changed meanwhile (JSON error\n\
head_moved; run it again), 4 the PR is not open (JSON with its state), 5 gh failed (JSON {error, detail}).")]
    Snapshot {
        /// The repository, OWNER/NAME.
        #[arg(long, value_name = "OWNER/NAME")]
        repo: String,
        /// The PR number.
        number: String,
        /// Directory for the PR description's pictures, inside the current directory; must not
        /// exist yet (created 0700). Default: a new `.muster-pr-XXXXXXXX` here.
        #[arg(long, value_name = "DIR")]
        media_dir: Option<PathBuf>,
        /// Total bytes of diff text across all files; bigger diffs are cut (`truncated`).
        #[arg(long, value_name = "N", default_value_t = DEFAULT_MAX_DIFF)]
        max_diff_bytes: usize,
        /// At most N requests to GitHub at once (1 to 4).
        #[arg(long, value_name = "N", default_value_t = MAX_PARALLEL, value_parser = clap::value_parser!(u8).range(1..=4))]
        parallel: u8,
        /// Print each GitHub call's time to stderr.
        #[arg(long)]
        verbose: bool,
    },
}

/// The programs this module runs (tests point them at stubs), the timing switch, and the gate
/// that keeps at most `parallel` requests to GitHub in flight.
pub struct Tools {
    pub gh: OsString,
    pub curl: OsString,
    pub verbose: bool,
    /// One second of a rate-limit wait (tests shorten it).
    pub wait_unit: Duration,
    t0: Instant,
    free: std::sync::Mutex<usize>,
    freed: std::sync::Condvar,
}

/// A slot in [`Tools`]'s gate, given back on drop.
struct Permit<'a>(&'a Tools);

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        *self.0.free.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        self.0.freed.notify_one();
    }
}

impl Tools {
    pub fn new(gh: impl Into<OsString>, curl: impl Into<OsString>, verbose: bool) -> Tools {
        Tools { gh: gh.into(), curl: curl.into(), verbose, wait_unit: Duration::from_secs(1), t0: Instant::now(),
            free: std::sync::Mutex::new(MAX_PARALLEL.into()), freed: std::sync::Condvar::new() }
    }

    /// Lowers the number of requests in flight (never above [`MAX_PARALLEL`]).
    pub fn parallel(self, n: u8) -> Tools {
        *self.free.lock().unwrap_or_else(|e| e.into_inner()) = n.clamp(1, MAX_PARALLEL).into();
        self
    }

    fn permit(&self) -> Permit<'_> {
        let mut free = self.free.lock().unwrap_or_else(|e| e.into_inner());
        while *free == 0 { free = self.freed.wait(free).unwrap_or_else(|e| e.into_inner()); }
        *free -= 1;
        Permit(self)
    }

    fn note(&self, what: &str, started: Instant) {
        if self.verbose {
            eprintln!("muster pr: {what}: {} ms (at {} ms)", started.elapsed().as_millis(), self.t0.elapsed().as_millis());
        }
    }
}

/// What a command prints (stdout JSON) and its exit code; `msg` goes to stderr when it fails.
pub struct Outcome {
    pub code: u8,
    pub json: Option<Value>,
    pub msg: String,
}

impl Outcome {
    fn ok(v: Value) -> Outcome { Outcome { code: 0, json: Some(v), msg: String::new() } }
    fn bad(msg: impl Into<String>) -> Outcome { Outcome { code: 2, json: None, msg: msg.into() } }
}

pub fn run(cmd: PrCmd) -> anyhow::Result<()> {
    let out = match cmd {
        PrCmd::List { repo, limit, parallel, verbose } => list(&Tools::new("gh", "curl", verbose).parallel(parallel), &repo, limit),
        PrCmd::Snapshot { repo, number, media_dir, max_diff_bytes, parallel, verbose } => {
            let cwd = std::env::current_dir()?;
            let tools = Tools::new("gh", "curl", verbose).parallel(parallel);
            let out = snapshot(&tools, &cwd, &SnapshotArgs { repo, number, media_dir, max_diff_bytes });
            tools.note("snapshot total", tools.t0);
            out
        }
    };
    if let Some(v) = &out.json {
        let mut so = std::io::stdout().lock();
        serde_json::to_writer(&mut so, v)?;
        writeln!(so)?;
        so.flush()?;
    }
    if out.code == 0 { Ok(()) } else { Err(crate::cli::Exit { code: out.code, msg: out.msg }.into()) }
}

// ----- arguments -----

/// `OWNER/NAME` (`^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$`, each part at most 100 characters).
pub fn split_repo(repo: &str) -> Option<(&str, &str)> {
    let part = |s: &str| (1..=100).contains(&s.len()) && s.bytes().all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c));
    repo.split_once('/').filter(|(a, b)| part(a) && part(b))
}

/// A PR number: 1 to 2^53, plain digits.
pub fn parse_number(n: &str) -> Option<u64> {
    if n.is_empty() || n.len() > 16 || !n.bytes().all(|c| c.is_ascii_digit()) || n.starts_with('0') { return None }
    n.parse().ok().filter(|n| *n >= 1 && *n <= 1 << 53)
}

/// `--media-dir`, as a path relative to `cwd`: inside it, not existing yet, parent existing.
fn media_dir_arg(cwd: &Path, dir: &Path) -> Result<String, String> {
    let rel = if dir.is_absolute() {
        dir.strip_prefix(cwd).map_err(|_| "--media-dir must be inside the current directory (the panel reads pictures from there)".to_string())?
    } else {
        dir
    };
    let mut parts = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(p) => parts.push(p.to_str().ok_or("--media-dir must be valid UTF-8")?),
            Component::CurDir => {}
            _ => return Err("--media-dir must stay inside the current directory (no `..`)".into()),
        }
    }
    if parts.is_empty() { return Err("--media-dir must name a new directory".into()) }
    let rel = parts.join("/");
    let (parent, last) = parent_at(cwd, &rel)?;
    if exists_at(&parent, &last) { return Err(format!("--media-dir {rel} already exists; give a new directory")) }
    Ok(rel)
}

fn c_name(name: &str) -> std::io::Result<std::ffi::CString> { std::ffi::CString::new(name).map_err(std::io::Error::other) }

/// The current directory, opened once; everything below it is reached through descriptors.
fn open_cwd(cwd: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC).open(cwd)
}

/// Opens directory `name` in `dir`, never through a symlink.
fn open_dir_at(dir: &std::fs::File, name: &str) -> std::io::Result<std::fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let c = c_name(name)?;
    // SAFETY: a valid directory descriptor and NUL-terminated name; the new fd is owned below.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if fd < 0 { return Err(std::io::Error::last_os_error()) }
    // SAFETY: fd was just opened and is owned by nothing else.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

/// Whether anything (a symlink included, even a dangling one) is called `name` in `dir`.
fn exists_at(dir: &std::fs::File, name: &str) -> bool {
    use std::os::fd::AsRawFd;
    let Ok(c) = c_name(name) else { return true };
    // SAFETY: valid descriptor, NUL-terminated name and a zeroed stat buffer to fill.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    unsafe { libc::fstatat(dir.as_raw_fd(), c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) == 0 }
}

/// The parent directory of `rel` (slash-separated, inside `cwd`), reached component by
/// component without following any symlink, and `rel`'s last component. A symlinked parent
/// can't move the picture directory outside the current directory.
fn parent_at(cwd: &Path, rel: &str) -> Result<(std::fs::File, String), String> {
    let mut dir = open_cwd(cwd).map_err(|e| format!("opening the current directory: {e}"))?;
    let parts: Vec<&str> = rel.split('/').collect();
    let (last, parents) = parts.split_last().ok_or("--media-dir must name a new directory")?;
    for (i, p) in parents.iter().enumerate() {
        let at = parts[..=i].join("/");
        dir = open_dir_at(&dir, p).map_err(|e| match e.raw_os_error() {
            Some(libc::ELOOP | libc::ENOTDIR) => format!("--media-dir: {at} is a symlink or not a directory"),
            _ => format!("--media-dir: {at}: {e}"),
        })?;
    }
    Ok((dir, last.to_string()))
}

// ----- running gh and curl -----

struct Captured {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    overflow: bool,
    timed_out: bool,
}

/// Runs `program args` (no shell), keeping at most `max_out` bytes of stdout (more kills it:
/// `overflow`) and killing it after `timeout`. stdin is closed, or gets `input` and is then
/// closed (how secrets reach a child without showing in its argv, `/proc/*/cmdline`).
fn capture(program: &OsStr, args: &[String], input: Option<&[u8]>, timeout: Duration, max_out: usize) -> std::io::Result<Captured> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("NO_COLOR", "1")
        .spawn()?;
    let in_t = match (input, child.stdin.take()) {
        (Some(data), Some(mut sin)) => {
            let data = data.to_vec();
            Some(std::thread::spawn(move || { let _ = sin.write_all(&data); }))
        }
        _ => None,
    };
    let overflow = Arc::new(AtomicBool::new(false));
    let (mut out, mut err) = (child.stdout.take().expect("piped"), child.stderr.take().expect("piped"));
    let flag = overflow.clone();
    let out_t = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut out).take(max_out as u64 + 1).read_to_end(&mut buf);
        if buf.len() > max_out {
            flag.store(true, Ordering::SeqCst);
            buf.truncate(max_out);
        }
        buf
    });
    let err_t = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut err).take(64 * 1024).read_to_end(&mut buf);
        let _ = std::io::copy(&mut err, &mut std::io::sink());
        buf
    });
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        if let Some(s) = child.try_wait()? { break Some(s) }
        if overflow.load(Ordering::SeqCst) || Instant::now() >= deadline {
            timed_out = !overflow.load(Ordering::SeqCst);
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(4));
    };
    let stdout = out_t.join().unwrap_or_default();
    let stderr = err_t.join().unwrap_or_default();
    if let Some(t) = in_t { let _ = t.join(); }
    Ok(Captured { code: status.and_then(|s| s.code()), stdout, stderr, overflow: overflow.load(Ordering::SeqCst), timed_out })
}

/// Why a trigger's `gh` call failed (`Tools::api_get`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GhError {
    /// gh isn't installed.
    Missing,
    /// gh isn't signed in to GitHub.
    NotSignedIn(String),
    Other(String),
}

#[derive(Debug)]
enum GhErr {
    Missing,
    Auth(String),
    NotFound(String),
    Failed(String),
    /// GitHub refused to build the reply because it is too large (HTTP 406/422).
    TooLarge(String),
    /// GitHub's rate limit: the detail, and when it resets (RFC 3339) if GitHub said.
    RateLimited(String, Option<String>),
}

impl GhErr {
    fn outcome(self) -> Outcome {
        let (error, detail) = match self {
            GhErr::Missing => ("gh_missing", "gh (the GitHub CLI) is not installed or not on PATH; install it from https://cli.github.com".to_string()),
            GhErr::Auth(d) => ("gh_auth", format!("gh is not logged in to GitHub: run `gh auth login` ({d})")),
            GhErr::NotFound(d) => ("not_found", d),
            GhErr::Failed(d) | GhErr::TooLarge(d) => ("gh_failed", d),
            GhErr::RateLimited(d, reset_at) => {
                let msg = match &reset_at { Some(r) => format!("{d}; it resets at {r}"), None => d.clone() };
                return Outcome { code: 5, msg, json: Some(json!({"error": "rate_limited", "detail": d, "reset_at": reset_at})) };
            }
        };
        Outcome { code: 5, msg: detail.clone(), json: Some(json!({"error": error, "detail": detail})) }
    }
}

fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

fn classify(what: &str, status: Option<u16>, code: Option<i32>, stderr: &str) -> GhErr {
    let detail = clip(stderr.trim(), 2000);
    let lower = stderr.to_ascii_lowercase();
    if code == Some(4) || stderr.contains("gh auth login") || stderr.contains("HTTP 401") || stderr.contains("Bad credentials") {
        GhErr::Auth(detail)
    } else if stderr.contains("Could not resolve to") || stderr.contains("HTTP 404") {
        GhErr::NotFound(format!("{what}: {detail}"))
    } else if matches!(status, Some(406 | 422)) || stderr.contains("HTTP 406") || stderr.contains("HTTP 422")
        || lower.contains("too large") || lower.contains("too long to generate") || lower.contains("maximum number of") {
        GhErr::TooLarge(format!("{what}: {detail}"))
    } else {
        let code = code.map_or("a signal".to_string(), |c| c.to_string());
        GhErr::Failed(format!("{what}: gh exited with {code}: {detail}"))
    }
}

/// `gh api --include` output: the HTTP status, headers (names lowercase) and body. Output
/// without a status line is taken as the body.
struct Reply {
    status: Option<u16>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn parse(out: Vec<u8>) -> Reply {
        if !out.starts_with(b"HTTP/") { return Reply { status: None, headers: Vec::new(), body: out } }
        let (mut pos, mut status, mut headers) = (0, None, Vec::new());
        for line in out.split_inclusive(|b| *b == b'\n') {
            pos += line.len();
            let text = String::from_utf8_lossy(line);
            let text = text.trim_end_matches(['\r', '\n']);
            if status.is_none() {
                status = text.split_whitespace().nth(1).and_then(|c| c.parse().ok()).or(Some(0));
                continue;
            }
            if text.is_empty() { break }
            if let Some((k, v)) = text.split_once(':') { headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string())); }
        }
        Reply { status, headers, body: out[pos.min(out.len())..].to_vec() }
    }

    fn header(&self, name: &str) -> Option<&str> { self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()) }
}

#[derive(Debug, PartialEq)]
enum Limit {
    /// The hourly allowance is used up: stop, and say when it resets (RFC 3339).
    Primary(Option<String>),
    /// A secondary (burst) limit: wait this many seconds (None: back off) and try again.
    Secondary(Option<u64>),
}

/// Whether a failed reply is GitHub's rate limit, and which one.
fn rate_limit(r: &Reply, stderr: &str) -> Option<Limit> {
    let text = format!("{} {}", String::from_utf8_lossy(&r.body), stderr).to_ascii_lowercase();
    let limited_status = matches!(r.status, Some(403 | 429));
    let secondary = text.contains("secondary rate limit") || text.contains("abuse detection") || r.status == Some(429);
    let retry_after = r.header("retry-after").and_then(|v| v.parse::<u64>().ok());
    if retry_after.is_some() && (limited_status || secondary) { return Some(Limit::Secondary(retry_after)) }
    if secondary { return Some(Limit::Secondary(None)) }
    let exhausted = r.header("x-ratelimit-remaining") == Some("0") && limited_status;
    if exhausted || text.contains("\"type\":\"rate_limited\"") || text.contains("api rate limit exceeded") {
        let reset = r.header("x-ratelimit-reset").and_then(|v| v.parse::<u64>().ok())
            .map(|t| rfc3339(UNIX_EPOCH + Duration::from_secs(t)));
        return Some(Limit::Primary(reset));
    }
    None
}

impl Tools {
    /// One `gh api` call (`args` start with "api"). GitHub's secondary rate limit is waited
    /// out (its Retry-After, else 2, 4, 8 s; at most 3 retries); an exhausted primary limit
    /// stops with its reset time. Each attempt holds a slot of the gate, waits do not.
    fn gh(&self, what: &str, mut args: Vec<String>) -> Result<Vec<u8>, GhErr> {
        args.insert(1, "--include".into());
        let mut attempt = 0;
        loop {
            let started = Instant::now();
            let r = { let _slot = self.permit(); capture(&self.gh, &args, None, GH_TIMEOUT, MAX_GH_OUTPUT) };
            self.note(what, started);
            let c = r.map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { GhErr::Missing } else { GhErr::Failed(format!("running gh: {e}")) })?;
            if c.timed_out { return Err(GhErr::Failed(format!("{what}: gh took longer than {} s", GH_TIMEOUT.as_secs()))) }
            if c.overflow { return Err(GhErr::Failed(format!("{what}: GitHub's reply is larger than {} MiB", MAX_GH_OUTPUT >> 20))) }
            let reply = Reply::parse(c.stdout);
            if c.code == Some(0) { return Ok(reply.body) }
            let stderr = String::from_utf8_lossy(&c.stderr).into_owned();
            match rate_limit(&reply, &stderr) {
                Some(Limit::Primary(reset)) => {
                    return Err(GhErr::RateLimited(format!("{what}: GitHub's API rate limit for this account is used up"), reset));
                }
                Some(Limit::Secondary(after)) => {
                    let wait = after.unwrap_or(2 << attempt);
                    if attempt >= MAX_RETRIES || wait > MAX_RETRY_WAIT_SECS {
                        let reset = (wait > MAX_RETRY_WAIT_SECS).then(|| rfc3339(SystemTime::now() + Duration::from_secs(wait)));
                        return Err(GhErr::RateLimited(format!("{what}: GitHub's secondary rate limit (too many requests at once); try again later"), reset));
                    }
                    eprintln!("muster pr: GitHub asks to slow down ({what}); waiting {wait} s");
                    std::thread::sleep(self.wait_unit * wait as u32);
                    attempt += 1;
                }
                None => return Err(classify(what, reply.status, c.code, &stderr)),
            }
        }
    }

    /// One read-only `gh api <path>` call for the GitHub trigger poller (src/schedules/
    /// triggers.rs): the path is built from a validated OWNER/NAME and fixed query strings.
    pub(crate) fn api_get(&self, what: &str, path: &str) -> Result<Value, GhError> {
        let out = self.gh(what, vec!["api".into(), "--method".into(), "GET".into(), path.into()]).map_err(|e| match e {
            GhErr::Missing => GhError::Missing,
            GhErr::Auth(d) => GhError::NotSignedIn(d),
            GhErr::RateLimited(d, reset) => GhError::Other(match reset { Some(r) => format!("{d}; it resets at {r}"), None => d }),
            GhErr::NotFound(d) | GhErr::Failed(d) | GhErr::TooLarge(d) => GhError::Other(d),
        })?;
        serde_json::from_slice(&out).map_err(|e| GhError::Other(format!("{what}: GitHub's reply is not JSON: {e}")))
    }

    /// One GraphQL query (a constant); variables go as separate `-f`/`-F` arguments, so no value
    /// is ever part of the query text. `-f` never reads files (`@`), `-F` only gets numbers.
    fn graphql(&self, what: &str, query: &str, strings: &[(&str, &str)], ints: &[(&str, u64)]) -> Result<Value, GhErr> {
        let mut args = vec!["api".to_string(), "graphql".into(), "-f".into(), format!("query={query}")];
        for (k, v) in strings { args.extend(["-f".to_string(), format!("{k}={v}")]); }
        for (k, n) in ints { args.extend(["-F".to_string(), format!("{k}={n}")]); }
        let out = self.gh(what, args)?;
        let mut v: Value = serde_json::from_slice(&out).map_err(|e| GhErr::Failed(format!("{what}: GitHub's reply is not JSON: {e}")))?;
        if let Some(errors) = v.get("errors").and_then(Value::as_array).filter(|e| !e.is_empty()) {
            let msg = errors.iter().filter_map(|e| e.get("message").and_then(Value::as_str)).collect::<Vec<_>>().join("; ");
            return Err(if msg.contains("Could not resolve to") { GhErr::NotFound(format!("{what}: {}", clip(&msg, 2000))) }
                else { GhErr::Failed(format!("{what}: {}", clip(&msg, 2000))) });
        }
        Ok(v.get_mut("data").map(Value::take).unwrap_or(Value::Null))
    }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str { v.get(k).and_then(Value::as_str).unwrap_or("") }
fn n(v: &Value, k: &str) -> u64 { v.get(k).and_then(Value::as_u64).unwrap_or(0) }
fn login(v: &Value) -> String {
    v.get("author").and_then(|a| a.get("login")).and_then(Value::as_str).filter(|l| !l.is_empty()).unwrap_or("ghost").to_string()
}

fn sha_ok(sha: &str) -> bool { sha.len() == 40 && sha.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)) }

fn https_url(u: &str) -> Option<&str> {
    (u.starts_with("https://") && u.len() > 8 && u.len() <= 2000 && !u.chars().any(|c| c.is_control() || c.is_whitespace()) && !u[8..].split('/').next().unwrap_or("").contains('@'))
        .then_some(u)
}

// ----- muster pr list -----

/// The open PRs' numbers, newest first: cheap, so the details can be fetched in parallel.
const LIST_Q: &str = "query PrList($owner: String!, $name: String!, $first: Int!, $after: String) {
  repository(owner: $owner, name: $name) {
    pullRequests(states: OPEN, first: $first, after: $after, orderBy: {field: CREATED_AT, direction: DESC}) {
      totalCount
      pageInfo { hasNextPage endCursor }
      nodes { number }
    }
  }
}";

/// What a pr_overview card needs; one batch query asks for up to [`LIST_BATCH`] PRs by number.
const CARD_FRAGMENT: &str = "fragment Card on PullRequest {
  number state title url createdAt isDraft additions deletions changedFiles reviewDecision headRefOid baseRefName
  author { login }
  labels(first: 20) { nodes { name } }
  commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
}";
const LIST_BATCH: usize = 25;

fn rollup_checks(state: Option<&str>) -> &'static str {
    match state {
        Some("SUCCESS") => "passing",
        Some("FAILURE" | "ERROR") => "failing",
        Some(_) => "pending",
        None => "none",
    }
}

fn list_item(p: &Value) -> Value {
    let rollup = p.pointer("/commits/nodes/0/commit/statusCheckRollup/state").and_then(Value::as_str);
    let labels: Vec<&str> = p.pointer("/labels/nodes").and_then(Value::as_array).into_iter().flatten()
        .filter_map(|l| l.get("name").and_then(Value::as_str)).collect();
    json!({
        "number": n(p, "number"), "title": s(p, "title"), "author": login(p), "url": s(p, "url"),
        "created_at": s(p, "createdAt"), "draft": p.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
        "additions": n(p, "additions"), "deletions": n(p, "deletions"), "files": n(p, "changedFiles"),
        "checks": rollup_checks(rollup), "review": s(p, "reviewDecision"), "labels": labels,
        "head_sha": s(p, "headRefOid"), "base": s(p, "baseRefName"),
    })
}

/// One batch of cards by number. The query names only variables (`$n0`, `$n1`, ...): the
/// numbers themselves go as `-F` arguments.
fn list_batch(t: &Tools, owner: &str, name: &str, numbers: &[u64], batch: usize) -> Result<Vec<Value>, GhErr> {
    let decls: String = (0..numbers.len()).map(|i| format!(", $n{i}: Int!")).collect();
    let fields: String = (0..numbers.len()).map(|i| format!("\n    p{i}: pullRequest(number: $n{i}) {{ ...Card }}")).collect();
    let query = format!("query PrListBatch($owner: String!, $name: String!{decls}) {{\n  repository(owner: $owner, name: $name) {{{fields}\n  }}\n}}\n{CARD_FRAGMENT}");
    let names: Vec<String> = (0..numbers.len()).map(|i| format!("n{i}")).collect();
    let ints: Vec<(&str, u64)> = names.iter().map(String::as_str).zip(numbers.iter().copied()).collect();
    let d = t.graphql(&format!("PR details (batch {})", batch + 1), &query, &[("owner", owner), ("name", name)], &ints)?;
    Ok((0..numbers.len())
        .filter_map(|i| d.pointer(&format!("/repository/p{i}")).filter(|p| !p.is_null() && s(p, "state") == "OPEN").map(list_item))
        .collect())
}

pub fn list(t: &Tools, repo: &str, limit: u64) -> Outcome {
    let Some((owner, name)) = split_repo(repo) else { return Outcome::bad(format!("--repo must be OWNER/NAME (got {repo:?})")) };
    if !(1..=MAX_LIST).contains(&limit) { return Outcome::bad(format!("--limit must be 1 to {MAX_LIST}")) }
    // 1. The numbers, newest first (one request per 100).
    let (mut numbers, mut total, mut after) = (Vec::new(), 0u64, None::<String>);
    while (numbers.len() as u64) < limit {
        let first = (limit - numbers.len() as u64).min(100);
        let mut strings = vec![("owner", owner), ("name", name)];
        if let Some(a) = &after { strings.push(("after", a.as_str())); }
        let d = match t.graphql("open PR numbers", LIST_Q, &strings, &[("first", first)]) { Ok(d) => d, Err(e) => return e.outcome() };
        let Some(conn) = d.pointer("/repository/pullRequests").filter(|c| !c.is_null()) else {
            return GhErr::NotFound(format!("repository {repo} not found")).outcome();
        };
        total = n(conn, "totalCount");
        let page: Vec<u64> = conn.get("nodes").and_then(Value::as_array).into_iter().flatten().map(|p| n(p, "number")).filter(|n| *n > 0).collect();
        if page.is_empty() { break }
        numbers.extend(page);
        match conn.pointer("/pageInfo/endCursor").and_then(Value::as_str) {
            Some(c) if conn.pointer("/pageInfo/hasNextPage") == Some(&json!(true)) => after = Some(c.to_string()),
            _ => break,
        }
    }
    numbers.truncate(limit as usize);
    // 2. The cards, in batches of 25 run side by side (the gate keeps at most 4 in flight).
    let batches: Vec<Result<Vec<Value>, GhErr>> = std::thread::scope(|sc| {
        let handles: Vec<_> = numbers.chunks(LIST_BATCH).enumerate().map(|(i, c)| sc.spawn(move || list_batch(t, owner, name, c, i))).collect();
        handles.into_iter().map(|h| h.join().expect("list batch thread")).collect()
    });
    let mut prs = Vec::with_capacity(numbers.len());
    for b in batches {
        match b { Ok(cards) => prs.extend(cards), Err(e) => return e.outcome() }
    }
    let truncated = total > prs.len() as u64;
    Outcome::ok(json!({"repo": repo, "total": total, "truncated": truncated, "prs": prs}))
}

// ----- muster pr snapshot -----

const SNAPSHOT_Q: &str = "query PrSnapshot($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    viewerDefaultMergeMethod squashMergeAllowed mergeCommitAllowed rebaseMergeAllowed deleteBranchOnMerge
    pullRequest(number: $number) {
      number title url state isDraft body bodyHTML mergeable reviewDecision
      author { login }
      baseRefName baseRefOid headRefName headRefOid
      mergedAt closedAt mergeCommit { oid }
      latestReviews(first: 100) { nodes { author { login } state } }
    }
  }
}";

const HEAD_Q: &str = "query PrHead($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) { state headRefOid baseRefName baseRefOid mergedAt closedAt mergeCommit { oid } }
  }
}";

const CHECKS_Q: &str = "query PrChecks($owner: String!, $name: String!, $oid: GitObjectID!, $after: String) {
  repository(owner: $owner, name: $name) {
    object(oid: $oid) {
      ... on Commit {
        statusCheckRollup {
          contexts(first: 100, after: $after) {
            totalCount
            pageInfo { hasNextPage endCursor }
            nodes {
              __typename
              ... on CheckRun { name status conclusion detailsUrl checkSuite { workflowRun { workflow { name } } } }
              ... on StatusContext { context state targetUrl }
            }
          }
        }
      }
    }
  }
}";

pub struct SnapshotArgs {
    pub repo: String,
    pub number: String,
    pub media_dir: Option<PathBuf>,
    pub max_diff_bytes: usize,
}

/// The repo's default merge method: GitHub's own default for this user when it is allowed,
/// else the first allowed of squash, merge, rebase.
fn merge_default(r: &Value) -> &'static str {
    let allowed = |m: &str| match m {
        "squash" => r.get("squashMergeAllowed") != Some(&json!(false)),
        "merge" => r.get("mergeCommitAllowed") != Some(&json!(false)),
        _ => r.get("rebaseMergeAllowed") != Some(&json!(false)),
    };
    let viewer = match s(r, "viewerDefaultMergeMethod") { "SQUASH" => Some("squash"), "MERGE" => Some("merge"), "REBASE" => Some("rebase"), _ => None };
    match viewer.filter(|m| allowed(m)) {
        Some(m) => m,
        None => ["squash", "merge", "rebase"].into_iter().find(|m| allowed(m)).unwrap_or("merge"),
    }
}

pub fn snapshot(t: &Tools, cwd: &Path, a: &SnapshotArgs) -> Outcome {
    let repo = a.repo.as_str();
    let Some((owner, name)) = split_repo(repo) else { return Outcome::bad(format!("--repo must be OWNER/NAME (got {repo:?})")) };
    let Some(number) = parse_number(&a.number) else { return Outcome::bad(format!("the PR number must be a positive whole number (got {:?})", a.number)) };
    let given_dir = match a.media_dir.as_deref().map(|d| media_dir_arg(cwd, d)).transpose() {
        Ok(d) => d,
        Err(m) => return Outcome::bad(m),
    };
    let vars = [("owner", owner), ("name", name)];

    // 1. The PR, its head and base, and the repo's merge settings: one read.
    let first = match t.graphql("PR", SNAPSHOT_Q, &vars, &[("number", number)]) { Ok(d) => d, Err(e) => return e.outcome() };
    let Some(r) = first.get("repository").filter(|r| !r.is_null()) else { return GhErr::NotFound(format!("repository {repo} not found")).outcome() };
    let Some(pr) = r.get("pullRequest").filter(|p| !p.is_null()) else { return GhErr::NotFound(format!("{repo} has no PR #{number}")).outcome() };
    let (head_sha, base_sha, base) = (s(pr, "headRefOid"), s(pr, "baseRefOid"), s(pr, "baseRefName"));
    if !sha_ok(head_sha) || !sha_ok(base_sha) {
        return GhErr::Failed(format!("GitHub returned no usable head or base commit for PR #{number}")).outcome();
    }
    let state = s(pr, "state");
    let mut out = json!({
        "repo": repo, "number": number, "title": s(pr, "title"), "author": login(pr), "url": s(pr, "url"),
        "state": state, "draft": pr.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
        "base": base, "base_sha": base_sha, "head": s(pr, "headRefName"), "head_sha": head_sha,
    });
    if state != "OPEN" {
        closure(&mut out, pr);
        out["fetched_at"] = json!(rfc3339(SystemTime::now()));
        return Outcome { code: 4, json: Some(out), msg: format!("PR #{number} is {}, not open", state.to_lowercase()) };
    }

    // 2. Everything else for exactly head_sha, in parallel: the diff, the checks, the pictures.
    let (refs, mut skipped) = image_refs(s(pr, "body"), s(pr, "bodyHTML"));
    let dir = if refs.is_empty() { None } else {
        match MediaDir::create(cwd, given_dir.as_deref()) {
            Ok(d) => Some(d),
            Err(e) if given_dir.is_some() => return Outcome::bad(e),
            Err(e) => {
                skipped.extend(refs.iter().map(|r| (r.url.clone(), format!("could not create a picture directory: {e}"))));
                None
            }
        }
    };
    let (files, checks, media) = std::thread::scope(|sc| {
        let files = sc.spawn(|| fetch_files(t, repo, base_sha, head_sha));
        let checks = sc.spawn(|| fetch_checks(t, owner, name, head_sha));
        let media = dir.as_ref().map(|d| download_all(t, d, &refs));
        (files.join().expect("diff thread"), checks.join().expect("checks thread"), media)
    });
    let fail = |e: GhErr| {
        if let Some(d) = &dir { d.remove(); }
        e.outcome()
    };
    let (mut files, files_complete) = match files { Ok(f) => f, Err(e) => return fail(e) };
    let checks = match checks { Ok(c) => c, Err(e) => return fail(e) };
    truncate_diffs(&mut files, a.max_diff_bytes);
    let (media, more_skipped) = media.unwrap_or_default();
    skipped.extend(more_skipped);
    let media_dir = dir.as_ref().filter(|_| !media.is_empty()).map(|d| d.rel.clone());
    if let (Some(d), None) = (&dir, &media_dir) { d.remove(); }

    // 3. Read the PR again: the snapshot only stands if the head and base branch did not move.
    let again = match t.graphql("PR again", HEAD_Q, &vars, &[("number", number)]) { Ok(d) => d, Err(e) => return fail(e) };
    let now = again.pointer("/repository/pullRequest").cloned().unwrap_or(Value::Null);
    let (state_now, head_now, base_now) = (s(&now, "state"), s(&now, "headRefOid"), s(&now, "baseRefName"));
    if head_now != head_sha || base_now != base {
        if let Some(d) = &dir { d.remove(); }
        let detail = if head_now != head_sha { format!("the head moved from {head_sha} to {head_now} while reading; run it again") }
            else { format!("the base branch changed from {base} to {base_now} while reading; run it again") };
        return Outcome { code: 3, msg: detail.clone(), json: Some(json!({
            "error": "head_moved", "detail": detail, "head_sha": head_sha, "head_sha_now": head_now, "base": base, "base_now": base_now,
            "state": state_now,
        })) };
    }

    out["mergeable"] = json!(s(pr, "mergeable"));
    out["merge_method_default"] = json!(merge_default(r));
    out["delete_branch_default"] = json!(r.get("deleteBranchOnMerge").and_then(Value::as_bool).unwrap_or(false));
    out["checks"] = json!({"state": checks.state, "items": checks.items});
    out["checks_complete"] = json!(checks.complete);
    if checks.total > MAX_CHECK_ITEMS || !checks.complete { out["checks_total"] = json!(checks.total); }
    let reviews: Vec<Value> = pr.pointer("/latestReviews/nodes").and_then(Value::as_array).into_iter().flatten()
        .map(|r| json!({"author": login(r), "state": s(r, "state")})).collect();
    out["reviews"] = json!({"decision": s(pr, "reviewDecision"), "items": reviews});
    out["body"] = json!(s(pr, "body"));
    out["files"] = Value::Array(files.iter().map(FileDiff::to_json).collect());
    out["files_complete"] = json!(files_complete);
    out["media"] = Value::Array(media);
    out["media_dir"] = json!(media_dir);
    out["media_skipped"] = Value::Array(skipped.into_iter().map(|(url, reason)| json!({"url": clip(&url, 2000), "reason": reason})).collect());
    out["fetched_at"] = json!(rfc3339(SystemTime::now()));
    if state_now != "OPEN" {
        out["state"] = json!(state_now);
        closure(&mut out, &now);
        if let Some(d) = &dir { d.remove(); }
        out["media"] = json!([]);
        out["media_dir"] = Value::Null;
        return Outcome { code: 4, json: Some(out), msg: format!("PR #{number} is {}, not open", state_now.to_lowercase()) };
    }
    Outcome::ok(out)
}

/// What exit 4 adds, from either read of a closed or merged PR.
fn closure(out: &mut Value, pr: &Value) {
    out["merged_at"] = pr.get("mergedAt").cloned().unwrap_or(Value::Null);
    out["merge_commit"] = pr.pointer("/mergeCommit/oid").cloned().unwrap_or(Value::Null);
    out["closed_at"] = pr.get("closedAt").cloned().unwrap_or(Value::Null);
}

// ----- checks -----

fn check_item(c: &Value) -> Value {
    let (name, state, url) = if s(c, "__typename") == "StatusContext" {
        let state = match s(c, "state") { "SUCCESS" => "pass", "FAILURE" | "ERROR" => "fail", _ => "pending" };
        (s(c, "context").to_string(), state, s(c, "targetUrl"))
    } else {
        let state = if s(c, "status") != "COMPLETED" { "pending" } else {
            match s(c, "conclusion") {
                "SUCCESS" => "pass",
                "FAILURE" | "TIMED_OUT" | "ACTION_REQUIRED" | "STARTUP_FAILURE" | "ERROR" => "fail",
                "SKIPPED" | "NEUTRAL" | "STALE" => "skipping",
                "CANCELLED" => "cancel",
                _ => "pending",
            }
        };
        let job = s(c, "name");
        let name = match c.pointer("/checkSuite/workflowRun/workflow/name").and_then(Value::as_str).filter(|w| !w.is_empty()) {
            Some(w) => format!("{w} / {job}"),
            None => job.to_string(),
        };
        (name, state, s(c, "detailsUrl"))
    };
    let name = if name.trim().is_empty() { "unnamed check".to_string() } else { clip(&name, 299) };
    let mut item = json!({"name": name, "state": state});
    if let Some(u) = https_url(url) { item["url"] = json!(u); }
    item
}

/// `failing` if any check fails, else `pending` if any is pending, else `passing`; `none` without
/// checks. From an incomplete list never `passing` (or `none`): `failing` if a failure is
/// already known, else `pending`.
fn checks_state(items: &[Value], complete: bool) -> &'static str {
    let any = |st: &str| items.iter().any(|i| s(i, "state") == st);
    if any("fail") { "failing" } else if !complete || any("pending") { "pending" } else if items.is_empty() { "none" } else { "passing" }
}

/// Pages of 100 checks read at most (so at most this many requests for checks).
const MAX_CHECK_PAGES: usize = 20;

/// The head commit's checks: check runs and commit statuses.
struct Checks {
    /// At most [`MAX_CHECK_ITEMS`]: failing, then pending, cancelled, passing, skipped, in order.
    items: Vec<Value>,
    /// The state over every check read (see [`checks_state`]).
    state: &'static str,
    /// How many checks GitHub has for the commit.
    total: usize,
    /// False when they were not all read (more than [`MAX_CHECK_PAGES`] pages).
    complete: bool,
}

fn fetch_checks(t: &Tools, owner: &str, name: &str, sha: &str) -> Result<Checks, GhErr> {
    let mut items = Vec::new();
    let (mut after, mut total, mut complete) = (None::<String>, 0usize, true);
    for page in 0..MAX_CHECK_PAGES {
        let mut strings = vec![("owner", owner), ("name", name), ("oid", sha)];
        if let Some(a) = &after { strings.push(("after", a.as_str())); }
        let d = t.graphql(&format!("checks (page {})", page + 1), CHECKS_Q, &strings, &[])?;
        let Some(ctx) = d.pointer("/repository/object/statusCheckRollup/contexts").filter(|c| !c.is_null()) else { break };
        total = total.max(n(ctx, "totalCount") as usize);
        items.extend(ctx.get("nodes").and_then(Value::as_array).into_iter().flatten().map(check_item));
        match ctx.pointer("/pageInfo/endCursor").and_then(Value::as_str) {
            Some(c) if ctx.pointer("/pageInfo/hasNextPage") == Some(&json!(true)) => {
                after = Some(c.to_string());
                complete = page + 1 < MAX_CHECK_PAGES;
            }
            _ => break,
        }
    }
    let total = total.max(items.len());
    let complete = complete && items.len() >= total;
    let state = checks_state(&items, complete);
    if items.len() > MAX_CHECK_ITEMS {
        let rank = |i: &Value| ["fail", "pending", "cancel", "pass", "skipping"].iter().position(|st| *st == s(i, "state")).unwrap_or(5);
        let mut idx: Vec<usize> = (0..items.len()).collect();
        idx.sort_by_key(|&i| rank(&items[i]));
        let mut keep: Vec<usize> = idx.into_iter().take(MAX_CHECK_ITEMS).collect();
        keep.sort_unstable();
        items = keep.into_iter().map(|i| items[i].take()).collect();
    }
    Ok(Checks { items, state, total, complete })
}

// ----- the diff -----

#[derive(Debug, Clone, PartialEq)]
pub struct FileDiff {
    pub file: String,
    pub status: &'static str,
    pub previous_file: Option<String>,
    pub additions: u64,
    pub deletions: u64,
    pub binary: bool,
    pub diff: String,
    pub truncated: bool,
    /// Why `diff` has no lines to show: a pure rename, a mode change, an empty file, a binary
    /// file, or text GitHub left out.
    pub note: Option<String>,
}

impl FileDiff {
    fn to_json(&self) -> Value {
        let mut v = json!({"file": self.file, "status": self.status, "additions": self.additions, "deletions": self.deletions,
            "binary": self.binary, "diff": self.diff, "truncated": self.truncated});
        if let Some(p) = &self.previous_file { v["previous_file"] = json!(p); }
        if let Some(note) = &self.note { v["note"] = json!(note); }
        v
    }
}

/// The files changed from the merge base of `base` to `head` (both SHAs), as one unified diff
/// (`compare/base...head`, pinned to the SHAs, unlike `gh pr diff`). Only if GitHub refuses
/// the diff as too large, its JSON form, which lists at most 300 files: `files_complete` is
/// false when that cap may have been hit. Every other error, a rate limit above all, stands.
fn fetch_files(t: &Tools, repo: &str, base: &str, head: &str) -> Result<(Vec<FileDiff>, bool), GhErr> {
    let path = format!("repos/{repo}/compare/{base}...{head}");
    let raw = t.gh("diff", vec!["api".into(), path.clone(), "-H".into(), "Accept: application/vnd.github.diff".into()]);
    match raw {
        Ok(bytes) => Ok((parse_diff(&String::from_utf8_lossy(&bytes)), true)),
        Err(e) if !matches!(e, GhErr::TooLarge(_)) => Err(e),
        Err(_) => {
            let bytes = t.gh("changed files", vec!["api".into(), format!("{path}?per_page=1")])?;
            let v: Value = serde_json::from_slice(&bytes).map_err(|e| GhErr::Failed(format!("changed files: GitHub's reply is not JSON: {e}")))?;
            let files: Vec<FileDiff> = v.get("files").and_then(Value::as_array).into_iter().flatten().map(api_file).collect();
            let complete = files.len() < 300;
            Ok((files, complete))
        }
    }
}

/// What a file without diff lines is, by its status (`None` when it should have lines).
fn structural_note(status: &str, previous: Option<&str>) -> Option<String> {
    let prev = previous.unwrap_or("?");
    match status {
        "renamed" => Some(format!("renamed from {prev}, content unchanged")),
        "copied" => Some(format!("copied from {prev}, content unchanged")),
        "changed" => Some("file mode changed only".into()),
        "added" => Some("new empty file".into()),
        "removed" => Some("deleted empty file".into()),
        _ => None,
    }
}

/// One file of the compare API's JSON. `patch` is missing for binary files and huge diffs, and
/// GitHub doesn't say which; it is also missing when there are no lines (a pure rename, a mode
/// change, an empty file). So `binary` stays false here, a file with changed lines but no
/// patch is `truncated`, and the others get a `note`.
fn api_file(f: &Value) -> FileDiff {
    let file = s(f, "filename").to_string();
    let status = match s(f, "status") { "added" => "added", "removed" => "removed", "renamed" => "renamed", "copied" => "copied", "modified" => "modified", _ => "changed" };
    let previous_file = f.get("previous_filename").and_then(Value::as_str).filter(|p| !p.is_empty()).map(str::to_string);
    let patch = f.get("patch").and_then(Value::as_str).filter(|p| !p.is_empty());
    let changed_lines = n(f, "changes") > 0 || n(f, "additions") > 0 || n(f, "deletions") > 0;
    let (diff, truncated, note) = match patch {
        Some(p) => {
            let old = if status == "added" { "/dev/null".to_string() } else { format!("a/{}", previous_file.as_deref().unwrap_or(&file)) };
            let new = if status == "removed" { "/dev/null".to_string() } else { format!("b/{file}") };
            (format!("--- {old}\n+++ {new}\n{p}{}", if p.ends_with('\n') { "" } else { "\n" }), false, None)
        }
        None if changed_lines => (String::new(), true, Some("GitHub left out this file's diff (too large)".to_string())),
        None => {
            let note = structural_note(status, previous_file.as_deref()).map(|n| match status {
                "added" | "removed" | "modified" => format!("{n} (or a binary file: GitHub's file list doesn't say)"),
                _ => n,
            });
            (String::new(), false, Some(note.unwrap_or_else(|| "no lines changed, or a binary file (GitHub's file list doesn't say)".into())))
        }
    };
    FileDiff { file, status, previous_file, additions: n(f, "additions"), deletions: n(f, "deletions"), binary: false, diff, truncated, note }
}

/// Git's C-style quoting of unusual file names (`"a/caf\303\251"`), undone.
fn unquote(p: &str) -> String {
    let Some(inner) = p.strip_prefix('"').and_then(|p| p.strip_suffix('"')) else { return p.to_string() };
    let (mut out, b, mut i) = (Vec::new(), inner.as_bytes(), 0);
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            i += 1;
            match b[i] {
                b'n' => out.push(b'\n'),
                b't' => out.push(b'\t'),
                b'r' => out.push(b'\r'),
                b'a' => out.push(7),
                b'b' => out.push(8),
                b'f' => out.push(12),
                b'v' => out.push(11),
                d @ b'0'..=b'3' if i + 2 < b.len() && b[i + 1].is_ascii_digit() && b[i + 2].is_ascii_digit() => {
                    out.push((d - b'0') * 64 + (b[i + 1] - b'0') * 8 + (b[i + 2] - b'0'));
                    i += 2;
                }
                c => out.push(c),
            }
        } else {
            out.push(b[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A path from a `---`/`+++` line (None for /dev/null), without its `a/`/`b/` prefix.
fn side_path(rest: &str, prefix: &str) -> Option<String> {
    let rest = rest.trim_end_matches(['\n', '\r']);
    let rest = rest.strip_suffix('\t').unwrap_or(rest);
    if rest == "/dev/null" { return None }
    let p = unquote(rest);
    Some(p.strip_prefix(prefix).map(str::to_string).unwrap_or(p))
}

/// The path from `diff --git a/P b/P` when both sides are the same (binary or mode changes).
fn git_line_path(line: &str) -> Option<String> {
    let rest = line.strip_prefix("diff --git ")?.trim_end_matches(['\n', '\r']);
    if rest.starts_with('"') {
        let end = rest[1..].find("\" ")? + 1;
        return Some(unquote(&rest[..=end]).strip_prefix("a/")?.to_string());
    }
    let len = rest.len().checked_sub(5)? / 2;
    let (a, b) = (rest.get(2..2 + len)?, rest.get(rest.len() - len..)?);
    (rest.starts_with("a/") && a == b).then(|| a.to_string())
}

/// Splits a unified git diff into its files: name, status, line counts, binary, and the text.
pub fn parse_diff(text: &str) -> Vec<FileDiff> {
    let mut chunks: Vec<&str> = Vec::new();
    let mut start = None;
    let mut pos = 0;
    for line in text.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            if let Some(s0) = start { chunks.push(&text[s0..pos]); }
            start = Some(pos);
        }
        pos += line.len();
    }
    if let Some(s0) = start { chunks.push(&text[s0..]); }
    chunks.into_iter().map(parse_file).collect()
}

fn parse_file(chunk: &str) -> FileDiff {
    let mut lines = chunk.split_inclusive('\n');
    let first = lines.next().unwrap_or("");
    let (mut status, mut binary, mut mode, mut in_hunk) = (None, false, false, false);
    let (mut minus, mut plus, mut from, mut to) = (None, None, None, None);
    let (mut additions, mut deletions) = (0, 0);
    for line in lines {
        if in_hunk {
            match line.as_bytes().first() {
                Some(b'+') => additions += 1,
                Some(b'-') => deletions += 1,
                _ => {}
            }
            continue;
        }
        let rest = |p: &str| line.strip_prefix(p).map(|r| unquote(r.trim_end_matches(['\n', '\r'])));
        if line.starts_with("@@") { in_hunk = true }
        else if line.starts_with("new file mode") { status = Some("added") }
        else if line.starts_with("deleted file mode") { status = Some("removed") }
        else if let Some(r) = rest("rename from ") { from = Some(r); status = Some("renamed") }
        else if let Some(r) = rest("rename to ") { to = Some(r) }
        else if let Some(r) = rest("copy from ") { from = Some(r); status = Some("copied") }
        else if let Some(r) = rest("copy to ") { to = Some(r) }
        else if line.starts_with("old mode") || line.starts_with("new mode") { mode = true }
        else if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") { binary = true }
        else if let Some(r) = line.strip_prefix("--- ") { minus = side_path(r, "a/") }
        else if let Some(r) = line.strip_prefix("+++ ") { plus = side_path(r, "b/") }
    }
    let file = plus.clone().or(to.clone()).or(minus.clone()).or_else(|| git_line_path(first)).unwrap_or_default();
    let status = status.unwrap_or(if !in_hunk && !binary && mode { "changed" } else { "modified" });
    let previous_file = matches!(status, "renamed" | "copied").then(|| from.or(minus)).flatten();
    let note = if binary { Some("binary file: no text diff".to_string()) }
        else if in_hunk { None }
        else { structural_note(status, previous_file.as_deref()) };
    FileDiff { file, status, previous_file, additions, deletions, binary, diff: chunk.to_string(), truncated: false, note }
}

/// Cuts the diffs to `budget` bytes in all, fairly: small files stay whole and the big ones
/// share what is left (each at most [`MAX_FILE_DIFF`]), cut at a line end.
pub fn truncate_diffs(files: &mut [FileDiff], budget: usize) {
    let cap = fair_cap(&files.iter().map(|f| f.diff.len().min(MAX_FILE_DIFF)).collect::<Vec<_>>(), budget).min(MAX_FILE_DIFF);
    for f in files.iter_mut() {
        if f.diff.len() <= cap { continue }
        let mut end = cap;
        while !f.diff.is_char_boundary(end) { end -= 1; }
        if let Some(nl) = f.diff[..end].rfind('\n') { end = nl + 1; }
        f.diff.truncate(end);
        f.truncated = true;
    }
}

/// The largest per-file cap `c` with `sum(min(size, c)) <= budget`.
fn fair_cap(sizes: &[usize], budget: usize) -> usize {
    if sizes.iter().sum::<usize>() <= budget { return usize::MAX }
    let mut sorted = sizes.to_vec();
    sorted.sort_unstable();
    let (mut left, mut count) = (budget, sorted.len());
    for size in sorted {
        if size * count <= left {
            left -= size;
            count -= 1;
        } else {
            return left / count;
        }
    }
    usize::MAX
}

// ----- pictures -----

#[derive(Debug, Clone, PartialEq)]
pub struct ImageRef {
    /// As written in the PR body (reported, never the signed URL).
    pub url: String,
    /// What is downloaded: `url`, or GitHub's short-lived signed URL for the same attachment
    /// (from the rendered body), which also works for private repos.
    pub fetch: String,
    pub alt: String,
}

const NOT_GITHUB: &str = "not on GitHub's image hosts (github.com/user-attachments/assets, *.githubusercontent.com)";

/// Only GitHub's own image hosts, in a strict character set: `https://github.com/user-attachments/assets/<id>`
/// and `https://<name>.githubusercontent.com/<path>` (optionally `?jwt=<token>`).
pub fn allowed(url: &str) -> Result<(), &'static str> {
    let Some(rest) = url.strip_prefix("https://") else {
        return Err(if url.starts_with("http://") { "not https" } else { "not an https URL" });
    };
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    if host == "github.com" {
        let id = path.strip_prefix("user-attachments/assets/").ok_or(NOT_GITHUB)?;
        return if (1..=100).contains(&id.len()) && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') { Ok(()) }
            else { Err("not a plain GitHub attachment URL") };
    }
    let sub = host.strip_suffix(".githubusercontent.com").ok_or(NOT_GITHUB)?;
    if sub.is_empty() || !sub.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-') { return Err(NOT_GITHUB) }
    let (p, q) = path.split_once('?').map_or((path, None), |(p, q)| (p, Some(q)));
    if p.is_empty() || p.len() > 2000 || !p.bytes().all(|c| c.is_ascii_alphanumeric() || b"._/-".contains(&c)) || p.split('/').any(|s| s == "..") {
        return Err("the picture's URL has unusual characters");
    }
    if let Some(q) = q {
        let jwt = q.strip_prefix("jwt=").ok_or("the picture's URL has an unusual query")?;
        if jwt.is_empty() || jwt.len() > 4000 || !jwt.bytes().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)) {
            return Err("the picture's URL has an unusual query");
        }
    }
    Ok(())
}

/// The value of attribute `name` in an HTML tag (quoted or not).
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(p) = lower[from..].find(name) {
        let at = from + p;
        from = at + name.len();
        let before_ok = at > 0 && lower.as_bytes()[at - 1].is_ascii_whitespace();
        let rest = lower[at + name.len()..].trim_start();
        if !before_ok || !rest.starts_with('=') { continue }
        let vstart = tag.len() - rest.len() + 1;
        let v = tag[vstart..].trim_start();
        let value = match v.chars().next() {
            Some(q @ ('"' | '\'')) => v[1..].split(q).next().unwrap_or(""),
            _ => v.split(|c: char| c.is_whitespace() || c == '>').next().unwrap_or(""),
        };
        return Some(value.replace("&amp;", "&").replace("&quot;", "\""));
    }
    None
}

/// The pictures in a PR body, in order: Markdown images, `<img>` tags, and bare GitHub
/// attachment links. Allowlisted ones are returned (deduplicated); other picture URLs are
/// listed as skipped with the reason.
pub fn image_refs(body: &str, body_html: &str) -> (Vec<ImageRef>, Vec<(String, String)>) {
    let mut found: Vec<(usize, String, String)> = Vec::new();
    let token = |s: &str| s.split(|c: char| c.is_whitespace() || ")\"'<>]".contains(c)).next().unwrap_or("").to_string();
    // ![alt](url)
    let mut i = 0;
    while let Some(p) = body[i..].find("![") {
        let at = i + p;
        i = at + 2;
        let Some(close) = body[i..].find("](") else { continue };
        let alt = &body[i..i + close];
        if alt.contains('\n') || alt.len() > 1000 { continue }
        let rest = &body[i + close + 2..];
        found.push((at, token(rest.strip_prefix('<').unwrap_or(rest)), alt.to_string()));
    }
    // <img ... src="url" alt="...">
    let lower = body.to_ascii_lowercase();
    let mut i = 0;
    while let Some(p) = lower[i..].find("<img") {
        let at = i + p;
        i = at + 4;
        let end = lower[at..].find('>').map_or(body.len(), |e| at + e);
        let tag = &body[at..end];
        if let Some(src) = attr(tag, "src") { found.push((at, src, attr(tag, "alt").unwrap_or_default())); }
    }
    // Bare attachment links (GitHub's own format for dropped files).
    let mut i = 0;
    while let Some(p) = body[i..].find("https://github.com/user-attachments/assets/") {
        let at = i + p;
        i = at + 8;
        found.push((at, token(&body[at..]).trim_end_matches(['.', ',', ';', ':']).to_string(), String::new()));
    }
    found.sort_by_key(|f| f.0);
    let signed = signed_urls(body_html);
    let (mut refs, mut skipped): (Vec<ImageRef>, Vec<(String, String)>) = (Vec::new(), Vec::new());
    let mut seen: Vec<String> = Vec::new();
    for (_, url, alt) in found {
        if url.is_empty() || seen.contains(&url) { continue }
        seen.push(url.clone());
        if let Err(why) = allowed(&url) {
            skipped.push((url, why.to_string()));
            continue;
        }
        let fetch = url.strip_prefix("https://github.com/user-attachments/assets/")
            .and_then(|id| signed.iter().find(|s| signed_id_matches(s, id)).cloned())
            .unwrap_or_else(|| url.clone());
        let alt = alt.trim();
        let alt = if alt.is_empty() || alt.eq_ignore_ascii_case("image") { format!("Picture {} from the PR description", refs.len() + 1) } else { clip(alt, 999) };
        refs.push(ImageRef { url, fetch, alt });
    }
    (refs, skipped)
}

/// GitHub's signed `private-user-images` URLs in the rendered body (allowlisted ones only).
fn signed_urls(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(p) = html[i..].find("src=\"https://private-user-images.githubusercontent.com/") {
        let start = i + p + 5;
        let end = html[start..].find('"').map_or(html.len(), |e| start + e);
        i = end;
        let url = html[start..end].replace("&amp;", "&");
        if allowed(&url).is_ok() { out.push(url); }
    }
    out
}

/// Whether a signed URL (`.../<n>-<id>.<ext>?jwt=...`) is for attachment `id`.
fn signed_id_matches(signed: &str, id: &str) -> bool {
    let path = signed.split('?').next().unwrap_or("");
    let last = path.rsplit('/').next().unwrap_or("");
    let stem = last.split('.').next().unwrap_or("");
    id.len() >= 8 && stem.ends_with(&format!("-{id}"))
}

/// What the picture is, by its first bytes: PNG, JPEG, GIF or WebP only.
fn sniff(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") { return Some("png") }
    if b.starts_with(&[0xFF, 0xD8, 0xFF]) { return Some("jpg") }
    if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") { return Some("gif") }
    if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" { return Some("webp") }
    None
}

/// A curl config line for `url` (curl's quoting: backslash escapes inside double quotes).
fn curl_config(url: &str) -> String {
    let mut q = String::with_capacity(url.len() + 10);
    for c in url.chars() {
        match c {
            '\\' => q.push_str("\\\\"),
            '"' => q.push_str("\\\""),
            '\n' => q.push_str("\\n"),
            '\r' => q.push_str("\\r"),
            '\t' => q.push_str("\\t"),
            c => q.push(c),
        }
    }
    format!("url = \"{q}\"\n")
}

/// Downloads one picture with curl: https only (redirects too), at most 1.5 MB and 20 s. The
/// URL (for an attachment, GitHub's signed link: a short-lived credential) goes to curl as a
/// config on stdin (`-K -`), never in its argv, and never into an error message. `-q` comes
/// first so no ~/.curlrc is read.
fn download(t: &Tools, url: &str) -> Result<(&'static str, Vec<u8>), String> {
    let args: Vec<String> = ["-q", "-sS", "-f", "-L", "-g", "--proto", "=https", "--proto-redir", "=https", "--max-redirs", "5",
        "--max-time", &IMAGE_TIMEOUT_SECS.to_string(), "--max-filesize", &MAX_IMAGE_BYTES.to_string(), "-K", "-"]
        .iter().map(|a| a.to_string()).collect();
    let started = Instant::now();
    let config = curl_config(url);
    let c = { let _slot = t.permit(); capture(&t.curl, &args, Some(config.as_bytes()), Duration::from_secs(IMAGE_TIMEOUT_SECS + 2), MAX_IMAGE_BYTES) };
    t.note("picture", started);
    let c = c.map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { "curl is not installed".to_string() } else { format!("running curl: {e}") })?;
    if c.overflow { return Err("larger than 1.5 MB".into()) }
    if c.timed_out { return Err(format!("took longer than {IMAGE_TIMEOUT_SECS} s")) }
    match c.code {
        Some(0) => {}
        Some(63) => return Err("larger than 1.5 MB".into()),
        Some(28) => return Err(format!("took longer than {IMAGE_TIMEOUT_SECS} s")),
        Some(1) => return Err("redirected to a URL that is not https".into()),
        Some(47) => return Err("too many redirects".into()),
        Some(22) => return Err(format!("the server refused it ({})", curl_error(&c.stderr, url))),
        code => return Err(format!("download failed (curl exit {})", code.map_or("signal".into(), |c| c.to_string()))),
    }
    let kind = sniff(&c.stdout).ok_or("not a PNG, JPEG, GIF or WebP picture")?;
    Ok((kind, c.stdout))
}

/// curl's error line, without the URL or any query string (a signed link's token).
fn curl_error(stderr: &[u8], url: &str) -> String {
    let text = String::from_utf8_lossy(stderr).replace(url, "<url>");
    let text = text.trim().trim_start_matches("curl: ");
    if text.contains('?') || text.contains("jwt") { return "details withheld".into() }
    clip(text, 200)
}

/// Downloads the first [`MAX_IMAGES`] pictures in parallel into `dir`, in body order.
fn download_all(t: &Tools, dir: &MediaDir, refs: &[ImageRef]) -> (Vec<Value>, Vec<(String, String)>) {
    let take = refs.len().min(MAX_IMAGES);
    let results: Vec<Result<(&'static str, Vec<u8>), String>> = std::thread::scope(|sc| {
        let handles: Vec<_> = refs[..take].iter().map(|r| sc.spawn(move || download(t, &r.fetch))).collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err("download failed".into()))).collect()
    });
    let (mut media, mut skipped) = (Vec::new(), Vec::new());
    for (i, (r, res)) in refs.iter().zip(results).enumerate() {
        match res.and_then(|(ext, bytes)| {
            let file = format!("{}.{ext}", i + 1);
            dir.write(&file, &bytes).map_err(|e| format!("could not save it: {e}"))?;
            Ok(file)
        }) {
            Ok(file) => media.push(json!({"kind": "image", "path": format!("{}/{file}", dir.rel), "alt": r.alt})),
            Err(why) => skipped.push((r.url.clone(), why)),
        }
    }
    for r in &refs[take..] { skipped.push((r.url.clone(), format!("only the first {MAX_IMAGES} pictures are downloaded"))); }
    (media, skipped)
}

/// A fresh private directory for the pictures, inside the current directory. It is created
/// with `mkdirat` in a parent reached without symlinks ([`parent_at`]), and files are created
/// through its descriptor, exclusively and never through a symlink.
struct MediaDir {
    rel: String,
    parent: std::fs::File,
    name: String,
    fd: std::fs::File,
    written: std::sync::Mutex<Vec<String>>,
}

impl MediaDir {
    fn create(cwd: &Path, given: Option<&str>) -> Result<MediaDir, String> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::PermissionsExt;
        let mkdir = |parent: &std::fs::File, name: &str| -> std::io::Result<()> {
            let c = c_name(name)?;
            // SAFETY: valid directory descriptor and NUL-terminated name.
            if unsafe { libc::mkdirat(parent.as_raw_fd(), c.as_ptr(), 0o700) } == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
        };
        let (parent, rel, name) = match given {
            Some(g) => {
                let (parent, last) = parent_at(cwd, g)?;
                mkdir(&parent, &last).map_err(|e| if e.kind() == std::io::ErrorKind::AlreadyExists { format!("{g} already exists") } else { format!("creating {g}: {e}") })?;
                (parent, g.to_string(), last)
            }
            None => {
                let parent = open_cwd(cwd).map_err(|e| format!("opening the current directory: {e}"))?;
                let mut tries = 0;
                let name = loop {
                    let name = format!(".muster-pr-{:08x}", rand::random::<u32>());
                    match mkdir(&parent, &name) {
                        Ok(()) => break name,
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && tries < 8 => tries += 1,
                        Err(e) => return Err(format!("creating {name}: {e}")),
                    }
                };
                (parent, name.clone(), name)
            }
        };
        let fd = open_dir_at(&parent, &name).map_err(|e| format!("opening {rel}: {e}"))?;
        fd.set_permissions(std::fs::Permissions::from_mode(0o700)).map_err(|e| format!("{rel}: {e}"))?;
        Ok(MediaDir { rel, parent, name, fd, written: Default::default() })
    }

    fn write(&self, name: &str, bytes: &[u8]) -> std::io::Result<()> {
        use std::os::fd::{AsRawFd, FromRawFd};
        let c = c_name(name)?;
        // SAFETY: a valid directory descriptor and NUL-terminated name; the new fd is owned below.
        let fd = unsafe {
            libc::openat(self.fd.as_raw_fd(), c.as_ptr(), libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC, 0o600)
        };
        if fd < 0 { return Err(std::io::Error::last_os_error()) }
        self.written.lock().unwrap_or_else(|e| e.into_inner()).push(name.to_string());
        // SAFETY: fd was just opened and is owned by nothing else.
        let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
        f.write_all(bytes)
    }

    /// Removes the files this run wrote, then the directory if it is empty.
    fn remove(&self) {
        use std::os::fd::AsRawFd;
        for name in self.written.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
            if let Ok(c) = std::ffi::CString::new(name) {
                // SAFETY: valid descriptor and NUL-terminated name.
                unsafe { libc::unlinkat(self.fd.as_raw_fd(), c.as_ptr(), 0) };
            }
        }
        if let Ok(c) = c_name(&self.name) {
            // SAFETY: valid descriptor and NUL-terminated name; only an empty directory goes.
            unsafe { libc::unlinkat(self.parent.as_raw_fd(), c.as_ptr(), libc::AT_REMOVEDIR) };
        }
    }
}

// ----- time -----

/// `2026-10-19T06:12:15Z`.
pub fn rfc3339(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_and_number_are_validated() {
        assert_eq!(split_repo("cli/cli"), Some(("cli", "cli")));
        assert_eq!(split_repo("a_b.c-d/e.f"), Some(("a_b.c-d", "e.f")));
        for bad in ["cli", "a/b/c", "a;b/c", "/c", "a/", "a b/c", "a/$(x)", "a/`x`", ""] { assert_eq!(split_repo(bad), None, "{bad}"); }
        assert_eq!(parse_number("812"), Some(812));
        for bad in ["0", "-1", "abc", "08", "1e3", "", "99999999999999999", "#12"] { assert_eq!(parse_number(bad), None, "{bad}"); }
    }

    #[test]
    fn media_dir_never_follows_a_symlink() {
        let root = std::env::temp_dir().join(format!("muster-pr-mlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (cwd, outside) = (root.join("cwd"), root.join("outside"));
        std::fs::create_dir_all(cwd.join("real")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, cwd.join("link")).unwrap();
        std::os::unix::fs::symlink(root.join("nowhere"), cwd.join("dangling")).unwrap();
        // A symlinked parent, a symlink as the directory itself, a dangling one.
        assert!(media_dir_arg(&cwd, Path::new("link/new")).unwrap_err().contains("symlink"));
        assert!(media_dir_arg(&cwd, Path::new("link")).unwrap_err().contains("already exists"));
        assert!(media_dir_arg(&cwd, Path::new("dangling")).unwrap_err().contains("already exists"));
        assert!(MediaDir::create(&cwd, Some("link/new")).is_err());
        assert!(MediaDir::create(&cwd, Some("link")).is_err());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0, "nothing was created outside");
        // A real parent works, and removal takes the files and the directory.
        let d = MediaDir::create(&cwd, Some("real/new")).unwrap();
        d.write("1.png", b"x").unwrap();
        assert!(d.write("1.png", b"y").is_err(), "never replaces a file");
        assert!(cwd.join("real/new/1.png").is_file());
        d.remove();
        assert!(!cwd.join("real/new").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn curl_config_quotes_the_url() {
        assert_eq!(curl_config("https://a/b?jwt=x.y"), "url = \"https://a/b?jwt=x.y\"\n");
        assert_eq!(curl_config("https://a/\"x\\y\nz"), "url = \"https://a/\\\"x\\\\y\\nz\"\n");
        assert_eq!(curl_error(b"curl: (22) The requested URL returned error: 404", "https://a"), "(22) The requested URL returned error: 404");
        assert_eq!(curl_error(b"curl: (22) https://a/b?jwt=SECRET failed", "https://x"), "details withheld");
    }

    #[test]
    fn media_dir_must_be_new_and_inside() {
        let cwd = std::env::temp_dir().join(format!("muster-pr-mdir-{}", std::process::id()));
        std::fs::create_dir_all(cwd.join("exists")).unwrap();
        std::fs::create_dir_all(cwd.join("x")).unwrap();
        assert_eq!(media_dir_arg(&cwd, Path::new("./shots")).unwrap(), "shots");
        assert!(media_dir_arg(&cwd, Path::new("missing/y")).is_err(), "parents must exist");
        assert_eq!(media_dir_arg(&cwd, &cwd.join("x/y")).unwrap(), "x/y");
        assert!(media_dir_arg(&cwd, Path::new("exists")).is_err());
        assert!(media_dir_arg(&cwd, Path::new("../out")).is_err());
        assert!(media_dir_arg(&cwd, Path::new("/tmp/elsewhere")).is_err());
        assert!(media_dir_arg(&cwd, Path::new(".")).is_err());
        std::fs::remove_dir_all(&cwd).unwrap();
    }

    #[test]
    fn only_github_image_hosts_are_allowed() {
        for ok in [
            "https://github.com/user-attachments/assets/e7d69315-2274-4873-a343-7c087c9f8543",
            "https://raw.githubusercontent.com/acme/api/0123abc/docs/shot.png",
            "https://user-images.githubusercontent.com/1/2-a.png",
            "https://private-user-images.githubusercontent.com/1/2-x.png?jwt=eyJ0.eyJp_c-3.6lu",
        ] { assert_eq!(allowed(ok), Ok(()), "{ok}"); }
        for bad in [
            "http://github.com/user-attachments/assets/abc",
            "https://evil.com/a.png",
            "https://github.com/acme/api/raw/main/a.png",
            "https://github.com/user-attachments/assets/abc?x=1",
            "https://github.com.evil.com/user-attachments/assets/abc",
            "https://user@github.com/user-attachments/assets/abc",
            "https://github.com:443/user-attachments/assets/abc",
            "https://evilgithubusercontent.com/a.png",
            "https://a.b.githubusercontent.com/a.png",
            "https://raw.githubusercontent.com/a/$(touch x).png",
            "https://raw.githubusercontent.com/a/../../b.png",
            "https://raw.githubusercontent.com/a.png?token=1",
            "ftp://github.com/user-attachments/assets/abc",
        ] { assert!(allowed(bad).is_err(), "{bad}"); }
    }

    #[test]
    fn image_refs_from_markdown_html_and_bare_links() {
        let body = "Intro ![Login page](https://github.com/user-attachments/assets/aaaaaaaa-1111) and\n\
            <img width=\"10\" alt=\"Dark mode\" src=\"https://raw.githubusercontent.com/a/b/c/d.png\">\n\
            https://github.com/user-attachments/assets/bbbbbbbb-2222.\n\
            ![x](https://evil.example/p.png) ![dup](https://github.com/user-attachments/assets/aaaaaaaa-1111)\n\
            <img src='http://user-images.githubusercontent.com/1.png'> [link](https://evil.example/not-an-image)";
        let html = "<img src=\"https://private-user-images.githubusercontent.com/9/77-bbbbbbbb-2222.png?jwt=abc.def\">";
        let (refs, skipped) = image_refs(body, html);
        let urls: Vec<&str> = refs.iter().map(|r| r.url.as_str()).collect();
        assert_eq!(urls, ["https://github.com/user-attachments/assets/aaaaaaaa-1111", "https://raw.githubusercontent.com/a/b/c/d.png",
            "https://github.com/user-attachments/assets/bbbbbbbb-2222"]);
        assert_eq!(refs[0].alt, "Login page");
        assert_eq!(refs[1].alt, "Dark mode");
        assert_eq!(refs[2].alt, "Picture 3 from the PR description");
        assert_eq!(refs[0].fetch, refs[0].url);
        assert_eq!(refs[2].fetch, "https://private-user-images.githubusercontent.com/9/77-bbbbbbbb-2222.png?jwt=abc.def");
        let bad: Vec<&str> = skipped.iter().map(|s| s.0.as_str()).collect();
        assert_eq!(bad, ["https://evil.example/p.png", "http://user-images.githubusercontent.com/1.png"]);
    }

    #[test]
    fn diff_is_split_into_files() {
        let diff = "diff --git a/src/a.rs b/src/a.rs\nindex 1..2 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n+-- not a header\n ctx\n\
diff --git a/new.txt b/new.txt\nnew file mode 100644\nindex 0..1\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+hi\n\
diff --git a/gone.txt b/gone.txt\ndeleted file mode 100644\n--- a/gone.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\n\
diff --git a/old name.md b/new name.md\nsimilarity index 90%\nrename from old name.md\nrename to new name.md\n\
diff --git a/img.png b/img.png\nindex 1..2 100644\nBinary files a/img.png and b/img.png differ\n\
diff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\n\
diff --git \"a/caf\\303\\251.txt\" \"b/caf\\303\\251.txt\"\n--- \"a/caf\\303\\251.txt\"\n+++ \"b/caf\\303\\251.txt\"\n@@ -1 +1 @@\n-a\n+b\n";
        let files = parse_diff(diff);
        let got: Vec<(&str, &str, Option<&str>, u64, u64, bool)> = files.iter()
            .map(|f| (f.file.as_str(), f.status, f.previous_file.as_deref(), f.additions, f.deletions, f.binary)).collect();
        assert_eq!(got, [
            ("src/a.rs", "modified", None, 2, 1, false),
            ("new.txt", "added", None, 1, 0, false),
            ("gone.txt", "removed", None, 0, 1, false),
            ("new name.md", "renamed", Some("old name.md"), 0, 0, false),
            ("img.png", "modified", None, 0, 0, true),
            ("run.sh", "changed", None, 0, 0, false),
            ("café.txt", "modified", None, 1, 1, false),
        ]);
        assert!(files[0].diff.starts_with("diff --git a/src/a.rs") && files[0].diff.ends_with(" ctx\n"));
        let notes: Vec<Option<&str>> = files.iter().map(|f| f.note.as_deref()).collect();
        assert_eq!(notes, [None, None, None, Some("renamed from old name.md, content unchanged"), Some("binary file: no text diff"),
            Some("file mode changed only"), None]);
        let empty = parse_diff("diff --git a/e.txt b/e.txt\nnew file mode 100644\nindex 0000000..e69de29\n");
        assert_eq!((empty[0].status, empty[0].binary, empty[0].note.as_deref()), ("added", false, Some("new empty file")));
        assert_eq!(files.iter().map(|f| f.diff.len()).sum::<usize>(), diff.len());
    }

    fn fd(name: &str, size: usize) -> FileDiff {
        let line = "+0123456789\n";
        FileDiff { file: name.into(), status: "modified", previous_file: None, additions: 0, deletions: 0, binary: false,
            diff: line.repeat(size / line.len()), truncated: false, note: None }
    }

    #[test]
    fn diffs_are_truncated_fairly_at_line_ends() {
        let mut files = vec![fd("small", 120), fd("big", 12_000), fd("mid", 1_200)];
        truncate_diffs(&mut files, 3_000);
        assert_eq!((files[0].diff.len(), files[0].truncated), (120, false));
        assert_eq!((files[2].diff.len(), files[2].truncated), (1_200, false));
        assert!(files[1].truncated && files[1].diff.len() <= 3_000 - 1_320 && files[1].diff.ends_with('\n'), "{}", files[1].diff.len());
        assert!(files.iter().map(|f| f.diff.len()).sum::<usize>() <= 3_000);
        // Within the budget nothing changes, but one file never exceeds pr_decision's limit.
        let mut files = vec![fd("a", 120)];
        truncate_diffs(&mut files, 1_000);
        assert!(!files[0].truncated);
        let mut files = vec![fd("huge", 240_000)];
        truncate_diffs(&mut files, 10_000_000);
        assert!(files[0].truncated && files[0].diff.len() <= MAX_FILE_DIFF);
        assert_eq!(fair_cap(&[10, 10, 100], 60), 40);
    }

    #[test]
    fn checks_map_to_the_panel_states() {
        let run = |status: &str, conclusion: &str| check_item(&json!({"__typename": "CheckRun", "name": "test", "status": status,
            "conclusion": conclusion, "detailsUrl": "https://ci/1", "checkSuite": {"workflowRun": {"workflow": {"name": "CI"}}}}));
        assert_eq!(run("COMPLETED", "SUCCESS"), json!({"name": "CI / test", "state": "pass", "url": "https://ci/1"}));
        assert_eq!(run("COMPLETED", "TIMED_OUT")["state"], "fail");
        assert_eq!(run("COMPLETED", "NEUTRAL")["state"], "skipping");
        assert_eq!(run("COMPLETED", "CANCELLED")["state"], "cancel");
        assert_eq!(run("IN_PROGRESS", "")["state"], "pending");
        let status = check_item(&json!({"__typename": "StatusContext", "context": "ci/legacy", "state": "ERROR", "targetUrl": "http://x"}));
        assert_eq!(status, json!({"name": "ci/legacy", "state": "fail"}));
        assert_eq!(checks_state(&[], true), "none");
        assert_eq!(checks_state(&[run("COMPLETED", "SUCCESS")], true), "passing");
        assert_eq!(checks_state(&[run("COMPLETED", "SUCCESS"), run("QUEUED", "")], true), "pending");
        assert_eq!(checks_state(&[run("COMPLETED", "FAILURE"), run("QUEUED", "")], true), "failing");
        // An incomplete list is never passing.
        assert_eq!(checks_state(&[run("COMPLETED", "SUCCESS")], false), "pending");
        assert_eq!(checks_state(&[], false), "pending");
        assert_eq!(checks_state(&[run("COMPLETED", "FAILURE")], false), "failing");
        assert_eq!(rollup_checks(Some("EXPECTED")), "pending");
        assert_eq!(rollup_checks(None), "none");
    }

    #[test]
    fn output_fits_the_panels_as_is() {
        let card = list_item(&json!({"number": 815, "state": "OPEN", "title": "Bump x", "url": "https://github.com/acme/api/pull/815",
            "createdAt": "2026-10-18T09:30:00Z", "isDraft": false, "additions": 4, "deletions": 4, "changedFiles": 2, "reviewDecision": null,
            "headRefOid": "0123456789abcdef0123456789abcdef01234567", "baseRefName": "master", "author": null, "labels": {"nodes": [{"name": "deps"}]},
            "commits": {"nodes": []}}));
        crate::panel_apps::check_overview(&json!({"repo": "acme/api", "prs": [card]})).unwrap();
        let items: Vec<Value> = [("COMPLETED", "SUCCESS"), ("IN_PROGRESS", ""), ("COMPLETED", "CANCELLED")].iter()
            .map(|(st, c)| check_item(&json!({"__typename": "CheckRun", "name": "", "status": st, "conclusion": c, "detailsUrl": "http://insecure"})))
            .chain([check_item(&json!({"__typename": "StatusContext", "context": "x".repeat(400), "state": "SUCCESS", "targetUrl": "https://ci/1"}))])
            .collect();
        let mut files = vec![fd("big", 300_000)];
        truncate_diffs(&mut files, DEFAULT_MAX_DIFF);
        crate::panel_apps::check_pr(&json!({"repo": "acme/api", "number": 812, "title": "T", "author": "dana",
            "url": "https://github.com/acme/api/pull/812", "base": "master", "head": "dana/x", "head_sha": "0123456789abcdef0123456789abcdef01234567",
            "summary": "S", "risk": {"level": "low"}, "checks": {"state": checks_state(&items, true), "items": items},
            "reviews": {"decision": "", "items": [{"author": "lee", "state": "APPROVED"}]},
            "merge_method_default": merge_default(&json!({})), "delete_branch_default": false,
            "changes": [{"file": files[0].file, "note": "n", "diff": files[0].diff}]})).unwrap();
    }

    #[test]
    fn merge_defaults_follow_the_repo() {
        assert_eq!(merge_default(&json!({"viewerDefaultMergeMethod": "REBASE"})), "rebase");
        assert_eq!(merge_default(&json!({"viewerDefaultMergeMethod": "MERGE", "mergeCommitAllowed": false})), "squash");
        assert_eq!(merge_default(&json!({"squashMergeAllowed": false, "mergeCommitAllowed": false})), "rebase");
    }

    #[test]
    fn times_are_rfc3339() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(UNIX_EPOCH + Duration::from_secs(1_792_390_335)), "2026-10-19T06:12:15Z");
        assert_eq!(rfc3339(UNIX_EPOCH + Duration::from_secs(951_782_400)), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn rate_limits_are_recognised() {
        let reply = |head: &str, body: &str| Reply::parse(format!("{head}\r\n\r\n{body}").into_bytes());
        let r = reply("HTTP/2.0 200 OK\nX-Ratelimit-Remaining: 4992\r\nX-Ratelimit-Reset: 1792391216", "{\"data\":1}");
        assert_eq!((r.status, r.header("x-ratelimit-remaining"), r.body.as_slice()), (Some(200), Some("4992"), &b"{\"data\":1}"[..]));
        assert_eq!(Reply::parse(b"{}".to_vec()).body, b"{}");
        let r = reply("HTTP/2.0 403 Forbidden\nRetry-After: 7", "{\"message\":\"You have exceeded a secondary rate limit\"}");
        assert_eq!(rate_limit(&r, ""), Some(Limit::Secondary(Some(7))));
        let r = reply("HTTP/2.0 403 Forbidden", "{\"message\":\"You have exceeded a secondary rate limit\"}");
        assert_eq!(rate_limit(&r, ""), Some(Limit::Secondary(None)));
        assert_eq!(rate_limit(&reply("HTTP/2.0 429 Too Many Requests", ""), ""), Some(Limit::Secondary(None)));
        let r = reply("HTTP/2.0 403 Forbidden\nX-Ratelimit-Remaining: 0\nX-Ratelimit-Reset: 1792390335", "{\"message\":\"API rate limit exceeded\"}");
        assert_eq!(rate_limit(&r, ""), Some(Limit::Primary(Some("2026-10-19T06:12:15Z".into()))));
        let r = reply("HTTP/2.0 200 OK\nX-Ratelimit-Remaining: 0", "{\"errors\":[{\"type\":\"RATE_LIMITED\",\"message\":\"API rate limit exceeded\"}]}");
        assert_eq!(rate_limit(&r, ""), Some(Limit::Primary(None)));
        assert_eq!(rate_limit(&reply("HTTP/2.0 404 Not Found\nX-Ratelimit-Remaining: 10", "{}"), "gh: Not Found (HTTP 404)"), None);
    }

    #[test]
    fn the_gate_never_lets_more_than_n_through() {
        let t = Tools::new("gh", "curl", false).parallel(2);
        let (now, peak) = (std::sync::atomic::AtomicUsize::new(0), std::sync::atomic::AtomicUsize::new(0));
        std::thread::scope(|sc| for _ in 0..8 {
            sc.spawn(|| {
                let _slot = t.permit();
                let k = now.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(k, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(20));
                now.fetch_sub(1, Ordering::SeqCst);
            });
        });
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        assert_eq!(*Tools::new("gh", "curl", false).parallel(9).free.lock().unwrap(), 4);
    }

    #[test]
    fn capture_caps_output_and_time() {
        let args = |s: &str| vec!["-c".to_string(), s.to_string()];
        let c = capture(OsStr::new("sh"), &args("head -c 5000 /dev/zero"), None, Duration::from_secs(10), 1000).unwrap();
        assert!(c.overflow && c.stdout.len() == 1000);
        let t0 = Instant::now();
        let c = capture(OsStr::new("sh"), &args("sleep 5"), None, Duration::from_millis(200), 1000).unwrap();
        assert!(c.timed_out && t0.elapsed() < Duration::from_secs(3));
        let c = capture(OsStr::new("sh"), &args("cat"), Some(b"url = \"x\"\n"), Duration::from_secs(10), 1000).unwrap();
        assert_eq!(c.stdout, b"url = \"x\"\n");
        assert!(matches!(capture(OsStr::new("/nonexistent/gh"), &[], None, Duration::from_secs(1), 10), Err(e) if e.kind() == std::io::ErrorKind::NotFound));
    }
}
