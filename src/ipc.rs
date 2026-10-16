//! Single-instance plumbing over our own unix socket.
//!
//! Not GApplication D-Bus uniqueness: that costs a synchronous ~200 ms handshake on the
//! main thread at every launch (ZAP §1). The wire format is NUL-separated fields, written
//! by the client, terminated by closing the connection:
//!
//!   toggle \0 <index> \0 <path> \0 <path> ...
//!   hide
//!   quit

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone)]
pub enum Request {
    /// Show `paths[index]`, or hide when it is already the file on screen.
    Toggle {
        index: usize,
        paths: Vec<PathBuf>,
    },
    Hide,
    Quit,
}

pub fn socket_path() -> PathBuf {
    dirs::runtime_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("flux-preview.sock")
}

fn encode(req: &Request) -> Vec<u8> {
    let mut fields: Vec<&[u8]> = Vec::new();
    let index;
    match req {
        Request::Toggle { index: i, paths } => {
            index = i.to_string();
            fields.push(b"toggle");
            fields.push(index.as_bytes());
            fields.extend(paths.iter().map(|p| p.as_os_str().as_bytes()));
        }
        Request::Hide => fields.push(b"hide"),
        Request::Quit => fields.push(b"quit"),
    }
    fields.join(&0u8)
}

fn decode(buf: &[u8]) -> Option<Request> {
    let mut fields = buf.split(|b| *b == 0);
    match fields.next()? {
        b"toggle" => {
            let index = std::str::from_utf8(fields.next()?).ok()?.parse().ok()?;
            let paths: Vec<PathBuf> = fields
                .filter(|f| !f.is_empty())
                .map(|f| PathBuf::from(OsStr::from_bytes(f)))
                .collect();
            (!paths.is_empty()).then_some(Request::Toggle { index, paths })
        }
        b"hide" => Some(Request::Hide),
        b"quit" => Some(Request::Quit),
        _ => None,
    }
}

/// Sends `req` to a running daemon. An error means there is none (or it's wedged).
pub fn send(req: &Request) -> io::Result<()> {
    let mut stream = UnixStream::connect(socket_path())?;
    stream.write_all(&encode(req))
}

pub fn is_running() -> bool {
    UnixStream::connect(socket_path()).is_ok()
}

/// Binds the daemon socket. Only called after connecting failed, so a leftover socket file
/// belongs to a dead daemon and can go.
pub fn bind() -> io::Result<UnixListener> {
    let path = socket_path();
    match UnixListener::bind(&path) {
        Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
            if is_running() {
                return Err(err);
            }
            std::fs::remove_file(&path)?;
            UnixListener::bind(&path)
        }
        other => other,
    }
}

/// Accept loop; runs on its own thread for the life of the daemon.
pub fn serve(listener: UnixListener, on_request: impl Fn(Request)) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        // A client that never closes must not wedge the daemon.
        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
        let mut buf = Vec::new();
        if stream.read_to_end(&mut buf).is_ok()
            && let Some(req) = decode(&buf)
        {
            on_request(req);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let req = Request::Toggle {
            index: 2,
            paths: vec!["/a b".into(), "/c\nd".into(), "/e".into()],
        };
        let Some(Request::Toggle { index, paths }) = decode(&encode(&req)) else {
            panic!("decode failed");
        };
        assert_eq!(index, 2);
        assert_eq!(
            paths,
            vec![PathBuf::from("/a b"), "/c\nd".into(), "/e".into()]
        );
        assert!(matches!(
            decode(&encode(&Request::Quit)),
            Some(Request::Quit)
        ));
        assert!(decode(b"toggle\x000").is_none());
    }
}
