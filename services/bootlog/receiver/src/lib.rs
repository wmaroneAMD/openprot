// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! # OpenPRoT Boot Log Receiver
//!
//! The PRoT side of boot logging: listens for boot status messages on MCTP and
//! hands them to a [`BootLogSink`].
//!
//! ```text
//! ┌──────────────┐  I2C   ┌──────────────┐        ┌──────────────┐
//! │  reporting   │ ─────> │ MCTP server  │ ─────> │  Receiver    │
//! │  device      │        │ (type 0x5A)  │        │  + sink      │
//! └──────────────┘        └──────────────┘        └──────────────┘
//! ```
//!
//! Senders are physically addressed and carry the null EID, so nothing about
//! the transport identifies them; identity comes from the message payload.
//!
//! ## Usage
//!
//! ```rust,ignore
//! use openprot_bootlog::{LogBuffer, MSG_TYPE};
//! use openprot_bootlog_receiver::Receiver;
//! use openprot_mctp_api::stack::Stack;
//!
//! let stack = Stack::new(client);
//! // 0 blocks indefinitely; boot status arrives whenever a device wakes up.
//! let listener = stack.listener(MSG_TYPE, 0)?;
//! let mut receiver = Receiver::new(listener, LogBuffer::<8, 4>::new());
//!
//! loop {
//!     receiver.poll()?;
//! }
//! ```

#![no_std]
#![warn(missing_docs)]

use openprot_bootlog::{BootLogSink, BootStatus, DecodeError};
use openprot_mctp_api::{MctpError, MctpListener};

pub use openprot_bootlog::MSG_TYPE;

/// Receive buffer size, one MCTP baseline transmission unit.
///
/// A boot status message is far smaller, but a sender is free to pad to the
/// transmission unit and a receiver has to take whatever arrives.
pub const RX_BUF_LEN: usize = 64;

/// Why a delivery did not become a log entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiveError {
    /// The listener call failed; nothing was delivered.
    Transport(MctpError),
    /// Wrong message type, or the integrity check bit was set. Boot status
    /// carries no integrity check, so a message claiming one is not ours.
    NotBootStatus {
        /// The message type that arrived.
        msg_type: u8,
    },
    /// The payload is not a boot status message this revision understands.
    Malformed(DecodeError),
}

impl From<MctpError> for ReceiveError {
    fn from(err: MctpError) -> Self {
        ReceiveError::Transport(err)
    }
}

/// Listens for boot status messages and records them in a sink.
pub struct Receiver<L, S> {
    listener: L,
    sink: S,
    buf: [u8; RX_BUF_LEN],
    received: u32,
    recorded: u32,
    rejected: u32,
}

impl<L: MctpListener, S: BootLogSink> Receiver<L, S> {
    /// Creates a receiver from a listener already registered for [`MSG_TYPE`]
    /// and the sink that will hold the entries.
    pub fn new(listener: L, sink: S) -> Self {
        Self {
            listener,
            sink,
            buf: [0; RX_BUF_LEN],
            received: 0,
            recorded: 0,
            rejected: 0,
        }
    }

    /// Waits for one inbound message, records it, and returns what it said.
    ///
    /// Blocks or times out according to the listener's timeout. A message that
    /// is not valid boot status is counted and reported, never recorded: the
    /// log holds what devices actually said, not what arrived on the bus.
    pub fn poll(&mut self) -> Result<BootStatus, ReceiveError> {
        let Self {
            listener,
            sink,
            buf,
            received,
            recorded,
            rejected,
        } = self;

        let (meta, payload, _resp) = listener.recv(buf)?;
        *received = received.saturating_add(1);

        if meta.msg_type != MSG_TYPE || meta.msg_ic {
            *rejected = rejected.saturating_add(1);
            return Err(ReceiveError::NotBootStatus {
                msg_type: meta.msg_type,
            });
        }

        let status = match BootStatus::decode(payload) {
            Ok(status) => status,
            Err(err) => {
                *rejected = rejected.saturating_add(1);
                return Err(ReceiveError::Malformed(err));
            }
        };

        sink.record(status);
        *recorded = recorded.saturating_add(1);
        Ok(status)
    }

    /// Count of messages taken off the listener since reset.
    pub fn received(&self) -> u32 {
        self.received
    }

    /// Count of messages decoded and handed to the sink.
    pub fn recorded(&self) -> u32 {
        self.recorded
    }

    /// Count of deliveries rejected as not valid boot status.
    pub fn rejected(&self) -> u32 {
        self.rejected
    }

    /// The sink holding recorded entries.
    pub fn sink(&self) -> &S {
        &self.sink
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openprot_bootlog::{DeviceIdentity, LogBuffer, MSG_LEN, code};
    use openprot_mctp_api::{MctpRespChannel, RecvMetadata, ResponseCode};

    /// Replays a canned message, then reports an empty queue.
    struct FakeListener {
        msg: [u8; MSG_LEN],
        /// Bytes of `msg` to deliver; short values truncate the payload.
        len: usize,
        msg_type: u8,
        msg_ic: bool,
        remaining: usize,
    }

    impl FakeListener {
        fn new(msg: [u8; MSG_LEN]) -> Self {
            Self {
                msg,
                len: MSG_LEN,
                msg_type: MSG_TYPE,
                msg_ic: false,
                remaining: 1,
            }
        }
    }

    struct FakeResp;

    impl MctpRespChannel for FakeResp {
        fn send(&mut self, _buf: &[u8]) -> Result<(), MctpError> {
            Ok(())
        }

        fn remote_eid(&self) -> u8 {
            0
        }
    }

    impl MctpListener for FakeListener {
        type RespChannel<'a>
            = FakeResp
        where
            Self: 'a;

        fn recv<'f>(
            &mut self,
            buf: &'f mut [u8],
        ) -> Result<(RecvMetadata, &'f mut [u8], Self::RespChannel<'_>), MctpError> {
            if self.remaining == 0 {
                return Err(MctpError::from_code(ResponseCode::TimedOut));
            }
            self.remaining -= 1;
            buf[..MSG_LEN].copy_from_slice(&self.msg);
            let meta = RecvMetadata {
                msg_type: self.msg_type,
                msg_ic: self.msg_ic,
                msg_tag: 0,
                // Null EID: every boot status sender looks the same here.
                remote_eid: 0,
                payload_size: self.len,
            };
            Ok((meta, &mut buf[..self.len], FakeResp))
        }
    }

    fn identity() -> DeviceIdentity {
        DeviceIdentity {
            vendor_id: 0x0000_1234,
            device_id: 0x0001,
            instance: 0,
        }
    }

    fn status() -> BootStatus {
        BootStatus {
            identity: identity(),
            sequence: 0,
            timestamp_us: 1_000,
            code: code::ROM_ENTERED,
            vendor_code: 0x01,
        }
    }

    #[test]
    fn records() {
        let mut receiver = Receiver::new(
            FakeListener::new(status().encode()),
            LogBuffer::<4, 2>::new(),
        );

        assert_eq!(receiver.poll().unwrap(), status());
        assert_eq!(receiver.received(), 1);
        assert_eq!(receiver.recorded(), 1);
        assert_eq!(receiver.rejected(), 0);

        let device = receiver.sink().device(&identity()).unwrap();
        assert_eq!(device.len(), 1);
        assert_eq!(device.first().unwrap().code, code::ROM_ENTERED);
    }

    #[test]
    fn empty_queue() {
        let mut listener = FakeListener::new(status().encode());
        listener.remaining = 0;
        let mut receiver = Receiver::new(listener, LogBuffer::<4, 2>::new());

        assert!(matches!(receiver.poll(), Err(ReceiveError::Transport(_))));
        assert_eq!(receiver.received(), 0);
    }

    #[test]
    fn rejects_wrong_type() {
        let mut listener = FakeListener::new(status().encode());
        listener.msg_type = 0x7E;
        let mut receiver = Receiver::new(listener, LogBuffer::<4, 2>::new());

        assert_eq!(
            receiver.poll(),
            Err(ReceiveError::NotBootStatus { msg_type: 0x7E })
        );
        assert_eq!(receiver.received(), 1);
        assert_eq!(receiver.rejected(), 1);
        assert_eq!(receiver.sink().device_count(), 0);
    }

    #[test]
    fn rejects_ic_set() {
        let mut listener = FakeListener::new(status().encode());
        listener.msg_ic = true;
        let mut receiver = Receiver::new(listener, LogBuffer::<4, 2>::new());

        assert!(matches!(
            receiver.poll(),
            Err(ReceiveError::NotBootStatus { .. })
        ));
        assert_eq!(receiver.rejected(), 1);
        assert_eq!(receiver.sink().device_count(), 0);
    }

    #[test]
    fn rejects_truncated() {
        let mut listener = FakeListener::new(status().encode());
        listener.len = 4;
        let mut receiver = Receiver::new(listener, LogBuffer::<4, 2>::new());

        assert_eq!(
            receiver.poll(),
            Err(ReceiveError::Malformed(DecodeError::Truncated))
        );
        assert_eq!(receiver.received(), 1);
        assert_eq!(receiver.rejected(), 1);
        assert_eq!(receiver.sink().device_count(), 0);
    }
}
