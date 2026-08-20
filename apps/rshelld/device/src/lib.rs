//! The headless remote-shell agent's device entry point.
//!
//! `daemon_entry!` rather than `entry!`: a bare `CActiveScheduler` with no Avkon, no framebuffer
//! and no font atlases, driving the same `Shell` through [`symbian_app::DaemonApp`].
#![no_std]
#![no_main]

symbian_app::daemon_entry!(rshell::Shell::new());
