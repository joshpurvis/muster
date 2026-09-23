//! MCP server (streamable HTTP, JSON responses) mounted at /mcp. OWNER: mcp implementer.

use std::sync::Arc;

use crate::hub::Hub;

pub fn router(_hub: Arc<Hub>) -> axum::Router {
    axum::Router::new()
}
