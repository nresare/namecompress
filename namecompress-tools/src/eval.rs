//! Held-out evaluation.
//!
//! Trains on 90% of the corpus and measures actual coding cost on the
//! remaining 10%. Cross-entropy on unseen rows cannot be inflated by
//! overfitting, so unlike a plug-in entropy estimate it is a number the codec
//! can actually be held to.

use std::collections::HashMap;
use std::path::Path;

use crate::corpus;
use crate::model::{CharModel, encode_chars};
use crate::stats::Interner;

/// Marginal distribution with an explicit escape into a character model,
/// mirroring the codec's dictionary-plus-escape structure.
struct Marginal {
    counts: Vec<u64>,
    total: u64,
    distinct: u64,
}

impl Marginal {
    fn new(interner: &Interner) -> Self {
        let counts = interner.counts().to_vec();
        Self {
            total: counts.iter().sum(),
            distinct: counts.len() as u64,
            counts,
        }
    }

    fn denominator(&self) -> f64 {
        (self.total + self.distinct) as f64
    }

    /// Probability of a seen symbol.
    fn seen(&self, id: u32) -> f64 {
        self.counts[id as usize] as f64 / self.denominator()
    }

    /// Total mass reserved for unseen symbols.
    fn escape(&self) -> f64 {
        self.distinct as f64 / self.denominator()
    }
}

/// Cost of a name that is not in the dictionary: the escape symbol plus
/// either the character model or the raw UTF-8 fallback, whichever is cheaper.
fn tail_bits(marginal: &Marginal, chars: &CharModel, name: &str) -> f64 {
    let escape = -marginal.escape().log2();
    // The raw fallback costs the UTF-8 bytes plus a length or terminator.
    let raw = 8.0 * (name.len() + 1) as f64;
    let modelled = match encode_chars(name) {
        Some(symbols) => chars.cost_bits(&symbols),
        None => f64::INFINITY,
    };
    escape + modelled.min(raw)
}

/// Bits to code `name` given the marginal, dictionary-first with escape.
fn marginal_bits(marginal: &Marginal, chars: &CharModel, id: Option<u32>, name: &str) -> f64 {
    match id {
        Some(id) => -marginal.seen(id).log2(),
        None => tail_bits(marginal, chars, name),
    }
}

#[derive(Default)]
struct Totals {
    rows: u64,
    raw_bytes: u64,
    first_bits: f64,
    last_independent_bits: f64,
    last_conditional_bits: f64,
    first_escapes: u64,
    last_escapes: u64,
}

pub fn run(path: &Path) -> std::io::Result<()> {
    let mut firsts = Interner::default();
    let mut lasts = Interner::default();
    let mut joint: HashMap<(u32, u32), u64> = HashMap::new();
    let mut chars = CharModel::new();

    // --- training pass -----------------------------------------------------
    for (index, record) in corpus::read(path)?.enumerate() {
        if crate::split::is_held_out(index) {
            continue;
        }
        let f = firsts.observe(&record.first);
        let l = lasts.observe(&record.last);
        *joint.entry((f, l)).or_insert(0) += 1;
        for name in [&record.first, &record.last] {
            if let Some(symbols) = encode_chars(name) {
                chars.train(&symbols, 1);
            }
        }
    }

    let first_marginal = Marginal::new(&firsts);
    let last_marginal = Marginal::new(&lasts);

    // Witten-Bell for P(last | first) needs, per first name, its total count
    // and the number of distinct surnames seen with it.
    let mut context_total: HashMap<u32, u64> = HashMap::new();
    let mut context_distinct: HashMap<u32, u64> = HashMap::new();
    for (&(f, _), &count) in &joint {
        *context_total.entry(f).or_insert(0) += count;
        *context_distinct.entry(f).or_insert(0) += 1;
    }

    // --- evaluation pass ---------------------------------------------------
    let mut totals = Totals::default();
    for (index, record) in corpus::read(path)?.enumerate() {
        if !crate::split::is_held_out(index) {
            continue;
        }
        totals.rows += 1;
        totals.raw_bytes += (record.first.len() + record.last.len() + 1) as u64;

        let f = firsts.get(&record.first);
        let l = lasts.get(&record.last);

        totals.first_bits += marginal_bits(&first_marginal, &chars, f, &record.first);
        if f.is_none() {
            totals.first_escapes += 1;
        }

        let independent = marginal_bits(&last_marginal, &chars, l, &record.last);
        totals.last_independent_bits += independent;
        if l.is_none() {
            totals.last_escapes += 1;
        }

        // Conditional model: interpolate P(last | first) with the marginal,
        // backing off entirely when the first name was never seen.
        totals.last_conditional_bits += match f {
            None => independent,
            Some(f) => {
                let total = *context_total.get(&f).unwrap_or(&0) as f64;
                let distinct = *context_distinct.get(&f).unwrap_or(&0) as f64;
                let base = match l {
                    Some(l) => last_marginal.seen(l),
                    None => {
                        // Spread the escape mass over the character model so
                        // both branches are one comparable distribution.
                        let bits = tail_bits(&last_marginal, &chars, &record.last);
                        2f64.powf(-bits)
                    }
                };
                let pair = match l {
                    Some(l) => *joint.get(&(f, l)).unwrap_or(&0) as f64,
                    None => 0.0,
                };
                let p = (pair + distinct * base) / (total + distinct);
                -p.log2()
            }
        };
    }

    let rows = totals.rows as f64;
    let first = totals.first_bits / rows;
    let independent = totals.last_independent_bits / rows;
    let conditional = totals.last_conditional_bits / rows;

    println!("== held-out ({} rows) ==", totals.rows);
    println!(
        "mean raw UTF-8        {:.2} bytes",
        totals.raw_bytes as f64 / rows
    );
    println!(
        "first-name escapes    {:.3}%",
        100.0 * totals.first_escapes as f64 / rows
    );
    println!(
        "surname escapes       {:.3}%",
        100.0 * totals.last_escapes as f64 / rows
    );
    println!("\nbits/name, held out:");
    println!("first                 {first:.3}");
    println!("last, independent     {independent:.3}");
    println!("last, conditional     {conditional:.3}");
    println!("realised MI gain      {:.3}", independent - conditional);
    println!(
        "\ntotal independent     {:.3} bits = {:.3} bytes",
        first + independent,
        (first + independent) / 8.0
    );
    println!(
        "total conditional     {:.3} bits = {:.3} bytes",
        first + conditional,
        (first + conditional) / 8.0
    );
    Ok(())
}
