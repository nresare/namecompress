//! End-to-end measurement: compress every held-out name, verify it round-trips
//! exactly, and report the true byte cost including coder termination.

use std::path::Path;

use crate::corpus;

const TEST_MODULUS: u64 = 10;

pub fn run(corpus_path: &Path, table_path: &Path) -> std::io::Result<()> {
    let table = crate::packing::read_table(table_path)?;

    let mut rows = 0u64;
    let mut raw_bytes = 0u64;
    let mut packed_bytes = 0u64;
    let mut failures = 0u64;
    let mut histogram = [0u64; 16];

    for (index, record) in corpus::read(corpus_path)?.enumerate() {
        if index as u64 % TEST_MODULUS != 0 {
            continue;
        }
        let name = format!("{} {}", record.first, record.last);
        let packed = match namecompress::compress(&table, &name) {
            Ok(packed) => packed,
            Err(_) => {
                failures += 1;
                continue;
            }
        };
        match namecompress::decompress(&table, &packed) {
            Ok(round_tripped) if round_tripped == name => {}
            _ => failures += 1,
        }
        rows += 1;
        raw_bytes += name.len() as u64;
        packed_bytes += packed.len() as u64;
        histogram[packed.len().min(15)] += 1;
    }

    let rows_f = rows as f64;
    println!("held-out names        {rows}");
    println!("round-trip failures   {failures}");
    println!("mean raw UTF-8        {:.3} bytes", raw_bytes as f64 / rows_f);
    println!(
        "mean compressed       {:.3} bytes",
        packed_bytes as f64 / rows_f
    );
    println!(
        "ratio                 {:.2}x",
        raw_bytes as f64 / packed_bytes as f64
    );
    println!("\nsize distribution:");
    for (size, &count) in histogram.iter().enumerate() {
        if count > 0 {
            println!(
                "  {size:>2} bytes  {:>6.2}%",
                100.0 * count as f64 / rows_f
            );
        }
    }
    Ok(())
}
