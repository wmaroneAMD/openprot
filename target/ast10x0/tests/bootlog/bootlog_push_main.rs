// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! Boot log dummy sender.
//!
//! Pushes the sample boot status messages from README.md onto I2C bus 2, for a
//! second board running the boot log receiver. Stands in for a real reporting
//! device until the OpenPRoT sender exists.
//!
//! Messages are addressed physically with the null EID, as a device in early
//! boot would send them, and use fixed timestamps so a run is reproducible.

#![no_main]
#![no_std]

use app_bootlog_push::handle;
use embedded_hal::i2c::I2c;
use i2c_client::I2cClient;
use i2c_client_ipc::IpcTransport;
use mctp_lib::i2c::MctpI2cEncap;
use openprot_bootlog::{BootLogTransport, DeviceIdentity, MSG_LEN, Sender, code};
use userspace::time::{Clock, Duration, SystemClock};
use userspace::{entry, syscall};

/// This board's I2C address, reported as the message source.
const OWN_I2C_ADDR: u8 = 0x42;

/// The receiving PRoT's I2C address.
const PROT_I2C_ADDR: u8 = 0x10;

/// MCTP transport header for a single-packet, physically addressed message:
/// header version 1, null destination and source EIDs, SOM | EOM | TO.
const MCTP_HEADER: [u8; 4] = [0x01, 0x00, 0x00, 0xC8];

/// Gap between messages, long enough to read the receiver's log.
const MSG_GAP: Duration = Duration::from_millis(500);

/// Gap between rounds.
const ROUND_GAP: Duration = Duration::from_millis(5000);

/// Sends boot status messages as MCTP-over-SMBus frames.
struct I2cPush<C: I2c<u8>> {
    i2c: C,
    encap: MctpI2cEncap,
}

impl<C: I2c<u8>> I2cPush<C> {
    fn new(i2c: C) -> Self {
        Self {
            i2c,
            encap: MctpI2cEncap::new(OWN_I2C_ADDR),
        }
    }
}

impl<C: I2c<u8>> BootLogTransport for I2cPush<C> {
    type Error = ();

    fn send(&mut self, msg: &[u8; MSG_LEN]) -> Result<(), Self::Error> {
        let mut pkt = [0u8; MCTP_HEADER.len() + MSG_LEN];
        pkt[..MCTP_HEADER.len()].copy_from_slice(&MCTP_HEADER);
        pkt[MCTP_HEADER.len()..].copy_from_slice(msg);

        let mut frame = [0u8; MCTP_HEADER.len() + MSG_LEN + 8];
        let frame = self
            .encap
            .encode(PROT_I2C_ADDR, &pkt, &mut frame, true)
            .map_err(|_| ())?;

        // encode() emits the full frame starting at the destination address;
        // embedded-hal adds that byte itself, so skip it here.
        self.i2c.write(PROT_I2C_ADDR, &frame[1..]).map_err(|_| ())
    }
}

/// One message to push: microseconds since the sender's reset, OpenPRoT boot
/// code, vendor boot code.
type Message = (u32, u16, u16);

const DEVICE_A: [Message; 3] = [
    (1_000, code::ROM_ENTERED, 0x0011),
    (5_000, code::MEMORY_OK, 0x0012),
    (9_000, code::IMAGE_FAILED, 0x00F1),
];

const DEVICE_B: [Message; 1] = [(2_000, code::BOOT_COMPLETE, 0x0099)];

fn identity(device_id: u16, instance: u8) -> DeviceIdentity {
    DeviceIdentity {
        vendor_id: 0x0000_1234,
        device_id,
        instance,
    }
}

fn sleep(duration: Duration) {
    let _ = userspace::time::sleep_until(SystemClock::now() + duration);
}

/// Pushes one device's messages, with a fresh sequence counter each round.
fn push(identity: DeviceIdentity, messages: &[Message]) {
    let transport = I2cPush::new(I2cClient::new(IpcTransport::new(handle::I2C)));
    let mut sender = Sender::new(transport, identity);

    for &(timestamp_us, code, vendor_code) in messages {
        match sender.report(timestamp_us, code, vendor_code) {
            Ok(()) => pw_log::info!(
                "pushed device=0x{:04x}.{} code=0x{:04x}",
                identity.device_id as u32,
                identity.instance as u32,
                code as u32
            ),
            Err(()) => pw_log::error!(
                "push failed device=0x{:04x}.{} code=0x{:04x}",
                identity.device_id as u32,
                identity.instance as u32,
                code as u32
            ),
        }
        sleep(MSG_GAP);
    }
}

#[entry]
fn entry() {
    pw_log::info!("boot log dummy sender starting");

    // Let the receiving board finish booting and register its listener.
    sleep(ROUND_GAP);

    let mut round: u32 = 0;
    loop {
        push(identity(0x0001, 0), &DEVICE_A);
        push(identity(0x0002, 1), &DEVICE_B);

        round += 1;
        pw_log::info!("push round {} complete", round as u32);
        sleep(ROUND_GAP);
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    pw_log::error!("boot log dummy sender panic");
    syscall::process_exit(1)
}
