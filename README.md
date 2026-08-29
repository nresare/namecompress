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
| **namecompress**, 200KB table, no verification | **4.59** |
| **namecompress**, 200KB table, 14-bit verification | **6.35** |
| raw UTF-8 | 12.91 |
| raw deflate (headerless) | 14.87 |
| brotli q11 | 16.84 |
| zstd -19 + 500KB trained dictionary, magicless | 17.42 |
| gzip | 32.87 |

Round-trip is exact: zero failures over 1,149,425 held-out names.

[name-dataset]: https://github.com/philipperemy/name-dataset

The byte-level format is specified in [FORMAT.md](FORMAT.md), which is enough
to write a decoder in another language without reading this one.

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
- **Table-defined alphabet.** The characters the model covers are carried by
  the table, derived from its own corpus, so a Swedish table gets `å ä ö` and
  a Polish one `ł ą ę`. Hardcoding `a-z` would silently assume English
  orthography and push a fifth of Swedish names onto the raw fallback.
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

## Tables are per-geography

A table is only as good as the corpus behind it. Against the GB slice of the
same dataset, on identical held-out rows:

| | GB names | SE names |
|---|---|---|
| GB table | **6.39 B** (2.02x) | 9.92 B (1.45x) |
| SE table | — | **6.89 B** (2.09x) |

Using the wrong table costs about 55% and is still perfectly correct: the raw
fallback absorbs whatever the model cannot represent, and no round trip fails.

Swedish beats British slightly once it has its own table, because Swedish names
are longer (14.40 against 12.91 raw bytes) and no less predictable. Note also
that the SE table is smaller — 168 KiB against 208 KiB — since Sweden
contributes fewer distinct names.

## Table budget

State roughly how large the shipped table may be and the builder works out the
rest: how much of the budget the character model may take, how hard to prune
it, and how many dictionary entries fit in what is left. Sizes are measured on
the actually-compressed table rather than estimated, because compression of
front-coded names is not linear in the entry count.

| target | table | pruning | bytes/name |
|---|---|---|---|
| 50 KiB | 51,016 | 65536 | 6.78 |
| 100 KiB | 101,892 | 16384 | 6.54 |
| **200 KiB** | **203,640** | **4096** | **6.35** |
| 500 KiB | 493,540 | 1024 | 6.18 |
| 2 MiB | 2,093,744 | 256 | 6.06 |

The budget is barely binding. Forty times the table buys 11%, because the name
distribution's tail is flat enough that dictionary entries stop paying for
themselves quickly, so 200 KiB is the recommended operating point.

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

The only knob is roughly how large the shipped table may be. The output format
follows the file extension: `.xz`, `.zst`, or uncompressed.

```
cargo run --release -p namecompress-tools -- GB.csv --build-table \
    --rough-target-size 200k --out table.ncmp.xz
```

```
alphabet     63 characters, 99.87% character coverage
model        prune 4096, 50908 B of 51200 B allowance
dictionary   22943 given names, 45886 surnames
table        203640 B xz (539544 B raw), target 204800 B
```

xz is the default because it compresses this table best: front-coded names in
lexicographic order give it long-range structure that LZMA exploits and BWT
does not. On the same table bzip2 --best reaches 166,335 bytes and zstd -19
172,168, against 160,740 for xz.

```
cargo run --release -p namecompress-tools -- GB.csv --bench --table table.ncmp.xz
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
