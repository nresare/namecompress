//! The train/test split shared by every tool.
//!
//! This has to be one definition. The builder trains on the complement of what
//! the measurement tools evaluate on, so if the two ever disagreed a table
//! would be scored on rows it had already seen, and every reported figure
//! would flatter it.

/// One row in ten is held out.
const TEST_MODULUS: usize = 10;

/// Whether the row at `index` belongs to the held-out evaluation set.
pub fn is_held_out(index: usize) -> bool {
    index.is_multiple_of(TEST_MODULUS)
}
