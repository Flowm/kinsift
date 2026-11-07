//! kinsift core: metadata model, index, scanning, views and file actions.
//!
//! This crate has no UI code. Frontends (CLI, TUI, later GUI) call into it.

pub mod actions;
pub mod config;
pub mod domain;
pub mod error;
pub mod filter;
pub mod hash;
pub mod index;
pub mod keep;
pub mod normalize;
pub mod provider;
pub mod quarantine;
pub mod query;
pub mod scan;
pub mod value;

pub use error::{Error, Result};

use index::Index;
use provider::Priority;

/// Re-resolve all fields if the provider priority changed since the last run.
pub fn sync_priority(index: &mut Index, config: &config::Config, priority: &Priority) -> Result<bool> {
    let signature = serde_json::to_string(&config.providers.priority).unwrap_or_default();
    if index.meta("priority")?.as_deref() == Some(signature.as_str()) {
        return Ok(false);
    }
    index.resolve_all(priority)?;
    index.set_meta("priority", &signature)?;
    Ok(true)
}
