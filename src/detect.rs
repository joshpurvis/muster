//! Screen-based state detection and stuck detection. OWNER: agents implementer.

use std::sync::Arc;

use crate::hub::Hub;

/// Called once at daemon start.
pub fn spawn(_hub: Arc<Hub>) {}
