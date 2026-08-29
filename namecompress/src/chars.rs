//! Order-3 character model over a 32-symbol alphabet.
//!
//! Probabilities are Witten-Bell interpolated across orders. The blend is done
//! in fixed-point integer arithmetic rather than floating point: the encoder
//! and decoder must derive bit-identical frequency tables, and float results
//! are not guaranteed reproducible across platforms or optimisation levels.

use crate::varint;

pub const ALPHABET: usize = 32;
pub const TERMINATOR: u8 = 26;
const HYPHEN: u8 = 27;
const SPACE: u8 = 28;
const APOSTROPHE: u8 = 29;

/// Denominator of the fixed-point probabilities handed to the coder.
pub const SCALE: u32 = 1 << 16;
const MAX_ORDER: usize = 3;

/// Maps a name to alphabet symbols, terminator included. `None` if the name
/// uses characters outside the alphabet, in which case the caller must take
/// the raw fallback.
pub fn encode(name: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(name.len() + 1);
    for c in name.chars() {
        out.push(match c {
            'a'..='z' => c as u8 - b'a',
            '-' => HYPHEN,
            ' ' => SPACE,
            '\'' => APOSTROPHE,
            _ => return None,
        });
    }
    out.push(TERMINATOR);
    Some(out)
}

/// Maps alphabet symbols back to text. The terminator is not included.
pub fn decode(symbols: &[u8]) -> String {
    symbols
        .iter()
        .map(|&s| match s {
            0..=25 => (b'a' + s) as char,
            HYPHEN => '-',
            SPACE => ' ',
            APOSTROPHE => '\'',
            _ => '\u{fffd}',
        })
        .collect()
}

/// One context's quantised counts, as stored in the table.
#[derive(Clone, Default)]
struct Context {
    counts: [u8; ALPHABET],
    total: u32,
    distinct: u32,
}

pub struct CharModel {
    /// Per order, a sparse map from packed context to its counts.
    orders: Vec<Vec<Option<Box<Context>>>>,
}

impl CharModel {
    /// Parses the character-model section of a table.
    pub fn parse(bytes: &[u8], cursor: &mut usize) -> Option<Self> {
        let mut orders = Vec::with_capacity(MAX_ORDER + 1);
        for order in 0..=MAX_ORDER {
            let slots = 1usize << (5 * order);
            let mut table: Vec<Option<Box<Context>>> = vec![None; slots];
            let count = varint::read(bytes, cursor)?;
            let mut ctx = 0usize;
            for _ in 0..count {
                ctx += varint::read(bytes, cursor)? as usize;
                if ctx >= slots {
                    return None;
                }
                let symbols = varint::read(bytes, cursor)?;
                let mut entry = Context::default();
                for _ in 0..symbols {
                    let sym = *bytes.get(*cursor)? as usize;
                    let freq = *bytes.get(*cursor + 1)?;
                    *cursor += 2;
                    if sym >= ALPHABET || freq == 0 {
                        return None;
                    }
                    entry.counts[sym] = freq;
                    entry.total += u32::from(freq);
                    entry.distinct += 1;
                }
                table[ctx] = Some(Box::new(entry));
            }
            orders.push(table);
        }
        Some(Self { orders })
    }

    /// Serialises to the table format.
    pub fn write(&self, out: &mut Vec<u8>) {
        for order in 0..=MAX_ORDER {
            let present: Vec<usize> = self.orders[order]
                .iter()
                .enumerate()
                .filter_map(|(i, c)| c.as_ref().map(|_| i))
                .collect();
            varint::push(out, present.len() as u64);
            let mut previous = 0usize;
            for ctx in present {
                varint::push(out, (ctx - previous) as u64);
                previous = ctx;
                let entry = self.orders[order][ctx].as_ref().expect("present");
                let symbols: Vec<usize> = (0..ALPHABET)
                    .filter(|&s| entry.counts[s] > 0)
                    .collect();
                varint::push(out, symbols.len() as u64);
                for s in symbols {
                    out.push(s as u8);
                    out.push(entry.counts[s]);
                }
            }
        }
    }

    /// Builds the frequency table for the symbol following `history`.
    ///
    /// Every entry is at least 1 so any string remains codable, and the
    /// entries sum to exactly [`SCALE`].
    pub fn distribution(&self, history: &[u8]) -> [u32; ALPHABET] {
        // Order 0 blends against a uniform base.
        let mut probabilities = [SCALE / ALPHABET as u32; ALPHABET];
        let available = history.len().min(MAX_ORDER);
        for order in 0..=available {
            let ctx = pack(&history[history.len() - order..]);
            let Some(entry) = self.orders[order][ctx].as_ref() else {
                continue;
            };
            let denominator = u64::from(entry.total + entry.distinct);
            for (sym, slot) in probabilities.iter_mut().enumerate() {
                let count = u64::from(entry.counts[sym]);
                let lower = u64::from(*slot);
                let blended = (count * u64::from(SCALE) + u64::from(entry.distinct) * lower)
                    / denominator;
                *slot = blended as u32;
            }
            normalise(&mut probabilities);
        }
        probabilities
    }
}

/// Forces every entry to be non-zero and the total to be exactly [`SCALE`].
fn normalise(probabilities: &mut [u32; ALPHABET]) {
    let mut total = 0u32;
    for slot in probabilities.iter_mut() {
        *slot = (*slot).max(1);
        total += *slot;
    }
    // Push the discrepancy onto the largest entry, which always has room.
    let largest = probabilities
        .iter()
        .enumerate()
        .max_by_key(|&(i, &p)| (p, std::cmp::Reverse(i)))
        .map(|(i, _)| i)
        .expect("alphabet is non-empty");
    probabilities[largest] = probabilities[largest] + SCALE - total;
}

/// Packs up to [`MAX_ORDER`] symbols into a context index, 5 bits each.
fn pack(symbols: &[u8]) -> usize {
    symbols
        .iter()
        .fold(0usize, |acc, &s| (acc << 5) | s as usize)
}

/// Build-time accumulator, used by the table generator.
#[derive(Default)]
pub struct CharModelBuilder {
    counts: Vec<Vec<u32>>,
    totals: Vec<Vec<u32>>,
}

impl CharModelBuilder {
    pub fn new() -> Self {
        let mut counts = Vec::new();
        let mut totals = Vec::new();
        for order in 0..=MAX_ORDER {
            let slots = 1usize << (5 * order);
            counts.push(vec![0u32; slots * ALPHABET]);
            totals.push(vec![0u32; slots]);
        }
        Self { counts, totals }
    }

    pub fn train(&mut self, symbols: &[u8], weight: u32) {
        for (i, &sym) in symbols.iter().enumerate() {
            for order in 0..=MAX_ORDER.min(i) {
                let ctx = pack(&symbols[i - order..i]);
                self.counts[order][ctx * ALPHABET + sym as usize] += weight;
                self.totals[order][ctx] += weight;
            }
        }
    }

    /// Drops thinly-observed contexts and quantises the rest to 8 bits.
    pub fn finish(self, prune: u32) -> CharModel {
        let mut orders = Vec::new();
        for order in 0..=MAX_ORDER {
            let slots = 1usize << (5 * order);
            let mut table: Vec<Option<Box<Context>>> = vec![None; slots];
            for ctx in 0..slots {
                let total = self.totals[order][ctx];
                if total == 0 || (order > 0 && total < prune) {
                    continue;
                }
                let row = &self.counts[order][ctx * ALPHABET..(ctx + 1) * ALPHABET];
                let mut entry = Context::default();
                for (sym, &count) in row.iter().enumerate() {
                    if count == 0 {
                        continue;
                    }
                    // Never quantise an observed symbol away entirely.
                    let q = ((u64::from(count) * 255) / u64::from(total)).max(1).min(255);
                    entry.counts[sym] = q as u8;
                    entry.total += q as u32;
                    entry.distinct += 1;
                }
                table[ctx] = Some(Box::new(entry));
            }
            orders.push(table);
        }
        CharModel { orders }
    }
}
