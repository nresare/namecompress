//! Distribution statistics over a name corpus.
//!
//! Everything here works on interned symbol ids so the joint table stays small.

use std::collections::HashMap;

/// Assigns a stable `u32` id to each distinct string, and counts occurrences.
#[derive(Default)]
pub struct Interner {
    ids: HashMap<String, u32>,
    counts: Vec<u64>,
    names: Vec<String>,
}

impl Interner {
    /// Interns `s`, bumps its count, and returns its id.
    pub fn observe(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.ids.get(s) {
            self.counts[id as usize] += 1;
            return id;
        }
        let id = u32::try_from(self.names.len()).expect("corpus exceeds u32 symbols");
        self.ids.insert(s.to_owned(), id);
        self.names.push(s.to_owned());
        self.counts.push(1);
        id
    }

    /// Looks up an already-interned symbol without counting an occurrence.
    pub fn get(&self, s: &str) -> Option<u32> {
        self.ids.get(s).copied()
    }

    pub fn distinct(&self) -> usize {
        self.names.len()
    }

    pub fn counts(&self) -> &[u64] {
        &self.counts
    }

    pub fn name(&self, id: u32) -> &str {
        &self.names[id as usize]
    }

    /// Ids ordered by descending frequency.
    pub fn by_frequency(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = (0..self.distinct() as u32).collect();
        ids.sort_unstable_by(|&a, &b| {
            self.counts[b as usize]
                .cmp(&self.counts[a as usize])
                .then_with(|| self.names[a as usize].cmp(&self.names[b as usize]))
        });
        ids
    }
}

/// Shannon entropy in bits of the distribution implied by `counts`.
pub fn entropy_bits(counts: &[u64]) -> f64 {
    let total: u64 = counts.iter().sum();
    if total == 0 {
        return 0.0;
    }
    let total = total as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / total;
            -p * p.log2()
        })
        .sum()
}

/// Fraction of total occurrences covered by the `n` most frequent symbols.
pub fn coverage(interner: &Interner, order: &[u32], n: usize) -> f64 {
    let total: u64 = interner.counts().iter().sum();
    if total == 0 {
        return 0.0;
    }
    let covered: u64 = order
        .iter()
        .take(n)
        .map(|&id| interner.counts()[id as usize])
        .sum();
    covered as f64 / total as f64
}

/// Expected cost in bits per name of a dictionary holding the top `n` symbols,
/// with an escape symbol carrying the remaining mass at `escape_bits` each.
///
/// This is the quantity the amortised dictionary-inclusion rule maximises
/// against table size.
pub fn dictionary_cost_bits(interner: &Interner, order: &[u32], n: usize, escape_bits: f64) -> f64 {
    let total: u64 = interner.counts().iter().sum();
    if total == 0 {
        return 0.0;
    }
    let total = total as f64;

    // Model: n dictionary symbols plus one escape symbol, coded at their
    // empirical probabilities.
    let mut head: Vec<u64> = order
        .iter()
        .take(n)
        .map(|&id| interner.counts()[id as usize])
        .collect();
    let head_sum: u64 = head.iter().sum();
    let tail_sum = total as u64 - head_sum;
    head.push(tail_sum);

    let symbol_bits = entropy_bits(&head);
    // Tail names additionally pay the character model.
    symbol_bits + (tail_sum as f64 / total) * escape_bits
}
