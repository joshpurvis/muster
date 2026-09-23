//! OS notifications when a session needs the human. OWNER: agents implementer.

use std::sync::Arc;

use crate::hub::Hub;

/// Called once at daemon start.
pub fn spawn(_hub: Arc<Hub>) {}
