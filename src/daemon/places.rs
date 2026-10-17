//! Where each PDF was left: page, zoom and rotation, so reopening it continues there.
//!
//! A small text file in the state dir, one line per document, most recent last. Read once
//! on the pool after startup and written on the pool; never touched on the GTK thread.

use std::path::{Path, PathBuf};

/// Documents remembered; the oldest are forgotten first.
const KEEP: usize = 500;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Place {
    /// 0-based.
    pub page: u32,
    /// `None`: fitted to the window.
    pub zoom: Option<f64>,
    /// Clockwise degrees.
    pub rotation: u16,
}

#[derive(Default)]
pub struct Places {
    entries: Vec<(PathBuf, Place)>,
}

fn file() -> Option<PathBuf> {
    Some(
        dirs::state_dir()
            .or_else(dirs::cache_dir)?
            .join("flux-preview")
            .join("pdf-places"),
    )
}

impl Places {
    /// Blocking: call from the pool.
    pub fn load() -> Self {
        let text = file()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .unwrap_or_default();
        Self::parse(&text)
    }

    fn parse(text: &str) -> Self {
        let entries = text
            .lines()
            .filter_map(|line| {
                let mut fields = line.splitn(4, '\t');
                let page = fields.next()?.parse().ok()?;
                let zoom = match fields.next()? {
                    "fit" => None,
                    z => Some(z.parse().ok().filter(|z: &f64| z.is_finite() && *z > 0.0)?),
                };
                let rotation = fields.next()?.parse::<u16>().ok().filter(|r| r % 90 == 0)?;
                let path = PathBuf::from(fields.next()?);
                Some((
                    path,
                    Place {
                        page,
                        zoom,
                        rotation: rotation % 360,
                    },
                ))
            })
            .collect();
        Self { entries }
    }

    fn serialize(&self) -> String {
        self.entries
            .iter()
            .filter_map(|(path, place)| {
                let zoom = place.zoom.map_or("fit".into(), |z| z.to_string());
                let path = path.to_str().filter(|p| !p.contains('\n'))?;
                Some(format!(
                    "{}\t{zoom}\t{}\t{path}\n",
                    place.page, place.rotation
                ))
            })
            .collect()
    }

    pub fn get(&self, path: &Path) -> Option<Place> {
        self.entries
            .iter()
            .find(|(p, _)| p == path)
            .map(|&(_, place)| place)
    }

    /// Remembers `place` for `path` (forgets it when `None`, i.e. page 1 as opened).
    /// Returns the file contents to write when anything changed.
    pub fn set(&mut self, path: &Path, place: Option<Place>) -> Option<String> {
        let old = self.entries.iter().position(|(p, _)| p == path);
        if old.map(|i| self.entries[i].1) == place {
            return None;
        }
        if let Some(i) = old {
            self.entries.remove(i);
        }
        if let Some(place) = place {
            self.entries.push((path.to_path_buf(), place));
            let excess = self.entries.len().saturating_sub(KEEP);
            self.entries.drain(..excess);
        }
        Some(self.serialize())
    }
}

/// Blocking: call from the pool. Written whole and renamed into place.
pub fn write(contents: &str) {
    let Some(file) = file() else { return };
    let result = (|| {
        std::fs::create_dir_all(file.parent().expect("has a parent"))?;
        let tmp = file.with_extension("tmp");
        std::fs::write(&tmp, contents)?;
        std::fs::rename(&tmp, &file)
    })();
    if let Err(err) = result {
        eprintln!("[flux-preview] saving PDF places failed: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_forgets_the_oldest() {
        let mut places = Places::default();
        let place = Place {
            page: 4,
            zoom: Some(1.25),
            rotation: 90,
        };
        assert!(places.set(Path::new("/a b/x.pdf"), Some(place)).is_some());
        // Unchanged: nothing to write.
        assert!(places.set(Path::new("/a b/x.pdf"), Some(place)).is_none());
        let fit = Place {
            zoom: None,
            ..place
        };
        let text = places.set(Path::new("/y.pdf"), Some(fit)).unwrap();
        let back = Places::parse(&text);
        assert_eq!(back.get(Path::new("/a b/x.pdf")), Some(place));
        assert_eq!(back.get(Path::new("/y.pdf")), Some(fit));

        places.set(Path::new("/y.pdf"), None);
        assert_eq!(places.get(Path::new("/y.pdf")), None);
        for i in 0..KEEP {
            places.set(Path::new(&format!("/{i}.pdf")), Some(place));
        }
        assert_eq!(places.entries.len(), KEEP);
        assert_eq!(places.get(Path::new("/a b/x.pdf")), None);
    }

    #[test]
    fn ignores_broken_lines() {
        let places = Places::parse(
            "x\tfit\t0\t/a.pdf\n3\t-1\t0\t/b.pdf\n2\tfit\t45\t/c.pdf\n1\tfit\t270\t/d.pdf\n",
        );
        assert_eq!(places.entries.len(), 1);
        assert_eq!(
            places.get(Path::new("/d.pdf")).map(|p| p.rotation),
            Some(270)
        );
    }
}
