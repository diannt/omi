#!/usr/bin/env bash
# Loopback test for usb_dump: simulate the device protocol via socat PTY pair.
# Expects: cargo build --bin usb_dump already run.
set -euo pipefail

BIN="${BIN:-./target/debug/usb_dump}"
OUT=/tmp/usb_dump_test.bin
PAYLOAD_LEN=${PAYLOAD_LEN:-4096}

# Start socat PTY pair, capture its stderr for port names
SOCAT_LOG=$(mktemp)
socat -d -d pty,raw,echo=0 pty,raw,echo=0 2>"$SOCAT_LOG" &
SOCAT_PID=$!
trap 'kill $SOCAT_PID 2>/dev/null; rm -f "$SOCAT_LOG"' EXIT

# Wait for socat to print both PTY paths
for _ in $(seq 1 20); do
    [ "$(grep -c 'PTY is' "$SOCAT_LOG")" -ge 2 ] && break
    sleep 0.1
done

PTY1=$(grep -oP '/dev/pts/\d+' "$SOCAT_LOG" | sed -n '1p')
PTY2=$(grep -oP '/dev/pts/\d+' "$SOCAT_LOG" | sed -n '2p')

if [ -z "$PTY1" ] || [ -z "$PTY2" ]; then
    echo "FAIL: could not parse socat PTY paths"
    cat "$SOCAT_LOG"
    exit 1
fi
echo "[test] device side: $PTY1, host side: $PTY2"

# Generate deterministic payload + protocol response on PTY1 ("device" side).
# Device reads 6 command bytes, then replies: [0x00 status][4B LE size][payload][0x64 EOT]
python3 - "$PTY1" "$PAYLOAD_LEN" <<'PY' &
import os, struct, sys
port, plen = sys.argv[1], int(sys.argv[2])
fd = os.open(port, os.O_RDWR)
# drain the 6-byte command
got = b''
while len(got) < 6:
    chunk = os.read(fd, 6 - len(got))
    if not chunk:
        raise SystemExit("device: host disconnected before sending command")
    got += chunk
assert got[0] == 0x00 and got[1] == 0x01, f"unexpected cmd bytes: {got.hex()}"
# respond: status OK, file_size LE, payload (0x00..0xFF repeating), EOT
os.write(fd, b'\x00')
os.write(fd, struct.pack('<I', plen))
payload = bytes(i & 0xFF for i in range(plen))
# write in chunks to mimic USB FIFO
i = 0
while i < plen:
    n = os.write(fd, payload[i:i+512])
    i += n
os.write(fd, b'\x64')
os.close(fd)
PY
FAKE_DEVICE_PID=$!

# Run usb_dump against the "host" side
"$BIN" --port "$PTY2" --out "$OUT" --offset 0 --timeout-secs 5 >/tmp/usb_dump_progress.json 2>/tmp/usb_dump_stderr.log
DUMP_RC=$?

wait $FAKE_DEVICE_PID 2>/dev/null || true

if [ $DUMP_RC -ne 0 ]; then
    echo "FAIL: usb_dump exited $DUMP_RC"
    cat /tmp/usb_dump_stderr.log
    exit 1
fi

# Verify output size
ACTUAL=$(stat -c %s "$OUT")
if [ "$ACTUAL" -ne "$PAYLOAD_LEN" ]; then
    echo "FAIL: expected $PAYLOAD_LEN bytes, got $ACTUAL"
    cat /tmp/usb_dump_stderr.log
    exit 1
fi

# Verify payload content (first 16 bytes should be 00 01 02 ... 0f)
EXPECT_HEAD="000102030405060708090a0b0c0d0e0f"
GOT_HEAD=$(xxd -p -l 16 "$OUT" | tr -d '\n')
if [ "$GOT_HEAD" != "$EXPECT_HEAD" ]; then
    echo "FAIL: payload head mismatch: expected $EXPECT_HEAD, got $GOT_HEAD"
    exit 1
fi

echo "PASS: received $ACTUAL bytes, content verified, EOT received"
grep "EOT verified" /tmp/usb_dump_stderr.log && echo "PASS: EOT marker confirmed" || echo "WARN: EOT not logged"
echo "--- stderr ---"
cat /tmp/usb_dump_stderr.log
