# OpenPRoT Enablement on AMD BMC Cards

Session record, 2026-09-21 → 2026-09-23.

Bringing an AST1060 RoT running OpenPRoT firmware into SPDM-over-MCTP
conversation with an AMD BMC, first an AST2600 (`10.0.0.226`, kernel 5.10) and
then an AST2700 (`eagle-0885.amd.com`, kernel 6.18).

**Outcome:** working end-to-end SPDM on both platforms — GET_VERSION,
GET_CAPABILITIES, NEGOTIATE_ALGORITHMS and GET_DIGESTS all complete.
GET_CERTIFICATE fails on both, identically, because the AST1060 responder's
`MockCertStore` serves `[0xAA; 32]` instead of a certificate chain. That is a
content/provisioning gap, not a transport one — reaching the same wall from two
different parents, two kernels and two different MCTP stacks is the evidence.

Companion runbook (board-specific, operational):
`/home/wmarone/eagle-0885-mctp-i2c6-setup.md`

---

## 1. SPI flash — wiring the driver to the portable HAL

**Starting question:** what SPI flash support exists in openprot?

Three layers existed, with a gap between them:

| Layer | State |
|---|---|
| `target/ast10x0/peripherals/smc/` | real, hardware-tested: `SpiNorFlash` with read/erase/program, JEDEC, DMA read |
| `hal/blocking/flash/` | `Flash` + `FlashDriver` traits, `BlockingFlash` adapter, unit-tested against a fake |
| `services/storage` | an 8-line empty stub |

Nothing implemented `FlashDriver`, so nothing above the driver could reach real
hardware.

**Changes**

- **`smc/device/hal_impl.rs`** (new) — `SpiNorFlashDriver` implementing
  `FlashDriver`, plus `ImmediateBlocking`. Capacity captured at construction
  because `size()` cannot fail; runtime geometry checked against the
  compile-time constants rather than silently mis-programming.
- **`smc/device/flash.rs`** — `validate_page_program` required a page-*aligned*
  start. SPI NOR only requires the write not *cross* a page boundary, and
  `BlockingFlash::program` splits at window boundaries without aligning starts.
  Extracted as `page_program_fits()`; strictly looser, existing callers
  unaffected.
- `device/mod.rs`, `smc/mod.rs`, `peripherals/BUILD.bazel` — re-exports and deps.
- **`tests/smc/write/target.rs`** — EVB check: geometry, HAL erase, unaligned
  program spanning three page-program windows, read-back.

**Verified:** peripherals crate builds; `smc_write_test` builds;
`no_panics_test` **PASSED** (confirming `BlockingFlash`'s `assert!`s const-fold
away rather than pulling in a panic handler); `flash_test` and `types_test`
**PASSED**.

**Note:** the three new `page_program_fits` unit tests live in `flash.rs`'s
existing `mod tests`, which no bazel target builds — the peripherals crate is
ARM-only and the file cannot compile standalone. Same status as the ~12 tests
already there. The EVB test is the check that actually runs.

---

## 2. uart_test_exec.py — `--no-timeout`

Added a flag to monitor UART indefinitely until Ctrl+C, plus a no-framework
self-check (`test_uart_test_exec.py`) covering: the flag ignores an expired
deadline but still stops on a success pattern; still stops on failure; without
the flag the loop does not run past the deadline; Ctrl+C propagates rather than
being swallowed into a bogus exit 0.

**Bug found and reported, not fixed:** `monitor_test_execution` returns
`test_results["failed"] == 0` on timeout — so a hung or dead board reports
**PASS**. Only an explicit `FAIL`/`panic`/`ERROR`/`abort` on the wire makes it
fail. Inverted exactly where it matters most.

Also noted: the SMC targets emit `TEST_RESULT:PASS`, which matches neither
`SUCCESS_PATTERNS` nor `FAILURE_PATTERNS`, so those runs fall through to the
timeout path and that bug decides them.

---

## 3. AST1060 reset — a dead end, documented

Goal was to reset the SoC over the Black Magic Probe. **No debugger-initiated
reset works on this board.**

| Mechanism | Result |
|---|---|
| `monitor reset` (BMP nRST pulse) | no reset |
| SYSRESETREQ via `AIRCR` | write lands, no reset |
| WDT0 full-chip (`WDT00C=0x23`, RstSysMode=Full chip) | watchdog fires, SoC ignores it |

**Proof method worth reusing:** `SCU074` (`0x7E6E2074`) is a per-source reset
event log. Unchanged across every attempt = no reset occurred. Beats inferring
from UART silence. Arming `DEMCR` bit 0 `VC_CORERESET` is a good second check —
a real reset must then halt at the reset vector.

The only working reset is a power cycle, or Zephyr's `kernel reboot cold` while
Zephyr is still the running image. `SCU510[8]` (boot-from-UART5) is write-locked
via SWD but lives in the RstPwr domain, so a power cycle clears it — which is
why the field sequence is always:

```
power cycle -> Zephyr -> mw 0x7E6E2510 0x100 -> kernel reboot cold -> 'U' -> uart_test_exec.py
```

**gdb gotcha:** with an openprot ELF loaded, gdb infers Rust and rejects C casts
(`No symbol 'unsigned' in current context`). `set language c` first, or the
write silently never happens. This cost one full cycle.

---

## 4. AST2600 — MCTP over SMBus

### Tracing the bus

AST1060 bus 2 → `PINCTRL_I2C2` → `SCU418[0:1]` → GPIOI0/I1 → **J15 on the Test
Harness board**, i.e. wired for daughter-to-daughter, not to a parent.

On the AST2600, `GPIOJ0` = gpiochip0 line 72 = ball **B20**, muxed to **I2C1** =
`/dev/i2c-0`. Confirmed at silicon level: `SCU418[8:9] = 0` and
`SCU4B8[8:9] = 1`, matching the datasheet's two-register condition.

Naming trap, hit repeatedly: **AST2600 pinctrl `I2C1` is Linux `/dev/i2c-0`**,
and the AST1060 SVD's `SCL3/SDA3` is PAC controller **2**. 1-based vs 0-based.

### Getting `/dev/mem`

`CONFIG_DEVMEM=y` but `CONFIG_DEVMEM_BOOTPARAM=y`, so the node only appears with
a boot parameter. First attempt used `devmem=1`, which the kernel did **not**
consume — it fell through to init as an environment variable. The real parameter
is namespaced to the built-in module: **`mem.devmem=1`**
(`/sys/module/mem/parameters/devmem`). `CONFIG_STRICT_DEVMEM=y` does not block
MMIO on ARM, and `CONFIG_IO_STRICT_DEVMEM` is off.

### Switching the AST1060 to a parent-facing bus

AST1060 moved from controller 2 to **controller 8** — SCL9/SDA9 =
`SCU418[12:13]` = GPIOJ4/J5, base `0x7e7b_0480`, IRQ **118**.

- new `PINCTRL_I2C8` in `scu/pinctrl.rs`
- `auth/i2c_server_main_peer.rs` → `open_bus_dma(8, …)`, `I2C8_IRQ`,
  `signals::I2C8`
- `auth/peer_system.json5` → irq object `i2c8_irq`, number **118**
- `auth/target.rs` → shared kernel brings up buses 2 *and* 8

**Then a latent bug surfaced.** Both system images used `kernel = ":target"`,
whose codegen comes from `system.json5` — the *requester's* config. The peer
image therefore booted a kernel that enabled **IRQ 112**, not 118. Visible in
the UART log as the peer image announcing `spdm_requester_process`.

Fix: a separate peer kernel — `codegen_peer` + `linker_script_peer` +
`target_peer`, with `crate_name = "codegen"` so the shared `target.rs` still
resolves `codegen::start()`. Verified the two kernels differ: 112/113 vs
**118/119**, requester vs responder process names.

This was a pre-existing landmine, not something the bus change created — the two
configs had been identical apart from names, so the shared kernel happened to be
correct. `vca` and `tests/mctp/server` still have that structure.

### Diagnosis by register read

With the peer kernel in place, hardware said: pinmux correct
(`SCU418 = 0x03003003`, bits 12/13 set; `SCU4B8 = 0`), slave enabled,
`I2CS40 = 0x000000C2` = enable | address `0x42`. But `NVIC ISER[3] = 0x00010000`
= IRQ 112, not 118 — which the kernel split then fixed (`0x00400000`, bit 22).

Manually re-arming `i2cs28` RX_DMA proved the electrical path: the slave
clock-stretched hard enough to wedge the BMC's i2c driver (`D` state). That
confirmed wiring and addressing, and also wedged the bus until the register was
restored — a self-inflicted detour.

### The BMC could not receive

`kind=0x04` from `sender.rs` is a **local** mapping meaning
`NoAcknowledge`, not the `I2cError` discriminant. Nothing on the BMC answered at
`0x10`: under DSP0237 the responder replies by becoming **bus master** and
writing to the requester, so Linux needs a slave backend bound. Two
non-persistent commands fixed it:

```sh
echo i2c-slave-backend 0x1010 > /sys/bus/i2c/devices/i2c-0/new_device
mknod /dev/i2c-slave-backend c 47 0
```

The second is needed because the driver claims **char major 47** via plain
`register_chrdev` with no class device, so devtmpfs never creates the node that
`smbus_shim.c:37` opens.

### Requester config

`mctp_interface = "LIBMCTP"`. `"SHIM"` routes to an `AF_MCTP` kernel socket
requiring Linux > 5.18; on 5.10 it fails as a **silent** `-EIO` from
`_SocketOpen(SOCKET_MCTP)` — no message of its own, just
`mctp_register_endpoint error -5`. The libmctp path fails *loudly*, so a silent
`-5` is itself the diagnostic.

### MTU

`mctp_pktbuf_push` failed with "cannot push MCTP Packet" at GET_CERTIFICATE —
the first exchange exceeding 64 bytes. libmctp allocates
`MCTP_PACKET_SIZE(MCTP_BTU)` = 68-byte buffers; the responder was fragmenting at
`MCTP_I2C_MAXMTU` = **254**.

Fix in `services/mctp/transport-i2c/src/sender.rs`: `get_mtu()` returns a new
`MCTP_BASELINE_MTU = 64` (DSP0236 baseline) instead of the medium maximum, with
`with_mtu()` to opt into more, clamped to `MCTP_I2C_MAXMTU`. Max-capability is
not the same as agreed-MTU, and nothing here negotiates. Two regression tests
added; `mctp_transport_i2c_test` **PASSED**.

---

## 5. AST2700 — MCTP over I2C6

New parent: `eagle-0885.amd.com`, AST2700, kernel **6.18.20** aarch64,
U-Boot 2023.10. Routed to **GPIOW4/W5** on the AST2700 side; the AST1060 end
unchanged.

Disambiguated without asking: the AST1060's pinctrl has banks A–U only, no W, so
GPIOW4/5 had to be the AST2700 side.

`gpio-ranges` gives a 1:1 `PINS [0-215]` mapping, so GPIOW4/W5 = pins **180/181**
= balls F7/D8. The kernel's own pin database confirmed the function:

```
group: I2C6
  pin 180 (F7)
  pin 181 (D8)
```

Here pinctrl names map 1:1 to Linux bus numbers (`I2C6` → `/dev/i2c-6`) —
**unlike** the AST2600.

### Two DT edits, no runtime overlay path

`i2c-bus@700` was `status = "disabled"` but otherwise complete. Enabling it gets
`/dev/i2c-6`; that alone is not enough, because `mctp-i2c` only creates a netdev
when the *adapter's* node carries the `mctp-controller` boolean
(`mctp_i2c_adapter_match()`). `CONFIG_OF_OVERLAY=y` but **`CONFIG_OF_CONFIGFS`
is not set**, so there is no userspace overlay entry point — hence U-Boot.

### The U-Boot sequence, and what broke

Working sequence (edits **after** relocation, `cmdline`/`bdt` skipped):

```
bootm start ${loadaddr}${board_conf}
bootm loados
bootm ramdisk
bootm fdt
fdt set /soc@14000000/bus@14c0f000/i2c-bus@700 status okay
fdt set /soc@14000000/bus@14c0f000/i2c-bus@700 mctp-controller
bootm prep
bootm go
```

Three failures got there:

1. `bootm cmdline` returned `-1` — ARM's `do_bootm_linux()` rejects
   `BOOTM_STATE_OS_CMDLINE` and `OS_BD_T` outright ("No need for those on ARM").
2. Editing the in-FIT blob gave `FDT_ERR_NOSPACE` — no slack for a *new*
   property.
3. Adding `fdt resize 8192` to fix (2) grew the blob over adjacent FIT bytes →
   `ERROR: fdt move failed`, and the half-modified blob then broke even the
   fallback boot with `fdt_find_or_add_subnode: chosen: FDT_ERR_BADSTRUCTURE`,
   producing a reset loop.

The insight that resolved it: `bootm fdt` relocates the blob to a destination
sized with ~12 KB of `CONFIG_SYS_FDT_PAD` and repoints the working FDT there
automatically. Editing *after* relocation needs no resize at all. Comparing the
two relocation lines gave it away — identical destination and size, 90833 bytes,
`OK` in one case and `ERROR` in the other, so the blob was the variable, not the
size.

**Design error of mine:** the `run bootspi_patch; run bootspi_orig` fallback
assumed a failed `fdt set` was side-effect-free. It is not. A safe fallback must
re-run `cp.b` to restore a clean FIT first.

### Recovery

The board boot-looped, then went silent. Two false readings on my side: first
the user's `picocom` held `/dev/ttyUSB0`; then my watcher latched onto a **stale
device node** that survived the USB disconnect, capturing from a dead handle and
reporting 0 bytes. Probing with `stty` instead of trusting the node's existence
fixed it, and Ctrl-C spam caught `sp7#` on the first try despite `bootdelay=0`.

Recovered with `setenv bootspi run bootspi_orig; saveenv; boot` — no SPI reflash
needed. `bootdelay` then set to **3** so future break-ins need one keypress.

**Board characteristic, 3/3 including once with a completely stock env:** warm
reset wedges this board at the BootMCU→ATF transition. Only cold power cycles
recover it. Budget one power cycle per iteration and prefer interactive testing
at `sp7#` over commit-and-reboot loops.

### Runtime setup

```sh
echo mctp-i2c-controller 0x1010 > /sys/bus/i2c/devices/i2c-6/new_device
mctp link set mctpi2c6 up
mctp addr add 32 dev mctpi2c6
mctp addr del 9 dev mctpi2c6      # something at boot claims 9 LOCALLY
mctp route add 9 via mctpi2c6     # must follow the addr del
mctp neigh add 9 dev mctpi2c6 lladdr 0x42
```

The `addr del 9` matters: with EID 9 held as a *local* address, traffic to the
responder is delivered up the local stack instead of going out on the wire.

---

## 6. proto-apps — `VARIANT=emu`

On aarch64 the app spun forever on:

```
Unable to open UIO device
Did you forget "modprobe uio"?
```

Traced:

```
libspdm_req_init() -> libspdm_req_initialize_cert_chain()
  -> spdm_read_root_cert()   (libspdm_common.c:30)
     -> _GetCertifyKey() -> dpe_certify_key() -> _IrotSendDpeCommand()
        -> caliptra_read/write_u32() -> MapCaliptra() -> open("/dev/uio4") FAILS
```

`MapCaliptra()` returns without setting `g_map_gpio`, so its `if (!g_map_gpio)`
guard never latches and every accessor retries — spam rather than one failure.

**This was never an AF_MCTP problem.** `mctp_init_ex()` had already succeeded;
the giveaway is the *absence* of `spdm_client_init: can't intialize MCTP`.

x86-64 already had the escape hatch: genoa's `config.cmake` only sets
`PLAT_SUPPORT_IROT` when `VARIANT != emu`, and `common/os/linux/CMakeLists.txt:56`
links `irot_stub.c` when it is unset. aarch64 had exactly one platform,
`caliptra`, with both the config and the CMakeLists unguarded.

**Changes**

- `aarch64-linux/plat/caliptra/config.cmake` — `PLAT_SUPPORT_IROT` guarded by
  the same `VARIANT != emu` test.
- `aarch64-linux/plat/caliptra/CMakeLists.txt` — `irot_shim.c` behind
  `if (DEFINED PLAT_SUPPORT_IROT)`; `caliptra_interface.c` left unconditional so
  libcaliptra's externs resolve (the linker drops it anyway).
- `Makefile` — `VARIANT` plumbed into the cmake line, build dir suffixed
  `-$(VARIANT)` (because `-DVARIANT=emu` sticks in CMakeCache), plus
  `emu-requester` / `emu-responder` / `emu-both` and suffixed
  `emu-requester-%` / `emu-responder-%`. `clean` glob widened.

`VARIANT` must be left **undefined**, never `OFF`: the plat CMakeLists tests
`DEFINED` while the common one tests truthiness, so `OFF` compiles *both*.

Verified the guard logic in isolation with a standalone cmake project — exactly
one of shim/stub in every case.

**Two rounds lost to deployment, not code:** first `make emu-responder-…` was run
(builds the *responder*; the BMC is the requester), then the correct build was
made but the copy came from the old directory — the emu artifact has the **same
filename** in a **different directory**. Settled by md5 on both ends; a correct
emu binary contains no `Unable to open UIO device` string at all.

---

## 7. Files changed

**openprot**
- `target/ast10x0/peripherals/smc/device/hal_impl.rs` (new)
- `target/ast10x0/peripherals/smc/device/flash.rs`, `device/mod.rs`, `smc/mod.rs`
- `target/ast10x0/peripherals/scu/pinctrl.rs` — `PINCTRL_I2C8`
- `target/ast10x0/peripherals/BUILD.bazel`
- `target/ast10x0/tests/smc/write/{target.rs,BUILD.bazel}`
- `target/ast10x0/tests/spdm/auth/{target.rs,BUILD.bazel,peer_system.json5,i2c_server_main_peer.rs}`
- `services/mctp/transport-i2c/src/sender.rs` — baseline MTU
- `target/ast10x0/harness/uart_test_exec.py`, `test_uart_test_exec.py`

**proto-apps**
- `platform_shim/aarch64-linux/plat/caliptra/{config.cmake,CMakeLists.txt}`
- `Makefile`

**Docs**
- `/home/wmarone/eagle-0885-mctp-i2c6-setup.md`
- this file

---

## 8. Open items

- **Certificate provisioning** — the AST1060's `MockCertStore` serves
  `[0xAA; 32]` (`mock_platform.rs:58`) and `sign_hash` is also a mock, so
  CHALLENGE_AUTH would fail even with a valid chain. Needs decisions: where the
  key and chain come from, where the key lives on the AST1060 (image / OTP / the
  flash now reachable through the `Flash` HAL), and whether signing uses the HACE
  block.
- **Nothing on eagle-0885 persists** except `bootdelay=3`, the env vars and the
  deployed binary. Persisting the DT edits means committing the U-Boot sequence
  to `bootspi` — with a fallback that re-copies the FIT.
- **`uart_test_exec.py` timeout-reports-PASS** — flagged, not fixed.
- **Requester has no local cert chain** with the emu stub. Fine for
  `req_mode = "ATTESTATOR"`; fails if anything negotiates MUT_AUTH.
- **`vca` and `tests/mctp/server`** still share one kernel across two system
  configs — the same landmine fixed in `auth`.
- **Latent `mctp_shim` defects** (confirmed not blocking): bare `malloc` leaves
  `dst_eid` uninitialised; `set_destination()` binds with the *old* EID; the bind
  uses the *remote* EID as the local bind address; the hand-rolled
  `sockaddr_mctp` omits the kernel's padding fields.

---

## 9. Recurring traps

- **1-based vs 0-based peripheral naming.** AST1060 SVD `SCL3/SDA3` = PAC
  controller 2. AST2600 pinctrl `I2C1` = `/dev/i2c-0`. AST2700 `I2C6` =
  `/dev/i2c-6` (no offset). Verify per platform; do not carry the rule over.
- **BusyBox `head -N` / `od -A`** are unsupported on these BMCs — use `head -n N`.
  Cost several silently truncated outputs.
- **`which` misses `/sbin` and `/usr/sbin`** in these shells. `fw_setenv` and
  `i2cdetect` were both declared absent when they were present.
- **Stale `/dev/ttyUSB0`** survives a USB disconnect; its existence is not proof
  the device is back. Probe with `stty`.
- **One reader per serial port** — a running `picocom` starves any capture.
- **Kernel-version-dependent config.** `mctp_interface` must be `LIBMCTP` below
  5.18 and `SHIM` above; the same file is wrong on the other box.
