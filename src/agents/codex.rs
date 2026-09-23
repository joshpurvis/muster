use crate::agents::*;
use crate::model::*;

pub struct CodexAdapter;

impl Adapter for CodexAdapter {
    fn kind(&self) -> SessionKind {
        SessionKind::Codex
    }
    fn command(&self, _ctx: &SpawnCtx) -> anyhow::Result<CommandSpec> {
        anyhow::bail!("not implemented")
    }
    fn on_hook(&self, _event: &str, _payload: &serde_json::Value) -> Option<StateUpdate> {
        None
    }
    fn detect_from_screen(&self, _screen: &ScreenSnapshot, _current: AgentState) -> Option<StateUpdate> {
        None
    }
    fn approve_bytes(&self, _decision: Decision, _screen: &ScreenSnapshot) -> Option<Vec<u8>> {
        None
    }
    fn interrupt_bytes(&self) -> Vec<u8> {
        vec![0x03]
    }
}
