//! What's inside an archive, read-only: nothing is ever extracted to disk.
//!
//! The per-format readers are cribbed from flux `src/services/archive/backends/`, minus
//! extraction, writing and the one-level-at-a-time listing: here every reader feeds full
//! paths into a `Lister`, which enforces the caps, and `Tree::build` does the rest.

mod iso;
mod pkzip;
mod rar;
mod sevenz;
mod tarball;
mod tool;

use crate::session::Session;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, Instant};

/// More entries than anyone scrolls through; ~100 bytes each.
const MAX_ENTRIES: usize = 200_000;
/// Streamed formats (compressed tar, deb) must decompress everything to list it.
const STREAM_BUDGET: Duration = Duration::from_secs(3);

/// A compression layer around a tarball or a single file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    None,
    Gzip,
    Bzip2,
    Xz,
    Lzma,
    Zstd,
    Lz4,
}

impl Codec {
    fn from_magic(head: &[u8]) -> Option<Codec> {
        Some(if head.starts_with(&[0x1f, 0x8b]) {
            Codec::Gzip
        } else if head.starts_with(b"BZh") {
            Codec::Bzip2
        } else if head.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0]) {
            Codec::Xz
        } else if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
            Codec::Zstd
        } else if head.starts_with(&[0x04, 0x22, 0x4d, 0x18]) {
            Codec::Lz4
        } else {
            return None;
        })
    }

    /// From a file name suffix; `.lzma` has no reliable magic.
    fn from_name(name: &str) -> Option<Codec> {
        let name = name.to_ascii_lowercase();
        let (_, ext) = name.rsplit_once('.')?;
        Some(match ext {
            "gz" | "tgz" => Codec::Gzip,
            "bz2" | "tbz2" | "tbz" => Codec::Bzip2,
            "xz" | "txz" => Codec::Xz,
            "lzma" | "tlz" => Codec::Lzma,
            "zst" | "zstd" | "tzst" => Codec::Zstd,
            "lz4" => Codec::Lz4,
            "tar" => Codec::None,
            _ => return None,
        })
    }

    fn label(self) -> &'static str {
        match self {
            Codec::None => "",
            Codec::Gzip => "GZIP",
            Codec::Bzip2 => "BZIP2",
            Codec::Xz => "XZ",
            Codec::Lzma => "LZMA",
            Codec::Zstd => "ZSTD",
            Codec::Lz4 => "LZ4",
        }
    }

    /// The decompressed stream.
    fn decoder<'a, R: Read + 'a>(self, reader: R) -> Result<Box<dyn Read + 'a>, String> {
        Ok(match self {
            Codec::None => Box::new(reader),
            Codec::Gzip => Box::new(flate2::read::MultiGzDecoder::new(reader)),
            Codec::Bzip2 => Box::new(bzip2::read::MultiBzDecoder::new(reader)),
            Codec::Xz => Box::new(xz2::read::XzDecoder::new_multi_decoder(reader)),
            Codec::Lzma => {
                let stream =
                    xz2::stream::Stream::new_lzma_decoder(u64::MAX).map_err(|e| e.to_string())?;
                Box::new(xz2::read::XzDecoder::new_stream(reader, stream))
            }
            Codec::Zstd => {
                Box::new(zstd::stream::read::Decoder::new(reader).map_err(|e| e.to_string())?)
            }
            Codec::Lz4 => Box::new(lz4_flex::frame::FrameDecoder::new(reader)),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Zip,
    SevenZ,
    Rar,
    Tar(Codec),
    /// One compressed file, not a tarball.
    Single(Codec),
    Iso {
        udf: bool,
    },
    Deb,
}

impl Format {
    pub fn label(self) -> String {
        match self {
            Format::Zip => "ZIP".into(),
            Format::SevenZ => "7-Zip".into(),
            Format::Rar => "RAR".into(),
            Format::Tar(Codec::None) => "TAR".into(),
            Format::Tar(codec) => format!("TAR · {}", codec.label()),
            Format::Single(codec) => codec.label().into(),
            Format::Iso { udf: false } => "ISO 9660".into(),
            Format::Iso { udf: true } => "UDF image".into(),
            Format::Deb => "Debian package".into(),
        }
    }

    fn streamed(self) -> bool {
        matches!(self, Format::Tar(_) | Format::Deb)
    }
}

/// By magic bytes; the name only breaks ties (`.lzma`, magic-less old tars).
pub fn detect(path: &Path) -> Result<Format, String> {
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut head = Vec::with_capacity(512);
    (&mut file)
        .take(512)
        .read_to_end(&mut head)
        .map_err(|e| e.to_string())?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

    if let Some(format) = detect_head(&head) {
        return Ok(format);
    }
    let by_name = Codec::from_name(name);
    let codec = Codec::from_magic(&head).or(by_name.filter(|c| *c == Codec::Lzma));
    if let Some(codec) = codec {
        // Peek inside: a tarball keeps its tree whatever the file is called.
        file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        let mut inner = Vec::with_capacity(512);
        let _ = codec.decoder(&mut file)?.take(512).read_to_end(&mut inner);
        let named_tar = name.to_ascii_lowercase().contains(".tar.")
            || matches!(
                name.rsplit_once('.')
                    .map(|(_, e)| e.to_ascii_lowercase())
                    .as_deref(),
                Some("tgz" | "tbz2" | "tbz" | "txz" | "tlz" | "tzst")
            );
        return Ok(if is_tar(&inner) || named_tar {
            Format::Tar(codec)
        } else {
            Format::Single(codec)
        });
    }
    if by_name == Some(Codec::None) {
        return Ok(Format::Tar(Codec::None));
    }

    // ISO 9660 ("CD001") or UDF ("BEA01"/"NSR02") volume descriptors at sector 16.
    let mut vd = [0u8; 6];
    if file.seek(SeekFrom::Start(16 * 2048)).is_ok() && file.read_exact(&mut vd).is_ok() {
        match &vd[1..6] {
            b"CD001" => {
                return Ok(Format::Iso {
                    udf: iso::has_udf(&mut file),
                });
            }
            b"BEA01" | b"NSR02" | b"NSR03" => return Ok(Format::Iso { udf: true }),
            _ => {}
        }
    }
    Err("Not a recognised archive".into())
}

fn detect_head(head: &[u8]) -> Option<Format> {
    Some(
        if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
            Format::Zip
        } else if head.starts_with(b"7z\xbc\xaf\x27\x1c") {
            Format::SevenZ
        } else if head.starts_with(b"Rar!\x1a\x07") {
            Format::Rar
        } else if head.starts_with(b"!<arch>\n") {
            Format::Deb
        } else if is_tar(head) {
            Format::Tar(Codec::None)
        } else {
            return None;
        },
    )
}

fn is_tar(block: &[u8]) -> bool {
    block.get(257..262) == Some(b"ustar")
}

/// One entry as a reader saw it.
#[derive(Debug, Default, Clone)]
pub struct Raw {
    pub path: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub mtime: Option<i64>,
    pub encrypted: bool,
}

/// Collects entries under the caps (ZAP rule 4) and the session (rule 3).
pub struct Lister<'a> {
    entries: Vec<Raw>,
    session: &'a Session,
    id: u64,
    started: Instant,
    streamed: bool,
    truncated: Option<String>,
}

impl<'a> Lister<'a> {
    fn new(session: &'a Session, id: u64, streamed: bool) -> Self {
        Self {
            entries: Vec::new(),
            session,
            id,
            started: Instant::now(),
            streamed,
            truncated: None,
        }
    }

    pub fn alive(&self) -> bool {
        self.session.is_current(self.id)
    }

    /// Adds one entry; `false` means stop listing.
    pub fn push(&mut self, raw: Raw) -> bool {
        if self.entries.len() >= MAX_ENTRIES {
            self.truncated = Some(format!(
                "Showing the first {} entries",
                group_digits(MAX_ENTRIES as u64)
            ));
            return false;
        }
        self.entries.push(raw);
        if self.entries.len().is_multiple_of(256) {
            if !self.alive() {
                return false;
            }
            if self.over_budget() {
                self.stop_for_time();
                return false;
            }
        }
        true
    }

    fn over_budget(&self) -> bool {
        self.streamed && self.started.elapsed() > STREAM_BUDGET
    }

    fn stop_for_time(&mut self) {
        self.truncated = Some(format!(
            "Listing stopped after {} s: {} entries shown",
            STREAM_BUDGET.as_secs(),
            group_digits(self.entries.len() as u64)
        ));
    }

    /// Wraps a decompressed stream so that a single huge member can't blow the budget or
    /// outlive the session: reads fail once either runs out. It has to sit after the
    /// decoder: a few KiB of input can inflate to gigabytes.
    pub fn guard<R: Read>(&self, inner: R) -> Guarded<'a, R> {
        Guarded {
            inner,
            session: self.session,
            id: self.id,
            deadline: self.started + STREAM_BUDGET,
            reads: 0,
        }
    }

    /// A reader failed after listing some entries: keep them, say why it stopped.
    fn stopped(&mut self, err: String) {
        if self.truncated.is_some() {
            return;
        }
        if self.over_budget() {
            self.stop_for_time();
        } else {
            self.truncated = Some(format!("Listing stopped early: {err}"));
        }
    }
}

pub struct Guarded<'a, R> {
    inner: R,
    session: &'a Session,
    id: u64,
    deadline: Instant,
    reads: u32,
}

impl<R: Read> Read for Guarded<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reads = self.reads.wrapping_add(1);
        if self.reads.is_multiple_of(64)
            && (Instant::now() > self.deadline || !self.session.is_current(self.id))
        {
            return Err(std::io::Error::other("listing budget spent"));
        }
        self.inner.read(buf)
    }
}

pub struct Listing {
    pub format: String,
    pub tree: Tree,
    /// The archive's own size on disk.
    pub packed: u64,
    pub encrypted: usize,
    /// Why the listing is partial, if it is.
    pub truncated: Option<String>,
    /// Package metadata (deb), shown above the tree.
    pub notes: Vec<(String, String)>,
}

/// Lists `path`. Blocking; call on the pool.
pub fn load(path: &Path, session: &Session, id: u64) -> Result<Listing, String> {
    crate::hit!("archive");
    let format = detect(path)?;
    let packed = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mut lister = Lister::new(session, id, format.streamed());
    let mut notes = Vec::new();

    let result = match format {
        Format::Zip => pkzip::list(path, &mut lister),
        Format::SevenZ => sevenz::list(path, &mut lister),
        Format::Rar => rar::list(path, &mut lister),
        Format::Tar(codec) => tarball::list_file(path, codec, &mut lister),
        Format::Single(codec) => tarball::single(path, codec, &mut lister),
        Format::Iso { udf } => iso::list(path, udf, &mut lister),
        Format::Deb => tarball::deb(path, &mut lister, &mut notes),
    };
    if let Err(err) = result {
        if lister.entries.is_empty() {
            return Err(err);
        }
        lister.stopped(err);
    }

    let encrypted = lister
        .entries
        .iter()
        .filter(|e| e.encrypted && !e.is_dir)
        .count();
    Ok(Listing {
        format: format.label(),
        tree: Tree::build(lister.entries),
        packed,
        encrypted,
        truncated: lister.truncated,
        notes,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub name: String,
    pub is_dir: bool,
    /// Folders: everything below. Files: as stored, or 0 when `known` is false.
    pub size: u64,
    pub known: bool,
    pub mtime: Option<i64>,
    pub encrypted: bool,
    /// Files anywhere below (folders only).
    pub files: u64,
    pub parent: u32,
    /// Sorted: folders first, then by name.
    pub children: Vec<u32>,
}

/// Every entry once, as a tree; node 0 is the root.
#[derive(Debug, Clone)]
pub struct Tree {
    pub nodes: Vec<Node>,
}

impl Tree {
    pub fn build(entries: Vec<Raw>) -> Tree {
        let mut nodes = vec![Node {
            name: String::new(),
            is_dir: true,
            size: 0,
            known: true,
            mtime: None,
            encrypted: false,
            files: 0,
            parent: 0,
            children: Vec::new(),
        }];
        let mut index: HashMap<(u32, String), u32> = HashMap::new();

        for raw in entries {
            let path = raw.path.replace('\\', "/");
            let parts: Vec<&str> = path
                .split('/')
                .filter(|p| !p.is_empty() && *p != ".")
                .collect();
            let Some(last) = parts.len().checked_sub(1) else {
                continue;
            };
            let mut parent = 0u32;
            for (i, part) in parts.iter().enumerate() {
                let leaf = i == last;
                let is_dir = !leaf || raw.is_dir;
                let key = (parent, (*part).to_owned());
                let idx = match index.get(&key) {
                    Some(&idx) => idx,
                    None => {
                        let idx = nodes.len() as u32;
                        nodes.push(Node {
                            name: (*part).to_owned(),
                            is_dir,
                            size: 0,
                            known: true,
                            mtime: None,
                            encrypted: false,
                            files: 0,
                            parent,
                            children: Vec::new(),
                        });
                        nodes[parent as usize].children.push(idx);
                        index.insert(key, idx);
                        idx
                    }
                };
                if leaf {
                    let node = &mut nodes[idx as usize];
                    // A folder that also appears as a file entry stays a folder.
                    node.is_dir |= is_dir;
                    if !node.is_dir {
                        node.size = raw.size.unwrap_or(0);
                        node.known = raw.size.is_some();
                    }
                    node.mtime = raw.mtime.or(node.mtime);
                    node.encrypted = raw.encrypted;
                }
                parent = idx;
            }
        }

        // Children always come after their parent: one reverse pass aggregates.
        for idx in (1..nodes.len()).rev() {
            let (size, known, files, mtime, parent) = {
                let n = &nodes[idx];
                let files = if n.is_dir { n.files } else { 1 };
                (n.size, n.known, files, n.mtime, n.parent as usize)
            };
            let p = &mut nodes[parent];
            p.size += size;
            p.known &= known;
            p.files += files;
            // Folders show the newest time inside them, or their own if that is newer.
            if let Some(m) = mtime
                && p.mtime.is_none_or(|t| m > t)
            {
                p.mtime = Some(m);
            }
        }
        for idx in 0..nodes.len() {
            let mut children = std::mem::take(&mut nodes[idx].children);
            children.sort_by_cached_key(|&c| {
                let n = &nodes[c as usize];
                (!n.is_dir, natural_key(&n.name), n.name.clone())
            });
            nodes[idx].children = children;
        }
        Tree { nodes }
    }

    pub fn root(&self) -> &Node {
        &self.nodes[0]
    }

    pub fn folders(&self) -> usize {
        self.nodes.iter().skip(1).filter(|n| n.is_dir).count()
    }
}

/// Case-insensitive, with digit runs compared as numbers: `f2` before `f10`.
fn natural_key(name: &str) -> String {
    let mut key = String::with_capacity(name.len() + 8);
    let mut digits = String::new();
    let flush = |digits: &mut String, key: &mut String| {
        if !digits.is_empty() {
            let trimmed = digits.trim_start_matches('0');
            key.push_str(&format!("{:0>20}", trimmed));
            digits.clear();
        }
    };
    for c in name.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
        } else {
            flush(&mut digits, &mut key);
            key.extend(c.to_lowercase());
        }
    }
    flush(&mut digits, &mut key);
    key
}

/// `1234567` → `1,234,567`.
pub fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A wall-clock time in the local zone (zip, 7z -slt, unrar) as a unix timestamp.
fn local_time(y: i32, mo: i32, d: i32, h: i32, mi: i32, s: f64) -> Option<i64> {
    glib::DateTime::from_local(y, mo, d, h, mi, s)
        .ok()
        .map(|dt| dt.to_unix())
}

/// `2024-05-01 10:00:00[.fraction|,fraction]` → unix time.
fn parse_local_time(text: &str) -> Option<i64> {
    let (date, time) = text.trim().split_once(' ')?;
    let mut date = date.split('-').map(|p| p.parse::<i32>().ok());
    let (y, mo, d) = (date.next()??, date.next()??, date.next()??);
    let time = time.split(['.', ',']).next()?;
    let mut time = time.split(':').map(|p| p.parse::<i32>().ok());
    let (h, mi) = (time.next()??, time.next()??);
    let s = time.next().flatten().unwrap_or(0);
    local_time(y, mo, d, h, mi, s as f64)
}

/// `Key = value` (7z) or `Key: value` (unrar) blocks, one per entry, separated by blank
/// lines. Keys are lower-cased and trimmed.
fn key_value_blocks(text: &str, sep: char) -> Vec<HashMap<String, String>> {
    let mut blocks = Vec::new();
    let mut block = HashMap::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            if !block.is_empty() {
                blocks.push(std::mem::take(&mut block));
            }
            continue;
        }
        if let Some((key, value)) = line.split_once(sep) {
            block.insert(key.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    if !block.is_empty() {
        blocks.push(block);
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(path: &str, is_dir: bool, size: u64) -> Raw {
        Raw {
            path: path.into(),
            is_dir,
            size: Some(size),
            ..Raw::default()
        }
    }

    fn child<'t>(tree: &'t Tree, node: &Node, name: &str) -> &'t Node {
        node.children
            .iter()
            .map(|&c| &tree.nodes[c as usize])
            .find(|n| n.name == name)
            .unwrap_or_else(|| panic!("no {name}"))
    }

    #[test]
    fn builds_implicit_folders_and_aggregates() {
        let tree = Tree::build(vec![
            raw("./src/main.rs", false, 10),
            raw("src/lib/mod.rs", false, 5),
            raw("/README", false, 1),
            raw("src/", true, 0),
            raw("src\\win.rs", false, 4),
        ]);
        let root = tree.root();
        assert_eq!((root.size, root.files), (20, 4));
        let names: Vec<_> = root
            .children
            .iter()
            .map(|&c| tree.nodes[c as usize].name.as_str())
            .collect();
        assert_eq!(names, ["src", "README"]);
        let src = child(&tree, root, "src");
        assert!(src.is_dir);
        assert_eq!((src.size, src.files), (19, 3));
        let names: Vec<_> = src
            .children
            .iter()
            .map(|&c| tree.nodes[c as usize].name.as_str())
            .collect();
        assert_eq!(names, ["lib", "main.rs", "win.rs"]);
        assert_eq!(tree.folders(), 2);

        let tree = Tree::build(vec![
            raw("f10", false, 1),
            raw("F2", false, 1),
            raw("f1", false, 1),
        ]);
        let names: Vec<_> = tree
            .root()
            .children
            .iter()
            .map(|&c| tree.nodes[c as usize].name.as_str())
            .collect();
        assert_eq!(names, ["f1", "F2", "f10"]);
    }

    #[test]
    fn merges_duplicates_and_unknown_sizes() {
        let mut unknown = raw("a.txt", false, 0);
        unknown.size = None;
        let tree = Tree::build(vec![raw("d/x", false, 3), raw("d/x", false, 7), unknown]);
        let root = tree.root();
        assert_eq!(root.children.len(), 2);
        assert_eq!(child(&tree, child(&tree, root, "d"), "x").size, 7);
        assert!(!child(&tree, root, "a.txt").known);
        assert!(!root.known);
    }

    #[test]
    fn detects_by_magic() {
        assert_eq!(detect_head(b"PK\x03\x04rest"), Some(Format::Zip));
        assert_eq!(detect_head(b"7z\xbc\xaf\x27\x1c"), Some(Format::SevenZ));
        assert_eq!(detect_head(b"Rar!\x1a\x07\x01\x00"), Some(Format::Rar));
        assert_eq!(detect_head(b"!<arch>\ndebian-binary"), Some(Format::Deb));
        let mut block = vec![0u8; 512];
        block[257..262].copy_from_slice(b"ustar");
        assert_eq!(detect_head(&block), Some(Format::Tar(Codec::None)));
        assert_eq!(detect_head(b"\x1f\x8b\x08"), None);
        assert_eq!(Codec::from_magic(b"\x1f\x8b\x08"), Some(Codec::Gzip));
        assert_eq!(Codec::from_magic(b"\xfd7zXZ\x00"), Some(Codec::Xz));
        assert_eq!(Codec::from_name("x.TLZ"), Some(Codec::Lzma));
    }

    #[test]
    fn parses_local_times_and_digits() {
        let t = parse_local_time("2024-05-01 10:00:00,123456789").unwrap();
        let u = parse_local_time("2024-05-01 10:00").unwrap();
        assert_eq!(t, u);
        assert_eq!(group_digits(1_234_567), "1,234,567");
        assert_eq!(group_digits(999), "999");
    }
}
