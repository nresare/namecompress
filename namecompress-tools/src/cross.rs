//! Wrong-table detection.
//!
//! Compresses against one table and decodes against another, classifying what
//! happens. Structural rejection costs no bits, so measuring it tells us how
//! large the explicit verification symbol actually has to be.

use std::path::Path;

use namecompress::Error;

use crate::corpus;

const TEST_MODULUS: u64 = 10;

pub fn run(corpus_path: &Path, a_path: &Path, b_path: &Path) -> std::io::Result<()> {
    let a = crate::packing::read_table(a_path)?;
    let b = crate::packing::read_table(b_path)?;

    let mut rows = 0u64;
    let mut malformed = 0u64;
    let mut flagged = 0u64;
    let mut silent_garble = 0u64;
    let mut coincidental = 0u64;
    let mut examples: Vec<(String, String)> = Vec::new();

    for (index, record) in corpus::read(corpus_path)?.enumerate() {
        if index as u64 % TEST_MODULUS != 0 {
            continue;
        }
        let name = format!("{} {}", record.first, record.last);
        let Ok(packed) = namecompress::compress(&a, &name) else {
            continue;
        };
        rows += 1;
        match namecompress::decompress(&b, &packed) {
            Err(Error::Malformed) | Err(Error::TooLong) => malformed += 1,
            Err(Error::WrongTable) => flagged += 1,
            Ok(other) if other == name => coincidental += 1,
            Ok(other) => {
                silent_garble += 1;
                if examples.len() < 5 {
                    examples.push((name.clone(), other));
                }
            }
        }
    }

    let rows_f = rows as f64;
    let percent = |n: u64| 100.0 * n as f64 / rows_f;
    println!("messages                {rows}");
    println!("rejected structurally   {:>8.4}%  ({malformed})", percent(malformed));
    println!("caught by check symbol  {:>8.4}%  ({flagged})", percent(flagged));
    println!("decoded to same name    {:>8.4}%  ({coincidental})", percent(coincidental));
    println!("SILENTLY WRONG          {:>8.4}%  ({silent_garble})", percent(silent_garble));
    if !examples.is_empty() {
        println!("\nexamples of silent corruption:");
        for (from, to) in &examples {
            println!("  {from:?} -> {to:?}");
        }
    }
    Ok(())
}
