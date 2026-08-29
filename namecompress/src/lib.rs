//! Compression of personal names against a separately distributed model
//! table.
//!
//! A name is coded as a draw from a known distribution rather than as a
//! string, which is why the result is a few bytes where general-purpose
//! compressors expand the input. The model lives in a [`Table`] that is built
//! offline from a corpus and shipped alongside the encoder and decoder.
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use namecompress::{Table, TableBuilder, chars::CharModelBuilder};
//!
//! // In practice the table is built offline and loaded with
//! // `Table::parse(&std::fs::read("table.ncmp")?)`.
//! let alphabet = namecompress::chars::Alphabet::new("abcdefghijklmnopqrstuvwxyz -'".chars().collect())
//!     .expect("valid alphabet");
//! let mut chars = CharModelBuilder::new(alphabet.symbols());
//! chars.train(&alphabet.encode("smith").unwrap(), 100);
//! let table = TableBuilder {
//!     given: vec![("John".into(), 500)],
//!     given_escape: 100,
//!     surname: vec![("Smith".into(), 400)],
//!     surname_escape: 100,
//!     alphabet,
//!     chars,
//!     prune: 0,
//!     check_modulus: 256,
//! }
//! .finish();
//!
//! let packed = namecompress::compress(&table, "John Smith")?;
//! assert_eq!(namecompress::decompress(&table, &packed)?, "John Smith");
//! # Ok(())
//! # }
//! ```

pub mod chars;
pub mod codec;
pub mod range;
pub mod table;
pub mod varint;

pub use codec::{Error, compress, decompress};
pub use table::{Table, TableBuilder};

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

pub(crate) fn fingerprint_seed(id: u32) -> u64 {
    fingerprint_update(FNV_OFFSET, &id.to_le_bytes())
}

pub(crate) fn fingerprint_update(mut hash: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// A 32-bit fingerprint, used to identify a table.
pub fn fingerprint(bytes: &[u8]) -> u32 {
    let hash = fingerprint_update(FNV_OFFSET, bytes);
    (hash ^ (hash >> 32)) as u32
}
