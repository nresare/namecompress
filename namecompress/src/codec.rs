//! Message coding.
//!
//! Decode order is: mode, then either the two name fields or a raw byte
//! string, then a verification symbol. Encoding tries both modes and emits
//! whichever is shorter, so the raw path bounds the worst case at roughly the
//! input length regardless of how strange the name is.

use crate::chars;
use crate::range::{Decoder, Encoder};
use crate::table::{Dictionary, Table};

/// Total for the small static distributions below.
const STATIC_SCALE: u32 = 1 << 16;

/// Mode symbol: a modelled name pair, or raw UTF-8.
const MODE_PAIR: u32 = 0;
const MODE_RAW_START: u32 = 64_500;
const MODE_TOTAL: u32 = STATIC_SCALE;

/// Case shapes, applied to the canonical dictionary spelling.
const SHAPE_AS_IS: u32 = 0;
const SHAPE_LOWER: u32 = 1;
const SHAPE_UPPER: u32 = 2;
const SHAPE_TITLE: u32 = 3;
const SHAPE_FREQS: [u32; 4] = [61_000, 2_000, 1_000, 1_536];

/// Longest name a raw-mode message can carry.
const MAX_RAW_LEN: usize = 255;
/// Guard against a corrupt stream producing an unbounded character run.
const MAX_FIELD_CHARS: usize = 64;

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// The name is longer than the format can represent.
    TooLong,
    /// The message is not decodable against this table. Most often this means
    /// the wrong table was used.
    Malformed,
    /// The verification symbol did not match, so the message was almost
    /// certainly coded against a different table.
    WrongTable,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLong => write!(f, "name too long"),
            Self::Malformed => write!(f, "malformed message"),
            Self::WrongTable => write!(f, "message was coded against a different table"),
        }
    }
}

impl std::error::Error for Error {}

/// Capitalises the first letter of each alphabetic run, lowercasing the rest.
/// This is the shape that covers hyphenated and apostrophed names.
pub fn title_runs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut starting = true;
    for c in s.chars() {
        if c.is_alphabetic() {
            if starting {
                out.extend(c.to_uppercase());
            } else {
                out.extend(c.to_lowercase());
            }
            starting = false;
        } else {
            out.push(c);
            starting = true;
        }
    }
    out
}

/// Finds the shape that turns `canonical` into `actual`, if one exists.
pub fn derive_shape(canonical: &str, actual: &str) -> Option<u32> {
    if actual == canonical {
        Some(SHAPE_AS_IS)
    } else if actual == canonical.to_lowercase() {
        Some(SHAPE_LOWER)
    } else if actual == canonical.to_uppercase() {
        Some(SHAPE_UPPER)
    } else if actual == title_runs(canonical) {
        Some(SHAPE_TITLE)
    } else {
        None
    }
}

fn apply_shape(canonical: &str, shape: u32) -> Option<String> {
    match shape {
        SHAPE_AS_IS => Some(canonical.to_owned()),
        SHAPE_LOWER => Some(canonical.to_lowercase()),
        SHAPE_UPPER => Some(canonical.to_uppercase()),
        SHAPE_TITLE => Some(title_runs(canonical)),
        _ => None,
    }
}

fn shape_range(shape: u32) -> (u32, u32) {
    let start: u32 = SHAPE_FREQS[..shape as usize].iter().sum();
    (start, SHAPE_FREQS[shape as usize])
}

fn shape_for_target(target: u32) -> Option<u32> {
    let mut start = 0;
    for (i, &f) in SHAPE_FREQS.iter().enumerate() {
        if target < start + f {
            return Some(i as u32);
        }
        start += f;
    }
    None
}

/// Codes one name field: a dictionary symbol, an escaped character string if
/// the name is not in the dictionary, then the case shape.
fn encode_field(
    encoder: &mut Encoder,
    dictionary: &Dictionary,
    table: &Table,
    name: &str,
) -> Option<()> {
    match dictionary.lookup_folded(name) {
        Some(symbol) => {
            let canonical = dictionary.name(symbol)?;
            let shape = derive_shape(canonical, name)?;
            let (start, size) = dictionary.range(symbol);
            encoder.encode(start, size, dictionary.total());
            let (start, size) = shape_range(shape);
            encoder.encode(start, size, STATIC_SCALE);
        }
        None => {
            let folded = name.to_lowercase();
            let shape = derive_shape(&folded, name)?;
            let symbols = table.alphabet.encode(&folded)?;
            if symbols.len() > MAX_FIELD_CHARS {
                return None;
            }
            let (start, size) = dictionary.range(dictionary.escape_symbol());
            encoder.encode(start, size, dictionary.total());

            let mut history: Vec<u8> = Vec::with_capacity(symbols.len());
            for &symbol in &symbols {
                let distribution = table.chars.distribution(&history);
                let start: u32 = distribution[..symbol as usize].iter().sum();
                encoder.encode(start, distribution[symbol as usize], chars::SCALE);
                history.push(symbol);
            }
            let (start, size) = shape_range(shape);
            encoder.encode(start, size, STATIC_SCALE);
        }
    }
    Some(())
}

fn decode_field(
    decoder: &mut Decoder,
    dictionary: &Dictionary,
    table: &Table,
) -> Result<String, Error> {
    let symbol = dictionary.symbol_for(decoder.target(dictionary.total()));
    let (start, size) = dictionary.range(symbol);
    decoder.advance(start, size, dictionary.total());

    let canonical = if symbol == dictionary.escape_symbol() {
        let mut history: Vec<u8> = Vec::new();
        loop {
            if history.len() > MAX_FIELD_CHARS {
                return Err(Error::Malformed);
            }
            let distribution = table.chars.distribution(&history);
            let target = decoder.target(chars::SCALE);
            let mut start = 0u32;
            let mut symbol = None;
            for (i, &f) in distribution.iter().enumerate() {
                if target < start + f {
                    symbol = Some((i as u8, start, f));
                    break;
                }
                start += f;
            }
            let (symbol, start, size) = symbol.ok_or(Error::Malformed)?;
            decoder.advance(start, size, chars::SCALE);
            if symbol == table.alphabet.terminator() {
                break;
            }
            history.push(symbol);
        }
        table.alphabet.decode(&history).ok_or(Error::Malformed)?
    } else {
        dictionary.name(symbol).ok_or(Error::Malformed)?.to_owned()
    };

    let shape = shape_for_target(decoder.target(STATIC_SCALE)).ok_or(Error::Malformed)?;
    let (start, size) = shape_range(shape);
    decoder.advance(start, size, STATIC_SCALE);
    apply_shape(&canonical, shape).ok_or(Error::Malformed)
}

/// The verification symbol, tying a message to the table that produced it.
fn check_value(table: &Table, name: &str) -> u32 {
    let mut hash = crate::fingerprint_seed(table.id);
    hash = crate::fingerprint_update(hash, name.as_bytes());
    (hash % u64::from(table.check_modulus)) as u32
}

fn encode_check(encoder: &mut Encoder, table: &Table, name: &str) {
    let value = check_value(table, name);
    encoder.encode(value, 1, table.check_modulus);
}

/// Encodes `name` as a modelled pair, or `None` if the model cannot represent
/// it and the raw fallback must be used instead.
fn compress_modelled(table: &Table, name: &str) -> Option<Vec<u8>> {
    let split = name.rfind(' ')?;
    let (given, surname) = (&name[..split], &name[split + 1..]);
    if given.is_empty() || surname.is_empty() {
        return None;
    }
    let mut encoder = Encoder::new();
    encoder.encode(MODE_PAIR, MODE_RAW_START, MODE_TOTAL);
    encode_field(&mut encoder, &table.given, table, given)?;
    encode_field(&mut encoder, &table.surname, table, surname)?;
    encode_check(&mut encoder, table, name);
    Some(encoder.finish())
}

fn compress_raw(table: &Table, name: &str) -> Result<Vec<u8>, Error> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_RAW_LEN {
        return Err(Error::TooLong);
    }
    let mut encoder = Encoder::new();
    encoder.encode(MODE_RAW_START, MODE_TOTAL - MODE_RAW_START, MODE_TOTAL);
    encoder.encode(bytes.len() as u32 - 1, 1, MAX_RAW_LEN as u32);
    for &byte in bytes {
        encoder.encode(u32::from(byte), 1, 256);
    }
    encode_check(&mut encoder, table, name);
    Ok(encoder.finish())
}

/// Compresses a name, choosing whichever of the modelled and raw encodings is
/// shorter.
pub fn compress(table: &Table, name: &str) -> Result<Vec<u8>, Error> {
    let raw = compress_raw(table, name)?;
    Ok(match compress_modelled(table, name) {
        Some(modelled) if modelled.len() <= raw.len() => modelled,
        _ => raw,
    })
}

pub fn decompress(table: &Table, bytes: &[u8]) -> Result<String, Error> {
    let mut decoder = Decoder::new(bytes);
    let target = decoder.target(MODE_TOTAL);
    let name = if target < MODE_RAW_START {
        decoder.advance(MODE_PAIR, MODE_RAW_START, MODE_TOTAL);
        let given = decode_field(&mut decoder, &table.given, table)?;
        let surname = decode_field(&mut decoder, &table.surname, table)?;
        format!("{given} {surname}")
    } else {
        decoder.advance(MODE_RAW_START, MODE_TOTAL - MODE_RAW_START, MODE_TOTAL);
        let length = decoder.target(MAX_RAW_LEN as u32);
        decoder.advance(length, 1, MAX_RAW_LEN as u32);
        let mut out = Vec::with_capacity(length as usize + 1);
        for _ in 0..length + 1 {
            let byte = decoder.target(256);
            decoder.advance(byte, 1, 256);
            out.push(byte as u8);
        }
        String::from_utf8(out).map_err(|_| Error::Malformed)?
    };

    let expected = check_value(table, &name);
    let found = decoder.target(table.check_modulus);
    if found != expected {
        return Err(Error::WrongTable);
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use crate::chars::CharModelBuilder;
    use crate::table::{Table, TableBuilder};

    fn table() -> Table {
        let alphabet =
            crate::chars::Alphabet::new("abcdefghijklmnopqrstuvwxyzåäö -'".chars().collect())
                .expect("valid alphabet");
        let mut builder = CharModelBuilder::new(alphabet.symbols());
        for name in [
            "smith",
            "jones",
            "brown",
            "o'brien",
            "anna-karin",
            "nkemdirim",
        ] {
            builder.train(&alphabet.encode(name).expect("in alphabet"), 100);
        }
        let chars = builder.build(0);
        TableBuilder {
            given: vec![
                ("John".into(), 5000),
                ("Sarah".into(), 3000),
                ("Anna-Karin".into(), 100),
            ],
            given_escape: 900,
            surname: vec![("Smith".into(), 4000), ("Jones".into(), 2000)],
            surname_escape: 900,
            alphabet,
            chars,
            check_modulus: 256,
        }
        .finish()
    }

    #[track_caller]
    fn assert_round_trips(table: &Table, name: &str) {
        let packed = super::compress(table, name).expect("compresses");
        let out = super::decompress(table, &packed);
        assert_eq!(
            out.as_deref(),
            Ok(name),
            "{name:?} -> {} bytes -> {out:?}",
            packed.len()
        );
    }

    #[test]
    fn round_trips_dictionary_hits() {
        let table = table();
        for name in ["John Smith", "Sarah Jones", "Anna-Karin Smith"] {
            assert_round_trips(&table, name);
        }
    }

    #[test]
    fn round_trips_case_variants() {
        let table = table();
        for name in ["JOHN SMITH", "john smith", "John SMITH", "john Smith"] {
            assert_round_trips(&table, name);
        }
    }

    #[test]
    fn round_trips_escaped_names() {
        let table = table();
        for name in ["Nkemdirim Okonkwo", "Zzzz Qqqq", "John Nkemdirim"] {
            assert_round_trips(&table, name);
        }
    }

    #[test]
    fn round_trips_raw_fallback() {
        let table = table();
        // Non-alphabet characters and shapes no rule can produce.
        for name in ["José Müller", "McDonald McSmith", "Ω Ψ", "Plato"] {
            assert_round_trips(&table, name);
        }
    }
}
