// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! # OpenPRoT Boot Logging
//!
//! Boot status reporting for devices that are still coming up. The wire format
//! and code assignments are specified in the OpenPRoT specification; this crate
//! provides the sender and receiver halves of it.
//!
//! ```text
//! ┌──────────────────┐   BootStatus    ┌──────────────────┐
//! │  Sender<T>       │ ──────────────> │  BootLogSink     │
//! │  (reporting      │   over MCTP     │  (LogBuffer, or  │
//! │   device)        │                 │   persistent)    │
//! └──────────────────┘                 └──────────────────┘
//! ```
//!
//! Boot logging is unauthenticated. Contents are diagnostic only and must not
//! influence attestation, measurement, or policy decisions.

#![no_std]
#![warn(missing_docs)]

pub mod wire;

pub use wire::{
    BootStatus, DecodeError, DeviceIdentity, ERROR_BIT, FORMAT_REV, MSG_LEN, MSG_TYPE, code,
};

/// Carries an encoded boot status message to the receiver.
///
/// Implemented over MCTP once a stack is available, and directly over the bus
/// by a pre-kernel sender that has nothing else running.
pub trait BootLogTransport {
    /// Transport-specific failure.
    type Error;

    /// Sends one encoded message. Senders do not wait for a response.
    fn send(&mut self, msg: &[u8; MSG_LEN]) -> Result<(), Self::Error>;
}

/// Reports boot status on behalf of one device.
///
/// Owns the identity and the sequence counter so callers report an event as a
/// pair of codes and a timestamp.
pub struct Sender<T> {
    transport: T,
    identity: DeviceIdentity,
    sequence: u8,
}

impl<T: BootLogTransport> Sender<T> {
    /// Creates a sender for `identity`, with the sequence counter at 0.
    pub const fn new(transport: T, identity: DeviceIdentity) -> Self {
        Self {
            transport,
            identity,
            sequence: 0,
        }
    }

    /// Reports one boot event.
    ///
    /// `timestamp_us` is microseconds since this device's reset, taken when the
    /// event occurred rather than when it is sent. The sequence number advances
    /// whether or not the transport accepts the message, so a receiver sees a
    /// gap rather than a silent loss.
    pub fn report(
        &mut self,
        timestamp_us: u32,
        code: u16,
        vendor_code: u16,
    ) -> Result<(), T::Error> {
        let status = BootStatus {
            identity: self.identity,
            sequence: self.sequence,
            timestamp_us,
            code,
            vendor_code,
        };
        self.sequence = self.sequence.wrapping_add(1);
        self.transport.send(&status.encode())
    }

    /// Returns the sequence number the next report will carry.
    pub const fn sequence(&self) -> u8 {
        self.sequence
    }
}

/// Receives decoded boot status messages.
///
/// The PRoT decodes with [`BootStatus::decode`] and hands the result to a sink;
/// which sink is used decides retention (RAM only, or persistent storage).
pub trait BootLogSink {
    /// Records one message. Sinks accept codes they do not recognize.
    fn record(&mut self, status: BootStatus);
}

/// One device's boot status entries.
///
/// Two slots are pinned and never overwritten: entry 0 is the first message the
/// device sent, entry 1 is its first error. The rest is a ring of the most
/// recent entries, oldest overwritten first. Those two are what a post-mortem
/// reader needs — where boot started and where it first went wrong — and they
/// are exactly the entries a plain ring drops first.
///
/// If the first message is itself an error, it occupies entry 0 alone and no
/// second slot is pinned.
///
/// `N` must be at least 2 for the pinned slots; the recent history holds
/// `N - 2`, so pick 4 or more to have any.
#[derive(Clone, Copy)]
pub struct DeviceLog<const N: usize> {
    identity: DeviceIdentity,
    claimed: bool,
    entries: [BootStatus; N],
    /// Offset of the oldest recent entry within the ring region.
    start: usize,
    /// Entries held in the ring region, excluding pinned slots.
    recent: usize,
    len: usize,
    overwritten: u32,
    /// An error has been seen, whether pinned at entry 1 or sent first.
    seen_error: bool,
    /// Entry 1 holds the first error.
    error_pinned: bool,
}

impl<const N: usize> DeviceLog<N> {
    /// Slots held back from the ring for the first message and first error.
    ///
    /// ponytail: fixed at 2. If a platform wants to preserve the first X errors
    /// instead, make it a third const generic before making it a runtime field -
    /// a runtime count costs per-device state and turns the ring arithmetic
    /// variable, for a value that is a platform build choice either way.
    const PINNED: usize = 2;

    /// Rejects a capacity too small to hold the pinned slots, at build time.
    const ASSERT_CAPACITY: () = assert!(N >= Self::PINNED, "DeviceLog needs N >= 2");

    const EMPTY: Self = Self {
        identity: DeviceIdentity {
            vendor_id: 0,
            device_id: 0,
            instance: 0,
        },
        claimed: false,
        entries: [BootStatus {
            identity: DeviceIdentity {
                vendor_id: 0,
                device_id: 0,
                instance: 0,
            },
            sequence: 0,
            timestamp_us: 0,
            code: code::RESERVED,
            vendor_code: 0,
        }; N],
        start: 0,
        recent: 0,
        len: 0,
        overwritten: 0,
        seen_error: false,
        error_pinned: false,
    };

    /// The device these entries came from.
    pub const fn identity(&self) -> DeviceIdentity {
        self.identity
    }

    /// Number of entries held.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if no entries are held.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Count of entries dropped to make room, since reset.
    ///
    /// Pinned entries are never counted here; they are never dropped.
    pub const fn overwritten(&self) -> u32 {
        self.overwritten
    }

    /// The first message this device sent, if any.
    pub const fn first(&self) -> Option<&BootStatus> {
        if self.len == 0 {
            None
        } else {
            Some(&self.entries[0])
        }
    }

    /// The first error this device reported, if any.
    pub const fn first_error(&self) -> Option<&BootStatus> {
        if self.error_pinned {
            Some(&self.entries[1])
        } else if self.seen_error {
            Some(&self.entries[0])
        } else {
            None
        }
    }

    /// Iterates entries: first message, first error, then recent history.
    pub fn iter(&self) -> impl Iterator<Item = &BootStatus> {
        let pinned = match (self.len, self.error_pinned) {
            (0, _) => 0,
            (_, true) => 2,
            (_, false) => 1,
        };
        let ring = N - Self::PINNED;
        self.entries[..pinned].iter().chain(
            (0..self.recent).map(move |i| &self.entries[Self::PINNED + (self.start + i) % ring]),
        )
    }

    fn push(&mut self, status: BootStatus) {
        let () = Self::ASSERT_CAPACITY;

        if self.len == 0 {
            self.entries[0] = status;
            self.seen_error = status.is_error();
            self.len = 1;
            return;
        }

        if status.is_error() && !self.seen_error {
            self.entries[1] = status;
            self.seen_error = true;
            self.error_pinned = true;
            self.len += 1;
            return;
        }

        let ring = N - Self::PINNED;
        if ring == 0 {
            self.overwritten = self.overwritten.saturating_add(1);
            return;
        }

        self.entries[Self::PINNED + (self.start + self.recent) % ring] = status;
        if self.recent == ring {
            self.start = (self.start + 1) % ring;
            self.overwritten = self.overwritten.saturating_add(1);
        } else {
            self.recent += 1;
            self.len += 1;
        }
    }
}

/// Fixed-size in-RAM log holding `N` entries for each of up to `D` devices.
///
/// A device claims a slot with its first message, so one noisy device cannot
/// push another device's entries out. Messages arriving after every slot is
/// claimed are dropped and counted.
///
/// ponytail: RAM ring only; back it with `services/storage` if boot logs need
/// to survive a PRoT reset.
pub struct LogBuffer<const N: usize, const D: usize> {
    devices: [DeviceLog<N>; D],
    dropped: u32,
}

impl<const N: usize, const D: usize> LogBuffer<N, D> {
    /// Creates an empty log with no device slots claimed.
    pub const fn new() -> Self {
        Self {
            devices: [DeviceLog::EMPTY; D],
            dropped: 0,
        }
    }

    /// Number of device slots claimed.
    pub fn device_count(&self) -> usize {
        self.devices.iter().filter(|d| d.claimed).count()
    }

    /// Iterates the logs of devices that have reported, in claim order.
    pub fn devices(&self) -> impl Iterator<Item = &DeviceLog<N>> {
        self.devices.iter().filter(|d| d.claimed)
    }

    /// Returns one device's log, if that device has reported.
    pub fn device(&self, identity: &DeviceIdentity) -> Option<&DeviceLog<N>> {
        self.devices
            .iter()
            .find(|d| d.claimed && d.identity == *identity)
    }

    /// Count of messages dropped because every device slot was claimed.
    pub const fn dropped(&self) -> u32 {
        self.dropped
    }
}

impl<const N: usize, const D: usize> Default for LogBuffer<N, D> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize, const D: usize> BootLogSink for LogBuffer<N, D> {
    fn record(&mut self, status: BootStatus) {
        let slot = self
            .devices
            .iter()
            .position(|d| d.claimed && d.identity == status.identity)
            .or_else(|| self.devices.iter().position(|d| !d.claimed));

        match slot {
            Some(i) => {
                let device = &mut self.devices[i];
                device.claimed = true;
                device.identity = status.identity;
                device.push(status);
            }
            None => self.dropped = self.dropped.saturating_add(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Captured {
        msgs: [[u8; MSG_LEN]; 4],
        count: usize,
    }

    impl BootLogTransport for Captured {
        type Error = ();

        fn send(&mut self, msg: &[u8; MSG_LEN]) -> Result<(), Self::Error> {
            self.msgs[self.count] = *msg;
            self.count += 1;
            Ok(())
        }
    }

    fn identity() -> DeviceIdentity {
        DeviceIdentity {
            vendor_id: 0x0000_1234,
            device_id: 0x0001,
            instance: 0,
        }
    }

    #[test]
    fn sender_numbers_messages_in_order() {
        let mut sender = Sender::new(
            Captured {
                msgs: [[0; MSG_LEN]; 4],
                count: 0,
            },
            identity(),
        );
        sender.report(10, code::ROM_ENTERED, 0x01).unwrap();
        sender.report(20, code::MEMORY_OK, 0x02).unwrap();
        assert_eq!(sender.sequence(), 2);

        let first = BootStatus::decode(&sender.transport.msgs[0]).unwrap();
        let second = BootStatus::decode(&sender.transport.msgs[1]).unwrap();
        assert_eq!(first.sequence, 0);
        assert_eq!(first.code, code::ROM_ENTERED);
        assert_eq!(second.sequence, 1);
        assert_eq!(second.timestamp_us, 20);
    }

    fn status(identity: DeviceIdentity, sequence: u8, code: u16) -> BootStatus {
        BootStatus {
            identity,
            sequence,
            timestamp_us: sequence as u32,
            code,
            vendor_code: 0,
        }
    }

    fn other_identity() -> DeviceIdentity {
        DeviceIdentity {
            device_id: 0x0002,
            ..identity()
        }
    }

    fn codes<const N: usize>(device: &DeviceLog<N>) -> [u16; 3] {
        let mut out = [code::RESERVED; 3];
        for (slot, status) in out.iter_mut().zip(device.iter()) {
            *slot = status.code;
        }
        out
    }

    #[test]
    fn log_buffer_keeps_first_and_newest_entries() {
        let mut log = LogBuffer::<4, 1>::new();
        assert_eq!(log.device_count(), 0);

        for (i, code) in [
            code::ROM_ENTERED,
            code::MEMORY_OK,
            code::IMAGE_OK,
            code::TRANSPORT_OK,
            code::BOOT_COMPLETE,
        ]
        .into_iter()
        .enumerate()
        {
            log.record(status(identity(), i as u8, code));
        }

        assert_eq!(log.device_count(), 1);
        let device = log.device(&identity()).unwrap();
        // Two entries rolled off the ring; the first message did not.
        assert_eq!(device.len(), 3);
        assert_eq!(device.overwritten(), 2);
        assert_eq!(
            codes(device),
            [code::ROM_ENTERED, code::TRANSPORT_OK, code::BOOT_COMPLETE]
        );
    }

    #[test]
    fn first_message_and_first_error_survive_wrapping() {
        let mut log = LogBuffer::<4, 1>::new();
        for (i, code) in [
            code::ROM_ENTERED,
            code::MEMORY_OK,
            code::MEMORY_FAILED,
            code::IMAGE_FAILED,
            code::BOOT_FAILED,
            code::VENDOR_FAILED,
        ]
        .into_iter()
        .enumerate()
        {
            log.record(status(identity(), i as u8, code));
        }

        let device = log.device(&identity()).unwrap();
        assert_eq!(device.first().unwrap().code, code::ROM_ENTERED);
        assert_eq!(device.first_error().unwrap().code, code::MEMORY_FAILED);
        // [0] and [1] are pinned; the ring holds the two newest.
        assert_eq!(device.len(), 4);
        assert_eq!(
            codes(device),
            [code::ROM_ENTERED, code::MEMORY_FAILED, code::BOOT_FAILED]
        );
        assert_eq!(device.iter().nth(3).unwrap().code, code::VENDOR_FAILED);
    }

    #[test]
    fn first_message_that_is_an_error_is_pinned_once() {
        let mut log = LogBuffer::<4, 1>::new();
        log.record(status(identity(), 0, code::BOOT_FAILED));

        let device = log.device(&identity()).unwrap();
        assert_eq!(device.len(), 1);
        assert_eq!(device.first().unwrap().code, code::BOOT_FAILED);
        assert_eq!(device.first_error().unwrap().code, code::BOOT_FAILED);
        assert_eq!(device.iter().count(), 1);
    }

    #[test]
    fn devices_do_not_share_entries() {
        let mut log = LogBuffer::<4, 2>::new();
        log.record(status(identity(), 0, code::ROM_ENTERED));
        log.record(status(other_identity(), 0, code::MEMORY_OK));
        log.record(status(other_identity(), 1, code::IMAGE_OK));

        assert_eq!(log.device_count(), 2);
        assert_eq!(log.device(&identity()).unwrap().len(), 1);
        assert_eq!(log.device(&other_identity()).unwrap().len(), 2);
        // A busy device does not evict the quiet one.
        assert_eq!(
            log.device(&identity()).unwrap().iter().next().unwrap().code,
            code::ROM_ENTERED
        );
        assert_eq!(log.dropped(), 0);
    }

    #[test]
    fn devices_beyond_capacity_are_dropped() {
        let mut log = LogBuffer::<4, 1>::new();
        log.record(status(identity(), 0, code::ROM_ENTERED));
        log.record(status(other_identity(), 0, code::MEMORY_OK));

        assert_eq!(log.device_count(), 1);
        assert_eq!(log.dropped(), 1);
        assert!(log.device(&other_identity()).is_none());
    }
}
