//! Minimal ISO-BMFF (MP4/MOV) header reader: tracks, codecs, size, fps, HDR.
//!
//! Only the `moov` box is read (it may sit at the start or the end of the
//! file); media data is never touched.

use std::io::{Read, Seek, SeekFrom};

use super::{Probe, Track, TrackKind};

const MAX_MOOV: u64 = 64 << 20;

pub fn probe<R: Read + Seek>(r: &mut R, len: u64) -> Result<Probe, String> {
    let mut pos = 0u64;
    let mut moov = None;
    let mut saw_ftyp = false;
    while pos + 8 <= len {
        r.seek(SeekFrom::Start(pos)).map_err(|e| e.to_string())?;
        let mut hdr = [0u8; 16];
        r.read_exact(&mut hdr[..8]).map_err(|e| e.to_string())?;
        let mut size = u64::from(u32::from_be_bytes(hdr[0..4].try_into().unwrap()));
        let kind = [hdr[4], hdr[5], hdr[6], hdr[7]];
        let mut header_len = 8;
        if size == 1 {
            r.read_exact(&mut hdr[8..16]).map_err(|e| e.to_string())?;
            size = u64::from_be_bytes(hdr[8..16].try_into().unwrap());
            header_len = 16;
        } else if size == 0 {
            size = len - pos;
        }
        if size < header_len {
            return Err("corrupt box size".into());
        }
        match &kind {
            b"ftyp" => saw_ftyp = true,
            b"moov" => {
                let body = size - header_len;
                if body > MAX_MOOV {
                    return Err("moov box too large".into());
                }
                let mut buf = vec![0u8; body as usize];
                r.read_exact(&mut buf).map_err(|e| e.to_string())?;
                moov = Some(buf);
                break;
            }
            _ => {}
        }
        if pos == 0 && !saw_ftyp && &kind != b"moov" && &kind != b"mdat" && &kind != b"wide" && &kind != b"free" {
            return Err("not an ISO-BMFF file".into());
        }
        // `size >= 8`, so this always advances; a corrupt 64-bit size must
        // not wrap around and restart the loop.
        pos = pos.checked_add(size).ok_or("corrupt box size")?;
    }
    let moov = moov.ok_or("no moov box (incomplete file?)")?;
    parse_moov(&moov)
}

struct Boxes<'a> {
    data: &'a [u8],
}

impl<'a> Iterator for Boxes<'a> {
    type Item = (&'a [u8; 4], &'a [u8]);
    fn next(&mut self) -> Option<Self::Item> {
        if self.data.len() < 8 {
            return None;
        }
        let mut size = u32::from_be_bytes(self.data[0..4].try_into().ok()?) as usize;
        let kind: &[u8; 4] = self.data[4..8].try_into().ok()?;
        let mut header = 8;
        if size == 1 {
            size = u64::from_be_bytes(self.data.get(8..16)?.try_into().ok()?) as usize;
            header = 16;
        } else if size == 0 {
            size = self.data.len();
        }
        if size < header || size > self.data.len() {
            return None;
        }
        let body = &self.data[header..size];
        self.data = &self.data[size..];
        Some((kind, body))
    }
}

fn boxes(data: &[u8]) -> Boxes<'_> {
    Boxes { data }
}

fn child<'a>(data: &'a [u8], name: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(data).find(|(k, _)| *k == name).map(|(_, b)| b)
}

fn be_u16(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(d.get(at..at + 2)?.try_into().ok()?))
}
fn be_u32(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(at..at + 4)?.try_into().ok()?))
}
fn be_u64(d: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(d.get(at..at + 8)?.try_into().ok()?))
}

/// (timescale, duration) from mvhd or mdhd.
fn time_header(d: &[u8]) -> Option<(u32, u64)> {
    match d.first()? {
        1 => Some((be_u32(d, 20)?, be_u64(d, 24)?)),
        _ => Some((be_u32(d, 12)?, u64::from(be_u32(d, 16)?))),
    }
}

fn parse_moov(moov: &[u8]) -> Result<Probe, String> {
    let mut probe = Probe::default();
    if let Some((scale, dur)) = child(moov, b"mvhd").and_then(time_header)
        && scale > 0
    {
        probe.duration = Some(dur as f64 / f64::from(scale));
    }
    for (kind, trak) in boxes(moov) {
        if kind != b"trak" {
            continue;
        }
        if let Some(t) = parse_trak(trak) {
            probe.tracks.push(t);
        }
    }
    if probe.tracks.is_empty() {
        return Err("no tracks".into());
    }
    Ok(probe)
}

fn parse_trak(trak: &[u8]) -> Option<Track> {
    let mdia = child(trak, b"mdia")?;
    let hdlr = child(mdia, b"hdlr")?;
    let kind = match hdlr.get(8..12)? {
        b"vide" => TrackKind::Video,
        b"soun" => TrackKind::Audio,
        b"subt" | b"text" | b"sbtl" => TrackKind::Subtitle,
        _ => return None,
    };
    let mdhd = child(mdia, b"mdhd")?;
    let (timescale, _) = time_header(mdhd)?;
    let lang_at = if mdhd.first() == Some(&1) { 32 } else { 20 };
    let language = be_u16(mdhd, lang_at).and_then(decode_language);
    let stbl = child(child(mdia, b"minf")?, b"stbl")?;
    let stsd = child(stbl, b"stsd")?;
    // full box (4) + entry count (4), then the first sample entry
    let (fourcc, entry) = boxes(stsd.get(8..)?).next()?;
    let mut track = Track { kind, codec: Some(codec_name(fourcc)), language, ..Default::default() };
    match kind {
        TrackKind::Video => {
            track.width = be_u16(entry, 24).map(u32::from);
            track.height = be_u16(entry, 26).map(u32::from);
            // Child boxes of a visual sample entry start after 78 bytes.
            for (k, b) in entry.get(78..).map(boxes).into_iter().flatten() {
                match k {
                    b"colr" if b.get(0..4) == Some(b"nclx") => {
                        track.hdr = be_u16(b, 6).and_then(super::hdr_from_transfer).map(str::to_owned);
                    }
                    b"dvcC" | b"dvvC" | b"dvwC" => track.hdr = Some("Dolby Vision".into()),
                    _ => {}
                }
            }
            if matches!(fourcc, b"dvh1" | b"dvhe" | b"dva1" | b"dvav") {
                track.hdr = Some("Dolby Vision".into());
            }
            if let Some(stts) = child(stbl, b"stts")
                && be_u32(stts, 4).unwrap_or(0) > 0
                && let Some(delta) = be_u32(stts, 12)
                && delta > 0
            {
                track.fps = Some(f64::from(timescale) / f64::from(delta));
            }
        }
        TrackKind::Audio => track.channels = be_u16(entry, 16).map(u32::from),
        TrackKind::Subtitle => {}
    }
    Some(track)
}

fn decode_language(packed: u16) -> Option<String> {
    if packed == 0 || packed == 0x7fff {
        return None;
    }
    let chars: String = [10u16, 5, 0].iter().map(|s| (((packed >> s) & 0x1f) as u8 + 0x60) as char).collect();
    (chars.chars().all(|c| c.is_ascii_lowercase()) && chars != "und").then_some(chars)
}

fn codec_name(fourcc: &[u8; 4]) -> String {
    match fourcc {
        b"avc1" | b"avc3" => "h264",
        b"hvc1" | b"hev1" | b"dvh1" | b"dvhe" => "hevc",
        b"av01" | b"dav1" => "av1",
        b"vp09" => "vp9",
        b"vp08" => "vp8",
        b"mp4v" => "mpeg4",
        b"mp4a" => "aac",
        b"ac-3" => "ac3",
        b"ec-3" => "eac3",
        b"Opus" => "opus",
        b"fLaC" => "flac",
        b"alac" => "alac",
        b"mp3 " | b".mp3" => "mp3",
        other => return String::from_utf8_lossy(other).trim().to_lowercase(),
    }
    .to_owned()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }

    fn full(version: u8, rest: &[u8]) -> Vec<u8> {
        let mut v = vec![version, 0, 0, 0];
        v.extend_from_slice(rest);
        v
    }

    fn mvhd(scale: u32, dur: u32) -> Vec<u8> {
        let mut r = vec![0u8; 8];
        r.extend(scale.to_be_bytes());
        r.extend(dur.to_be_bytes());
        r.extend(vec![0u8; 80]);
        bx(b"mvhd", &full(0, &r))
    }

    fn mdhd(scale: u32, lang: &str) -> Vec<u8> {
        let mut r = vec![0u8; 8];
        r.extend(scale.to_be_bytes());
        r.extend(0u32.to_be_bytes());
        let b = lang.as_bytes();
        let packed = (u16::from(b[0] - 0x60) << 10) | (u16::from(b[1] - 0x60) << 5) | u16::from(b[2] - 0x60);
        r.extend(packed.to_be_bytes());
        r.extend([0, 0]);
        bx(b"mdhd", &full(0, &r))
    }

    fn hdlr(kind: &[u8; 4]) -> Vec<u8> {
        let mut r = vec![0u8; 4];
        r.extend_from_slice(kind);
        r.extend(vec![0u8; 13]);
        bx(b"hdlr", &full(0, &r))
    }

    fn video_entry(fourcc: &[u8; 4], w: u16, h: u16, extra: &[u8]) -> Vec<u8> {
        let mut e = vec![0u8; 24];
        e.extend(w.to_be_bytes());
        e.extend(h.to_be_bytes());
        e.extend(vec![0u8; 50]);
        e.extend_from_slice(extra);
        bx(fourcc, &e)
    }

    fn audio_entry(fourcc: &[u8; 4], channels: u16) -> Vec<u8> {
        let mut e = vec![0u8; 16];
        e.extend(channels.to_be_bytes());
        e.extend(vec![0u8; 10]);
        bx(fourcc, &e)
    }

    fn trak(handler: &[u8; 4], scale: u32, lang: &str, entry: Vec<u8>, stts_delta: Option<u32>) -> Vec<u8> {
        let mut stsd = full(0, &1u32.to_be_bytes());
        stsd.extend(entry);
        let mut stbl = bx(b"stsd", &stsd);
        if let Some(d) = stts_delta {
            let mut s = 1u32.to_be_bytes().to_vec();
            s.extend(100u32.to_be_bytes());
            s.extend(d.to_be_bytes());
            stbl.extend(bx(b"stts", &full(0, &s)));
        }
        let minf = bx(b"minf", &bx(b"stbl", &stbl));
        let mut mdia = mdhd(scale, lang);
        mdia.extend(hdlr(handler));
        mdia.extend(minf);
        bx(b"trak", &bx(b"mdia", &mdia))
    }

    /// A synthetic MP4 with an HEVC 3840x1600 HDR10 video and a 6-channel E-AC-3 track.
    pub fn sample(moov_at_end: bool) -> Vec<u8> {
        let mut colr = b"nclx".to_vec();
        colr.extend([0, 9, 0, 16, 0, 9, 0x80]);
        let mut moov = mvhd(1000, 5_400_000);
        moov.extend(trak(b"vide", 24000, "und", video_entry(b"hvc1", 3840, 1600, &bx(b"colr", &colr)), Some(1001)));
        moov.extend(trak(b"soun", 48000, "deu", audio_entry(b"ec-3", 6), None));
        let ftyp = bx(b"ftyp", b"isom\0\0\0\0isomiso2");
        let mdat = bx(b"mdat", &[0u8; 64]);
        let moov = bx(b"moov", &moov);
        if moov_at_end { [ftyp, mdat, moov].concat() } else { [ftyp, moov, mdat].concat() }
    }

    #[test]
    fn reads_tracks() {
        for at_end in [false, true] {
            let data = sample(at_end);
            let p = probe(&mut std::io::Cursor::new(&data), data.len() as u64).unwrap();
            let v = p.video().unwrap();
            assert_eq!((v.width, v.height), (Some(3840), Some(1600)));
            assert_eq!(v.codec.as_deref(), Some("hevc"));
            assert_eq!(v.hdr.as_deref(), Some("HDR10"));
            assert!((v.fps.unwrap() - 23.976).abs() < 0.01);
            assert_eq!(p.duration, Some(5400.0));
            let a = p.audio().next().unwrap();
            assert_eq!((a.codec.as_deref(), a.channels, a.language.as_deref()), (Some("eac3"), Some(6), Some("deu")));
        }
    }

    #[test]
    fn rejects_garbage() {
        let data = b"definitely not an mp4 file at all".to_vec();
        assert!(probe(&mut std::io::Cursor::new(&data), data.len() as u64).is_err());
    }

    #[test]
    fn huge_box_size_is_an_error_not_a_loop() {
        let mut data = bx(b"ftyp", b"isom\0\0\0\0");
        data.extend_from_slice(&1u32.to_be_bytes());
        data.extend_from_slice(b"junk");
        // Wraps the position back to 0 if added unchecked.
        data.extend_from_slice(&(u64::MAX - 15).to_be_bytes());
        let err = probe(&mut std::io::Cursor::new(&data), data.len() as u64).unwrap_err();
        assert!(err.contains("corrupt"), "{err}");
    }
}
