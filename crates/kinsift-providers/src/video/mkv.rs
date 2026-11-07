//! Minimal Matroska/WebM header reader: Info and Tracks only.
//!
//! Walks the top level of the Segment, following the SeekHead if Tracks come
//! after the first Cluster. Media data is never read.

use std::io::{Read, Seek, SeekFrom};

use super::{Probe, Track, TrackKind};

const EBML: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const SEEK_HEAD: u32 = 0x114D_9B74;
const SEEK: u32 = 0x4DBB;
const SEEK_ID: u32 = 0x53AB;
const SEEK_POSITION: u32 = 0x53AC;
const INFO: u32 = 0x1549_A966;
const TIMESTAMP_SCALE: u32 = 0x2A_D7B1;
const DURATION: u32 = 0x4489;
const TRACKS: u32 = 0x1654_AE6B;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_TYPE: u32 = 0x83;
const CODEC_ID: u32 = 0x86;
const LANGUAGE: u32 = 0x22_B59C;
const LANGUAGE_BCP47: u32 = 0x22_B59D;
const DEFAULT_DURATION: u32 = 0x23_E383;
const VIDEO: u32 = 0xE0;
const PIXEL_WIDTH: u32 = 0xB0;
const PIXEL_HEIGHT: u32 = 0xBA;
const COLOUR: u32 = 0x55B0;
const TRANSFER: u32 = 0x55BA;
const AUDIO: u32 = 0xE1;
const CHANNELS: u32 = 0x9F;
const BLOCK_ADDITION_MAPPING: u32 = 0x41E4;
const BLOCK_ADD_ID_TYPE: u32 = 0x41E7;
const CLUSTER: u32 = 0x1F43_B675;
const MAX_ELEMENT: u64 = 16 << 20;

/// Read a variable-length integer. `keep_marker` keeps the length bits (IDs).
fn read_vint<R: Read>(r: &mut R, keep_marker: bool) -> Result<(u64, usize), String> {
    let mut first = [0u8; 1];
    r.read_exact(&mut first).map_err(|e| e.to_string())?;
    let len = first[0].leading_zeros() as usize + 1;
    if len > 8 {
        return Err("invalid EBML vint".into());
    }
    let mut value = if keep_marker { u64::from(first[0]) } else { u64::from(first[0]) & ((1u64 << (8 - len)) - 1) };
    let mut rest = [0u8; 7];
    r.read_exact(&mut rest[..len - 1]).map_err(|e| e.to_string())?;
    let mut all_ones = value == (1u64 << (8 - len)) - 1;
    for b in &rest[..len - 1] {
        value = (value << 8) | u64::from(*b);
        all_ones &= *b == 0xff;
    }
    if !keep_marker && all_ones {
        return Ok((u64::MAX, len)); // unknown size
    }
    Ok((value, len))
}

fn vint_slice(d: &[u8], keep_marker: bool) -> Option<(u64, usize)> {
    read_vint(&mut &d[..], keep_marker).ok()
}

/// Iterate child elements of an in-memory master element.
fn elements(mut d: &[u8]) -> impl Iterator<Item = (u32, &[u8])> {
    std::iter::from_fn(move || {
        let (id, il) = vint_slice(d, true)?;
        let (size, sl) = vint_slice(&d[il..], false)?;
        let start = il + sl;
        let end = start.checked_add(usize::try_from(size).ok()?)?;
        if end > d.len() {
            return None;
        }
        let body = &d[start..end];
        d = &d[end..];
        Some((id as u32, body))
    })
}

fn uint(d: &[u8]) -> u64 {
    d.iter().take(8).fold(0, |acc, b| (acc << 8) | u64::from(*b))
}

fn float(d: &[u8]) -> Option<f64> {
    match d.len() {
        4 => Some(f64::from(f32::from_be_bytes(d.try_into().ok()?))),
        8 => Some(f64::from_be_bytes(d.try_into().ok()?)),
        _ => None,
    }
}

fn string(d: &[u8]) -> String {
    String::from_utf8_lossy(d).trim_end_matches('\0').to_owned()
}

pub fn probe<R: Read + Seek>(r: &mut R, len: u64) -> Result<Probe, String> {
    r.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let (id, _) = read_vint(r, true)?;
    if id as u32 != EBML {
        return Err("not an EBML file".into());
    }
    let (size, _) = read_vint(r, false)?;
    r.seek(SeekFrom::Current(size as i64)).map_err(|e| e.to_string())?;
    let (id, _) = read_vint(r, true)?;
    if id as u32 != SEGMENT {
        return Err("no Matroska segment".into());
    }
    let (seg_size, _) = read_vint(r, false)?;
    let seg_start = r.stream_position().map_err(|e| e.to_string())?;
    let seg_end = if seg_size == u64::MAX { len } else { (seg_start + seg_size).min(len) };

    let mut probe = Probe::default();
    let mut scale = 1_000_000u64;
    let mut duration = None;
    let mut seek_tracks = None;
    let mut seek_info = None;
    let mut have_tracks = false;
    let mut have_info = false;
    let mut pos = seg_start;
    while pos < seg_end && !(have_tracks && have_info) {
        r.seek(SeekFrom::Start(pos)).map_err(|e| e.to_string())?;
        let Ok((id, il)) = read_vint(r, true) else { break };
        let Ok((size, sl)) = read_vint(r, false) else { break };
        let body_start = pos + (il + sl) as u64;
        let id = id as u32;
        if id == CLUSTER || size == u64::MAX {
            break;
        }
        if matches!(id, SEEK_HEAD | INFO | TRACKS) {
            if size > MAX_ELEMENT {
                return Err("header element too large".into());
            }
            let mut buf = vec![0u8; size as usize];
            r.read_exact(&mut buf).map_err(|e| e.to_string())?;
            match id {
                SEEK_HEAD => {
                    for (_, seek) in elements(&buf).filter(|(i, _)| *i == SEEK) {
                        let mut target = None;
                        let mut position = None;
                        for (eid, v) in elements(seek) {
                            match eid {
                                SEEK_ID => target = Some(uint(v) as u32),
                                SEEK_POSITION => position = Some(seg_start + uint(v)),
                                _ => {}
                            }
                        }
                        match (target, position) {
                            (Some(TRACKS), Some(p)) => seek_tracks = Some(p),
                            (Some(INFO), Some(p)) => seek_info = Some(p),
                            _ => {}
                        }
                    }
                }
                INFO => {
                    have_info = true;
                    for (eid, v) in elements(&buf) {
                        match eid {
                            TIMESTAMP_SCALE => scale = uint(v).max(1),
                            DURATION => duration = float(v),
                            _ => {}
                        }
                    }
                }
                _ => {
                    have_tracks = true;
                    probe.tracks =
                        elements(&buf).filter(|(i, _)| *i == TRACK_ENTRY).filter_map(|(_, t)| track(t)).collect();
                }
            }
        }
        pos = body_start + size;
    }
    // Tracks/Info after the first cluster: follow the SeekHead.
    for (wanted, target) in [(!have_tracks, seek_tracks), (!have_info, seek_info)] {
        let (true, Some(p)) = (wanted, target) else { continue };
        r.seek(SeekFrom::Start(p)).map_err(|e| e.to_string())?;
        let (id, _) = read_vint(r, true)?;
        let (size, _) = read_vint(r, false)?;
        if size > MAX_ELEMENT {
            continue;
        }
        let mut buf = vec![0u8; size as usize];
        r.read_exact(&mut buf).map_err(|e| e.to_string())?;
        match id as u32 {
            TRACKS => {
                probe.tracks =
                    elements(&buf).filter(|(i, _)| *i == TRACK_ENTRY).filter_map(|(_, t)| track(t)).collect();
            }
            INFO => {
                for (eid, v) in elements(&buf) {
                    match eid {
                        TIMESTAMP_SCALE => scale = uint(v).max(1),
                        DURATION => duration = float(v),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    probe.duration = duration.map(|d| d * scale as f64 / 1e9);
    if probe.tracks.is_empty() {
        return Err("no tracks found".into());
    }
    Ok(probe)
}

fn track(d: &[u8]) -> Option<Track> {
    let mut t = Track::default();
    let mut kind = None;
    let mut codec = None;
    let mut lang = Some("eng".to_owned()); // Matroska default
    let mut lang_bcp47 = None;
    let mut default_duration = None;
    for (id, v) in elements(d) {
        match id {
            TRACK_TYPE => {
                kind = match uint(v) {
                    1 => Some(TrackKind::Video),
                    2 => Some(TrackKind::Audio),
                    17 => Some(TrackKind::Subtitle),
                    _ => None,
                }
            }
            CODEC_ID => codec = Some(string(v)),
            LANGUAGE => lang = Some(string(v)),
            LANGUAGE_BCP47 => lang_bcp47 = Some(string(v)),
            DEFAULT_DURATION => default_duration = Some(uint(v)),
            VIDEO => {
                for (vid, vv) in elements(v) {
                    match vid {
                        PIXEL_WIDTH => t.width = Some(uint(vv) as u32),
                        PIXEL_HEIGHT => t.height = Some(uint(vv) as u32),
                        COLOUR => {
                            for (cid, cv) in elements(vv) {
                                if cid == TRANSFER {
                                    t.hdr = super::hdr_from_transfer(uint(cv) as u16).map(str::to_owned);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            AUDIO => {
                for (aid, av) in elements(v) {
                    if aid == CHANNELS {
                        t.channels = Some(uint(av) as u32);
                    }
                }
            }
            BLOCK_ADDITION_MAPPING => {
                for (bid, bv) in elements(v) {
                    if bid == BLOCK_ADD_ID_TYPE && matches!(&uint(bv).to_be_bytes()[4..], b"dvcC" | b"dvvC" | b"dvwC") {
                        t.hdr = Some("Dolby Vision".into());
                    }
                }
            }
            _ => {}
        }
    }
    t.kind = kind?;
    t.codec = codec.as_deref().map(codec_name);
    t.language = lang_bcp47.or(lang).filter(|l| l != "und" && !l.is_empty());
    if t.kind == TrackKind::Video
        && let Some(ns) = default_duration.filter(|n| *n > 0)
    {
        t.fps = Some(1e9 / ns as f64);
    }
    Some(t)
}

fn codec_name(id: &str) -> String {
    let n = match id {
        "V_MPEGH/ISO/HEVC" => "hevc",
        "V_MPEG4/ISO/AVC" => "h264",
        "V_AV1" => "av1",
        "V_VP9" => "vp9",
        "V_VP8" => "vp8",
        "V_MPEG2" => "mpeg2",
        "V_MS/VFW/FOURCC" => "vfw",
        "A_AC3" => "ac3",
        "A_EAC3" => "eac3",
        "A_TRUEHD" => "truehd",
        "A_OPUS" => "opus",
        "A_FLAC" => "flac",
        "A_VORBIS" => "vorbis",
        "A_MPEG/L3" => "mp3",
        "A_MPEG/L2" => "mp2",
        s if s.starts_with("V_MPEG4/ISO/") => "mpeg4",
        s if s.starts_with("A_AAC") => "aac",
        s if s.starts_with("A_DTS") => "dts",
        s if s.starts_with("A_PCM") => "pcm",
        s if s.starts_with("V_REAL") => "real",
        other => return other.to_lowercase(),
    };
    n.to_owned()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn id_bytes(id: u32) -> Vec<u8> {
        let b = id.to_be_bytes();
        let skip = b.iter().position(|&x| x != 0).unwrap_or(3);
        b[skip..].to_vec()
    }

    fn size_bytes(n: usize) -> Vec<u8> {
        // Always use an 8-byte size for simplicity.
        let mut v = vec![0x01];
        v.extend(&(n as u64).to_be_bytes()[1..]);
        v
    }

    pub fn el(id: u32, body: &[u8]) -> Vec<u8> {
        [id_bytes(id), size_bytes(body.len()), body.to_vec()].concat()
    }

    fn u(id: u32, v: u64) -> Vec<u8> {
        el(id, &v.to_be_bytes())
    }

    /// Synthetic MKV: HEVC 1920x800 23.976fps HDR10, two audio tracks.
    /// With `tracks_after_cluster`, Tracks are only reachable via the SeekHead.
    pub fn sample(tracks_after_cluster: bool) -> Vec<u8> {
        let header = el(EBML, &el(0x4282, b"matroska"));
        let info = el(INFO, &[u(TIMESTAMP_SCALE, 1_000_000), el(DURATION, &(2_700_000.0f64).to_be_bytes())].concat());
        let video = el(VIDEO, &[u(PIXEL_WIDTH, 1920), u(PIXEL_HEIGHT, 800), el(COLOUR, &u(TRANSFER, 16))].concat());
        let t1 = el(
            TRACK_ENTRY,
            &[u(TRACK_TYPE, 1), el(CODEC_ID, b"V_MPEGH/ISO/HEVC"), u(DEFAULT_DURATION, 41_708_333), video].concat(),
        );
        let t2 = el(
            TRACK_ENTRY,
            &[u(TRACK_TYPE, 2), el(CODEC_ID, b"A_EAC3"), el(LANGUAGE, b"ger"), el(AUDIO, &u(CHANNELS, 6))].concat(),
        );
        let t3 = el(TRACK_ENTRY, &[u(TRACK_TYPE, 2), el(CODEC_ID, b"A_AAC"), el(AUDIO, &u(CHANNELS, 2))].concat());
        let tracks = el(TRACKS, &[t1, t2, t3].concat());
        let cluster = el(CLUSTER, &[0u8; 32]);
        let body = if tracks_after_cluster {
            // SeekHead first (fixed size, so positions can be computed), then Info, Cluster, Tracks.
            let seekhead_len =
                el(SEEK_HEAD, &el(SEEK, &[el(SEEK_ID, &id_bytes(TRACKS)), u(SEEK_POSITION, 0)].concat())).len();
            let tracks_pos = (seekhead_len + info.len() + cluster.len()) as u64;
            let seekhead =
                el(SEEK_HEAD, &el(SEEK, &[el(SEEK_ID, &id_bytes(TRACKS)), u(SEEK_POSITION, tracks_pos)].concat()));
            [seekhead, info, cluster, tracks].concat()
        } else {
            [info, tracks, cluster].concat()
        };
        [header, el(SEGMENT, &body)].concat()
    }

    #[test]
    fn reads_tracks_in_both_layouts() {
        for after in [false, true] {
            let data = sample(after);
            let p = probe(&mut std::io::Cursor::new(&data), data.len() as u64).unwrap();
            let v = p.video().unwrap();
            assert_eq!((v.width, v.height), (Some(1920), Some(800)), "after={after}");
            assert_eq!(v.codec.as_deref(), Some("hevc"));
            assert_eq!(v.hdr.as_deref(), Some("HDR10"));
            assert!((v.fps.unwrap() - 23.976).abs() < 0.01);
            assert_eq!(p.duration, Some(2700.0));
            let langs: Vec<_> = p.audio().filter_map(|a| a.language.clone()).collect();
            assert_eq!(langs, ["ger", "eng"]);
        }
    }

    #[test]
    fn rejects_non_matroska() {
        let data = b"RIFF....AVI LIST".to_vec();
        assert!(probe(&mut std::io::Cursor::new(&data), data.len() as u64).is_err());
    }

    #[test]
    fn vints() {
        assert_eq!(vint_slice(&[0x81], false), Some((1, 1)));
        assert_eq!(vint_slice(&[0x40, 0x02], false), Some((2, 2)));
        assert_eq!(vint_slice(&[0xFF], false), Some((u64::MAX, 1)));
        assert_eq!(vint_slice(&[0x1A, 0x45, 0xDF, 0xA3], true), Some((0x1A45DFA3, 4)));
    }
}
