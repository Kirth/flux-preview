//! 7-Zip, from the header alone. From flux `backends/sevenz.rs`.

use super::{Lister, Raw};
use sevenz_rust2::{ArchiveReader, EncoderMethod, Password};
use std::path::Path;

pub fn list(path: &Path, lister: &mut Lister) -> Result<(), String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let reader = ArchiveReader::new(file, Password::empty()).map_err(|e| match e {
        sevenz_rust2::Error::PasswordRequired | sevenz_rust2::Error::MaybeBadPassword(_) => {
            "File names are encrypted".to_owned()
        }
        e => format!("7z: {e}"),
    })?;
    let archive = reader.archive();
    let encrypted_block = |block: Option<usize>| {
        block.and_then(|b| archive.blocks.get(b)).is_some_and(|b| {
            b.coders
                .iter()
                .any(|c| c.encoder_method_id() == EncoderMethod::ID_AES256_SHA256)
        })
    };

    for (i, entry) in archive.files.iter().enumerate() {
        if entry.is_anti_item {
            continue;
        }
        let mtime = entry
            .has_last_modified_date
            .then(|| std::time::SystemTime::from(entry.last_modified_date))
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);
        let raw = Raw {
            path: entry.name().to_owned(),
            is_dir: entry.is_directory,
            size: Some(entry.size),
            mtime,
            encrypted: encrypted_block(
                archive
                    .stream_map
                    .file_block_index
                    .get(i)
                    .copied()
                    .flatten(),
            ),
        };
        if !lister.push(raw) {
            break;
        }
    }
    Ok(())
}
