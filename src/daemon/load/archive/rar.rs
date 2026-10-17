//! RAR has no pure-Rust reader: `unrar lt`, else `7z l -slt`. From flux `backends/rar.rs`.

use super::tool::{self, ToolError};
use super::{Lister, Raw, key_value_blocks, parse_local_time};
use std::path::Path;
use std::process::Command;

pub fn list(path: &Path, lister: &mut Lister) -> Result<(), String> {
    let mut unrar = Command::new("unrar");
    // lt: technical listing, one `Key: value` block per entry; -p-: never ask for a password.
    unrar.args(["lt", "-p-", "--"]).arg(path);
    match tool::run(unrar, lister) {
        Ok(out) => {
            let entries = parse_unrar(&out.stdout);
            if entries.is_empty() && !out.success {
                return Err(tool_error("unrar", &out.stderr));
            }
            push_all(entries, lister);
            return Ok(());
        }
        Err(ToolError::Failed(err)) => return Err(err),
        Err(ToolError::Missing) => {}
    }
    match sevenz_slt(path, lister) {
        Err(ToolError::Missing) => {
            Err("Listing RAR archives needs unrar (or 7-Zip with RAR support)".into())
        }
        Err(ToolError::Failed(err)) => Err(err),
        Ok(()) => Ok(()),
    }
}

/// `7z l -slt`: also how UDF images are listed.
pub fn sevenz_slt(path: &Path, lister: &mut Lister) -> Result<(), ToolError> {
    let mut cmd = Command::new("7z");
    // No -p: with stdin closed, an encrypted header fails instead of prompting.
    cmd.args(["l", "-slt", "--"]).arg(path);
    let out = tool::run(cmd, lister)?;
    let entries = parse_7z_slt(&out.stdout);
    if entries.is_empty() && !out.success {
        return Err(ToolError::Failed(tool_error("7z", &out.stderr)));
    }
    push_all(entries, lister);
    Ok(())
}

fn push_all(entries: Vec<Raw>, lister: &mut Lister) {
    for raw in entries {
        if !lister.push(raw) {
            break;
        }
    }
}

fn tool_error(tool: &str, stderr: &str) -> String {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("password") || lower.contains("encrypted") {
        "File names are encrypted".into()
    } else {
        let line = stderr.lines().map(str::trim).rfind(|l| !l.is_empty());
        format!("{tool} failed: {}", line.unwrap_or("no output"))
    }
}

fn parse_unrar(stdout: &str) -> Vec<Raw> {
    key_value_blocks(stdout, ':')
        .into_iter()
        .filter_map(|block| {
            let path = block.get("name")?.clone();
            let kind = block.get("type").map(String::as_str).unwrap_or("File");
            Some(Raw {
                path,
                is_dir: kind.eq_ignore_ascii_case("directory"),
                size: block.get("size").and_then(|s| s.parse().ok()),
                mtime: block.get("mtime").and_then(|t| parse_local_time(t)),
                encrypted: block
                    .get("flags")
                    .is_some_and(|f| f.to_ascii_lowercase().contains("encrypted")),
            })
        })
        .collect()
}

/// Entry blocks follow the `----------` line; the block before it describes the archive.
fn parse_7z_slt(stdout: &str) -> Vec<Raw> {
    let Some((_, entries)) = stdout.split_once("\n----------\n") else {
        return Vec::new();
    };
    key_value_blocks(entries, '=')
        .into_iter()
        .filter_map(|block| {
            let path = block.get("path")?.clone();
            let is_dir = block.get("folder").is_some_and(|f| f == "+")
                || block.get("attributes").is_some_and(|a| a.starts_with('D'));
            Some(Raw {
                path,
                is_dir,
                size: block.get("size").and_then(|s| s.parse().ok()),
                mtime: block.get("modified").and_then(|t| parse_local_time(t)),
                encrypted: block.get("encrypted").is_some_and(|e| e == "+"),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_unrar_technical_listing() {
        let out = "\nUNRAR 7.00 freeware      Copyright (c) 1993-2024 Alexander Roshal\n\n\
                   Archive: t.rar\nDetails: RAR 5\n\n\
                   \x20       Name: dir/a.txt\n        Type: File\n        Size: 6\n Packed size: 6\n\
                   \x20      mtime: 2024-05-01 10:00:00,123456789\n       Flags: encrypted\n\n\
                   \x20       Name: dir\n        Type: Directory\n\n";
        let entries = parse_unrar(out);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "dir/a.txt");
        assert_eq!(entries[0].size, Some(6));
        assert!(entries[0].encrypted && entries[0].mtime.is_some());
        assert!(entries[1].is_dir);
    }

    #[test]
    fn parses_7z_slt() {
        let out = "7-Zip 17.06\n\n--\nPath = x.iso\nType = Iso\n\n----------\n\
                   Path = BOOT\nFolder = +\nSize = 0\nModified = 2022-09-17 12:34:56\n\n\
                   Path = BOOT/a.bin\nFolder = -\nSize = 2048\nModified = 2022-09-17 12:34:56.1234567\nEncrypted = -\n";
        let entries = parse_7z_slt(out);
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_dir);
        assert_eq!(entries[1].size, Some(2048));
        assert!(entries[1].mtime.is_some());
    }
}
