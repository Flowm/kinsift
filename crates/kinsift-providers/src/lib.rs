//! Built-in metadata providers.

pub mod fs;
pub mod name;

use kinsift_core::provider::Provider;

/// All built-in providers, in pipeline order.
pub fn default_providers() -> Vec<Box<dyn Provider>> {
    vec![Box::new(fs::FsProvider), Box::new(name::NameProvider)]
}
