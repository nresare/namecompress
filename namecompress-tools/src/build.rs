//! Table generation.
//!
//! Dictionary entries are canonical spellings: variants differing only by case
//! are merged into the most frequent form and recovered at decode time by the
//! shape symbol, so `SMITH`, `Smith`, and `smith` share one entry and pool
//! their counts.

use std::collections::HashMap;
use std::path::Path;

use namecompress::chars::CharModelBuilder;
use namecompress::codec::derive_shape;
use namecompress::table::TableBuilder;

use crate::corpus;

const TEST_MODULUS: u64 = 10;

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

/// Selects the `n` most frequent groups as dictionary entries, returning them
/// and the escape weight for everything else.
fn select(groups: HashMap<String, Group>, n: usize) -> (Vec<(String, u64)>, u64) {
    let mut scored: Vec<(String, u64)> = groups
        .values()
        .map(|g| (g.canonical().to_owned(), g.representable()))
        .collect();
    scored.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let distinct = scored.len() as u64;
    let kept: Vec<(String, u64)> = scored.iter().take(n).cloned().collect();
    let excluded: u64 = scored.iter().skip(n).map(|&(_, c)| c).sum();
    // Witten-Bell: the escape carries the excluded mass plus a novelty term,
    // so names never seen in training remain codable.
    (kept, excluded + distinct)
}

pub fn run(
    path: &Path,
    out: &Path,
    given_size: usize,
    surname_size: usize,
    prune: u32,
    check_modulus: u32,
) -> std::io::Result<()> {
    let mut given_groups: HashMap<String, Group> = HashMap::new();
    let mut surname_groups: HashMap<String, Group> = HashMap::new();
    let mut chars = CharModelBuilder::new();

    for (index, record) in corpus::read(path)?.enumerate() {
        if index as u64 % TEST_MODULUS == 0 {
            continue;
        }
        fold(&mut given_groups, &record.first);
        fold(&mut surname_groups, &record.last);
        for name in [&record.first, &record.last] {
            if let Some(symbols) = namecompress::chars::encode(&name.to_lowercase()) {
                chars.train(&symbols, 1);
            }
        }
    }

    let (given, given_escape) = select(given_groups, given_size);
    let (surname, surname_escape) = select(surname_groups, surname_size);
    println!(
        "given {} entries, surnames {} entries",
        given.len(),
        surname.len()
    );

    let table = TableBuilder {
        given,
        given_escape,
        surname,
        surname_escape,
        chars,
        prune,
        check_modulus,
    }
    .finish();

    // Verify the table survives serialisation before writing it out, and say
    // exactly which entry broke if it does not.
    let bytes = table.write();
    match namecompress::Table::load(&bytes) {
        Ok(parsed) => {
            for (label, before, after) in [
                ("given", &table.given, &parsed.given),
                ("surname", &table.surname, &parsed.surname),
            ] {
                for i in 0..before.len() as u32 {
                    if before.name(i) != after.name(i) {
                        println!(
                            "{label} entry {i} differs: {:?} -> {:?}",
                            before.name(i),
                            after.name(i)
                        );
                        break;
                    }
                }
            }
        }
        Err(e) => {
            println!("table does not parse: {e}");
            for (label, dictionary) in [("given", &table.given), ("surname", &table.surname)] {
                for i in 0..dictionary.len() as u32 {
                    let name = dictionary.name(i).expect("in range");
                    if name.is_empty() || name.bytes().any(|b| b == 0) {
                        println!("  {label} entry {i} suspicious: {name:?}");
                    }
                }
            }
        }
    }
    std::fs::write(out, &bytes)?;
    println!("table id {:08x}, {} bytes raw", table.id, bytes.len());
    Ok(())
}
