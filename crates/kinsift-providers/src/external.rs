//! Optional providers backed by external tools, used only when found on PATH:
//! `ffprobe` for video containers the native readers do not cover, and
//! `exiftool` for image formats the native EXIF reader does not cover.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use kinsift_core::domain::{Domain, ExifMeta, ImageMeta};
use kinsift_core::provider::{Extraction, FileCtx, Provider, Stage};
use serde_json::Value as Json;

use crate::image::{NATIVE_UNSUPPORTED, camera_name};
use crate::video::{Probe, Track, TrackKind};

/// Find an executable on PATH.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidates = [dir.join(name), dir.join(format!("{name}.exe"))];
        candidates.into_iter().find(|p| p.is_file())
    })
}

static FFPROBE: OnceLock<Option<PathBuf>> = OnceLock::new();
static EXIFTOOL: OnceLock<Option<PathBuf>> = OnceLock::new();

const NATIVE_VIDEO: &[&str] = &["mkv", "webm", "mk3d", "mp4", "m4v", "mov"];

pub struct FfprobeProvider;

impl Provider for FfprobeProvider {
    fn name(&self) -> &'static str {
        "ffprobe"
    }
    fn version(&self) -> u32 {
        1
    }
    fn stage(&self) -> Stage {
        Stage::Headers
    }
    fn default_priority(&self) -> i32 {
        60
    }
    fn applies(&self, ctx: &FileCtx) -> bool {
        ctx.is_video()
            && !NATIVE_VIDEO.contains(&ctx.ext().as_str())
            && FFPROBE.get_or_init(|| which("ffprobe")).is_some()
    }
    fn extract(&self, ctx: &FileCtx) -> Result<Option<Extraction>, String> {
        let bin = FFPROBE.get_or_init(|| which("ffprobe")).clone().ok_or("ffprobe not found")?;
        let out = run(&bin, &["-v", "error", "-print_format", "json", "-show_format", "-show_streams"], ctx.abs)?;
        let probe = probe_from_ffprobe(&out)?;
        Ok(Some(probe.into_extraction(ctx.size)))
    }
}

fn run(bin: &Path, args: &[&str], file: &Path) -> Result<String, String> {
    let out = Command::new(bin).args(args).arg(file).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().chars().take(200).collect());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Parse `ffprobe -print_format json -show_format -show_streams` output.
pub fn probe_from_ffprobe(json: &str) -> Result<Probe, String> {
    let v: Json = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let duration = v["format"]["duration"].as_str().and_then(|d| d.parse().ok());
    let mut tracks = Vec::new();
    for s in v["streams"].as_array().into_iter().flatten() {
        let kind = match s["codec_type"].as_str() {
            Some("video") if s["disposition"]["attached_pic"].as_i64() != Some(1) => TrackKind::Video,
            Some("audio") => TrackKind::Audio,
            Some("subtitle") => TrackKind::Subtitle,
            _ => continue,
        };
        let fps = s["avg_frame_rate"].as_str().or(s["r_frame_rate"].as_str()).and_then(|r| {
            let (n, d) = r.split_once('/')?;
            let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
            (d > 0.0 && n > 0.0).then_some(n / d)
        });
        let codec = s["codec_name"].as_str().map(|c| match c {
            "h264" => "h264".to_owned(),
            "hevc" => "hevc".to_owned(),
            "mpeg4" | "msmpeg4v3" => "mpeg4".to_owned(),
            "mpeg2video" => "mpeg2".to_owned(),
            other => other.to_owned(),
        });
        tracks.push(Track {
            kind,
            codec,
            width: s["width"].as_u64().map(|w| w as u32),
            height: s["height"].as_u64().map(|h| h as u32),
            fps: if kind == TrackKind::Video { fps } else { None },
            hdr: match s["color_transfer"].as_str() {
                Some("smpte2084") => Some("HDR10".into()),
                Some("arib-std-b67") => Some("HLG".into()),
                _ => None,
            },
            channels: s["channels"].as_u64().map(|c| c as u32),
            language: s["tags"]["language"].as_str().filter(|l| *l != "und").map(str::to_owned),
        });
    }
    if tracks.is_empty() {
        return Err("ffprobe found no streams".into());
    }
    Ok(Probe { duration, tracks })
}

pub struct ExiftoolProvider;

impl Provider for ExiftoolProvider {
    fn name(&self) -> &'static str {
        "exiftool"
    }
    fn version(&self) -> u32 {
        1
    }
    fn stage(&self) -> Stage {
        Stage::Headers
    }
    fn default_priority(&self) -> i32 {
        60
    }
    fn applies(&self, ctx: &FileCtx) -> bool {
        NATIVE_UNSUPPORTED.contains(&ctx.ext().as_str()) && EXIFTOOL.get_or_init(|| which("exiftool")).is_some()
    }
    fn extract(&self, ctx: &FileCtx) -> Result<Option<Extraction>, String> {
        let bin = EXIFTOOL.get_or_init(|| which("exiftool")).clone().ok_or("exiftool not found")?;
        let out = run(
            &bin,
            &[
                "-json",
                "-n",
                "-DateTimeOriginal",
                "-Make",
                "-Model",
                "-LensModel",
                "-Software",
                "-Orientation",
                "-ImageWidth",
                "-ImageHeight",
            ],
            ctx.abs,
        )?;
        Ok(Some(extraction_from_exiftool(&out, &ctx.ext())?))
    }
}

/// Parse `exiftool -json -n` output for one file.
pub fn extraction_from_exiftool(json: &str, ext: &str) -> Result<Extraction, String> {
    let v: Json = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let o = v.get(0).ok_or("empty exiftool output")?;
    let s = |k: &str| o[k].as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    let taken_at = s("DateTimeOriginal").and_then(|d| {
        let d = d.as_bytes();
        let n = |a: usize, b: usize| std::str::from_utf8(d.get(a..b)?).ok()?.parse::<u32>().ok();
        Some(kinsift_core::value::unix_from_civil(
            i64::from(n(0, 4)?),
            n(5, 7)?,
            n(8, 10)?,
            n(11, 13)?,
            n(14, 16)?,
            n(17, 19)?,
        ))
    });
    let (make, model) = (s("Make"), s("Model"));
    let mut sets = vec![
        ExifMeta {
            taken_at,
            camera: camera_name(make.as_deref(), model.as_deref()),
            make,
            model,
            lens: s("LensModel"),
            software: s("Software"),
            orientation: o["Orientation"].as_i64(),
        }
        .into_fields(),
    ];
    if let (Some(w), Some(h)) = (o["ImageWidth"].as_i64(), o["ImageHeight"].as_i64()) {
        sets.push(
            ImageMeta { width: Some(w), height: Some(h), pixels: Some(w * h), format: Some(ext.to_owned()) }
                .into_fields(),
        );
    }
    Ok(Extraction { sets, raw: Some(o.clone()) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ffprobe_json() {
        let json = r#"{"streams":[
            {"codec_type":"video","codec_name":"mpeg4","width":720,"height":576,"avg_frame_rate":"25/1","disposition":{"attached_pic":0}},
            {"codec_type":"audio","codec_name":"mp3","channels":2,"tags":{"language":"ger"}},
            {"codec_type":"video","codec_name":"mjpeg","width":100,"height":100,"disposition":{"attached_pic":1}}],
            "format":{"duration":"1320.5"}}"#;
        let p = probe_from_ffprobe(json).unwrap();
        let v = p.video().unwrap();
        assert_eq!((v.width, v.height, v.fps), (Some(720), Some(576), Some(25.0)));
        assert_eq!(p.tracks.len(), 2, "cover art is ignored");
        assert_eq!(p.duration, Some(1320.5));
    }

    #[test]
    fn parses_exiftool_json() {
        let json = r#"[{"SourceFile":"x.cr3","DateTimeOriginal":"2024:05:06 07:08:09","Make":"Canon","Model":"Canon EOS R6","ImageWidth":6000,"ImageHeight":4000,"Orientation":1}]"#;
        let e = extraction_from_exiftool(json, "cr3").unwrap();
        let fields: Vec<(String, String)> =
            e.sets.iter().flat_map(|s| s.keys().map(|(k, v)| (k, v.to_string()))).collect();
        assert!(fields.contains(&("exif.camera".into(), "Canon EOS R6".into())));
        assert!(fields.contains(&("exif.taken_at".into(), "2024-05-06 07:08".into())));
        assert!(fields.contains(&("image.pixels".into(), "24000000".into())));
    }
}
