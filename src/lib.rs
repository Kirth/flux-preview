//! Shared by both binaries. Must stay std-only: the client links nothing else, so it starts
//! in a few ms instead of paying ~50 ms to load GTK, WebKit and GStreamer.

pub mod cli;
pub mod ipc;
