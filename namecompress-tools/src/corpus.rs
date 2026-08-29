//! Loading of the name-dataset CSV format: `first,last,gender,country`,
//! no header, one row per person (duplicates are meaningful).

use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;

pub struct Record {
    pub first: String,
    pub last: String,
}

/// Streams records, skipping rows whose first or last name is empty.
pub fn read(path: &Path) -> io::Result<impl Iterator<Item = Record>> {
    let reader = BufReader::with_capacity(1 << 20, File::open(path)?);
    Ok(reader.lines().filter_map(|line| {
        let line = line.ok()?;
        let mut fields = line.split(',');
        let first = fields.next()?;
        let last = fields.next()?;
        if first.is_empty() || last.is_empty() {
            return None;
        }
        Some(Record {
            first: first.to_owned(),
            last: last.to_owned(),
        })
    }))
}
