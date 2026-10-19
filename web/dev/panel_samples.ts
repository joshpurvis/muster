// Sample inputs for the built-in panel apps, used by the mock e2e (dev/panel_apps_e2e.ts) and
// handy for trying an app by hand: `muster panel open --app chart --data "$(...)"`.

export const HEAD_SHA = "3f9c2b7e41d0a8c6e5f4b3a2918d7c6b5a493827";
/** The quick-win card's head (#815 in overviewSample). */
export const QUICK_SHA = "8a1d0c9b7e6f5a4b3c2d1e0f9a8b7c6d5e4f3a21";

export const prSample = {
  repo: "acme/api",
  number: 812,
  title: "Rate-limit failed logins per IP",
  author: "dana",
  url: "https://github.com/acme/api/pull/812",
  base: "master",
  head: "dana/login-limit",
  head_sha: HEAD_SHA,
  merge_method_default: "squash",
  delete_branch_default: true,
  verdict: "Worth a careful look: it changes how login works, but the tests pass.",
  undo: "If this goes wrong: revert the merge commit; no data needs fixing.",
  summary:
    "Adds a sliding-window limit of 5 failed logins per IP per 10 minutes to POST /login, returning 429 with Retry-After. Successful logins reset the window. The limiter lives in a new module and reuses the existing Redis pool.",
  risk: {
    level: "medium",
    reasons: [
      "Changes the login path: a bug here locks real users out.",
      "Behind a proxy every client shares one IP unless X-Forwarded-For is trusted; the PR trusts it only from the configured proxy list.",
    ],
  },
  checks: {
    state: "passing",
    items: [
      { name: "test (ubuntu)", state: "pass", url: "https://github.com/acme/api/actions/runs/1" },
      { name: "lint", state: "pass" },
      { name: "deploy-preview", state: "skipping" },
    ],
  },
  reviews: { decision: "REVIEW_REQUIRED", items: [{ author: "lee", state: "COMMENTED" }] },
  changes: [
    {
      file: "src/auth/login.rs",
      note: "The limit is checked before the password hash, so a locked-out IP costs no bcrypt time. The reset on success is the line to look at.",
      diff: "@@ -41,9 +41,16 @@ pub async fn login(State(app): State<App>, req: LoginRequest) -> Response {\n     let ip = client_ip(&req, &app.config.trusted_proxies);\n+    if let Some(wait) = app.limiter.blocked(ip).await? {\n+        return too_many(wait);\n+    }\n     let user = match app.users.find(&req.email).await? {\n         Some(u) => u,\n-        None => return unauthorized(),\n+        None => { app.limiter.fail(ip).await?; return unauthorized() }\n     };\n     if !verify(&req.password, &user.hash)? {\n-        return unauthorized();\n+        app.limiter.fail(ip).await?;\n+        return unauthorized();\n     }\n+    app.limiter.reset(ip).await?;\n     session::start(&app, user).await",
    },
    {
      file: "src/auth/limiter.rs",
      note: "New: a Redis sorted set per IP, trimmed to the window on every call.",
      diff: "@@ -0,0 +1,8 @@\n+pub struct Limiter { redis: Pool, max: usize, window: Duration }\n+\n+impl Limiter {\n+    pub async fn blocked(&self, ip: IpAddr) -> Result<Option<Duration>> {\n+        let key = format!(\"login-fail:{ip}\");\n+        let count = self.redis.zcount_since(&key, self.window).await?;\n+        Ok((count >= self.max).then(|| self.window))\n+    }",
    },
  ],
  rest_note: "Not shown: 64 lines of tests in tests/login_limit.rs and a config default.",
  questions: ["Is 5 attempts per 10 minutes what support wants, or should it match the old 10 per hour?"],
};

/** The agent's questions in the new form, answered in place (and the old string form above). */
export const prQuestionsSample = {
  ...prSample,
  questions: [
    { id: "limit", text: "Keep the limit at 5 failed logins per 10 minutes?", why: "Support asked for 10 per hour last year; 5 is stricter.",
      kind: "yes_no", recommended: "yes" },
    { id: "proxy", text: "Which addresses should count as one visitor?", why: "Behind a proxy, everyone can look like one visitor.", kind: "choice",
      options: [{ label: "Trust only our own proxies", value: "ours" }, { label: "Trust any proxy", value: "any" }, { label: "Ignore proxies", value: "none" }],
      recommended: "ours" },
    { id: "note", text: "Anything to tell the author?", kind: "text" },
  ],
};

export const chartSample = {
  type: "bar",
  title: "CI minutes per day",
  x: { label: "Day", values: ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"] },
  series: [
    { name: "master", values: [42, 55, 38, 61, 70, 12, 9] },
    { name: "PRs", values: [80, 96, 73, 110, 124, 20, null] },
  ],
  y: { label: "Minutes" },
};

export const lineSample = {
  type: "line",
  title: "p95 login latency",
  x: { label: "Build", values: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10] },
  series: [
    { name: "before", values: [120, 118, 125, 131, 122, 119, 140, 135, 128, 126] },
    { name: "after", values: [118, 117, 121, 119, null, 116, 118, 117, 115, 116] },
  ],
  y: { label: "ms" },
};

export const tableSample = {
  title: "Slowest tests",
  columns: [
    { key: "test", label: "Test" },
    { key: "file", label: "File" },
    { key: "seconds", label: "Seconds" },
    { key: "runs", label: "Runs" },
    { key: "flaky", label: "Flaky", align: "center" },
  ],
  rows: [
    { test: "login_limit_resets_on_success", file: "tests/login_limit.rs", seconds: 4.82, runs: 120, flaky: false },
    { test: "session_survives_restart", file: "tests/session.rs", seconds: 12.4, runs: 118, flaky: true },
    { test: "oauth_callback_rejects_state", file: "tests/oauth.rs", seconds: 0.91, runs: 120, flaky: false },
    { test: "export_large_csv", file: "tests/export.rs", seconds: 27.05, runs: 64, flaky: false },
    { test: "webhook_retries_with_backoff", file: "tests/webhooks.rs", seconds: 9.3, runs: 120, flaky: true },
    { test: "admin_can_unlock_ip", file: "tests/login_limit.rs", seconds: 2.2, runs: 120, flaky: false },
  ],
  sort: { key: "seconds", dir: "desc" },
};

/** The user's own actions, as an agent adds them when asked for "a Needs QA button". */
export const actionsSample = [
  { id: "needs-qa", label: "Needs QA", key: "q", description: "Add the needs-qa label to this PR on GitHub.", outward: true },
  { id: "later", label: "Label later", key: "l", description: "Note this PR in your list to label later; nothing changes on GitHub.", outward: false },
];

export const overviewSample = {
  repo: "acme/api",
  prs: [
    {
      number: 812, title: "Rate-limit failed logins per IP", author: "dana", url: "https://github.com/acme/api/pull/812",
      created_at: "2026-10-15T09:00:00Z", additions: 182, deletions: 12, files: 6, checks: "passing", review: "REVIEW_REQUIRED",
      labels: ["security"], risk: "medium", head_sha: HEAD_SHA,
      summary: "Adds a per-IP limit of 5 failed logins per 10 minutes to POST /login; 429 with Retry-After.",
    },
    {
      number: 815, title: "Fix a typo in the README", author: "lee", url: "https://github.com/acme/api/pull/815",
      created_at: "2026-10-18T16:00:00Z", additions: 1, deletions: 1, files: 1, checks: "passing", review: "APPROVED",
      labels: ["docs"], risk: "low", summary: "One word in the install section.", base: "master",
      quick_win: true, status: "ready", head_sha: QUICK_SHA, verdict: "Safe to merge: one word in the docs, checks pass.",
      delete_branch_default: true,
    },
    {
      number: 801, title: "Rewrite the session store on Redis streams", author: "sam", url: "https://github.com/acme/api/pull/801",
      created_at: "2026-10-01T10:00:00Z", additions: 940, deletions: 455, files: 31, checks: "failing", review: "",
      labels: ["refactor", "needs-design"], risk: "high", draft: true,
      summary: "Replaces the session table with Redis streams; migrations for existing sessions are not in yet.",
    },
    {
      number: 820, title: "Bump serde to 1.0.228", author: "renovate", url: "https://github.com/acme/api/pull/820",
      created_at: "2026-10-19T07:30:00Z", additions: 6, deletions: 6, files: 2, checks: "pending", risk: "low",
      summary: "Dependency bump; lockfile only.",
    },
  ],
  actions: actionsSample,
};

/** prSample with pictures and a chart before the diffs (the path is a file in this repo). */
export const prMediaSample = {
  ...prSample,
  media: [
    { kind: "image", path: "web/m/icon-512.png", alt: "Login page after the change", caption: "429 page shown after 5 failed logins" },
    { kind: "image", path: "web/m/icon-192.png", alt: "Retry-After countdown", caption: "The countdown on the login form" },
    {
      kind: "chart", caption: "Lines changed per area",
      spec: { type: "bar", x: { label: "Area", values: ["src/auth", "tests", "config"] },
        series: [{ name: "added", values: [124, 64, 2] }, { name: "removed", values: [9, 0, 3] }], y: { label: "Lines" } },
    },
  ],
  actions: actionsSample,
};
