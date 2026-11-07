//! Video container readers (MKV/WebM and MP4/MOV), pure Rust, headers only.

pub mod mkv;
pub mod mp4;

use std::fs::File;
use std::io::BufReader;

use kinsift_core::domain::{AudioMeta, Domain, VideoMeta, resolution_class};
use kinsift_core::provider::{Extraction, FileCtx, Provider, Stage};
use serde_json::json;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrackKind {
    #[default]
    Video,
    Audio,
    Subtitle,
}

#[derive(Debug, Clone, Default)]
pub struct Track {
    pub kind: TrackKind,
    pub codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub hdr: Option<String>,
    pub channels: Option<u32>,
    pub language: Option<String>,
}

/// What a container reader found.
#[derive(Debug, Clone, Default)]
pub struct Probe {
    pub duration: Option<f64>,
    pub tracks: Vec<Track>,
}

impl Probe {
    pub fn video(&self) -> Option<&Track> {
        self.tracks.iter().find(|t| t.kind == TrackKind::Video)
    }

    pub fn audio(&self) -> impl Iterator<Item = &Track> {
        self.tracks.iter().filter(|t| t.kind == TrackKind::Audio)
    }

    /// Convert to domain fields. `size` is used for the overall bit rate.
    pub fn into_extraction(self, size: u64) -> Extraction {
        let mut sets = Vec::new();
        if let Some(v) = self.video() {
            let (w, h) = (v.width.map(i64::from), v.height.map(i64::from));
            let video = VideoMeta {
                width: w,
                height: h,
                resolution: match (w, h) {
                    (Some(w), Some(h)) if w > 0 && h > 0 => Some(resolution_class(w, h)),
                    _ => None,
                },
                codec: v.codec.clone(),
                fps: v.fps.map(|f| (f * 1000.0).round() / 1000.0),
                bitrate: self.duration.filter(|d| *d > 0.0).map(|d| (size as f64 * 8.0 / d) as i64),
                duration: self.duration,
                hdr: v.hdr.clone(),
            };
            sets.push(video.into_fields());
        }
        let audio: Vec<&Track> = self.audio().collect();
        if let Some(first) = audio.first() {
            let mut languages: Vec<String> = Vec::new();
            for l in audio.iter().filter_map(|a| a.language.clone()) {
                if !languages.contains(&l) {
                    languages.push(l);
                }
            }
            let meta = AudioMeta {
                codec: first.codec.clone(),
                channels: first.channels.map(i64::from),
                languages: (!languages.is_empty()).then_some(languages),
                tracks: Some(audio.len() as i64),
            };
            sets.push(meta.into_fields());
        }
        let raw = json!({
            "duration": self.duration,
            "tracks": self.tracks.iter().map(|t| json!({
                "kind": format!("{:?}", t.kind).to_lowercase(),
                "codec": t.codec, "width": t.width, "height": t.height, "fps": t.fps,
                "hdr": t.hdr, "channels": t.channels, "language": t.language,
            })).collect::<Vec<_>>(),
        });
        Extraction { sets, raw: Some(raw) }
    }
}

/// HDR flavour from an ITU-T H.273 transfer characteristic.
pub fn hdr_from_transfer(tc: u16) -> Option<&'static str> {
    match tc {
        16 => Some("HDR10"),
        18 => Some("HLG"),
        _ => None,
    }
}

/// Probe a file, trusting its first bytes over its extension.
fn run(ctx: &FileCtx) -> Result<Option<Extraction>, String> {
    use std::io::{Read, Seek, SeekFrom};
    let file = File::open(ctx.abs).map_err(|e| e.to_string())?;
    let mut r = BufReader::with_capacity(64 * 1024, file);
    let mut magic = [0u8; 8];
    let n = r.read(&mut magic).map_err(|e| e.to_string())?;
    r.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let (container, probe) = if n >= 4 && magic[..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        ("matroska", mkv::probe(&mut r, ctx.size)?)
    } else if n >= 8 && matches!(&magic[4..8], b"ftyp" | b"moov" | b"mdat" | b"free" | b"wide") {
        ("mp4", mp4::probe(&mut r, ctx.size)?)
    } else {
        return Err("unknown container (neither Matroska nor MP4)".into());
    };
    let mut extraction = probe.into_extraction(ctx.size);
    if let Some(raw) = extraction.raw.as_mut().and_then(|r| r.as_object_mut()) {
        raw.insert("container".into(), container.into());
        let expected = match ctx.ext().as_str() {
            "mkv" | "webm" | "mk3d" => "matroska",
            _ => "mp4",
        };
        if expected != container {
            raw.insert("extension_mismatch".into(), true.into());
        }
    }
    Ok(Some(extraction))
}

pub struct MkvProvider;

impl Provider for MkvProvider {
    fn name(&self) -> &'static str {
        "mkv"
    }
    fn version(&self) -> u32 {
        2
    }
    fn stage(&self) -> Stage {
        Stage::Headers
    }
    fn default_priority(&self) -> i32 {
        80
    }
    fn applies(&self, ctx: &FileCtx) -> bool {
        matches!(ctx.ext().as_str(), "mkv" | "webm" | "mk3d")
    }
    fn extract(&self, ctx: &FileCtx) -> Result<Option<Extraction>, String> {
        run(ctx)
    }
}

pub struct Mp4Provider;

impl Provider for Mp4Provider {
    fn name(&self) -> &'static str {
        "mp4"
    }
    fn version(&self) -> u32 {
        2
    }
    fn stage(&self) -> Stage {
        Stage::Headers
    }
    fn default_priority(&self) -> i32 {
        80
    }
    fn applies(&self, ctx: &FileCtx) -> bool {
        matches!(ctx.ext().as_str(), "mp4" | "m4v" | "mov")
    }
    fn extract(&self, ctx: &FileCtx) -> Result<Option<Extraction>, String> {
        run(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kinsift_core::value::Value;

    #[test]
    fn sniffs_container_regardless_of_extension() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("really_an_mp4.mkv");
        let data = mp4::tests::sample(false);
        std::fs::write(&path, &data).unwrap();
        let ctx =
            FileCtx { root: dir.path(), abs: &path, rel: "really_an_mp4.mkv", size: data.len() as u64, mtime_ns: 0 };
        let e = MkvProvider.extract(&ctx).unwrap().unwrap();
        let raw = e.raw.unwrap();
        assert_eq!(raw["container"], "mp4");
        assert_eq!(raw["extension_mismatch"], true);
    }

    #[test]
    fn extraction_fields() {
        let data = mkv::tests::sample(false);
        let p = mkv::probe(&mut std::io::Cursor::new(&data), data.len() as u64).unwrap();
        let e = p.into_extraction(2_700_000_000);
        let fields: Vec<(String, Value)> = e.sets.iter().flat_map(|s| s.keys().map(|(k, v)| (k, v.clone()))).collect();
        let get = |k: &str| fields.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
        assert_eq!(get("video.resolution"), Some(Value::Int(1080)), "1920x800 is a 1080 class encode");
        assert_eq!(get("video.bitrate"), Some(Value::Int(8_000_000)));
        assert_eq!(get("audio.channels"), Some(Value::Int(6)));
        assert_eq!(get("audio.tracks"), Some(Value::Int(2)));
    }
}
