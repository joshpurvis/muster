use crate::agents::*;
use crate::model::*;

pub struct ShellAdapter;

impl Adapter for ShellAdapter {
    fn kind(&self) -> SessionKind {
        SessionKind::Shell
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
