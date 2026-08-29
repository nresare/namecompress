# namecompress wire format

Table format version 1. This is everything needed to write a decoder without
reading the Rust, and exists because people are implementing one elsewhere —
a Swift decoder for iOS was the first.

Two artifacts are involved: the **table file**, read once at startup, and a
**message**, a few bytes per name.

> **The one rule that matters.** Every probability in this format is an
> integer. Encoder and decoder walk the same arithmetic in lockstep, and a
> single differing bit does not produce a decode error — it produces a
> different, entirely plausible name. Never introduce floating point. Where
> this document says integer division it means truncating division on unsigned
> integers, in the stated width.

Constants cited here come from `range.rs`, `chars.rs`, `table.rs` and
`codec.rs`. If the two ever disagree, the implementation is authoritative and
this document is the bug.

## 1. Primitives

### 1.1 Varints

Unsigned LEB128: seven bits per byte, little-endian, high bit set on every byte
but the last. Values fit in 64 bits; an encoding running past ten bytes is
malformed.

### 1.2 Bit reader

Messages are read most-significant-bit first within each byte. **Reading past
the end yields zero bits indefinitely** — this is required, not a convenience.
The final byte is zero-padded to a byte boundary and the decoder routinely asks
for bits beyond it.

```
byte = position >> 3 < len ? bytes[position >> 3] : 0
bit  = (byte >> (7 - (position & 7))) & 1
position += 1
```

## 2. Table file

### 2.1 Container

The builder writes the table compressed or raw according to the output file
extension. Detect it from the leading bytes:

| Leading bytes | Format |
|---|---|
| `28 B5 2F FD` | zstd frame |
| `FD 37 7A 58 5A 00` | xz stream |
| `4E 43 4D 50` (`NCMP`) | raw table, no container |

For a mobile decoder, prefer shipping the table **raw** and skipping the
decompressor: a 200 KiB compressed table is about 540 KB raw, the app bundle is
already compressed for distribution, and you avoid the dependency. Build it
with an output path ending in `.ncmp`.

Note that `--rough-target-size` applies to the file the builder actually
writes. Asking for 200k with a raw output path yields a 200 KiB *raw* table,
holding far fewer names than a 200 KiB compressed one.

### 2.2 Header

| Offset | Size | Type | Field |
|---|---|---|---|
| 0 | 4 | bytes | Magic, `"NCMP"` |
| 4 | 2 | u16 LE | Version, must be `1` |
| 6 | 2 | u16 LE | Check modulus *M*, non-zero (§6) |
| 8 | 4 | u32 LE | Table identifier |

Everything after offset 12 is varint-structured and read strictly in order:
alphabet, given-name dictionary, surname dictionary, character model.

### 2.3 Alphabet

The characters the model covers are carried by the table rather than fixed in
the code, so a Swedish table covers `å ä ö` and a Polish one `ł ą ę`.

| Field | Type | Notes |
|---|---|---|
| Character count *N* | varint | 1 ≤ *N* ≤ 63 |
| Characters | varint × *N* | Unicode scalar values, distinct and valid |

Symbol *i* for *i* < *N* is that character; symbol *N* is the **terminator**.
The symbol count is *N* + 1, at most 64.

### 2.4 Dictionaries

Two follow, given names first, then surnames, with identical structure.

| Field | Type | Notes |
|---|---|---|
| Frequency scale *S* | varint | Non-zero, ≤ 2²⁴ |
| Entry count *C* | varint | |
| Blob length *L* | varint | |
| Name blob | *L* bytes | Front-coded, below |
| Frequencies | varint × (*C*+1) | Entry frequencies, then the escape |

**Name blob.** Entries are in lexicographic byte order, each encoded as a `u8`
count of leading bytes shared with the previous name, the remaining bytes of
this name, then a `0x00` terminator. The name is
`previous[0..shared] + suffix`; the first entry has `shared == 0`.

The producer never splits a multi-byte character, so both halves are always
valid UTF-8. Reject a table where `shared` exceeds the previous name's length
or lands mid-character.

**Frequencies.** Build cumulative bounds in order: symbol *i* occupies
`[cum[i], cum[i] + freq[i])` out of *S*. Symbol *C*, the last, is the
**escape**. Every frequency is at least 1, and they must sum to exactly *S* —
if they do not, the table is corrupt. To map a coder target back to a symbol,
find the last index whose cumulative start is ≤ the target.

### 2.5 Character model

Four order tables follow, for orders 0, 1, 2 and 3 in that sequence. Each is a
varint context count *K*, then *K* entries of:

| Field | Type | Notes |
|---|---|---|
| Context delta | varint | Added to a running total starting at 0 |
| Present count *P* | varint | |
| Symbol / frequency | (u8, u8) × *P* | Frequency is never 0 |

Contexts are stored ascending and delta-coded, so accumulate. The first delta
is the absolute context key and may legitimately be 0.

Keep each context's **total** (sum of frequencies present) and **distinct**
(the count *P*) alongside its frequency array; both are needed in §4.

Context keys pack the preceding symbols at six bits each, most recent in the
low bits. For order 3 over the last three symbols *a*, *b*, *c* with *c* most
recent:

```
key = ((a << 6) | b) << 6 | c
```

Order 0 has exactly one possible key, `0`.

## 3. Arithmetic decoder

A CACM'87 arithmetic coder with underflow counting, over a 32-bit interval held
in 64-bit arithmetic. rANS would be the conventional modern choice, but its
four-byte state flush is untenable when the whole message is under five bytes;
this terminates in two bits plus padding.

| Constant | Value |
|---|---|
| `TOP` | 1 << 32 |
| `HALF` | 1 << 31 |
| `QUARTER` | 1 << 30 |
| `THREE_QUARTERS` | 3 << 30 |
| `MAX_TOTAL` | 1 << 24 |

Initialise `low = 0`, `high = TOP - 1`, and `value` to the first 32 bits of the
message, MSB first, zero-filled past the end.

Decoding a symbol is always two steps: ask where in the current distribution
the coder sits, map that to a symbol yourself, then tell the coder which
interval you took.

```
target(total):
    range = high - low + 1
    return ((value - low + 1) * total - 1) / range

advance(start, size, total):
    range = high - low + 1
    high = low + range * (start + size) / total - 1
    low  = low + range * start / total
    loop:
        if   high < HALF:                              pass
        elif low >= HALF:                              value -= HALF
                                                       low  -= HALF
                                                       high -= HALF
        elif low >= QUARTER and high < THREE_QUARTERS: value -= QUARTER
                                                       low  -= QUARTER
                                                       high -= QUARTER
        else:                                          break
        low   = low << 1
        high  = (high << 1) | 1
        value = (value << 1) | nextBit()
```

**Width hazard.** `range * (start + size)` reaches 2³² × 2²⁴ = 2⁵⁶. All three
state variables and every intermediate must be 64-bit. A 32-bit intermediate
truncates silently and the decoder diverges without erroring.

After normalisation `low`, `high` and `value` all stay below 2³², so no masking
is needed — but only if the loop is followed exactly.

## 4. Character model probabilities

Escaped names are coded character by character against an order-3 model with
Witten-Bell interpolation. The blend is integer fixed-point precisely so two
implementations agree bit for bit. The scale is `SCALE = 65536`.

Given the symbols decoded so far (`history`, terminator excluded):

```
symbolCount = alphabet.count + 1
uniform     = 65536 / symbolCount            # truncating
p[i]        = uniform for all i

for order in 0 ..= min(history.count, 3):
    key = packContext(last `order` symbols of history)
    ctx = model[order][key]; if absent: continue
    denom = ctx.total + ctx.distinct
    for i in 0 ..< symbolCount:
        p[i] = (ctx.counts[i] * 65536 + ctx.distinct * p[i]) / denom
    normalise(p)                             # after EVERY order
```

Three ways to get this wrong:

- **Normalise after every order**, not once at the end. The normalised vector
  feeds the next order's blend as its lower-order estimate.
- **Skip absent contexts, don't abort.** A missing context at order 2 does not
  stop order 3 being consulted.
- **The initial uniform vector need not sum to `SCALE`.** `65536 / symbolCount`
  truncates, so for 33 symbols it sums to 65538 — over. That value is used
  as-is as the order-0 lower estimate. Do not "fix" it.

Normalisation forces every entry non-zero, so any string stays codable, and the
total to exactly `SCALE`:

```
normalise(p):
    total = 0
    for i: p[i] = max(p[i], 1); total += p[i]
    largest = first index holding the maximum value      # lowest index wins ties
    p[largest] = p[largest] + 65536 - total              # may go negative, see below
```

**Signed arithmetic is required.** `total` can exceed `SCALE`, making
`SCALE - total` negative. The Rust implementation relies on release-mode
wrapping, which yields the same value signed arithmetic does. A language that
traps on unsigned underflow — Swift among them — must compute this in signed
64-bit and convert back.

**Tie-breaking matters.** The reference picks the *lowest* index among equal
maxima. Picking the highest gives a different distribution and a different
decode.

To decode one character, take `target(65536)`, walk the frequency array
accumulating a running start, and select the first index where
`target < start + p[i]`; then `advance` with that interval. Stop when the
symbol equals the terminator. Reject any field exceeding 64 characters — the
guard against a corrupt stream producing an unbounded run.

## 5. Message format

### 5.1 Mode

The first symbol selects the mode, out of a total of 65536:

| Mode | Interval | Meaning |
|---|---|---|
| Pair | `[0, 64500)` | Two modelled name fields |
| Raw | `[64500, 65536)` | Literal UTF-8 bytes |

The encoder tries both and emits whichever is shorter, so a decoder must
implement both paths.

### 5.2 Pair mode

Decode a field against the given-name dictionary, then a field against the
surname dictionary; the name is the two joined by a single space.

Each field, in order:

1. Take `target(S)` for that dictionary's scale, map it to a symbol, consume
   its interval.
2. If the symbol is the escape (index *C*), decode characters per §4 until the
   terminator and map them through the alphabet to get the **canonical form**.
   Otherwise the canonical form is the dictionary entry at that index.
3. Take `target(65536)` and map it to a case shape (§5.4). Consume its
   interval.
4. Apply the shape to the canonical form.

The encoder splits the input at its *final* space, so a given-name field may
itself contain spaces (`Mary Jane` + `Smith`). Rejoining with one space
reproduces the original string regardless of how the fields were divided.

### 5.3 Raw mode

1. Consume the mode interval `[64500, 65536)`.
2. Take `target(255)`, call it *n*, and consume `[n, n+1)` out of 255. The byte
   count is *n* + 1.
3. For each byte, take `target(256)` and consume `[b, b+1)` out of 256.
4. Decode as UTF-8; invalid UTF-8 is a malformed message.

Raw mode carries 1 to 255 bytes and bounds the worst case at roughly the input
length, whatever the name.

### 5.4 Case shapes

Dictionary entries store one canonical spelling. `SMITH`, `Smith` and `smith`
share an entry, and a shape symbol recovers the original.

| Shape | Interval | Transformation |
|---|---|---|
| As-is | `[0, 61000)` | The canonical form unchanged |
| Lower | `[61000, 63000)` | Full lowercase |
| Upper | `[63000, 64000)` | Full uppercase |
| Title | `[64000, 65536)` | First letter of each alphabetic run |

Title is the one needing care:

```
titleRuns(s):
    starting = true
    for scalar in s:                      # Unicode scalars, not grapheme clusters
        if scalar is Alphabetic:          # Unicode Alphabetic property
            append starting ? uppercase(scalar) : lowercase(scalar)
            starting = false
        else:
            append scalar
            starting = true
```

Two traps for a port:

- **Alphabetic, not letter.** Rust's `char::is_alphabetic` tests the Unicode
  *Alphabetic* property. Swift's `Character.isLetter` tests general category
  `L*`, a strictly smaller set; the right equivalent is
  `Unicode.Scalar.Properties.isAlphabetic`.
- **Case mapping expands.** Uppercasing one scalar can yield several — `ß`
  becomes `SS`. Append the resulting string rather than assuming one scalar in,
  one scalar out.

Iterate scalars, not grapheme clusters: the reference iterates Rust `char`s,
which are scalars, so a `Character`-based loop disagrees on any name containing
combining marks.

If case mapping does diverge, the verification symbol catches it — the check is
computed over the decoded name, so a name that comes out differently hashes
differently and is reported as a wrong-table error rather than silently
returned. Divergence fails loudly.

## 6. Verification symbol

FNV-1a over 64 bits, seeded with the table identifier then run over the decoded
name's UTF-8 bytes.

```
OFFSET = 0xcbf29ce484222325
PRIME  = 0x00000100000001b3

hash = OFFSET
for byte in tableId as 4 little-endian bytes:  hash ^= byte; hash *= PRIME   # wrapping
for byte in name.utf8:                          hash ^= byte; hash *= PRIME
check = hash % M
```

The multiply wraps by design; a language that traps on overflow needs its
wrapping operator (Swift's `&*`). Use the identifier read from the header —
there is no need to recompute it from the table contents.

After decoding the name, take `target(M)` and compare. No `advance` is needed;
it is the last symbol. A mismatch means the message was coded against a
different table: report it, never return the name.

The modulus costs log₂(*M*) bits per message and bounds the chance of accepting
a foreign message at 1/*M*. At the default *M* = 16384 that is 14 bits, roughly
1.75 bytes of a 6.4-byte message. There is no cheaper mechanism: structural
rejection measures at 0%, because every symbol carries non-zero probability and
so every bit string decodes to *some* valid name.

## 7. Test vectors

Generated against a raw table built from the GB corpus:

```
namecompress-tools GB.csv --build-table --rough-target-size 200k --out table.ncmp
```

| | |
|---|---|
| SHA-256 | `390b1c846880a51c0304c5865077050b9b1eacd48e03fc2505c24bc6334074c3` |
| Size | 203,060 bytes |
| Table id | `0x95585d0d` |
| Check modulus | 16384 |

| Name | Message | Bytes | Exercises |
|---|---|---|---|
| `John Smith` | `62cb4ef8` | 4 | Dictionary hit, as-is |
| `JOHN SMITH` | `63c31b8e03` | 5 | Upper shape |
| `john smith` | `63b64138c8` | 5 | Lower shape |
| `Sarah Jones` | `b54f25c4` | 4 | Dictionary hit |
| `Ada Lovelace` | `0167eb06b78e` | 6 | Dictionary hit |
| `Anna-Karin Nilsson` | `d5c9962902e257ead65c` | 10 | Hyphen, title shape |
| `Zaphod Beeblebrox` | `ef5dad485f8103d4c50d947056c6a0` | 15 | Escape, character model |
| `Siobhán O'Brien` | `fc3243d2804d56b77295b43a7675576779cf9848` | 20 | Raw fallback |

Generate more with the reference tool:

```
printf 'John Smith' | namecompress -t table.ncmp | xxd -p
```

## 8. Suggested implementation order

Build bottom-up and test each layer, because a fault in a lower layer surfaces
as a wrong name rather than an error.

1. **Varints and the bit reader.** Check the zero-fill past the end
   explicitly — it is easy to miss and breaks only short messages.
2. **Table parsing.** Assert every dictionary's frequencies sum to its scale.
   That single check catches most parsing slips immediately.
3. **Arithmetic decoder.** Verify against a dictionary-hit vector like
   `John Smith`, which touches no character model at all.
4. **Case shapes.** The upper and lower vectors exercise these without the
   character model.
5. **Character model.** `Zaphod Beeblebrox` escapes on both fields, so it
   exercises the blend, normalisation and terminator.
6. **Raw mode.** `Siobhán O'Brien` takes the fallback.
7. **Verification.** Enable it last, then confirm a message decoded against a
   *different* table is rejected.

Nothing above requires an encoder. Case shapes are chosen by the encoder and
merely applied by the decoder, so the reverse mapping is never needed, and the
table is read-only.
