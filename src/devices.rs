//! Paired remote devices (docs/ANDROID.md): phones on the tailnet that reach the daemon through
//! `tailscale serve` and authenticate with an HttpOnly cookie. The registry keeps every device
//! in memory and writes through to the store, like the Hub; tokens are stored as SHA-256
//! hashes only. Pairing codes live in memory only.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::model::now_ms;
use crate::paths;
use crate::store::Store;

/// Device cookie. `__Host-`: the browser only accepts it with `Secure`, `Path=/` and no
/// `Domain`, so no other tailnet host can plant or scope one.
pub const COOKIE: &str = "__Host-muster_device";
/// Sliding lifetime of a device: renewed on use (at most once a day).
pub const LIFETIME_MS: i64 = 30 * 24 * 60 * 60 * 1000;
const RENEW_AFTER_MS: i64 = 24 * 60 * 60 * 1000;
pub const PAIR_TTL_DEFAULT_S: u64 = 120;
const PAIR_TTL_MAX_S: u64 = 600;
/// Wrong codes before the pending pairing is burnt.
const MAX_PAIR_FAILS: u8 = 3;
/// Pairing attempts per source per minute.
const PAIR_RATE: usize = 5;
/// Short-code alphabet: base32 without I, O, 0, 1 (9 chars = 45 bits).
const SHORT_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const SHORT_LEN: usize = 9;
/// Pairing outcomes remembered for `device.pair_status`.
const OUTCOMES_MAX: usize = 16;

/// What a device may do. Default deny; `Manage` ⊃ `Interact` ⊃ `View`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    View,
    Interact,
    Manage,
}

impl Scope {
    /// `scopes` as stored/requested -> the highest one (a scope implies the ones below it).
    pub fn highest(scopes: &[Scope]) -> Option<Scope> {
        scopes.iter().copied().max()
    }

    /// The scope and every scope below it, for display.
    pub fn implied(self) -> Vec<Scope> {
        [Scope::View, Scope::Interact, Scope::Manage]
            .into_iter()
            .filter(|s| *s <= self)
            .collect()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Device {
    pub id: i64,
    pub name: String,
    pub scopes: Vec<Scope>,
    /// `Tailscale-User-Login` seen at pairing (when the proxy headers were trusted).
    pub ts_login: Option<String>,
    pub created_at: i64,
    pub last_seen_at: i64,
    pub last_source: Option<String>,
    pub expires_at: i64,
    pub revoked_at: Option<i64>,
    #[serde(skip)]
    pub token_hash: String,
}

impl Device {
    pub fn live(&self, now: i64) -> bool {
        self.revoked_at.is_none() && self.expires_at > now
    }

    pub fn scope(&self) -> Option<Scope> {
        Scope::highest(&self.scopes)
    }
}

/// Who is making a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    /// Client token (desktop UI, CLI).
    Local,
    /// A paired device (cookie).
    Device { id: i64 },
}

impl Principal {
    /// `from` for prompts and approvals in the event log.
    pub fn label(&self) -> String {
        match self {
            Principal::Local => "human".into(),
            Principal::Device { id } => format!("device:{id}"),
        }
    }

    /// `SessionInfo.size_owner` after this principal resized a session.
    pub fn size_owner(&self) -> String {
        match self {
            Principal::Local => "local".into(),
            Principal::Device { id } => format!("device:{id}"),
        }
    }
}

/// The scope a device needs for `method` (JSON-RPC over `/ws` or `/api/rpc`). None: never
/// callable by a device.
pub fn method_scope(method: &str) -> Option<Scope> {
    Some(match method {
        "hello" | "session.attach" | "session.detach" | "session.screen" | "session.output" | "session.wait"
        | "channel.timeline" | "channel.diff" | "session.queue"
        // Phone alerts: a device's own subscription (src/push.rs); any paired device may get them.
        | "push.key" | "push.status" | "push.subscribe" | "push.unsubscribe" | "push.test" => Scope::View,
        "session.send" | "session.input" | "session.approve" | "session.interrupt" | "session.clear_queue"
        | "session.resume_queue" | "session.continue_input"
        | "session.resize" | "channel.mark_read" | "channel.post" => Scope::Interact,
        "channel.create" | "channel.archive" | "channel.rename" | "session.spawn" | "session.kill"
        | "session.restart" | "session.remove" | "session.rename" | "workspace.add" | "fs.complete" => {
            Scope::Manage
        }
        _ => return None,
    })
}

/// Params a device may never pass, even with `manage`: a permission profile or extra agent
/// arguments (both widen what an agent may do; those stay with the human at the desktop).
pub fn forbidden_param(params: &serde_json::Value) -> Option<&'static str> {
    fn walk(v: &serde_json::Value) -> Option<&'static str> {
        match v {
            serde_json::Value::Object(m) => {
                for (k, v) in m {
                    if k == "permissions" && !v.is_null() {
                        return Some("permissions");
                    }
                    if (k == "extra_args" || k == "args") && v.as_array().is_some_and(|a| !a.is_empty()) {
                        return Some("extra_args");
                    }
                    if let Some(f) = walk(v) {
                        return Some(f);
                    }
                }
                None
            }
            serde_json::Value::Array(a) => a.iter().find_map(walk),
            _ => None,
        }
    }
    walk(params)
}

/// A 9-character short code as typed: case-insensitive, spaces and dashes ignored.
pub fn normalize_short(code: &str) -> String {
    code.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

fn random_short() -> String {
    (0..SHORT_LEN)
        .map(|_| SHORT_ALPHABET[rand::random_range(0..SHORT_ALPHABET.len())] as char)
        .collect()
}

pub fn hash_token(token: &str) -> String {
    sha256(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

struct Pending {
    id: u64,
    code: String,
    short: String,
    expires: Instant,
    fails: u8,
    name: Option<String>,
    scopes: Vec<Scope>,
}

/// `device.pair_status` answer.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PairOutcome {
    Pending,
    Paired { device: Device },
    /// Too many wrong codes.
    Burnt,
    Expired,
    /// A newer `muster pair` took its place.
    Replaced,
}

pub struct NewPairing {
    pub id: u64,
    pub code: String,
    pub short_code: String,
    pub expires_at: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PairError {
    RateLimited,
    Invalid,
}

#[derive(Default)]
struct Inner {
    devices: HashMap<i64, Device>,
    pending: Option<Pending>,
    next_pairing: u64,
    outcomes: VecDeque<(u64, PairOutcome)>,
    attempts: HashMap<String, VecDeque<Instant>>,
    origins: HashSet<String>,
    /// Phone alerts (src/push.rs): each device's Web Push subscription, at most one.
    push: HashMap<i64, crate::push::Subscription>,
}

pub struct Registry {
    inner: Mutex<Inner>,
    /// Device ids whose sockets must close now (revoked).
    revoked_tx: broadcast::Sender<i64>,
}

impl Registry {
    pub fn load(store: &Store) -> anyhow::Result<Registry> {
        let devices = store.load_devices()?.into_iter().map(|d| (d.id, d)).collect();
        let origins = store.load_remote_origins()?.into_iter().collect();
        let push = store.load_push_subscriptions()?.into_iter().collect();
        Ok(Registry {
            inner: Mutex::new(Inner {
                devices,
                origins,
                push,
                next_pairing: 1,
                ..Default::default()
            }),
            revoked_tx: broadcast::channel(64).0,
        })
    }

    pub fn subscribe_revoked(&self) -> broadcast::Receiver<i64> {
        self.revoked_tx.subscribe()
    }

    // ----- remote origins -----

    pub fn origin_allowed(&self, origin: &str) -> bool {
        self.inner.lock().origins.contains(origin)
    }

    pub fn add_origin(&self, store: &Store, origin: &str) -> anyhow::Result<()> {
        let mut g = self.inner.lock();
        if g.origins.insert(origin.to_string()) {
            store.insert_remote_origin(origin, now_ms())?;
        }
        Ok(())
    }

    /// One phone page origin (for the ntfy fallback's tap link), the same one each time.
    pub fn any_origin(&self) -> Option<String> {
        self.inner.lock().origins.iter().min().cloned()
    }

    // ----- phone alerts (src/push.rs) -----

    pub fn push_subscription(&self, id: i64) -> Option<crate::push::Subscription> {
        self.inner.lock().push.get(&id).cloned()
    }

    /// Replace a live device's subscription. A new endpoint starts with a clean status; the same
    /// endpoint sent again (the phone re-checking) keeps its history.
    pub fn set_push(&self, store: &Store, id: i64, mut sub: crate::push::Subscription) -> anyhow::Result<()> {
        let mut g = self.inner.lock();
        if !g.devices.get(&id).is_some_and(|d| d.live(now_ms())) {
            anyhow::bail!("device revoked or expired");
        }
        if let Some(old) = g.push.get(&id)
            && old.endpoint == sub.endpoint
        {
            sub.created_at = old.created_at;
            sub.last_ok_at = old.last_ok_at;
            sub.last_error = old.last_error.clone();
            sub.last_error_at = old.last_error_at;
        }
        store.put_push_subscription(id, &sub)?;
        g.push.insert(id, sub);
        Ok(())
    }

    /// Turn a device's alerts off. True when it had a subscription.
    pub fn remove_push(&self, store: &Store, id: i64) -> anyhow::Result<bool> {
        let mut g = self.inner.lock();
        store.delete_push_subscription(id)?;
        Ok(g.push.remove(&id).is_some())
    }

    /// Subscriptions to send an alert to: live devices only. A revoked or expired device's row
    /// is deleted here (revocation deletes it already; expiry is noticed on the next alert).
    pub fn push_targets(&self, store: &Store) -> Vec<(i64, crate::push::Subscription)> {
        let now = now_ms();
        let mut g = self.inner.lock();
        let dead: Vec<i64> = g
            .push
            .keys()
            .copied()
            .filter(|id| !g.devices.get(id).is_some_and(|d| d.live(now)))
            .collect();
        for id in dead {
            if let Err(e) = store.delete_push_subscription(id) {
                tracing::warn!("deleting device {id}'s push subscription: {e:#}");
                continue;
            }
            g.push.remove(&id);
            tracing::info!("device {id} is no longer live; its push subscription was deleted");
        }
        let mut v: Vec<_> = g.push.iter().map(|(id, s)| (*id, s.clone())).collect();
        v.sort_by_key(|(id, _)| *id);
        v
    }

    /// A delivery's result, kept only if `endpoint` is still the device's subscription (the
    /// phone may have subscribed again meanwhile). Gone (404/410) deletes the row.
    pub fn record_push(&self, store: &Store, id: i64, endpoint: &str, outcome: &crate::push::Outcome) {
        use crate::push::Outcome;
        let now = now_ms();
        let mut g = self.inner.lock();
        if !g.push.get(&id).is_some_and(|s| s.endpoint == endpoint) {
            return;
        }
        if matches!(outcome, Outcome::Gone(_)) {
            match store.delete_push_subscription(id) {
                Ok(()) => {
                    g.push.remove(&id);
                }
                Err(e) => tracing::warn!("deleting device {id}'s push subscription: {e:#}"),
            }
            return;
        }
        let sub = g.push.get_mut(&id).expect("checked above");
        let result = match outcome {
            Outcome::Gone(_) => Ok(()),
            Outcome::Delivered => {
                sub.last_ok_at = Some(now);
                sub.last_error = None;
                store.put_push_subscription(id, sub)
            }
            Outcome::Failed(e) => {
                sub.last_error = Some(e.clone());
                sub.last_error_at = Some(now);
                store.put_push_subscription(id, sub)
            }
        };
        if let Err(e) = result {
            tracing::warn!("saving device {id}'s push status: {e:#}");
        }
    }

    // ----- pairing -----

    /// Start a pairing (replacing any pending one).
    pub fn begin_pairing(&self, ttl_s: u64, name: Option<String>, scopes: Vec<Scope>) -> NewPairing {
        let ttl = Duration::from_secs(ttl_s.clamp(10, PAIR_TTL_MAX_S));
        let mut g = self.inner.lock();
        if let Some(old) = g.pending.take() {
            push_outcome(&mut g, old.id, PairOutcome::Replaced);
        }
        let id = g.next_pairing;
        g.next_pairing += 1;
        let p = Pending {
            id,
            code: paths::random_token(),
            short: random_short(),
            expires: Instant::now() + ttl,
            fails: 0,
            name,
            scopes,
        };
        let out = NewPairing {
            id,
            code: p.code.clone(),
            short_code: p.short.clone(),
            expires_at: now_ms() + ttl.as_millis() as i64,
        };
        g.pending = Some(p);
        push_outcome(&mut g, id, PairOutcome::Pending);
        out
    }

    pub fn pair_status(&self, id: u64) -> Option<PairOutcome> {
        let mut g = self.inner.lock();
        expire_pending(&mut g);
        g.outcomes.iter().find(|(i, _)| *i == id).map(|(_, o)| o.clone())
    }

    /// Redeem a pairing code (long or short) from `source`. On success the new device and its
    /// token (for the cookie; never stored). The pending pairing is consumed under the lock,
    /// so of two concurrent redemptions exactly one wins.
    pub fn redeem(
        &self,
        store: &Store,
        code: &str,
        name: Option<String>,
        source: &str,
        ts_login: Option<String>,
    ) -> Result<(Device, String), PairError> {
        let mut g = self.inner.lock();
        let now = Instant::now();
        let bucket = g.attempts.entry(source.to_string()).or_default();
        while bucket.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60)) {
            bucket.pop_front();
        }
        if bucket.len() >= PAIR_RATE {
            return Err(PairError::RateLimited);
        }
        bucket.push_back(now);
        g.attempts.retain(|_, b| !b.is_empty());
        expire_pending(&mut g);
        let Some(p) = g.pending.as_mut() else {
            return Err(PairError::Invalid);
        };
        let code = code.trim();
        let ok = (code.len() == 64 && paths::token_eq(&code.to_ascii_lowercase(), &p.code))
            || paths::token_eq(&normalize_short(code), &p.short);
        if !ok {
            p.fails += 1;
            if p.fails >= MAX_PAIR_FAILS {
                let id = p.id;
                g.pending = None;
                push_outcome(&mut g, id, PairOutcome::Burnt);
                tracing::warn!("pairing burnt after {MAX_PAIR_FAILS} wrong codes (last from {source})");
            }
            return Err(PairError::Invalid);
        }
        let p = g.pending.take().expect("checked above");
        let token = paths::random_token();
        let t = now_ms();
        let name = p
            .name
            .or(name)
            .map(|n| n.trim().chars().take(60).collect::<String>())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "Phone".into());
        let mut dev = Device {
            id: 0,
            name,
            scopes: Scope::highest(&p.scopes).unwrap_or(Scope::View).implied(),
            ts_login,
            created_at: t,
            last_seen_at: t,
            last_source: Some(source.to_string()),
            expires_at: t + LIFETIME_MS,
            revoked_at: None,
            token_hash: hash_token(&token),
        };
        match store.insert_device(&dev) {
            Ok(id) => dev.id = id,
            Err(e) => {
                tracing::warn!("storing a paired device: {e:#}");
                // The code is spent either way; `muster pair` again.
                push_outcome(&mut g, p.id, PairOutcome::Expired);
                return Err(PairError::Invalid);
            }
        }
        g.devices.insert(dev.id, dev.clone());
        push_outcome(&mut g, p.id, PairOutcome::Paired { device: dev.clone() });
        tracing::info!("paired device {} ({}) from {source}", dev.id, dev.name);
        Ok((dev, token))
    }

    // ----- authentication -----

    /// The live device a cookie token belongs to.
    pub fn authenticate(&self, token: &str) -> Option<Device> {
        if token.len() != 64 {
            return None;
        }
        let h = hash_token(token);
        let now = now_ms();
        let g = self.inner.lock();
        g.devices
            .values()
            .find(|d| paths::token_eq(&d.token_hash, &h))
            .filter(|d| d.live(now))
            .cloned()
    }

    /// The device's scope now (None once revoked or expired). Checked on every call.
    pub fn scope(&self, id: i64) -> Option<Scope> {
        let now = now_ms();
        self.inner.lock().devices.get(&id).filter(|d| d.live(now)).and_then(|d| d.scope())
    }

    /// Record use; slides the expiry at most once a day. True when it was renewed (the caller
    /// re-sends the cookie with a fresh Max-Age).
    pub fn touch(&self, store: &Store, id: i64, source: &str) -> bool {
        let now = now_ms();
        let mut g = self.inner.lock();
        let Some(d) = g.devices.get_mut(&id) else {
            return false;
        };
        if !d.live(now) || now - d.last_seen_at < RENEW_AFTER_MS {
            return false;
        }
        d.last_seen_at = now;
        d.last_source = Some(source.to_string());
        d.expires_at = now + LIFETIME_MS;
        if let Err(e) = store.update_device(d) {
            tracing::warn!("updating device {id}: {e:#}");
        }
        true
    }

    pub fn list(&self) -> Vec<Device> {
        let mut v: Vec<Device> = self.inner.lock().devices.values().cloned().collect();
        v.sort_by_key(|d| d.id);
        v
    }

    /// Revoke devices (`None` = all live ones). Their sockets are closed. Returns the ids.
    pub fn revoke(&self, store: &Store, id: Option<i64>) -> anyhow::Result<Vec<i64>> {
        let now = now_ms();
        let mut out = Vec::new();
        {
            let mut g = self.inner.lock();
            if let Some(id) = id
                && !g.devices.contains_key(&id)
            {
                anyhow::bail!("no such device: {id}");
            }
            for d in g.devices.values_mut() {
                if id.is_some_and(|i| i != d.id) || d.revoked_at.is_some() {
                    continue;
                }
                d.revoked_at = Some(now);
                store.update_device(d)?;
                out.push(d.id);
            }
            // A revoked device gets no more alerts: its subscription goes with it.
            for id in &out {
                store.delete_push_subscription(*id)?;
                g.push.remove(id);
            }
        }
        for id in &out {
            tracing::info!("revoked device {id}");
            let _ = self.revoked_tx.send(*id);
        }
        Ok(out)
    }
}

fn expire_pending(g: &mut Inner) {
    if g.pending.as_ref().is_some_and(|p| p.expires <= Instant::now()) {
        let id = g.pending.take().expect("checked").id;
        push_outcome(g, id, PairOutcome::Expired);
    }
}

fn push_outcome(g: &mut Inner, id: u64, o: PairOutcome) {
    g.outcomes.retain(|(i, _)| *i != id);
    g.outcomes.push_back((id, o));
    while g.outcomes.len() > OUTCOMES_MAX {
        g.outcomes.pop_front();
    }
}

/// SHA-256 (FIPS 180-4). Only hashes device tokens, so a small local implementation instead of
/// a dependency.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64).wrapping_mul(8)).to_be_bytes());
    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, c) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 32];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn sha256_vectors() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // Crosses a block boundary with the length in the second block.
        assert_eq!(
            hex(&sha256(&[b'a'; 1000])),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
    }

    #[test]
    fn scopes_table() {
        assert_eq!(method_scope("hello"), Some(Scope::View));
        assert_eq!(method_scope("session.attach"), Some(Scope::View));
        assert_eq!(method_scope("session.approve"), Some(Scope::Interact));
        assert_eq!(method_scope("session.input"), Some(Scope::Interact));
        assert_eq!(method_scope("session.spawn"), Some(Scope::Manage));
        for m in [
            "device.pair_code",
            "device.list",
            "device.revoke",
            "channel.set_permissions",
            "channel.set_layout",
            "channel.reorder",
            "channel.move",
            "window.claim",
            "window.show",
            "workspace.remove",
            "session.set_window",
            "session.move",
            "nonsense",
        ] {
            assert_eq!(method_scope(m), None, "{m}");
        }
        assert!(Scope::Manage > Scope::Interact && Scope::Interact > Scope::View);
        assert_eq!(Scope::Interact.implied(), vec![Scope::View, Scope::Interact]);
    }

    #[test]
    fn forbidden_params() {
        use serde_json::json;
        assert_eq!(forbidden_param(&json!({"name": "x"})), None);
        assert_eq!(forbidden_param(&json!({"permissions": {"claude_mode": "bypass"}})), Some("permissions"));
        assert_eq!(forbidden_param(&json!({"permissions": null})), None);
        assert_eq!(forbidden_param(&json!({"extra_args": []})), None);
        assert_eq!(forbidden_param(&json!({"session": {"extra_args": ["--x"]}})), Some("extra_args"));
        // `channel.create` accepts `args` as an alias.
        assert_eq!(forbidden_param(&json!({"agent": "shell", "args": ["-c", "x"]})), Some("extra_args"));
    }

    fn reg() -> (Registry, Store) {
        let store = Store::open_memory().unwrap();
        (Registry::load(&store).unwrap(), store)
    }

    #[test]
    fn pairing_long_and_short_code_single_use() {
        let (r, s) = reg();
        let p = r.begin_pairing(120, Some("pixel".into()), vec![Scope::Interact]);
        assert_eq!(p.short_code.len(), SHORT_LEN);
        let (d, tok) = r.redeem(&s, &p.code, None, "a", None).unwrap();
        assert_eq!(d.name, "pixel");
        assert_eq!(d.scopes, vec![Scope::View, Scope::Interact]);
        assert_eq!(r.authenticate(&tok).map(|x| x.id), Some(d.id));
        assert_eq!(r.scope(d.id), Some(Scope::Interact));
        // Consumed: neither code works again.
        assert_eq!(r.redeem(&s, &p.code, None, "a", None).unwrap_err(), PairError::Invalid);
        assert_eq!(r.redeem(&s, &p.short_code, None, "a", None).unwrap_err(), PairError::Invalid);
        assert!(matches!(r.pair_status(p.id), Some(PairOutcome::Paired { .. })));

        // Short code, typed loosely.
        let p = r.begin_pairing(120, None, vec![Scope::View]);
        let typed = format!("{}-{} ", &p.short_code[..4].to_lowercase(), &p.short_code[4..]);
        let (d2, _) = r.redeem(&s, &typed, Some("mine".into()), "b", None).unwrap();
        assert_eq!(d2.name, "mine");
        assert_eq!(r.scope(d2.id), Some(Scope::View));
        // Persisted: a fresh registry over the same store knows both.
        let r2 = Registry::load(&s).unwrap();
        assert_eq!(r2.authenticate(&tok).map(|x| x.id), Some(d.id));
    }

    #[test]
    fn pairing_burns_after_three_wrong_codes_and_rate_limits() {
        let (r, s) = reg();
        let p = r.begin_pairing(120, None, vec![Scope::Interact]);
        for src in ["x", "y", "z"] {
            assert_eq!(r.redeem(&s, "WRONGCODE", None, src, None).unwrap_err(), PairError::Invalid);
        }
        assert!(matches!(r.pair_status(p.id), Some(PairOutcome::Burnt)));
        assert_eq!(r.redeem(&s, &p.code, None, "x", None).unwrap_err(), PairError::Invalid);

        let _p = r.begin_pairing(120, None, vec![Scope::Interact]);
        for _ in 0..PAIR_RATE {
            let _ = r.redeem(&s, "", None, "flood", None);
            let _ = r.begin_pairing(120, None, vec![Scope::Interact]); // keep one pending
        }
        assert_eq!(r.redeem(&s, "", None, "flood", None).unwrap_err(), PairError::RateLimited);
    }

    #[test]
    fn new_pairing_replaces_old() {
        let (r, s) = reg();
        let a = r.begin_pairing(120, None, vec![Scope::Interact]);
        let b = r.begin_pairing(120, None, vec![Scope::Interact]);
        assert!(matches!(r.pair_status(a.id), Some(PairOutcome::Replaced)));
        assert!(r.redeem(&s, &a.code, None, "s", None).is_err());
        assert!(r.redeem(&s, &b.code, None, "s", None).is_ok());
    }

    #[test]
    fn revoke_closes_and_refuses() {
        let (r, s) = reg();
        let p = r.begin_pairing(120, None, vec![Scope::Interact]);
        let (d, tok) = r.redeem(&s, &p.code, None, "s", None).unwrap();
        let mut rx = r.subscribe_revoked();
        assert_eq!(r.revoke(&s, Some(d.id)).unwrap(), vec![d.id]);
        assert_eq!(rx.try_recv().unwrap(), d.id);
        assert!(r.authenticate(&tok).is_none());
        assert_eq!(r.scope(d.id), None);
        assert!(r.revoke(&s, Some(999)).is_err());
        let r2 = Registry::load(&s).unwrap();
        assert!(r2.authenticate(&tok).is_none());
    }

    fn sub(endpoint: &str) -> crate::push::Subscription {
        crate::push::Subscription {
            endpoint: endpoint.into(),
            p256dh: "k".into(),
            auth: "a".into(),
            created_at: 1,
            last_ok_at: None,
            last_error: None,
            last_error_at: None,
        }
    }

    #[test]
    fn push_subscriptions_follow_the_device() {
        use crate::push::Outcome;
        let (r, s) = reg();
        let pair = |r: &Registry| {
            let p = r.begin_pairing(120, None, vec![Scope::View]);
            r.redeem(&s, &p.code, None, "s", None).unwrap().0
        };
        let (a, b, c) = (pair(&r), pair(&r), pair(&r));
        for d in [&a, &b, &c] {
            r.set_push(&s, d.id, sub(&format!("https://fcm.googleapis.com/{}", d.id))).unwrap();
        }
        assert_eq!(r.push_targets(&s).len(), 3);
        // Persisted.
        assert_eq!(Registry::load(&s).unwrap().push_targets(&s).len(), 3);

        // Delivery status sticks to the endpoint it was for.
        let ea = format!("https://fcm.googleapis.com/{}", a.id);
        r.record_push(&s, a.id, &ea, &Outcome::Failed("boom".into()));
        assert_eq!(r.push_subscription(a.id).unwrap().last_error.as_deref(), Some("boom"));
        r.record_push(&s, a.id, &ea, &Outcome::Delivered);
        let now = r.push_subscription(a.id).unwrap();
        assert!(now.last_ok_at.is_some() && now.last_error.is_none());
        // Same endpoint again keeps its history; a new one starts clean.
        r.set_push(&s, a.id, sub(&ea)).unwrap();
        assert!(r.push_subscription(a.id).unwrap().last_ok_at.is_some());
        r.set_push(&s, a.id, sub("https://fcm.googleapis.com/new")).unwrap();
        assert!(r.push_subscription(a.id).unwrap().last_ok_at.is_none());
        // A result for the old endpoint no longer touches it, not even a 410.
        r.record_push(&s, a.id, &ea, &Outcome::Gone(410));
        assert!(r.push_subscription(a.id).is_some());
        r.record_push(&s, a.id, "https://fcm.googleapis.com/new", &Outcome::Gone(410));
        assert!(r.push_subscription(a.id).is_none(), "404/410 delete the row");

        // Revoke deletes it, in the store too.
        r.revoke(&s, Some(b.id)).unwrap();
        assert!(r.push_subscription(b.id).is_none());
        assert!(Registry::load(&s).unwrap().push_subscription(b.id).is_none());
        assert!(r.set_push(&s, b.id, sub("https://fcm.googleapis.com/b")).is_err(), "a revoked device can't subscribe");

        // Expiry: noticed on the next alert.
        r.inner.lock().devices.get_mut(&c.id).unwrap().expires_at = now_ms() - 1;
        assert!(r.push_targets(&s).is_empty());
        assert!(Registry::load(&s).unwrap().push_subscription(c.id).is_none());

        assert!(!r.remove_push(&s, a.id).unwrap());
    }
}
