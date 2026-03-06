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
- Firmware build deferred: `west` + nRF Connect SDK 2.9 not installed (~1.5 GB). User builds via `nrfutil toolchain-manager` per `omi/firmware/BUILD_AND_OTA_FLASH.md` then OTA DFU via nRF Connect mobile app.

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

## Firmware Build (user action)
```bash
cd omi/firmware/v2.9.0
west build -b omi/nrf5340/cpuapp ../omi --sysbuild -- -DBOARD_ROOT=$(pwd)/..
# Output: build/dfu_application.zip → transfer to phone → nRF Connect DFU
```

**End:** 2026-03-05 18:54:48
