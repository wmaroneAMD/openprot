# Boot Logging

Status: Draft

## Overview

Boot logging lets a device report its own boot progress to the OpenPRoT while
that device is still coming up. It is the earliest telemetry a platform has:
the sender may have nothing working but its clocks, its I2C controller, and a
few bytes of ROM.

Goals

*   Report boot progress and boot failures before any secure channel exists.
*   Be usable from ROM, with I2C as the only working peripheral.
*   Give OpenPRoT a common view of boot across devices from different vendors.

## Security

Boot logging is **not secure**. Messages are unauthenticated, unencrypted, and
trivially spoofable, because senders emit them before cryptographic services
are available.

Consumers shall treat boot log contents as diagnostic information only. Boot
log contents shall not influence attestation, measurement, policy, or any
access control decision.

## Transport

Messages are carried over MCTP, to disconnect boot logging from the underlying
wire trnasport.

### MCTP Rules

* All boot messages will use message type `0x5A` (see DSP0239) and IC = 0.
* A boot status message shall fit in a single MCTP baseline transmission unit
  (64 bytes), so no message needs reassembly.
* Every message sets both SOM and EOM. A device shall send exactly one boot
  status message per MCTP message.
* Senders do not wait for, and do not receive, a response.
* A sender whose transmit is delayed by bus contention or intenrnal timing 
  should buffer pending boot status messages and send when possible.
* If a buffer should roll over, buffered entries tagged as a vendor boot
  message may be discarded. All others must be preserved and sent when possible.

> Note: `0x5A` is a placeholder pending a DMTF message type assignment.

## Message Format

The message is fixed-layout so a sender can hold a prebuilt template in ROM and
patch in only the sequence number and the two codes before transmission.

Byte 1 below is the first byte of the MCTP message body, immediately following
the MCTP transport header. All multi-byte fields are big endian.

```
                  +0                +1                +2                +3
          +-----------------+-----------------+-----------------+-----------------+
Byte  1 > |I| Msg Type = 5Ah|  Fmt Rev = 01h  | Sequence Number | Device Instance |
          +-----------------+-----------------+-----------------+-----------------+
Byte  5 > |                   Vendor ID (IANA Enterprise Number)                  |
          +-----------------+-----------------+-----------------+-----------------+
Byte  9 > |                        Timestamp (microseconds)                       |
          +-----------------+-----------------+-----------------+-----------------+
Byte 13 > |             Device ID             |         OpenPRoT Boot Code        |
          +-----------------+-----------------+-----------------+-----------------+
Byte 17 > |          Vendor Boot Code         |
          +-----------------+-----------------+
```

`I` is the MCTP integrity check bit (IC), always 0b for boot status messages.

| Offset | Size | Field |
|:-------|:-----|:------|
| 0 | 1 | MCTP message type, `0x5A` |
| 1 | 1 | Format revision, `0x01` |
| 2 | 1 | Sequence number |
| 3 | 1 | Device instance |
| 4 | 4 | Vendor ID (IANA Enterprise Number) |
| 8 | 4 | Timestamp (microseconds) |
| 12 | 2 | Device ID (vendor assigned) |
| 14 | 2 | OpenPRoT boot code |
| 16 | 2 | Vendor boot code |

Total payload is 18 bytes.

*   **Vendor ID**, **Device ID**, and **Device instance** identify the sender.
    They are carried in the payload because an EID may not yet be assigned.
*   **Vendor ID** is the sender's IANA Private Enterprise Number (PEN), the
    4-byte identifier assigned to an organization from the [SMI Network
    Management Private Enterprise Codes
    registry](https://www.iana.org/assignments/enterprise-numbers/). This is the
    same identifier PLDM (DSP0240) calls the IANA Enterprise ID.
*   **Timestamp** is microseconds since the sender's own reset, not a wall clock
    time: senders have no common time base this early. It records when the event
    occurred, which may be well before the message is sent if the sender had to
    buffer it. The 32-bit counter wraps after about 71.6 minutes; a receiver
    should treat a decrease as a wrap, not as reordering.
*   **Sequence number** starts at 0 on each device reset and increments per
    message sent, wrapping at 0xFF. Together with the identity fields it makes
    each message unique, so a receiver can order messages, detect gaps, and
    discard duplicates produced by a buffer flush.

## Boot Codes

Each message carries two codes describing the same event.

*   **Vendor boot code** is device specific, in the spirit of a POST code. Its
    meaning is defined by the vendor.
*   **OpenPRoT boot code** is common across all devices and describes the boot
    stage or error in vendor-neutral terms.

### Ranges

| Range | Meaning |
|:------|:--------|
| `0x0000` - `0x7FFF` | Boot stage reached |
| `0x8000` - `0xFFFF` | Error |

### Stages

Defined codes (TBD, to be expanded):

| Code | Meaning | Description |
|:-----|:--------|:------------|
| `0x0000` | Reserved | Never sent, so a zeroed or partially initialized template is not a valid stage report. |
| `0x0001` | ROM entered | The device is executing from ROM, before memory is available, running entirely out of cache and registers. |
| `0x0002` | Memory OK | The device has done a startup memory test and found its memory to be functional. |
| `0x0003` | Image OK | The device has checked its image and verified it. |
| `0x0004` | Transport OK | Boot logging transport is online and functional. |
| `0x0005` | Mutable stage entered | The device has left ROM and is executing updatable firmware. |
| `0x1000` | Vendor boot log entry | The event has no common meaning; the vendor boot code carries it. |
| `0x7fff` | Boot complete | The device has finished booting and is operational. |

### Errors

An error code is its corresponding stage code with bit 15 set, so a receiver
translates between the two by flipping one bit. Stages `0x0001`, `0x0005` and
`0x7fff` are progress markers with no failure counterpart; a device that never
reaches them reports `0x8000`. A vendor-specific failure is `0x9000`, the
counterpart of `0x1000`.

Which transport failed is reported in the vendor boot code.

Defined codes (TBD, to be expanded):

| Code | Meaning | Description |
|:-----|:--------|:------------|
| `0x8000` | Unspecified boot failure | The device failed to boot and has no more specific code to report. |
| `0x8002` | Memory failed | The startup memory test failed. |
| `0x8003` | Image verification failed | The device could not verify its image. |
| `0x8004` | Unspecified transport failure | The boot logging transport failed to come up; the vendor boot code says which one. |
| `0x9000` | Vendor-specific failure | The failure has no common meaning; the vendor boot code carries it. |
