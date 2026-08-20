//! The remote-shell agent's device entry point. A GUI app: `entry!` supplies the allocator,
//! panic handler and the C symbols the shim calls, plus the event pump that drives the RFCOMM
//! server's active objects.
#![no_std]
#![no_main]

extern crate alloc;

symbian_app::entry!(rshell::Shell::new());
