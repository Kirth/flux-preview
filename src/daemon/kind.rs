//! What kind of preview a file gets: extension first (no I/O), sniff only when needed.

use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Image,
    Text,
    Html,
    Pdf,
    Video,
    Audio,
    Archive,
    Binary,
    Unknown,
}

const SNIFF_LEN: u64 = 4096;

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Image => "Image",
            Kind::Text => "Text",
            Kind::Html => "HTML",
            Kind::Pdf => "PDF",
            Kind::Video => "Video",
            Kind::Audio => "Audio",
            Kind::Archive => "Archive",
            Kind::Binary => "Binary",
            Kind::Unknown => "File",
        }
    }

    /// Whether a freedesktop thumbnail is worth showing while the real preview loads.
    pub fn has_thumbnail(self) -> bool {
        matches!(self, Kind::Image | Kind::Video | Kind::Pdf)
    }

    fn from_mime(mime: &str) -> Option<Kind> {
        let (top, sub) = mime.split_once('/')?;
        Some(match (top, sub) {
            (_, "pdf") => Kind::Pdf,
            ("text", "html") | ("application", "xhtml+xml") => Kind::Html,
            ("text", _) => Kind::Text,
            ("application", "json" | "xml" | "javascript" | "x-shellscript" | "toml" | "yaml") => {
                Kind::Text
            }
            ("image", _) => Kind::Image,
            ("video", _) => Kind::Video,
            ("audio", _) => Kind::Audio,
            (
                "application",
                "zip"
                | "x-zip-compressed"
                | "java-archive"
                | "x-7z-compressed"
                | "x-tar"
                | "x-compressed-tar"
                | "x-rar"
                | "x-rar-compressed"
                | "vnd.rar"
                | "gzip"
                | "x-bzip2"
                | "x-xz"
                | "zstd"
                | "x-lz4"
                | "x-iso9660-image"
                | "vnd.debian.binary-package",
            ) => Kind::Archive,
            _ => return None,
        })
    }
}

pub fn from_extension(path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" | "jpg" | "jpeg" | "jpe" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "avif"
        | "heic" | "heif" | "jxl" | "ico" | "svg" | "svgz" | "tga" | "pnm" | "pbm" | "pgm"
        | "ppm" | "xpm" => Kind::Image,
        "html" | "htm" | "xhtml" => Kind::Html,
        "pdf" => Kind::Pdf,
        "mp4" | "m4v" | "mkv" | "webm" | "mov" | "avi" | "wmv" | "ogv" | "mpg" | "mpeg"
        | "m2ts" | "3gp" => Kind::Video,
        "mp3" | "flac" | "ogg" | "oga" | "opus" | "m4a" | "aac" | "wav" | "wma" | "aif"
        | "aiff" | "ape" | "wv" => Kind::Audio,
        "txt" | "md" | "markdown" | "rst" | "log" | "csv" | "tsv" | "json" | "toml" | "yaml"
        | "yml" | "ini" | "cfg" | "conf" | "xml" | "rs" | "py" | "c" | "h" | "cc" | "cpp"
        | "hpp" | "go" | "java" | "kt" | "js" | "mjs" | "ts" | "tsx" | "jsx" | "css" | "scss"
        | "sh" | "bash" | "zsh" | "fish" | "nix" | "lua" | "rb" | "pl" | "sql" | "diff"
        | "patch" | "lock" | "srt" | "vtt" => Kind::Text,
        // `foo.tar.gz` lands here as `gz`; the loader sniffs tar vs. single file.
        "zip" | "jar" | "war" | "apk" | "aar" | "cbz" | "whl" | "xpi" | "vsix" | "nupkg"
        | "ipa" | "7z" | "rar" | "cbr" | "tar" | "tgz" | "tbz" | "tbz2" | "txz" | "tlz"
        | "tzst" | "gz" | "bz2" | "xz" | "lzma" | "zst" | "zstd" | "lz4" | "iso" | "deb" => {
            Kind::Archive
        }
        _ => return None,
    })
}

/// Final classification plus a MIME type for display. Reads at most 4 KiB.
///
/// `hint` is the extension verdict; the sniff can only demote it (a `.txt` full of NULs is
/// binary), except for unknown extensions where the sniff decides.
pub fn resolve(path: &Path, hint: Option<Kind>) -> (Kind, String) {
    crate::hit!("resolve");
    let mut head = Vec::with_capacity(SNIFF_LEN as usize);
    if let Ok(file) = std::fs::File::open(path) {
        let _ = file.take(SNIFF_LEN).read_to_end(&mut head);
    }
    let name = path.file_name().and_then(|n| n.to_str());
    let (ctype, _uncertain) = gio::content_type_guess(name, head.as_slice());
    let mime = gio::content_type_get_mime_type(&ctype)
        .map(|m| m.to_string())
        .unwrap_or_else(|| ctype.to_string());
    let has_nul = head.contains(&0);

    let kind = match hint {
        Some(Kind::Text | Kind::Html) if has_nul => Kind::Binary,
        Some(kind) => kind,
        None => match Kind::from_mime(&mime) {
            Some(Kind::Text | Kind::Html) if has_nul => Kind::Binary,
            Some(kind) => kind,
            None if has_nul => Kind::Binary,
            None if !head.is_empty() && std::str::from_utf8(&head).is_ok() => Kind::Text,
            None => Kind::Unknown,
        },
    };
    (kind, mime)
}
