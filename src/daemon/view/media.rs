//! Video and audio via GtkMediaFile (GStreamer).
//!
//! Only reached after `load::playback_available()` said yes on the pool: GtkMediaFile
//! aborts the process when playbin3 is missing.
//!
//! TODO: keep the thumbnail on screen until the first video frame arrives
//! (connect_invalidate_contents, flux `video_preview.rs:295-319`).

use super::Page;
use crate::load::AudioInfo;
use gtk::prelude::*;
use std::path::Path;

pub struct VideoPage {
    video: gtk::Video,
}

impl Page for VideoPage {
    const NAME: &'static str = "video";

    fn build() -> Self {
        let video = gtk::Video::builder()
            .autoplay(true)
            .hexpand(true)
            .vexpand(true)
            .build();
        Self { video }
    }

    fn widget(&self) -> gtk::Widget {
        self.video.clone().upcast()
    }
}

impl VideoPage {
    pub fn play(&self, path: &Path) {
        self.video.set_filename(Some(path));
    }

    /// Fired on a double click or double tap anywhere on the video.
    pub fn connect_double_tap(&self, f: impl Fn() + 'static) {
        let click = gtk::GestureClick::new();
        click.connect_pressed(move |_, n, _, _| {
            if n == 2 {
                f();
            }
        });
        self.video.add_controller(click);
    }

    pub fn stop(&self) {
        // Dropping the stream tears down the pipeline; pausing alone keeps decoders alive.
        self.video.set_media_stream(None::<&gtk::MediaStream>);
    }
}

pub struct AudioPage {
    root: gtk::Box,
    cover: gtk::Picture,
    title: gtk::Label,
    subtitle: gtk::Label,
    controls: gtk::MediaControls,
}

impl Page for AudioPage {
    const NAME: &'static str = "audio";

    fn build() -> Self {
        let cover = gtk::Picture::builder()
            .can_shrink(true)
            .width_request(256)
            .height_request(256)
            .content_fit(gtk::ContentFit::Contain)
            .build();
        cover.add_css_class("cover");
        let title = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();
        title.add_css_class("title-2");
        let subtitle = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();
        subtitle.add_css_class("dim-label");
        let controls = gtk::MediaControls::builder().width_request(320).build();

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .build();
        root.append(&cover);
        root.append(&title);
        root.append(&subtitle);
        root.append(&controls);
        Self {
            root,
            cover,
            title,
            subtitle,
            controls,
        }
    }

    fn widget(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }
}

impl AudioPage {
    pub fn play(&self, path: &Path, info: &AudioInfo) {
        let file_name = path.file_name().unwrap_or_default().to_string_lossy();
        self.title
            .set_text(info.title.as_deref().unwrap_or(&file_name));

        let mut parts: Vec<String> = [&info.artist, &info.album]
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        if let Some(d) = info.duration {
            parts.push(format!("{}:{:02}", d.as_secs() / 60, d.as_secs() % 60));
        }
        if !info.playable {
            parts.push("playback unavailable (GStreamer missing)".into());
        }
        self.subtitle.set_text(&parts.join(" · "));

        match &info.cover {
            Some(cover) => self.cover.set_paintable(Some(&cover.texture())),
            None => self.cover.set_paintable(
                gtk::IconTheme::for_display(&self.root.display())
                    .lookup_icon(
                        "audio-x-generic",
                        &[],
                        256,
                        1,
                        gtk::TextDirection::None,
                        gtk::IconLookupFlags::empty(),
                    )
                    .upcast_ref::<gtk::gdk::Paintable>()
                    .into(),
            ),
        }

        if info.playable {
            let stream = gtk::MediaFile::for_filename(path);
            self.controls.set_media_stream(Some(&stream));
            stream.play();
        }
    }

    pub fn stop(&self) {
        if let Some(stream) = self.controls.media_stream() {
            stream.pause();
        }
        self.controls.set_media_stream(None::<&gtk::MediaStream>);
    }
}
