//! Focus model for a 10-foot screen: a stack of horizontal rows, each holding items.
//!
//! Focus is a (row, column) pair. The model is pure: it holds only the row lengths and
//! the focus position, and it draws nothing.
//!
//! Rules:
//! - Left and right stop at the first and last item of the current row. Focus never
//!   wraps around to the other end.
//! - Up and down move to the nearest non-empty row in that direction and stop at the
//!   first or last non-empty row. Empty rows are skipped, so focus always lands on an
//!   item. Focus does not remember a column from an earlier row.
//! - When focus changes row, the column is clamped to the last item of the new row,
//!   so it always lies within that row's length.
//! - A model with no items has no focused item. Its moves do nothing.
//! - Items are laid out from cell 0 of their row, each `item_width` cells wide with no
//!   gap. An item is visible only when its whole span lies inside the viewport.

use crate::focus::Direction;

/// Focus position over rows of items. Build it with the length of each row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusGrid {
    row_lengths: Vec<usize>,
    row: usize,
    column: usize,
}

impl FocusGrid {
    /// Create a model with one entry per row giving that row's item count.
    ///
    /// Focus starts on the first item of the first non-empty row. If every row is
    /// empty, the model has no focused item.
    pub fn new(row_lengths: Vec<usize>) -> Self {
        let row = row_lengths
            .iter()
            .position(|&len| len > 0)
            .unwrap_or_default();
        Self {
            row_lengths,
            row,
            column: 0,
        }
    }

    /// Index of the focused row. Meaningful only when [`FocusGrid::current`] is `Some`.
    pub fn row(&self) -> usize {
        self.row
    }

    /// Index of the focused item within its row.
    pub fn column(&self) -> usize {
        self.column
    }

    /// The focused `(row, column)`, or `None` when the model has no items.
    pub fn current(&self) -> Option<(usize, usize)> {
        if self.current_row_len() == 0 {
            None
        } else {
            Some((self.row, self.column))
        }
    }

    /// Move focus one step in `direction`, following the rules in the module
    /// documentation. A move that would leave the grid leaves focus where it is.
    pub fn step(&mut self, direction: Direction) {
        match direction {
            Direction::Left => self.column = self.column.saturating_sub(1),
            Direction::Right => {
                if self.column + 1 < self.current_row_len() {
                    self.column += 1;
                }
            }
            Direction::Up => self.move_row(false),
            Direction::Down => self.move_row(true),
        }
    }

    /// Whether the focused item lies fully inside a viewport.
    ///
    /// `scroll_offset` is the first visible cell of the row and `viewport_width` is the
    /// number of visible cells. An item whose span only partly overlaps the viewport is
    /// not visible. Returns `false` when the model has no focused item.
    pub fn is_focus_visible(
        &self,
        item_width: usize,
        scroll_offset: usize,
        viewport_width: usize,
    ) -> bool {
        if self.current().is_none() {
            return false;
        }
        let start = self.column.saturating_mul(item_width);
        let end = start.saturating_add(item_width);
        let view_end = scroll_offset.saturating_add(viewport_width);
        start >= scroll_offset && end <= view_end
    }

    fn current_row_len(&self) -> usize {
        self.row_lengths.get(self.row).copied().unwrap_or_default()
    }

    /// Move to the nearest non-empty row above (`forward == false`) or below
    /// (`forward == true`), clamping the column to the new row's length.
    fn move_row(&mut self, forward: bool) {
        let lengths = &self.row_lengths;
        let is_populated = |row: &usize| lengths.get(*row).is_some_and(|&len| len > 0);
        let target = if forward {
            (self.row + 1..lengths.len()).find(is_populated)
        } else {
            (0..self.row).rev().find(is_populated)
        };
        if let Some(next) = target {
            self.row = next;
            let len = self.current_row_len();
            self.column = self.column.min(len.saturating_sub(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(grid: &FocusGrid) -> Option<(usize, usize)> {
        grid.current()
    }

    #[test]
    fn focus_starts_on_the_first_item_of_the_first_row() {
        let grid = FocusGrid::new(vec![3, 2]);
        assert_eq!(at(&grid), Some((0, 0)));
    }

    #[test]
    fn leading_empty_rows_are_skipped_at_start() {
        let grid = FocusGrid::new(vec![0, 0, 2]);
        assert_eq!(at(&grid), Some((2, 0)));
    }

    #[test]
    fn right_clamps_at_the_last_item_of_the_row() {
        let mut grid = FocusGrid::new(vec![3]);
        grid.step(Direction::Right);
        grid.step(Direction::Right);
        assert_eq!(at(&grid), Some((0, 2)));
        grid.step(Direction::Right);
        grid.step(Direction::Right);
        assert_eq!(at(&grid), Some((0, 2)));
    }

    #[test]
    fn left_clamps_at_the_first_item_of_the_row() {
        let mut grid = FocusGrid::new(vec![3]);
        grid.step(Direction::Left);
        assert_eq!(at(&grid), Some((0, 0)));
        grid.step(Direction::Right);
        grid.step(Direction::Left);
        grid.step(Direction::Left);
        assert_eq!(at(&grid), Some((0, 0)));
    }

    #[test]
    fn up_clamps_at_the_top_row_and_down_at_the_bottom_row() {
        let mut grid = FocusGrid::new(vec![2, 2]);
        grid.step(Direction::Up);
        assert_eq!(at(&grid), Some((0, 0)));
        grid.step(Direction::Down);
        grid.step(Direction::Down);
        assert_eq!(at(&grid), Some((1, 0)));
    }

    #[test]
    fn single_item_row_stays_put_in_every_direction() {
        let mut grid = FocusGrid::new(vec![1]);
        for direction in [
            Direction::Left,
            Direction::Right,
            Direction::Up,
            Direction::Down,
        ] {
            grid.step(direction);
            assert_eq!(at(&grid), Some((0, 0)), "{direction:?}");
        }
    }

    #[test]
    fn row_change_into_a_shorter_row_clamps_the_column() {
        let mut grid = FocusGrid::new(vec![4, 2]);
        for _ in 0..3 {
            grid.step(Direction::Right);
        }
        assert_eq!(at(&grid), Some((0, 3)));
        grid.step(Direction::Down);
        assert_eq!(at(&grid), Some((1, 1)));
        grid.step(Direction::Up);
        assert_eq!(at(&grid), Some((0, 1)));
    }

    #[test]
    fn column_does_not_return_to_an_earlier_longer_column() {
        let mut grid = FocusGrid::new(vec![4, 2, 4]);
        for _ in 0..3 {
            grid.step(Direction::Right);
        }
        grid.step(Direction::Down);
        grid.step(Direction::Down);
        assert_eq!(at(&grid), Some((2, 1)));
    }

    #[test]
    fn empty_rows_are_skipped_when_changing_row() {
        let mut grid = FocusGrid::new(vec![2, 0, 2]);
        grid.step(Direction::Right);
        grid.step(Direction::Down);
        assert_eq!(at(&grid), Some((2, 1)));
        grid.step(Direction::Up);
        assert_eq!(at(&grid), Some((0, 1)));
    }

    #[test]
    fn an_empty_row_at_the_edge_is_never_focused() {
        let mut grid = FocusGrid::new(vec![2, 0]);
        grid.step(Direction::Down);
        assert_eq!(at(&grid), Some((0, 0)));
    }

    #[test]
    fn empty_model_has_no_focus_and_ignores_moves() {
        let mut grid = FocusGrid::new(Vec::new());
        assert_eq!(at(&grid), None);
        for direction in [
            Direction::Left,
            Direction::Right,
            Direction::Up,
            Direction::Down,
        ] {
            grid.step(direction);
            assert_eq!(at(&grid), None, "{direction:?}");
        }
    }

    #[test]
    fn row_of_zero_items_has_no_focus() {
        let mut grid = FocusGrid::new(vec![0, 0]);
        assert_eq!(at(&grid), None);
        grid.step(Direction::Right);
        grid.step(Direction::Down);
        assert_eq!(at(&grid), None);
        assert!(!grid.is_focus_visible(100, 0, 1_000));
    }

    #[test]
    fn visible_item_fully_inside_the_viewport() {
        // Five items, 100 cells each. Viewport shows cells 0 to 299.
        let mut grid = FocusGrid::new(vec![5]);
        assert!(grid.is_focus_visible(100, 0, 300));
        grid.step(Direction::Right);
        grid.step(Direction::Right);
        assert!(grid.is_focus_visible(100, 0, 300));
    }

    #[test]
    fn item_just_past_the_right_edge_is_not_visible() {
        let mut grid = FocusGrid::new(vec![5]);
        for _ in 0..3 {
            grid.step(Direction::Right);
        }
        // Column 3 spans cells 300 to 399, outside the 0 to 299 viewport.
        assert!(!grid.is_focus_visible(100, 0, 300));
    }

    #[test]
    fn item_that_exactly_fills_the_last_cells_is_visible() {
        let mut grid = FocusGrid::new(vec![5]);
        grid.step(Direction::Right);
        grid.step(Direction::Right);
        // Column 2 spans cells 200 to 299, so its end meets the viewport edge exactly.
        assert!(grid.is_focus_visible(100, 0, 300));
    }

    #[test]
    fn item_left_of_the_scroll_offset_is_not_visible() {
        let grid = FocusGrid::new(vec![5]);
        // Column 0 spans 0 to 99 and the viewport starts at cell 150.
        assert!(!grid.is_focus_visible(100, 150, 300));
    }

    #[test]
    fn item_partly_scrolled_off_the_left_is_not_visible() {
        let mut grid = FocusGrid::new(vec![5]);
        grid.step(Direction::Right);
        // Column 1 spans 100 to 199. The viewport starts at 150, so it is cut off.
        assert!(!grid.is_focus_visible(100, 150, 300));
        grid.step(Direction::Right);
        // Column 2 spans 200 to 299 and sits inside 150 to 449.
        assert!(grid.is_focus_visible(100, 150, 300));
    }

    #[test]
    fn viewport_narrower_than_the_item_shows_nothing() {
        let grid = FocusGrid::new(vec![5]);
        assert!(!grid.is_focus_visible(100, 0, 99));
    }

    #[test]
    fn model_with_no_focus_is_never_visible() {
        let grid = FocusGrid::new(Vec::new());
        assert!(!grid.is_focus_visible(100, 0, 1_000));
    }

    #[test]
    fn row_and_column_accessors_track_focus() {
        let mut grid = FocusGrid::new(vec![2, 3]);
        grid.step(Direction::Right);
        grid.step(Direction::Down);
        assert_eq!(grid.row(), 1);
        assert_eq!(grid.column(), 1);
    }
}
