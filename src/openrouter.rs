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

/// Variables that would pick another endpoint or credential ahead of OpenRouter's in a Claude
/// session (cloud providers rank above `ANTHROPIC_AUTH_TOKEN`). Removed from an OpenRouter
/// session's inherited environment.
const CLAUDE_OVERRIDES: &[&str] = &[
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL",
];

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
    /// Blank (after trimming) is no key.
    pub fn new(value: &str) -> Option<ApiKey> {
        let v = value.trim();
        (!v.is_empty() && !v.chars().any(char::is_control)).then(|| ApiKey(v.to_string()))
    }
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

/// At daemon start, before any thread exists: take the key out of the daemon's environment
/// (so git, gh, the terminal host and every session inherit nothing), and drop OpenRouter
/// settings inherited from an OpenRouter session (see [`MARKER`]).
pub fn take_key_from_env() -> Option<ApiKey> {
    let key = std::env::var(ENV_KEY).ok().and_then(|v| ApiKey::new(&v));
    let mut remove = vec![ENV_KEY];
    if std::env::var_os(MARKER).is_some() {
        remove.extend_from_slice(SET_BY_US);
    }
    for var in remove {
        // SAFETY: called from `run_daemon` before the runtime (or any other thread) starts, so
        // nothing reads the environment concurrently.
        unsafe { std::env::remove_var(var) };
    }
    key
}

/// Every session's inherited environment, before its own variables go on: never the key, and
/// none of the OpenRouter settings when they came from an OpenRouter session.
pub fn scrub_inherited(env: &mut BTreeMap<String, String>) {
    if env.contains_key(MARKER) {
        for var in SET_BY_US {
            env.remove(*var);
        }
    }
    env.remove(ENV_KEY);
}

pub fn missing_key_error() -> anyhow::Error {
    anyhow::anyhow!(
        "OpenRouter needs the {ENV_KEY} environment variable on Muster's backend, which doesn't have it. \
         Restart the backend with it set (e.g. `env {ENV_KEY}=… muster reload` from a shell that has the key); \
         the key is read only when the backend starts"
    )
}

/// Point a session at OpenRouter: its environment (after [`scrub_inherited`] and any Claude
/// profile) gets the endpoint and the key. Subscription sessions never come here.
pub fn apply_session_env(kind: SessionKind, key: &ApiKey, env: &mut BTreeMap<String, String>) -> anyhow::Result<()> {
    match kind {
        SessionKind::Claude => {
            for var in CLAUDE_OVERRIDES {
                env.remove(*var);
            }
            env.insert("ANTHROPIC_BASE_URL".into(), CLAUDE_BASE_URL.into());
            env.insert("ANTHROPIC_AUTH_TOKEN".into(), key.expose().into());
            // OpenRouter's guide: "must be explicitly empty", so an inherited key isn't used.
            env.insert("ANTHROPIC_API_KEY".into(), String::new());
            for (var, model) in CLAUDE_ALIASES {
                env.insert((*var).into(), (*model).into());
            }
        }
        SessionKind::Codex => {
            env.insert(ENV_KEY.into(), key.expose().into());
        }
        SessionKind::Shell => anyhow::bail!("only Claude and Codex sessions can run on OpenRouter"),
    }
    env.insert(MARKER.into(), MARKER_VALUE.into());
    Ok(())
}

/// Codex's provider settings, as `-c` overrides placed before the agent's own arguments (the
/// agent's `-m` names the model). Muster's own: they never go through the argument allowlist
/// and are not stored with the session.
pub fn codex_args() -> Vec<String> {
    [
        "model_provider=\"openrouter\"".to_string(),
        "model_providers.openrouter.name=\"OpenRouter\"".to_string(),
        format!("model_providers.openrouter.base_url=\"{CODEX_BASE_URL}\""),
        format!("model_providers.openrouter.env_key=\"{ENV_KEY}\""),
        "model_providers.openrouter.wire_api=\"responses\"".to_string(),
    ]
    .into_iter()
    .flat_map(|kv| ["-c".to_string(), kv])
    .collect()
}

/// What a launch record keeps in place of the key.
const PLACEHOLDER: &str = "<muster: OpenRouter key, kept in memory only>";

/// The launch as Muster stores it: every environment value equal to the key replaced.
pub fn redact_spec(mut spec: muster_terminal_protocol::SpawnSpec, key: Option<&ApiKey>) -> muster_terminal_protocol::SpawnSpec {
    if let Some(key) = key {
        for v in spec.env.values_mut() {
            if v == key.expose() {
                *v = PLACEHOLDER.into();
            }
        }
    }
    spec
}

/// A stored launch, ready to send to the host again: the placeholder becomes the key. Without a
/// key (the backend was restarted without it) the placeholder stays and OpenRouter refuses it.
pub fn restore_spec(mut spec: muster_terminal_protocol::SpawnSpec, key: Option<&ApiKey>) -> muster_terminal_protocol::SpawnSpec {
    for v in spec.env.values_mut() {
        if v == PLACEHOLDER {
            match key {
                Some(key) => *v = key.expose().into(),
                None => tracing::warn!("a stored OpenRouter launch is re-sent without a key: the backend has no {ENV_KEY}"),
            }
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
}

impl ModelCache {
    pub fn new(path: PathBuf) -> ModelCache {
        ModelCache { path, state: Default::default(), fetching: tokio::sync::Mutex::new(()) }
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
        if due {
            if have {
                let me = self.clone();
                tokio::spawn(async move { me.refresh().await });
            } else {
                self.refresh().await;
            }
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
        let result = fetch().await.and_then(|body| parse_models(&body));
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
    let out = tokio::time::timeout(
        Duration::from_secs(FETCH_SECS + 5),
        tokio::process::Command::new("curl")
            .args(["-q", "-sS", "-f", "-L", "--proto", "=https", "--proto-redir", "=https", "--max-redirs", "3"])
            .args(["--max-time", &FETCH_SECS.to_string(), "--max-filesize", &MAX_BYTES.to_string(), MODELS_URL])
            .env_remove(ENV_KEY)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("OpenRouter didn't answer within {FETCH_SECS} s"))?
    .map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { anyhow::anyhow!("curl is not installed") } else { anyhow::anyhow!("running curl: {e}") })?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        let why = why.trim().trim_start_matches("curl: ");
        anyhow::bail!("couldn't reach OpenRouter ({})", clip(if why.is_empty() { "no details" } else { why }, 200));
    }
    anyhow::ensure!(out.stdout.len() <= MAX_BYTES, "OpenRouter's model list is larger than expected");
    Ok(out.stdout)
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
        assert!(ApiKey::new("a\nb").is_none());
    }

    #[test]
    fn claude_sessions_get_the_gateway_and_codex_sessions_the_key() {
        let key = ApiKey::new("sk-or-fixture-secret").unwrap();
        let mut env = BTreeMap::from([
            ("ANTHROPIC_API_KEY".to_string(), "inherited".to_string()),
            ("CLAUDE_CODE_USE_BEDROCK".to_string(), "1".to_string()),
            ("EDITOR".to_string(), "vi".to_string()),
        ]);
        apply_session_env(SessionKind::Claude, &key, &mut env).unwrap();
        assert_eq!(env["ANTHROPIC_BASE_URL"], "https://openrouter.ai/api");
        assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "sk-or-fixture-secret");
        assert_eq!(env["ANTHROPIC_API_KEY"], "");
        assert_eq!(env["ANTHROPIC_DEFAULT_SONNET_MODEL"], "~anthropic/claude-sonnet-latest[1m]");
        assert!(!env.contains_key("CLAUDE_CODE_USE_BEDROCK") && !env.contains_key(ENV_KEY));
        assert_eq!(env["EDITOR"], "vi");
        assert_eq!(env[MARKER], "openrouter");

        let mut env = BTreeMap::new();
        apply_session_env(SessionKind::Codex, &key, &mut env).unwrap();
        assert_eq!(env[ENV_KEY], "sk-or-fixture-secret");
        assert!(!env.contains_key("ANTHROPIC_BASE_URL") && !env.contains_key("ANTHROPIC_AUTH_TOKEN"));
        assert!(apply_session_env(SessionKind::Shell, &key, &mut env).is_err());

        let args = codex_args();
        assert!(args.chunks(2).all(|p| p[0] == "-c"));
        assert!(args.contains(&"model_provider=\"openrouter\"".to_string()));
        assert!(args.contains(&"model_providers.openrouter.base_url=\"https://openrouter.ai/api/v1\"".to_string()));
        assert!(args.contains(&"model_providers.openrouter.env_key=\"OPENROUTER_API_KEY\"".to_string()));
        assert!(!args.iter().any(|a| a.contains("sk-or")));
    }

    #[test]
    fn inherited_openrouter_settings_never_reach_another_session() {
        // The owner's own gateway settings (no marker) are left alone; the key never passes.
        let mut env = BTreeMap::from([
            ("ANTHROPIC_BASE_URL".to_string(), "https://gateway.example".to_string()),
            (ENV_KEY.to_string(), "sk-or-fixture-secret".to_string()),
        ]);
        scrub_inherited(&mut env);
        assert_eq!(env.get("ANTHROPIC_BASE_URL").map(String::as_str), Some("https://gateway.example"));
        assert!(!env.contains_key(ENV_KEY));
        // A backend started from inside an OpenRouter session: all of it goes.
        let key = ApiKey::new("sk-or-fixture-secret").unwrap();
        let mut env = BTreeMap::from([("PATH".to_string(), "/bin".to_string())]);
        apply_session_env(SessionKind::Claude, &key, &mut env).unwrap();
        scrub_inherited(&mut env);
        assert_eq!(env, BTreeMap::from([("PATH".to_string(), "/bin".to_string())]));
    }

    #[test]
    fn launch_records_keep_a_placeholder_not_the_key() {
        let key = ApiKey::new("sk-or-fixture-secret").unwrap();
        let spec = muster_terminal_protocol::SpawnSpec {
            run: muster_terminal_protocol::RunId { id: 1, generation: 1 },
            program: "claude".into(),
            args: vec![],
            env: BTreeMap::from([
                ("ANTHROPIC_AUTH_TOKEN".to_string(), "sk-or-fixture-secret".to_string()),
                ("PATH".to_string(), "/bin".to_string()),
            ]),
            cwd: "/".into(),
            cols: 80,
            rows: 24,
        };
        let stored = redact_spec(spec.clone(), Some(&key));
        assert!(!serde_json::to_string(&stored).unwrap().contains("sk-or-fixture-secret"));
        assert_eq!(stored.env["PATH"], "/bin");
        assert_eq!(restore_spec(stored.clone(), Some(&key)), spec);
        assert_eq!(restore_spec(stored.clone(), None).env["ANTHROPIC_AUTH_TOKEN"], PLACEHOLDER);
        assert_eq!(redact_spec(spec.clone(), None), spec);
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
