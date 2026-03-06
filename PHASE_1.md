# Phase 1 — Firmware USB CDC + Rust Serial Receiver

**Start:** 2026-03-05 18:41:33

## Goal
Add USB CDC-ACM endpoint to nRF5340 firmware that streams SD card contents on command. Build Rust CLI that opens serial port, sends 6-byte storage command, streams to disk.

## Protocol (mirrors BLE storage protocol)
```
Host → Device (6 bytes): [0x00=READ][0x01=file_num][offset BE u32]
Device → Host (1 byte):  [status: 0x00=OK, 0x03=bad_size, 0x04=zero, 0x06=bad_cmd]
Device → Host (4 bytes): [u32 LE file_size]
Device → Host:           [raw stream, file_size bytes]
Device → Host (1 byte):  [0x64=EOT]
```

## Files Created
- `omi/firmware/omi/src/usb_dump.c` — CDC init, command parser, TX loop (4KB chunks)
- `omi/firmware/omi/src/usb_dump.h` — public API
- `omi/firmware/omi/boards/omi_nrf5340_cpuapp.overlay` — enable zephyr_udc0 + cdc_acm_uart0
- `desktop/Backend-Rust/src/bin/usb_dump.rs` — serial CLI, stream-to-disk

## Files Modified
- `omi/firmware/omi/omi.conf` — USB CDC Kconfig opts + `CONFIG_OMI_ENABLE_USB_DUMP=y`
- `omi/firmware/omi/Kconfig` — add `OMI_ENABLE_USB_DUMP`
- `omi/firmware/omi/CMakeLists.txt` — conditional `usb_dump.c`
- `omi/firmware/omi/src/main.c` — call `usb_dump_init()` after SD init
- `desktop/Backend-Rust/Cargo.toml` — `serialport`, `clap`, `[[bin]]`

## Roadblocks
- `clang-format` not installed in WSL — deferred to pre-commit hook
- Firmware build deferred to P6 (toolchain IS present at `~/ncs` — initially overlooked, corrected post-commit)
- Flash path undetermined: `nrfutil` not installed, SWD availability unknown, device's current CDC endpoint purpose unknown (console? mcumgr?)

## Test Output

### Rust cargo check
```
Checking omi-desktop-backend v0.1.0
Finished `dev` profile [unoptimized + debuginfo] target(s) in 12.04s
```

### socat loopback (4 KB)
```
[test] device side: /dev/pts/8, host side: /dev/pts/9
PASS: received 4096 bytes, content verified, EOT received
PASS: EOT marker confirmed
[usb_dump] done: 4096 bytes → /tmp/usb_dump_test.bin in 0.00s @ 62.63 MB/s
```

### socat loopback (256 KB, chunked)
```
[test] device side: /dev/pts/10, host side: /dev/pts/11
PASS: received 262144 bytes, content verified, EOT received
PASS: EOT marker confirmed
[usb_dump] done: 262144 bytes → /tmp/usb_dump_test.bin in 0.01s @ 19.30 MB/s
```

## Firmware Build & Flash — DEFERRED TO PHASE 6

**Environment inventory (probed 2026-03-05 18:57):**
| Tool | Status | Path |
|---|---|---|
| west | ✅ v1.5.0 | PATH |
| nRF Connect SDK | ✅ 2.9.0 | `~/ncs/{zephyr,nrf,nrfxlib,bootloader}` |
| Zephyr SDK toolchain | ✅ arm-zephyr-eabi | `~/zephyr-sdk*` |
| openocd | ✅ | `/usr/bin/openocd` |
| nrfutil | ❌ not installed | — |
| **Device** | ✅ enumerated | `/dev/ttyACM0` (VID:PID `2fe3:0100 NordicSemiconductor USB-DEV`) |

**Live device probe:**
```python
# Sent 6-byte READ cmd [0x00 0x01 0x00 0x00 0x00 0x00] to /dev/ttyACM0
# → 0 bytes response
```
Current firmware does NOT speak storage protocol over CDC. The `2fe3:0100` enumeration is from existing firmware (USB console or MCUboot serial recovery), NOT from `usb_dump.c`. **Build + flash required.**

**Scheduled for P6** alongside E2E pipeline test. Build command (no user action — runs in WSL):
```bash
cd ~/ncs && west build -b omi/nrf5340/cpuapp \
  ~/dev/code_pref/model_b/omi/firmware/omi --sysbuild \
  -- -DBOARD_ROOT=~/dev/code_pref/model_b/omi/firmware
```

**Flash path — OPEN QUESTION (see P1 feedback request):**
- openocd + SWD → needs SWD pins exposed on device
- MCUboot serial recovery → if current CDC is mcumgr, can `mcumgr image upload` over `/dev/ttyACM0`
- BLE DFU via nRF Connect mobile app → only path that is **actual user action** (physical phone)

**End:** 2026-03-05 18:54:48
