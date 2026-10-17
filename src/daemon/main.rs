//! `flux-preview-daemon`: the resident previewer. Normally started by the `flux-preview`
//! client (exec on cold start), or directly with `--daemon` for autostart.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod app;
mod hwga;
mod kind;
mod load;
mod places;
mod pool;
mod session;
mod thumbcache;
mod view;

use flux_preview::cli::{self, Mode};
use flux_preview::ipc;

fn main() {
    let initial = match cli::parse_args(std::env::args_os().skip(1)) {
        Ok(Mode::Request(req)) => Some(req),
        Ok(Mode::Daemon) => None,
        Err(msg) => cli::exit_usage(&msg),
    };

    // Wayland app id / WM_CLASS: keep the user-facing name, not the binary's, so window
    // rules (float, centre, size) keep matching.
    glib::set_prgname(Some("flux-preview"));

    let listener = match ipc::bind() {
        Ok(listener) => listener,
        Err(err) => {
            // Lost a race with another cold start; let the winner show it.
            if let Some(req) = &initial
                && ipc::send(req).is_ok()
            {
                return;
            }
            eprintln!(
                "flux-preview: cannot bind {}: {err}",
                ipc::socket_path().display()
            );
            std::process::exit(1);
        }
    };

    app::run(listener, initial);
    let _ = std::fs::remove_file(ipc::socket_path());
}
