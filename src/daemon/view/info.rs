use super::Page;
use crate::load::Meta;
use gtk::prelude::*;
use std::path::Path;

/// Fallback for binary/unknown files and failed previews.
pub struct InfoPage {
    status: adw::StatusPage,
}

impl Page for InfoPage {
    const NAME: &'static str = "info";

    fn build() -> Self {
        Self {
            status: adw::StatusPage::new(),
        }
    }

    fn widget(&self) -> gtk::Widget {
        self.status.clone().upcast()
    }
}

impl InfoPage {
    pub fn set(&self, path: &Path, meta: &Meta, error: Option<&str>) {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        self.status.set_title(&name);
        let icon = gio::content_type_from_mime_type(&meta.mime)
            .and_then(|ctype| gio::content_type_get_generic_icon_name(&ctype))
            .unwrap_or_else(|| "text-x-generic".into());
        self.status.set_icon_name(Some(&icon));

        let mut lines = vec![meta.mime.clone(), format_size(meta.size)];
        if let Some(modified) = meta.modified.and_then(format_time) {
            lines.push(modified);
        }
        let mut description = glib::markup_escape_text(&lines.join(" · ")).to_string();
        if let Some(error) = error {
            description.push_str("\n\n");
            description.push_str(&glib::markup_escape_text(error));
        }
        self.status.set_description(Some(&description));
    }
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_time(time: std::time::SystemTime) -> Option<String> {
    let secs = time.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    let dt = glib::DateTime::from_unix_local(secs as i64).ok()?;
    dt.format("%Y-%m-%d %H:%M").ok().map(|s| s.to_string())
}
