#!/usr/bin/env python3
# Licensed under the Apache-2.0 license
# SPDX-License-Identifier: Apache-2.0
"""
AST1060 EVB test orchestration layer.

Loads evb_config.toml, resolves the ELF for detokenization, then either
spawns pi_test_runner.py locally (USB-attached board) or SCP+SSHes it to a
Raspberry Pi fixture. Raw UART bytes come back over a pipe or SSH stdout;
this script detokenizes them and pattern-matches for PASS/FAIL sentinels.
Diagnostics go to stderr; nothing is written to the Pi filesystem except the
firmware binary and pi_test_runner.py itself.
"""

import argparse
import base64
import binascii
import os
import signal
import subprocess
import sys
import threading
import time
import tomllib
from pathlib import Path

AST1060_EVB_PI_HOST = "AST1060_EVB_PI_HOST"

# ── pw_tokenizer discovery ────────────────────────────────────────────────────
# When run as a Bazel py_binary, pw_tokenizer is already on sys.path via deps.
# When run outside Bazel, try PW_TOK_ROOT or the Bazel output base as fallbacks.


def _extend_path_for_pw_tokenizer() -> None:
    pw_tok_root = os.environ.get("PW_TOK_ROOT")
    if pw_tok_root:
        sys.path.insert(0, os.path.join(pw_tok_root, "pw_tokenizer", "py"))
        return
    try:
        output_base = subprocess.check_output(
            ["bazel", "info", "output_base"], text=True, stderr=subprocess.DEVNULL
        ).strip()
        candidate = os.path.join(
            output_base, "external", "pigweed+", "pw_tokenizer", "py"
        )
        if os.path.isdir(candidate):
            sys.path.insert(0, candidate)
    except (subprocess.CalledProcessError, FileNotFoundError):
        pass


_extend_path_for_pw_tokenizer()

try:
    from pw_tokenizer import Detokenizer
    from pw_tokenizer.detokenize import NestedMessageParser

    _PW_TOKENIZER_AVAILABLE = True
except ImportError:
    _PW_TOKENIZER_AVAILABLE = False

# ── Remote lock constants ─────────────────────────────────────────────────────
# The Pi is a single-board computer; only one test session should hold the
# UART device and GPIO lines at a time. We use an atomic noclobber lock file
# on the Pi and touch it every _LOCK_TOUCH_INTERVAL seconds from a background
# thread so the stale-lock detector knows we're still alive.

_LOCK_PATH = "/tmp/ast1060_evb.lock"

# Bracketed first letter so the pattern does not match the shell running it.
_KILL_RUNNER = "pkill -f '[p]i_test_runner.py' || true"
_LOCK_TOUCH_INTERVAL = 10  # seconds between lock touches
_LOCK_STALE_THRESHOLD = 60  # seconds since last touch → lock is stale
_LOCK_ACQUIRE_TIMEOUT = 120  # seconds to wait before giving up


# ── SSH helpers ───────────────────────────────────────────────────────────────


def _ssh(host: str, cmd: str, **kwargs) -> subprocess.CompletedProcess:
    """Run a single SSH command synchronously."""
    return subprocess.run(
        ["ssh", "-o", "BatchMode=yes", host, cmd],
        **kwargs,
    )


def _ssh_stream(host: str, cmd: str) -> subprocess.Popen:
    """Open a streaming SSH session; stdout is a pipe the caller reads."""
    return subprocess.Popen(
        ["ssh", "-o", "BatchMode=yes", "-o", "ServerAliveInterval=5", host, cmd],
        stdout=subprocess.PIPE,
        stderr=sys.stderr,
    )


def _acquire_lock(host: str, timeout: int = _LOCK_ACQUIRE_TIMEOUT) -> bool:
    """Atomically create lock file on Pi; retry until timeout."""
    deadline = time.time() + timeout
    local_user = os.environ.get("USER", "unknown")
    # On noclobber failure, cat outputs the lock file contents so we get
    # owner info from the same SSH call without a second round-trip.
    create = (
        f"set -o noclobber && "
        f"printf '%s\\t%s\\n' '{local_user}' \"$(date +%s)\" > {_LOCK_PATH} || "
        f"{{ cat {_LOCK_PATH} 2>/dev/null; false; }}"
    )
    # A stale lock means the holder is gone or wedged, but its runner may still
    # be alive and holding the serial ports — which looks to the next run like a
    # board that never reaches the bootloader. Clear both.
    stale_check = (
        f"mtime=$(stat -c %Y {_LOCK_PATH} 2>/dev/null) && "
        f"now=$(date +%s) && "
        f"[ $(( now - mtime )) -gt {_LOCK_STALE_THRESHOLD} ] && "
        f"{{ {_KILL_RUNNER}; rm -f {_LOCK_PATH}; }}"
    )
    last_printed = 0.0
    while time.time() < deadline:
        r = _ssh(host, create, capture_output=True)
        if r.returncode == 0:
            return True
        if not r.stdout.strip():
            if msg := r.stderr.decode(errors="replace").strip():
                print(f"RUNNER: {msg}", file=sys.stderr)
                return False
            continue
        parts = r.stdout.decode(errors="replace").split()
        now = time.time()
        if len(parts) == 2 and now - last_printed >= 30:
            lock_user, acq_ts = parts[0], int(parts[1])
            held_s = int(now) - acq_ts
            mins, secs = divmod(held_s, 60)
            held = f"{mins}m {secs}s" if mins else f"{secs}s"
            stat_r = _ssh(
                host,
                f"now=$(date +%s); mtime=$(stat -c %Y {_LOCK_PATH} 2>/dev/null); echo $((now - mtime))",
                capture_output=True,
            )
            touch_ago = int(stat_r.stdout.decode().strip() or -1)
            if touch_ago > _LOCK_STALE_THRESHOLD:
                print(
                    f"RUNNER: removing stale lock held by {lock_user} {held} ago, last touched {touch_ago}s ago...",
                    file=sys.stderr,
                )
            else:
                print(
                    f"RUNNER: Pi locked by {lock_user} {held} ago, last touched {touch_ago}s ago; waiting...",
                    file=sys.stderr,
                )
            last_printed = now
        _ssh(host, stale_check, capture_output=True)
        time.sleep(2)
    print(
        f"RUNNER: timeout acquiring Pi lock after {timeout}s (Pi may be busy)",
        file=sys.stderr,
    )
    return False


def _kill_remote_runner(host: str) -> None:
    """Kill any pi_test_runner.py on the Pi so it lets go of the serial ports.

    Terminating the local ssh does not reach the remote process — there is no
    tty for the hangup to travel down — so a run that ends any way other than
    the runner exiting on its own leaves it holding both UARTs.
    """
    _ssh(host, _KILL_RUNNER, capture_output=True)


def _release_lock(host: str) -> None:
    _ssh(host, f"rm -f {_LOCK_PATH}", capture_output=True)


def _reap_stale_runners(host: str) -> None:
    """Kill orphaned runners left behind when an SSH session drops.

    The remote python does not die with its SSH client, and an orphan that still
    holds the UARTs silently swallows the bootloader handshake of every later
    run. Only called while holding the lock, so any match is an orphan.
    """
    # The [p] keeps the pattern from matching the shell that runs pkill.
    r = _ssh(host, "pkill -f '[p]i_test_runner.py'", capture_output=True)
    if r.returncode == 0:
        print("RUNNER: reaped orphaned pi_test_runner.py on the Pi", file=sys.stderr)
        # Let the kernel release the serial ports before we reopen them.
        time.sleep(1)


def _touch_lock_forever(host: str, stop: threading.Event) -> None:
    """Background thread: touch the lock file every _LOCK_TOUCH_INTERVAL seconds."""
    while not stop.wait(_LOCK_TOUCH_INTERVAL):
        _ssh(host, f"touch {_LOCK_PATH}", capture_output=True)


# ── UART monitor ──────────────────────────────────────────────────────────────


class UartMonitor:
    """Detokenizes and displays raw UART bytes. Pass/fail is determined by pi_test_runner.py exit code."""

    def __init__(
        self,
        args: argparse.Namespace,
        *elf_paths: Path,
        board_logs: dict[str, Path] | None = None,
    ) -> None:
        self.args = args
        self.log_file_handle = open(args.log_file, "w") if args.log_file else None
        elf_args = [str(p) for p in elf_paths if p is not None]
        self.detokenizer = (
            Detokenizer(*elf_args) if _PW_TOKENIZER_AVAILABLE and elf_args else None
        )
        # One parser per board: it keeps state across calls, so a single shared
        # parser lets an unterminated token from one board be completed by the
        # next line from the other, printing that board's message in the wrong
        # stream.
        self._token_parsers: dict[str, object] = {}
        # Per-board log files, keyed by the tag pi_test_runner.py prefixes each
        # line with. Opened lazily so a board that never speaks leaves no file.
        self._board_log_paths = board_logs or {}
        self._board_log_handles: dict[str, object] = {}
        self._pending = b""

    def _write_log(self, text: str) -> None:
        if self.log_file_handle:
            self.log_file_handle.write(text)
            self.log_file_handle.flush()

    def _write_board_log(self, tag: str, text: str) -> None:
        path = self._board_log_paths.get(tag)
        if path is None:
            return
        handle = self._board_log_handles.get(tag)
        if handle is None:
            handle = open(path, "w")
            self._board_log_handles[tag] = handle
        handle.write(text)
        handle.flush()

    def _render(self, raw: bytes, tag: str) -> str:
        """Detokenize one line, returning text with tokens replaced."""
        if not (self.detokenizer and _PW_TOKENIZER_AVAILABLE):
            return raw.decode("utf-8", errors="replace")

        parser = self._token_parsers.get(tag)
        if parser is None:
            parser = NestedMessageParser()
            self._token_parsers[tag] = parser

        out = []
        for is_token, span in parser.read_messages(raw):
            if not is_token:
                out.append(span.decode("utf-8", errors="replace"))
                continue

            # span is b'$<base64chars>' — strip '$' and add padding.
            try:
                b64 = span[1:]
                b64 += b"=" * (-len(b64) % 4)
                result = self.detokenizer.detokenize(
                    base64.b64decode(b64, validate=True)
                )
            except (binascii.Error, ValueError):
                result = None

            if result is not None and result.ok():
                out.append(f"\033[32m{result}\033[0m")
            else:
                out.append(span.decode("utf-8", errors="replace"))
        return "".join(out)

    def _handle_line(self, line: bytes) -> None:
        """Route one line: SOH + tag identifies the board that sent it."""
        tag = ""
        if line.startswith(b"\x01") and len(line) >= 2:
            tag = line[1:2].decode("ascii", errors="replace")
            line = line[2:]

        text = self._render(line, tag)
        label = f"{tag}| " if tag else ""
        os.write(1, f"{label}{text}\n".encode("utf-8", errors="replace"))
        self._write_log(f"{label}{text}\n")
        if tag:
            # Board logs hold that board's output alone, without the label.
            self._write_board_log(tag, f"{text}\n")

    def print_uart_data(self, raw: bytes) -> None:
        """Buffer raw bytes and process them a line at a time.

        pi_test_runner.py emits one whole line per write, tagged with the board
        it came from, so lines arrive intact even with two boards sharing the
        stream. Chunk boundaries still fall mid-line, hence the buffer.
        """
        self._pending += raw
        while b"\n" in self._pending:
            line, self._pending = self._pending.split(b"\n", 1)
            self._handle_line(line)

    def display_stream(self, pipe) -> None:
        """Read raw bytes from pipe and detokenize for display until EOF."""
        while True:
            chunk = pipe.raw.read(4096)
            if not chunk:
                break
            self.print_uart_data(chunk)

    def cleanup(self) -> None:
        if self._pending:
            self._handle_line(self._pending)
            self._pending = b""
        if self.log_file_handle:
            self.log_file_handle.close()
            self.log_file_handle = None
        for handle in self._board_log_handles.values():
            handle.close()
        self._board_log_handles.clear()


# ── Local execution ───────────────────────────────────────────────────────────


def _run_local(
    args: argparse.Namespace,
    config: dict,
    runner: Path,
    monitor: UartMonitor,
    uart_device: str,
) -> bool:
    """Wired (non-SSH) connection to the Pi fixture — not yet implemented."""
    print(
        "Error: wired Pi mode is not yet implemented. "
        f"Set ${AST1060_EVB_PI_HOST} or pass --pi-host to use SSH.",
        file=sys.stderr,
    )
    return False


# ── Remote execution ──────────────────────────────────────────────────────────


def _run_remote(
    args: argparse.Namespace,
    config: dict,
    runner: Path,
    monitor: UartMonitor,
    uart_device: str,
) -> bool:
    """SCP firmware and runner to Pi; stream UART back over SSH stdout."""
    host = args.pi_host
    gpio = config["gpio"]
    uart = config["uart"]
    baudrate = args.baudrate if args.baudrate else uart["baudrate"]

    remote_dir = "/tmp/ast1060_test"

    if not _acquire_lock(host):
        return False
    print("RUNNER: lock acquired, starting test", file=sys.stderr)

    _reap_stale_runners(host)

    stop_touch = threading.Event()
    touch_thread = threading.Thread(
        target=_touch_lock_forever, args=(host, stop_touch), daemon=True
    )
    touch_thread.start()

    _ssh(host, f"rm -rf {remote_dir} && mkdir -p {remote_dir}", check=True)

    subprocess.run(
        ["scp", "-q", str(runner), f"{host}:{remote_dir}/pi_test_runner.py"],
        check=True,
    )

    if not args.parse_only:
        firmware_path = Path(args.firmware)
        remote_fw = f"{remote_dir}/{firmware_path.name}"
        subprocess.run(
            ["scp", "-q", str(firmware_path), f"{host}:{remote_fw}"],
            check=True,
        )
    else:
        remote_fw = None

    remote_slave_fw = None
    if args.slave_firmware:
        slave_firmware_path = Path(args.slave_firmware)
        remote_slave_fw = f"{remote_dir}/{slave_firmware_path.name}"
        subprocess.run(
            ["scp", "-q", str(slave_firmware_path), f"{host}:{remote_slave_fw}"],
            check=True,
        )

    remote_cmd = f"python3 -u {remote_dir}/pi_test_runner.py {uart_device}"
    if not args.parse_only:
        remote_cmd += f" {remote_fw}"
    remote_cmd += (
        f" --srst-pin {gpio['srst_pin']}"
        f" --fwspick-pin {gpio['fwspick_pin']}"
        f" --baudrate {baudrate}"
    )
    if args.parse_only:
        remote_cmd += " --stream-only"
    if remote_slave_fw:
        device_b = config["device_b"]
        remote_cmd += (
            f" --slave-firmware {remote_slave_fw}"
            f" --slave-uart-device {device_b['serial_port']}"
            f" --slave-srst-pin {device_b['srst_pin']}"
            f" --slave-fwspick-pin {device_b['fwspick_pin']}"
        )

    proc = _ssh_stream(host, remote_cmd)

    # --duration bounds a test that would otherwise run until Bazel's test
    # timeout. Reaching it is a failure: the test had that long to reach a
    # result and did not.
    expired = threading.Event()
    duration_timer = None
    if args.duration:
        def _expire() -> None:
            expired.set()
            print(
                f"RUNNER: {args.duration}s duration reached with no result, "
                "failing test",
                file=sys.stderr,
            )
            proc.terminate()

        duration_timer = threading.Timer(args.duration, _expire)
        duration_timer.daemon = True
        duration_timer.start()

    try:
        monitor.display_stream(proc.stdout)
        proc.wait()
    finally:
        if duration_timer:
            duration_timer.cancel()
        if proc.returncode is None:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
        _reap_stale_runners(host)
        stop_touch.set()
        _release_lock(host)

    return proc.returncode == 0 and not expired.is_set()


# ── Entry point ───────────────────────────────────────────────────────────────


def main() -> int:
    with (Path(__file__).parent / "evb_config.toml").open("rb") as f:
        config = tomllib.load(f)

    parser = argparse.ArgumentParser(
        description="AST1060 EVB test orchestration: detokenize UART, match sentinels"
    )
    parser.add_argument(
        "firmware",
        nargs="?",
        help=(
            "Firmware image (.elf or .bin). system_image emits both under the "
            "same stem; pass either and the other is derived automatically."
        ),
    )
    parser.add_argument(
        "--pi-host",
        default=None,
        help=f"Raspberry Pi hostname/IP. Falls back to ${AST1060_EVB_PI_HOST} env var. Omit both to use a locally attached board",
    )
    parser.add_argument(
        "--baudrate",
        type=int,
        default=None,
        help=f"Override baud rate from evb_config.toml (default: {config['uart']['baudrate']})",
    )
    parser.add_argument(
        "--log-file",
        help="Write detokenized UART output to this file",
    )
    parser.add_argument(
        "--duration",
        type=int,
        default=None,
        help=(
            "Fail the test if it has not reached a result within this many "
            "seconds. Bounds a run that would otherwise sit until the Bazel "
            "test timeout"
        ),
    )
    parser.add_argument(
        "--log-dir",
        default=None,
        help=(
            "Directory for per-board UART logs, one file per board named "
            "<image>_<board>_<device>_<timestamp>.log. Defaults to "
            "$TEST_UNDECLARED_OUTPUTS_DIR, else the current directory"
        ),
    )
    parser.add_argument(
        "-q",
        "--quiet",
        action="store_true",
        help="Suppress diagnostic messages to stderr",
    )
    parser.add_argument(
        "--parse-only",
        action="store_true",
        help="Skip GPIO and firmware upload; stream and detokenize UART output only",
    )
    args, remaining = parser.parse_known_args()

    if os.environ.get("PW_RUNNER_PASSTHROUGH") == "1":
        import subprocess

        res = subprocess.run([args.firmware] + remaining)
        return res.returncode

    uart_device = os.environ.get("UART_DEVICE") or config["uart"]["serial_port"]

    if not args.firmware:
        parser.error("firmware is required")

    # system_image emits both .elf and .bin under the same stem; accept either.
    # When invoked via --run_under on a system_image_test, Bazel passes the
    # no-suffix symlink (e.g. threads_test → threads.elf); resolve it first.
    image = Path(args.firmware)

    # When invoked via --run_under the firmware path has no suffix (it is the
    # test-name symlink).  The system_image_test rule places a companion
    # <name>.slave.elf symlink alongside it when slave_image is set; detecting
    # that file is how we enter paired mode without any extra CLI arguments.
    args.slave_firmware = None
    slave_elf_path = None
    if not image.suffix:
        slave_symlink = image.parent / (image.name + ".slave.elf")
        if slave_symlink.exists():
            slave_elf_path = slave_symlink.resolve()
            args.slave_firmware = str(slave_elf_path.with_suffix(".bin"))
        image = image.resolve()
        args.firmware = str(image.with_suffix(".bin"))
    else:
        args.firmware = str(image.with_suffix(".bin"))

    elf_path = image.with_suffix(".elf")
    if not elf_path.exists():
        print(f"Error: ELF not found at {elf_path}", file=sys.stderr)
        return 1

    args.pi_host = os.environ.get(AST1060_EVB_PI_HOST) or args.pi_host

    runner = Path(__file__).parent / "pi_test_runner.py"

    log_dir = Path(
        args.log_dir or os.environ.get("TEST_UNDECLARED_OUTPUTS_DIR") or "."
    )
    log_dir.mkdir(parents=True, exist_ok=True)
    stamp = time.strftime("%Y%m%d-%H%M%S")

    def _board_log(image: Path, board: str, device: str) -> Path:
        return log_dir / f"{image.stem}_{board}_{Path(device).name}_{stamp}.log"

    board_logs = {"A": _board_log(elf_path, "A", uart_device)}
    if slave_elf_path is not None:
        board_logs["B"] = _board_log(
            slave_elf_path, "B", config["device_b"]["serial_port"]
        )
    for board, path in board_logs.items():
        print(f"RUNNER: board {board} log -> {path}", file=sys.stderr)

    monitor = UartMonitor(args, elf_path, slave_elf_path, board_logs=board_logs)

    signal.signal(signal.SIGINT, lambda s, f: sys.exit(130))

    try:
        if args.pi_host:
            ok = _run_remote(args, config, runner, monitor, uart_device)
        else:
            ok = _run_local(args, config, runner, monitor, uart_device)
    except Exception as e:
        print(f"Error: {e}", file=sys.stderr)
        return 1
    finally:
        monitor.cleanup()

    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
