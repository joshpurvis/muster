//! Local, content-free performance history. Hot paths only try a bounded memory lock;
//! one worker owns a separate SQLite connection. Failure must never stop terminal I/O.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak, mpsc};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const SERIES_CAP: usize = 512;
const ROW_CAP: usize = 12000;
const BINS: usize = 80;
const DAY: i64 = 86_400_000;
pub const DISK_CAP: u64 = 32 * 1024 * 1024;

fn bound(i: usize) -> f64 { 2f64.powf(i as f64 / 2.0 - 3.0) }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Summary {
    pub count: u64,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
    pub bins: Vec<u64>,
}

impl Summary {
    fn new(v: f64) -> Self {
        let mut s = Self { count: 0, sum: 0.0, min: v, max: v, bins: vec![0; BINS + 1] };
        s.observe(v);
        s
    }
    fn observe(&mut self, v: f64) {
        self.count += 1;
        self.sum += v;
        self.min = self.min.min(v);
        self.max = self.max.max(v);
        let bin = (0..BINS).find(|&i| v <= bound(i)).unwrap_or(BINS);
        self.bins[bin] += 1;
    }
    fn merge(&mut self, other: &Self) {
        self.count += other.count;
        self.sum += other.sum;
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
        for (a, b) in self.bins.iter_mut().zip(&other.bins) { *a += b; }
    }
    fn quantile(&self, q: f64) -> f64 {
        let rank = (self.count as f64 * q).ceil() as u64;
        let mut n = 0;
        for (i, count) in self.bins.iter().enumerate() {
            n += count;
            if n >= rank { return if i == BINS { self.max } else { bound(i).min(self.max) }; }
        }
        self.max
    }
}

type Key = (i64, &'static str, String);
type Batch = BTreeMap<Key, Summary>;
struct Command { method: String, params: Value, reply: mpsc::SyncSender<Result<Value, String>> }

#[derive(Default)]
pub struct Metrics {
    enabled: AtomicBool,
    pending: Mutex<Batch>,
    dropped: AtomicU64,
    write_errors: AtomicU64,
    command: OnceLock<mpsc::SyncSender<Command>>,
}

impl Metrics {
    pub fn enabled(&self) -> bool { self.enabled.load(Ordering::Relaxed) }

    pub fn record(&self, name: &'static str, label: &str, value: f64) {
        if !self.enabled() { return; }
        if !value.is_finite() || !(0.0..=1e12).contains(&value) || label.len() > 100 { return; }
        let Some(mut pending) = self.pending.try_lock() else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if !self.enabled() { return; }
        let key = (crate::model::now_ms() / 60_000 * 60_000, name, label.to_owned());
        if let Some(s) = pending.get_mut(&key) { s.observe(value); }
        else if pending.len() < SERIES_CAP { pending.insert(key, Summary::new(value)); }
        else { self.dropped.fetch_add(1, Ordering::Relaxed); }
    }

    pub async fn command(self: &Arc<Self>, method: &str, params: Value) -> anyhow::Result<Value> {
        let sender = self.command.get().ok_or_else(|| anyhow::anyhow!("metrics worker unavailable"))?.clone();
        let method = method.to_owned();
        tokio::task::spawn_blocking(move || {
            let (reply, rx) = mpsc::sync_channel(1);
            sender.try_send(Command { method, params, reply }).map_err(|_| anyhow::anyhow!("metrics worker busy or unavailable"))?;
            rx.recv_timeout(Duration::from_secs(10))?.map_err(anyhow::Error::msg)
        }).await?
    }

    /// Reports have a fixed vocabulary, bounded size and no free-form text labels.
    pub fn report(&self, body: Value) -> anyhow::Result<()> {
        #[derive(Deserialize)] #[serde(deny_unknown_fields)]
        struct Report { build: String, renderer: String, samples: Vec<Sample> }
        #[derive(Deserialize)] #[serde(deny_unknown_fields)]
        struct Sample { name: String, value: f64 }
        let r: Report = serde_json::from_value(body)?;
        anyhow::ensure!(r.build.len() == 16 && r.build.bytes().all(|b| b.is_ascii_hexdigit()), "invalid UI build");
        anyhow::ensure!(matches!(r.renderer.as_str(), "chromium" | "webkit" | "firefox" | "other"), "invalid renderer");
        anyhow::ensure!(r.samples.len() <= 256, "too many samples");
        let mut checked = Vec::new();
        for s in r.samples {
            let name = match s.name.as_str() {
                "ui.typing_ms" => "ui.typing_ms",
                "ui.typing_idle_ms" => "ui.typing_idle_ms",
                "ui.input_send_ms" => "ui.input_send_ms",
                "ui.echo_wait_ms" => "ui.echo_wait_ms",
                "ui.parse_ms" => "ui.parse_ms",
                "ui.typing_expired" => "ui.typing_expired",
                "ui.typing_overflow" => "ui.typing_overflow",
                "ui.reconnect_ms" => "ui.reconnect_ms",
                "ui.disconnects" => "ui.disconnects",
                "ui.attach_parse_ms" => "ui.attach_parse_ms",
                "ui.attach_failures" => "ui.attach_failures",
                "ui.replay_bytes" => "ui.replay_bytes",
                "ui.visible_terminals" => "ui.visible_terminals",
                "ui.samples_dropped" => "ui.samples_dropped",
                _ => anyhow::bail!("unknown UI metric"),
            };
            anyhow::ensure!(s.value.is_finite() && (0.0..=1e12).contains(&s.value), "invalid value");
            checked.push((name, s.value));
        }
        let label = format!("{}:{}", r.build, r.renderer);
        for (name, value) in checked { self.record(name, &label, value); }
        Ok(())
    }
}

fn open(path: &Path) -> anyhow::Result<Connection> {
    let c = Connection::open(path)?;
    c.busy_timeout(Duration::from_millis(250))?;
    // DELETE journal bounds side files to one transaction; never put a WAL on the input DB.
    c.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA auto_vacuum=INCREMENTAL;
        PRAGMA max_page_count=8192;
        CREATE TABLE IF NOT EXISTS settings (id INTEGER PRIMARY KEY CHECK(id=1), enabled INTEGER NOT NULL);
        INSERT OR IGNORE INTO settings VALUES(1,1);
        CREATE TABLE IF NOT EXISTS runs (id TEXT PRIMARY KEY, started INTEGER NOT NULL, metadata TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS samples (run TEXT NOT NULL, period INTEGER NOT NULL, ts INTEGER NOT NULL,
            name TEXT NOT NULL, label TEXT NOT NULL, summary TEXT NOT NULL,
            PRIMARY KEY(run,period,ts,name,label));")?;
    Ok(c)
}

fn persist(c: &mut Connection, run: &str, metadata: &Value, batch: &Batch, now: i64) -> anyhow::Result<()> {
    let tx = c.transaction()?;
    tx.execute("INSERT OR IGNORE INTO runs VALUES(?1,?2,?3)", params![run, now, metadata.to_string()])?;
    // Retention is applied even if there are no observations. Row cap may shorten history.
    tx.execute("DELETE FROM samples WHERE (period=60 AND ts<?1) OR ts<?2", params![now-7*DAY, now-90*DAY])?;
    tx.execute("DELETE FROM samples WHERE rowid IN (SELECT rowid FROM samples ORDER BY ts DESC LIMIT -1 OFFSET ?1)", [ROW_CAP.saturating_sub(batch.len()*2) as i64])?;
    for ((ts, name, label), summary) in batch {
        for period in [60i64, 3600] {
            let bucket = ts / (period*1000) * (period*1000);
            let old: Option<String> = tx.query_row("SELECT summary FROM samples WHERE run=?1 AND period=?2 AND ts=?3 AND name=?4 AND label=?5",
                params![run, period, bucket, name, label], |r| r.get(0)).optional()?;
            let mut merged = summary.clone();
            if let Some(old) = old { merged.merge(&serde_json::from_str::<Summary>(&old)?); }
            tx.execute("INSERT OR REPLACE INTO samples VALUES(?1,?2,?3,?4,?5,?6)",
                params![run, period, bucket, name, label, serde_json::to_string(&merged)?])?;
        }
    }
    tx.execute("DELETE FROM runs WHERE id NOT IN (SELECT DISTINCT run FROM samples) AND id<>?1", [run])?;
    tx.commit()?;
    c.execute_batch("PRAGMA incremental_vacuum(64)")?;
    Ok(())
}

fn flush(c: &mut Connection, m: &Metrics, run: &str, metadata: &Value) -> anyhow::Result<()> {
    let batch = std::mem::take(&mut *m.pending.lock());
    if let Err(e) = persist(c, run, metadata, &batch, crate::model::now_ms()) {
        m.write_errors.fetch_add(1, Ordering::Relaxed);
        m.dropped.fetch_add(batch.values().map(|s| s.count).sum::<u64>(), Ordering::Relaxed);
        return Err(e);
    }
    Ok(())
}

fn command(c: &mut Connection, m: &Metrics, run: &str, metadata: &Value, cmd: &Command) -> anyhow::Result<Value> {
    match cmd.method.as_str() {
        "metrics.enable" | "metrics.disable" => {
            let enabled = cmd.method == "metrics.enable";
            c.execute("UPDATE settings SET enabled=?1 WHERE id=1", [enabled])?;
            m.enabled.store(enabled, Ordering::Relaxed);
            if !enabled { m.pending.lock().clear(); }
        }
        "metrics.clear" => {
            let mut pending = m.pending.lock();
            c.execute_batch("BEGIN; DELETE FROM samples; DELETE FROM runs; COMMIT; PRAGMA incremental_vacuum;")?;
            pending.clear();
            m.dropped.store(0, Ordering::Relaxed);
            m.write_errors.store(0, Ordering::Relaxed);
        }
        "metrics.status" => { let _ = flush(c, m, run, metadata); }
        "metrics.export" => { flush(c, m, run, metadata)?; }
        _ => anyhow::bail!("unknown metrics command"),
    }
    let mut result = json!({"schema":1, "enabled":m.enabled(), "run":run,
        "dropped_samples":m.dropped.load(Ordering::Relaxed), "persistence_errors":m.write_errors.load(Ordering::Relaxed),
        "database_bytes":c.query_row("SELECT page_count*page_size FROM pragma_page_count(), pragma_page_size()", [], |r| r.get::<_,u64>(0))?,
        "disk_cap_bytes":DISK_CAP, "row_cap":ROW_CAP,
        "minute_retention_days":7,"hour_retention_days":90});
    if cmd.method == "metrics.export" {
        let since = cmd.params.get("since").and_then(Value::as_i64).unwrap_or(0);
        let period = cmd.params.get("period").and_then(Value::as_i64).unwrap_or(60);
        anyhow::ensure!(matches!(period, 60 | 3600), "period must be 60 or 3600 seconds");
        let mut q = c.prepare("SELECT run,period,ts,name,label,summary FROM samples WHERE ts>=?1 AND period=?2 ORDER BY ts,run,name,label")?;
        let rows = q.query_map(params![since,period], |r| Ok((r.get::<_,String>(0)?, r.get::<_,i64>(1)?, r.get::<_,i64>(2)?, r.get::<_,String>(3)?, r.get::<_,String>(4)?, r.get::<_,String>(5)?)))?;
        let mut samples = Vec::new();
        for row in rows {
            let (run, period, ts, name, label, raw) = row?;
            let s: Summary = serde_json::from_str(&raw)?;
            samples.push(json!({"run":run,"period":period,"ts":ts,"name":name,"label":label,
                "p50_upper":s.quantile(0.5),"p95_upper":s.quantile(0.95),"summary":s}));
        }
        let mut q = c.prepare("SELECT id,metadata FROM runs ORDER BY started")?;
        let rows = q.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?)))?;
        let mut runs = serde_json::Map::new();
        for row in rows { let (id, metadata) = row?; runs.insert(id, serde_json::from_str(&metadata)?); }
        result["runs"] = json!(runs);
        result["samples"] = json!(samples);
        result["histogram_upper_bounds"] = json!((0..BINS).map(bound).collect::<Vec<_>>());
    }
    Ok(result)
}

/// Linux process counters exclude agents. PID start ticks prevent a reused PID being sampled.
pub fn process_sample(pid: u32) -> Option<(u64, u64, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<_> = stat.rsplit_once(") ")?.1.split_whitespace().collect();
    let ticks = fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?;
    let start = fields.get(19)?.parse().ok()?;
    let pages = fields.get(21)?.parse::<u64>().ok()?;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    Some((start, ticks, pages * u64::try_from(page_size).ok()?))
}

pub fn start(hub: &Arc<crate::hub::Hub>, path: PathBuf, metadata: Value, host_pid: u32) {
    let m = hub.metrics.clone();
    let weak = Arc::downgrade(hub);
    let (tx, rx) = mpsc::sync_channel(8);
    let _ = m.command.set(tx);
    let result = std::thread::Builder::new().name("muster-metrics".into()).spawn(move || {
        if let Err(e) = worker(&m, weak, &path, metadata, host_pid, rx) {
            m.enabled.store(false, Ordering::Relaxed);
            tracing::warn!("metrics unavailable: {e:#}");
        }
    });
    if let Err(e) = result { tracing::warn!("metrics worker unavailable: {e:#}"); }
}

fn worker(m: &Metrics, hub: Weak<crate::hub::Hub>, path: &Path, metadata: Value, host_pid: u32,
    rx: mpsc::Receiver<Command>) -> anyhow::Result<()> {
    let mut c = open(path)?;
    let configured: bool = c.query_row("SELECT enabled FROM settings WHERE id=1", [], |r| r.get(0))?;
    let forced_off = std::env::var("MUSTER_METRICS").ok().as_deref() == Some("off");
    m.enabled.store(configured && !forced_off, Ordering::Relaxed);
    let run = crate::paths::random_token();
    let mut previous = BTreeMap::new();
    let mut sampled = Instant::now();
    let mut flushed = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(cmd) => {
                let result = if forced_off && cmd.method == "metrics.enable" { Err(anyhow::anyhow!("MUSTER_METRICS=off; restart without this override to enable")) }
                    else { command(&mut c, m, &run, &metadata, &cmd) };
                let _ = cmd.reply.send(result.map_err(|e| format!("{e:#}")));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {},
        }
        let Some(hub) = hub.upgrade() else { break };
        if sampled.elapsed() >= Duration::from_secs(10) {
            if m.enabled() {
                let elapsed = sampled.elapsed().as_secs_f64();
                let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
                for (label, pid) in [("backend", std::process::id()), ("host", host_pid)] {
                    if let Some((start, ticks, rss)) = process_sample(pid) {
                        m.record("process.rss_bytes", label, rss as f64);
                        if let Some((old_start, old_ticks)) = previous.insert(label, (start,ticks)) {
                            if old_start == start && ticks >= old_ticks && hz > 0.0 {
                                m.record("process.cpu_percent", label, (ticks-old_ticks) as f64 / hz / elapsed * 100.0);
                            }
                        }
                    }
                }
                m.record("sessions.active", "", hub.live_session_ids().len() as f64);
                m.record("metrics.observed_seconds", "", elapsed);
                for (label, file) in [("application",crate::paths::db_path()), ("metrics",path.to_path_buf())] {
                    if let Ok(stat) = std::fs::metadata(file) { m.record("database.bytes",label,stat.len() as f64); }
                }
                m.record("metrics.dropped_total", "", m.dropped.load(Ordering::Relaxed) as f64);
                m.record("metrics.write_errors_total", "", m.write_errors.load(Ordering::Relaxed) as f64);
            } else { previous.clear(); }
            sampled = Instant::now();
        }
        if flushed.elapsed() >= Duration::from_secs(15) {
            if let Err(e) = flush(&mut c, m, &run, &metadata) { tracing::warn!("metrics flush failed: {e:#}"); }
            flushed = Instant::now();
        }
    }
    flush(&mut c, m, &run, &metadata)
}

#[derive(clap::Subcommand)]
pub enum Cli {
    /// Collection health and storage usage (JSON).
    Status,
    /// Export minute or hourly summaries with build identities (JSON).
    Export { #[arg(long, default_value_t=0)] since: i64, #[arg(long, default_value_t=60)] period: u32 },
    /// Enable local collection; persists across restarts.
    Enable,
    /// Disable collection; retain existing history.
    Disable,
    /// Delete metric history without changing the collection setting.
    Clear,
}

pub fn cli(d: &crate::paths::Discovery, action: Cli) -> anyhow::Result<()> {
    let (method, params) = match action {
        Cli::Status => ("metrics.status", json!({})),
        Cli::Export { since, period } => ("metrics.export", json!({"since":since,"period":period})),
        Cli::Enable => ("metrics.enable", json!({})), Cli::Disable => ("metrics.disable", json!({})),
        Cli::Clear => ("metrics.clear", json!({})),
    };
    println!("{}", serde_json::to_string_pretty(&crate::client::rpc(d, method, params)?)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database() -> (PathBuf, Connection) {
        let path = std::env::temp_dir().join(format!("muster-metrics-{}.db", crate::paths::random_token()));
        let c = open(&path).unwrap();
        (path, c)
    }
    fn cmd(method: &str) -> Command {
        Command { method: method.into(), params: json!({}), reply: mpsc::sync_channel(1).0 }
    }

    #[test]
    fn rollups_merge_distributions_and_keep_builds_separate() {
        let (path, mut c) = database();
        let now = 100*DAY;
        let a = BTreeMap::from([((now,"latency_ms","".into()),Summary::new(10.0))]);
        let b = BTreeMap::from([((now+60_000,"latency_ms","".into()),Summary::new(90.0))]);
        persist(&mut c,"a",&json!({"backend":"a"}),&a,now).unwrap();
        persist(&mut c,"a",&json!({"backend":"a"}),&b,now+60_000).unwrap();
        persist(&mut c,"b",&json!({"backend":"b"}),&a,now).unwrap();
        let raw: String = c.query_row("SELECT summary FROM samples WHERE run='a' AND period=3600",[],|r|r.get(0)).unwrap();
        let s: Summary = serde_json::from_str(&raw).unwrap();
        assert_eq!((s.count,s.sum,s.min,s.max),(2,100.0,10.0,90.0));
        assert_eq!(s.bins.iter().sum::<u64>(),2);
        assert!(s.quantile(0.5)>=10.0 && s.quantile(0.5)<15.0);
        assert_eq!(s.quantile(0.95),90.0);
        assert_eq!(c.query_row("SELECT count(*) FROM runs",[],|r|r.get::<_,u64>(0)).unwrap(),2);
        persist(&mut c,"a",&json!({}),&BTreeMap::new(),now+8*DAY).unwrap();
        assert_eq!(c.query_row("SELECT count(*) FROM samples WHERE period=60",[],|r|r.get::<_,u64>(0)).unwrap(),0);
        assert_eq!(c.query_row("SELECT count(*) FROM samples WHERE period=3600",[],|r|r.get::<_,u64>(0)).unwrap(),2);
        persist(&mut c,"a",&json!({}),&BTreeMap::new(),now+91*DAY).unwrap();
        assert_eq!(c.query_row("SELECT count(*) FROM samples",[],|r|r.get::<_,u64>(0)).unwrap(),0);
        drop(c); std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn controls_clear_pending_data_and_persist_disable() {
        let (path,mut c)=database();
        let m=Metrics::default();
        command(&mut c,&m,"run",&json!({}),&cmd("metrics.enable")).unwrap();
        m.record("a","",1.0);
        command(&mut c,&m,"run",&json!({}),&cmd("metrics.disable")).unwrap();
        m.record("a","",2.0);
        assert!(m.pending.lock().is_empty());
        assert!(!c.query_row("SELECT enabled FROM settings",[],|r|r.get::<_,bool>(0)).unwrap());
        command(&mut c,&m,"run",&json!({}),&cmd("metrics.enable")).unwrap();
        m.record("a","",3.0);
        let export=command(&mut c,&m,"run",&json!({}),&cmd("metrics.export")).unwrap();
        assert_eq!(export["samples"][0]["summary"]["sum"],3.0);
        m.record("a","",4.0);
        command(&mut c,&m,"run",&json!({}),&cmd("metrics.clear")).unwrap();
        assert!(m.pending.lock().is_empty());
        assert!(command(&mut c,&m,"run",&json!({}),&cmd("metrics.export")).unwrap()["samples"].as_array().unwrap().is_empty());
        command(&mut c,&m,"run",&json!({}),&cmd("metrics.disable")).unwrap();
        drop(c);
        let c=open(&path).unwrap();
        assert!(!c.query_row("SELECT enabled FROM settings",[],|r|r.get::<_,bool>(0)).unwrap());
        drop(c);std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn collection_is_bounded_nonblocking_and_rejects_content() {
        let m=Metrics::default();m.enabled.store(true,Ordering::Relaxed);
        let lock=m.pending.lock();
        m.record("a","",1.0); // Must return, even while the worker holds the lock.
        assert_eq!(m.dropped.load(Ordering::Relaxed),1);
        drop(lock);
        for n in 0..SERIES_CAP+20 { m.record("a",&n.to_string(),1.0); }
        assert_eq!(m.pending.lock().len(),SERIES_CAP);
        assert_eq!(m.dropped.load(Ordering::Relaxed),21);
        m.pending.lock().clear();
        let valid=json!({"build":"0123456789abcdef","renderer":"webkit","samples":[{"name":"ui.typing_ms","value":12.0}]});
        m.report(valid.clone()).unwrap();
        let mut bad=valid.clone();bad["samples"][0]["text"]=json!("secret prompt");
        assert!(m.report(bad).is_err());
        let mut bad=valid.clone();bad["samples"][0]["name"]=json!("secret prompt");
        assert!(m.report(bad).is_err());
        let mut bad=valid;bad["samples"][0]["value"]=json!(-1);
        assert!(m.report(bad).is_err());
        assert_eq!(m.pending.lock().len(),1);
    }

    #[test]
    fn failed_flush_is_atomic_and_accounted_for() {
        let (path,mut c)=database();
        let m=Metrics::default();m.enabled.store(true,Ordering::Relaxed);
        m.record("a","",1.0);
        c.execute_batch("CREATE TRIGGER fail BEFORE INSERT ON samples BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        assert!(flush(&mut c,&m,"run",&json!({})).is_err());
        assert_eq!(m.dropped.load(Ordering::Relaxed),1);
        assert_eq!(m.write_errors.load(Ordering::Relaxed),1);
        assert_eq!(c.query_row("SELECT count(*) FROM samples",[],|r|r.get::<_,u64>(0)).unwrap(),0);
        c.execute_batch("DROP TRIGGER fail").unwrap();
        m.record("a","",2.0);
        flush(&mut c,&m,"run",&json!({})).unwrap();
        assert!(process_sample(std::process::id()).is_some());
        drop(c);std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn storage_cap_prunes_old_rows_and_uses_a_fixed_page_budget() {
        let (path,mut c)=database();
        let now=100*DAY;
        c.execute("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<13000)
            INSERT INTO samples SELECT 'old',3600,?1,'fixture',CAST(x AS TEXT),'{}' FROM n",[now]).unwrap();
        persist(&mut c,"new",&json!({}),&BTreeMap::from([((now,"a","".into()),Summary::new(1.0))]),now).unwrap();
        assert_eq!(c.query_row("SELECT count(*) FROM samples",[],|r|r.get::<_,usize>(0)).unwrap(),ROW_CAP);
        let pages=c.query_row("PRAGMA max_page_count",[],|r|r.get::<_,u64>(0)).unwrap();
        let size=c.query_row("PRAGMA page_size",[],|r|r.get::<_,u64>(0)).unwrap();
        assert_eq!(pages*size,DISK_CAP);
        drop(c);std::fs::remove_file(path).unwrap();
    }
}
