# Boot Logging Service

Sender and receiver halves of the boot status message defined in
[the specification](../../docs/src/specification/services/bootlog.md). The
specification owns the message structure and the policy; this crate is the
implementation.

Boot logging is unauthenticated. Entries are diagnostic only and must not
influence attestation, measurement, or policy decisions.

## Contents

- `wire` - message encode/decode, boot code constants, and the stage/error
  mirror helpers.
- `Sender<T>` over `BootLogTransport` - reports events, owns the identity and
  sequence counter.
- `BootLogSink`, `LogBuffer<N, D>`, and `DeviceLog<N>` - the receiver side:
  `N` entries for each of up to `D` devices, oldest overwritten first.

- `receiver/` - the PRoT-side service crate: owns the MCTP listener, decodes
  deliveries, and feeds a sink. Separate crate so the core stays
  dependency-free for senders that have no MCTP stack.

Not here yet: the pre-kernel sender, and the C bindings for non-Rust senders.

## Receiver

The PRoT listens for MCTP message type `0x5A`, decodes each message with
`BootStatus::decode`, and hands it to a `BootLogSink`. There is no response;
a sender that cannot be heard has no way to learn that.

`receiver::Receiver<L, S>` wraps a registered `MctpListener` and a sink.
`poll()` takes delivery of one message, rejects anything that is not boot
status - wrong message type, integrity check bit set, undecodable payload - and
records the rest, returning the decoded `BootStatus`. Rejections are counted by
`rejected()` and never reach the sink, so the log holds what devices said rather
than what arrived on the bus. Senders carry the null EID, so
`RecvMetadata::remote_eid` is 0 for all of them and identity comes from the
payload.

Retention is the sink's decision. `LogBuffer<N, D>` gives each device its own
`N` entries, so a chatty device cannot push a quiet one out of the log. A device
claims a slot with its first message and holds it until reset; messages from a
device arriving after all `D` slots are claimed are dropped and counted by
`dropped()`.

Within a device, entry 0 is the first message it sent and entry 1 is its first
error. Neither is ever overwritten - they are what a post-mortem reader needs,
and exactly what a plain ring discards first. The remaining `N - 2` entries are
the most recent history, oldest overwritten first and counted by `overwritten()`.
A device whose first message is itself an error pins it once, at entry 0.

### Relationship to PLDM

PLDM monitoring (DSP0248) is the platform telemetry path once devices are up,
and boot logging does not replace it. PLDM needs an assigned EID, discovery,
and a PDR exchange before a single reading can be taken - none of which exist
while a device is still in ROM. Boot logging fills exactly that window, with a
message a device can emit as soon as it can drive the bus.

Once a device is discoverable, its runtime telemetry belongs in PLDM. Its boot
log stays a separate retained record of how it got there.

## Test Coverage

This section documents test target coverage (Bazel targets), not line or branch
coverage percentages.

```bash
bazelisk test //services/bootlog:bootlog_test --test_output=errors
```

Covers wire round-trip, decode rejection (truncated, wrong type, wrong
revision), forward compatibility with trailing bytes, the stage/error mirror,
sender sequence numbering, and ring buffer overwrite accounting.
