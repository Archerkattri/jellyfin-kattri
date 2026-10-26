//! Pure 2D focus navigation over rectangles.
//!
//! Given the on-screen rectangles of focusable items and the currently focused
//! item, [`move_focus`] picks the item that the directional key should move to.
//! No UI toolkit is involved, so the same rule works for egui, Slint or any
//! future front end, and the rule can be tested without a window.
//!
//! Rule, applied per direction:
//! 1. A candidate must lie strictly in the requested direction, measured between
//!    rectangle centres. Items at the same centre (or behind) are never candidates.
//! 2. Candidates whose perpendicular extent overlaps the current item (same row for
//!    Left/Right, same column for Up/Down) are preferred over ones that do not.
//! 3. Among candidates in the same class, the lowest score wins, where
//!    `score = primary_distance + 2 * perpendicular_offset`. This favours items
//!    aligned with the current one.
//! 4. Ties keep the earlier item in the slice, so ordering is deterministic.
//!
//! There is no wrap-around. If nothing qualifies the result is `None`, and the caller
//! decides whether to stay put, emit a boundary signal or move to a parent scope.

/// An axis-aligned rectangle in any consistent coordinate space (typically logical pixels).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    fn center(&self) -> (f32, f32) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    fn right(&self) -> f32 {
        self.x + self.width
    }

    fn bottom(&self) -> f32 {
        self.y + self.height
    }
}

/// A focus direction, from a directional key or D-pad.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

/// A focusable item: a caller-defined id and its on-screen rectangle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FocusItem<Id> {
    pub id: Id,
    pub rect: Rect,
}

/// Choose the item to focus after moving `direction` from `current`.
///
/// Returns `None` if `current` is not in `items` or no item lies in that direction.
pub fn move_focus<Id: Copy + PartialEq>(
    items: &[FocusItem<Id>],
    current: Id,
    direction: Direction,
) -> Option<Id> {
    let from = items.iter().find(|item| item.id == current)?.rect;
    let (cx, cy) = from.center();

    // (preferred_class, score, index) where a smaller tuple is better.
    let mut best: Option<(u8, f32, usize)> = None;

    for (index, item) in items.iter().enumerate() {
        if item.id == current {
            continue;
        }
        let (ix, iy) = item.rect.center();
        let (primary, perpendicular, aligned) = match direction {
            Direction::Right => (
                ix - cx,
                (iy - cy).abs(),
                spans_overlap(from.y, from.bottom(), item.rect.y, item.rect.bottom()),
            ),
            Direction::Left => (
                cx - ix,
                (iy - cy).abs(),
                spans_overlap(from.y, from.bottom(), item.rect.y, item.rect.bottom()),
            ),
            Direction::Down => (
                iy - cy,
                (ix - cx).abs(),
                spans_overlap(from.x, from.right(), item.rect.x, item.rect.right()),
            ),
            Direction::Up => (
                cy - iy,
                (ix - cx).abs(),
                spans_overlap(from.x, from.right(), item.rect.x, item.rect.right()),
            ),
        };
        if primary <= 0.0 {
            continue;
        }
        let class = u8::from(!aligned);
        let candidate = (class, primary + 2.0 * perpendicular, index);
        let better = match best {
            None => true,
            Some(current_best) => {
                candidate.0 < current_best.0
                    || (candidate.0 == current_best.0 && candidate.1 < current_best.1)
            }
        };
        if better {
            best = Some(candidate);
        }
    }

    best.map(|(_, _, index)| items[index].id)
}

fn spans_overlap(a_start: f32, a_end: f32, b_start: f32, b_end: f32) -> bool {
    a_start < b_end && b_start < a_end
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 3x3 grid of 100x100 cells with 10px gaps. Ids are 0..9 row-major.
    fn grid() -> Vec<FocusItem<u8>> {
        let mut items = Vec::new();
        for row in 0..3u8 {
            for col in 0..3u8 {
                items.push(FocusItem {
                    id: row * 3 + col,
                    rect: Rect::new(f32::from(col) * 110.0, f32::from(row) * 110.0, 100.0, 100.0),
                });
            }
        }
        items
    }

    #[test]
    fn moves_to_adjacent_cell_in_each_direction() {
        let g = grid();
        assert_eq!(move_focus(&g, 4, Direction::Right), Some(5));
        assert_eq!(move_focus(&g, 4, Direction::Left), Some(3));
        assert_eq!(move_focus(&g, 4, Direction::Up), Some(1));
        assert_eq!(move_focus(&g, 4, Direction::Down), Some(7));
    }

    #[test]
    fn edges_return_none_without_wrapping() {
        let g = grid();
        assert_eq!(move_focus(&g, 0, Direction::Left), None);
        assert_eq!(move_focus(&g, 0, Direction::Up), None);
        assert_eq!(move_focus(&g, 8, Direction::Right), None);
        assert_eq!(move_focus(&g, 8, Direction::Down), None);
    }

    #[test]
    fn unknown_current_item_returns_none() {
        assert_eq!(move_focus(&grid(), 42, Direction::Right), None);
    }

    #[test]
    fn same_row_item_beats_nearer_diagonal_item() {
        // A is the focus. B is far away in the same row. C is closer but in the next row.
        let items = [
            FocusItem {
                id: 'A',
                rect: Rect::new(0.0, 0.0, 100.0, 100.0),
            },
            FocusItem {
                id: 'C',
                rect: Rect::new(120.0, 150.0, 100.0, 100.0),
            },
            FocusItem {
                id: 'B',
                rect: Rect::new(400.0, 0.0, 100.0, 100.0),
            },
        ];
        assert_eq!(move_focus(&items, 'A', Direction::Right), Some('B'));
    }

    #[test]
    fn falls_back_to_nearest_non_aligned_item_when_no_row_mate_exists() {
        let items = [
            FocusItem {
                id: 1u8,
                rect: Rect::new(0.0, 0.0, 100.0, 100.0),
            },
            FocusItem {
                id: 2,
                rect: Rect::new(150.0, 180.0, 100.0, 100.0),
            },
            FocusItem {
                id: 3,
                rect: Rect::new(400.0, 400.0, 100.0, 100.0),
            },
        ];
        assert_eq!(move_focus(&items, 1, Direction::Right), Some(2));
    }

    #[test]
    fn items_behind_or_level_with_the_focus_are_never_chosen() {
        let items = [
            FocusItem {
                id: 1u8,
                rect: Rect::new(100.0, 0.0, 100.0, 100.0),
            },
            FocusItem {
                id: 2,
                rect: Rect::new(0.0, 0.0, 100.0, 100.0),
            },
        ];
        assert_eq!(move_focus(&items, 1, Direction::Right), None);
    }

    #[test]
    fn ties_resolve_to_the_earlier_item() {
        let items = [
            FocusItem {
                id: 'A',
                rect: Rect::new(0.0, 0.0, 10.0, 10.0),
            },
            FocusItem {
                id: 'B',
                rect: Rect::new(20.0, 0.0, 10.0, 10.0),
            },
            FocusItem {
                id: 'C',
                rect: Rect::new(20.0, 0.0, 10.0, 10.0),
            },
        ];
        assert_eq!(move_focus(&items, 'A', Direction::Right), Some('B'));
    }
}
