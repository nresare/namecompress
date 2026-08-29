# namecompress

Compression of personal names against a separately distributed model table.

A name is coded as a draw from a known distribution rather than as a string.
That is the whole idea, and it is why the result is a few bytes where
general-purpose compressors *expand* the input: at this message size LZ has no
repetition to exploit and frame overhead alone exceeds the payload.

Measured on the GB slice of the [name-dataset][] corpus (11.5M rows, 6.2M
distinct pairs), training on 90% and evaluating on the untouched 10%:

| Method | bytes/name |
|---|---|
| **namecompress**, 200KB table, no verification | **4.66** |
| **namecompress**, 200KB table, 14-bit verification | **6.42** |
| raw UTF-8 | 12.91 |
| raw deflate (headerless) | 14.87 |
| brotli q11 | 16.84 |
| zstd -19 + 500KB trained dictionary, magicless | 17.42 |
| gzip | 32.87 |

Round-trip is exact: zero failures over 1,149,425 held-out names.

[name-dataset]: https://github.com/philipperemy/name-dataset

## Design

Decode order is: mode symbol, then either two name fields or a raw byte string,
then a verification symbol.

- **Arithmetic coding**, not rANS. rANS is the conventional modern choice and
  is faster, but its final state must be flushed in full — four bytes — which
  is untenable when the entire message is under five. An arithmetic coder
  terminates in two bits plus padding to the byte boundary.
- **Dictionary with escape** per field. In-dictionary names cost `-log2 p`.
- **Order-3 character model** for escaped names, Witten-Bell interpolated. The
  blend is computed in fixed-point integer arithmetic, not floating point, so
  encoder and decoder derive bit-identical frequency tables on every platform.
- **Case shapes.** Dictionary entries are canonical spellings; `SMITH`,
  `Smith`, and `smith` share one entry and pool their counts, with a shape
  symbol recovering the original.
- **Raw UTF-8 fallback.** The encoder tries both paths and emits the shorter,
  so the worst case is bounded at roughly the input length no matter how
  strange the name is.

Two things measurement ruled *out* of the design:

- **No cultural clustering.** The in-sample estimate said given name and
  surname share 4.51 bits of mutual information. On held-out data the realised
  gain is 0.32 bits — the rest was finite-sample bias from a joint table with
  6.1M cells. The EM cluster model was not worth its table bytes.
- **No reliance on structural corruption detection.** Measured at 0%. Every
  symbol has non-zero probability, so every bit string decodes to *some* valid
  name. A near-optimal code has no redundancy left to detect errors with.

## Table budget

The budget is barely binding — the name distribution's tail is flat enough that
dictionary entries stop paying for themselves quickly:

```
  100 KB  ->  4.03 bytes/name        500 KB  ->  3.71
  200 KB  ->  3.84                  1000 KB  ->  3.65
  300 KB  ->  3.78                  2000 KB  ->  3.61
```

(Model entropy, excluding coder termination and the verification symbol.)
Twenty times the table buys 3%, so 200KB is the recommended operating point.

## Wrong-table detection

Using the wrong table produces plausible but wrong output, and nothing detects
that for free. The verification symbol costs exactly `log2(M)` bits and bounds
the chance of accepting a foreign message at `1/M`:

| M | bits | bytes/name | measured silent corruption |
|---|---|---|---|
| 1 | 0 | 4.66 | 30.3% |
| 256 | 8 | 5.66 | 0.118% |
| 4096 | 12 | 6.17 | 0.0065% |
| 16384 | 14 | 6.42 | bounded below 0.0061% |

**Prefer carrying the table identity out of band** — in a column type, schema
version, or container header — where it costs nothing per message. In-band
verification is a 38% surcharge.

## Usage

### Command line

`namecompress` is a filter in the style of `gzip`: a name on standard input
becomes compressed bytes on standard output, and `-d` reverses that. The table
may be given raw or zstd-compressed; the format is detected from the file.

```
namecompress --table table.ncmp.zst    < name.txt > name.bin
namecompress --table table.ncmp.zst -d < name.bin
```

Either direction works in a pipe:

```
$ printf 'John Smith' | namecompress -t table.ncmp.zst | xxd
00000000: 04e6 1a50                                ...P

$ printf 'John Smith' | namecompress -t table.ncmp.zst |
      namecompress -t table.ncmp.zst -d
John Smith
```

As with `gzip`, compressed output is not written to a terminal unless `--force`
is given. Decoding against the wrong table fails rather than printing a
plausible but wrong name, and nothing reaches standard output:

```
$ namecompress -t other.ncmp.zst -d < name.bin
namecompress: message was coded against a different table
```

### Building a table

```
cargo run --release -p namecompress-tools -- GB.csv --build-table \
    --given 20000 --surnames 40000 --prune 16384 --check 16384 --out table.ncmp
zstd -19 table.ncmp

cargo run --release -p namecompress-tools -- GB.csv --bench --table table.ncmp
```

Other tool modes: `--eval` (held-out entropy), `--sweep` (table budget curves),
`--cross --other <table>` (wrong-table detection).

### Library

`namecompress` itself has no dependencies; the zstd and argument-parsing
dependencies live only in the command line crate.

```rust
let table = namecompress::Table::parse(&bytes).expect("valid table");
let packed = namecompress::compress(&table, "John Smith")?;
assert_eq!(namecompress::decompress(&table, &packed)?, "John Smith");
```
