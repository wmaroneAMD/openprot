// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! Boot status message wire format
//!
//! Encoding and decoding of the 20-byte boot status message defined in the
//! OpenPRoT specification. The layout is fixed so a sender can hold a prebuilt
//! template and patch in only the sequence number, timestamp, and codes. The
//! first 18 bytes are defined; the last 2 pad the message to a dword boundary,
//! sent as zero and ignored on receive. A later revision that needs more space
//! takes it by bumping [`FORMAT_REV`].
//!
//! ```text
//!                   +0                +1                +2                +3
//!           +-----------------+-----------------+-----------------+-----------------+
//! Byte  1 > |I| Msg Type = 5Ah|  Fmt Rev = 01h  | Sequence Number | Device Instance |
//!           +-----------------+-----------------+-----------------+-----------------+
//! Byte  5 > |                   Vendor ID (IANA Enterprise Number)                  |
//!           +-----------------+-----------------+-----------------+-----------------+
//! Byte  9 > |                        Timestamp (microseconds)                       |
//!           +-----------------+-----------------+-----------------+-----------------+
//! Byte 13 > |             Device ID             |         OpenPRoT Boot Code        |
//!           +-----------------+-----------------+-----------------+-----------------+
//! Byte 17 > |          Vendor Boot Code         |              Reserved             |
//!           +-----------------+-----------------+-----------------+-----------------+
//! ```

/// MCTP message type carrying boot status messages.
///
/// Placeholder pending a DMTF message type assignment (DSP0239).
pub const MSG_TYPE: u8 = 0x5A;

/// Format revision of the message layout in this module.
pub const FORMAT_REV: u8 = 0x01;

/// Encoded length of a boot status message, in bytes.
///
/// Well inside the 64-byte MCTP baseline transmission unit, so a message is
/// always one packet.
pub const MSG_LEN: usize = 20;

/// Length of the defined portion of a message; the rest is reserved padding.
pub const DEFINED_LEN: usize = 18;

/// OpenPRoT boot codes.
///
/// Codes are plain `u16` rather than an enum: a receiver must accept codes it
/// does not recognize, since the code list grows independently of any
/// implementation.
pub mod code {
    /// Reserved; never sent. Guards against a zeroed template.
    pub const RESERVED: u16 = 0x0000;
    /// Executing from ROM, before memory is available.
    pub const ROM_ENTERED: u16 = 0x0001;
    /// Startup memory test passed.
    pub const MEMORY_OK: u16 = 0x0002;
    /// Image verified.
    pub const IMAGE_OK: u16 = 0x0003;
    /// Boot logging transport is online.
    pub const TRANSPORT_OK: u16 = 0x0004;
    /// Executing updatable firmware.
    pub const MUTABLE_STAGE_ENTERED: u16 = 0x0005;
    /// Event has no common meaning; the vendor boot code carries it.
    pub const VENDOR_ENTRY: u16 = 0x1000;
    /// Device has finished booting.
    pub const BOOT_COMPLETE: u16 = 0x7FFF;

    /// Boot failed with no more specific code.
    pub const BOOT_FAILED: u16 = 0x8000;
    /// Startup memory test failed.
    pub const MEMORY_FAILED: u16 = 0x8002;
    /// Image verification failed.
    pub const IMAGE_FAILED: u16 = 0x8003;
    /// Boot logging transport failed to come up.
    pub const TRANSPORT_FAILED: u16 = 0x8004;
    /// Failure has no common meaning; the vendor boot code carries it.
    pub const VENDOR_FAILED: u16 = 0x9000;
}

/// Bit that distinguishes an error code from a stage code.
pub const ERROR_BIT: u16 = 0x8000;

/// Returns `true` if `code` reports a failure rather than a stage reached.
#[inline]
pub const fn is_error(code: u16) -> bool {
    code & ERROR_BIT != 0
}

/// Returns the error counterpart of a stage code.
#[inline]
pub const fn to_error(code: u16) -> u16 {
    code | ERROR_BIT
}

/// Returns the stage counterpart of an error code.
#[inline]
pub const fn to_stage(code: u16) -> u16 {
    code & !ERROR_BIT
}

/// Errors from decoding a boot status message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// Buffer is shorter than [`DEFINED_LEN`].
    Truncated,
    /// First byte is not [`MSG_TYPE`], or the integrity check bit is set.
    NotBootStatus(u8),
    /// Format revision is not one this implementation understands.
    UnsupportedRevision(u8),
}

/// Identity of a boot status sender.
///
/// Carried in every message because an EID may not be assigned this early.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeviceIdentity {
    /// Sender's IANA Private Enterprise Number.
    pub vendor_id: u32,
    /// Vendor-assigned device identifier.
    pub device_id: u16,
    /// Distinguishes multiple instances of the same device on a platform.
    pub instance: u8,
}

/// A decoded boot status message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BootStatus {
    /// Sender identity.
    pub identity: DeviceIdentity,
    /// Sender's message counter, starting at 0 on reset and wrapping at 0xFF.
    pub sequence: u8,
    /// Microseconds since the sender's own reset, when the event occurred.
    pub timestamp_us: u32,
    /// OpenPRoT boot code; see [`code`].
    pub code: u16,
    /// Vendor boot code, in the spirit of a POST code.
    pub vendor_code: u16,
}

impl BootStatus {
    /// Encodes the message, including the leading MCTP message type byte.
    ///
    /// Reserved bytes past [`DEFINED_LEN`] are zeroed.
    pub const fn encode(&self) -> [u8; MSG_LEN] {
        let vid = self.identity.vendor_id.to_be_bytes();
        let ts = self.timestamp_us.to_be_bytes();
        let did = self.identity.device_id.to_be_bytes();
        let code = self.code.to_be_bytes();
        let vcode = self.vendor_code.to_be_bytes();

        let mut buf = [0u8; MSG_LEN];
        buf[0] = MSG_TYPE;
        buf[1] = FORMAT_REV;
        buf[2] = self.sequence;
        buf[3] = self.identity.instance;
        buf[4] = vid[0];
        buf[5] = vid[1];
        buf[6] = vid[2];
        buf[7] = vid[3];
        buf[8] = ts[0];
        buf[9] = ts[1];
        buf[10] = ts[2];
        buf[11] = ts[3];
        buf[12] = did[0];
        buf[13] = did[1];
        buf[14] = code[0];
        buf[15] = code[1];
        buf[16] = vcode[0];
        buf[17] = vcode[1];
        buf
    }

    /// Decodes a message body, starting at the MCTP message type byte.
    ///
    /// Everything past [`DEFINED_LEN`] is reserved and ignored, so a sender that
    /// omits the reserved padding still decodes, as does a future revision that
    /// claims part of it.
    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        if buf.len() < DEFINED_LEN {
            return Err(DecodeError::Truncated);
        }
        if buf[0] != MSG_TYPE {
            return Err(DecodeError::NotBootStatus(buf[0]));
        }
        if buf[1] != FORMAT_REV {
            return Err(DecodeError::UnsupportedRevision(buf[1]));
        }
        Ok(Self {
            identity: DeviceIdentity {
                vendor_id: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
                device_id: u16::from_be_bytes([buf[12], buf[13]]),
                instance: buf[3],
            },
            sequence: buf[2],
            timestamp_us: u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]),
            code: u16::from_be_bytes([buf[14], buf[15]]),
            vendor_code: u16::from_be_bytes([buf[16], buf[17]]),
        })
    }

    /// Returns `true` if this message reports a failure.
    #[inline]
    pub const fn is_error(&self) -> bool {
        is_error(self.code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BootStatus {
        BootStatus {
            identity: DeviceIdentity {
                vendor_id: 0x0000_1234,
                device_id: 0xABCD,
                instance: 2,
            },
            sequence: 7,
            timestamp_us: 1_000_000,
            code: code::MEMORY_OK,
            vendor_code: 0x0042,
        }
    }

    #[test]
    fn roundtrip() {
        let status = sample();
        let encoded = status.encode();
        assert_eq!(encoded.len(), MSG_LEN);
        assert_eq!(encoded[0], MSG_TYPE);
        assert!(encoded[DEFINED_LEN..].iter().all(|&b| b == 0));
        assert_eq!(BootStatus::decode(&encoded), Ok(status));
    }

    #[test]
    fn decode_rejects_bad_input() {
        let encoded = sample().encode();
        assert_eq!(
            BootStatus::decode(&encoded[..DEFINED_LEN - 1]),
            Err(DecodeError::Truncated)
        );
        // Reserved padding may be absent.
        assert_eq!(BootStatus::decode(&encoded[..DEFINED_LEN]), Ok(sample()));

        let mut wrong_type = encoded;
        wrong_type[0] = 0x7E;
        assert_eq!(
            BootStatus::decode(&wrong_type),
            Err(DecodeError::NotBootStatus(0x7E))
        );

        let mut wrong_rev = encoded;
        wrong_rev[1] = 0x02;
        assert_eq!(
            BootStatus::decode(&wrong_rev),
            Err(DecodeError::UnsupportedRevision(0x02))
        );
    }

    #[test]
    fn trailing_bytes_ignored() {
        let mut buf = [0u8; MSG_LEN + 4];
        buf[..MSG_LEN].copy_from_slice(&sample().encode());
        assert_eq!(BootStatus::decode(&buf), Ok(sample()));
    }

    #[test]
    fn error_codes_mirror_stages() {
        assert_eq!(to_error(code::MEMORY_OK), code::MEMORY_FAILED);
        assert_eq!(to_error(code::IMAGE_OK), code::IMAGE_FAILED);
        assert_eq!(to_error(code::TRANSPORT_OK), code::TRANSPORT_FAILED);
        assert_eq!(to_error(code::VENDOR_ENTRY), code::VENDOR_FAILED);
        assert_eq!(to_stage(code::MEMORY_FAILED), code::MEMORY_OK);
        assert!(is_error(code::BOOT_FAILED));
        assert!(!is_error(code::BOOT_COMPLETE));
    }
}
