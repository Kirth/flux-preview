use super::Pixels;
use gdk_pixbuf::Pixbuf;
use lofty::prelude::*;
use std::path::Path;
use std::time::Duration;

const COVER_SIZE: i32 = 512;

#[derive(Default)]
pub struct AudioInfo {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration: Option<Duration>,
    pub cover: Option<Pixels>,
    pub playable: bool,
}

/// Tags + embedded cover, decoded here so the GTK thread only uploads pixels.
/// A file without readable tags still previews (with controls only).
pub fn load(path: &Path) -> AudioInfo {
    crate::hit!("audio");
    let mut info = AudioInfo {
        playable: super::playback_available(),
        ..AudioInfo::default()
    };
    let Ok(tagged) = lofty::read_from_path(path) else {
        return info;
    };
    info.duration = Some(tagged.properties().duration()).filter(|d| !d.is_zero());
    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        info.title = tag.title().map(|s| s.into_owned());
        info.artist = tag.artist().map(|s| s.into_owned());
        info.album = tag.album().map(|s| s.into_owned());
        info.cover = tag.pictures().first().and_then(|p| decode_cover(p.data()));
    }
    info
}

fn decode_cover(data: &[u8]) -> Option<Pixels> {
    let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from(data));
    let pixbuf = Pixbuf::from_stream_at_scale(
        &stream,
        COVER_SIZE,
        COVER_SIZE,
        true,
        None::<&gio::Cancellable>,
    )
    .ok()?;
    Some(Pixels::from_pixbuf(&pixbuf))
}
