//! Still images: dimensions (header only) and EXIF.

use std::fs::File;
use std::io::BufReader;

use exif::{In, Reader, Tag};
use kinsift_core::domain::{Domain, ExifMeta, ImageMeta};
use kinsift_core::provider::{Extraction, FileCtx, Provider, Stage};
use kinsift_core::value::unix_from_civil;
use serde_json::json;

pub struct ImageProvider;

/// Formats the EXIF reader does not understand (left to exiftool if present).
pub const NATIVE_UNSUPPORTED: &[&str] = &["cr3", "raf", "rw2", "orf"];

impl Provider for ImageProvider {
    fn name(&self) -> &'static str {
        "image"
    }
    fn version(&self) -> u32 {
        1
    }
    fn stage(&self) -> Stage {
        Stage::Headers
    }
    fn default_priority(&self) -> i32 {
        80
    }
    fn applies(&self, ctx: &FileCtx) -> bool {
        ctx.is_image() && !NATIVE_UNSUPPORTED.contains(&ctx.ext().as_str())
    }
    fn extract(&self, ctx: &FileCtx) -> Result<Option<Extraction>, String> {
        let mut sets = Vec::new();
        let mut raw = serde_json::Map::new();
        match imagesize::size(ctx.abs) {
            Ok(size) => {
                let (w, h) = (size.width as i64, size.height as i64);
                sets.push(
                    ImageMeta { width: Some(w), height: Some(h), pixels: Some(w * h), format: Some(ctx.ext()) }
                        .into_fields(),
                );
                raw.insert("width".into(), json!(w));
                raw.insert("height".into(), json!(h));
            }
            Err(e) => {
                raw.insert("size_error".into(), json!(e.to_string()));
            }
        }
        let file = File::open(ctx.abs).map_err(|e| e.to_string())?;
        if let Ok(exif) = Reader::new().read_from_container(&mut BufReader::new(file)) {
            let text = |tag: Tag| {
                exif.get_field(tag, In::PRIMARY).map(|f| match &f.value {
                    exif::Value::Ascii(parts) => {
                        parts.iter().map(|p| String::from_utf8_lossy(p).trim().to_owned()).collect::<Vec<_>>().join(" ")
                    }
                    _ => f.display_value().to_string(),
                })
            };
            let taken_at = [Tag::DateTimeOriginal, Tag::DateTimeDigitized, Tag::DateTime].into_iter().find_map(|tag| {
                let f = exif.get_field(tag, In::PRIMARY)?;
                let exif::Value::Ascii(parts) = &f.value else { return None };
                let dt = exif::DateTime::from_ascii(parts.first()?).ok()?;
                Some(unix_from_civil(
                    i64::from(dt.year),
                    u32::from(dt.month),
                    u32::from(dt.day),
                    u32::from(dt.hour),
                    u32::from(dt.minute),
                    u32::from(dt.second),
                ))
            });
            let make = text(Tag::Make).filter(|s| !s.is_empty());
            let model = text(Tag::Model).filter(|s| !s.is_empty());
            let meta = ExifMeta {
                taken_at,
                camera: camera_name(make.as_deref(), model.as_deref()),
                make,
                model,
                lens: text(Tag::LensModel).filter(|s| !s.is_empty()),
                software: text(Tag::Software).filter(|s| !s.is_empty()),
                orientation: exif
                    .get_field(Tag::Orientation, In::PRIMARY)
                    .and_then(|f| f.value.get_uint(0))
                    .map(i64::from),
            };
            for f in exif.fields().filter(|f| f.ifd_num == In::PRIMARY).take(64) {
                raw.insert(f.tag.to_string(), json!(f.display_value().to_string()));
            }
            sets.push(meta.into_fields());
        }
        Ok(Some(Extraction { sets, raw: Some(serde_json::Value::Object(raw)) }))
    }
}

/// "Canon" + "Canon EOS R5" → "Canon EOS R5"; "SONY" + "ILCE-7M3" → "SONY ILCE-7M3".
pub fn camera_name(make: Option<&str>, model: Option<&str>) -> Option<String> {
    match (make, model) {
        (Some(make), Some(model)) => {
            let first = make.split_whitespace().next().unwrap_or(make);
            if model.to_lowercase().starts_with(&first.to_lowercase()) {
                Some(model.to_owned())
            } else {
                Some(format!("{make} {model}"))
            }
        }
        (None, Some(m)) | (Some(m), None) => Some(m.to_owned()),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_names() {
        assert_eq!(camera_name(Some("Canon"), Some("Canon EOS R5")).as_deref(), Some("Canon EOS R5"));
        assert_eq!(camera_name(Some("NIKON CORPORATION"), Some("NIKON Z 6")).as_deref(), Some("NIKON Z 6"));
        assert_eq!(camera_name(Some("SONY"), Some("ILCE-7M3")).as_deref(), Some("SONY ILCE-7M3"));
        assert_eq!(camera_name(None, None), None);
    }

    /// A tiny JPEG with an EXIF APP1 segment, built by hand.
    fn jpeg_with_exif() -> Vec<u8> {
        // TIFF (little endian) with IFD0: Make, Model, ExifIFD pointer; Exif IFD: DateTimeOriginal.
        let make = b"Canon\0";
        let model = b"Canon EOS R5\0";
        let dt = b"2023:07:04 12:34:56\0";
        let mut t = b"II*\0".to_vec();
        t.extend(8u32.to_le_bytes());
        // IFD0 at 8: 3 entries
        let ifd0_len = 2 + 3 * 12 + 4;
        let data0 = 8 + ifd0_len; // where Make string goes
        let model_off = data0 + make.len();
        let exif_ifd = model_off + model.len();
        let exif_len = 2 + 12 + 4;
        let dt_off = exif_ifd + exif_len;
        t.extend(3u16.to_le_bytes());
        let entry = |tag: u16, typ: u16, count: u32, val: u32| {
            let mut e = tag.to_le_bytes().to_vec();
            e.extend(typ.to_le_bytes());
            e.extend(count.to_le_bytes());
            e.extend(val.to_le_bytes());
            e
        };
        t.extend(entry(0x010F, 2, make.len() as u32, data0 as u32));
        t.extend(entry(0x0110, 2, model.len() as u32, model_off as u32));
        t.extend(entry(0x8769, 4, 1, exif_ifd as u32));
        t.extend(0u32.to_le_bytes());
        t.extend_from_slice(make);
        t.extend_from_slice(model);
        t.extend(1u16.to_le_bytes());
        t.extend(entry(0x9003, 2, dt.len() as u32, dt_off as u32));
        t.extend(0u32.to_le_bytes());
        t.extend_from_slice(dt);
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend(t);
        let mut j = vec![0xFF, 0xD8, 0xFF, 0xE1];
        j.extend(((app1.len() + 2) as u16).to_be_bytes());
        j.extend(app1);
        // SOF0 with 640x480 so the size reader works
        j.extend([0xFF, 0xC0, 0x00, 0x11, 0x08]);
        j.extend(480u16.to_be_bytes());
        j.extend(640u16.to_be_bytes());
        j.extend([0x03, 0x01, 0x22, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01]);
        j.extend([0xFF, 0xD9]);
        j
    }

    #[test]
    fn photo_duplicates_view_groups_copies() {
        use kinsift_core::config::Config;
        use kinsift_core::filter::Filter;
        use kinsift_core::index::Index;
        use kinsift_core::provider::Priority;
        use kinsift_core::query::build_tree;
        use kinsift_core::scan::{ScanOptions, scan_root};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("export")).unwrap();
        std::fs::write(root.join("img_0001.jpg"), jpeg_with_exif()).unwrap();
        std::fs::write(root.join("export/img_0001 copy.jpg"), jpeg_with_exif()).unwrap();
        let mut other = jpeg_with_exif();
        let pos = other.windows(10).position(|w| w == b"2023:07:04").unwrap();
        other[pos..pos + 10].copy_from_slice(b"2024:01:01");
        std::fs::write(root.join("img_0002.jpg"), other).unwrap();

        let config = Config::defaults();
        let providers = crate::default_providers();
        let priority = Priority::new(config.providers.priority.clone(), &providers);
        let mut index = Index::open_in_memory().unwrap();
        scan_root(&mut index, &providers, &priority, root, &ScanOptions::default(), &|_| {}).unwrap();
        let view = config.view("photo-duplicates").unwrap();
        let filter = Filter::parse(view.filter.as_deref().unwrap()).unwrap();
        let entries = index.load_entries(Some(&filter), None).unwrap();
        assert_eq!(entries.len(), 3);
        let tree = build_tree(&entries, &view.group_by, view.duplicates_only);
        assert_eq!(tree.dup_groups, 1);
        assert_eq!(tree.file_count, 2, "only the two copies with the same capture time");
    }

    #[test]
    fn reads_size_and_exif() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("img_0001.jpg");
        std::fs::write(&path, jpeg_with_exif()).unwrap();
        let ctx = FileCtx { root: dir.path(), abs: &path, rel: "img_0001.jpg", size: 1, mtime_ns: 0 };
        let e = ImageProvider.extract(&ctx).unwrap().unwrap();
        let fields: Vec<(String, kinsift_core::value::Value)> =
            e.sets.iter().flat_map(|s| s.keys().map(|(k, v)| (k, v.clone()))).collect();
        let get = |k: &str| fields.iter().find(|(key, _)| key == k).map(|(_, v)| v.to_string());
        assert_eq!(get("image.width").as_deref(), Some("640"));
        assert_eq!(get("image.pixels").as_deref(), Some("307200"));
        assert_eq!(get("exif.camera").as_deref(), Some("Canon EOS R5"));
        assert_eq!(get("exif.taken_at").as_deref(), Some("2023-07-04 12:34"));
    }
}
