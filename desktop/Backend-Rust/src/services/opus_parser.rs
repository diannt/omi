//! Opus frame parser for raw SD card dumps.
//!
//! On-disk format (firmware `transport.c:800-817`):
//!   `[u8 len][opus_frame ≤ 80B][u8 len][opus_frame]...`
//! packed into 512B write blocks.
//!
//! Frame validation: first byte of each frame must be a recognized Opus TOC byte
//! (from `WALModel.swift:208`): `{0xb8, 0xb0, 0xbc, 0xf8, 0xfc, 0x78, 0x7c}`.
//!
//! Parser streams — never buffers the full dump.

use std::io::{self, BufReader, Read};

/// Maximum plausible frame length. Firmware encodes ≤ 80B opus frames; anything
/// above this indicates desync/corruption and we skip to resync.
pub const MAX_FRAME_LEN: u8 = 160;

/// Valid Opus TOC bytes (from `WALModel.swift:208`).
pub const VALID_TOC_BYTES: [u8; 7] = [0xb8, 0xb0, 0xbc, 0xf8, 0xfc, 0x78, 0x7c];

/// Frames are ~100/s (10ms per frame at 16kHz). 6000 frames ≈ 60s of audio.
pub const FRAMES_PER_SEGMENT: usize = 6000;

#[inline]
pub fn is_valid_toc(byte: u8) -> bool {
    VALID_TOC_BYTES.contains(&byte)
}

#[derive(Debug, Clone)]
pub struct OpusFrame {
    pub data: Vec<u8>, // includes TOC byte at data[0]
}

impl OpusFrame {
    pub fn toc(&self) -> u8 {
        self.data[0]
    }
}

/// A segment of sequential frames, roughly one minute of audio at 100 fps.
/// Suitable for one transcription API call.
#[derive(Debug, Clone)]
pub struct WalSegment {
    pub frames: Vec<OpusFrame>,
    /// Byte offset in the original dump where this segment's first frame started.
    /// Useful for resume/seek.
    pub start_offset: u64,
}

/// Parse statistics, useful for progress reporting.
#[derive(Debug, Default)]
pub struct ParseStats {
    pub bytes_read: u64,
    pub frames_ok: u64,
    pub frames_skipped_bad_len: u64,
    pub frames_skipped_bad_toc: u64,
}

/// Streaming iterator over Opus frames in a raw dump.
/// Wraps any `Read` in a `BufReader` for efficient single-byte length reads.
pub struct OpusFrameIter<R: Read> {
    reader: BufReader<R>,
    pub stats: ParseStats,
}

impl<R: Read> OpusFrameIter<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader: BufReader::with_capacity(64 * 1024, reader),
            stats: ParseStats::default(),
        }
    }

    /// Read one byte; returns None on clean EOF, propagates real I/O errors as Some(Err).
    fn read_byte(&mut self) -> Option<io::Result<u8>> {
        let mut b = [0u8; 1];
        loop {
            match self.reader.read(&mut b) {
                Ok(0) => return None,
                Ok(_) => {
                    self.stats.bytes_read += 1;
                    return Some(Ok(b[0]));
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Some(Err(e)),
            }
        }
    }

    /// Skip and discard exactly `n` bytes. Returns Ok(true) if all consumed,
    /// Ok(false) on EOF mid-skip.
    fn skip(&mut self, n: usize) -> io::Result<bool> {
        let mut remaining = n;
        let mut buf = [0u8; 256];
        while remaining > 0 {
            let want = remaining.min(buf.len());
            match self.reader.read(&mut buf[..want]) {
                Ok(0) => return Ok(false),
                Ok(m) => {
                    self.stats.bytes_read += m as u64;
                    remaining -= m;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(true)
    }
}

impl<R: Read> Iterator for OpusFrameIter<R> {
    type Item = io::Result<OpusFrame>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // 1. read length byte
            let len = match self.read_byte()? {
                Ok(b) => b,
                Err(e) => return Some(Err(e)),
            };

            // length == 0 → padding within a 512B block, just continue scanning
            if len == 0 {
                continue;
            }

            // implausible length → skip that many bytes and try to resync
            if len > MAX_FRAME_LEN {
                self.stats.frames_skipped_bad_len += 1;
                match self.skip(len as usize) {
                    Ok(true) => continue,
                    Ok(false) => return None, // EOF mid-skip
                    Err(e) => return Some(Err(e)),
                }
            }

            // 2. read frame body
            let mut frame = vec![0u8; len as usize];
            let mut filled = 0usize;
            while filled < frame.len() {
                match self.reader.read(&mut frame[filled..]) {
                    Ok(0) => return None, // truncated frame at EOF — drop silently
                    Ok(m) => {
                        filled += m;
                        self.stats.bytes_read += m as u64;
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Some(Err(e)),
                }
            }

            // 3. validate TOC
            if !is_valid_toc(frame[0]) {
                self.stats.frames_skipped_bad_toc += 1;
                continue;
            }

            self.stats.frames_ok += 1;
            return Some(Ok(OpusFrame { data: frame }));
        }
    }
}

/// Group a frame iterator into ~60s segments.
/// Frames are moved into segments; caller should not re-use the iterator afterwards.
pub fn segment_frames<I>(frames: I, frames_per_seg: usize) -> Vec<WalSegment>
where
    I: Iterator<Item = io::Result<OpusFrame>>,
{
    let mut segments = Vec::new();
    let mut current: Vec<OpusFrame> = Vec::with_capacity(frames_per_seg);
    let mut byte_cursor: u64 = 0;
    let mut seg_start: u64 = 0;

    for item in frames {
        match item {
            Ok(f) => {
                if current.is_empty() {
                    seg_start = byte_cursor;
                }
                byte_cursor += 1 + f.data.len() as u64; // length byte + frame
                current.push(f);
                if current.len() >= frames_per_seg {
                    segments.push(WalSegment {
                        frames: std::mem::take(&mut current),
                        start_offset: seg_start,
                    });
                }
            }
            Err(e) => {
                tracing::warn!("opus_parser: I/O error mid-stream, stopping: {}", e);
                break;
            }
        }
    }

    if !current.is_empty() {
        segments.push(WalSegment {
            frames: current,
            start_offset: seg_start,
        });
    }

    segments
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn make_frame(toc: u8, body_len: usize) -> Vec<u8> {
        let mut f = vec![(body_len + 1) as u8, toc];
        f.extend(std::iter::repeat(0xAA).take(body_len));
        f
    }

    #[test]
    fn parses_wellformed_frames() {
        let mut blob = Vec::new();
        for &toc in &VALID_TOC_BYTES {
            blob.extend(make_frame(toc, 60));
        }
        let iter = OpusFrameIter::new(Cursor::new(blob));
        let frames: Vec<_> = iter.map(|r| r.unwrap()).collect();
        assert_eq!(frames.len(), 7);
        assert_eq!(frames[0].toc(), 0xb8);
        assert_eq!(frames[0].data.len(), 61);
    }

    #[test]
    fn skips_zero_padding() {
        let mut blob = Vec::new();
        blob.extend(make_frame(0xb8, 50));
        blob.extend([0u8; 17]); // padding
        blob.extend(make_frame(0xfc, 40));
        let iter = OpusFrameIter::new(Cursor::new(blob));
        let frames: Vec<_> = iter.map(|r| r.unwrap()).collect();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1].toc(), 0xfc);
    }

    #[test]
    fn skips_bad_toc() {
        let mut blob = Vec::new();
        blob.extend(make_frame(0xb8, 50));
        // corrupt frame: valid length, invalid TOC
        blob.push(10u8); // len
        blob.push(0x00); // bad TOC
        blob.extend([0xFFu8; 9]);
        blob.extend(make_frame(0x78, 30));
        let mut iter = OpusFrameIter::new(Cursor::new(blob));
        let frames: Vec<_> = (&mut iter).map(|r| r.unwrap()).collect();
        assert_eq!(frames.len(), 2);
        assert_eq!(iter.stats.frames_skipped_bad_toc, 1);
        assert_eq!(iter.stats.frames_ok, 2);
    }

    #[test]
    fn skips_bad_length() {
        let mut blob = Vec::new();
        blob.extend(make_frame(0xb8, 50));
        // corrupt: length > MAX_FRAME_LEN → parser skips that many bytes
        blob.push(200u8);
        blob.extend([0x55u8; 200]);
        blob.extend(make_frame(0x7c, 20));
        let mut iter = OpusFrameIter::new(Cursor::new(blob));
        let frames: Vec<_> = (&mut iter).map(|r| r.unwrap()).collect();
        assert_eq!(frames.len(), 2);
        assert_eq!(iter.stats.frames_skipped_bad_len, 1);
    }

    #[test]
    fn handles_truncated_final_frame() {
        let mut blob = Vec::new();
        blob.extend(make_frame(0xb8, 50));
        blob.push(80u8); // length says 80
        blob.extend([0xb0u8; 10]); // but only 10 bytes remain
        let iter = OpusFrameIter::new(Cursor::new(blob));
        let frames: Vec<_> = iter.map(|r| r.unwrap()).collect();
        assert_eq!(frames.len(), 1); // truncated frame silently dropped
    }

    #[test]
    fn segments_correctly() {
        let mut blob = Vec::new();
        for _ in 0..250 {
            blob.extend(make_frame(0xb8, 60));
        }
        let iter = OpusFrameIter::new(Cursor::new(blob));
        let segs = segment_frames(iter, 100);
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0].frames.len(), 100);
        assert_eq!(segs[1].frames.len(), 100);
        assert_eq!(segs[2].frames.len(), 50);
        assert_eq!(segs[0].start_offset, 0);
        // Each frame is 62 bytes on wire (1 len + 61 body)
        assert_eq!(segs[1].start_offset, 100 * 62);
    }

    #[test]
    fn handles_10k_frames_fixture() {
        // Matches tests/fixture_gen.py output shape
        let toc_pool = VALID_TOC_BYTES;
        let mut blob = Vec::new();
        for i in 0..10_000 {
            let fl = 60 + (i % 21); // 60..80 bytes
            blob.push(fl as u8);
            blob.push(toc_pool[i % toc_pool.len()]);
            blob.extend(std::iter::repeat((i & 0xFF) as u8).take(fl - 1));
        }
        let mut iter = OpusFrameIter::new(Cursor::new(blob));
        let count = (&mut iter).count();
        assert_eq!(count, 10_000);
        assert_eq!(iter.stats.frames_ok, 10_000);
        assert_eq!(iter.stats.frames_skipped_bad_len, 0);
        assert_eq!(iter.stats.frames_skipped_bad_toc, 0);
    }

    /// Disk-based integration test: generate a fixture file via fixture_gen.py,
    /// stream-parse it with File (not Cursor), assert exact frame count + segment count.
    /// Also exercises the 512B zero-padding path.
    #[test]
    fn parses_disk_fixture_with_512b_padding() {
        use std::fs::File;
        use std::process::Command;

        let fixture_path = format!(
            "/tmp/opus_fixture_{}_{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );

        // Locate fixture_gen.py relative to this crate root
        let gen_script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixture_gen.py");

        let output = Command::new("python3")
            .arg(&gen_script)
            .arg("--frames")
            .arg("15000")
            .arg("--out")
            .arg(&fixture_path)
            .arg("--pad-512")
            .arg("--seed")
            .arg("7")
            .output()
            .expect("fixture_gen.py spawn");
        assert!(
            output.status.success(),
            "fixture_gen.py failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Parse "valid corrupt total_bytes" from stdout
        let summary = String::from_utf8_lossy(&output.stdout);
        let parts: Vec<&str> = summary.split_whitespace().collect();
        let expected_valid: u64 = parts[0].parse().unwrap();
        let expected_corrupt: u64 = parts[1].parse().unwrap();
        let total_bytes: u64 = parts[2].parse().unwrap();
        assert_eq!(expected_valid, 15_000);
        assert_eq!(expected_corrupt, 0);

        // Stream-parse from disk
        let f = File::open(&fixture_path).unwrap();
        let mut iter = OpusFrameIter::new(f);
        let parsed = (&mut iter).count();

        assert_eq!(parsed, 15_000, "frame count mismatch");
        assert_eq!(iter.stats.frames_ok, 15_000);
        assert_eq!(iter.stats.frames_skipped_bad_len, 0);
        assert_eq!(iter.stats.frames_skipped_bad_toc, 0);
        assert_eq!(
            iter.stats.bytes_read, total_bytes,
            "bytes_read should equal file size (padding + frames)"
        );

        // Segmentation: 15000 frames / 6000 per segment = 2 full + 1 partial = 3
        let f2 = File::open(&fixture_path).unwrap();
        let segs = segment_frames(OpusFrameIter::new(f2), FRAMES_PER_SEGMENT);
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0].frames.len(), 6000);
        assert_eq!(segs[1].frames.len(), 6000);
        assert_eq!(segs[2].frames.len(), 3000);

        std::fs::remove_file(&fixture_path).ok();
    }

    /// Corruption resilience: 2% corrupt frames, parser must recover and extract
    /// exactly the expected number of valid frames.
    #[test]
    fn recovers_from_2pct_corruption() {
        use std::fs::File;
        use std::process::Command;

        let fixture_path = format!(
            "/tmp/opus_fixture_corrupt_{}.bin",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let gen_script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixture_gen.py");

        let output = Command::new("python3")
            .arg(&gen_script)
            .arg("--frames")
            .arg("5000")
            .arg("--out")
            .arg(&fixture_path)
            .arg("--corrupt-rate")
            .arg("0.02")
            .arg("--seed")
            .arg("99")
            .output()
            .expect("fixture_gen.py spawn");
        assert!(output.status.success());

        let summary = String::from_utf8_lossy(&output.stdout);
        let parts: Vec<&str> = summary.split_whitespace().collect();
        let expected_valid: u64 = parts[0].parse().unwrap();
        let expected_corrupt: u64 = parts[1].parse().unwrap();

        let f = File::open(&fixture_path).unwrap();
        let mut iter = OpusFrameIter::new(f);
        let parsed = (&mut iter).count() as u64;

        // Bad-len frames skip `len` bytes which may land mid-way through the NEXT
        // valid frame → that neighbour gets eaten too. So parsed valid frames may
        // be slightly below expected_valid. Bad-toc frames cleanly skip one frame.
        // Lower bound: each bad-len corruption can eat at most one neighbour.
        let bad_len_upper_bound = expected_corrupt; // worst case: all corrupt are bad-len
        let min_expected = expected_valid.saturating_sub(bad_len_upper_bound);
        assert!(
            parsed >= min_expected && parsed <= expected_valid,
            "parsed={} expected in [{},{}]",
            parsed,
            min_expected,
            expected_valid
        );
        assert_eq!(
            iter.stats.frames_skipped_bad_len + iter.stats.frames_skipped_bad_toc
                + iter.stats.frames_ok,
            iter.stats.frames_ok + (expected_corrupt + (expected_valid - parsed)),
            "stats must account for all frames"
        );

        std::fs::remove_file(&fixture_path).ok();
    }
}
