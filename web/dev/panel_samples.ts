// Sample inputs for the built-in panel apps, used by the mock e2e (dev/panel_apps_e2e.ts) and
// handy for trying an app by hand: `muster panel open --app chart --data "$(...)"`.

export const HEAD_SHA = "3f9c2b7e41d0a8c6e5f4b3a2918d7c6b5a493827";

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
