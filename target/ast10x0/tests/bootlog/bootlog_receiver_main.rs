// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! Boot log receiver hardware test.
//!
//! Listens for boot status messages (MCTP type 0x5A), records them per device,
//! and logs what each one said. Messages are pushed onto the bus by an external
//! I2C master; see README.md for ready-made frames.
//!
//! Passes once it has recorded a full push round from two distinct devices,
//! dumping the per-device logs before it shuts down.

#![no_main]
#![no_std]

use app_bootlog_receiver::handle;
use openprot_bootlog::{LogBuffer, MSG_TYPE};
use openprot_bootlog_receiver::{ReceiveError, Receiver};
use openprot_mctp_api::{ResponseCode, Stack};
use openprot_mctp_client_ipc::IpcMctpClient;
use userspace::{entry, syscall};

/// Boot logging never needs this - senders address the PRoT physically and
/// carry the null EID - but the stack wants an EID set.
const OWN_EID: u8 = 8;

/// Entries kept per device.
const ENTRIES: usize = 8;

/// Devices tracked at once.
const DEVICES: usize = 4;

/// Poll interval rather than a blocking wait.
///
/// A device sends when it wakes, not on a clock, so blocking forever would fit
/// — but that parks the MCTP server in an untimed wait where only an I2C event
/// can wake it. The working MCTP tests all use a finite timeout.
const LISTEN_TIMEOUT_MS: u32 = 100;

/// Distinct devices that must report before the test passes.
const EXPECTED_DEVICES: usize = 2;

/// Messages that must be decoded and recorded before the test passes.
///
/// One full push round: three from one device, one from another.
const EXPECTED_MESSAGES: u32 = 4;

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
            // Nothing arrived within the poll interval; expected, not a fault.
            Err(ReceiveError::Transport(e)) if e.code == ResponseCode::TimedOut => {}
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

        if receiver.recorded() >= EXPECTED_MESSAGES
            && receiver.sink().device_count() >= EXPECTED_DEVICES
        {
            break;
        }
    }

    // Enough arrived to prove the path end to end: messages decoded, and told
    // apart by payload identity rather than by EID, which is null for all of
    // them. Dump what was kept, then pass.
    pw_log::info!(
        "recorded {} of {} deliveries from {} devices",
        receiver.recorded() as u32,
        receiver.received() as u32,
        receiver.sink().device_count() as u32
    );

    for device in receiver.sink().devices() {
        let id = device.identity();
        pw_log::info!(
            "device vendor=0x{:08x} id=0x{:04x}.{} entries={} overwritten={}",
            id.vendor_id as u32,
            id.device_id as u32,
            id.instance as u32,
            device.len() as u32,
            device.overwritten() as u32
        );
        for entry in device.iter() {
            pw_log::info!(
                "  seq={} t={}us code=0x{:04x} vendor_code=0x{:04x}",
                entry.sequence as u32,
                entry.timestamp_us as u32,
                entry.code as u32,
                entry.vendor_code as u32
            );
        }
    }

    let _ = syscall::debug_shutdown(Ok(()));
    loop {}
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    pw_log::error!("boot log receiver panic");
    loop {}
}
