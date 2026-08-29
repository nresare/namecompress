//! Smoothed models trained on a corpus split, for held-out evaluation.
//!
//! Smoothing is Witten-Bell throughout: it is parameter-free, which keeps the
//! measurements honest, and it yields an explicit escape mass that maps
//! directly onto the codec's dictionary-escape symbol.

/// Number of symbols in the character alphabet, a power of two so contexts
/// pack into shifts. 26 letters plus separators, terminator, and an
/// out-of-alphabet marker.
pub const ALPHABET: usize = 32;
pub const TERMINATOR: u8 = 26;
const HYPHEN: u8 = 27;
const SPACE: u8 = 28;
const APOSTROPHE: u8 = 29;

/// Maps a name to alphabet symbols, or `None` if it contains characters the
/// character model does not cover (those names take the raw UTF-8 fallback).
pub fn encode_chars(name: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(name.len() + 1);
    for c in name.chars() {
        let sym = match c.to_ascii_lowercase() {
            c @ 'a'..='z' => c as u8 - b'a',
            '-' => HYPHEN,
            ' ' => SPACE,
            '\'' => APOSTROPHE,
            _ => return None,
        };
        if !c.is_ascii() {
            return None;
        }
        out.push(sym);
    }
    out.push(TERMINATOR);
    Some(out)
}

/// Order-3 character model with Witten-Bell backoff to order 0 and a uniform
/// base. Dense tables: 32^3 contexts is only a few MB and keeps lookups flat.
pub struct CharModel {
    /// Counts indexed by [order][context][symbol], context packed 5 bits per
    /// symbol, most recent symbol in the low bits.
    counts: Vec<Vec<u32>>,
    /// Per-context totals and distinct-symbol counts, for Witten-Bell.
    totals: Vec<Vec<u32>>,
    distinct: Vec<Vec<u32>>,
}

const MAX_ORDER: usize = 3;

impl CharModel {
    pub fn new() -> Self {
        let mut counts = Vec::with_capacity(MAX_ORDER + 1);
        let mut totals = Vec::with_capacity(MAX_ORDER + 1);
        let mut distinct = Vec::with_capacity(MAX_ORDER + 1);
        for order in 0..=MAX_ORDER {
            let contexts = 1usize << (5 * order);
            counts.push(vec![0u32; contexts * ALPHABET]);
            totals.push(vec![0u32; contexts]);
            distinct.push(vec![0u32; contexts]);
        }
        Self {
            counts,
            totals,
            distinct,
        }
    }

    /// Trains on one name, repeated `weight` times to reflect its corpus
    /// frequency.
    pub fn train(&mut self, symbols: &[u8], weight: u32) {
        for (i, &sym) in symbols.iter().enumerate() {
            for order in 0..=MAX_ORDER.min(i) {
                let ctx = pack_context(&symbols[i - order..i]);
                let slot = &mut self.counts[order][ctx * ALPHABET + sym as usize];
                if *slot == 0 {
                    self.distinct[order][ctx] += 1;
                }
                *slot += weight;
                self.totals[order][ctx] += weight;
            }
        }
    }

    /// Cost in bits of coding `symbols` under this model.
    pub fn cost_bits(&self, symbols: &[u8]) -> f64 {
        let mut bits = 0.0;
        for (i, &sym) in symbols.iter().enumerate() {
            let highest = MAX_ORDER.min(i);
            bits -= self.probability(symbols, i, sym, highest).log2();
        }
        bits
    }

    /// Witten-Bell interpolated probability of `sym` at position `i`.
    fn probability(&self, symbols: &[u8], i: usize, sym: u8, order: usize) -> f64 {
        let ctx = pack_context(&symbols[i - order..i]);
        let total = self.totals[order][ctx] as f64;
        let distinct = self.distinct[order][ctx] as f64;
        let count = self.counts[order][ctx * ALPHABET + sym as usize] as f64;

        let lower = if order == 0 {
            1.0 / ALPHABET as f64
        } else {
            self.probability(symbols, i, sym, order - 1)
        };

        if total == 0.0 {
            return lower;
        }
        (count + distinct * lower) / (total + distinct)
    }
}

/// Packs up to `MAX_ORDER` symbols into a context index, 5 bits each.
fn pack_context(symbols: &[u8]) -> usize {
    symbols
        .iter()
        .fold(0usize, |acc, &s| (acc << 5) | s as usize)
}

/// Appends `value` to `out` as a LEB128 varint.
fn push_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

impl CharModel {
    /// Drops contexts seen fewer than `threshold` times at order 1 and above.
    /// Coding falls back to the next shorter context automatically, so this
    /// trades a little accuracy on escaped names for a much smaller table.
    pub fn prune(&mut self, threshold: u32) {
        for order in 1..=MAX_ORDER {
            let contexts = 1usize << (5 * order);
            for ctx in 0..contexts {
                if self.totals[order][ctx] < threshold {
                    self.totals[order][ctx] = 0;
                    self.distinct[order][ctx] = 0;
                    self.counts[order][ctx * ALPHABET..(ctx + 1) * ALPHABET].fill(0);
                }
            }
        }
    }

    /// Rewrites every context's counts as 8-bit quantised frequencies, the
    /// form the shipped table stores. Applied before both evaluation and
    /// serialisation so measured cost reflects what the codec will really do.
    pub fn quantise(&mut self) {
        for order in 0..=MAX_ORDER {
            let contexts = 1usize << (5 * order);
            for ctx in 0..contexts {
                let total = self.totals[order][ctx];
                if total == 0 {
                    continue;
                }
                let row = &mut self.counts[order][ctx * ALPHABET..(ctx + 1) * ALPHABET];
                let mut sum = 0u32;
                for slot in row.iter_mut() {
                    if *slot > 0 {
                        // Never quantise a seen symbol to zero: it must stay
                        // codable.
                        let q = ((*slot as f64 / total as f64) * 255.0).round().max(1.0);
                        *slot = q as u32;
                        sum += *slot;
                    }
                }
                self.totals[order][ctx] = sum;
            }
        }
    }

    /// Serialises the non-empty contexts in the on-disk table form. Returned
    /// uncompressed; the caller compresses it to get the shipped size.
    pub fn serialise(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for order in 0..=MAX_ORDER {
            let contexts = 1usize << (5 * order);
            let mut previous = 0usize;
            for ctx in 0..contexts {
                if self.totals[order][ctx] == 0 {
                    continue;
                }
                push_varint(&mut out, (ctx - previous) as u64);
                previous = ctx;
                let row = &self.counts[order][ctx * ALPHABET..(ctx + 1) * ALPHABET];
                push_varint(&mut out, row.iter().filter(|&&c| c > 0).count() as u64);
                for (sym, &count) in row.iter().enumerate() {
                    if count > 0 {
                        out.push(sym as u8);
                        out.push(count.min(255) as u8);
                    }
                }
            }
        }
        out
    }
}
