//! Toolkit-independent focus graph shared by keyboard and gamepad navigation.

use std::collections::HashSet;

/// Stable identity for one focusable UI element.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FocusId(String);

impl FocusId {
    /// Creates a stable ID for a focusable element.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the ID's stable string value.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for FocusId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for FocusId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

/// User intent independent of keyboard, gamepad, or UI toolkit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavAction {
    Up,
    Down,
    Left,
    Right,
    Select,
    Back,
    PlayPause,
    Menu,
}

/// One focusable element and its horizontal center in logical UI coordinates.
#[derive(Clone, Debug, PartialEq)]
pub struct FocusNode {
    /// Stable identity used to preserve focus across list updates.
    pub id: FocusId,
    /// Horizontal center in logical UI coordinates.
    pub center_x: f32,
}

impl FocusNode {
    /// Creates a focus target at a logical horizontal position.
    pub fn new(id: impl Into<FocusId>, center_x: f32) -> Self {
        Self {
            id: id.into(),
            center_x,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Error returned when one graph contains the same focus ID more than once.
pub struct DuplicateFocusId(pub FocusId);

impl std::fmt::Display for DuplicateFocusId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "duplicate focus id `{}`", self.0.as_str())
    }
}

impl std::error::Error for DuplicateFocusId {}

/// Ordered rows of focus targets, with stable focus and per-row index memory.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FocusGraph {
    rows: Vec<Vec<FocusNode>>,
    focused: Option<FocusId>,
    last_focused_indices: Vec<usize>,
}

impl FocusGraph {
    /// Creates a graph from ordered rows, rejecting duplicate IDs.
    pub fn new(rows: Vec<Vec<FocusNode>>) -> Result<Self, DuplicateFocusId> {
        let mut graph = Self::default();
        graph.set_rows(rows)?;
        Ok(graph)
    }

    /// Replace the visible rows without losing a still-present focus target.
    /// If it disappeared, use its old index in the same row, then the nearest
    /// non-empty row. Duplicate IDs are rejected rather than made ambiguous.
    /// Replaces the current ordered rows and preserves or restores focus.
    pub fn set_rows(
        &mut self,
        rows: Vec<Vec<FocusNode>>,
    ) -> Result<Option<FocusId>, DuplicateFocusId> {
        let mut ids = HashSet::new();
        for node in rows.iter().flatten() {
            if !ids.insert(node.id.clone()) {
                return Err(DuplicateFocusId(node.id.clone()));
            }
        }

        let previous = self.focused.clone();
        let old_location = previous.as_ref().and_then(|id| self.locate(id));
        self.rows = rows;
        self.last_focused_indices.resize(self.rows.len(), 0);

        if let Some(id) = previous
            && let Some((row, index)) = self.locate(&id)
        {
            self.remember(row, index);
            self.focused = Some(id.clone());
            return Ok(Some(id));
        }

        let next = if let Some((old_row, old_index)) = old_location {
            self.fallback(old_row, old_index)
        } else {
            self.first_available()
        };
        self.focused = next.clone();
        Ok(next)
    }

    /// Returns the currently focused element, if any.
    pub fn current(&self) -> Option<&FocusId> {
        self.focused.as_ref()
    }

    /// Focuses an ID currently present in the graph.
    pub fn focus(&mut self, id: &FocusId) -> bool {
        let Some((row, index)) = self.locate(id) else {
            return false;
        };
        self.remember(row, index);
        self.focused = Some(id.clone());
        true
    }

    /// Move within the current row horizontally and to the nearest horizontal
    /// position in the next non-empty row vertically. Boundaries do not wrap.
    /// Non-navigation actions leave focus in place and return the current ID.
    /// Moves focus according to the graph rules and returns the resulting ID.
    pub fn navigate(&mut self, action: NavAction) -> Option<FocusId> {
        let id = self.focused.clone()?;
        let (row, index) = self.locate(&id)?;
        let target = match action {
            NavAction::Left if index > 0 => Some((row, index - 1)),
            NavAction::Right if index + 1 < self.rows[row].len() => Some((row, index + 1)),
            NavAction::Up => self.adjacent_row(row, -1, self.rows[row][index].center_x),
            NavAction::Down => self.adjacent_row(row, 1, self.rows[row][index].center_x),
            NavAction::Left
            | NavAction::Right
            | NavAction::Select
            | NavAction::Back
            | NavAction::PlayPause
            | NavAction::Menu => None,
        };
        if let Some((target_row, target_index)) = target {
            let target_id = self.rows[target_row][target_index].id.clone();
            self.remember(target_row, target_index);
            self.focused = Some(target_id.clone());
            Some(target_id)
        } else {
            Some(id)
        }
    }

    /// Returns the last focused item index for a row.
    pub fn last_focused_index(&self, row: usize) -> Option<usize> {
        self.last_focused_indices.get(row).copied()
    }

    /// Returns the row containing an ID.
    pub fn row_of(&self, id: &FocusId) -> Option<usize> {
        self.locate(id).map(|(row, _)| row)
    }

    /// Returns the item's index within its row.
    pub fn index_in_row(&self, id: &FocusId) -> Option<usize> {
        self.locate(id).map(|(_, index)| index)
    }

    fn locate(&self, id: &FocusId) -> Option<(usize, usize)> {
        self.rows.iter().enumerate().find_map(|(row, nodes)| {
            nodes
                .iter()
                .position(|node| &node.id == id)
                .map(|index| (row, index))
        })
    }

    fn remember(&mut self, row: usize, index: usize) {
        if let Some(last) = self.last_focused_indices.get_mut(row) {
            *last = index;
        }
    }

    fn first_available(&self) -> Option<FocusId> {
        self.rows
            .iter()
            .enumerate()
            .find(|(_, nodes)| !nodes.is_empty())
            .map(|(row, nodes)| {
                let index = self.last_focused_indices[row].min(nodes.len() - 1);
                nodes[index].id.clone()
            })
    }

    fn fallback(&mut self, old_row: usize, old_index: usize) -> Option<FocusId> {
        if self.rows.is_empty() {
            return None;
        }
        let anchor_row = old_row.min(self.rows.len() - 1);
        let target_row = if self.rows[anchor_row].is_empty() {
            self.rows
                .iter()
                .enumerate()
                .filter(|(_, nodes)| !nodes.is_empty())
                .min_by_key(|(row, _)| (row.abs_diff(anchor_row), *row))
                .map(|(row, _)| row)?
        } else {
            anchor_row
        };
        let target_index = old_index.min(self.rows[target_row].len() - 1);
        self.remember(target_row, target_index);
        Some(self.rows[target_row][target_index].id.clone())
    }

    fn adjacent_row(&self, row: usize, direction: isize, center_x: f32) -> Option<(usize, usize)> {
        let mut target_row = row as isize + direction;
        while (0..self.rows.len() as isize).contains(&target_row) {
            let nodes = &self.rows[target_row as usize];
            if !nodes.is_empty() {
                let target_index = nodes
                    .iter()
                    .enumerate()
                    .min_by(|(left_index, left), (right_index, right)| {
                        let left_distance = (left.center_x - center_x).abs();
                        let right_distance = (right.center_x - center_x).abs();
                        left_distance.total_cmp(&right_distance).then_with(|| {
                            let remembered = self.last_focused_indices[target_row as usize];
                            left_index
                                .abs_diff(remembered)
                                .cmp(&right_index.abs_diff(remembered))
                        })
                    })
                    .map(|(index, _)| index)?;
                return Some((target_row as usize, target_index));
            }
            target_row += direction;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, x: f32) -> FocusNode {
        FocusNode::new(id, x)
    }

    #[test]
    fn horizontal_moves_stay_in_row_and_do_not_wrap() {
        let mut graph = FocusGraph::new(vec![
            vec![node("a", 0.0), node("b", 100.0)],
            vec![node("c", 0.0)],
        ])
        .expect("unique ids");
        assert_eq!(graph.current(), Some(&FocusId::from("a")));
        assert_eq!(graph.navigate(NavAction::Left), Some(FocusId::from("a")));
        assert_eq!(graph.navigate(NavAction::Right), Some(FocusId::from("b")));
        assert_eq!(graph.navigate(NavAction::Right), Some(FocusId::from("b")));
        assert_eq!(graph.row_of(&FocusId::from("b")), Some(0));
    }

    #[test]
    fn vertical_moves_choose_nearest_horizontal_item_and_skip_empty_rows() {
        let mut graph = FocusGraph::new(vec![
            vec![node("left", 0.0), node("right", 100.0)],
            vec![],
            vec![node("near", 92.0), node("far", 250.0)],
        ])
        .expect("unique ids");
        graph.focus(&FocusId::from("right"));
        assert_eq!(graph.navigate(NavAction::Down), Some(FocusId::from("near")));
        assert_eq!(graph.navigate(NavAction::Up), Some(FocusId::from("right")));
        assert_eq!(graph.navigate(NavAction::Up), Some(FocusId::from("right")));
    }

    #[test]
    fn each_row_remembers_its_last_index() {
        let mut graph = FocusGraph::new(vec![
            vec![node("top-a", 0.0), node("top-b", 100.0)],
            vec![node("bottom-a", 0.0), node("bottom-b", 100.0)],
        ])
        .expect("unique ids");
        graph.focus(&FocusId::from("top-b"));
        graph.navigate(NavAction::Down);
        assert_eq!(graph.current(), Some(&FocusId::from("bottom-b")));
        graph.focus(&FocusId::from("top-b"));
        graph.navigate(NavAction::Down);
        assert_eq!(graph.last_focused_index(1), Some(1));
    }

    #[test]
    fn vertical_ties_prefer_the_target_rows_remembered_index() {
        let mut graph = FocusGraph::new(vec![
            vec![node("top", 50.0)],
            vec![node("bottom-left", 0.0), node("bottom-right", 100.0)],
        ])
        .expect("unique ids");
        graph.focus(&FocusId::from("bottom-right"));
        graph.focus(&FocusId::from("top"));
        assert_eq!(
            graph.navigate(NavAction::Down),
            Some(FocusId::from("bottom-right"))
        );
    }

    #[test]
    fn data_updates_preserve_focus_then_fall_back_by_index_and_nearest_row() {
        let mut graph = FocusGraph::new(vec![
            vec![node("a", 0.0), node("b", 100.0), node("c", 200.0)],
            vec![node("d", 0.0)],
        ])
        .expect("unique ids");
        graph.focus(&FocusId::from("c"));
        graph
            .set_rows(vec![
                vec![node("a", 0.0), node("c", 200.0)],
                vec![node("d", 0.0)],
            ])
            .expect("unique ids");
        assert_eq!(graph.current(), Some(&FocusId::from("c")));

        graph
            .set_rows(vec![vec![node("a", 0.0)], vec![node("d", 0.0)]])
            .expect("unique ids");
        assert_eq!(graph.current(), Some(&FocusId::from("a")));

        graph
            .set_rows(vec![vec![], vec![node("d", 0.0), node("e", 100.0)]])
            .expect("unique ids");
        assert_eq!(graph.current(), Some(&FocusId::from("d")));
    }

    #[test]
    fn removal_falls_back_to_the_previous_index_in_the_same_row() {
        let mut graph = FocusGraph::new(vec![vec![
            node("a", 0.0),
            node("b", 100.0),
            node("c", 200.0),
            node("d", 300.0),
        ]])
        .expect("unique ids");
        graph.focus(&FocusId::from("c"));
        graph
            .set_rows(vec![vec![
                node("a", 0.0),
                node("c", 200.0),
                node("d", 300.0),
            ]])
            .expect("unique ids");
        assert_eq!(graph.current(), Some(&FocusId::from("c")));
        graph
            .set_rows(vec![vec![node("a", 0.0), node("d", 300.0)]])
            .expect("unique ids");
        assert_eq!(graph.current(), Some(&FocusId::from("d")));
    }

    #[test]
    fn duplicate_ids_are_rejected_and_empty_graph_has_no_focus() {
        assert!(matches!(
            FocusGraph::new(vec![
                vec![node("duplicate", 0.0)],
                vec![node("duplicate", 10.0)]
            ]),
            Err(DuplicateFocusId(_))
        ));
        let mut graph = FocusGraph::default();
        graph.set_rows(vec![vec![], vec![]]).expect("empty rows");
        assert_eq!(graph.current(), None);
        assert_eq!(graph.navigate(NavAction::Select), None);
    }
}
