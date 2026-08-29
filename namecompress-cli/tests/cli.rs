//! End-to-end tests driving the built binary.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use namecompress::chars::CharModelBuilder;
use namecompress::{Table, TableBuilder};

const BINARY: &str = env!("CARGO_BIN_EXE_namecompress");

fn build_table(check_modulus: u32, surname: &str) -> Table {
    let alphabet =
        namecompress::chars::Alphabet::new("abcdefghijklmnopqrstuvwxyzåäö -'".chars().collect())
            .expect("valid alphabet");
    let mut builder = CharModelBuilder::new(alphabet.symbols());
    for name in ["smith", "jones", "okonkwo", "anna-karin"] {
        builder.train(&alphabet.encode(name).expect("in alphabet"), 100);
    }
    let chars = builder.build(0);
    TableBuilder {
        given: vec![("John".into(), 5000), ("Sarah".into(), 3000)],
        given_escape: 900,
        surname: vec![(surname.into(), 4000), ("Jones".into(), 2000)],
        surname_escape: 900,
        alphabet,
        chars,
        check_modulus,
    }
    .finish()
}

/// Writes a table, optionally zstd-compressed, and returns its path.
fn write_table(directory: &Path, name: &str, table: &Table, compress: bool) -> PathBuf {
    let path = directory.join(name);
    let bytes = table.write();
    let bytes = if compress {
        zstd::stream::encode_all(&bytes[..], 19).expect("compresses")
    } else {
        bytes
    };
    std::fs::write(&path, bytes).expect("writes table");
    path
}

/// Runs the binary, returning its exit success flag and captured output.
fn run(args: &[&str], input: &[u8]) -> (bool, Vec<u8>, String) {
    let mut child = Command::new(BINARY)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("binary runs");
    child
        .stdin
        .take()
        .expect("stdin piped")
        .write_all(input)
        .expect("writes input");
    let output = child.wait_with_output().expect("binary completes");
    (
        output.status.success(),
        output.stdout,
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn round_trips_through_the_binary() {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let table = build_table(16_384, "Smith");

    // Both a raw and a zstd-compressed table must be accepted.
    for (name, compress) in [("plain.ncmp", false), ("packed.ncmp.zst", true)] {
        let path = write_table(directory, name, &table, compress);
        let path = path.to_str().expect("utf-8 path");

        for subject in ["John Smith", "Sarah Jones", "John Okonkwo", "José Müller"] {
            let (ok, packed, err) = run(&["-t", path], subject.as_bytes());
            assert!(ok, "encode {subject}: {err}");

            let (ok, out, err) = run(&["-t", path, "-d"], &packed);
            assert!(ok, "decode {subject}: {err}");
            assert_eq!(String::from_utf8_lossy(&out).trim_end(), subject);
        }
    }
}

#[test]
fn writes_raw_bytes_not_text() {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let path = write_table(directory, "raw.ncmp", &build_table(16_384, "Smith"), false);
    let path = path.to_str().expect("utf-8 path");

    let (ok, packed, err) = run(&["-t", path], b"John Smith
");
    assert!(ok, "compress: {err}");
    // A common name must land in a handful of bytes, which also rules out any
    // textual encoding of the output.
    assert!(packed.len() <= 6, "unexpectedly long output: {packed:?}");

    let (ok, out, err) = run(&["-t", path, "-d"], &packed);
    assert!(ok, "decompress: {err}");
    assert_eq!(String::from_utf8_lossy(&out).trim_end(), "John Smith");
}

/// A message decoded against a different table must be reported, not printed.
#[test]
fn reports_a_mismatched_table() {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let mine = write_table(directory, "mine.ncmp", &build_table(16_384, "Smith"), false);
    let other = write_table(directory, "other.ncmp", &build_table(16_384, "Brown"), false);

    let (ok, packed, _) = run(&["-t", mine.to_str().unwrap()], b"John Smith");
    assert!(ok);

    let (ok, out, err) = run(&["-t", other.to_str().unwrap(), "-d"], &packed);
    assert!(!ok, "mismatched table must fail");
    assert!(out.is_empty(), "nothing may be printed on failure");
    assert!(err.contains("different table"), "unexpected error: {err}");
}

#[test]
fn rejects_bad_input() {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let path = write_table(directory, "bad.ncmp", &build_table(16_384, "Smith"), false);
    let path = path.to_str().expect("utf-8 path");

    for (args, input) in [
        (vec!["-t", path], &b""[..]),
        (vec!["-t", path], &b"\n"[..]),
        (vec!["-t", path, "-d"], &b""[..]),
    ] {
        let (ok, _, err) = run(&args, input);
        assert!(!ok, "expected failure for {args:?}");
        assert!(err.starts_with("namecompress: "), "unexpected error: {err}");
    }

    let (ok, _, err) = run(&["-t", "/nonexistent/table.ncmp", "-d"], b"\x00");
    assert!(!ok);
    assert!(err.contains("namecompress: "), "unexpected error: {err}");
}
