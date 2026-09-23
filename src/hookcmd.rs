//! Client side of `muster hook <agent> <event> [payload]`: runs inside agent CLIs' hook
//! callbacks, forwards the payload to the daemon, and must never block or fail the agent.
//! OWNER: agents implementer.

/// Returns the process exit code (always 0 unless the adapter wants to block the hook).
pub fn run(_agent: &str, _event: &str, _payload_arg: Option<String>) -> i32 {
    0
}
