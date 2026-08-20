//! The device build. `entry!` supplies the allocator, panic handler and the three C symbols
//! the shim calls, exactly as for every other GUI app here.
#![no_std]
#![no_main]

extern crate alloc;

symbian_app::entry!(rfprobe::RfProbe::new());
