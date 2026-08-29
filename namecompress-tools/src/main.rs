//! Offline corpus analysis for the namecompress model.
//!
//! Establishes the entropy floor and the structural facts that decide which
//! model layers are worth their table bytes.

mod bench;
mod build;
mod corpus;
mod cross;
mod eval;
mod model;
mod packing;
mod split;
mod stats;
mod sweep;

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;

use stats::{Interner, coverage, dictionary_cost_bits, entropy_bits};

/// Assumed cost of coding one escaped (out-of-dictionary) name with a
/// character model, for the dictionary-cost curve. Refined once the
/// character model exists.
const ASSUMED_ESCAPE_BITS: f64 = 20.0;

/// Structural properties of the raw name strings, which decide how much
/// tokenisation and orthography machinery the codec actually needs.
#[derive(Default)]
struct Structure {
    rows: u64,
    multi_token_first: u64,
    multi_token_last: u64,
    hyphenated_first: u64,
    hyphenated_last: u64,
    apostrophe: u64,
    non_ascii: u64,
    non_titlecase: u64,
    utf8_bytes: u64,
}

impl Structure {
    fn observe(&mut self, first: &str, last: &str) {
        self.rows += 1;
        // +1 for the joining space, matching the "Firstname Lastname" input.
        self.utf8_bytes += (first.len() + last.len() + 1) as u64;

        if first.contains(' ') {
            self.multi_token_first += 1;
        }
        if last.contains(' ') {
            self.multi_token_last += 1;
        }
        if first.contains('-') {
            self.hyphenated_first += 1;
        }
        if last.contains('-') {
            self.hyphenated_last += 1;
        }
        if first.contains('\'') || last.contains('\'') {
            self.apostrophe += 1;
        }
        if !first.is_ascii() || !last.is_ascii() {
            self.non_ascii += 1;
        }
        if !is_titlecase(first) || !is_titlecase(last) {
            self.non_titlecase += 1;
        }
    }

    fn report(&self) {
        let pct = |n: u64| 100.0 * n as f64 / self.rows as f64;
        println!("\n== structure ==");
        println!("rows                  {}", self.rows);
        println!(
            "mean raw UTF-8 bytes  {:.2}",
            self.utf8_bytes as f64 / self.rows as f64
        );
        println!("multi-token first     {:.3}%", pct(self.multi_token_first));
        println!("multi-token last      {:.3}%", pct(self.multi_token_last));
        println!("hyphenated first      {:.3}%", pct(self.hyphenated_first));
        println!("hyphenated last       {:.3}%", pct(self.hyphenated_last));
        println!("apostrophe            {:.3}%", pct(self.apostrophe));
        println!("non-ASCII             {:.3}%", pct(self.non_ascii));
        println!("not title case        {:.3}%", pct(self.non_titlecase));
    }
}

/// True if `s` is a single leading uppercase followed by lowercase, the
/// canonical form the orthography model will assume by default.
fn is_titlecase(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        None => false,
        Some(c) if !c.is_uppercase() => false,
        Some(_) => chars.all(|c| c.is_lowercase()),
    }
}

/// Reads a byte count, accepting `k`, `M` and `G` suffixes as powers of 1024.
fn parse_size(text: &str) -> Option<usize> {
    let text = text.trim();
    let digits = text.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let suffix = text[digits.len()..].to_ascii_lowercase();
    let scale: usize = match suffix.trim_end_matches('b').trim_end_matches('i') {
        "" => 1,
        "k" => 1 << 10,
        "m" => 1 << 20,
        "g" => 1 << 30,
        _ => return None,
    };
    digits.trim().parse::<usize>().ok()?.checked_mul(scale)
}

fn main() -> ExitCode {
    let Some(path) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: namecompress-tools <corpus.csv>");
        return ExitCode::FAILURE;
    };

    let arg = |name: &str| -> Option<String> { std::env::args().skip_while(|a| a != name).nth(1) };

    if std::env::args().any(|a| a == "--build-table") {
        let out = arg("--out").unwrap_or_else(|| "table.ncmp.xz".to_owned());
        let Some(target) = arg("--rough-target-size").as_deref().map(parse_size) else {
            eprintln!("--build-table requires --rough-target-size, e.g. 200k");
            return ExitCode::FAILURE;
        };
        let Some(target) = target else {
            eprintln!("could not read --rough-target-size; try 200k, 512KiB or 1M");
            return ExitCode::FAILURE;
        };
        // The wrong-table check is a table-wide setting, so it is chosen
        // here rather than per message. A modulus of 1 disables it outright
        // and costs nothing, which `--no-wrong-table-check` says out loud.
        let disabled = std::env::args().any(|a| a == "--no-wrong-table-check");
        let explicit = arg("--check").and_then(|a| a.parse::<u32>().ok());
        let check = match (disabled, explicit) {
            (true, Some(modulus)) if modulus != 1 => {
                eprintln!("--no-wrong-table-check contradicts --check {modulus}");
                return ExitCode::FAILURE;
            }
            (true, _) => 1,
            (false, Some(modulus)) => modulus,
            (false, None) => 16_384,
        };
        // The modulus is stored as a u16, and a larger value would be
        // truncated on the way out, leaving encoder and decoder disagreeing.
        if check == 0 || check > u32::from(u16::MAX) {
            eprintln!("--check must be between 1 and {}", u16::MAX);
            return ExitCode::FAILURE;
        }

        if let Err(err) = build::run(&path, std::path::Path::new(&out), target, check) {
            eprintln!("build-table: {err}");
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS;
    }

    if std::env::args().any(|a| a == "--cross") {
        let a = arg("--table").unwrap_or_else(|| "table.ncmp".to_owned());
        let b = arg("--other").expect("--other <table>");
        if let Err(err) = cross::run(&path, std::path::Path::new(&a), std::path::Path::new(&b)) {
            eprintln!("cross: {err}");
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS;
    }

    if std::env::args().any(|a| a == "--bench") {
        let table = arg("--table").unwrap_or_else(|| "table.ncmp".to_owned());
        if let Err(err) = bench::run(&path, std::path::Path::new(&table)) {
            eprintln!("bench: {err}");
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS;
    }

    if std::env::args().any(|a| a == "--sweep") {
        let prune = std::env::args()
            .skip_while(|a| a != "--prune")
            .nth(1)
            .and_then(|a| a.parse().ok())
            .unwrap_or(0);
        if let Err(err) = sweep::run(&path, std::path::Path::new("tables"), prune) {
            eprintln!("sweep: {err}");
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS;
    }

    if std::env::args().any(|a| a == "--eval") {
        if let Err(err) = eval::run(&path) {
            eprintln!("{}: {err}", path.display());
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS;
    }

    let records = match corpus::read(&path) {
        Ok(records) => records,
        Err(err) => {
            eprintln!("{}: {err}", path.display());
            return ExitCode::FAILURE;
        }
    };

    let mut firsts = Interner::default();
    let mut lasts = Interner::default();
    let mut joint: HashMap<(u32, u32), u64> = HashMap::new();
    let mut structure = Structure::default();

    for record in records {
        structure.observe(&record.first, &record.last);
        let f = firsts.observe(&record.first);
        let l = lasts.observe(&record.last);
        *joint.entry((f, l)).or_insert(0) += 1;
    }

    structure.report();

    let joint_counts: Vec<u64> = joint.values().copied().collect();
    let h_first = entropy_bits(firsts.counts());
    let h_last = entropy_bits(lasts.counts());
    let h_joint = entropy_bits(&joint_counts);
    let mutual_information = h_first + h_last - h_joint;

    println!("\n== entropy (bits) ==");
    println!("distinct first        {}", firsts.distinct());
    println!("distinct last         {}", lasts.distinct());
    println!("distinct pairs        {}", joint.len());
    println!("H(first)              {h_first:.3}");
    println!("H(last)               {h_last:.3}");
    println!("H(first, last)        {h_joint:.3}");
    println!("H(first) + H(last)    {:.3}", h_first + h_last);
    println!("mutual information    {mutual_information:.3}");
    println!(
        "\nindependent floor     {:.3} bytes",
        (h_first + h_last) / 8.0
    );
    println!("joint floor           {:.3} bytes", h_joint / 8.0);

    for (label, interner) in [("first", &firsts), ("last", &lasts)] {
        let order = interner.by_frequency();
        println!("\n== {label}: dictionary size vs cost ==");
        println!("      N   coverage   bits/name");
        for n in [100, 1_000, 5_000, 10_000, 25_000, 50_000, 100_000] {
            if n > order.len() {
                break;
            }
            println!(
                "{n:>7}   {:>7.3}%   {:>9.3}",
                100.0 * coverage(interner, &order, n),
                dictionary_cost_bits(interner, &order, n, ASSUMED_ESCAPE_BITS)
            );
        }
    }

    ExitCode::SUCCESS
}
