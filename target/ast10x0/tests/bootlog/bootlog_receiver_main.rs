// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! Boot log receiver hardware test.
//!
//! Listens for boot status messages (MCTP type 0x5A), records them per device,
//! and logs what each one said. Messages are pushed onto the bus by an external
//! I2C master; see README.md for ready-made frames.

#![no_main]
#![no_std]

use app_bootlog_receiver::handle;
use openprot_bootlog::{LogBuffer, MSG_TYPE};
use openprot_bootlog_receiver::{ReceiveError, Receiver};
use openprot_mctp_api::Stack;
use openprot_mctp_client_ipc::IpcMctpClient;
use userspace::{entry, syscall};

/// Boot logging never needs this - senders address the PRoT physically and
/// carry the null EID - but the stack wants an EID set.
const OWN_EID: u8 = 8;

/// Entries kept per device.
const ENTRIES: usize = 8;

/// Devices tracked at once.
const DEVICES: usize = 4;

/// Block indefinitely; a reporting device sends when it wakes, not on a clock.
const LISTEN_TIMEOUT_MS: u32 = 0;

#[entry]
fn entry() {
    pw_log::info!("boot log receiver starting");

    let stack = Stack::new(IpcMctpClient::new(handle::MCTP));

    if let Err(e) = stack.set_eid(OWN_EID) {
        pw_log::error!("set_eid failed: code={}", e.code as u32);
        syscall::process_exit(1);
    }

    let listener = match stack.listener(MSG_TYPE, LISTEN_TIMEOUT_MS) {
        Ok(listener) => listener,
        Err(e) => {
            pw_log::error!("listener registration failed: code={}", e.code as u32);
            syscall::process_exit(1);
        }
    };

    let mut receiver = Receiver::new(listener, LogBuffer::<ENTRIES, DEVICES>::new());
    pw_log::info!("listening for boot status on MCTP type 0x5A");

    loop {
        match receiver.poll() {
            Ok(status) => {
                pw_log::info!(
                    "vendor=0x{:08x} device=0x{:04x}.{} seq={} t={}us code=0x{:04x} vendor_code=0x{:04x}",
                    status.identity.vendor_id as u32,
                    status.identity.device_id as u32,
                    status.identity.instance as u32,
                    status.sequence as u32,
                    status.timestamp_us as u32,
                    status.code as u32,
                    status.vendor_code as u32
                );
            }
            Err(ReceiveError::Transport(e)) => {
                pw_log::error!("recv failed: code={}", e.code as u32);
            }
            Err(ReceiveError::NotBootStatus { msg_type }) => {
                pw_log::error!("not boot status: type=0x{:02x}", msg_type as u32);
            }
            Err(ReceiveError::Malformed(_)) => {
                pw_log::error!("malformed boot status discarded");
            }
        }
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    pw_log::error!("boot log receiver panic");
    loop {}
}
