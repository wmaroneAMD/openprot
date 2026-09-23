# eagle-0885 (AST2700) — MCTP over I2C6 to the AST1060

Bring-up runbook for MCTP-over-I2C between the AST1060 RoT and the AST2700 BMC
`eagle-0885.amd.com` (root / `0penBmc`), OpenBMC `sp7_v2.18.0.100B`,
kernel 6.18.20 aarch64, U-Boot 2023.10.

Verified working 2026-09-23: end-to-end SPDM from the BMC requester to the
AST1060 responder over MCTP/I2C6. GET_VERSION, GET_CAPABILITIES,
NEGOTIATE_ALGORITHMS and GET_DIGESTS all complete. GET_CERTIFICATE fails on
*content*, not transport — the AST1060 responder's `MockCertStore` serves
`[0xAA; 32]` (`mock_platform.rs:58`), which is not a cert chain. Real
provisioning is a separate task.

## Persistence at a glance

| Change | Where it lives | Survives reboot? |
|---|---|---|
| `i2c-bus@700` `status = okay` | RAM only — U-Boot `fdt set`, applied by hand at `sp7#` | **no** |
| `mctp-controller` property | RAM only — same | **no** |
| `mctp-i2c-controller` client on i2c-6 | runtime sysfs `new_device` | **no** |
| `mctp` link / addr / route / neigh | runtime, kernel state | **no** |
| `bootdelay = 3` (was 0) | eagle U-Boot env, `saveenv` | **yes** |
| `bootspi` / `bootspi_orig` | eagle U-Boot env | **yes** — currently restored to stock (`run bootspi_orig`) |
| `VARIANT=emu` build support | proto-apps repo (git) | **yes** |
| `emu-*` Makefile targets | proto-apps repo (git) | **yes** |
| `spdm_requester.elf` (emu build) | `/home/root` on eagle | **yes** (old one kept as `.pre-emu`) |

So after any reboot the board comes up with **no I2C6 at all** and everything in
"U-Boot sequence" + "Runtime setup" must be redone. Only the env tweaks, the
repo changes and the deployed binary carry over.

---

## Topology

| | |
|---|---|
| AST1060 pins | GPIOJ4 / GPIOJ5 = SCL9 / SDA9 = SCU418[12:13] |
| AST1060 controller | PAC `I2c8`, base `0x7e7b_0480`, IRQ 118, bus index **8** |
| AST2700 pins | GPIOW4 / GPIOW5 = pins 180/181 = balls F7 / D8 |
| AST2700 controller | I2C6 = `14c0f700.i2c-bus` = **`/dev/i2c-6`** |
| AST2700 DT node | `/soc@14000000/bus@14c0f000/i2c-bus@700` |
| Addresses | BMC EID 32 (0x20) @ `0x10` ; RoT EID 9 @ `0x42` |

Note the AST2700 pinctrl names map 1:1 to Linux bus numbers (`I2C6` →
`/dev/i2c-6`). This is **unlike** the AST2600, where pinctrl `I2C1` was
`/dev/i2c-0`.

---

## Why two DT edits are needed

`i2c-bus@700` ships `status = "disabled"`. It is otherwise complete — it
already has `pinctrl-0`, `pinctrl-names`, `clocks`, `resets`,
`interrupts-extended`, `reg`, `compatible = "aspeed,ast2700-i2c"` — so only the
status needs flipping to get `/dev/i2c-6` and the pinmux.

That alone is **not** enough for MCTP. `mctp-i2c` only creates a netdev if the
*adapter's* DT node carries the `mctp-controller` boolean property:

```c
static bool mctp_i2c_adapter_match(struct i2c_adapter *adap)
{
    if (!adap->dev.of_node)
        return false;
    return of_property_read_bool(adap->dev.of_node, MCTP_I2C_OF_PROP);
}
```

Without it the client binds but no `mctpi2cN` ever appears.

There is no runtime path for either edit: `CONFIG_OF_OVERLAY=y` but
**`CONFIG_OF_CONFIGFS` is not set**, so `/sys/kernel/config/device-tree/overlays`
does not exist. Hence the U-Boot `fdt` approach.

---

## U-Boot sequence (proven)

Break in at `sp7#` (`bootdelay` is now **3**), then:

```
fdt addr ${fdtspiaddr}
fdt header get fitsize totalsize
cp.b ${fdtspiaddr} ${loadaddr} ${fitsize}
bootm start ${loadaddr}${board_conf}
bootm loados
bootm ramdisk
bootm fdt
fdt set /soc@14000000/bus@14c0f000/i2c-bus@700 status okay
fdt set /soc@14000000/bus@14c0f000/i2c-bus@700 mctp-controller
bootm prep
bootm go
```

### Three things that must not change

1. **Edits go AFTER `bootm fdt`, not before.** `bootm fdt` relocates the blob
   to `0x47ad1d000` with ~12 KB of `CONFIG_SYS_FDT_PAD` headroom, and repoints
   the working FDT there automatically (`Working FDT set to 47ad1d000`).
   Editing the in-FIT copy at `0x40356f0d0` beforehand has zero slack.
2. **No `fdt resize`.** Resizing the in-FIT blob grows it over adjacent FIT
   bytes; `bootm fdt` then fails with `ERROR: fdt move failed`, and the
   half-modified blob makes even a fallback boot die at
   `fdt_find_or_add_subnode: chosen: FDT_ERR_BADSTRUCTURE` → reset loop.
   Editing after relocation needs no resize at all.
3. **Skip `bootm cmdline` and `bootm bdt`.** ARM's `do_bootm_linux()` rejects
   both ("No need for those on ARM", returns -1) → `subcommand failed (err=-1)`.

---

## Runtime setup (after boot)

```sh
echo mctp-i2c-controller 0x1010 > /sys/bus/i2c/devices/i2c-6/new_device
mctp link set mctpi2c6 up
mctp addr add 32 dev mctpi2c6
mctp addr del 9 dev mctpi2c6        # see note
mctp route add 9 via mctpi2c6       # must come AFTER the addr del
mctp neigh add 9 dev mctpi2c6 lladdr 0x42
```

`0x1010` = `I2C_CLIENT_SLAVE` (`0x1000`) OR'd with local address `0x10`.

**The `addr del 9` matters.** Something at boot (likely the `MCTP device
configuration` service) claims EID 9 as a *local* address. Left in place,
traffic to EID 9 is delivered up the local stack instead of going out on the
wire, and the requester silently gets nothing. Deleting the local address also
drops its route, so re-add the route afterwards.

### Expected end state

```
link : dev mctpi2c6 index 6 address 0x10 net 1 mtu 254 up
addr : eid 32 net 1 dev mctpi2c6
route: eid min 9 max 9 net 1 dev mctpi2c6 mtu 0
neigh: eid 9 net 1 dev mctpi2c6 lladdr 0x42
```

---

## Requester config

Kernel 6.18 has `CONFIG_MCTP=y` + `CONFIG_MCTP_TRANSPORT_I2C=y` and AF_MCTP
registered, so in the proto-apps Lua config use:

```lua
mctp_interface = "SHIM",
```

`SHIM` routes through the kernel's AF_MCTP socket interface and needs Linux
> 5.18. This is the **opposite** of the AST2600 box (kernel 5.10), which needed
`LIBMCTP` — on 5.10 `SHIM` fails as a silent `-EIO` from
`_SocketOpen(SOCKET_MCTP)`.

Addresses in `cfg_example_malta.lua` must match the kernel MCTP config above:
local `eid = 0x20` / `smbus_address = 0x10`, target `eid = 9` /
`smbus_address = 0x42`.

`smbus_port = 0` in that file is **dead in SHIM mode** — the shim's
`register_endpoint()` ignores `phy`/`prv_binding` and just opens an AF_MCTP
socket; the interface is chosen by the kernel route/neigh tables, not the
config. It would matter (and be wrong — it points at `/dev/i2c-0`) if you ever
switch back to `LIBMCTP`.

---

## Building the requester: VARIANT=emu is mandatory here

**Persistent — these are committed repo changes, not board state.**

The aarch64 build has exactly one platform, `caliptra`, which unconditionally
compiled `irot_shim.c`. On a board with no local Caliptra the DPE path does:

```
libspdm_req_init()
  -> libspdm_req_initialize_cert_chain()
       -> spdm_read_root_cert()            (libspdm_common.c:30)
            -> _GetCertifyKey()            -> dpe_certify_key()
                 -> _IrotSendDpeCommand()  -> caliptra_read/write_u32()
                      -> MapCaliptra() -> open("/dev/uio4") FAILS
```

`MapCaliptra()` returns without setting `g_map_gpio`, so its `if (!g_map_gpio)`
guard never latches and **every** subsequent accessor retries the open. Result:
the console spams

```
Unable to open UIO device
Did you forget "modprobe uio"?
```

forever, and SPDM never starts. This is *not* an AF_MCTP problem — `mctp_init_ex()`
has already succeeded by then (no `spdm_client_init: can't intialize MCTP`).

### Repo changes made

- `platform_shim/aarch64-linux/plat/caliptra/config.cmake` — `PLAT_SUPPORT_IROT`
  now set only when `VARIANT != emu`, mirroring `x64-linux/plat/genoa`.
- `platform_shim/aarch64-linux/plat/caliptra/CMakeLists.txt` — `irot_shim.c`
  guarded by `if (DEFINED PLAT_SUPPORT_IROT)`. `caliptra_interface.c` left
  unconditional so libcaliptra's externs resolve (the linker drops it anyway
  once nothing references it).
- `Makefile` — `VARIANT` plumbed into the cmake line, build dir suffixed
  `-$(VARIANT)`, `emu-requester` / `emu-responder` / `emu-both` and the
  suffixed `emu-requester-%` / `emu-responder-%` forms, `clean` glob widened.

`VARIANT` must be left **undefined**, not `OFF`: the plat CMakeLists tests
`if (DEFINED PLAT_SUPPORT_IROT)` while the common one tests truthiness, so `OFF`
would compile *both* the shim and the stub. `-DVARIANT=emu` is the only reliable
switch. (Same pre-existing inconsistency exists on genoa.)

### Build and deploy

```sh
make emu-requester          # PLAT defaults to aarch64-linux-caliptra-elf
```

Verify **before** copying — expect `1` (stub only, no shim):

```sh
strings ../spdm_requester_aarch64-linux-caliptra-emu/spdm_requester.elf \
  | grep -cE "mailbox_send_dpe_cmd finished|not support for this platform"
```

**The copy trap:** the emu artifact sits in a *different directory* with the
*same filename* as the normal build, so a copy command pointing at the old path
succeeds silently and you keep running the old binary. Check the md5 on both
ends. `scp` needs `-O -T` (dropbear, no sftp-server), or:

```sh
tar cf - -C ../spdm_requester_aarch64-linux-caliptra-emu spdm_requester.elf \
  | ssh root@eagle-0885.amd.com 'tar xf - -C /home/root'
```

A correct emu binary contains **no** `Unable to open UIO device` string at all.

### Latent defects in mctp_shim (not blocking — do not chase these first)

Found while tracing, and confirmed **not** to break this flow, but real:

- `shimmctp_context_init()` (`mctp_shim/mctp.c:119`) uses bare `malloc`, so
  `mctp_t.dst_eid` starts as uninitialised heap garbage.
- `set_destination()` (`:70-83`) binds with `.eid = context->dst_eid` — the
  *old* value — and only assigns `context->dst_eid = eid` afterwards.
- `mctp_socket_bind()` (`socket/mctp_socket.c:69`) sets
  `addr.smctp_addr.s_addr` to the *remote* EID; `bind()` on AF_MCTP sets what
  the socket listens as (local EID / `MCTP_ADDR_ANY`).
- The hand-rolled `struct sockaddr_mctp` (`:29`) omits the kernel's
  `__smctp_pad0`/`__smctp_pad1`. Layout happens to match on aarch64 via natural
  alignment and `= {0}` zeroes it in practice, but `mctp_sockaddr_is_ok()`
  rejects the call unless both pads are zero — so this relies on unspecified
  padding behaviour.

### Expected behaviour with the stub

Once, instead of spinning:

```
_IrotSendDpeCommand: Error not support for this platform
libspdm_read_root_cert: Error in GetCertifyKey 0
```

then `libspdm_req_initialize_cert_chain()` returns early and SPDM proceeds. The
requester therefore has **no local cert chain** — fine for
`req_mode = "ATTESTATOR"` with mutual auth off, but it will fail if anything
negotiates MUT_AUTH.

---

## Board gotchas

- **Warm reset wedges this board.** Observed 3/3, including once with a
  completely stock env. `reboot` from Linux, or a U-Boot self-reset, leaves it
  silent at the BootMCU→ATF transition (last line
  `cptra_ipc: Register IPC ipc1@200 channel 1 callback`). Only a **cold power
  cycle** recovers it. Budget one power cycle per iteration.
- **`bootdelay` is set to 3** (was 0) — **persistent**, written to the U-Boot env
  with `saveenv`. Breaking into U-Boot no longer needs Ctrl-C spam; a single
  keypress during the 3 s window is enough. If you ever need the old blind
  method (e.g. after an env reset), spam `\003` at ~50 Hz while watching for the
  `Hit any key` banner.
- **Stale USB node.** After a power cycle `/dev/ttyUSB0` on rot-ser lingers as a
  dead node — its existence is not proof the device is back. Probe with
  `stty -F /dev/ttyUSB0 115200 raw -echo` and only proceed when that succeeds.
- Console is `/dev/ttyUSB0` on **rot-ser.amd.com**, 115200. Only one reader at a
  time — a running `picocom` will starve any capture.
- Pre-existing, harmless boot noise: `Caliptra is unavailable`,
  `SFDP magic 00000000 invalid`, `Failed to get image info for ID 13`,
  `EEPROM i2c error in misc_init_r`, `HPM slave probe failed`.

---

## Persisting the DT edits

Not done — `bootspi` is currently stock (`run bootspi_orig`), so the DT edits
must be re-applied by hand at `sp7#` after every power cycle.

To persist, put the sequence above into `bootspi_patch` and set
`bootspi = run bootspi_patch`. Do **not** reuse the earlier
`run bootspi_patch; run bootspi_orig` fallback pattern as written: a failed
`fdt set` is not side-effect-free, and the corrupted in-RAM blob poisons the
retry. If a fallback is wanted, it must re-run `cp.b` to restore a clean FIT
first.

Original env is preserved in `bootspi_orig`, and a full backup is at
`/tmp/env-backup-eagle-0885.txt` on the workstation.

### Recovery if a boot change goes wrong

At `sp7#`:

```
setenv bootspi run bootspi_orig
saveenv
boot
```

Backstop is the Dediprog SF100; only **mtd2 (u-boot-env, 128 KB)** would need
rewriting — kernel, rootfs and U-Boot itself are never touched by any of this.

---

## AST1060 side

Already done and unchanged from the AST2600 work: `spdm_auth_peer_image`
drives controller 8 (SCL9/SDA9 on GPIOJ4/J5), responder slave `0x42`, EID 9,
IRQ 118 via the separate `:target_peer` kernel. MCTP sender fragments at the
DSP0236 baseline MTU of 64.
