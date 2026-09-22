// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! I2C MCTP sender — outbound transport binding.
//!
//! Direct port of Hubris `mctp-server/src/i2c.rs` `I2cSender`.
//! Only the I2C driver API is replaced with `embedded_hal::i2c::I2c`.

use embedded_hal::i2c::I2c;
use mctp::Result;
use mctp_lib::i2c::{MctpI2cEncap, MCTP_I2C_MAXMTU};

/// DSP0236 baseline transmission unit: the payload size every MCTP endpoint
/// must accept without negotiation.
///
/// `MCTP_I2C_MAXMTU` is what the I2C *medium* can carry, not what a peer has
/// agreed to receive — sending at the medium maximum overruns any endpoint
/// sized for the baseline (e.g. libmctp's `MCTP_BTU`, also 64). Until MTU
/// negotiation exists, fragment at the baseline and let callers opt into more
/// via [`I2cSender::with_mtu`].
pub const MCTP_BASELINE_MTU: usize = 64;

/// I2C MCTP sender.
///
/// Implements `mctp_lib::Sender` to fragment and send MCTP packets
/// over I2C using the OpenPRoT I2C client API.
///
/// This is a direct port of the Hubris `I2cSender`. The fragmentation
/// loop, I2C encoding via `MctpI2cEncap`, and error mapping are preserved
/// as-is. Only the I2C write call is changed from `drv_i2c_api::I2cDevice::write`
/// to `embedded_hal::i2c::I2c::write`.
pub struct I2cSender<C: I2c<u8>> {
    i2c: C,
    own_addr: u8,
    // Simple static remote address. Full neighbor table (EID → I2C address mapping)
    // will be implemented later per https://github.com/OpenPRoT/mctp-lib/issues/4.
    // For now, this supports single-peer communication (requester ↔ responder).
    remote_addr: u8,
    /// Payload bytes per fragment. Defaults to [`MCTP_BASELINE_MTU`].
    mtu: usize,
}

impl<C: I2c<u8>> I2cSender<C> {
    /// Create a new I2C sender.
    ///
    /// * `i2c` - I2C client for bus writes
    /// * `own_addr` - Own I2C address (7-bit, used in MCTP-I2C header)
    /// * `remote_addr` - Remote peer's I2C address (7-bit, destination for outbound packets)
    pub fn new(i2c: C, own_addr: u8, remote_addr: u8) -> Self {
        Self {
            i2c,
            own_addr,
            remote_addr,
            mtu: MCTP_BASELINE_MTU,
        }
    }

    /// Override the fragmentation MTU.
    ///
    /// Only safe above [`MCTP_BASELINE_MTU`] when the peer is known to accept
    /// larger packets, since nothing here negotiates it. Clamped to
    /// `MCTP_I2C_MAXMTU` because the send buffers are sized for that.
    #[must_use]
    pub fn with_mtu(mut self, mtu: usize) -> Self {
        self.mtu = mtu.clamp(1, MCTP_I2C_MAXMTU);
        self
    }
}

impl<C: I2c<u8>> mctp_lib::Sender for I2cSender<C> {
    fn send_vectored(
        &mut self,
        mut fragmenter: mctp_lib::fragment::Fragmenter,
        payload: &[&[u8]],
    ) -> Result<mctp::Tag> {
        // Use the configured remote address. In a full implementation, this would
        // look up the destination EID in a neighbor table to find the corresponding
        // I2C address. For now, we support single-peer communication with a static
        // remote address configured at construction time.
        // TODO: Implement full EID → I2C address neighbor table
        //       (see https://github.com/OpenPRoT/mctp-lib/issues/4)
        let addr = self.remote_addr;
        let encoder = MctpI2cEncap::new(self.own_addr);
        let mtu = self.get_mtu();
        pw_log::info!(
            "Starting fragmentation: MTU=0x{:04x}, buffer size=0x{:04x}",
            mtu as u32,
            mctp_lib::serial::MTU_MAX as u32
        );

        loop {
            let mut pkt = [0u8; MCTP_I2C_MAXMTU + 4]; // MTU + MCTP transport header
            pw_log::debug!(
                "Calling fragment_vectored with buffer size 0x{:04x}",
                pkt.len() as u32
            );
            let r = fragmenter.fragment_vectored(payload, &mut pkt);

            match r {
                mctp_lib::fragment::SendOutput::Packet(p) => {
                    pw_log::info!("packet sending to 0x{:02x}...", addr as u32);
                    let mut out = [0; MCTP_I2C_MAXMTU + 8]; // max MTU + I2C header size
                    let packet = encoder.encode(addr, p, &mut out, true)?;
                    pw_log::debug!("Encoded packet length: 0x{:04x}", packet.len() as u32);

                    // Skip the first byte (destination address) since the I2C driver
                    // automatically prepends it. mctp-estack's encode() includes the
                    // full I2C frame [dest][cmd][bc][src][...], but embedded-hal I2C
                    // write() expects [cmd][bc][src][...] and adds [dest] itself.
                    let packet_without_dest = &packet[1..];
                    let packet_len = packet_without_dest.len();
                    if let Err(i2c_err) = self.i2c.write(addr, packet_without_dest) {
                        use embedded_hal::i2c::Error as _;
                        let kind = i2c_err.kind();
                        let kind_code: u8 = match kind {
                            embedded_hal::i2c::ErrorKind::Bus => 0,
                            embedded_hal::i2c::ErrorKind::ArbitrationLoss => 1,
                            embedded_hal::i2c::ErrorKind::NoAcknowledge(src) => match src {
                                embedded_hal::i2c::NoAcknowledgeSource::Address => 2,
                                embedded_hal::i2c::NoAcknowledgeSource::Data => 3,
                                embedded_hal::i2c::NoAcknowledgeSource::Unknown => 4,
                            },
                            embedded_hal::i2c::ErrorKind::Overrun => 5,
                            embedded_hal::i2c::ErrorKind::Other => 6,
                            _ => 0xFF,
                        };
                        pw_log::error!("I2C write failed: kind=0x{:02x}", kind_code as u32);
                        pw_log::error!(
                            "Address: 0x{:02x}, Data len: 0x{:04x}",
                            addr as u32,
                            packet_len as u32
                        );
                        return Err(mctp::Error::TxFailure);
                    }
                    pw_log::info!("packet sent");
                }
                mctp_lib::fragment::SendOutput::Complete { tag, .. } => {
                    pw_log::info!("complete");
                    break Ok(tag);
                }
                mctp_lib::fragment::SendOutput::Error { err, .. } => {
                    let err_code: u8 = match err {
                        mctp::Error::TxFailure => 0,
                        mctp::Error::RxFailure => 1,
                        mctp::Error::TimedOut => 2,
                        mctp::Error::BadArgument => 3,
                        mctp::Error::InvalidInput => 4,
                        mctp::Error::TagUnavailable => 5,
                        mctp::Error::Unreachable => 6,
                        mctp::Error::AddrInUse => 7,
                        mctp::Error::NoSpace => 8,
                        mctp::Error::Unsupported => 9,
                        _ => 0xFF,
                    };
                    pw_log::error!(
                        "fragment_vectored failed with error: 0x{:02x}",
                        err_code as u32
                    );
                    pw_log::error!(
                        "Buffer size: 0x{:04x}, MTU: 0x{:04x}",
                        mctp_lib::serial::MTU_MAX as u32,
                        mtu as u32
                    );
                    break Err(err);
                }
            }
        }
    }

    fn get_mtu(&self) -> usize {
        self.mtu
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::cell::RefCell;
    use std::vec::Vec;

    use mctp::Eid;

    use i2c_api::seam::{ErrorKind, ErrorType, I2c, I2cBusError, Operation, SevenBitAddress};
    use i2c_client::I2cClient;
    use i2c_server::loopback::LoopbackTransport;
    use openprot_mctp_server::Server;

    use super::{I2cSender, MCTP_BASELINE_MTU, MCTP_I2C_MAXMTU};
    use crate::MctpI2cReceiver;

    // A bus that records every write() payload verbatim. Reads are not needed
    // since MCTP-over-I2C is master-write only for outbound packets.
    struct CaptureBus<'a> {
        writes: &'a RefCell<Vec<Vec<u8>>>,
        addr: &'a RefCell<Vec<u8>>,
    }

    #[derive(Debug)]
    struct CaptureErr;
    impl I2cBusError for CaptureErr {
        fn kind(&self) -> ErrorKind {
            ErrorKind::Other
        }
    }
    impl ErrorType for CaptureBus<'_> {
        type Error = CaptureErr;
    }
    impl I2c<SevenBitAddress> for CaptureBus<'_> {
        fn transaction(
            &mut self,
            address: SevenBitAddress,
            operations: &mut [Operation<'_>],
        ) -> Result<(), Self::Error> {
            for op in operations.iter() {
                if let Operation::Write(bytes) = op {
                    self.writes.borrow_mut().push(bytes.to_vec());
                    self.addr.borrow_mut().push(address);
                }
            }
            Ok(())
        }
    }

    // Drive Server::send() to push a message through I2cSender and capture the
    // raw I2C frames, then decode them with MctpI2cReceiver and assert the
    // payload survives the round-trip.
    #[test]
    fn sender_receiver_roundtrip() {
        let writes: RefCell<Vec<Vec<u8>>> = RefCell::new(Vec::new());
        let addrs: RefCell<Vec<u8>> = RefCell::new(Vec::new());

        const OWN_ADDR: u8 = 0x10;
        const REMOTE_ADDR: u8 = 0x42;
        const OWN_EID: u8 = 8;
        const REMOTE_EID: u8 = 48;
        const MSG_TYPE: u8 = 0x05; // SPDM

        let bus = CaptureBus {
            writes: &writes,
            addr: &addrs,
        };
        let transport = LoopbackTransport::new(bus);
        let i2c_client = I2cClient::new(transport);
        let sender = I2cSender::new(i2c_client, OWN_ADDR, REMOTE_ADDR);

        let mut server: Server<I2cSender<I2cClient<LoopbackTransport<CaptureBus<'_>>>>, 16> =
            Server::new(Eid(OWN_EID), 0, sender);

        let payload = b"hello mctp";
        let req_handle = server.req(REMOTE_EID).unwrap();
        server
            .send(Some(req_handle), MSG_TYPE, None, None, false, payload)
            .unwrap();

        // I2cSender skips the first byte (dest addr) before calling i2c.write(),
        // so CaptureBus sees [cmd][byte_count][src_addr][mctp_hdr...][payload][PEC].
        // MctpI2cReceiver::decode() expects the full SMBus frame including the
        // leading dest addr byte. Prepend it before decoding.
        let captured = writes.borrow();
        assert!(!captured.is_empty(), "no I2C writes captured");

        let receiver = MctpI2cReceiver::new(REMOTE_ADDR);

        // Reconstruct the full SMBus frame: [dest_addr_byte] + captured write bytes.
        // The dest addr byte is REMOTE_ADDR << 1 (write bit = 0).
        let mut full_frame = Vec::new();
        full_frame.push(REMOTE_ADDR << 1);
        full_frame.extend_from_slice(&captured[0]);

        let (mctp_pkt, i2c_hdr) = receiver.decode(&full_frame).expect("decode failed");

        // source is already a 7-bit address per MctpI2cHeader docs.
        assert_eq!(i2c_hdr.source, OWN_ADDR, "source address mismatch");

        // The MCTP packet payload starts after the 4-byte MCTP transport header
        // and the 1-byte message type field.
        assert!(mctp_pkt.len() >= 5, "MCTP packet too short");
        let msg_type_byte = mctp_pkt[4];
        assert_eq!(msg_type_byte & 0x7F, MSG_TYPE, "message type mismatch");
        assert_eq!(&mctp_pkt[5..], payload, "payload mismatch");
    }

    // A payload that fits in one fragment should produce exactly one I2C write.
    #[test]
    fn single_fragment_produces_one_write() {
        let writes: RefCell<Vec<Vec<u8>>> = RefCell::new(Vec::new());
        let addrs: RefCell<Vec<u8>> = RefCell::new(Vec::new());

        let bus = CaptureBus {
            writes: &writes,
            addr: &addrs,
        };
        let sender = I2cSender::new(I2cClient::new(LoopbackTransport::new(bus)), 0x10, 0x42);
        let mut server: Server<_, 16> = Server::new(Eid(8), 0, sender);

        let req = server.req(48).unwrap();
        server
            .send(Some(req), 1, None, None, false, b"short")
            .unwrap();

        assert_eq!(
            writes.borrow().len(),
            1,
            "expected exactly one I2C write for a short payload"
        );
    }

    /// Regression: the sender used to fragment at `MCTP_I2C_MAXMTU` (254),
    /// which overruns any peer sized for the DSP0236 baseline (libmctp's
    /// `MCTP_BTU`, 64) and fails on the first large response — GET_CERTIFICATE
    /// in practice, since everything before it fits in 64 bytes.
    #[test]
    fn default_mtu_is_the_dsp0236_baseline() {
        use mctp_lib::Sender as _;
        let writes: RefCell<Vec<Vec<u8>>> = RefCell::new(Vec::new());
        let addrs: RefCell<Vec<u8>> = RefCell::new(Vec::new());
        let bus = CaptureBus {
            writes: &writes,
            addr: &addrs,
        };
        let sender = I2cSender::new(I2cClient::new(LoopbackTransport::new(bus)), 0x10, 0x42);
        assert_eq!(sender.get_mtu(), 64);
        assert_eq!(MCTP_BASELINE_MTU, 64);
    }

    #[test]
    fn with_mtu_overrides_and_clamps_to_medium_max() {
        use mctp_lib::Sender as _;
        let writes: RefCell<Vec<Vec<u8>>> = RefCell::new(Vec::new());
        let addrs: RefCell<Vec<u8>> = RefCell::new(Vec::new());
        let mk = || {
            I2cSender::new(
                I2cClient::new(LoopbackTransport::new(CaptureBus {
                    writes: &writes,
                    addr: &addrs,
                })),
                0x10,
                0x42,
            )
        };
        assert_eq!(mk().with_mtu(128).get_mtu(), 128);
        // Buffers are sized for MCTP_I2C_MAXMTU, so anything above it must clamp
        // rather than overflow them.
        assert_eq!(mk().with_mtu(usize::MAX).get_mtu(), MCTP_I2C_MAXMTU);
        assert_eq!(mk().with_mtu(0).get_mtu(), 1);
    }
}
