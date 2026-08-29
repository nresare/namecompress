//! Command line encoder and decoder.
//!
//! A filter in the style of `gzip`: a name on standard input becomes
//! compressed bytes on standard output, and `-d` reverses that.

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use namecompress::Table;

/// Leading bytes of the compressed forms a table may arrive in, so the file
/// the builder produced can be given directly.
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];
const XZ_MAGIC: [u8; 6] = [0xfd, b'7', b'z', b'X', b'Z', 0x00];

/// Ceiling on a decompressed table, guarding against a hostile frame.
const MAX_TABLE_BYTES: usize = 256 << 20;

#[derive(Parser)]
#[command(
    name = "namecompress",
    about = "Compress a personal name against a model table",
    long_about = "Reads a name from standard input and writes the compressed \
                  bytes to standard output. With -d the direction is \
                  reversed. The same table must be used on both sides: a \
                  message decoded against a different table yields a \
                  plausible but wrong name, which is reported as an error \
                  rather than printed."
)]
struct Args {
    /// Model table, raw or zstd-compressed
    #[arg(short, long, value_name = "PATH")]
    table: PathBuf,

    /// Decompress rather than compress
    #[arg(short = 'd', long)]
    decompress: bool,

    /// Write compressed bytes even when standard output is a terminal
    #[arg(short, long)]
    force: bool,
}

fn load_table(path: &Path) -> Result<Table, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let bytes = if bytes.starts_with(&ZSTD_MAGIC) {
        zstd::stream::decode_all(&bytes[..])
            .map_err(|e| format!("{}: could not decompress: {e}", path.display()))?
    } else if bytes.starts_with(&XZ_MAGIC) {
        let mut out = Vec::new();
        xz2::read::XzDecoder::new(&bytes[..])
            .read_to_end(&mut out)
            .map_err(|e| format!("{}: could not decompress: {e}", path.display()))?;
        out
    } else {
        bytes
    };
    if bytes.len() > MAX_TABLE_BYTES {
        return Err(format!("{}: table is implausibly large", path.display()));
    }
    Table::load(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

fn read_stdin() -> Result<Vec<u8>, String> {
    let mut buffer = Vec::new();
    std::io::stdin()
        .read_to_end(&mut buffer)
        .map_err(|e| format!("reading standard input: {e}"))?;
    Ok(buffer)
}

fn run(args: Args) -> Result<(), String> {
    let table = load_table(&args.table)?;
    let input = read_stdin()?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    if args.decompress {
        if input.is_empty() {
            return Err("no input on standard input".to_owned());
        }
        let name = namecompress::decompress(&table, &input).map_err(|e| e.to_string())?;
        writeln!(out, "{name}").map_err(|e| format!("writing standard output: {e}"))
    } else {
        // Compressed output is binary; keep it out of a terminal unless asked,
        // the way gzip does.
        if out.is_terminal() && !args.force {
            return Err(
                "compressed output would go to a terminal; redirect it or pass --force".to_owned(),
            );
        }
        let name = std::str::from_utf8(&input)
            .map_err(|_| "input is not valid UTF-8".to_owned())?
            .trim_end_matches(['\n', '\r']);
        if name.is_empty() {
            return Err("no name on standard input".to_owned());
        }
        let packed = namecompress::compress(&table, name).map_err(|e| e.to_string())?;
        out.write_all(&packed)
            .map_err(|e| format!("writing standard output: {e}"))
    }
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("namecompress: {message}");
            ExitCode::FAILURE
        }
    }
}
