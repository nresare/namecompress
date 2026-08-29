//! Binary-output arithmetic coder.
//!
//! rANS would be the conventional modern choice, but its final state must be
//! flushed in full — four bytes — which is untenable when the whole message
//! is under five. An arithmetic coder terminates in two bits plus padding to
//! the byte boundary, so essentially all of the output is payload.
//!
//! This is the classic CACM'87 construction with underflow counting, widened
//! to a 64-bit interval so cumulative frequency totals up to `MAX_TOTAL` are
//! exact.

/// Precision of the coding interval.
const TOP: u64 = 1 << 32;
const HALF: u64 = 1 << 31;
const QUARTER: u64 = 1 << 30;
const THREE_QUARTERS: u64 = 3 << 30;

/// Largest permitted cumulative frequency total. Bounded so that
/// `range * total` cannot overflow 64 bits.
pub const MAX_TOTAL: u32 = 1 << 24;

/// Writes bits most-significant first into a byte buffer.
#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    partial: u8,
    filled: u8,
}

impl BitWriter {
    fn push(&mut self, bit: bool) {
        self.partial = (self.partial << 1) | u8::from(bit);
        self.filled += 1;
        if self.filled == 8 {
            self.bytes.push(self.partial);
            self.partial = 0;
            self.filled = 0;
        }
    }

    /// Pads the final byte with zeros and returns the buffer.
    fn finish(mut self) -> Vec<u8> {
        if self.filled > 0 {
            self.bytes.push(self.partial << (8 - self.filled));
        }
        self.bytes
    }
}

/// Reads bits most-significant first, yielding zeros past the end so the
/// decoder can always fill its window.
struct BitReader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn pull(&mut self) -> u64 {
        let byte = self.bytes.get(self.position / 8).copied().unwrap_or(0);
        let bit = (byte >> (7 - self.position % 8)) & 1;
        self.position += 1;
        u64::from(bit)
    }
}

/// The interval shared by encoder and decoder.
struct Interval {
    low: u64,
    high: u64,
}

impl Interval {
    fn new() -> Self {
        Self {
            low: 0,
            high: TOP - 1,
        }
    }

    /// Narrows to the sub-interval `[start, start + size)` of `total`.
    fn narrow(&mut self, start: u32, size: u32, total: u32) {
        debug_assert!(size > 0 && start + size <= total && total <= MAX_TOTAL);
        let range = self.high - self.low + 1;
        self.high = self.low + range * u64::from(start + size) / u64::from(total) - 1;
        self.low += range * u64::from(start) / u64::from(total);
    }
}

pub struct Encoder {
    interval: Interval,
    pending: u64,
    out: BitWriter,
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Encoder {
    pub fn new() -> Self {
        Self {
            interval: Interval::new(),
            pending: 0,
            out: BitWriter::default(),
        }
    }

    /// Codes a symbol occupying `[start, start + size)` out of `total`.
    pub fn encode(&mut self, start: u32, size: u32, total: u32) {
        self.interval.narrow(start, size, total);
        loop {
            let Interval { low, high } = self.interval;
            if high < HALF {
                self.emit(false);
            } else if low >= HALF {
                self.emit(true);
                self.interval.low -= HALF;
                self.interval.high -= HALF;
            } else if low >= QUARTER && high < THREE_QUARTERS {
                // Straddling the midpoint: defer the bit, remembering that
                // whatever it turns out to be, the next ones are its opposite.
                self.pending += 1;
                self.interval.low -= QUARTER;
                self.interval.high -= QUARTER;
            } else {
                break;
            }
            self.interval.low <<= 1;
            self.interval.high = (self.interval.high << 1) | 1;
        }
    }

    fn emit(&mut self, bit: bool) {
        self.out.push(bit);
        for _ in 0..std::mem::take(&mut self.pending) {
            self.out.push(!bit);
        }
    }

    /// Emits the two bits that pin down the final interval, then pads.
    pub fn finish(mut self) -> Vec<u8> {
        self.pending += 1;
        let bit = self.interval.low >= QUARTER;
        self.emit(bit);
        self.out.finish()
    }
}

pub struct Decoder<'a> {
    interval: Interval,
    value: u64,
    input: BitReader<'a>,
}

impl<'a> Decoder<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        let mut input = BitReader::new(bytes);
        let mut value = 0;
        for _ in 0..32 {
            value = (value << 1) | input.pull();
        }
        Self {
            interval: Interval::new(),
            value,
            input,
        }
    }

    /// Returns a value in `[0, total)` locating the next symbol, which the
    /// caller maps back to a symbol before calling [`Decoder::advance`].
    pub fn target(&self, total: u32) -> u32 {
        let range = self.interval.high - self.interval.low + 1;
        (((self.value - self.interval.low + 1) * u64::from(total) - 1) / range) as u32
    }

    /// Consumes the symbol occupying `[start, start + size)` of `total`.
    pub fn advance(&mut self, start: u32, size: u32, total: u32) {
        self.interval.narrow(start, size, total);
        loop {
            let Interval { low, high } = self.interval;
            if high < HALF {
                // Nothing to subtract.
            } else if low >= HALF {
                self.value -= HALF;
                self.interval.low -= HALF;
                self.interval.high -= HALF;
            } else if low >= QUARTER && high < THREE_QUARTERS {
                self.value -= QUARTER;
                self.interval.low -= QUARTER;
                self.interval.high -= QUARTER;
            } else {
                break;
            }
            self.interval.low <<= 1;
            self.interval.high = (self.interval.high << 1) | 1;
            self.value = (self.value << 1) | self.input.pull();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic xorshift, to keep the tests dependency-free.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    /// Round-trips symbol sequences drawn from skewed distributions, since
    /// skew is exactly what the name model produces.
    #[test]
    fn round_trips_skewed_symbols() {
        let mut rng = Rng(0x2545F4914F6CDD1D);
        for trial in 0..200 {
            let symbols = 2 + (trial % 40);
            // Frequencies spanning three orders of magnitude.
            let freqs: Vec<u32> = (0..symbols)
                .map(|i| 1 + (rng.next() % (1 << (1 + i % 12))) as u32)
                .collect();
            let total: u32 = freqs.iter().sum();
            let cumulative: Vec<u32> = freqs
                .iter()
                .scan(0, |acc, &f| {
                    let start = *acc;
                    *acc += f;
                    Some(start)
                })
                .collect();

            let message: Vec<usize> = (0..1 + trial % 30)
                .map(|_| (rng.next() % symbols as u64) as usize)
                .collect();

            let mut encoder = Encoder::new();
            for &s in &message {
                encoder.encode(cumulative[s], freqs[s], total);
            }
            let bytes = encoder.finish();

            let mut decoder = Decoder::new(&bytes);
            for &expected in &message {
                let target = decoder.target(total);
                let s = cumulative
                    .iter()
                    .rposition(|&c| c <= target)
                    .expect("target within total");
                assert_eq!(s, expected, "trial {trial}");
                decoder.advance(cumulative[s], freqs[s], total);
            }
        }
    }

    /// A single near-certain symbol must cost essentially nothing.
    #[test]
    fn near_certain_symbol_is_cheap() {
        let mut encoder = Encoder::new();
        for _ in 0..64 {
            encoder.encode(0, 65_535, 65_536);
        }
        assert!(encoder.finish().len() <= 3);
    }
}
