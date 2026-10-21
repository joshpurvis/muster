//! Running agents on OpenRouter (pay per token) instead of the CLIs' own subscription login.
//!
//! The key is the owner's own OpenRouter API key, not a CLI credential (PLAN.md §3): it comes
//! from the daemon's `OPENROUTER_API_KEY` environment variable, read once when the daemon starts
//! ([`take_key_from_env`], which also removes it from the daemon's own environment so no other
//! child process inherits it). It is held only in memory ([`ApiKey`] never prints its value) and
//! goes only into the environment of sessions of agents set to run on OpenRouter
//! ([`apply_session_env`]). Clients only learn whether it is there (`hello.openrouter`). The
//! launch record Muster keeps for each run has the value replaced by a placeholder
//! ([`redact_spec`]), put back only when that launch is re-sent to the terminal host.
//!
//! How each CLI is pointed at OpenRouter (checked 2026-10-20):
//! - Claude Code: `ANTHROPIC_BASE_URL=https://openrouter.ai/api`, `ANTHROPIC_AUTH_TOKEN=<key>`
//!   and `ANTHROPIC_API_KEY` set empty
//!   (<https://openrouter.ai/docs/guides/guides/claude-code-integration>). The auth token wins
//!   over a subscription login (<https://code.claude.com/docs/en/authentication>). Claude Code's
//!   model aliases (`opus`, `sonnet`, `haiku`, `fable`) are mapped to OpenRouter's `~anthropic/…`
//!   aliases with the `ANTHROPIC_DEFAULT_*_MODEL` variables the guide gives; `--model` takes any
//!   OpenRouter model id.
//! - Codex: a `model_providers.openrouter` entry with `base_url=https://openrouter.ai/api/v1`,
//!   `env_key=OPENROUTER_API_KEY` and the Responses wire API (the only one Codex supports), set
//!   with `-c` overrides (<https://openrouter.ai/docs/cookbook/coding-agents/codex-cli.md>,
//!   <https://developers.openai.com/codex/config-reference>). Model ids need their vendor prefix
//!   (`openai/gpt-6.1-sol`), so a Codex agent on OpenRouter must name its model.
//!
//! The model list comes from OpenRouter's public models endpoint (no key), fetched by the daemon
//! with curl, kept in memory and in `<data>/openrouter-models.json`, refreshed at most hourly
//! ([`ModelCache`]).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::model::{SessionKind, now_ms};

/// The variable the owner sets on the daemon.
pub const ENV_KEY: &str = "OPENROUTER_API_KEY";
/// Marks a session Muster pointed at OpenRouter. A backend started from inside such a session
/// (`muster reload` typed there) would otherwise hand OpenRouter's endpoint and the key to every
/// session it starts; [`scrub_inherited`] removes them when it sees this.
pub const MARKER: &str = "MUSTER_PROVIDER";
const MARKER_VALUE: &str = "openrouter";

pub const CLAUDE_BASE_URL: &str = "https://openrouter.ai/api";
pub const CODEX_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const MODELS_URL: &str = "https://openrouter.ai/api/v1/models";

/// Claude Code's aliases, mapped to OpenRouter's latest-model aliases (the guide's values; the
/// `[1m]` suffix tells Claude Code the model has a 1M-token context).
const CLAUDE_ALIASES: &[(&str, &str)] = &[
    ("ANTHROPIC_DEFAULT_OPUS_MODEL", "~anthropic/claude-opus-latest[1m]"),
    ("ANTHROPIC_DEFAULT_SONNET_MODEL", "~anthropic/claude-sonnet-latest[1m]"),
    ("ANTHROPIC_DEFAULT_HAIKU_MODEL", "~anthropic/claude-haiku-latest"),
    ("ANTHROPIC_DEFAULT_FABLE_MODEL", "~anthropic/claude-fable-latest[1m]"),
];

/// Inherited variables removed from an OpenRouter Claude session: ones that pick the model
/// outside the agent's record. Every inherited `CLAUDE_CODE_USE_*` provider selector goes too
/// ([`is_provider_selector`]): Bedrock, Vertex, Foundry, Mantle, Claude Platform on AWS and any
/// later one rank above `ANTHROPIC_AUTH_TOKEN`.
const CLAUDE_OVERRIDES: &[&str] = &["ANTHROPIC_MODEL", "ANTHROPIC_SMALL_FAST_MODEL"];

/// `CLAUDE_CODE_USE_<provider>` picks a provider; `CLAUDE_CODE_USE_POWERSHELL_TOOL` is the only
/// one of that family that doesn't (<https://code.claude.com/docs/en/env-vars>).
fn is_provider_selector(var: &str) -> bool {
    var.starts_with("CLAUDE_CODE_USE_") && var != "CLAUDE_CODE_USE_POWERSHELL_TOOL"
}

/// Claude Code ignores provider, endpoint and authentication variables (`ANTHROPIC_BASE_URL`,
/// `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`, `CLAUDE_CODE_USE_*`) set in settings files, so a
/// profile's or project's `env` block can't send the key to another endpoint.
const PROVIDER_MANAGED: &str = "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST";
/// Claude Code strips credentials (by name or value: the key, `MUSTER_TOKEN`) from the
/// environment of the Bash commands, hooks and stdio MCP servers it starts, and runs Bash in its
/// own PID namespace (so it can't read the key from another process's `/proc/<pid>/environ`).
const SUBPROCESS_SCRUB: &str = "CLAUDE_CODE_SUBPROCESS_ENV_SCRUB";

/// Everything [`apply_session_env`] may set: removed again from an environment that carries
/// [`MARKER`] (inherited from an OpenRouter session, never the owner's own setting).
const SET_BY_US: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
    PROVIDER_MANAGED,
    SUBPROCESS_SCRUB,
    ENV_KEY,
    MARKER,
];

/// Where an agent's model runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    /// The CLI's own login (a Claude or ChatGPT subscription, or whatever the CLI is set up with).
    #[default]
    Subscription,
    /// OpenRouter, billed per token to the owner's OpenRouter key.
    #[serde(rename = "openrouter")]
    OpenRouter,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Subscription => "subscription",
            Provider::OpenRouter => "openrouter",
        }
    }
    /// Stored values; anything unknown is the subscription (never a paid provider by accident).
    pub fn parse(s: &str) -> Provider {
        match s.trim().to_ascii_lowercase().as_str() {
            "openrouter" => Provider::OpenRouter,
            _ => Provider::Subscription,
        }
    }
}

/// Shortest key accepted. Real keys are `sk-or-v1-` and 64 hex digits; the floor keeps the
/// redaction of stored launches ([`redact_spec`], a substring replacement) meaningful.
const KEY_MIN: usize = 16;

/// The owner's OpenRouter key. Its value is never printed (`Debug` and errors show a
/// placeholder) and never serialized.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

impl ApiKey {
    /// Blank, short (under 16 characters) or with control characters: no key.
    pub fn new(value: &str) -> Option<ApiKey> {
        let v = value.trim();
        (v.len() >= KEY_MIN && !v.chars().any(|c| c.is_control() || c.is_whitespace())).then(|| ApiKey(v.to_string()))
    }
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    /// A short, one-way fingerprint of the key (a salted SHA-256, 64 bits shown): tells two keys
    /// apart (which key an OpenRouter session was launched with) without revealing either.
    pub fn fingerprint(&self) -> String {
        use sha2::Digest;
        let digest = sha2::Sha256::digest(format!("muster-openrouter-key-fingerprint\0{}", self.0).as_bytes());
        digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
    }
}

/// The variables to take out of an inherited environment (the daemon's own, or what a session
/// would inherit from it) before anything of Muster's goes on:
/// - `OPENROUTER_API_KEY`, always;
/// - everything [`apply_session_env`] sets, when [`MARKER`] says the environment came from an
///   OpenRouter session (a backend started from inside one);
/// - with the daemon's key known: every variable whose value is that key, and the gateway it was
///   paired with. OpenRouter's own setup has the owner export `ANTHROPIC_AUTH_TOKEN=<key>`,
///   `ANTHROPIC_BASE_URL=https://openrouter.ai/api` and an empty `ANTHROPIC_API_KEY`; inherited,
///   that would put every Claude session on the owner's bill. So when the inherited
///   `ANTHROPIC_AUTH_TOKEN` is the key, an OpenRouter `ANTHROPIC_BASE_URL` and an empty
///   `ANTHROPIC_API_KEY` go with it. Another token or gateway of the owner's (not this key) stays.
pub fn inherited_removals<'a>(env: impl IntoIterator<Item = (&'a str, &'a str)> + Clone, key: Option<&ApiKey>) -> Vec<String> {
    let mut out = vec![ENV_KEY.to_string()];
    if env.clone().into_iter().any(|(k, _)| k == MARKER) {
        out.extend(SET_BY_US.iter().map(|s| s.to_string()));
    }
    if let Some(key) = key {
        let paired = env.clone().into_iter().any(|(k, v)| k == "ANTHROPIC_AUTH_TOKEN" && v.trim() == key.expose());
        for (k, v) in env {
            let gateway = k == "ANTHROPIC_BASE_URL" && is_openrouter_url(v);
            let empty_key = k == "ANTHROPIC_API_KEY" && v.is_empty();
            if v.contains(key.expose()) || (paired && (gateway || empty_key)) {
                out.push(k.to_string());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// `https://openrouter.ai/…` (any path), the host compared exactly.
fn is_openrouter_url(url: &str) -> bool {
    let rest = url.trim().strip_prefix("https://").or_else(|| url.trim().strip_prefix("http://")).unwrap_or("");
    let host = rest.split(['/', '?', '#']).next().unwrap_or("").rsplit('@').next().unwrap_or("");
    let host = host.split(':').next().unwrap_or("").to_ascii_lowercase();
    host == "openrouter.ai" || host.ends_with(".openrouter.ai")
}

/// At daemon start, before any thread exists: take the key out of the daemon's environment, with
/// every other variable holding it ([`inherited_removals`]), so git, gh, the terminal host and
/// every session inherit none of it.
pub fn take_key_from_env() -> Option<ApiKey> {
    let raw = std::env::var(ENV_KEY).ok();
    let key = raw.as_deref().and_then(ApiKey::new);
    if raw.as_deref().is_some_and(|r| !r.trim().is_empty()) && key.is_none() {
        eprintln!("muster: ignoring {ENV_KEY}: not an OpenRouter key (too short, or has spaces or control characters)");
    }
    let vars: Vec<(String, String)> = std::env::vars().collect();
    for var in inherited_removals(vars.iter().map(|(k, v)| (k.as_str(), v.as_str())), key.as_ref()) {
        // SAFETY: called from `run_daemon` before the runtime (or any other thread) starts, so
        // nothing reads the environment concurrently.
        unsafe { std::env::remove_var(var) };
    }
    key
}

/// Every session's inherited environment, before its own variables go on ([`inherited_removals`]).
pub fn scrub_inherited(env: &mut BTreeMap<String, String>, key: Option<&ApiKey>) {
    for var in inherited_removals(env.iter().map(|(k, v)| (k.as_str(), v.as_str())), key) {
        env.remove(&var);
    }
}

pub fn missing_key_error() -> anyhow::Error {
    anyhow::anyhow!(
        "OpenRouter needs the {ENV_KEY} environment variable on Muster's backend, which doesn't have it. \
         Restart the backend with it set (e.g. `env {ENV_KEY}=… muster reload` from a shell that has the key); \
         the key is read only when the backend starts"
    )
}

/// Point a session at OpenRouter: its environment (after [`scrub_inherited`] and any Claude
/// profile) gets the endpoint and the key. Subscription sessions never come here. Returns the
/// variables set to the key (the only ones [`restore_spec`] fills in again).
pub fn apply_session_env(kind: SessionKind, key: &ApiKey, env: &mut BTreeMap<String, String>) -> anyhow::Result<Vec<String>> {
    let injected = match kind {
        SessionKind::Claude => {
            env.retain(|k, _| !is_provider_selector(k) && !CLAUDE_OVERRIDES.contains(&k.as_str()));
            env.insert("ANTHROPIC_BASE_URL".into(), CLAUDE_BASE_URL.into());
            env.insert("ANTHROPIC_AUTH_TOKEN".into(), key.expose().into());
            // OpenRouter's guide: "must be explicitly empty", so an inherited key isn't used.
            env.insert("ANTHROPIC_API_KEY".into(), String::new());
            for (var, model) in CLAUDE_ALIASES {
                env.insert((*var).into(), (*model).into());
            }
            env.insert(PROVIDER_MANAGED.into(), "1".into());
            env.insert(SUBPROCESS_SCRUB.into(), "1".into());
            vec!["ANTHROPIC_AUTH_TOKEN".to_string()]
        }
        SessionKind::Codex => {
            env.insert(ENV_KEY.into(), key.expose().into());
            vec![ENV_KEY.to_string()]
        }
        SessionKind::Shell => anyhow::bail!("only Claude and Codex sessions can run on OpenRouter"),
    };
    env.insert(MARKER.into(), MARKER_VALUE.into());
    Ok(injected)
}

/// Codex's provider id for OpenRouter: Muster's own, so a `[model_providers.openrouter]` table
/// from OpenRouter's guide (with its `auth` command, which Codex refuses next to `env_key`) can't
/// merge into it. Defined completely here.
pub const CODEX_PROVIDER: &str = "muster_openrouter";

/// Codex's provider settings, as `-c` overrides placed before the agent's own arguments (the
/// agent's `-m` names the model). Muster's own: they never go through the argument allowlist
/// and are not stored with the session. The last one keeps Codex's default exclusion of
/// `*KEY*`/`*SECRET*`/`*TOKEN*` variables from the commands it runs, whatever the owner's config
/// says.
pub fn codex_args() -> Vec<String> {
    let p = CODEX_PROVIDER;
    [
        format!("model_provider=\"{p}\""),
        format!("model_providers.{p}.name=\"OpenRouter (Muster)\""),
        format!("model_providers.{p}.base_url=\"{CODEX_BASE_URL}\""),
        format!("model_providers.{p}.env_key=\"{ENV_KEY}\""),
        format!("model_providers.{p}.wire_api=\"responses\""),
        "shell_environment_policy.ignore_default_excludes=false".to_string(),
    ]
    .into_iter()
    .flat_map(|kv| ["-c".to_string(), kv])
    .collect()
}

/// Which environment variables of a stored launch Muster set to the key (stored empty): the only
/// places a re-sent launch gets the key back. Kept beside the launch, apart from every string a
/// user or an agent controls (a prompt, an argument), so no text can ask for the key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyRefs {
    #[serde(default)]
    pub env: Vec<String>,
}

impl KeyRefs {
    pub fn is_empty(&self) -> bool {
        self.env.is_empty()
    }
}

/// What a stored launch has where the key appeared anywhere Muster didn't put it (a prompt or a
/// variable that happened to hold it). Never turned back into the key.
const REMOVED: &str = "<muster: removed OpenRouter key>";

/// The launch as Muster stores it, for any session whatever its provider: the variables in
/// `injected` (the ones [`apply_session_env`] set to the key) stored empty and listed in the
/// returned [`KeyRefs`], and the key cut out of every other environment value and argument.
pub fn redact_spec(
    mut spec: muster_terminal_protocol::SpawnSpec,
    key: Option<&ApiKey>,
    injected: &[String],
) -> (muster_terminal_protocol::SpawnSpec, KeyRefs) {
    let mut refs = KeyRefs::default();
    let Some(key) = key else { return (spec, refs) };
    for (name, v) in spec.env.iter_mut() {
        if injected.contains(name) && v == key.expose() {
            v.clear();
            refs.env.push(name.clone());
        } else if v.contains(key.expose()) {
            *v = v.replace(key.expose(), REMOVED);
        }
    }
    for v in spec.args.iter_mut() {
        if v.contains(key.expose()) {
            *v = v.replace(key.expose(), REMOVED);
        }
    }
    (spec, refs)
}

/// A stored launch, ready to send to the host again: only the variables `refs` names get the key,
/// and only in a launch Muster made for OpenRouter ([`MARKER`]). Callers check `refs` first:
/// without a key, such a launch is never re-sent (`hub` defers it).
pub fn restore_spec(
    mut spec: muster_terminal_protocol::SpawnSpec,
    refs: &KeyRefs,
    key: Option<&ApiKey>,
) -> muster_terminal_protocol::SpawnSpec {
    let Some(key) = key else { return spec };
    if spec.env.get(MARKER).map(String::as_str) != Some(MARKER_VALUE) {
        return spec;
    }
    for name in &refs.env {
        if let Some(v) = spec.env.get_mut(name)
            && v.is_empty()
        {
            *v = key.expose().to_string();
        }
    }
    spec
}

// ----- the model list -----

/// One OpenRouter model as the agent dialog shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    /// What `--model` / `-m` gets, e.g. `anthropic/claude-sonnet-5.5`.
    pub id: String,
    pub name: String,
    /// The id's prefix without `~` (`anthropic`), for grouping.
    pub vendor: String,
    pub context_length: Option<u64>,
    /// US dollars per million tokens (None: not given).
    pub prompt_per_m: Option<f64>,
    pub completion_per_m: Option<f64>,
    /// Unix seconds.
    pub created: Option<i64>,
    /// An alias (`~anthropic/claude-opus-latest`) and the model it points at now.
    pub alias_of: Option<String>,
}

/// Vendors whose ids are routers that pick some model, not a model.
const ROUTER_VENDORS: &[&str] = &["openrouter"];

/// The models in an `/api/v1/models` response that make sense for a coding agent: tool calling
/// (both CLIs drive everything through tools), text in and text out, no `:batch` copies
/// (OpenRouter's batch endpoint, not for an interactive session) and no routers. Sorted by
/// vendor, then newest first.
pub fn parse_models(body: &[u8]) -> anyhow::Result<Vec<Model>> {
    #[derive(Deserialize)]
    struct Body {
        data: Vec<Value>,
    }
    let body: Body = serde_json::from_slice(body).map_err(|e| anyhow::anyhow!("OpenRouter's model list didn't parse: {e}"))?;
    let strs = |v: &Value| -> Vec<String> {
        v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default()
    };
    let per_m = |v: &Value| -> Option<f64> {
        let p = match v {
            Value::String(s) => s.trim().parse::<f64>().ok()?,
            Value::Number(n) => n.as_f64()?,
            _ => return None,
        };
        (p.is_finite() && p >= 0.0).then_some(p * 1_000_000.0)
    };
    let mut out = Vec::new();
    for m in &body.data {
        let Some(id) = m["id"].as_str().map(str::trim).filter(|id| valid_id(id)) else { continue };
        if id.ends_with(":batch") {
            continue;
        }
        let vendor = id.trim_start_matches('~').split('/').next().unwrap_or("").to_ascii_lowercase();
        if vendor.is_empty() || ROUTER_VENDORS.contains(&vendor.as_str()) {
            continue;
        }
        if !strs(&m["supported_parameters"]).iter().any(|p| p == "tools") {
            continue;
        }
        let arch = &m["architecture"];
        let (input, output) = (strs(&arch["input_modalities"]), strs(&arch["output_modalities"]));
        if !input.iter().any(|x| x == "text") || !output.iter().any(|x| x == "text") {
            continue;
        }
        let name = m["name"].as_str().map(str::trim).filter(|n| !n.is_empty()).unwrap_or(id);
        out.push(Model {
            id: id.to_string(),
            name: clip(name, 120),
            vendor,
            context_length: m["context_length"].as_u64(),
            prompt_per_m: per_m(&m["pricing"]["prompt"]),
            completion_per_m: per_m(&m["pricing"]["completion"]),
            created: m["created"].as_i64(),
            alias_of: m["alias_target"]["slug"].as_str().filter(|s| valid_id(s)).map(str::to_string),
        });
    }
    out.sort_by(|a, b| a.vendor.cmp(&b.vendor).then(b.created.cmp(&a.created)).then(a.id.cmp(&b.id)));
    out.dedup_by(|a, b| a.id == b.id);
    Ok(out)
}

/// A model id Muster passes on a command line: what `agents::agent_args` accepts.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= crate::agents::MODEL_MAX && !id.starts_with('-')
        && !id.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { s.chars().take(max).collect::<String>() + "…" }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Cached {
    /// Unix ms of the fetch.
    fetched_at: i64,
    models: Vec<Model>,
}

/// At most one fetch an hour; after a failure, wait this long before trying again.
const REFRESH_MS: i64 = 60 * 60 * 1000;
const RETRY_MS: i64 = 5 * 60 * 1000;
const FETCH_SECS: u64 = 20;
/// The full response is under 1 MB today.
const MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Default)]
struct State {
    cached: Option<Cached>,
    loaded: bool,
    last_attempt: i64,
    error: Option<String>,
}

/// The OpenRouter model list: memory, then `<data>/openrouter-models.json`, then the network.
pub struct ModelCache {
    path: PathBuf,
    state: parking_lot::Mutex<State>,
    /// One fetch at a time.
    fetching: tokio::sync::Mutex<()>,
    /// Tests: answer fetches with this body after this delay instead of going to the network.
    #[cfg(test)]
    fake: Option<(Vec<u8>, Duration)>,
}

impl ModelCache {
    pub fn new(path: PathBuf) -> ModelCache {
        ModelCache {
            path,
            state: Default::default(),
            fetching: tokio::sync::Mutex::new(()),
            #[cfg(test)]
            fake: None,
        }
    }

    async fn fetch(&self) -> anyhow::Result<Vec<u8>> {
        #[cfg(test)]
        if let Some((body, delay)) = &self.fake {
            tokio::time::sleep(*delay).await;
            return Ok(body.clone());
        }
        fetch().await
    }

    fn load_file(&self, st: &mut State) {
        if st.loaded {
            return;
        }
        st.loaded = true;
        match std::fs::read(&self.path) {
            Ok(raw) => match serde_json::from_slice::<Cached>(&raw) {
                Ok(c) => st.cached = Some(c),
                Err(e) => tracing::warn!("ignoring {}: {e}", self.path.display()),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("reading {}: {e}", self.path.display()),
        }
    }

    /// `openrouter.models`: the list, fetched first when there is none yet (waiting up to the
    /// fetch timeout) and refreshed in the background when it is older than an hour.
    pub async fn models(self: &std::sync::Arc<Self>, key_present: bool) -> Value {
        let (have, due) = {
            let mut st = self.state.lock();
            self.load_file(&mut st);
            let now = now_ms();
            let stale = st.cached.as_ref().is_none_or(|c| now - c.fetched_at >= REFRESH_MS);
            (st.cached.is_some(), stale && now - st.last_attempt >= RETRY_MS)
        };
        if !have {
            // No list yet: wait for a fetch, this call's or one already running (`refresh`
            // serializes them and skips a second try within the retry interval).
            self.refresh().await;
        } else if due {
            let me = self.clone();
            tokio::spawn(async move { me.refresh().await });
        }
        self.reply(key_present)
    }

    fn reply(&self, key_present: bool) -> Value {
        let st = self.state.lock();
        let now = now_ms();
        json!({
            "key_present": key_present,
            "models": st.cached.as_ref().map(|c| c.models.as_slice()).unwrap_or_default(),
            "fetched_at": st.cached.as_ref().map(|c| c.fetched_at),
            "stale": st.cached.as_ref().is_some_and(|c| now - c.fetched_at >= REFRESH_MS),
            "error": st.error,
        })
    }

    async fn refresh(&self) {
        let _one = self.fetching.lock().await;
        {
            let mut st = self.state.lock();
            if now_ms() - st.last_attempt < RETRY_MS && st.last_attempt > 0 {
                return;
            }
            st.last_attempt = now_ms();
        }
        let result = self.fetch().await.and_then(|body| parse_models(&body));
        let mut st = self.state.lock();
        match result {
            Ok(models) if !models.is_empty() => {
                let c = Cached { fetched_at: now_ms(), models };
                match serde_json::to_vec(&c) {
                    Ok(raw) => {
                        if let Err(e) = crate::paths::write_private(&self.path, &raw) {
                            tracing::warn!("saving {}: {e}", self.path.display());
                        }
                    }
                    Err(e) => tracing::warn!("encoding the OpenRouter model list: {e}"),
                }
                st.cached = Some(c);
                st.error = None;
            }
            Ok(_) => st.error = Some("OpenRouter's model list had no models a coding agent can use".into()),
            Err(e) => {
                tracing::warn!("fetching OpenRouter's model list: {e:#}");
                st.error = Some(format!("{e:#}"));
            }
        }
    }

    #[cfg(test)]
    pub fn seed(&self, fetched_at: i64, models: Vec<Model>) {
        let mut st = self.state.lock();
        st.loaded = true;
        st.cached = Some(Cached { fetched_at, models });
    }
}

/// GET the public models endpoint with curl (no key, https only, `-q`: no ~/.curlrc).
async fn fetch() -> anyhow::Result<Vec<u8>> {
    let mut args: Vec<String> = ["-q", "-sS", "-f", "-L", "--proto", "=https", "--proto-redir", "=https", "--max-redirs", "3"]
        .iter().map(|a| a.to_string()).collect();
    args.extend(["--max-time".into(), FETCH_SECS.to_string(), "--max-filesize".into(), MAX_BYTES.to_string(), MODELS_URL.into()]);
    run_capped("curl", &args, MAX_BYTES, Duration::from_secs(FETCH_SECS + 5)).await
}

/// Run `program` and return its stdout, read as it arrives and never more than `max` bytes: past
/// that the process is killed (curl before 8.4 doesn't hold `--max-filesize` for a chunked
/// response). Also killed past `deadline`. The OpenRouter key is never in its environment.
async fn run_capped(program: &str, args: &[String], max: usize, deadline: Duration) -> anyhow::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .env_remove(ENV_KEY)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { anyhow::anyhow!("{program} is not installed") } else { anyhow::anyhow!("running {program}: {e}") })?;
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    // Stderr: the first 4 KiB kept, the rest drained, so the process never blocks on it.
    let errors = tokio::spawn(async move {
        let (mut kept, mut buf) = (Vec::new(), [0u8; 4096]);
        while let Ok(n) = stderr.read(&mut buf).await {
            if n == 0 { break }
            let room = 4096usize.saturating_sub(kept.len());
            kept.extend_from_slice(&buf[..n.min(room)]);
        }
        kept
    });
    let work = async {
        let mut body = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = stdout.read(&mut buf).await?;
            if n == 0 { break }
            if body.len() + n > max {
                return Ok::<_, std::io::Error>(None);
            }
            body.extend_from_slice(&buf[..n]);
        }
        Ok(Some(body))
    };
    let body = match tokio::time::timeout(deadline, work).await {
        Err(_) => {
            let _ = child.kill().await;
            anyhow::bail!("OpenRouter didn't answer within {} s", deadline.as_secs());
        }
        Ok(Err(e)) => {
            let _ = child.kill().await;
            anyhow::bail!("reading {program}'s output: {e}");
        }
        Ok(Ok(None)) => {
            let _ = child.kill().await;
            anyhow::bail!("OpenRouter's model list is larger than {} MB", max / (1024 * 1024));
        }
        Ok(Ok(Some(body))) => body,
    };
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait()).await
        .map_err(|_| anyhow::anyhow!("{program} didn't exit"))??;
    let stderr = errors.await.unwrap_or_default();
    if !status.success() {
        let why = String::from_utf8_lossy(&stderr);
        let why = why.trim().trim_start_matches("curl: ");
        anyhow::bail!("couldn't reach OpenRouter ({})", clip(if why.is_empty() { "no details" } else { why }, 200));
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("fixtures/openrouter_models.json");

    #[test]
    fn the_fixture_keeps_tool_models_for_coding_agents() {
        let models = parse_models(FIXTURE.as_bytes()).unwrap();
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"anthropic/claude-sonnet-5.5"), "{ids:?}");
        assert!(ids.contains(&"openai/gpt-6.1-sol"), "{ids:?}");
        assert!(ids.contains(&"google/gemini-3.8-flash"), "{ids:?}");
        assert!(ids.contains(&"minimax/minimax-m3.1-flash:free"), "free models stay: {ids:?}");
        assert!(ids.contains(&"~anthropic/claude-opus-latest"), "aliases stay: {ids:?}");
        assert!(!ids.iter().any(|i| i.ends_with(":batch")), "no batch copies: {ids:?}");
        assert!(!ids.contains(&"moondream/moondream3-preview"), "no model without tools: {ids:?}");
        let alias = models.iter().find(|m| m.id == "~anthropic/claude-opus-latest").unwrap();
        assert_eq!(alias.vendor, "anthropic");
        assert!(alias.alias_of.as_deref().is_some_and(|a| a.starts_with("anthropic/")));
        let free = models.iter().find(|m| m.id.ends_with(":free")).unwrap();
        assert_eq!(free.prompt_per_m, Some(0.0));
        let sonnet = models.iter().find(|m| m.id == "anthropic/claude-sonnet-5.5").unwrap();
        assert!(sonnet.prompt_per_m.is_some_and(|p| p > 0.0) && sonnet.context_length.is_some());
        // Grouped by vendor.
        let vendors: Vec<&str> = models.iter().map(|m| m.vendor.as_str()).collect();
        let mut sorted = vendors.clone();
        sorted.sort();
        assert_eq!(vendors, sorted);
    }

    #[test]
    fn odd_entries_are_skipped_not_fatal() {
        let body = json!({"data": [
            {"id": "-x/flag", "supported_parameters": ["tools"], "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]}},
            {"id": "a b/space", "supported_parameters": ["tools"], "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]}},
            {"id": "openrouter/auto", "supported_parameters": ["tools"], "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]}},
            {"id": "acme/image-only", "supported_parameters": ["tools"], "architecture": {"input_modalities": ["text"], "output_modalities": ["image"]}},
            {"id": "acme/ok", "supported_parameters": ["tools"], "pricing": {"prompt": "nonsense", "completion": 0.000002},
             "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]}},
            {"no_id": true},
        ]});
        let models = parse_models(body.to_string().as_bytes()).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "acme/ok");
        assert_eq!(models[0].name, "acme/ok");
        assert_eq!(models[0].prompt_per_m, None);
        assert!((models[0].completion_per_m.unwrap() - 2.0).abs() < 1e-9);
        assert!(parse_models(b"<html>").is_err());
    }

    #[test]
    fn the_key_never_prints_and_blank_is_no_key() {
        let key = ApiKey::new(" sk-or-fixture-secret ").unwrap();
        assert_eq!(key.expose(), "sk-or-fixture-secret");
        assert!(!format!("{key:?}").contains("secret"));
        assert!(!missing_key_error().to_string().contains("secret"));
        assert!(ApiKey::new("  ").is_none());
        assert!(ApiKey::new("sk-or-fixture\nsecret").is_none());
        assert!(ApiKey::new("sk-or-fixture secret").is_none());
        assert!(ApiKey::new("short").is_none(), "too short to redact safely");
    }

    #[test]
    fn claude_sessions_get_the_gateway_and_codex_sessions_the_key() {
        let key = ApiKey::new("sk-or-fixture-secret").unwrap();
        let mut env = BTreeMap::from([
            ("ANTHROPIC_API_KEY".to_string(), "inherited".to_string()),
            ("ANTHROPIC_MODEL".to_string(), "opus".to_string()),
            ("EDITOR".to_string(), "vi".to_string()),
            ("CLAUDE_CODE_USE_POWERSHELL_TOOL".to_string(), "0".to_string()),
        ]);
        for selector in ["BEDROCK", "VERTEX", "FOUNDRY", "MANTLE", "ANTHROPIC_AWS", "SOMETHING_NEW"] {
            env.insert(format!("CLAUDE_CODE_USE_{selector}"), "1".into());
        }
        apply_session_env(SessionKind::Claude, &key, &mut env).unwrap();
        assert_eq!(env["ANTHROPIC_BASE_URL"], "https://openrouter.ai/api");
        assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "sk-or-fixture-secret");
        assert_eq!(env["ANTHROPIC_API_KEY"], "");
        assert_eq!(env["ANTHROPIC_DEFAULT_SONNET_MODEL"], "~anthropic/claude-sonnet-latest[1m]");
        // Settings files can't re-route it, and its subprocesses don't get the key.
        assert_eq!(env["CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST"], "1");
        assert_eq!(env["CLAUDE_CODE_SUBPROCESS_ENV_SCRUB"], "1");
        assert!(!env.keys().any(|k| is_provider_selector(k)), "{env:?}");
        assert_eq!(env["CLAUDE_CODE_USE_POWERSHELL_TOOL"], "0", "not a provider selector");
        assert!(!env.contains_key("ANTHROPIC_MODEL") && !env.contains_key(ENV_KEY));
        assert_eq!(env["EDITOR"], "vi");
        assert_eq!(env[MARKER], "openrouter");

        let mut env = BTreeMap::new();
        apply_session_env(SessionKind::Codex, &key, &mut env).unwrap();
        assert_eq!(env[ENV_KEY], "sk-or-fixture-secret");
        assert!(!env.contains_key("ANTHROPIC_BASE_URL") && !env.contains_key("ANTHROPIC_AUTH_TOKEN"));
        assert!(apply_session_env(SessionKind::Shell, &key, &mut env).is_err());

        // Muster's own provider id, defined completely: an `openrouter` table from OpenRouter's
        // guide (with `auth`) never merges into it.
        let args = codex_args();
        assert!(args.chunks(2).all(|p| p[0] == "-c"));
        let values: Vec<&str> = args.chunks(2).map(|p| p[1].as_str()).collect();
        assert!(values.contains(&"model_provider=\"muster_openrouter\""));
        assert!(values.contains(&"model_providers.muster_openrouter.base_url=\"https://openrouter.ai/api/v1\""));
        assert!(values.contains(&"model_providers.muster_openrouter.env_key=\"OPENROUTER_API_KEY\""));
        assert!(values.contains(&"model_providers.muster_openrouter.wire_api=\"responses\""));
        assert!(values.contains(&"shell_environment_policy.ignore_default_excludes=false"));
        assert!(!values.iter().any(|v| v.contains("model_providers.openrouter")));
        assert!(!args.iter().any(|a| a.contains("sk-or")));
    }

    #[test]
    fn inherited_openrouter_settings_never_reach_another_session() {
        let key = ApiKey::new("sk-or-fixture-secret").unwrap();
        // The owner's own gateway settings (not this key) are left alone; the key never passes.
        let mut env = BTreeMap::from([
            ("ANTHROPIC_BASE_URL".to_string(), "https://gateway.example".to_string()),
            ("ANTHROPIC_AUTH_TOKEN".to_string(), "another-gateway-token".to_string()),
            (ENV_KEY.to_string(), "sk-or-fixture-secret".to_string()),
        ]);
        scrub_inherited(&mut env, Some(&key));
        assert_eq!(env.get("ANTHROPIC_BASE_URL").map(String::as_str), Some("https://gateway.example"));
        assert_eq!(env.get("ANTHROPIC_AUTH_TOKEN").map(String::as_str), Some("another-gateway-token"));
        assert!(!env.contains_key(ENV_KEY));
        // OpenRouter's own shell setup, with this key: the token and the gateway it goes with.
        let mut env = BTreeMap::from([
            ("ANTHROPIC_BASE_URL".to_string(), "https://openrouter.ai/api".to_string()),
            ("ANTHROPIC_AUTH_TOKEN".to_string(), "sk-or-fixture-secret".to_string()),
            ("ANTHROPIC_API_KEY".to_string(), String::new()),
            ("SOME_OTHER".to_string(), "Bearer sk-or-fixture-secret".to_string()),
            ("PATH".to_string(), "/bin".to_string()),
        ]);
        scrub_inherited(&mut env, Some(&key));
        assert_eq!(env, BTreeMap::from([("PATH".to_string(), "/bin".to_string())]));
        // The same gateway with another token is the owner's own choice.
        let mut env = BTreeMap::from([
            ("ANTHROPIC_BASE_URL".to_string(), "https://openrouter.ai/api".to_string()),
            ("ANTHROPIC_AUTH_TOKEN".to_string(), "sk-or-someone-elses-key".to_string()),
        ]);
        scrub_inherited(&mut env, Some(&key));
        assert_eq!(env.len(), 2);
        // A backend started from inside an OpenRouter session: all of it goes, key or not.
        let mut env = BTreeMap::from([("PATH".to_string(), "/bin".to_string())]);
        apply_session_env(SessionKind::Claude, &key, &mut env).unwrap();
        scrub_inherited(&mut env, None);
        assert_eq!(env, BTreeMap::from([("PATH".to_string(), "/bin".to_string())]));
        assert!(is_openrouter_url("https://openrouter.ai/api") && is_openrouter_url("https://OPENROUTER.ai:443/x"));
        assert!(!is_openrouter_url("https://openrouter.ai.evil.example/api") && !is_openrouter_url("https://evil.example/openrouter.ai"));
    }

    #[test]
    fn launch_records_keep_no_key_and_only_muster_slots_get_it_back() {
        let key = ApiKey::new("sk-or-fixture-secret").unwrap();
        let spec = |env: &[(&str, &str)], args: &[&str]| muster_terminal_protocol::SpawnSpec {
            run: muster_terminal_protocol::RunId { id: 1, generation: 1 },
            program: "claude".into(),
            args: args.iter().map(|a| a.to_string()).collect(),
            env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            cwd: "/".into(),
            cols: 80,
            rows: 24,
        };
        let injected = vec!["ANTHROPIC_AUTH_TOKEN".to_string()];
        let launch = spec(
            &[("ANTHROPIC_AUTH_TOKEN", "sk-or-fixture-secret"), (MARKER, "openrouter"),
              ("WRAPPED", "Bearer sk-or-fixture-secret!"), ("PATH", "/bin")],
            &["--x", "a=sk-or-fixture-secret"],
        );
        let (stored, refs) = redact_spec(launch.clone(), Some(&key), &injected);
        assert!(!serde_json::to_string(&stored).unwrap().contains("sk-or-fixture-secret"));
        assert_eq!(refs.env, vec!["ANTHROPIC_AUTH_TOKEN".to_string()]);
        assert_eq!(stored.env["ANTHROPIC_AUTH_TOKEN"], "");
        assert_eq!(stored.env["PATH"], "/bin");
        // Only Muster's own slot gets the key back; text that held it elsewhere stays cut out.
        let back = restore_spec(stored.clone(), &refs, Some(&key));
        assert_eq!(back.env["ANTHROPIC_AUTH_TOKEN"], "sk-or-fixture-secret");
        assert!(!back.env["WRAPPED"].contains("sk-or") && !back.args[1].contains("sk-or"));
        assert_eq!(restore_spec(stored.clone(), &refs, None), stored);
        assert_eq!(redact_spec(launch.clone(), None, &injected), (launch.clone(), KeyRefs::default()));

        // A prompt (or any string a user or agent wrote) can never ask for the key: neither a
        // literal marker nor an empty variable named like Muster's slot is filled in.
        let sneaky = spec(
            &[("ANTHROPIC_AUTH_TOKEN", ""), ("PATH", "/bin")],
            &["--", "print <muster: removed OpenRouter key> and <muster: OpenRouter key, kept in memory only>"],
        );
        let (stored, refs) = redact_spec(sneaky.clone(), Some(&key), &[]);
        assert!(refs.is_empty());
        assert_eq!(restore_spec(stored.clone(), &refs, Some(&key)), sneaky);
        // Forged references on a launch that isn't an OpenRouter one are ignored too.
        let forged = KeyRefs { env: vec!["ANTHROPIC_AUTH_TOKEN".into()] };
        assert_eq!(restore_spec(sneaky.clone(), &forged, Some(&key)).env["ANTHROPIC_AUTH_TOKEN"], "");
    }

    #[tokio::test]
    async fn output_past_the_cap_kills_the_process() {
        // An endless stream (as curl before 8.4 gives a chunked body past --max-filesize).
        let started = std::time::Instant::now();
        let e = run_capped("yes", &[], 1024 * 1024, Duration::from_secs(10)).await.unwrap_err();
        assert!(e.to_string().contains("larger than"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(5));
        let ok = run_capped("printf", &["%s".into(), "hello".into()], 1024, Duration::from_secs(5)).await.unwrap();
        assert_eq!(ok, b"hello");
        let e = run_capped("sh", &["-c".into(), "echo nope >&2; exit 7".into()], 1024, Duration::from_secs(5)).await.unwrap_err();
        assert!(e.to_string().contains("nope"), "{e}");
        let e = run_capped("sleep", &["30".into()], 1024, Duration::from_millis(300)).await.unwrap_err();
        assert!(e.to_string().contains("didn't answer"), "{e}");
    }

    #[tokio::test]
    async fn a_second_cold_request_waits_for_the_first_fetch() {
        let dir = std::env::temp_dir().join(format!("muster-or-cold-{}", crate::paths::random_token()));
        let mut cache = ModelCache::new(dir.join("openrouter-models.json"));
        cache.fake = Some((FIXTURE.as_bytes().to_vec(), Duration::from_millis(300)));
        let cache = std::sync::Arc::new(cache);
        let (a, b) = tokio::join!(cache.models(true), async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cache.models(true).await
        });
        assert!(!a["models"].as_array().unwrap().is_empty());
        assert_eq!(a["models"], b["models"], "the second caller got the list, not an empty reply");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn providers_parse_safely() {
        assert_eq!(Provider::parse("openrouter"), Provider::OpenRouter);
        assert_eq!(Provider::parse("anything"), Provider::Subscription);
        assert_eq!(serde_json::to_value(Provider::OpenRouter).unwrap(), json!("openrouter"));
        assert_eq!(serde_json::from_value::<Provider>(json!("subscription")).unwrap(), Provider::Subscription);
    }

    #[tokio::test]
    async fn a_fresh_cache_answers_without_fetching() {
        let dir = std::env::temp_dir().join(format!("muster-or-{}", crate::paths::random_token()));
        let cache = std::sync::Arc::new(ModelCache::new(dir.join("openrouter-models.json")));
        let models = parse_models(FIXTURE.as_bytes()).unwrap();
        cache.seed(now_ms(), models.clone());
        let v = cache.models(false).await;
        assert_eq!(v["key_present"], false);
        assert_eq!(v["models"].as_array().unwrap().len(), models.len());
        assert_eq!(v["stale"], false);
        assert!(v["error"].is_null());
        // A cache file written by an earlier run is read back.
        let c = Cached { fetched_at: now_ms(), models };
        crate::paths::write_private(&dir.join("openrouter-models.json"), &serde_json::to_vec(&c).unwrap()).unwrap();
        let again = std::sync::Arc::new(ModelCache::new(dir.join("openrouter-models.json")));
        assert_eq!(again.models(true).await["models"], v["models"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
