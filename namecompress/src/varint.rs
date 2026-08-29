//! LEB128 varints, used throughout the table format.

pub fn push(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// Reads a varint, advancing `cursor`. Returns `None` on a truncated or
/// over-long encoding.
pub fn read(bytes: &[u8], cursor: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let byte = *bytes.get(*cursor)?;
        *cursor += 1;
        value |= u64::from(byte & 0x7f).checked_shl(shift)?;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}
