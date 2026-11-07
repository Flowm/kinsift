//! File system facts: path, folder, name, extension, size, mtime.

use kinsift_core::domain::{Domain, FsMeta};
use kinsift_core::provider::{Extraction, FileCtx, Provider, Stage};

pub struct FsProvider;

impl Provider for FsProvider {
    fn name(&self) -> &'static str {
        "fs"
    }

    fn version(&self) -> u32 {
        1
    }

    fn stage(&self) -> Stage {
        Stage::Walk
    }

    fn default_priority(&self) -> i32 {
        100
    }

    fn applies(&self, _ctx: &FileCtx) -> bool {
        true
    }

    fn extract(&self, ctx: &FileCtx) -> Result<Option<Extraction>, String> {
        let segments: Vec<String> = ctx.rel.split('/').map(str::to_owned).collect();
        let name = segments.last().cloned().unwrap_or_default();
        let dir = segments[..segments.len().saturating_sub(1)].join("/");
        let ext = ctx.ext();
        let meta = FsMeta {
            root: Some(ctx.root.to_string_lossy().into_owned()),
            path: Some(segments),
            dir: Some(dir),
            name: Some(name),
            ext: (!ext.is_empty()).then_some(ext),
            size: Some(ctx.size as i64),
            mtime: Some(ctx.mtime_ns.div_euclid(1_000_000_000)),
        };
        Ok(Some(Extraction { sets: vec![meta.into_fields()], raw: None }))
    }
}
