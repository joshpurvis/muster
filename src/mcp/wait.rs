//! `wait_for_state` decision logic (pure) and the waiting loop.
//!
//! The problem: an agent that was just sent a message is often still `idle` for a moment
//! (the message is queued, or delivered but the agent hasn't reacted yet). Returning that
//! stale `idle` would make "send, then wait" useless. Rules, in order:
//!
//! 1. The session's current state must be one of the requested states.
//! 2. `idle` is the state that means "done", so only `idle` can be stale. Other states
//!    (needs_approval, asking_question, error, stuck, exited) count at once, because a queued
//!    message can't be delivered while the agent is blocked on them.
//! 3. Explicit `after_ms` (unix ms, e.g. send_message's `sent_at_ms`): a non-idle state must
//!    have been entered at/after it. `after_ms = 0` accepts any matching state and turns off
//!    every staleness rule.
//! 4. For `idle` (unless `after_ms = 0`) it is stale if:
//!    a. the session still has queued input (the queue will drain soon); or
//!    b. the last "activity" happened after the session went idle, less than
//!       `settle_grace_ms` (15 s) ago. Activity = `after_ms` (explicit mode) or our last MCP
//!       send_message to it (implicit mode), and in both modes a delivery out of its queue
//!       (the queue length shrank). After the grace period idle is accepted anyway (the agent
//!       may have finished too fast for a state change to be seen).
//! 5. A session that exits while waiting for other states ends the wait (`ended: "exited"`).

use std::time::Duration;

use tokio::sync::broadcast;

use crate::hub::ServerEvent;
use crate::model::{AgentState, SessionId, SessionInfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Reached,
    Wait,
    /// Exited and `exited` wasn't requested.
    Ended,
}

#[derive(Debug, Clone, Copy)]
pub struct WaitCtx {
    pub after_ms: Option<i64>,
    pub last_send_ms: Option<i64>,
    pub last_delivery_ms: Option<i64>,
    pub settle_grace_ms: i64,
    pub now_ms: i64,
}

pub fn evaluate(info: &SessionInfo, states: &[AgentState], ctx: &WaitCtx) -> Verdict {
    if !states.contains(&info.state) {
        return if info.state == AgentState::Exited {
            Verdict::Ended
        } else {
            Verdict::Wait
        };
    }
    if ctx.after_ms == Some(0) {
        return Verdict::Reached;
    }
    if info.state != AgentState::Idle {
        return match ctx.after_ms {
            Some(after) if info.state_since < after => Verdict::Wait,
            _ => Verdict::Reached,
        };
    }
    if info.queued > 0 {
        return Verdict::Wait;
    }
    // Explicit mode: after_ms itself is the activity; implicit: our last send. Deliveries out
    // of the queue count in both.
    let activity = match ctx.after_ms {
        Some(after) => Some(after).max(ctx.last_delivery_ms),
        None => ctx.last_send_ms.max(ctx.last_delivery_ms),
    };
    if let Some(t) = activity
        && info.state_since < t
        && ctx.now_ms - t < ctx.settle_grace_ms
    {
        return Verdict::Wait;
    }
    Verdict::Reached
}

pub const DEFAULT_STATES: [AgentState; 6] = [
    AgentState::Idle,
    AgentState::NeedsApproval,
    AgentState::AskingQuestion,
    AgentState::Error,
    AgentState::Stuck,
    AgentState::Exited,
];

/// Block until a SessionUpserted for `id` arrives, or `max` elapses. Returns early on any
/// relevant event; callers re-read the session and re-evaluate.
pub async fn next_change(rx: &mut broadcast::Receiver<ServerEvent>, id: SessionId, max: Duration) {
    let _ = tokio::time::timeout(max, async {
        loop {
            match rx.recv().await {
                Ok(ServerEvent::SessionUpserted(s)) if s.id == id => return,
                Ok(ServerEvent::SessionRemoved { id: r }) if r == id => return,
                Ok(_) => continue,
                // Missed events: re-check state now.
                Err(broadcast::error::RecvError::Lagged(_)) => return,
                // Bus gone: fall back to polling at `max` intervals.
                Err(broadcast::error::RecvError::Closed) => {
                    tokio::time::sleep(max).await;
                    return;
                }
            }
        }
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::guard::test_session;

    fn ctx(after: Option<i64>, sent: Option<i64>, now: i64) -> WaitCtx {
        WaitCtx {
            after_ms: after,
            last_send_ms: sent,
            last_delivery_ms: None,
            settle_grace_ms: 15_000,
            now_ms: now,
        }
    }

    fn sess(state: AgentState, since: i64, queued: u32) -> SessionInfo {
        let mut s = test_session(1, None);
        s.state = state;
        s.state_since = since;
        s.queued = queued;
        s
    }

    #[test]
    fn basic_match() {
        let st = &DEFAULT_STATES;
        assert_eq!(
            evaluate(&sess(AgentState::Idle, 0, 0), st, &ctx(None, None, 10)),
            Verdict::Reached
        );
        assert_eq!(
            evaluate(&sess(AgentState::Working, 0, 0), st, &ctx(None, None, 10)),
            Verdict::Wait
        );
        assert_eq!(
            evaluate(
                &sess(AgentState::Exited, 0, 0),
                &[AgentState::Idle],
                &ctx(None, None, 10)
            ),
            Verdict::Ended
        );
    }

    #[test]
    fn stale_idle_after_send() {
        let st = &DEFAULT_STATES;
        // Idle since 1000, message sent at 2000, now 3000: stale.
        assert_eq!(
            evaluate(
                &sess(AgentState::Idle, 1000, 0),
                st,
                &ctx(None, Some(2000), 3000)
            ),
            Verdict::Wait
        );
        // Went idle again after the send: fresh.
        assert_eq!(
            evaluate(
                &sess(AgentState::Idle, 2500, 0),
                st,
                &ctx(None, Some(2000), 3000)
            ),
            Verdict::Reached
        );
        // Grace expired without a visible transition: accept.
        assert_eq!(
            evaluate(
                &sess(AgentState::Idle, 1000, 0),
                st,
                &ctx(None, Some(2000), 17_001)
            ),
            Verdict::Reached
        );
        // Queued input keeps idle stale regardless.
        assert_eq!(
            evaluate(
                &sess(AgentState::Idle, 5000, 1),
                st,
                &ctx(None, None, 99_999)
            ),
            Verdict::Wait
        );
        // Blocked states return immediately even with a queue / recent send.
        assert_eq!(
            evaluate(
                &sess(AgentState::NeedsApproval, 1000, 2),
                st,
                &ctx(None, Some(2000), 3000)
            ),
            Verdict::Reached
        );
    }

    #[test]
    fn explicit_after() {
        let st = &DEFAULT_STATES;
        assert_eq!(
            evaluate(
                &sess(AgentState::Idle, 1000, 0),
                st,
                &ctx(Some(1500), None, 2000)
            ),
            Verdict::Wait
        );
        assert_eq!(
            evaluate(
                &sess(AgentState::Idle, 1500, 0),
                st,
                &ctx(Some(1500), None, 2000)
            ),
            Verdict::Reached
        );
        // Explicit after_ms gets the same grace: idle that never changed is accepted after 15 s.
        assert_eq!(
            evaluate(
                &sess(AgentState::Idle, 1000, 0),
                st,
                &ctx(Some(1500), None, 16_600)
            ),
            Verdict::Reached
        );
        // Explicit after_ms applies to non-idle states too.
        assert_eq!(
            evaluate(
                &sess(AgentState::Error, 1000, 0),
                st,
                &ctx(Some(1500), None, 2000)
            ),
            Verdict::Wait
        );
        // Explicit after_ms still refuses idle with queued input, and idle that predates a
        // delivery out of the queue (within the grace period).
        assert_eq!(
            evaluate(
                &sess(AgentState::Idle, 1500, 1),
                st,
                &ctx(Some(1500), None, 2000)
            ),
            Verdict::Wait
        );
        let mut c = ctx(Some(1000), None, 3000);
        c.last_delivery_ms = Some(2000);
        assert_eq!(
            evaluate(&sess(AgentState::Idle, 1500, 0), st, &c),
            Verdict::Wait
        );
        assert_eq!(
            evaluate(&sess(AgentState::Idle, 2500, 0), st, &c),
            Verdict::Reached
        );
        // Implicit: a delivery after the send counts as newer activity.
        let mut c = ctx(None, Some(1000), 3000);
        c.last_delivery_ms = Some(2000);
        assert_eq!(
            evaluate(&sess(AgentState::Idle, 1500, 0), st, &c),
            Verdict::Wait
        );
        // after_ms = 0 disables staleness, even with queued input.
        assert_eq!(
            evaluate(
                &sess(AgentState::Idle, 1, 3),
                st,
                &ctx(Some(0), Some(2000), 2001)
            ),
            Verdict::Reached
        );
    }
}
