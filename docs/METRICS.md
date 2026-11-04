# Local metrics and evals

Muster records content-free performance summaries in `$MUSTER_DATA_DIR/metrics.db`.
Collection is enabled by default. Nothing is uploaded. Terminal contents, keys, prompts,
agent messages, project names, paths and request parameters are not recorded here.

```sh
muster metrics status
muster metrics export > /tmp/muster-history.json
muster metrics export --period 3600 > /tmp/muster-hourly.json
muster metrics export --since 1793491200000  # Unix milliseconds, inclusive bucket start
muster metrics disable
muster metrics enable
muster metrics clear
```

Commands require a running backend; they do not start one. Disable persists across restarts
and retains history. Clear removes history and pending samples but leaves collection enabled
if it was enabled; disable first to keep history empty. `MUSTER_METRICS=off` forces collection
off for that backend's lifetime. Desktop windows learn setting changes within 15 seconds while
connected; the backend stops accepting observations immediately.

## Storage and interpretation

- Hot paths only try a bounded memory lock, never wait for the collector or write to SQLite.
  At most 512 metric/bucket/label combinations wait in memory. Contention and overflow count
  as dropped observations. A worker flushes every 15 seconds; export flushes first.
- Minute buckets last up to seven days; hour buckets up to ninety days. A 12,000-row cap can
  shorten these windows. The separate database has a 32 MiB page budget, plus a temporary
  transaction-sized rollback journal. Freed pages are reused and incrementally reclaimed.
- Failed flushes discard the batch and increment persistence-error and lost-observation
  counters. Status exposes counters even if writes fail. Export reports a flush failure.
  A hard process crash can lose the pending interval. This is not an accounting ledger.
- CPU and RSS are sampled every ten seconds for the backend and host separately. Agents and
  browser renderer processes are excluded. Process start time guards against PID reuse.
  CPU is percent of one logical CPU; multithreaded work can exceed 100%.
- Each backend run records full backend/running-host hashes, build mode, OS, architecture and
  CPU count. UI samples carry their own stamped bundle hash and browser family, so an old
  open window remains distinguishable after an update.
- Collection unavailable at startup is logged; commands return an unavailable error. Terminal
  work continues. The control worker remains available while collection is disabled.

Exports include mergeable histograms, count, sum, minimum, maximum and approximate p50/p95
upper bounds. Boundaries increase by √2. Merge histogram counts before calculating an overall
percentile; never average percentiles. Minute and hour series cover the same observations;
do not add them together. Counter-style series use `sum`; sampled gauges use `sum/count`,
maximum or their distribution. Missing observations are not zero latency/failures. Keep
build/run boundaries visible when comparing history.

## Measurements

| Metric | Meaning |
|---|---|
| `rpc.duration_ms`, `rpc.calls`, `rpc.errors` | Shared RPC dispatch entry to result, by fixed method family. Excludes socket queuing, connection-scoped attach, binary input and metrics calls. Long waits are included: compare like families/workloads. |
| `detector.cycle_ms` | Full detector iteration, including screen reads and progress persistence. |
| `process.rss_bytes`, `process.cpu_percent` | Ten-second backend/host samples. RSS includes shared pages in each process. |
| `database.bytes` | Application/metrics main-file sizes, excluding journals, logs, snapshots and worktrees. |
| `sessions.active` | Live sessions at resource sampling time. |
| `metrics.observed_seconds` | Sampled enabled time; coverage, not exact crash-safe uptime. |
| `metrics.dropped_total`, `metrics.write_errors_total` | Cumulative within this backend run: use changes, not sums. Also in status. |
| `ws.connections`, `ws.lagged_events`, `ws.resyncs` | Authenticated connections and event-bus lag; includes CLI/test clients. |
| `input.failures` | Binary terminal-input errors observed by the backend. |
| `terminal.attach_ms`, `terminal.attach_failures` | Attach to backend replay acquisition, or failure. Latencies exclude failures. |
| `terminal.replay_bytes` | Replay payload queued to a client on attach. |
| `ui.typing_ms`, `ui.typing_idle_ms` | Visible keydown to next animation frame after subsequent output was parsed/rendered. Idle means at least one second since the prior key. Unrelated output can satisfy this approximate probe. |
| `ui.input_send_ms`, `ui.echo_wait_ms`, `ui.parse_ms` | Time to WebSocket send, subsequent output arrival, and xterm parse completion. Passive “echo” is not verified. |
| `ui.typing_expired` | Sent keys with no complete sample within three seconds; includes legitimate no-echo input. Shortcuts sending no bytes are excluded. |
| `ui.typing_overflow` | Keys evicted from the bounded 256-key-per-terminal matcher while output falls behind. |
| `ui.disconnects`, `ui.reconnect_ms` | Ready socket closes; close-to-successful-hello time when visible at reconnection. Does not imply all terminals have replayed. |
| `ui.attach_parse_ms`, `ui.attach_failures` | Attach/reattach initiation to replay parse completion, or rejected attach. Not screen presentation time. |
| `ui.replay_bytes`, `ui.visible_terminals` | Received replay size and mounted terminal count. |
| `ui.samples_dropped` | Frontend buffer overflow or samples discarded during failed/unavailable sends. |

Frontend batches contain at most 256 observations per 15 seconds. Busy typing can exceed
that budget; inspect dropped counts. UI observations are assigned the backend receive-minute
and do not survive a page crash. Passive collection has no permanent animation-frame loop.
The intrusive diagnostic typing overlay's samples are excluded from passive typing history.

This milestone does not persist mobile telemetry, native freeze/crash events, database lock
timing or GPU/render-mode labels. Labeled agent-state accuracy evals and browser cache-eviction
evals remain future work; the existing watchdogs and correctness tests still operate.

## Repeatable evals

Requires Python 3, Deno, Chromium and the Rust workspace build dependencies.

```sh
deno task eval --output /tmp/muster-baseline.json
# Make a change, then repeat on the same machine:
deno task eval --output /tmp/muster-candidate.json
deno task eval:compare /tmp/muster-baseline.json /tmp/muster-candidate.json

# Collector overhead, identical workloads:
deno task eval --collection off --output /tmp/muster-off.json
deno task eval --collection on --output /tmp/muster-on.json
deno task eval:compare /tmp/muster-off.json /tmp/muster-on.json

# Short development check, using an already-built debug workspace:
python3 scripts/eval.py --binary target/debug/muster --sessions 1 --keys 10 --repeats 1
```

`eval` builds the release workspace and production UI. Each fixture has its own temporary
runtime/data/home directories, port and Chromium profile. It launches only shell/Python
fixtures with a small explicit environment and no agent credentials. Cleanup stops only its
own backend and private host; it never calls the real daemon. Logs/exports remain under
`/tmp/muster-eval-*` for diagnosis.

The default suite repeats three times at one, four and twelve active sessions:

1. Controlled character echo, quiet and with background output from the other sessions.
   A unique sequence marker must render before the timing completes. Each tenth key follows
   a one-second pause. Timeouts fail the run and stay in the report, never silently excluded.
2. Backend termination/restart while the terminal survives, with output written during
   downtime. Recovery requires the same PID and exactly the expected output sequence.
   Timing includes shutdown, startup and fixture orchestration; compare like for like.
3. Five create/archive cycles, measuring retained backend/host RSS after settling. This is a
   leak indicator, not proof: allocator caching and background work also retain memory.
4. Real frontend-to-backend metric export, or verification that disabled collection recorded
   no samples.

Reports retain raw samples, correctness checks, binary/UI identities, browser version,
hardware, kernel and build mode. Comparison rejects incompatible environments/workloads
and failed runs. It prints p95 changes and counts, without automatic performance gates until
repeated runs establish noise and sensible budgets. Three first-key/recovery samples do not
produce a stable p95.

The harness uses **headless Chromium with software rendering**; these numbers do not measure
native WebKit or GPU performance. Establish baselines without concurrent builds/heavy work.
Use passive native-window history to identify a problem, then add a focused reproducible eval
before optimizing it.
