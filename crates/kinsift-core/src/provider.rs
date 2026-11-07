//! The provider interface: anything that turns a file into domain fields.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use crate::domain::FieldSet;

/// When a provider runs. Scans run `Walk`, `Name` and `Headers`;
/// `Hash` providers only run on demand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    Walk,
    Name,
    Headers,
    Hash,
}

/// Everything a provider gets to know about a file.
#[derive(Debug, Clone, Copy)]
pub struct FileCtx<'a> {
    pub root: &'a Path,
    pub abs: &'a Path,
    /// Path relative to the root, `/`-separated.
    pub rel: &'a str,
    pub size: u64,
    pub mtime_ns: i64,
}

impl FileCtx<'_> {
    /// Lower-case extension without the dot.
    pub fn ext(&self) -> String {
        self.abs.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default()
    }

    pub fn is_video(&self) -> bool {
        VIDEO_EXTENSIONS.contains(&self.ext().as_str())
    }

    pub fn is_image(&self) -> bool {
        IMAGE_EXTENSIONS.contains(&self.ext().as_str())
    }
}

/// What one provider extracted from one file.
#[derive(Debug, Clone, Default)]
pub struct Extraction {
    pub sets: Vec<FieldSet>,
    /// The provider's own view of the file, kept for `inspect`.
    pub raw: Option<serde_json::Value>,
}

pub trait Provider: Send + Sync {
    /// Stable identifier, used in the index and in `providers.priority`.
    fn name(&self) -> &'static str;
    /// Bump to re-run this provider on files extracted by an older version.
    fn version(&self) -> u32;
    fn stage(&self) -> Stage;
    /// Priority when several providers fill the same field and the config
    /// does not say otherwise. Higher wins.
    fn default_priority(&self) -> i32 {
        50
    }
    fn applies(&self, ctx: &FileCtx) -> bool;
    /// `Ok(None)` means "nothing to say about this file".
    fn extract(&self, ctx: &FileCtx) -> std::result::Result<Option<Extraction>, String>;
}

pub const VIDEO_EXTENSIONS: &[&str] =
    &["mkv", "mp4", "m4v", "avi", "mov", "wmv", "flv", "webm", "mpg", "mpeg", "m2ts", "ts", "vob", "ogv"];

pub const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "heic", "heif", "avif", "webp", "tif", "tiff", "dng", "cr2", "cr3", "nef", "arw", "orf",
    "rw2", "raf",
];

/// Decides which provider's value wins for a field.
#[derive(Debug, Clone, Default)]
pub struct Priority {
    /// Configured order per domain, highest priority first.
    configured: BTreeMap<String, Vec<String>>,
    defaults: HashMap<String, i32>,
}

impl Priority {
    pub fn new(configured: BTreeMap<String, Vec<String>>, providers: &[Box<dyn Provider>]) -> Self {
        let defaults = providers.iter().map(|p| (p.name().to_owned(), p.default_priority())).collect();
        Priority { configured, defaults }
    }

    /// Rank of `provider` for field `key`; higher wins.
    pub fn rank(&self, key: &str, provider: &str) -> i64 {
        let domain = key.split('.').next().unwrap_or(key);
        if let Some(order) = self.configured.get(domain)
            && let Some(pos) = order.iter().position(|p| p == provider)
        {
            return 1_000_000 - pos as i64;
        }
        i64::from(self.defaults.get(provider).copied().unwrap_or(0))
    }
}
