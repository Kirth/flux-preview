//! tar in any compression, single compressed files, and Debian packages (an `ar`
//! container around two tarballs). From flux `backends/{tar,single,deb}.rs`; the deb
//! reader parses `ar` itself and streams the members in place instead of running `ar x`
//! into a temp dir.

use super::{Codec, Lister, Raw};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Biggest control file read from a deb.
const MAX_CONTROL: u64 = 64 * 1024;

pub fn list_file(path: &Path, codec: Codec, lister: &mut Lister) -> Result<(), String> {
    let file = std::io::BufReader::new(std::fs::File::open(path).map_err(|e| e.to_string())?);
    let reader = lister.guard(codec.decoder(file)?);
    list(reader, lister)
}

/// Streams `reader` as a tarball; entry data is skipped, but compressed data still has to
/// be decompressed to get past it.
fn list<R: Read>(reader: R, lister: &mut Lister) -> Result<(), String> {
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().map_err(|e| format!("TAR: {e}"))? {
        let entry = entry.map_err(|e| format!("TAR: {e}"))?;
        let header = entry.header();
        let path = entry.path().map_err(|e| format!("TAR path: {e}"))?;
        let path = path.to_string_lossy().into_owned();
        let kind = header.entry_type();
        let raw = Raw {
            path,
            is_dir: kind.is_dir(),
            size: Some(if kind.is_file() {
                header.size().unwrap_or(0)
            } else {
                0
            }),
            mtime: header.mtime().ok().map(|t| t as i64),
            encrypted: false,
        };
        if !lister.push(raw) {
            break;
        }
    }
    Ok(())
}

/// A lone compressed file: one entry, named after the archive minus its suffix.
pub fn single(path: &Path, codec: Codec, lister: &mut Lister) -> Result<(), String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let inner = match name.rsplit_once('.') {
        Some((stem, ext)) if Codec::from_name(&format!(".{ext}")) == Some(codec) => stem,
        _ => name.as_str(),
    };
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let size = match codec {
        Codec::Gzip => gzip_size(&mut file),
        _ => None,
    };
    let mtime = file
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64);
    lister.push(Raw {
        path: inner.to_owned(),
        is_dir: false,
        size,
        mtime,
        encrypted: false,
    });
    Ok(())
}

/// The gzip trailer's ISIZE: the uncompressed size mod 2³², so only trusted below 4 GiB
/// and for single-member files.
fn gzip_size<R: Read + Seek>(file: &mut R) -> Option<u64> {
    let len = file.seek(SeekFrom::End(0)).ok()?;
    if !(18..u32::MAX as u64).contains(&len) {
        return None;
    }
    file.seek(SeekFrom::End(-4)).ok()?;
    let mut isize = [0u8; 4];
    file.read_exact(&mut isize).ok()?;
    Some(u32::from_le_bytes(isize) as u64)
}

/// One member of an `ar` archive: where its data starts and how long it is.
#[derive(Debug, PartialEq)]
struct Member {
    name: String,
    offset: u64,
    size: u64,
}

/// The members of an `ar` archive (60-byte headers, data padded to even offsets).
fn ar_members<R: Read + Seek>(file: &mut R) -> Result<Vec<Member>, String> {
    let mut magic = [0u8; 8];
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    file.read_exact(&mut magic).map_err(|e| e.to_string())?;
    if &magic != b"!<arch>\n" {
        return Err("Not an ar archive".into());
    }
    let mut members = Vec::new();
    let mut offset = 8u64;
    loop {
        let mut header = [0u8; 60];
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| e.to_string())?;
        if file.read_exact(&mut header).is_err() {
            break;
        }
        if &header[58..60] != b"`\n" {
            return Err("Corrupt ar header".into());
        }
        let name = String::from_utf8_lossy(&header[0..16])
            .trim_end()
            .trim_end_matches('/')
            .to_owned();
        let size: u64 = String::from_utf8_lossy(&header[48..58])
            .trim()
            .parse()
            .map_err(|_| "Corrupt ar member size")?;
        members.push(Member {
            name,
            offset: offset + 60,
            size,
        });
        offset += 60 + size + (size & 1);
    }
    Ok(members)
}

/// The installed files (`data.tar.*`), plus the package fields from `control.tar.*`.
pub fn deb(
    path: &Path,
    lister: &mut Lister,
    notes: &mut Vec<(String, String)>,
) -> Result<(), String> {
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let members = ar_members(&mut file)?;
    let member = |prefix: &str| members.iter().find(|m| m.name.starts_with(prefix));

    if let Some(control) = member("control.tar") {
        let codec = Codec::from_name(&control.name).unwrap_or(Codec::None);
        if let Ok(text) = read_control(&mut file, control, codec) {
            *notes = control_fields(&text);
        }
    }

    let data = member("data.tar").ok_or("No data.tar in this package")?;
    let codec = Codec::from_name(&data.name).unwrap_or(Codec::None);
    file.seek(SeekFrom::Start(data.offset))
        .map_err(|e| e.to_string())?;
    let reader = std::io::BufReader::new((&mut file).take(data.size));
    list(lister.guard(codec.decoder(reader)?), lister)
}

fn read_control(file: &mut std::fs::File, member: &Member, codec: Codec) -> Result<String, String> {
    file.seek(SeekFrom::Start(member.offset))
        .map_err(|e| e.to_string())?;
    let reader = codec.decoder(std::io::BufReader::new(file.take(member.size)))?;
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let is_control = entry
            .path()
            .is_ok_and(|p| p.to_string_lossy().trim_start_matches("./") == "control");
        if is_control {
            let mut text = String::new();
            entry
                .take(MAX_CONTROL)
                .read_to_string(&mut text)
                .map_err(|e| e.to_string())?;
            return Ok(text);
        }
    }
    Err("No control file".into())
}

/// The fields worth a glance, in a fixed order; Description is its first line only.
fn control_fields(text: &str) -> Vec<(String, String)> {
    const KEEP: [&str; 6] = [
        "Package",
        "Version",
        "Architecture",
        "Maintainer",
        "Installed-Size",
        "Description",
    ];
    let mut fields = Vec::new();
    for key in KEEP {
        let value = text.lines().find_map(|line| {
            let (k, v) = line.split_once(':')?;
            (k == key).then(|| v.trim().to_owned())
        });
        if let Some(mut value) = value {
            if key == "Installed-Size"
                && let Ok(kib) = value.parse::<u64>()
            {
                value = crate::view::format_size(kib * 1024);
            }
            fields.push((key.replace('-', " "), value));
        }
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn ar(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = b"!<arch>\n".to_vec();
        for (name, data) in members {
            let header = format!(
                "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                format!("{name}/"),
                0,
                0,
                0,
                100644,
                data.len()
            );
            out.extend_from_slice(header.as_bytes());
            out.extend_from_slice(data);
            if data.len() % 2 == 1 {
                out.push(b'\n');
            }
        }
        out
    }

    #[test]
    fn parses_ar_members() {
        let bytes = ar(&[
            ("debian-binary", b"2.0\n"),
            ("control.tar.gz", b"abc"),
            ("data.tar.xz", b"0123"),
        ]);
        let members = ar_members(&mut Cursor::new(bytes)).unwrap();
        let names: Vec<_> = members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["debian-binary", "control.tar.gz", "data.tar.xz"]);
        assert_eq!((members[1].offset, members[1].size), (8 + 60 + 4 + 60, 3));
        // Odd-sized members are padded to an even offset.
        assert_eq!(members[2].offset, members[1].offset + 3 + 1 + 60);
    }

    #[test]
    fn keeps_control_fields() {
        let text = "Package: hello\nVersion: 2.10-3\nArchitecture: amd64\nInstalled-Size: 280\n\
                    Description: example package\n based on GNU hello\n";
        let fields = control_fields(text);
        assert_eq!(fields[0], ("Package".into(), "hello".into()));
        assert_eq!(fields[3], ("Installed Size".into(), "286.7 KB".into()));
        assert_eq!(fields[4].1, "example package");
    }

    #[test]
    fn reads_gzip_size_trailer() {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&[7u8; 12345]).unwrap();
        let bytes = enc.finish().unwrap();
        assert_eq!(gzip_size(&mut Cursor::new(bytes)), Some(12345));
    }

    #[test]
    fn lists_a_tarball() {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(5);
        header.set_mtime(1_700_000_000);
        header.set_cksum();
        builder
            .append_data(&mut header, "pkg/a.txt", &b"hello"[..])
            .unwrap();
        let bytes = builder.into_inner().unwrap();

        let session = crate::session::Session::default();
        let id = session.bump();
        let mut lister = Lister::new(&session, id, true);
        list(Cursor::new(bytes), &mut lister).unwrap();
        assert_eq!(lister.entries.len(), 1);
        assert_eq!(lister.entries[0].path, "pkg/a.txt");
        assert_eq!(lister.entries[0].size, Some(5));
        assert_eq!(lister.entries[0].mtime, Some(1_700_000_000));
    }
}
