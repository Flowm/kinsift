//! Built-in metadata providers.
//!
//! Cargo features: `video` (MKV/MP4 readers), `image` (dimensions and EXIF),
//! `external` (ffprobe/exiftool when on PATH). All are on by default.

#[cfg(feature = "external")]
pub mod external;
pub mod fs;
#[cfg(feature = "image")]
pub mod image;
pub mod name;
#[cfg(feature = "video")]
pub mod video;

use kinsift_core::provider::Provider;

/// All built-in providers, in pipeline order.
pub fn default_providers() -> Vec<Box<dyn Provider>> {
    #[allow(unused_mut)] // without optional features nothing is pushed
    let mut providers: Vec<Box<dyn Provider>> = vec![Box::new(fs::FsProvider), Box::new(name::NameProvider)];
    #[cfg(feature = "video")]
    {
        providers.push(Box::new(video::MkvProvider));
        providers.push(Box::new(video::Mp4Provider));
    }
    #[cfg(feature = "image")]
    providers.push(Box::new(image::ImageProvider));
    #[cfg(feature = "external")]
    {
        providers.push(Box::new(external::FfprobeProvider));
        providers.push(Box::new(external::ExiftoolProvider));
    }
    providers
}
