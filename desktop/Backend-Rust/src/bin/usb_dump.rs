//! USB CDC serial dump client for the Omi device.
//!
//! Opens the device's CDC-ACM port, sends the 6-byte storage READ command
//! (same protocol as BLE storage.c), and streams the raw SD card file to disk
//! without buffering the whole file in memory.
//!
//! Protocol:
//!   Host → Device (6 B):  [0x00=READ][0x01=file][offset BE u32]
//!   Device → Host (1 B):  [status: 0=OK, 3=INVALID_FILE_SIZE, 4=ZERO_FILE_SIZE, 6=INVALID_COMMAND]
//!   Device → Host (4 B):  [file_size LE u32]  (only if status==0)
//!   Device → Host:        [raw stream, file_size-offset bytes]
//!   Device → Host (1 B):  [0x64 EOT]

use clap::Parser;
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

// Protocol constants (mirror firmware storage.c / usb_dump.c)
const READ_COMMAND: u8 = 0x00;
const FILE_NUM: u8 = 0x01;
const STATUS_OK: u8 = 0x00;
const STATUS_INVALID_FILE_SIZE: u8 = 0x03;
const STATUS_ZERO_FILE_SIZE: u8 = 0x04;
const STATUS_INVALID_COMMAND: u8 = 0x06;
const EOT_MARKER: u8 = 0x64;

const CHUNK_LEN: usize = 4096;

#[derive(Parser, Debug)]
#[command(name = "usb_dump", about = "Dump Omi SD card over USB CDC serial")]
struct Args {
    /// Serial port path, e.g. /dev/ttyACM0 (Linux) or /dev/tty.usbmodem* (macOS)
    #[arg(long)]
    port: String,

    /// Baud rate (CDC-ACM ignores this, but serialport crate requires a value)
    #[arg(long, default_value_t = 921600)]
    baud: u32,

    /// Output file path
    #[arg(long)]
    out: PathBuf,

    /// Byte offset within the SD file to start reading from
    #[arg(long, default_value_t = 0)]
    offset: u32,

    /// Read timeout in seconds
    #[arg(long, default_value_t = 10)]
    timeout_secs: u64,
}

/// JSON progress line printed to stdout for machine consumption (Phase 6).
#[derive(serde::Serialize)]
struct Progress {
    bytes: u64,
    total: u64,
    rate_bps: u64,
    elapsed_secs: f64,
}

fn read_exact(port: &mut dyn Read, buf: &mut [u8]) -> io::Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        match port.read(&mut buf[filled..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("device closed connection after {} bytes", filled),
                ))
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("timeout after reading {} of {} bytes", filled, buf.len()),
                ))
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn status_name(code: u8) -> &'static str {
    match code {
        STATUS_OK => "OK",
        STATUS_INVALID_FILE_SIZE => "INVALID_FILE_SIZE (offset >= file size)",
        STATUS_ZERO_FILE_SIZE => "ZERO_FILE_SIZE (no data on SD card)",
        STATUS_INVALID_COMMAND => "INVALID_COMMAND",
        _ => "UNKNOWN",
    }
}

fn main() -> io::Result<()> {
    let args = Args::parse();

    eprintln!("[usb_dump] opening {} @ {} baud", args.port, args.baud);
    let mut port = serialport::new(&args.port, args.baud)
        .timeout(Duration::from_secs(args.timeout_secs))
        .open()
        .map_err(|e| io::Error::new(io::ErrorKind::NotFound, format!("open {}: {}", args.port, e)))?;

    // Assemble 6-byte command: [READ][FILE=1][offset BE u32]
    let off = args.offset;
    let cmd: [u8; 6] = [
        READ_COMMAND,
        FILE_NUM,
        ((off >> 24) & 0xFF) as u8,
        ((off >> 16) & 0xFF) as u8,
        ((off >> 8) & 0xFF) as u8,
        (off & 0xFF) as u8,
    ];
    eprintln!("[usb_dump] sending READ cmd, offset={}", off);
    port.write_all(&cmd)?;
    port.flush()?;

    // Read 1-byte status
    let mut status = [0u8; 1];
    read_exact(&mut *port, &mut status)?;
    if status[0] != STATUS_OK {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("device status 0x{:02x}: {}", status[0], status_name(status[0])),
        ));
    }

    // Read 4-byte LE file size
    let mut size_buf = [0u8; 4];
    read_exact(&mut *port, &mut size_buf)?;
    let file_size = u32::from_le_bytes(size_buf) as u64;
    let to_receive = file_size.saturating_sub(off as u64);
    eprintln!(
        "[usb_dump] file_size={} bytes, receiving {} bytes from offset {}",
        file_size, to_receive, off
    );

    // Stream to disk — never hold full file in memory
    let out_file = File::create(&args.out)?;
    let mut writer = BufWriter::with_capacity(CHUNK_LEN, out_file);

    let start = Instant::now();
    let mut received: u64 = 0;
    let mut chunk = vec![0u8; CHUNK_LEN];
    let mut last_report = Instant::now();

    while received < to_receive {
        let want = std::cmp::min(CHUNK_LEN as u64, to_receive - received) as usize;
        match port.read(&mut chunk[..want]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("stream ended at {} / {} bytes", received, to_receive),
                ))
            }
            Ok(n) => {
                writer.write_all(&chunk[..n])?;
                received += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("read timeout at {} / {} bytes", received, to_receive),
                ))
            }
            Err(e) => return Err(e),
        }

        // Report progress ~once per second on stderr + JSON line on stdout
        if last_report.elapsed() >= Duration::from_secs(1) {
            let elapsed = start.elapsed().as_secs_f64();
            let rate = if elapsed > 0.0 {
                (received as f64 / elapsed) as u64
            } else {
                0
            };
            eprintln!(
                "[usb_dump] {} / {} bytes ({:.1}%) @ {:.2} MB/s",
                received,
                to_receive,
                (received as f64 / to_receive as f64) * 100.0,
                rate as f64 / 1_000_000.0
            );
            let p = Progress {
                bytes: received,
                total: to_receive,
                rate_bps: rate,
                elapsed_secs: elapsed,
            };
            println!("{}", serde_json::to_string(&p).unwrap());
            last_report = Instant::now();
        }
    }

    writer.flush()?;

    // Read and verify EOT marker
    let mut eot = [0u8; 1];
    match read_exact(&mut *port, &mut eot) {
        Ok(()) if eot[0] == EOT_MARKER => {
            eprintln!("[usb_dump] EOT verified");
        }
        Ok(()) => {
            eprintln!("[usb_dump] WARNING: expected EOT 0x64, got 0x{:02x}", eot[0]);
        }
        Err(e) => {
            eprintln!("[usb_dump] WARNING: failed to read EOT byte: {}", e);
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    let rate = if elapsed > 0.0 {
        (received as f64 / elapsed) as u64
    } else {
        0
    };
    eprintln!(
        "[usb_dump] done: {} bytes → {} in {:.2}s @ {:.2} MB/s",
        received,
        args.out.display(),
        elapsed,
        rate as f64 / 1_000_000.0
    );
    let p = Progress {
        bytes: received,
        total: to_receive,
        rate_bps: rate,
        elapsed_secs: elapsed,
    };
    println!("{}", serde_json::to_string(&p).unwrap());

    Ok(())
}
