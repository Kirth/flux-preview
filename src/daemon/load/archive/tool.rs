//! Running an external lister (unrar, 7z) without letting it hang the pool.

use super::Lister;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(20);
const MAX_STDOUT: u64 = 64 * 1024 * 1024;
const MAX_STDERR: u64 = 64 * 1024;

pub enum ToolError {
    /// Not installed: try the next tool.
    Missing,
    Failed(String),
}

pub struct Output {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs `cmd`, killing it on a timeout or once the session moves on. Both pipes are
/// drained on their own threads so a chatty tool can't fill one and stall.
pub fn run(mut cmd: Command, lister: &Lister) -> Result<Output, ToolError> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ToolError::Missing,
            _ => ToolError::Failed(e.to_string()),
        })?;
    let drain = |pipe: Option<Box<dyn Read + Send>>, cap: u64| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(pipe) = pipe {
                let _ = pipe.take(cap).read_to_end(&mut bytes);
            }
            String::from_utf8_lossy(&bytes).into_owned()
        })
    };
    let stdout = drain(child.stdout.take().map(|p| Box::new(p) as _), MAX_STDOUT);
    let stderr = drain(child.stderr.take().map(|p| Box::new(p) as _), MAX_STDERR);

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if lister.alive() && started.elapsed() < TIMEOUT => std::thread::sleep(POLL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ToolError::Failed(if lister.alive() {
                    format!("Listing timed out after {} s", TIMEOUT.as_secs())
                } else {
                    "Cancelled".into()
                }));
            }
            Err(e) => return Err(ToolError::Failed(e.to_string())),
        }
    };
    Ok(Output {
        success: status.success(),
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}
