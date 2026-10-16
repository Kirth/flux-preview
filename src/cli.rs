use crate::ipc::Request;
use std::path::PathBuf;

pub const USAGE: &str = "\
usage: flux-preview [--index N] FILE...   toggle the preview of FILE[N] (default 0);
                                         left/right step through the other FILEs
       flux-preview --daemon             start the resident previewer hidden
       flux-preview --hide | --quit      control a running previewer";

pub enum Mode {
    Request(Request),
    Daemon,
}

/// Parses `args` (without argv[0]). `Err("")` means: print usage.
pub fn parse_args(args: impl IntoIterator<Item = std::ffi::OsString>) -> Result<Mode, String> {
    let mut index = 0;
    let mut paths = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--daemon") => return Ok(Mode::Daemon),
            Some("--hide") => return Ok(Mode::Request(Request::Hide)),
            Some("--quit") => return Ok(Mode::Request(Request::Quit)),
            Some("-h" | "--help") => return Err(String::new()),
            Some("--index") => {
                index = args
                    .next()
                    .and_then(|v| v.to_str()?.parse().ok())
                    .ok_or("--index needs a number")?;
            }
            _ => {
                // The daemon has its own cwd: resolve here, without touching the disk.
                let path = PathBuf::from(arg);
                paths.push(std::path::absolute(&path).unwrap_or(path));
            }
        }
    }
    if paths.is_empty() {
        return Err(String::new());
    }
    Ok(Mode::Request(Request::Toggle { index, paths }))
}

pub fn exit_usage(msg: &str) -> ! {
    if !msg.is_empty() {
        eprintln!("flux-preview: {msg}");
    }
    eprintln!("{USAGE}");
    std::process::exit(2);
}
