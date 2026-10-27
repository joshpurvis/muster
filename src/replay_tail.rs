//! Raw output tails for cheap phone reconnects (docs/ANDROID.md "Reconnect and battery",
//! docs/PROTOCOL.md `attach_info`).
//!
//! A client that attaches with `replay: "screen"` or `replay: {since}` (the phone) makes the
//! daemon keep the last `DELTA_MAX` bytes of that session's raw output in memory, fed by a
//! host subscription of its own. The tail keeps running while the client is attached and for
//! `KEEP` after it leaves, and stops as soon as it has rolled past the furthest offset a
//! departed client reached (no `since` could be served from it any more). A client that comes
//! back with the offset it had reached gets just the bytes since then (`delta`), appended to
//! the terminal it still has, instead of a screen that replaces it.
//!
//! Everything lives in the daemon: the terminal host only provides the ordinary output
//! stream, so this needs no host change and no full restart.
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::{broadcast, watch, Notify};

use crate::model::SessionId;
use crate::terminal::Output;

/// The largest `delta` replay; a longer absence gets a `screen`.
pub const DELTA_MAX: usize = 64 * 1024;
/// How long a tail outlives the last client that left it.
const KEEP: Duration = Duration::from_secs(30 * 60);
/// Tails at a time (each holds one host output stream and up to `DELTA_MAX` bytes). A new one
/// evicts the one left longest ago; when every tail has a client attached, none is added.
const MAX_TAILS: usize = 8;
/// How long a `delta` waits for the tail's own stream to reach the new attachment's offset.
const CATCH_UP: Duration = Duration::from_millis(500);
/// How often an idle tail checks whether it is still wanted.
const SWEEP: Duration = Duration::from_secs(30);

pub struct Tails {
    /// Random per daemon start: offsets and generations are only comparable within it.
    epoch: String,
    slots: Mutex<Slots>,
}

#[derive(Default)]
struct Slots {
    map: HashMap<SessionId, Arc<Tail>>,
    /// Tails being started (room taken, stream not open yet): counted against `MAX_TAILS`.
    reserved: usize,
}

impl std::fmt::Debug for Tails {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tails").field("epoch", &self.epoch).finish_non_exhaustive()
    }
}

impl Default for Tails {
    fn default() -> Self {
        Tails { epoch: format!("{:016x}", rand::random::<u64>()), slots: Mutex::default() }
    }
}

/// One session run's tail.
pub struct Tail {
    pub generation: u64,
    state: Mutex<State>,
    /// The tail's end offset, for `delta` waiting on it to catch up.
    end: watch::Sender<u64>,
    /// Ends the keeper (evicted or replaced).
    stop: Notify,
}

#[derive(Debug)]
struct State {
    /// Offset of `buf[0]`.
    start: u64,
    buf: VecDeque<u8>,
    /// Clients attached through this tail now.
    users: usize,
    /// When the last client left (None while one is attached, or none ever left).
    left_at: Option<Instant>,
    /// The furthest offset any departed client reached: a `since` is never past it.
    left_end: u64,
    /// The tail's stream failed (lagged or ended): it serves nothing more.
    dead: bool,
}

impl State {
    fn new(start: u64) -> State {
        State { start, buf: VecDeque::new(), users: 0, left_at: None, left_end: start, dead: false }
    }
    fn end(&self) -> u64 {
        self.start + self.buf.len() as u64
    }
    fn push(&mut self, bytes: &[u8]) {
        if bytes.len() >= DELTA_MAX {
            self.start += (self.buf.len() + bytes.len() - DELTA_MAX) as u64;
            self.buf.clear();
            self.buf.extend(&bytes[bytes.len() - DELTA_MAX..]);
            return;
        }
        let over = (self.buf.len() + bytes.len()).saturating_sub(DELTA_MAX);
        self.buf.drain(..over);
        self.start += over as u64;
        self.buf.extend(bytes);
    }
    /// The host stream started over at `offset` (its own replay): nothing before it is known.
    fn reset(&mut self, offset: u64) {
        self.buf.clear();
        self.start = offset;
    }
    /// The bytes `[since, upto)`, if this tail holds all of them and they are few enough.
    /// Whether the terminal still draws them the same way (size, run alive) is the caller's.
    fn delta(&self, since: u64, upto: u64) -> Option<Vec<u8>> {
        if self.dead || since < self.start || upto > self.end() || since > upto
            || upto - since > DELTA_MAX as u64
        {
            return None;
        }
        let from = (since - self.start) as usize;
        Some(self.buf.range(from..from + (upto - since) as usize).copied().collect())
    }
    /// No client is attached and none could come back to it: the last one left more than
    /// `KEEP` ago, or the tail has rolled past every offset a departed client reached.
    fn unwanted(&self, now: Instant) -> bool {
        self.users == 0
            && (self.left_at.is_none_or(|t| now.duration_since(t) > KEEP) || self.start > self.left_end)
    }
}

/// A client attached through a tail; leaving (drop) records how far it got.
pub struct TailUse {
    tail: Arc<Tail>,
    /// The offset after the last byte sent to the client.
    pub reached: u64,
}

impl TailUse {
    pub fn generation(&self) -> u64 {
        self.tail.generation
    }
    /// The bytes `[since, upto)` of this run, waiting briefly for the tail to reach `upto`.
    pub async fn delta(&self, since: u64, upto: u64) -> Option<Vec<u8>> {
        let mut end = self.tail.end.subscribe();
        let _ = tokio::time::timeout(CATCH_UP, end.wait_for(|e| *e >= upto)).await;
        self.tail.state.lock().delta(since, upto)
    }
}

impl Drop for TailUse {
    fn drop(&mut self) {
        let mut s = self.tail.state.lock();
        s.users = s.users.saturating_sub(1);
        s.left_end = s.left_end.max(self.reached);
        s.left_at = Some(Instant::now());
    }
}

impl Tails {
    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    /// Attach a client to session `id`'s tail for run `generation`, starting one if needed;
    /// `subscribe` opens the tail's own output stream (its replay is not used) and returns the
    /// offset that stream starts at. Called before the client's own replay is taken, so the
    /// tail starts no later than the client's offset. None: no tail (all in use, or the
    /// stream could not be opened); the client then only ever gets screens.
    pub fn join(
        self: &Arc<Self>,
        id: SessionId,
        generation: u64,
        at: u64,
        subscribe: impl FnOnce() -> anyhow::Result<(u64, broadcast::Receiver<Output>)>,
    ) -> Option<TailUse> {
        let existing = self.slots.lock().map.get(&id).cloned();
        let tail = match existing {
            Some(t) if t.generation == generation && !t.state.lock().dead => t,
            _ => {
                // Room is taken before the (blocking) subscription, so concurrent attaches
                // can't start more than MAX_TAILS between them.
                if !self.reserve(id) {
                    return None;
                }
                let subscribed = subscribe();
                let mut slots = self.slots.lock();
                slots.reserved -= 1;
                let (start, rx) = match subscribed {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::debug!(session = id, "replay tail: {e:#}");
                        return None;
                    }
                };
                let tail = Arc::new(Tail {
                    generation,
                    state: Mutex::new(State::new(start)),
                    end: watch::channel(start).0,
                    stop: Notify::new(),
                });
                if let Some(old) = slots.map.insert(id, tail.clone()) {
                    old.stop.notify_one();
                }
                drop(slots);
                tokio::spawn(keep(Arc::downgrade(self), id, tail.clone(), rx));
                tail
            }
        };
        tail.state.lock().users += 1;
        Some(TailUse { tail, reached: at })
    }

    /// Take room for a tail for `id`: evict `id`'s own (another run) or, at the cap, the unused
    /// tail left longest ago. False when every tail (and every one being started) is in use.
    fn reserve(&self, id: SessionId) -> bool {
        let mut slots = self.slots.lock();
        if let Some(old) = slots.map.remove(&id) {
            old.stop.notify_one();
        }
        if slots.map.len() + slots.reserved >= MAX_TAILS {
            let victim = slots
                .map
                .iter()
                .filter_map(|(k, t)| {
                    let s = t.state.lock();
                    (s.users == 0).then_some((*k, s.left_at))
                })
                .min_by_key(|(_, left)| *left)
                .map(|(k, _)| k);
            match victim.and_then(|k| slots.map.remove(&k)) {
                Some(t) => t.stop.notify_one(),
                None => return false,
            }
        }
        slots.reserved += 1;
        true
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.slots.lock().map.len()
    }
}

/// Feeds `tail` from its own output stream until it is unwanted, replaced or its stream ends.
async fn keep(tails: Weak<Tails>, id: SessionId, tail: Arc<Tail>, mut rx: broadcast::Receiver<Output>) {
    let mut sweep = tokio::time::interval(SWEEP);
    sweep.tick().await;
    loop {
        tokio::select! {
            r = rx.recv() => match r {
                Ok(Output::Bytes(bytes)) => {
                    let end = {
                        let mut s = tail.state.lock();
                        s.push(&bytes);
                        s.end()
                    };
                    tail.end.send_replace(end);
                }
                Ok(Output::Replay(_, offset)) => {
                    tail.state.lock().reset(offset);
                    tail.end.send_replace(offset);
                }
                // Missed bytes (lagged) or the run's output ended: nothing more to serve.
                Err(_) => break,
            },
            _ = sweep.tick() => if tail.state.lock().unwanted(Instant::now()) { break },
            _ = tail.stop.notified() => break,
        }
    }
    tail.state.lock().dead = true;
    if let Some(tails) = tails.upgrade() {
        let mut slots = tails.slots.lock();
        if slots.map.get(&id).is_some_and(|t| Arc::ptr_eq(t, &tail)) {
            slots.map.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;

    #[test]
    fn rolls_and_serves_only_what_it_still_holds() {
        let mut s = State::new(100);
        s.push(b"hello ");
        s.push(b"world");
        assert_eq!(s.end(), 111);
        assert_eq!(s.delta(106, 111).as_deref(), Some(&b"world"[..]));
        assert_eq!(s.delta(100, 106).as_deref(), Some(&b"hello "[..]));
        assert_eq!(s.delta(111, 111).as_deref(), Some(&b""[..]), "nothing new is an empty delta");
        assert_eq!(s.delta(99, 111), None, "before the tail began");
        assert_eq!(s.delta(106, 112), None, "past what the tail has");
        // Roll: a big chunk keeps only the last DELTA_MAX bytes.
        s.push(&vec![b'x'; DELTA_MAX + 10]);
        assert_eq!(s.buf.len(), DELTA_MAX);
        assert_eq!(s.end(), 111 + DELTA_MAX as u64 + 10);
        assert_eq!(s.start, s.end() - DELTA_MAX as u64);
        assert_eq!(s.delta(106, s.end()), None, "rolled out");
        assert_eq!(s.delta(s.start, s.end()).map(|d| d.len()), Some(DELTA_MAX));
        // Small pushes roll byte-exact.
        let end = s.end();
        s.push(b"abc");
        assert_eq!((s.start, s.end()), (end - DELTA_MAX as u64 + 3, end + 3));
        assert_eq!(s.delta(end, end + 3).as_deref(), Some(&b"abc"[..]));
    }

    #[test]
    fn a_stream_reset_drops_what_came_before() {
        let mut s = State::new(0);
        s.push(b"0123456789ab");
        assert_eq!(s.delta(10, 12).as_deref(), Some(&b"ab"[..]));
        s.reset(500);
        assert_eq!(s.delta(10, 12), None);
        s.push(b"z");
        assert_eq!(s.delta(500, 501).as_deref(), Some(&b"z"[..]));
        s.dead = true;
        assert_eq!(s.delta(500, 501), None);
    }

    #[test]
    fn unwanted_once_nobody_can_come_back_to_it() {
        let now = Instant::now();
        let mut s = State::new(0);
        s.users = 1;
        assert!(!s.unwanted(now));
        s.users = 0;
        s.left_at = Some(now);
        s.left_end = 10;
        s.push(b"0123456789");
        assert!(!s.unwanted(now), "just left, still holds its offset");
        assert!(s.unwanted(now + KEEP + Duration::from_secs(1)), "kept long enough");
        s.push(&vec![0; DELTA_MAX]);
        assert!(!s.unwanted(now), "offset 10 is still the start of a full-size delta");
        s.push(b"!");
        assert!(s.unwanted(now), "rolled past every offset a client reached");
    }

    fn feed() -> (broadcast::Sender<Output>, impl FnOnce() -> anyhow::Result<(u64, broadcast::Receiver<Output>)>) {
        let (tx, _) = broadcast::channel(64);
        let rx = tx.subscribe();
        (tx, move || Ok((1000, rx)))
    }

    #[tokio::test]
    async fn a_returning_client_gets_the_bytes_it_missed() {
        let tails = Arc::new(Tails::default());
        let (tx, subscribe) = feed();
        let mut first = tails.join(7, 3, 1000, subscribe).expect("tail");
        tx.send(Output::Bytes(Bytes::from_static(b"seen "))).unwrap();
        first.reached = 1005;
        drop(first);
        tx.send(Output::Bytes(Bytes::from_static(b"missed"))).unwrap();
        // The second attach reuses the tail (no new stream) and waits for it to catch up.
        let back = tails.join(7, 3, 1011, || panic!("tail already running")).expect("tail");
        assert_eq!(back.generation(), 3);
        assert_eq!(back.delta(1005, 1011).await.as_deref(), Some(&b"missed"[..]));
        assert_eq!(back.delta(1000, 1011).await.as_deref(), Some(&b"seen missed"[..]));
        assert_eq!(back.delta(999, 1011).await, None, "before the tail's stream began");
        // Not caught up within CATCH_UP: no delta (the caller sends a screen).
        assert_eq!(back.delta(1005, 1020).await, None);
        // Another run of the session replaces the tail.
        let (_tx2, subscribe2) = feed();
        let next = tails.join(7, 4, 1000, subscribe2).expect("tail");
        assert_eq!(next.generation(), 4);
        assert_eq!(tails.len(), 1);
        drop((back, next));
        drop(tx);
    }

    #[tokio::test]
    async fn tails_being_started_count_against_the_cap() {
        let tails = Arc::new(Tails::default());
        for id in 0..MAX_TAILS as u32 {
            assert!(tails.reserve(id));
        }
        let (_tx, subscribe) = feed();
        assert!(tails.join(99, 0, 1000, subscribe).is_none(), "all room taken by starts in progress");
        tails.slots.lock().reserved -= 1;
        let failing = || -> anyhow::Result<(u64, broadcast::Receiver<Output>)> { anyhow::bail!("host busy") };
        assert!(tails.join(99, 0, 1000, failing).is_none());
        assert_eq!(tails.slots.lock().reserved, MAX_TAILS - 1, "a failed subscription gives its room back");
        let (_tx2, subscribe) = feed();
        assert!(tails.join(99, 0, 1000, subscribe).is_some());
    }

    #[tokio::test]
    async fn the_tail_ends_with_its_stream_and_at_the_cap_evicts_the_oldest_unused() {
        let tails = Arc::new(Tails::default());
        let (tx, subscribe) = feed();
        let use_ = tails.join(1, 0, 1000, subscribe).unwrap();
        drop(tx);
        for _ in 0..100 {
            if tails.len() == 0 { break }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(tails.len(), 0, "an ended stream removes its tail");
        assert_eq!(use_.delta(1000, 1000).await, None, "and it serves nothing more");
        drop(use_);

        let mut senders = Vec::new();
        let mut uses = Vec::new();
        for id in 0..MAX_TAILS as u32 {
            let (tx, subscribe) = feed();
            senders.push(tx);
            uses.push(tails.join(100 + id, 0, 1000, subscribe).unwrap());
        }
        let (tx, subscribe) = feed();
        assert!(tails.join(999, 0, 1000, subscribe).is_none(), "every tail has a client");
        drop(tx);
        uses.remove(0);
        let (tx, subscribe) = feed();
        assert!(tails.join(999, 0, 1000, subscribe).is_some(), "the unused tail made room");
        assert_eq!(tails.len(), MAX_TAILS);
        assert!(!tails.slots.lock().map.contains_key(&100));
        drop((tx, senders, uses));
    }
}
