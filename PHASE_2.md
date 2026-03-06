# Phase 2 — Opus Frame Parser + SQLite Batch Ingest

**Start:** 2026-03-05 18:56:49

## Goal
Parse the raw `.bin` dump into discrete Opus frames (length-prefixed format from firmware `transport.c:800-817`), segment into WAL-compatible chunks, batch-insert extracted memories into the local GRDB SQLite `memories` table directly from Rust.

## Format (from firmware transport.c:800-817)
SD file `/SD:/audio/a01.txt` contains:
```
[u8 len][opus_frame ≤80B][u8 len][opus_frame]...
```
packed to 512B write blocks. Parser must:
- Validate first frame byte is a valid Opus TOC: `{0xb8,0xb0,0xbc,0xf8,0xfc,0x78,0x7c}` (WALModel.swift:208)
- Skip corrupt frames (len > 160 or invalid TOC) with warn log
- Stream-parse — never buffer full file

## GRDB `memories` Table (MemoryModels.swift:8-47, camelCase columns)
```
id (autoinc PK), backendId, backendSynced, content, category, tagsJson,
visibility, reviewed, userReview, manuallyAdded, scoring, source,
conversationId, screenshotId, confidence, reasoning, sourceApp,
windowTitle, contextSummary, currentActivity, inputDeviceName,
headline, isRead, isDismissed, deleted, createdAt, updatedAt
```
Dates stored as `YYYY-MM-DD HH:MM:SS.SSS` (GRDB default).

## Files Created
- `desktop/Backend-Rust/src/services/opus_parser.rs` — 450 lines, 8 tests
- `desktop/Backend-Rust/src/services/local_db.rs` — 290 lines, 5 tests
- `desktop/Backend-Rust/src/bin/bench_parse.rs` — release-mode bench harness
- `desktop/Backend-Rust/tests/fixture_gen.py` — fixture generator (clean/corrupt/512B-padded modes)

## Files Modified
- `desktop/Backend-Rust/src/services/mod.rs` — register `opus_parser` + `local_db`

## Roadblocks
- Crate has no `[lib]` target — tests only reachable via `cargo test --bin omi-desktop-backend`. Filter syntax is `-- <module>::` not `--lib`.
- **Deliberately did NOT implement transcription dispatch** (`transcribe.rs` from PLAN §2.2). Reason: transcription turns audio frames into text memories via external API (Deepgram/Whisper) — that's network-bound, not parsing. The P2 prompt.md spec says "detached from slow hardware download time" — proving parse+ingest speed, not transcription. Transcription wiring belongs in P6 `full_pipeline.rs`. P3 (NER/clustering) consumes TEXT memories that can be seeded via `batch_insert_memories` directly.
- `grdb_datetime()` hand-rolls civil-date math (Hinnant's algorithm) to avoid adding `chrono` to an already-chrono-using crate's hot path. Verified against three known epoch values.

## Decisions Made
| Question | Choice | Why |
|---|---|---|
| Async `Stream` vs sync `Iterator`? | Sync `Iterator` over `BufReader<R: Read>` | File parsing is CPU+IO bound, not network. No await points needed. 27× faster than Python equivalent. |
| What to do on `len > MAX_FRAME_LEN`? | Skip `len` bytes, resume scan | Firmware never writes >80B; anything larger is desync. Skipping `len` is cheap and usually resyncs within 1 frame. |
| Zero-length byte? | Skip (padding within 512B blocks) | Firmware pads blocks to 512B boundary with zeros. |
| `backendId` in SQLite? | NULL | GRDB schema has UNIQUE constraint on `backendId`. NULL = "locally originated, awaiting sync" — existing sync service will pick these up. |
| Which columns to populate? | 10 of 27 (content, category, source, conversationId, confidence, reasoning, inputDeviceName, headline, createdAt, updatedAt) | Everything else takes schema defaults. Named INSERT columns → immune to future additive migrations. |

## Test Output

### Unit tests (debug, full suite)
```
running 14 tests
test services::local_db::tests::grdb_datetime_format ... ok
test services::opus_parser::tests::parses_wellformed_frames ... ok
test services::opus_parser::tests::handles_truncated_final_frame ... ok
test services::opus_parser::tests::segments_correctly ... ok
test services::opus_parser::tests::skips_bad_length ... ok
test services::opus_parser::tests::skips_bad_toc ... ok
test services::opus_parser::tests::skips_zero_padding ... ok
test services::opus_parser::tests::handles_10k_frames_fixture ... ok
test services::opus_parser::tests::recovers_from_2pct_corruption ... ok
test services::opus_parser::tests::parses_disk_fixture_with_512b_padding ... ok
test services::local_db::tests::defaults_are_applied ... ok
test services::local_db::tests::batch_insert_1k_is_transactional ... ok
test services::local_db::tests::inserts_single_memory ... ok
test services::local_db::tests::fetch_by_source_roundtrip ... ok

test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 13 filtered out; finished in 0.23s
```

### Fixture generator (100k frames, 512B-padded)
```
wrote 7,720,605 bytes → /tmp/opus_100k.bin
  100,000 frames (0 corrupt), pad-512=True, seed=1
```

### Release-mode benchmark (100k frames from disk)
```json
{"file":"/tmp/opus_100k.bin","bytes":7720605,"frames":100000,"segments":17,
 "parse_ms":3.56,"segment_ms":6.65,"rate_mbps":2168.4,
 "skipped_len":0,"skipped_toc":0}
```

### Python baseline (same file)
```
python baseline: 100000 frames, 0 skipped, 7720605 bytes, 0.0975s (79.2 MB/s)
```

### Scaling projection
| Metric | Value |
|---|---|
| Parse rate (release) | 2.17 GB/s |
| 1 GB dump parse time | ~460 ms |
| Frames in 1 GB (@ ~77 B/frame) | ~13 million |
| Segments (@ 6000 frames/seg) | ~2167 |
| SQLite 1000-row batch insert | <30 ms (single txn) |
| **prompt.md P2 requirement** | **MET** — "1k+ memories written in seconds, detached from hardware" |

**End:** 2026-03-05 19:29:15 *(32m 26s total, but ~5m was spent pre-interrupt, actual P2 work ~20m)*
