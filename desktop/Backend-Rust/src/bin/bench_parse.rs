//! One-shot: parse a .bin fixture, report rate. Not a production bin — bench harness only.
use std::fs::File;
use std::time::Instant;

#[path = "../services/opus_parser.rs"]
mod opus_parser;
use opus_parser::{segment_frames, OpusFrameIter, FRAMES_PER_SEGMENT};

fn main() {
    let path = std::env::args().nth(1).expect("usage: bench_parse <file.bin>");
    let meta = std::fs::metadata(&path).expect("stat");
    let size = meta.len();

    // Pass 1: count frames
    let t0 = Instant::now();
    let f = File::open(&path).expect("open");
    let mut iter = OpusFrameIter::new(f);
    let n = (&mut iter).count();
    let dt = t0.elapsed();

    // Pass 2: segment
    let t1 = Instant::now();
    let f2 = File::open(&path).expect("open");
    let segs = segment_frames(OpusFrameIter::new(f2), FRAMES_PER_SEGMENT);
    let dt2 = t1.elapsed();

    eprintln!("{{\"file\":\"{}\",\"bytes\":{},\"frames\":{},\"segments\":{},\"parse_ms\":{:.2},\"segment_ms\":{:.2},\"rate_mbps\":{:.1},\"skipped_len\":{},\"skipped_toc\":{}}}",
        path, size, n, segs.len(),
        dt.as_secs_f64()*1000.0, dt2.as_secs_f64()*1000.0,
        size as f64 / dt.as_secs_f64() / 1e6,
        iter.stats.frames_skipped_bad_len, iter.stats.frames_skipped_bad_toc);
}
