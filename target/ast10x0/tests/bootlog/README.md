# Boot Log Receiver Hardware Test

Two AST1060 EVBs wired together on I2C bus 2. One runs the boot log receiver;
the other pushes sample boot status messages at it. The pusher stands in for a
real reporting device until the OpenPRoT sender exists.

The receiver decodes each message, records it in a per-device log, and prints
what it said. Success is a matching log line per message pushed.

Messages can also be pushed by any other I2C master - see
[Pushing frames by hand](#pushing-frames-by-hand).

## Images

`bootlog_image` (board A, the PRoT):

| App | Role |
|:----|:-----|
| `i2c_server` | owns I2C bus 2, slave address `0x10` |
| `mctp_server` | I2C transport binding, MCTP channel handler, EID 8 |
| `bootlog_receiver` | listens on MCTP message type `0x5A` |

`bootlog_push_image` (board B, the dummy sender):

| App | Role |
|:----|:-----|
| `i2c_server_push` | owns I2C bus 2, address `0x42` |
| `bootlog_push` | masters the bus and pushes the sample messages |

The pusher reuses `Sender<T>` from `//services/bootlog` over a small
`BootLogTransport` that wraps each message in an MCTP-over-SMBus frame, so the
bytes on the wire are built the same way a real sender would build them. It
loops: four messages 500ms apart, then a 5s gap, restarting each device's
sequence counter every round.

## Running

```bash
bazel test \
    --config=k_ast1060_evb \
    --test_env=AST1060_EVB_PI_HOST=<pi-hostname> \
    --test_output=streamed \
    -- //target/ast10x0/tests/bootlog:bootlog_receiver_test
```

One command flashes both boards. The test target carries `bootlog_image` as its
own binary and `bootlog_push_image` as `slave_image`; the runner picks the
second up automatically from the `.slave.elf` symlink beside the first:

| Board | Image | `evb_config.toml` |
|:------|:------|:------------------|
| A | `bootlog_image` (receiver) | `[uart]`, `/dev/ttyUSB0` |
| B | `bootlog_push_image` (sender) | `[device_b]`, `/dev/ttyUSB1` |

Both UART streams are spliced into the same output, so board A's received lines
and board B's pushed lines interleave.

There is no PASS/FAIL sentinel - the receiver loops forever - so the test runs
until `Ctrl-C` or the `eternal` timeout. Fixture setup (Pi wiring, SSH keys,
lockfile behavior) is identical to the MCTP server test; see
[that README](../mctp/server/README.md).

Expected output from board A once messages start arriving:

```
boot log receiver starting
listening for boot status on MCTP type 0x5A
vendor=0x00001234 device=0x0001.0 seq=0 t=1000us code=0x0001 vendor_code=0x0011
vendor=0x00001234 device=0x0001.0 seq=1 t=5000us code=0x0002 vendor_code=0x0012
vendor=0x00001234 device=0x0001.0 seq=2 t=9000us code=0x8003 vendor_code=0x00f1
vendor=0x00001234 device=0x0002.1 seq=0 t=2000us code=0x7fff vendor_code=0x0099
```

Those four lines are one push round. Board B logs a matching `pushed ...` line
for each. Anything that is not boot status - wrong message type, integrity check
bit set, payload too short - is logged as an error and discarded rather than
recorded.

## Pushing frames by hand

The same messages as raw frames, for pushing from a bench master instead of
board B.

Complete SMBus block writes to slave address `0x10`, including the destination
address byte and the trailing PEC. Source address is `0x42`, both EIDs are null,
and SOM/EOM/TO are set.

```
device A, ROM entered
20 0F 19 85 01 00 00 C8 5A 01 00 00 00 00 12 34 00 00 03 E8 00 01 00 01 00 11 00 00 A8

device A, memory OK
20 0F 19 85 01 00 00 C8 5A 01 01 00 00 00 12 34 00 00 13 88 00 01 00 02 00 12 00 00 1C

device A, image verification failed
20 0F 19 85 01 00 00 C8 5A 01 02 00 00 00 12 34 00 00 23 28 00 01 80 03 00 F1 00 00 17

device B, boot complete
20 0F 19 85 01 00 00 C8 5A 01 00 01 00 00 12 34 00 00 07 D0 00 02 7F FF 00 99 00 00 6C
```

Device A is vendor `0x00001234`, device `0x0001`, instance 0; device B is the
same vendor, device `0x0002`, instance 1. Pushing all four exercises two
senders, a sequence run, and an error code.

### Frame layout

```
20      destination address, 0x10 << 1 | write
0F      SMBus command code, MCTP
19      byte count, 25 = 20 payload + 5
85      source address, 0x42 << 1 | 1
01      MCTP reserved + header version 1
00      destination EID, null
00      source EID, null
C8      SOM | EOM | TO, packet sequence 0, tag 0
5A ...  boot status message, 20 bytes
A8      PEC, CRC-8 over every preceding byte
```

### From a Linux master

Where `$BUS` is the adapter number:

```bash
# device A, ROM entered
i2ctransfer -y $BUS w28@0x10 0x0F 0x19 0x85 0x01 0x00 0x00 0xC8 \
    0x5A 0x01 0x00 0x00 0x00 0x00 0x12 0x34 0x00 0x00 0x03 0xE8 \
    0x00 0x01 0x00 0x01 0x00 0x11 0x00 0x00 0xA8
```

`i2ctransfer` supplies the address byte itself, so the frame above starts at the
command code and the PEC is passed as an ordinary data byte.

### Generating more

The PEC is CRC-8 (polynomial `0x07`, initial value 0) over the whole frame
starting at the destination address byte:

```python
def pec(data):
    crc = 0
    for b in data:
        crc ^= b
        for _ in range(8):
            crc = ((crc << 1) ^ 0x07) & 0xFF if crc & 0x80 else (crc << 1) & 0xFF
    return crc
```

Message field order is in
[the specification](../../../../docs/src/specification/services/bootlog.md);
`BootStatus::encode` in `//services/bootlog` is the same layout in code.
