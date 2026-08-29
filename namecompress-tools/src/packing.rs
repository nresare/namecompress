//! Compression of the shipped table.
//!
//! The table is front-coded text plus binary frequency data. LZMA does best on
//! it once dictionary entries are in lexicographic order, since the shared
//! prefixes give it long-range structure to exploit.

use std::io::Write;
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Packing {
    None,
    Zstd,
    Xz,
}

impl Packing {
    /// Chooses the format from the output file's extension, so the caller
    /// names the file it wants and gets it.
    pub fn for_path(path: &Path) -> Self {
        match path.extension().and_then(|e| e.to_str()) {
            Some("xz") | Some("lzma") => Self::Xz,
            Some("zst") | Some("zstd") => Self::Zstd,
            _ => Self::None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::None => "uncompressed",
            Self::Zstd => "zstd",
            Self::Xz => "xz",
        }
    }

    pub fn pack(self, bytes: &[u8]) -> std::io::Result<Vec<u8>> {
        match self {
            Self::None => Ok(bytes.to_vec()),
            Self::Zstd => zstd::stream::encode_all(bytes, 19),
            Self::Xz => {
                let mut encoder = xz2::write::XzEncoder::new(Vec::new(), 9);
                encoder.write_all(bytes)?;
                encoder.finish()
            }
        }
    }
}

/// Leading bytes of the compressed formats a table may arrive in.
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];
const XZ_MAGIC: [u8; 6] = [0xfd, b'7', b'z', b'X', b'Z', 0x00];

/// Decompresses a table if it carries a compression header, so a caller can
/// accept whichever form the builder produced.
pub fn unpack(bytes: Vec<u8>) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    if bytes.starts_with(&ZSTD_MAGIC) {
        zstd::stream::decode_all(&bytes[..])
    } else if bytes.starts_with(&XZ_MAGIC) {
        let mut out = Vec::new();
        xz2::read::XzDecoder::new(&bytes[..]).read_to_end(&mut out)?;
        Ok(out)
    } else {
        Ok(bytes)
    }
}

/// Reads a table file in any of the supported forms.
pub fn read_table(path: &Path) -> std::io::Result<namecompress::Table> {
    let bytes = unpack(std::fs::read(path)?)?;
    namecompress::Table::load(&bytes)
        .map_err(|e| std::io::Error::other(format!("{}: {e}", path.display())))
}
