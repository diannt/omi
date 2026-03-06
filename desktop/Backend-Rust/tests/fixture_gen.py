#!/usr/bin/env python3
"""
Generate a realistic raw SD-card dump fixture for the Opus frame parser.

On-disk format (firmware transport.c:800-817):
  [u8 len][opus_frame ≤80B][u8 len][opus_frame]...
packed into 512B write blocks with zero-padding at block boundaries.

Valid Opus TOC bytes (WALModel.swift:208):
  {0xb8, 0xb0, 0xbc, 0xf8, 0xfc, 0x78, 0x7c}

Usage:
  ./fixture_gen.py [--frames N] [--out PATH] [--corrupt-rate R] [--pad-512]
"""
import argparse
import random
import sys

TOC_BYTES = [0xB8, 0xB0, 0xBC, 0xF8, 0xFC, 0x78, 0x7C]


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--frames", type=int, default=10_000, help="number of valid frames")
    ap.add_argument("--out", default="/tmp/opus_fixture.bin", help="output path")
    ap.add_argument(
        "--corrupt-rate",
        type=float,
        default=0.0,
        help="fraction of frames to corrupt (bad TOC or bad len)",
    )
    ap.add_argument(
        "--pad-512",
        action="store_true",
        help="zero-pad to 512B block boundaries like real firmware",
    )
    ap.add_argument("--seed", type=int, default=42, help="RNG seed for determinism")
    args = ap.parse_args()

    random.seed(args.seed)
    buf = bytearray()
    block_fill = 0
    corrupt_count = 0

    for i in range(args.frames):
        is_corrupt = random.random() < args.corrupt_rate
        if is_corrupt:
            corrupt_count += 1
            # Alternate corruption modes: bad-len vs bad-toc
            if corrupt_count % 2 == 0:
                # bad len: > MAX_FRAME_LEN (160)
                flen = random.randint(170, 250)
                frame = bytes([flen]) + bytes(
                    random.randint(0, 255) for _ in range(flen)
                )
            else:
                # bad TOC: valid len, invalid first byte
                flen = random.randint(60, 80)
                bad_toc = random.randint(0, 255)
                while bad_toc in TOC_BYTES:
                    bad_toc = random.randint(0, 255)
                body = bytes([bad_toc]) + bytes(
                    random.randint(0, 255) for _ in range(flen - 1)
                )
                frame = bytes([flen]) + body
        else:
            flen = 60 + (i % 21)  # 60..80 bytes, deterministic cycle
            toc = TOC_BYTES[i % len(TOC_BYTES)]
            body = bytes([toc]) + bytes((i & 0xFF) for _ in range(flen - 1))
            frame = bytes([flen]) + body

        if args.pad_512:
            # If frame would cross a 512B boundary, pad to boundary first
            if block_fill + len(frame) > 512:
                pad = 512 - block_fill
                buf.extend(b"\x00" * pad)
                block_fill = 0
            buf.extend(frame)
            block_fill = (block_fill + len(frame)) % 512
        else:
            buf.extend(frame)

    with open(args.out, "wb") as f:
        f.write(buf)

    print(f"wrote {len(buf):,} bytes → {args.out}", file=sys.stderr)
    print(
        f"  {args.frames:,} frames ({corrupt_count} corrupt), "
        f"pad-512={args.pad_512}, seed={args.seed}",
        file=sys.stderr,
    )
    # Machine-parseable summary on stdout
    print(f"{args.frames - corrupt_count} {corrupt_count} {len(buf)}")


if __name__ == "__main__":
    main()
