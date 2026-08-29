//! Table-budget sweep.
//!
//! For each candidate dictionary size this measures both sides of the
//! trade-off: the held-out coding cost, and the serialised size of the table
//! that size implies. The two dictionaries and the character model compete for
//! one fixed budget, and cost is separable across them, so each axis is swept
//! independently and combined afterwards.

use std::io::Write;
use std::path::Path;

use crate::corpus;
use crate::model::{CharModel, encode_chars};
use crate::stats::Interner;

const TEST_MODULUS: u64 = 10;
const GRID: [usize; 10] = [
    1_000, 2_000, 5_000, 10_000, 20_000, 40_000, 80_000, 160_000, 320_000, usize::MAX,
];

/// Coding cost of one field under a dictionary capped at `n` entries.
struct Capped {
    /// Cumulative train count of the top `k` names, indexed by `k`.
    cumulative: Vec<u64>,
    total: u64,
    distinct: u64,
    /// Train count per symbol id.
    counts: Vec<u64>,
    /// Frequency rank per symbol id.
    rank: Vec<u32>,
}

impl Capped {
    fn new(interner: &Interner, order: &[u32]) -> Self {
        let counts = interner.counts().to_vec();
        let total = counts.iter().sum();
        let mut cumulative = Vec::with_capacity(order.len() + 1);
        cumulative.push(0);
        let mut running = 0;
        for &id in order {
            running += counts[id as usize];
            cumulative.push(running);
        }
        let mut rank = vec![0u32; counts.len()];
        for (position, &id) in order.iter().enumerate() {
            rank[id as usize] = position as u32;
        }
        Self {
            distinct: counts.len() as u64,
            cumulative,
            total,
            counts,
            rank,
        }
    }

    fn cap(&self, n: usize) -> usize {
        n.min(self.distinct as usize)
    }

    /// Denominator and escape mass for a dictionary of the top `n` names.
    /// Witten-Bell: excluded names contribute both their mass and a novelty
    /// term, which is what lets genuinely unseen names be coded at all.
    fn escape_bits(&self, n: usize) -> f64 {
        let n = self.cap(n);
        let denom = (self.total + self.distinct) as f64;
        let escape = (self.total - self.cumulative[n] + self.distinct) as f64;
        -(escape / denom).log2()
    }

    /// Bits for a name that is in the dictionary at size `n`.
    fn dictionary_bits(&self, n: usize, id: u32) -> Option<f64> {
        let n = self.cap(n);
        if (self.rank[id as usize] as usize) >= n {
            return None;
        }
        let denom = (self.total + self.distinct) as f64;
        Some(-(self.counts[id as usize] as f64 / denom).log2())
    }
}

/// Bits to code a name that escaped the dictionary, taking whichever of the
/// character model and the raw UTF-8 fallback is cheaper.
fn tail_bits(chars: &CharModel, name: &str) -> f64 {
    let raw = 8.0 * (name.len() + 1) as f64;
    match encode_chars(name) {
        Some(symbols) => chars.cost_bits(&symbols).min(raw),
        None => raw,
    }
}

/// Front-coded name blob plus frequencies, the on-disk table form. Returned
/// uncompressed; the caller compresses it to get the shipped size.
fn serialise_dictionary(interner: &Interner, order: &[u32], n: usize) -> Vec<u8> {
    let n = n.min(order.len());
    let mut entries: Vec<(&str, u64)> = order[..n]
        .iter()
        .map(|&id| (interner.name(id), interner.counts()[id as usize]))
        .collect();
    entries.sort_unstable_by_key(|&(name, _)| name);

    let mut out = Vec::new();
    let mut previous = "";
    for &(name, _) in &entries {
        let shared = name
            .bytes()
            .zip(previous.bytes())
            .take_while(|(a, b)| a == b)
            .count()
            .min(255);
        out.push(shared as u8);
        out.extend_from_slice(&name.as_bytes()[shared..]);
        out.push(0);
        previous = name;
    }
    // Frequencies in the same lexicographic order, varint-coded.
    for &(_, count) in &entries {
        let mut v = count;
        while v >= 0x80 {
            out.push((v as u8) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }
    out
}

fn write_blob(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::File::create(dir.join(name))?;
    file.write_all(bytes)
}

pub fn run(path: &Path, out_dir: &Path, prune: u32) -> std::io::Result<()> {
    std::fs::create_dir_all(out_dir)?;

    let mut firsts = Interner::default();
    let mut lasts = Interner::default();
    let mut chars = CharModel::new();

    for (index, record) in corpus::read(path)?.enumerate() {
        if index as u64 % TEST_MODULUS == 0 {
            continue;
        }
        firsts.observe(&record.first);
        lasts.observe(&record.last);
        for name in [&record.first, &record.last] {
            if let Some(symbols) = encode_chars(name) {
                chars.train(&symbols, 1);
            }
        }
    }

    chars.prune(prune);
    chars.quantise();
    let char_table = chars.serialise();
    println!("# prune {prune} char_table_raw {}", char_table.len());

    let first_order = firsts.by_frequency();
    let last_order = lasts.by_frequency();
    let first_capped = Capped::new(&firsts, &first_order);
    let last_capped = Capped::new(&lasts, &last_order);

    // One test pass accumulating cost at every grid point on both axes.
    let mut first_bits = [0f64; GRID.len()];
    let mut last_bits = [0f64; GRID.len()];
    let mut rows = 0u64;

    for (index, record) in corpus::read(path)?.enumerate() {
        if index as u64 % TEST_MODULUS != 0 {
            continue;
        }
        rows += 1;
        for (field, interner, capped, accumulator) in [
            (&record.first, &firsts, &first_capped, &mut first_bits),
            (&record.last, &lasts, &last_capped, &mut last_bits),
        ] {
            let id = interner.get(field);
            let tail = tail_bits(&chars, field);
            for (slot, &n) in accumulator.iter_mut().zip(GRID.iter()) {
                *slot += match id.and_then(|id| capped.dictionary_bits(n, id)) {
                    Some(bits) => bits,
                    None => capped.escape_bits(n) + tail,
                };
            }
        }
    }

    // Serialise each candidate table so the caller can compress and size it.
    for (i, &n) in GRID.iter().enumerate() {
        write_blob(
            out_dir,
            &format!("first.{i}.bin"),
            &serialise_dictionary(&firsts, &first_order, n),
        )?;
        write_blob(
            out_dir,
            &format!("last.{i}.bin"),
            &serialise_dictionary(&lasts, &last_order, n),
        )?;
    }
    write_blob(out_dir, &format!("charmodel.p{prune}.bin"), &char_table)?;

    let rows = rows as f64;
    println!("# axis  index  entries  bits_per_name");
    for (i, &n) in GRID.iter().enumerate() {
        let n = n.min(firsts.distinct());
        println!("first {i} {n} {:.4}", first_bits[i] / rows);
    }
    for (i, &n) in GRID.iter().enumerate() {
        let n = n.min(lasts.distinct());
        println!("last {i} {n} {:.4}", last_bits[i] / rows);
    }
    Ok(())
}
