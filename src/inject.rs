//! Intelligent injection: per-session input queue and reliable prompt submission.
//! OWNER: agents implementer. Used by rpc.rs and mcp.rs through the functions below.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::hub::Hub;
use crate::model::{Decision, SessionId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SendMode {
    /// Deliver when the agent is ready (default).
    #[default]
    Queue,
    /// Interrupt the current turn, then deliver.
    Interrupt,
}

#[derive(Debug, Clone, Serialize)]
pub struct SendOutcome {
    /// True if it was written immediately; false if queued.
    pub delivered: bool,
    /// Position in queue after enqueueing (0 when delivered).
    pub queued: u32,
}

/// Send a prompt. `from` is "human" or "session:<id>" (for the timeline).
pub async fn send(_hub: &Arc<Hub>, _id: SessionId, _text: String, _mode: SendMode, _from: String) -> anyhow::Result<SendOutcome> {
    anyhow::bail!("not implemented")
}

pub fn approve(_hub: &Arc<Hub>, _id: SessionId, _decision: Decision) -> anyhow::Result<()> {
    anyhow::bail!("not implemented")
}

pub fn interrupt(_hub: &Arc<Hub>, _id: SessionId) -> anyhow::Result<()> {
    anyhow::bail!("not implemented")
}

pub fn clear_queue(_hub: &Arc<Hub>, _id: SessionId) {}

/// Background tasks (queue draining). Called once at daemon start.
pub fn spawn_tasks(_hub: Arc<Hub>) {}
