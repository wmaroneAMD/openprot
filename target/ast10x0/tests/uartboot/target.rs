// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! OpenPRoT uartboot bootstrap image for the AST1060 — Hello World.
//!
//! First step toward a UART-resident loader for a device that cannot UART boot
//! natively but can be programmed externally: program this once, confirm the
//! boot path, then grow the loader in place.
//!
//! Kernel boots and prints `Hello World` once a second, forever. Console is
//! UART5 (`0x7e78_4000`), left at whatever the ROM configured — see
//! `target/ast10x0/console_backend.rs`.

#![no_std]
#![no_main]

use arch_arm_cortex_m::Arch;
use console_backend::console_backend_write_all;
use kernel::{Arch as _, Duration};
use target_common::{declare_target, TargetInterface};
use {console_backend as _, entry as _};

/// The clock the kernel drives on this target — names the `Duration` below.
type Clock = <Arch as kernel::Arch>::Clock;

/// Busy-wait `ms` milliseconds against the kernel's monotonic clock.
///
/// Spins instead of `kernel::sleep_until` so the loop does not depend on the
/// scheduler: nothing else runs in this image, so there is no one to yield to.
fn busy_wait_ms(ms: u64) {
    let until = Arch.now() + Duration::<Clock>::from_millis(ms);
    while Arch.now() < until {}
}

pub struct Target {}

impl TargetInterface for Target {
    const NAME: &'static str = "OpenPRoT uartboot (AST1060)";

    fn main() -> ! {
        loop {
            let _ = console_backend_write_all(b"Hello World\n");
            busy_wait_ms(1000);
        }
    }
}

declare_target!(Target);
