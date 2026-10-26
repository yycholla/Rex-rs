use std::{fmt, num::NonZeroU16};

/// A terminal's logical dimensions, independent of pixels and display scale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridSize {
    columns: NonZeroU16,
    rows: NonZeroU16,
}

impl GridSize {
    /// Construct a grid with nonzero dimensions.
    ///
    /// # Errors
    /// Returns `InvalidGridSize` when either dimension is zero.
    pub fn new(columns: u16, rows: u16) -> Result<Self, InvalidGridSize> {
        Ok(Self {
            columns: NonZeroU16::new(columns).ok_or(InvalidGridSize)?,
            rows: NonZeroU16::new(rows).ok_or(InvalidGridSize)?,
        })
    }

    #[must_use]

    pub const fn columns(self) -> u16 {
        self.columns.get()
    }
    #[must_use]
    pub const fn rows(self) -> u16 {
        self.rows.get()
    }
}

impl Default for GridSize {
    #[allow(
        clippy::expect_used,
        reason = "The literal dimensions are checked at compile time; this cannot panic at runtime."
    )]
    fn default() -> Self {
        const {
            Self {
                columns: NonZeroU16::new(80).expect("default columns are nonzero"),
                rows: NonZeroU16::new(24).expect("default rows are nonzero"),
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidGridSize;

impl fmt::Display for InvalidGridSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("terminal rows and columns must both be greater than zero")
    }
}
impl std::error::Error for InvalidGridSize {}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "Tests assert fixture preconditions and should fail immediately when those preconditions break."
)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_dimensions_at_the_boundary() {
        assert_eq!(GridSize::new(0, 24), Err(InvalidGridSize));
        assert_eq!(GridSize::new(80, 0), Err(InvalidGridSize));
        assert_eq!(GridSize::new(0, 0), Err(InvalidGridSize));
        assert!(GridSize::new(u16::MAX, u16::MAX).is_ok());
    }
}
