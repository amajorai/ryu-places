//! Shared Ryu sidecar data-directory seam.

use std::path::{Path, PathBuf};

/// This app's own data dir: `<data dir>/plugin-data/@ryu+places`
/// (`RYU_PLUGIN_DATA_DIR` when Core spawned the process). Every file the app
/// persists lives under it.
pub fn data_dir() -> PathBuf {
    ryu_sidecar_runtime::plugin_data_dir("@ryu/places")
}

/// The node-local map: `<root>/places.db`.
pub fn local_db_path(root: &Path) -> PathBuf {
    root.join("places.db")
}

/// Every other tenant's map: `<root>/tenants/<stem>.db`.
pub fn tenant_db_path(root: &Path, stem: &str) -> PathBuf {
    root.join("tenants").join(format!("{stem}.db"))
}

/// The node-level agent lane record (which one map the MCP server serves):
/// `<root>/agent.db`.
pub fn agent_db_path(root: &Path) -> PathBuf {
    root.join("agent.db")
}
