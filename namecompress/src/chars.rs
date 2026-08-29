//! Order-3 character model over a table-defined alphabet.
//!
//! The alphabet is carried by the table rather than fixed in the code, so a
//! Swedish table covers `å ä ö` and a Polish one `ł ą ę`. A name using a
//! character the table does not know is not modelled at all; the codec falls
//! back to raw UTF-8 for it.
//!
//! Probabilities are Witten-Bell interpolated across orders. The blend is done
//! in fixed-point integer arithmetic rather than floating point: encoder and
//! decoder must derive bit-identical frequency tables, and float results are
//! not guaranteed reproducible across platforms or optimisation levels.

use std::collections::HashMap;

use crate::varint;

/// Bits per symbol in a packed context. Six admits alphabets large enough for
/// European orthographies; contexts are stored sparsely, so the wider packing
/// costs nothing for a small alphabet.
const CONTEXT_BITS: u32 = 6;

/// Most symbols an alphabet may hold, terminator included.
pub const MAX_SYMBOLS: usize = 1 << CONTEXT_BITS;

/// Denominator of the fixed-point probabilities handed to the coder.
pub const SCALE: u32 = 1 << 16;

const MAX_ORDER: usize = 3;

/// The characters a table can model, plus an implicit terminator whose symbol
/// is `characters.len()`.
pub struct Alphabet {
    characters: Vec<char>,
    lookup: HashMap<char, u8>,
}

impl Alphabet {
    /// Builds an alphabet, rejecting duplicates and oversized sets. One slot
    /// is reserved for the terminator.
    pub fn new(characters: Vec<char>) -> Option<Self> {
        if characters.is_empty() || characters.len() >= MAX_SYMBOLS {
            return None;
        }
        let mut lookup = HashMap::with_capacity(characters.len());
        for (index, &c) in characters.iter().enumerate() {
            if lookup.insert(c, index as u8).is_some() {
                return None;
            }
        }
        Some(Self { characters, lookup })
    }

    /// Symbol count including the terminator.
    pub fn symbols(&self) -> usize {
        self.characters.len() + 1
    }

    pub fn terminator(&self) -> u8 {
        self.characters.len() as u8
    }

    pub fn characters(&self) -> &[char] {
        &self.characters
    }

    /// Maps a name to symbols, terminator included. `None` if the name uses a
    /// character outside this alphabet.
    pub fn encode(&self, name: &str) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(name.len() + 1);
        for c in name.chars() {
            out.push(*self.lookup.get(&c)?);
        }
        out.push(self.terminator());
        Some(out)
    }

    /// Maps symbols back to text. The terminator must not be included.
    pub fn decode(&self, symbols: &[u8]) -> Option<String> {
        symbols
            .iter()
            .map(|&s| self.characters.get(s as usize).copied())
            .collect()
    }

    pub fn write(&self, out: &mut Vec<u8>) {
        varint::push(out, self.characters.len() as u64);
        for &c in &self.characters {
            varint::push(out, u32::from(c) as u64);
        }
    }

    pub fn parse(bytes: &[u8], cursor: &mut usize) -> Option<Self> {
        let count = varint::read(bytes, cursor)?;
        let mut characters = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let scalar = u32::try_from(varint::read(bytes, cursor)?).ok()?;
            characters.push(char::from_u32(scalar)?);
        }
        Self::new(characters)
    }
}

/// One context's quantised counts, as stored in the table.
#[derive(Clone)]
struct Context {
    counts: Vec<u8>,
    total: u32,
    distinct: u32,
}

pub struct CharModel {
    symbols: usize,
    /// Per order, a sparse map from packed context to its counts. Contexts are
    /// sparse by nature and a dense table would be gigabytes at this alphabet
    /// width.
    orders: Vec<HashMap<u32, Context>>,
}

impl CharModel {
    pub fn parse(bytes: &[u8], cursor: &mut usize, symbols: usize) -> Option<Self> {
        if symbols > MAX_SYMBOLS {
            return None;
        }
        let mut orders = Vec::with_capacity(MAX_ORDER + 1);
        for _ in 0..=MAX_ORDER {
            let count = varint::read(bytes, cursor)?;
            let mut table = HashMap::with_capacity(count as usize);
            let mut context = 0u32;
            for _ in 0..count {
                context = context.checked_add(u32::try_from(varint::read(bytes, cursor)?).ok()?)?;
                let present = varint::read(bytes, cursor)?;
                let mut entry = Context {
                    counts: vec![0; symbols],
                    total: 0,
                    distinct: 0,
                };
                for _ in 0..present {
                    let symbol = *bytes.get(*cursor)? as usize;
                    let frequency = *bytes.get(*cursor + 1)?;
                    *cursor += 2;
                    if symbol >= symbols || frequency == 0 {
                        return None;
                    }
                    entry.counts[symbol] = frequency;
                    entry.total += u32::from(frequency);
                    entry.distinct += 1;
                }
                if table.insert(context, entry).is_some() {
                    return None;
                }
            }
            orders.push(table);
        }
        Some(Self { symbols, orders })
    }

    pub fn write(&self, out: &mut Vec<u8>) {
        for order in &self.orders {
            let mut contexts: Vec<u32> = order.keys().copied().collect();
            contexts.sort_unstable();
            varint::push(out, contexts.len() as u64);
            let mut previous = 0u32;
            for context in contexts {
                varint::push(out, u64::from(context - previous));
                previous = context;
                let entry = &order[&context];
                let present: Vec<usize> = (0..self.symbols)
                    .filter(|&s| entry.counts[s] > 0)
                    .collect();
                varint::push(out, present.len() as u64);
                for symbol in present {
                    out.push(symbol as u8);
                    out.push(entry.counts[symbol]);
                }
            }
        }
    }

    /// Frequency table for the symbol following `history`. Every entry is at
    /// least 1 so any string stays codable, and the entries sum to [`SCALE`].
    pub fn distribution(&self, history: &[u8]) -> Vec<u32> {
        let uniform = SCALE / self.symbols as u32;
        let mut probabilities = vec![uniform; self.symbols];
        let available = history.len().min(MAX_ORDER);
        for order in 0..=available {
            let context = pack(&history[history.len() - order..]);
            let Some(entry) = self.orders[order].get(&context) else {
                continue;
            };
            let denominator = u64::from(entry.total + entry.distinct);
            for (symbol, slot) in probabilities.iter_mut().enumerate() {
                let count = u64::from(entry.counts[symbol]);
                let lower = u64::from(*slot);
                *slot = ((count * u64::from(SCALE) + u64::from(entry.distinct) * lower)
                    / denominator) as u32;
            }
            normalise(&mut probabilities);
        }
        probabilities
    }
}

/// Forces every entry non-zero and the total to be exactly [`SCALE`].
fn normalise(probabilities: &mut [u32]) {
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

/// Packs up to [`MAX_ORDER`] symbols into a context key.
fn pack(symbols: &[u8]) -> u32 {
    symbols
        .iter()
        .fold(0u32, |acc, &s| (acc << CONTEXT_BITS) | u32::from(s))
}

/// Build-time accumulator, used by the table generator.
pub struct CharModelBuilder {
    symbols: usize,
    orders: Vec<HashMap<u32, Vec<u32>>>,
}

impl CharModelBuilder {
    pub fn new(symbols: usize) -> Self {
        Self {
            symbols,
            orders: (0..=MAX_ORDER).map(|_| HashMap::new()).collect(),
        }
    }

    pub fn train(&mut self, symbols: &[u8], weight: u32) {
        for (i, &symbol) in symbols.iter().enumerate() {
            for order in 0..=MAX_ORDER.min(i) {
                let context = pack(&symbols[i - order..i]);
                self.orders[order]
                    .entry(context)
                    .or_insert_with(|| vec![0; self.symbols])[symbol as usize] += weight;
            }
        }
    }

    /// Drops thinly-observed contexts and quantises the rest to 8 bits.
    pub fn finish(self, prune: u32) -> CharModel {
        let mut orders = Vec::with_capacity(MAX_ORDER + 1);
        for (order, counts) in self.orders.into_iter().enumerate() {
            let mut table = HashMap::new();
            for (context, row) in counts {
                let total: u32 = row.iter().sum();
                if total == 0 || (order > 0 && total < prune) {
                    continue;
                }
                let mut entry = Context {
                    counts: vec![0; self.symbols],
                    total: 0,
                    distinct: 0,
                };
                for (symbol, &count) in row.iter().enumerate() {
                    if count == 0 {
                        continue;
                    }
                    // Never quantise an observed symbol away entirely.
                    let q = ((u64::from(count) * 255) / u64::from(total)).clamp(1, 255) as u8;
                    entry.counts[symbol] = q;
                    entry.total += u32::from(q);
                    entry.distinct += 1;
                }
                table.insert(context, entry);
            }
            orders.push(table);
        }
        CharModel {
            symbols: self.symbols,
            orders,
        }
    }
}
