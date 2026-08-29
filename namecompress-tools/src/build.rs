//! Table generation.
//!
//! The caller states roughly how large the shipped table may be and this
//! chooses everything else: how much of the budget the character model may
//! take, how aggressively to prune it, and how many dictionary entries fit in
//! what remains. Sizes are measured on the actually-compressed table rather
//! than estimated, because compression of front-coded names is not linear in
//! the entry count.
//!
//! Dictionary entries are canonical spellings: variants differing only by case
//! are merged into the most frequent form and recovered at decode time by the
//! shape symbol, so `SMITH`, `Smith`, and `smith` share one entry and pool
//! their counts.

use std::collections::HashMap;
use std::path::Path;

use namecompress::chars::{Alphabet, CharModelBuilder, MAX_SYMBOLS};
use namecompress::codec::derive_shape;
use namecompress::table::TableBuilder;

use crate::corpus;
use crate::packing::Packing;

/// Pruning thresholds to consider, finest model first.
const PRUNE_CANDIDATES: [u32; 6] = [256, 1024, 4096, 16_384, 65_536, 262_144];

/// Share of the budget the character model may occupy. It serves only escaped
/// names, so spending much more on it than this buys less than dictionary
/// entries would.
const CHAR_MODEL_SHARE: f64 = 0.25;

/// Surnames get more dictionary slots than given names: they are both more
/// numerous and less predictable.
const SURNAME_RATIO: usize = 2;

/// Enough refinement to converge; each step costs one compression.
const MAX_ATTEMPTS: usize = 12;

/// Stop once the table is within this fraction of the target from below.
const CLOSE_ENOUGH: f64 = 0.02;

/// Counts of a case-folded name group, with the spellings seen for it.
#[derive(Default)]
struct Group {
    variants: HashMap<String, u64>,
}

impl Group {
    /// The most frequent spelling, which becomes the dictionary entry.
    fn canonical(&self) -> &str {
        self.variants
            .iter()
            .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
            .map(|(name, _)| name.as_str())
            .expect("group is non-empty")
    }

    fn total(&self) -> u64 {
        self.variants.values().sum()
    }

    /// Occurrences the dictionary entry can actually represent: those whose
    /// spelling is reachable from the canonical form by a shape.
    fn representable(&self) -> u64 {
        let canonical = self.canonical();
        self.variants
            .iter()
            .filter(|(name, _)| derive_shape(canonical, name).is_some())
            .map(|(_, &count)| count)
            .sum()
    }
}

fn fold(groups: &mut HashMap<String, Group>, name: &str) {
    *groups
        .entry(name.to_lowercase())
        .or_default()
        .variants
        .entry(name.to_owned())
        .or_insert(0) += 1;
}

/// Chooses the alphabet from the characters the corpus actually uses, weighted
/// by how often they occur. One symbol is reserved for the terminator, and
/// anything that does not fit is left to the raw fallback.
fn derive_alphabet(groups: &[&HashMap<String, Group>]) -> (Alphabet, f64) {
    let mut weights: HashMap<char, u64> = HashMap::new();
    let mut total = 0u64;
    for set in groups {
        for (folded, group) in *set {
            let count = group.total();
            total += count * folded.chars().count() as u64;
            for c in folded.chars() {
                *weights.entry(c).or_insert(0) += count;
            }
        }
    }

    let mut ranked: Vec<(char, u64)> = weights.into_iter().collect();
    ranked.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(MAX_SYMBOLS - 1);

    let covered: u64 = ranked.iter().map(|&(_, c)| c).sum();
    // A stable, readable ordering; symbol indices themselves are arbitrary.
    let mut characters: Vec<char> = ranked.into_iter().map(|(c, _)| c).collect();
    characters.sort_unstable();

    let coverage = if total == 0 {
        0.0
    } else {
        covered as f64 / total as f64
    };
    (
        Alphabet::new(characters).expect("alphabet is non-empty and within range"),
        coverage,
    )
}

/// Groups ordered by descending representable count, with the total number of
/// distinct groups.
fn rank(groups: &HashMap<String, Group>) -> (Vec<(String, u64)>, u64) {
    let mut ranked: Vec<(String, u64)> = groups
        .values()
        .map(|g| (g.canonical().to_owned(), g.representable()))
        .collect();
    ranked.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let distinct = ranked.len() as u64;
    (ranked, distinct)
}

/// The top `n` entries, and the escape weight covering everything else.
fn take(ranked: &[(String, u64)], distinct: u64, n: usize) -> (Vec<(String, u64)>, u64) {
    let n = n.min(ranked.len());
    let excluded: u64 = ranked[n..].iter().map(|&(_, c)| c).sum();
    // Witten-Bell: the escape carries the excluded mass plus a novelty term,
    // so names never seen in training remain codable.
    (ranked[..n].to_vec(), excluded + distinct)
}

/// Describes what the search settled on.
struct Chosen {
    packed: Vec<u8>,
    raw_len: usize,
    given: usize,
    surnames: usize,
}

pub fn run(path: &Path, out: &Path, target: usize, check_modulus: u32) -> std::io::Result<()> {
    let packing = Packing::for_path(out);

    let mut given_groups: HashMap<String, Group> = HashMap::new();
    let mut surname_groups: HashMap<String, Group> = HashMap::new();
    for (index, record) in corpus::read(path)?.enumerate() {
        if crate::split::is_held_out(index) {
            continue;
        }
        fold(&mut given_groups, &record.first);
        fold(&mut surname_groups, &record.last);
    }

    let (alphabet, coverage) = derive_alphabet(&[&given_groups, &surname_groups]);
    println!(
        "alphabet     {} characters, {:.2}% character coverage",
        alphabet.characters().len(),
        100.0 * coverage
    );

    // The folded groups already carry occurrence counts, so the character
    // model can be trained from them rather than by rereading the corpus.
    let mut chars = CharModelBuilder::new(alphabet.symbols());
    for set in [&given_groups, &surname_groups] {
        for (folded, group) in set {
            if let Some(symbols) = alphabet.encode(folded) {
                chars.train(&symbols, u32::try_from(group.total()).unwrap_or(u32::MAX));
            }
        }
    }

    // Pick the finest character model whose share of the budget it fits into.
    let allowance = (target as f64 * CHAR_MODEL_SHARE) as usize;
    let mut model = chars.build(*PRUNE_CANDIDATES.last().expect("non-empty"));
    let mut model_size = 0usize;
    for &prune in &PRUNE_CANDIDATES {
        let candidate = chars.build(prune);
        let mut serialised = Vec::new();
        candidate.write(&mut serialised);
        let size = packing.pack(&serialised)?.len();
        if size <= allowance || prune == *PRUNE_CANDIDATES.last().expect("non-empty") {
            println!("model        prune {prune}, {size} B of {allowance} B allowance");
            model = candidate;
            model_size = size;
            break;
        }
    }

    let (given_ranked, given_distinct) = rank(&given_groups);
    let (surname_ranked, surname_distinct) = rank(&surname_groups);

    // Refine the dictionary size against the measured compressed size. The
    // character model is a near-fixed overhead, so scale only what remains.
    let mut entries = 2_000usize;
    let mut best: Option<Chosen> = None;
    for _ in 0..MAX_ATTEMPTS {
        let (given, given_escape) = take(&given_ranked, given_distinct, entries);
        let (surname, surname_escape) =
            take(&surname_ranked, surname_distinct, entries * SURNAME_RATIO);

        let table = TableBuilder {
            given,
            given_escape,
            surname,
            surname_escape,
            alphabet: alphabet.clone(),
            chars: model.clone(),
            check_modulus,
        }
        .finish();
        let raw = table.write();
        let packed = packing.pack(&raw)?;

        let current = packed.len();
        let fits = current <= target;
        if fits && best.as_ref().is_none_or(|b| current > b.packed.len()) {
            best = Some(Chosen {
                raw_len: raw.len(),
                packed,
                given: table.given.len(),
                surnames: table.surname.len(),
            });
        }
        if fits && target - current <= (target as f64 * CLOSE_ENOUGH) as usize {
            break;
        }

        // The character model is near-fixed overhead, so scale only the part
        // of the budget the dictionaries are actually competing for.
        let overhead = model_size.min(target / 2);
        let dictionary_now = current.saturating_sub(overhead).max(1);
        let dictionary_target = target.saturating_sub(overhead).max(1);
        let scale = (dictionary_target as f64 / dictionary_now as f64).clamp(0.5, 2.0);
        let next = (((entries as f64) * scale).round() as usize).clamp(100, given_ranked.len());
        if next == entries {
            break;
        }
        entries = next;
    }

    let Some(chosen) = best else {
        eprintln!("no table fits within {target} bytes; raise the target");
        return Err(std::io::Error::other("target too small"));
    };

    std::fs::write(out, &chosen.packed)?;
    println!(
        "dictionary   {} given names, {} surnames",
        chosen.given, chosen.surnames
    );
    println!(
        "table        {} B {} ({} B raw), target {} B",
        chosen.packed.len(),
        packing.name(),
        chosen.raw_len,
        target
    );
    Ok(())
}
