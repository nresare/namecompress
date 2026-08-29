//! The separately distributed model table.

use std::collections::HashMap;

use crate::chars::{Alphabet, CharModel};
use crate::varint;

const MAGIC: &[u8; 4] = b"NCMP";
const VERSION: u16 = 2;

/// Smallest denominator used for dictionary frequencies. Larger dictionaries
/// scale up from here, since every symbol needs a frequency of at least one
/// and the rare tail would otherwise consume the whole scale.
const MIN_FIELD_SCALE: u32 = 1 << 20;

/// Headroom above the symbol count, so common names keep useful precision
/// rather than being crowded out by the rare tail.
const SCALE_HEADROOM: u32 = 16;

/// A dictionary of names plus the escape symbol that follows them.
pub struct Dictionary {
    names: Vec<String>,
    /// Denominator these frequencies are expressed against.
    scale: u32,
    /// `cumulative[i]` is the start of symbol `i`; the last entry is
    /// [`FIELD_SCALE`]. Symbol `names.len()` is the escape.
    cumulative: Vec<u32>,
    /// Case-folded lookup from a name to its canonical dictionary entry.
    folded: HashMap<String, u32>,
}

impl Dictionary {
    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    pub fn escape_symbol(&self) -> u32 {
        self.names.len() as u32
    }

    pub fn name(&self, symbol: u32) -> Option<&str> {
        self.names.get(symbol as usize).map(String::as_str)
    }

    /// The canonical entry whose case-folded form matches `name`.
    pub fn lookup_folded(&self, name: &str) -> Option<u32> {
        self.folded.get(&name.to_lowercase()).copied()
    }

    /// Denominator for this dictionary's frequencies.
    pub fn total(&self) -> u32 {
        self.scale
    }

    pub fn range(&self, symbol: u32) -> (u32, u32) {
        let start = self.cumulative[symbol as usize];
        (start, self.cumulative[symbol as usize + 1] - start)
    }

    /// Maps a coder target back to a symbol.
    pub fn symbol_for(&self, target: u32) -> u32 {
        // partition_point gives the first index whose start exceeds target.
        (self.cumulative.partition_point(|&c| c <= target) - 1) as u32
    }

    fn build(entries: &[(String, u64)], escape_weight: u64) -> Self {
        // Lexicographic order, so front-coding in `write` has shared prefixes
        // to exploit. Symbol indices are internal, so any order will do.
        let mut entries = entries.to_vec();
        entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let entries = &entries[..];

        let symbols = entries.len() as u32 + 1;
        let scale = field_scale(symbols);
        let total: u64 = entries.iter().map(|&(_, c)| c).sum::<u64>() + escape_weight;

        // Give every symbol one unit first, then share out what is left in
        // proportion to the counts. Built this way the total can never exceed
        // the scale, however long the rare tail is.
        let spare = u64::from(scale - symbols);
        let share =
            |count: u64| -> u32 { 1 + (count * spare).checked_div(total).unwrap_or(0) as u32 };
        let mut freqs: Vec<u32> = entries.iter().map(|&(_, c)| share(c)).collect();
        freqs.push(share(escape_weight));

        // Rounding leaves the total a little short; give the remainder to the
        // largest entry, which has the most room to absorb it.
        let sum: u32 = freqs.iter().sum();
        debug_assert!(sum <= scale, "frequencies exceeded the scale");
        let largest = freqs
            .iter()
            .enumerate()
            .max_by_key(|&(_, &f)| f)
            .map(|(i, _)| i)
            .expect("escape symbol always present");
        freqs[largest] += scale - sum;

        let mut cumulative = Vec::with_capacity(freqs.len() + 1);
        let mut running = 0;
        for f in &freqs {
            cumulative.push(running);
            running += f;
        }
        cumulative.push(running);

        let names: Vec<String> = entries.iter().map(|(n, _)| n.clone()).collect();
        let folded = names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.to_lowercase(), i as u32))
            .collect();

        Self {
            names,
            scale,
            cumulative,
            folded,
        }
    }

    fn write(&self, out: &mut Vec<u8>) {
        varint::push(out, u64::from(self.scale));
        varint::push(out, self.names.len() as u64);
        // Front-coded names: shared prefix length, then the differing suffix.
        let mut blob = Vec::new();
        let mut previous = "";
        for name in &self.names {
            let mut shared = name
                .bytes()
                .zip(previous.bytes())
                .take_while(|(a, b)| a == b)
                .count()
                .min(255);
            // A byte-wise common prefix can end inside a multi-byte
            // character; back up to a boundary so both halves stay valid
            // UTF-8.
            while shared > 0 && !name.is_char_boundary(shared) {
                shared -= 1;
            }
            blob.push(shared as u8);
            blob.extend_from_slice(&name.as_bytes()[shared..]);
            blob.push(0);
            previous = name;
        }
        varint::push(out, blob.len() as u64);
        out.extend_from_slice(&blob);
        for i in 0..self.cumulative.len() - 1 {
            varint::push(out, u64::from(self.cumulative[i + 1] - self.cumulative[i]));
        }
    }

    fn parse(bytes: &[u8], cursor: &mut usize) -> Result<Self, &'static str> {
        let scale = u32::try_from(varint::read(bytes, cursor).ok_or("frequency scale")?)
            .map_err(|_| "frequency scale out of range")?;
        if scale == 0 || scale > crate::range::MAX_TOTAL {
            return Err("frequency scale out of range");
        }
        let count = varint::read(bytes, cursor).ok_or("dictionary count")? as usize;
        let blob_len = varint::read(bytes, cursor).ok_or("blob length")? as usize;
        let blob = bytes
            .get(*cursor..*cursor + blob_len)
            .ok_or("blob truncated")?;
        *cursor += blob_len;

        let mut names = Vec::with_capacity(count);
        let mut position = 0usize;
        let mut previous = String::new();
        for _ in 0..count {
            let shared = *blob.get(position).ok_or("name prefix")? as usize;
            position += 1;
            let end = blob[position..]
                .iter()
                .position(|&b| b == 0)
                .ok_or("unterminated name")?
                + position;
            let suffix = std::str::from_utf8(&blob[position..end]).map_err(|_| "name not UTF-8")?;
            if shared > previous.len() || !previous.is_char_boundary(shared) {
                return Err("prefix not on a character boundary");
            }
            let mut name = String::with_capacity(shared + suffix.len());
            name.push_str(&previous[..shared]);
            name.push_str(suffix);
            position = end + 1;
            previous = name.clone();
            names.push(name);
        }

        let mut cumulative = Vec::with_capacity(count + 2);
        let mut running = 0u32;
        for _ in 0..count + 1 {
            cumulative.push(running);
            let delta = varint::read(bytes, cursor).ok_or("frequency")? as u32;
            running = running.checked_add(delta).ok_or("frequency overflow")?;
        }
        cumulative.push(running);
        if running != scale {
            return Err("frequencies do not sum to the field scale");
        }

        let folded = names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.to_lowercase(), i as u32))
            .collect();
        Ok(Self {
            names,
            scale,
            cumulative,
            folded,
        })
    }
}

pub struct Table {
    pub given: Dictionary,
    pub surname: Dictionary,
    /// The characters this table's model covers. Names using anything else
    /// take the raw fallback.
    pub alphabet: Alphabet,
    pub chars: CharModel,
    /// Modulus of the verification symbol. Larger values cost
    /// `log2(modulus)` bits per message and reduce the chance that a message
    /// coded against a different table decodes to plausible output.
    pub check_modulus: u32,
    /// Fingerprint of this table, mixed into the verification symbol.
    pub id: u32,
}

impl Table {
    /// Parses a table, or `None` if the bytes are not a table this build
    /// understands. Use [`Table::load`] when the reason matters.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        Self::load(bytes).ok()
    }

    /// Parses a table, reporting which part of the format was rejected.
    pub fn load(bytes: &[u8]) -> Result<Self, &'static str> {
        let read = |n: usize, cursor: &mut usize| -> Result<&[u8], &'static str> {
            let slice = bytes.get(*cursor..*cursor + n).ok_or("header truncated")?;
            *cursor += n;
            Ok(slice)
        };
        let mut cursor = 0usize;
        if read(4, &mut cursor)? != MAGIC {
            return Err("not a namecompress table");
        }
        let version = u16::from_le_bytes(read(2, &mut cursor)?.try_into().expect("two bytes"));
        if version != VERSION {
            return Err("unsupported table version");
        }
        let check_modulus = u32::from(u16::from_le_bytes(
            read(2, &mut cursor)?.try_into().expect("two bytes"),
        ));
        let id = u32::from_le_bytes(read(4, &mut cursor)?.try_into().expect("four bytes"));
        if check_modulus == 0 {
            return Err("check modulus must be non-zero");
        }

        // The alphabet precedes the model, which needs its size to parse.
        let alphabet = Alphabet::parse(bytes, &mut cursor).ok_or("alphabet")?;
        let given = Dictionary::parse(bytes, &mut cursor)?;
        let surname = Dictionary::parse(bytes, &mut cursor)?;
        let chars =
            CharModel::parse(bytes, &mut cursor, alphabet.symbols()).ok_or("character model")?;
        Ok(Self {
            given,
            surname,
            alphabet,
            chars,
            check_modulus,
            id,
        })
    }

    pub fn write(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&(self.check_modulus as u16).to_le_bytes());
        out.extend_from_slice(&self.id.to_le_bytes());
        self.alphabet.write(&mut out);
        self.given.write(&mut out);
        self.surname.write(&mut out);
        self.chars.write(&mut out);
        out
    }
}

/// The denominator to express `symbols` frequencies against: enough headroom
/// that common names keep precision, capped at what the coder accepts.
fn field_scale(symbols: u32) -> u32 {
    let wanted = symbols.saturating_mul(SCALE_HEADROOM).max(MIN_FIELD_SCALE);
    let rounded = wanted
        .checked_next_power_of_two()
        .unwrap_or(crate::range::MAX_TOTAL);
    rounded.min(crate::range::MAX_TOTAL)
}

/// Largest dictionary the frequency scale can represent.
pub const MAX_DICTIONARY: usize = (crate::range::MAX_TOTAL / 2) as usize;

/// Assembles a table from corpus statistics.
pub struct TableBuilder {
    pub given: Vec<(String, u64)>,
    pub given_escape: u64,
    pub surname: Vec<(String, u64)>,
    pub surname_escape: u64,
    pub alphabet: Alphabet,
    pub chars: CharModel,
    pub check_modulus: u32,
}

impl TableBuilder {
    pub fn finish(self) -> Table {
        let given = Dictionary::build(&self.given, self.given_escape.max(1));
        let surname = Dictionary::build(&self.surname, self.surname_escape.max(1));
        let mut table = Table {
            given,
            surname,
            alphabet: self.alphabet,
            chars: self.chars,
            check_modulus: self.check_modulus,
            id: 0,
        };
        // The fingerprint covers the serialised table with the id field zero,
        // so it is reproducible from the shipped file.
        table.id = crate::fingerprint(&table.write());
        table
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chars::CharModelBuilder;

    fn sample_table() -> Table {
        let alphabet = Alphabet::new("abcdefghijklmnopqrstuvwxyzåäö -'".chars().collect())
            .expect("valid alphabet");
        let mut builder = CharModelBuilder::new(alphabet.symbols());
        for name in ["smith", "jones", "o'brien", "anna-karin", "wangari"] {
            builder.train(&alphabet.encode(name).expect("in alphabet"), 100);
        }
        let chars = builder.build(0);
        TableBuilder {
            given: vec![
                ("John".into(), 500),
                ("Sarah".into(), 300),
                ("José".into(), 40),
                ("Anna-Karin".into(), 10),
            ],
            given_escape: 100,
            surname: vec![("Smith".into(), 400), ("Ó Súilleabháin".into(), 20)],
            surname_escape: 100,
            alphabet,
            chars,
            check_modulus: 256,
        }
        .finish()
    }

    /// Names with multi-byte characters exercise the front-coding boundary
    /// handling, which is where a byte-wise shared prefix can split a
    /// character.
    #[test]
    fn round_trips_through_serialisation() {
        let table = sample_table();
        let bytes = table.write();
        let parsed = Table::parse(&bytes).expect("table parses");
        assert_eq!(parsed.given.len(), table.given.len());
        assert_eq!(parsed.surname.len(), table.surname.len());
        for i in 0..table.given.len() as u32 {
            assert_eq!(parsed.given.name(i), table.given.name(i));
        }
        for i in 0..table.surname.len() as u32 {
            assert_eq!(parsed.surname.name(i), table.surname.name(i));
        }
        assert_eq!(parsed.id, table.id);
    }

    #[test]
    fn rejects_foreign_bytes() {
        assert!(Table::parse(b"not a table").is_none());
        assert!(Table::parse(&[]).is_none());
    }
}

#[cfg(test)]
mod scale_tests {
    use super::*;

    /// A long tail of once-seen names is the case that overflows a fixed
    /// frequency scale: every symbol needs at least one unit, and with enough
    /// symbols those units alone exceed the scale.
    #[test]
    fn survives_a_long_rare_tail() {
        let mut entries: Vec<(String, u64)> = vec![("Smith".to_owned(), 900_000)];
        entries.extend((0..200_000).map(|i| (format!("rare{i:06}"), 1)));

        let dictionary = Dictionary::build(&entries, 1_000);
        assert_eq!(
            dictionary.cumulative.last().copied(),
            Some(dictionary.total()),
            "frequencies must sum to the scale"
        );
        for symbol in 0..=dictionary.escape_symbol() {
            let (_, size) = dictionary.range(symbol);
            assert!(size > 0, "symbol {symbol} is uncodable");
        }

        // And it must survive a round trip through the table format.
        let mut bytes = Vec::new();
        dictionary.write(&mut bytes);
        let mut cursor = 0usize;
        let parsed = Dictionary::parse(&bytes, &mut cursor).expect("dictionary parses");
        assert_eq!(parsed.len(), dictionary.len());
        assert_eq!(parsed.total(), dictionary.total());
    }

    /// Small dictionaries should keep compact frequencies rather than paying
    /// for precision they cannot use.
    #[test]
    fn scale_grows_only_when_needed() {
        assert_eq!(field_scale(10), MIN_FIELD_SCALE);
        assert!(field_scale(200_000) > MIN_FIELD_SCALE);
        assert!(field_scale(u32::MAX) <= crate::range::MAX_TOTAL);
    }
}
