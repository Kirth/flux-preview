//! `flux-preview`: the thin client a file manager runs on space.
//!
//! Std-only on purpose (see lib.rs). With a daemon running it sends the request over the
//! socket and exits; otherwise it execs `flux-preview-daemon` with the same arguments, so the
//! cold start still shows the file and this process becomes the daemon.

use flux_preview::cli::{self, Mode};
use flux_preview::ipc::{self, Request};
use std::os::unix::process::CommandExt;

const DAEMON: &str = "flux-preview-daemon";

fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let mode = cli::parse_args(args.clone()).unwrap_or_else(|msg| cli::exit_usage(&msg));

    match &mode {
        Mode::Request(req) => match ipc::send(req) {
            Ok(()) => return,
            Err(_) if matches!(req, Request::Hide | Request::Quit) => return,
            Err(_) => {}
        },
        Mode::Daemon if ipc::is_running() => {
            eprintln!("flux-preview: already running");
            return;
        }
        Mode::Daemon => {}
    }

    // Next to this binary (cargo, installs), else $PATH.
    let daemon = std::env::current_exe()
        .ok()
        .map(|exe| exe.with_file_name(DAEMON))
        .filter(|path| path.exists())
        .map_or_else(|| DAEMON.into(), |path| path.into_os_string());
    let err = std::process::Command::new(&daemon).args(&args).exec();
    eprintln!(
        "flux-preview: cannot start {}: {err}",
        daemon.to_string_lossy()
    );
    std::process::exit(1);
}
