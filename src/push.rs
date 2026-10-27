//! Phone alerts over Web Push (docs/ANDROID.md "Notifications", docs/PROTOCOL.md "Phone
//! alerts"): RFC 8030 delivery, RFC 8291 message encryption, RFC 8292 VAPID.
//!
//! A paired device hands us a push subscription (an endpoint URL at its browser's push service
//! plus two keys). Because the daemon then makes outbound requests to an address a device
//! supplied, the endpoint is held to strict rules ([`check_endpoint`]): `https` only, a host on
//! an allowlist of the big browser push services (extendable with `MUSTER_PUSH_HOSTS`), no
//! redirects, a 5 s limit. Each alert is encrypted end to end to the phone and padded to one
//! fixed size, so the push service sees only that an alert of the usual size went out.
//!
//! Requests go through `curl`, like the OpenRouter model list (src/openrouter.rs): no TLS stack
//! in the daemon. The endpoint and the signed VAPID token travel on curl's stdin (a config), never
//! on its command line, which other local users can read.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use base64::engine::DecodePaddingMode;
use p256::elliptic_curve::sec1::ToSec1Point;
use serde::Serialize;
use serde_json::json;

use crate::hub::Hub;
use crate::model::now_ms;

/// Push services' hosts a subscription may point at. `*.` matches any subdomain (not the bare
/// domain). Chrome on Android uses FCM; Firefox Mozilla's autopush; Edge on Windows WNS.
pub const DEFAULT_HOSTS: &[&str] = &["fcm.googleapis.com", "*.push.services.mozilla.com", "*.notify.windows.com"];
/// Extra allowed hosts, comma-separated (`host`, `*.domain` or `host:port`). The owner's
/// configuration only; a device can never widen it.
pub const ENV_HOSTS: &str = "MUSTER_PUSH_HOSTS";
/// The VAPID `sub` claim (a contact for the push service's operators). Default: a neutral URL
/// naming the tool, never anything about the owner.
pub const ENV_SUBJECT: &str = "MUSTER_PUSH_SUBJECT";
pub const DEFAULT_SUBJECT: &str = "https://muster.invalid/web-push";
/// Optional fallback: also post each alert as plain text to this ntfy topic URL (a self-hosted
/// ntfy; ntfy.sh would see the text).
pub const ENV_NTFY: &str = "MUSTER_NTFY_URL";

/// RFC 8030: how long the push service keeps an alert for an unreachable phone (an approval
/// older than 5 minutes is better seen in the session list than as a late alert).
pub const TTL_S: u32 = 300;
/// Whole-request limit, connect included.
pub const TIMEOUT_S: u64 = 5;
/// Every alert's plaintext is padded to exactly this many bytes (the JSON, the RFC 8291
/// delimiter, zeros), so all alerts have one length on the wire.
pub const PADDED_LEN: usize = 1024;
/// RFC 8188 record size: one record holds a whole padded alert plus its tag.
const RECORD_SIZE: u32 = 4096;
const ENDPOINT_MAX: usize = 2048;
/// Curl processes running at once.
const CONCURRENCY: usize = 4;
/// Deliveries waiting or running at once, all devices together.
const QUEUE_MAX: usize = 64;
/// `push.test` at most this often per device.
const TEST_GAP_S: u64 = 5;

/// Base64url as browsers write subscription keys: padding optional on input, none on output.
const B64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::URL_SAFE,
    GeneralPurposeConfig::new().with_encode_padding(false).with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

pub fn b64(data: &[u8]) -> String {
    B64.encode(data)
}

/// Standard or url-safe base64, padded or not (as `PushSubscription.toJSON()` writes keys).
pub fn unb64(s: &str) -> Option<Vec<u8>> {
    let s: String = s.trim().chars().map(|c| match c { '+' => '-', '/' => '_', c => c }).collect();
    B64.decode(s).ok()
}

// ----- subscriptions -----

/// A device's push subscription (one per device). Keys stay in the daemon.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Subscription {
    pub endpoint: String,
    #[serde(skip)]
    pub p256dh: String,
    #[serde(skip)]
    pub auth: String,
    pub created_at: i64,
    pub last_ok_at: Option<i64>,
    pub last_error: Option<String>,
    pub last_error_at: Option<i64>,
}

impl Subscription {
    /// What the phone and `device.list` show: no keys, the endpoint's host rather than the
    /// whole capability URL for the desktop.
    pub fn summary(&self) -> serde_json::Value {
        json!({
            "endpoint_host": endpoint_host(&self.endpoint),
            "created_at": self.created_at,
            "last_ok_at": self.last_ok_at,
            "last_error": self.last_error,
            "last_error_at": self.last_error_at,
        })
    }
}

/// The checked parts of an endpoint URL.
#[derive(Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// `https://host[:port]`: the VAPID audience.
    pub origin: String,
    pub host: String,
}

fn endpoint_host(url: &str) -> String {
    check_endpoint_syntax(url).map(|e| e.host).unwrap_or_default()
}

/// The allowlist: the defaults plus `MUSTER_PUSH_HOSTS`.
pub fn allowed_hosts() -> Vec<String> {
    let mut v: Vec<String> = DEFAULT_HOSTS.iter().map(|s| s.to_string()).collect();
    if let Ok(extra) = std::env::var(ENV_HOSTS) {
        v.extend(extra.split(',').map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()));
    }
    v
}

/// Does `host` (lowercase, with `:port` when not 443) match an allowlist entry? An entry
/// without a port matches only the default port; `*.domain` matches subdomains only.
fn host_allowed(host_port: &str, allow: &[String]) -> bool {
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (host_port, None),
    };
    allow.iter().any(|entry| {
        let (pat, want_port) = match entry.rsplit_once(':') {
            Some((h, p)) if !h.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => (h, Some(p)),
            _ => (entry.as_str(), None),
        };
        if want_port != port {
            return false;
        }
        match pat.strip_prefix("*.") {
            Some(domain) => {
                !domain.is_empty()
                    && host.len() > domain.len() + 1
                    && host.ends_with(domain)
                    && host.as_bytes()[host.len() - domain.len() - 1] == b'.'
            }
            None => host == pat,
        }
    })
}

/// Syntax only: `https://host[:port]/path...`, plain ASCII URL characters (nothing that needs
/// quoting for curl), no user info, no fragment, a lowercase DNS-style host.
fn check_endpoint_syntax(url: &str) -> Result<Endpoint, String> {
    if url.len() > ENDPOINT_MAX {
        return Err("push endpoint is too long".into());
    }
    if !url.bytes().all(|b| b.is_ascii_graphic() && !matches!(b, b'"' | b'\\' | b'<' | b'>' | b'`' | b'{' | b'}' | b'|' | b'^')) {
        return Err("push endpoint has characters a URL can't have".into());
    }
    let rest = url.strip_prefix("https://").ok_or("push endpoint must be an https URL")?;
    if url.contains('#') {
        return Err("push endpoint can't have a fragment".into());
    }
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.contains('@') {
        return Err("push endpoint can't carry user info".into());
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (authority, None),
    };
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    if host.is_empty() || host.len() > 253 || !host.split('.').all(label_ok) {
        return Err("push endpoint's host is not a plain lowercase host name".into());
    }
    let host_port = match port {
        None | Some("443") => host.to_string(),
        Some(p) if !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()) && p.parse::<u16>().is_ok_and(|n| n > 0) => {
            format!("{host}:{p}")
        }
        Some(_) => return Err("push endpoint's port is not valid".into()),
    };
    if !rest[end..].is_empty() && !rest[end..].starts_with('/') {
        return Err("push endpoint needs a path".into());
    }
    Ok(Endpoint { origin: format!("https://{host_port}"), host: host_port })
}

/// The endpoint rules (docs/PROTOCOL.md "Phone alerts"): syntax, then the allowlist.
pub fn check_endpoint(url: &str, allow: &[String]) -> Result<Endpoint, String> {
    let e = check_endpoint_syntax(url)?;
    if !host_allowed(&e.host, allow) {
        return Err(format!(
            "{} is not a known push service (allowed: {}; the owner can add one with {ENV_HOSTS})",
            e.host,
            allow.join(", ")
        ));
    }
    Ok(e)
}

/// Check the keys a browser gave (`p256dh`: an uncompressed P-256 point, `auth`: 16 bytes).
pub fn check_keys(p256dh: &str, auth: &str) -> Result<([u8; 65], [u8; 16]), String> {
    let key = unb64(p256dh).ok_or("p256dh is not base64url")?;
    let key: [u8; 65] = key.try_into().map_err(|_| "p256dh must be a 65-byte uncompressed P-256 key")?;
    if key[0] != 4 || p256::PublicKey::from_sec1_bytes(&key).is_err() {
        return Err("p256dh is not a valid P-256 public key".into());
    }
    let auth = unb64(auth).ok_or("auth is not base64url")?;
    let auth: [u8; 16] = auth.try_into().map_err(|_| "auth must be 16 bytes")?;
    Ok((key, auth))
}

// ----- RFC 8291 -----

fn hkdf_expand(salt: &[u8], ikm: &[u8], info: &[u8], out: &mut [u8]) {
    hkdf::Hkdf::<sha2::Sha256>::new(Some(salt), ikm)
        .expand(info, out)
        .expect("HKDF output lengths here are far below the limit");
}

fn public_bytes(secret: &p256::SecretKey) -> [u8; 65] {
    let point = secret.public_key().to_sec1_point(false);
    point.as_bytes().try_into().expect("an uncompressed P-256 point is 65 bytes")
}

fn random_secret() -> p256::SecretKey {
    loop {
        // A uniformly random 32-byte string is a valid scalar with overwhelming probability.
        if let Ok(k) = p256::SecretKey::from_slice(&rand::random::<[u8; 32]>()) {
            return k;
        }
    }
}

/// RFC 8291 `aes128gcm` message: `salt || rs || idlen || as_public || AES-GCM(record)`, where
/// the record is `plaintext || 0x02 || zeros` to `pad_to` bytes (at least the plaintext plus
/// its delimiter). `as_secret` and `salt` are fresh for every message (parameters for tests).
pub fn encrypt(
    ua_public: &[u8; 65],
    auth_secret: &[u8; 16],
    plaintext: &[u8],
    pad_to: usize,
    as_secret: &p256::SecretKey,
    salt: &[u8; 16],
) -> Result<Vec<u8>, String> {
    use aes_gcm::aead::{Aead, KeyInit};
    let ua = p256::PublicKey::from_sec1_bytes(ua_public).map_err(|_| "invalid p256dh")?;
    let as_public = public_bytes(as_secret);
    let shared = as_secret.diffie_hellman(&ua);
    // PRK_key = HMAC(auth_secret, ecdh_secret); IKM = HKDF-Expand(PRK_key, key_info, 32).
    let mut key_info = Vec::with_capacity(14 + 130);
    key_info.extend_from_slice(b"WebPush: info\0");
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(&as_public);
    let mut ikm = [0u8; 32];
    hkdf_expand(auth_secret, shared.raw_secret_bytes().as_slice(), &key_info, &mut ikm);
    // PRK = HMAC(salt, IKM); CEK and NONCE from it.
    let mut cek = [0u8; 16];
    hkdf_expand(salt, &ikm, b"Content-Encoding: aes128gcm\0", &mut cek);
    let mut nonce = [0u8; 12];
    hkdf_expand(salt, &ikm, b"Content-Encoding: nonce\0", &mut nonce);
    let len = pad_to.max(plaintext.len() + 1);
    if len + 16 > RECORD_SIZE as usize {
        return Err("alert is too large".into());
    }
    let mut record = Vec::with_capacity(len);
    record.extend_from_slice(plaintext);
    record.push(2);
    record.resize(len, 0);
    let cipher = aes_gcm::Aes128Gcm::new_from_slice(&cek).map_err(|_| "bad key length")?;
    let sealed = cipher
        .encrypt(&nonce.into(), record.as_slice())
        .map_err(|_| "encryption failed")?;
    let mut out = Vec::with_capacity(86 + sealed.len());
    out.extend_from_slice(salt);
    out.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    out.push(65);
    out.extend_from_slice(&as_public);
    out.extend_from_slice(&sealed);
    Ok(out)
}

// ----- VAPID (RFC 8292) -----

/// The daemon's application-server key, generated once into `<data>/push_vapid_key` (0600).
pub struct Vapid {
    key: p256::ecdsa::SigningKey,
    /// Uncompressed public key, base64url: the phone's `applicationServerKey`.
    pub public_key: String,
}

impl Vapid {
    pub fn from_secret(secret: &p256::SecretKey) -> Vapid {
        Vapid { key: p256::ecdsa::SigningKey::from(secret), public_key: b64(&public_bytes(secret)) }
    }

    /// Load the key, creating it on first use. A corrupt file is an error (replacing it would
    /// silently break every phone's subscription).
    pub fn load_or_create(path: &std::path::Path) -> anyhow::Result<Vapid> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let bytes = unb64(&text).ok_or_else(|| anyhow::anyhow!("{} is not base64url", path.display()))?;
                let secret = p256::SecretKey::from_slice(&bytes)
                    .map_err(|_| anyhow::anyhow!("{} does not hold a P-256 key", path.display()))?;
                Ok(Vapid::from_secret(&secret))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let secret = random_secret();
                crate::paths::write_private(path, b64(secret.to_bytes().as_slice()).as_bytes())?;
                tracing::info!("created the Web Push key {}", path.display());
                Ok(Vapid::from_secret(&secret))
            }
            Err(e) => Err(anyhow::anyhow!("reading {}: {e}", path.display())),
        }
    }

    /// `Authorization: vapid t=<JWT>, k=<public key>` for a push service origin, valid 12 h.
    pub fn authorization(&self, audience: &str, subject: &str, now_s: i64) -> String {
        use p256::ecdsa::signature::Signer;
        let header = b64(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = b64(json!({ "aud": audience, "exp": now_s + 12 * 3600, "sub": subject }).to_string().as_bytes());
        let signing_input = format!("{header}.{claims}");
        let sig: p256::ecdsa::Signature = self.key.sign(signing_input.as_bytes());
        format!("vapid t={signing_input}.{}, k={}", b64(&sig.to_bytes()), self.public_key)
    }
}

fn vapid_path() -> std::path::PathBuf {
    crate::paths::data_dir().join("push_vapid_key")
}

static VAPID: std::sync::OnceLock<Vapid> = std::sync::OnceLock::new();
static VAPID_INIT: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// The daemon's VAPID key (loaded or created on first use).
pub fn vapid() -> anyhow::Result<&'static Vapid> {
    if let Some(v) = VAPID.get() {
        return Ok(v);
    }
    let _g = VAPID_INIT.lock();
    if let Some(v) = VAPID.get() {
        return Ok(v);
    }
    let v = Vapid::load_or_create(&vapid_path())?;
    Ok(VAPID.get_or_init(|| v))
}

fn subject() -> String {
    std::env::var(ENV_SUBJECT)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| (s.starts_with("https://") || s.starts_with("mailto:")) && s.len() < 256)
        .unwrap_or_else(|| DEFAULT_SUBJECT.into())
}

// ----- alerts -----

/// One alert for the phones (built by src/notify.rs).
#[derive(Debug, Clone)]
pub struct Alert {
    /// None: a test alert (`push.test`).
    pub session_id: Option<crate::model::SessionId>,
    pub channel_id: Option<crate::model::ChannelId>,
    pub state: Option<crate::model::AgentState>,
    pub state_since: i64,
    pub state_seq: u64,
    pub title: String,
    pub body: String,
}

fn clip_chars(s: &str, n: usize) -> String {
    let mut out: String = s.chars().filter(|c| !c.is_control()).take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out
}

/// The JSON the service worker reads (web/m/sw.js), short enough to pad to [`PADDED_LEN`]
/// (title and body are cut until it fits).
pub fn payload(a: &Alert, at: i64) -> Vec<u8> {
    let (mut title_n, mut body_n) = (120usize, 400usize);
    loop {
        let v = json!({
            "v": 1,
            "kind": if a.session_id.is_some() { "session" } else { "test" },
            "session_id": a.session_id,
            "channel_id": a.channel_id,
            "state": a.state.map(|s| s.as_str()),
            "state_since": a.state_since,
            "state_seq": a.state_seq,
            "at": at,
            "title": clip_chars(&a.title, title_n),
            "body": clip_chars(&a.body, body_n),
        });
        let bytes = v.to_string().into_bytes();
        // One byte for the RFC 8291 delimiter.
        if bytes.len() < PADDED_LEN || (title_n == 0 && body_n == 0) {
            return bytes;
        }
        if body_n > 0 {
            body_n /= 2;
        } else {
            title_n /= 2;
        }
    }
}

/// How one delivery went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Delivered,
    /// 404/410: the subscription no longer exists; its row is deleted.
    Gone(u16),
    Failed(String),
}

/// Map a push service's HTTP status (RFC 8030 §5, plus what FCM answers).
pub fn outcome_of(status: u16) -> Outcome {
    match status {
        200..=299 => Outcome::Delivered,
        404 | 410 => Outcome::Gone(status),
        300..=399 => Outcome::Failed(format!("the push service redirected ({status}); redirects are not followed")),
        400 => Outcome::Failed("the push service refused the request (400)".into()),
        401 | 403 => Outcome::Failed(format!(
            "the push service refused this daemon's key ({status}); turn alerts off and on again on the phone"
        )),
        413 => Outcome::Failed("the push service says the alert is too large (413)".into()),
        429 => Outcome::Failed("the push service is rate limiting alerts (429)".into()),
        0 => Outcome::Failed("no answer from the push service".into()),
        s => Outcome::Failed(format!("the push service answered {s}")),
    }
}

/// A value inside a double-quoted curl config string. Refuses control characters (none of
/// ours have any; a URL is checked before it gets here).
fn curl_quote(s: &str) -> Result<String, String> {
    if s.chars().any(|c| c.is_control()) {
        return Err("value has control characters".into());
    }
    Ok(format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")))
}

/// curl's config (read from stdin) for one push.
pub fn push_config(endpoint: &str, authorization: &str, topic: Option<&str>, body_path: &str) -> Result<String, String> {
    let mut c = String::new();
    c.push_str(&format!("url = {}\n", curl_quote(endpoint)?));
    for h in [
        format!("TTL: {TTL_S}"),
        "Urgency: high".to_string(),
        "Content-Encoding: aes128gcm".to_string(),
        "Content-Type: application/octet-stream".to_string(),
        format!("Authorization: {authorization}"),
    ] {
        c.push_str(&format!("header = {}\n", curl_quote(&h)?));
    }
    if let Some(t) = topic {
        // RFC 8030 §5.4: a newer alert for the same session replaces one still waiting.
        c.push_str(&format!("header = {}\n", curl_quote(&format!("Topic: {t}"))?));
    }
    c.push_str(&format!("data-binary = {}\n", curl_quote(&format!("@{body_path}"))?));
    Ok(c)
}

fn user_agent() -> String {
    format!("Muster/{} (web push)", env!("CARGO_PKG_VERSION"))
}

/// Run curl with `config` on stdin: POST, only `protos` (`=https` for push endpoints), no
/// redirects, at most [`TIMEOUT_S`]. The HTTP status, 0 when nothing answered.
async fn curl_post(config: String, protos: &str) -> Result<u16, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let args = [
        "-q", "-sS", "--proto", protos, "--max-redirs", "0", "--max-time", &TIMEOUT_S.to_string(),
        "--connect-timeout", &TIMEOUT_S.to_string(), "--max-filesize", "65536", "-o", "/dev/null",
        "-w", "%{http_code}", "-A", &user_agent(), "-X", "POST", "-K", "-",
    ]
    .map(|s| s.to_string());
    let mut child = tokio::process::Command::new("curl")
        .args(&args)
        .env_remove("OPENROUTER_API_KEY")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { "curl is not installed".to_string() } else { format!("running curl: {e}") })?;
    let mut stdin = child.stdin.take().expect("piped");
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let work = async {
        stdin.write_all(config.as_bytes()).await.map_err(|e| format!("writing to curl: {e}"))?;
        drop(stdin);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let (a, b) = tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
        a.and(b).map_err(|e| format!("reading curl: {e}"))?;
        let status = child.wait().await.map_err(|e| format!("curl: {e}"))?;
        Ok::<_, String>((status, out, err))
    };
    let (status, out, err) = tokio::time::timeout(Duration::from_secs(TIMEOUT_S + 3), work)
        .await
        .map_err(|_| format!("no answer from the push service within {TIMEOUT_S} s"))??;
    let code: u16 = String::from_utf8_lossy(&out).trim().parse().unwrap_or(0);
    if code == 0 && !status.success() {
        let why = String::from_utf8_lossy(&err);
        let why = why.trim().trim_start_matches("curl: ");
        let why: String = why.chars().take(200).collect();
        return Err(if status.code() == Some(28) {
            format!("no answer from the push service within {TIMEOUT_S} s")
        } else if why.is_empty() {
            "couldn't reach the push service".into()
        } else {
            format!("couldn't reach the push service ({why})")
        });
    }
    Ok(code)
}

/// Encrypt and send `alert` to one subscription.
pub async fn deliver(vapid: &Vapid, sub: &Subscription, alert: &Alert, allow: &[String]) -> Outcome {
    let endpoint = match check_endpoint(&sub.endpoint, allow) {
        Ok(e) => e,
        Err(e) => return Outcome::Failed(e),
    };
    let (key, auth) = match check_keys(&sub.p256dh, &sub.auth) {
        Ok(k) => k,
        Err(e) => return Outcome::Failed(e),
    };
    let body = match encrypt(&key, &auth, &payload(alert, now_ms()), PADDED_LEN, &random_secret(), &rand::random()) {
        Ok(b) => b,
        Err(e) => return Outcome::Failed(e),
    };
    // The ciphertext goes through a private temp file (curl's stdin carries the config).
    let dir = crate::paths::runtime_dir();
    let path = dir.join(format!("push-{:016x}.bin", rand::random::<u64>()));
    if let Err(e) = crate::paths::write_private(&path, &body) {
        return Outcome::Failed(format!("writing the alert: {e}"));
    }
    let topic = alert.session_id.map(|s| format!("muster-s{s}"));
    let auth_header = vapid.authorization(&endpoint.origin, &subject(), now_ms() / 1000);
    let result = match push_config(&sub.endpoint, &auth_header, topic.as_deref(), &path.to_string_lossy()) {
        Ok(config) => curl_post(config, "=https").await,
        Err(e) => Err(e),
    };
    let _ = std::fs::remove_file(&path);
    match result {
        Ok(code) => outcome_of(code),
        Err(e) => Outcome::Failed(e),
    }
}

// ----- ntfy fallback -----

/// RFC 2047 encoded word: ntfy reads UTF-8 headers this way.
fn mime_word(s: &str) -> String {
    format!("=?UTF-8?B?{}?=", base64::engine::general_purpose::STANDARD.encode(s.as_bytes()))
}

/// curl config for one ntfy message (`MUSTER_NTFY_URL`): the text in the clear, so meant for a
/// self-hosted ntfy. `click`: where tapping it goes (the phone page, when one is paired).
pub fn ntfy_config(url: &str, alert: &Alert, click: Option<&str>) -> Result<String, String> {
    let ok_scheme = url.starts_with("https://") || url.starts_with("http://");
    if !ok_scheme || url.chars().any(|c| c.is_control() || c == ' ') {
        return Err(format!("{ENV_NTFY} must be an http(s) URL"));
    }
    let mut c = format!("url = {}\n", curl_quote(url)?);
    c.push_str(&format!("header = {}\n", curl_quote(&format!("Title: {}", mime_word(&clip_chars(&alert.title, 120))))?));
    c.push_str(&format!("header = {}\n", curl_quote("Priority: high")?));
    if let Some(click) = click {
        c.push_str(&format!("header = {}\n", curl_quote(&format!("Click: {click}"))?));
    }
    let body = clip_chars(&alert.body.replace(['\n', '\r', '\t'], " "), 400);
    // `data-raw`, never `data-binary`/`data`: those read a file when the text starts with `@`,
    // and the text is a session's state detail, which an agent's output shapes.
    c.push_str(&format!("data-raw = {}\n", curl_quote(&body)?));
    Ok(c)
}

async fn ntfy(hub: &Hub, url: &str, alert: &Alert) {
    let origin = hub.devices().any_origin();
    let click = match (origin, alert.session_id) {
        (Some(o), Some(id)) => Some(format!("{o}/m/#s={id}")),
        (Some(o), None) => Some(format!("{o}/m/")),
        _ => None,
    };
    let config = match ntfy_config(url, alert, click.as_deref()) {
        Ok(c) => c,
        Err(e) => return tracing::warn!("ntfy: {e}"),
    };
    // Unlike push endpoints, the owner chose this URL: http is allowed (a LAN ntfy).
    match curl_post(config, "=http,https").await {
        Ok(code) if (200..300).contains(&code) => {}
        Ok(code) => tracing::warn!("ntfy answered {code}"),
        Err(e) => tracing::warn!("ntfy: {e}"),
    }
}

// ----- the sink -----

/// Phone alerts (one sink of src/notify.rs). Never blocks the notify loop: each delivery runs
/// in its own task, at most [`CONCURRENCY`] curl processes at once.
pub struct Sink {
    hub: Arc<Hub>,
    gate: Arc<tokio::sync::Semaphore>,
    /// Deliveries started and not finished; past [`QUEUE_MAX`] new ones are dropped (a push
    /// service that hangs must not pile up tasks).
    queued: Arc<std::sync::atomic::AtomicUsize>,
}

impl Sink {
    pub fn new(hub: Arc<Hub>) -> Sink {
        Sink {
            hub,
            gate: Arc::new(tokio::sync::Semaphore::new(CONCURRENCY)),
            queued: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Send `alert` to every live device that has alerts on (and to ntfy when configured).
    pub fn send(&self, alert: Alert) {
        let targets = self.hub.devices().push_targets(self.hub.store());
        if let Some(url) = std::env::var(ENV_NTFY).ok().filter(|u| !u.trim().is_empty()) {
            let (hub, alert) = (self.hub.clone(), alert.clone());
            tokio::spawn(async move { ntfy(&hub, url.trim(), &alert).await });
        }
        if targets.is_empty() {
            return;
        }
        let vapid = match vapid() {
            Ok(v) => v,
            Err(e) => return tracing::warn!("phone alerts: {e:#}"),
        };
        let allow = Arc::new(allowed_hosts());
        use std::sync::atomic::Ordering;
        for (device, sub) in targets {
            if self.queued.fetch_add(1, Ordering::SeqCst) >= QUEUE_MAX {
                self.queued.fetch_sub(1, Ordering::SeqCst);
                tracing::warn!("device {device}: phone alert dropped ({QUEUE_MAX} deliveries already waiting)");
                continue;
            }
            let (hub, gate, alert, allow) = (self.hub.clone(), self.gate.clone(), alert.clone(), allow.clone());
            let queued = self.queued.clone();
            tokio::spawn(async move {
                if let Ok(_permit) = gate.acquire().await {
                    let outcome = deliver(vapid, &sub, &alert, &allow).await;
                    record(&hub, device, &sub.endpoint, &outcome);
                }
                queued.fetch_sub(1, Ordering::SeqCst);
            });
        }
    }
}

/// Keep a delivery's result on the subscription it went to (not a newer one), and log it.
pub fn record(hub: &Hub, device: i64, endpoint: &str, outcome: &Outcome) {
    match outcome {
        Outcome::Delivered => {}
        Outcome::Gone(code) => tracing::info!("device {device}: push subscription gone ({code}); removed"),
        Outcome::Failed(e) => tracing::warn!("device {device}: phone alert failed: {e}"),
    }
    hub.devices().record_push(hub.store(), device, endpoint, outcome);
}

/// `push.test`: one alert to the calling device now, awaited, so the phone can say at once
/// whether alerts work.
pub async fn test(hub: &Hub, device: i64) -> Result<Outcome, String> {
    static LAST: parking_lot::Mutex<Option<std::collections::HashMap<i64, std::time::Instant>>> =
        parking_lot::Mutex::new(None);
    let sub = hub.devices().push_subscription(device).ok_or("alerts are off on this device")?;
    {
        let mut g = LAST.lock();
        let map = g.get_or_insert_with(Default::default);
        let now = std::time::Instant::now();
        if map.get(&device).is_some_and(|t| now.duration_since(*t) < Duration::from_secs(TEST_GAP_S)) {
            return Err(format!("wait {TEST_GAP_S} seconds between test alerts"));
        }
        map.insert(device, now);
    }
    let vapid = vapid().map_err(|e| format!("{e:#}"))?;
    let alert = Alert {
        session_id: None,
        channel_id: None,
        state: None,
        state_since: now_ms(),
        state_seq: 0,
        title: "Muster".into(),
        body: "Test alert: alerts work on this phone.".into(),
    };
    let outcome = deliver(vapid, &sub, &alert, &allowed_hosts()).await;
    record(hub, device, &sub.endpoint, &outcome);
    Ok(outcome)
}

/// A browser-style `(p256dh, auth)` pair for tests elsewhere.
#[cfg(test)]
pub fn test_keys() -> (String, String) {
    (b64(&public_bytes(&random_secret())), b64(&rand::random::<[u8; 16]>()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow(extra: &[&str]) -> Vec<String> {
        DEFAULT_HOSTS.iter().chain(extra).map(|s| s.to_string()).collect()
    }

    /// RFC 8291 §5 and Appendix A.
    #[test]
    fn rfc8291_example() {
        let d = |s: &str| unb64(s).unwrap();
        let as_secret = p256::SecretKey::from_slice(&d("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw")).unwrap();
        assert_eq!(
            b64(&public_bytes(&as_secret)),
            "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8"
        );
        let ua_public: [u8; 65] =
            d("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4").try_into().unwrap();
        let auth: [u8; 16] = d("BTBZMqHH6r4Tts7J_aSIgg").try_into().unwrap();
        let salt: [u8; 16] = d("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap();

        // Intermediate values.
        let ua = p256::PublicKey::from_sec1_bytes(&ua_public).unwrap();
        let shared = as_secret.diffie_hellman(&ua);
        assert_eq!(b64(shared.raw_secret_bytes().as_slice()), "kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs");
        let mut key_info = b"WebPush: info\0".to_vec();
        key_info.extend_from_slice(&ua_public);
        key_info.extend_from_slice(&public_bytes(&as_secret));
        assert_eq!(
            b64(&key_info),
            "V2ViUHVzaDogaW5mbwAEJXGyvs3942BVGq8e0PTNNmwRzr5VX4m8t7GGpTM5FzFo7OLr4BhZe9MEebhuPI-OztV3ylkYfpJGmQ22ggCLDgT-M_SrDepxkU21WCP3O1SUj0EwbZIHMtu5pZpTKGSCIA5Zent7wmC6HCJ5mFgJkuk5cwAvMBKiiujwa7t45ewP"
        );
        let (prk_key, _) = hkdf::Hkdf::<sha2::Sha256>::extract(Some(&auth), shared.raw_secret_bytes().as_slice());
        assert_eq!(b64(prk_key.as_slice()), "Snr3JMxaHVDXHWJn5wdC52WjpCtd2EIEGBykDcZW32k");
        let mut ikm = [0u8; 32];
        hkdf_expand(&auth, shared.raw_secret_bytes().as_slice(), &key_info, &mut ikm);
        assert_eq!(b64(&ikm), "S4lYMb_L0FxCeq0WhDx813KgSYqU26kOyzWUdsXYyrg");
        let (prk, _) = hkdf::Hkdf::<sha2::Sha256>::extract(Some(&salt), &ikm);
        assert_eq!(b64(prk.as_slice()), "09_eUZGrsvxChDCGRCdkLiDXrReGOEVeSCdCcPBSJSc");
        let mut cek = [0u8; 16];
        hkdf_expand(&salt, &ikm, b"Content-Encoding: aes128gcm\0", &mut cek);
        assert_eq!(b64(&cek), "oIhVW04MRdy2XN9CiKLxTg");
        let mut nonce = [0u8; 12];
        hkdf_expand(&salt, &ikm, b"Content-Encoding: nonce\0", &mut nonce);
        assert_eq!(b64(&nonce), "4h_95klXJ5E_qnoN");

        // The whole message, with no padding beyond the delimiter.
        let msg = encrypt(&ua_public, &auth, b"When I grow up, I want to be a watermelon", 0, &as_secret, &salt).unwrap();
        assert_eq!(
            b64(&msg),
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN"
        );
    }

    /// The receiving side (as a browser does it), to check padded messages round-trip.
    fn decrypt(ua_secret: &p256::SecretKey, auth: &[u8; 16], msg: &[u8]) -> Vec<u8> {
        use aes_gcm::aead::{Aead, KeyInit};
        let salt = &msg[..16];
        assert_eq!(u32::from_be_bytes(msg[16..20].try_into().unwrap()), RECORD_SIZE);
        assert_eq!(msg[20], 65);
        let as_public = p256::PublicKey::from_sec1_bytes(&msg[21..86]).unwrap();
        let shared = ua_secret.diffie_hellman(&as_public);
        let mut key_info = b"WebPush: info\0".to_vec();
        key_info.extend_from_slice(&public_bytes(ua_secret));
        key_info.extend_from_slice(&msg[21..86]);
        let mut ikm = [0u8; 32];
        hkdf_expand(auth, shared.raw_secret_bytes().as_slice(), &key_info, &mut ikm);
        let (mut cek, mut nonce) = ([0u8; 16], [0u8; 12]);
        hkdf_expand(salt, &ikm, b"Content-Encoding: aes128gcm\0", &mut cek);
        hkdf_expand(salt, &ikm, b"Content-Encoding: nonce\0", &mut nonce);
        let cipher = aes_gcm::Aes128Gcm::new_from_slice(&cek).unwrap();
        let mut record = cipher.decrypt(&nonce.into(), &msg[86..]).unwrap();
        let end = record.iter().rposition(|b| *b != 0).unwrap();
        assert_eq!(record[end], 2, "last-record delimiter");
        record.truncate(end);
        record
    }

    #[test]
    fn alerts_are_padded_to_one_size_and_round_trip() {
        let ua_secret = random_secret();
        let ua_public = public_bytes(&ua_secret);
        let auth: [u8; 16] = rand::random();
        let mk = |title: &str, body: &str| Alert {
            session_id: Some(7),
            channel_id: Some(2),
            state: Some(crate::model::AgentState::NeedsApproval),
            state_since: 1_700_000_000_000,
            state_seq: 9,
            title: title.into(),
            body: body.into(),
        };
        let short = mk("#a · b", "Bash(ls)");
        let long = mk(&"t".repeat(5000), &"é\"\\x".repeat(3000));
        let mut sizes = Vec::new();
        for a in [&short, &long] {
            let p = payload(a, 1);
            assert!(p.len() < PADDED_LEN, "{}", p.len());
            let msg = encrypt(&ua_public, &auth, &p, PADDED_LEN, &random_secret(), &rand::random()).unwrap();
            sizes.push(msg.len());
            let back = decrypt(&ua_secret, &auth, &msg);
            assert_eq!(back, p);
            let v: serde_json::Value = serde_json::from_slice(&back).unwrap();
            assert_eq!(v["session_id"], 7);
            assert_eq!(v["state"], "needs_approval");
            assert_eq!(v["kind"], "session");
        }
        assert_eq!(sizes[0], sizes[1], "every alert has the same length");
        assert_eq!(sizes[0], 86 + PADDED_LEN + 16);
        let v: serde_json::Value = serde_json::from_slice(&payload(&short, 1)).unwrap();
        assert_eq!(v["body"], "Bash(ls)");
        assert_eq!(v["title"], "#a · b");
    }

    #[test]
    fn vapid_token_verifies_and_names_no_one() {
        use p256::ecdsa::signature::Verifier;
        let secret = random_secret();
        let v = Vapid::from_secret(&secret);
        let h = v.authorization("https://fcm.googleapis.com", DEFAULT_SUBJECT, 1_000);
        let rest = h.strip_prefix("vapid t=").unwrap();
        let (jwt, k) = rest.split_once(", k=").unwrap();
        assert_eq!(k, v.public_key);
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: serde_json::Value = serde_json::from_slice(&unb64(parts[0]).unwrap()).unwrap();
        assert_eq!(header["alg"], "ES256");
        let claims: serde_json::Value = serde_json::from_slice(&unb64(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://fcm.googleapis.com");
        assert_eq!(claims["exp"], 1_000 + 12 * 3600);
        assert_eq!(claims["sub"], DEFAULT_SUBJECT);
        let sig = p256::ecdsa::Signature::from_slice(&unb64(parts[2]).unwrap()).unwrap();
        let vk = p256::ecdsa::VerifyingKey::from(&p256::ecdsa::SigningKey::from(&secret));
        vk.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig).unwrap();
        assert_eq!(unb64(&v.public_key).unwrap().len(), 65);
    }

    #[test]
    fn vapid_key_is_created_once_and_kept() {
        let dir = std::env::temp_dir().join(format!("muster-vapid-{:x}", rand::random::<u64>()));
        let path = dir.join("push_vapid_key");
        let a = Vapid::load_or_create(&path).unwrap();
        let b = Vapid::load_or_create(&path).unwrap();
        assert_eq!(a.public_key, b.public_key);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::write(&path, "garbage!").unwrap();
        assert!(Vapid::load_or_create(&path).is_err(), "a damaged key is never silently replaced");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn endpoint_rules() {
        let a = allow(&[]);
        let good = check_endpoint("https://fcm.googleapis.com/fcm/send/abc:APA91b-x_y", &a).unwrap();
        assert_eq!(good.origin, "https://fcm.googleapis.com");
        assert_eq!(
            check_endpoint("https://updates.push.services.mozilla.com/wpush/v2/gAAA", &a).unwrap().origin,
            "https://updates.push.services.mozilla.com"
        );
        assert!(check_endpoint("https://wns2-par02p.notify.windows.com/w/?token=AQ%3d", &a).is_ok());
        assert_eq!(check_endpoint("https://fcm.googleapis.com:443/x", &a).unwrap().origin, "https://fcm.googleapis.com");
        for bad in [
            "http://fcm.googleapis.com/fcm/send/x",                 // not https
            "HTTPS://fcm.googleapis.com/x",                         // scheme spelled oddly
            "https://FCM.googleapis.com/x",                         // not lowercase
            "https://fcm.googleapis.com.evil.com/x",                // suffix trick
            "https://evilfcm.googleapis.com/x",                     // not the host
            "https://push.services.mozilla.com/x",                  // wildcard needs a subdomain
            "https://xpush.services.mozilla.com/x",                 // not a subdomain
            "https://fcm.googleapis.com:8443/x",                    // other port
            "https://user@fcm.googleapis.com/x",                    // user info
            "https://fcm.googleapis.com@evil.com/x",                // user info trick
            "https://fcm.googleapis.com./x",                        // trailing dot
            "https://127.0.0.1/x",                                  // not on the list
            "https://[::1]/x",                                      // IP literal
            "https://localhost/x",                                  // not on the list
            "https://fcm.googleapis.com/x#frag",                    // fragment
            "https://fcm.googleapis.com/x y",                       // space
            "https://fcm.googleapis.com/x\"\nheader = \"Evil: 1",   // config injection
            "https://fcm.googleapis.com\\@evil.com/",               // backslash
            "https://fcm.googleapis.com?x",                         // no path
            "",
        ] {
            assert!(check_endpoint(bad, &a).is_err(), "{bad:?} should be refused");
        }
        assert!(check_endpoint(&format!("https://fcm.googleapis.com/{}", "a".repeat(3000)), &a).is_err());
        // Extended by the owner: exact port, wildcard.
        let a = allow(&["localhost:8443", "*.push.example.org"]);
        assert_eq!(check_endpoint("https://localhost:8443/p/1", &a).unwrap().origin, "https://localhost:8443");
        assert!(check_endpoint("https://localhost/p/1", &a).is_err(), "an entry with a port allows only that port");
        assert!(check_endpoint("https://localhost:8444/p/1", &a).is_err());
        assert!(check_endpoint("https://a.push.example.org/p", &a).is_ok());
    }

    #[test]
    fn subscription_keys_are_checked() {
        let pk = b64(&public_bytes(&random_secret()));
        let auth = b64(&[7u8; 16]);
        assert!(check_keys(&pk, &auth).is_ok());
        // Padded standard base64 is accepted too.
        let std_pk = base64::engine::general_purpose::STANDARD.encode(unb64(&pk).unwrap());
        assert!(check_keys(&std_pk, &auth).is_ok());
        assert!(check_keys(&pk, &b64(&[7u8; 15])).is_err());
        assert!(check_keys(&b64(&[4u8; 65]), &auth).is_err(), "not a point on the curve");
        assert!(check_keys("not base64!", &auth).is_err());
    }

    #[test]
    fn curl_config_is_quoted_and_complete() {
        let c = push_config("https://fcm.googleapis.com/fcm/send/x", "vapid t=a.b.c, k=K", Some("muster-s3"), "/run/a \"b\"\\c.bin").unwrap();
        assert!(c.contains("url = \"https://fcm.googleapis.com/fcm/send/x\"\n"));
        for h in ["TTL: 300", "Urgency: high", "Content-Encoding: aes128gcm", "Authorization: vapid t=a.b.c, k=K", "Topic: muster-s3"] {
            assert!(c.contains(&format!("header = \"{h}\"\n")), "{h}: {c}");
        }
        assert!(c.contains(r#"data-binary = "@/run/a \"b\"\\c.bin""#), "{c}");
        assert_eq!(c.lines().count(), 8);
        assert!(push_config("https://x/\n", "a", None, "/p").is_err());
    }

    #[test]
    fn statuses_map_to_outcomes() {
        assert_eq!(outcome_of(201), Outcome::Delivered);
        assert_eq!(outcome_of(410), Outcome::Gone(410));
        assert_eq!(outcome_of(404), Outcome::Gone(404));
        assert!(matches!(outcome_of(302), Outcome::Failed(e) if e.contains("redirect")));
        assert!(matches!(outcome_of(403), Outcome::Failed(e) if e.contains("off and on")));
        assert!(matches!(outcome_of(0), Outcome::Failed(_)));
    }

    #[test]
    fn ntfy_config_encodes_the_title() {
        let a = Alert {
            session_id: Some(4),
            channel_id: None,
            state: None,
            state_since: 0,
            state_seq: 0,
            title: "#web · claude".into(),
            body: "Bash(\"rm\")\nnext".into(),
        };
        let c = ntfy_config("https://ntfy.example.org/muster", &a, Some("https://box.ts.net/m/#s=4")).unwrap();
        assert!(c.contains("url = \"https://ntfy.example.org/muster\""));
        assert!(c.contains(&format!("Title: {}", mime_word("#web · claude"))));
        assert!(c.contains("Click: https://box.ts.net/m/#s=4"));
        assert!(c.contains(r#"data-raw = "Bash(\"rm\") next""#), "{c}");
        // Agent-shaped text that looks like curl's "read this file" is sent as text.
        let sneaky = Alert { body: "@/home/me/.ssh/id_ed25519".into(), ..a.clone() };
        let c = ntfy_config("https://ntfy.example.org/muster", &sneaky, None).unwrap();
        assert!(c.contains("data-raw = \"@/home/me/.ssh/id_ed25519\"\n") && !c.contains("data-binary"), "{c}");
        assert!(ntfy_config("file:///etc/passwd", &a, None).is_err());
    }
}
