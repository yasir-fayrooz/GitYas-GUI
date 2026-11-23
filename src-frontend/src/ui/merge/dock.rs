//! Pure, UI-free layout of the merge editor's three panes: a tree of row / column splits with the panes as leaves,
//! each exactly once. Panes are moved by docking one onto an edge of another (splitting it) or onto its centre
//! (swapping the two); splits are resized by their shares. Persisted in egui's memory, so it is validated on load.

use egui::{Pos2, Rect, pos2};
use serde::{Deserialize, Serialize};

/// A pane in the layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Slot {
    Ours,
    Theirs,
    Result,
}

impl Slot {
    pub const ALL: [Slot; 3] = [Slot::Ours, Slot::Theirs, Slot::Result];

    pub fn name(self) -> &'static str {
        match self {
            Slot::Ours => "Ours",
            Slot::Theirs => "Theirs",
            Slot::Result => "Result",
        }
    }
}

/// Direction children of a split are laid out in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Axis {
    /// Side by side (columns).
    Row,
    /// Above one another (rows).
    Column,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Node {
    Leaf(Slot),
    /// `shares` (one per child, summing to 1) are the children's fractions of the split's length.
    Split {
        axis: Axis,
        children: Vec<Node>,
        shares: Vec<f32>,
    },
}

/// Where a dragged pane lands on a target pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    Left,
    Right,
    Top,
    Bottom,
    /// Swap with the target.
    Center,
}

/// Smallest share a child can be resized to.
const MIN_SHARE: f32 = 0.08;

fn split(axis: Axis, children: Vec<Node>, shares: &[f32]) -> Node {
    Node::Split {
        axis,
        children,
        shares: shares.to_vec(),
    }
}

use Node::Leaf;
use Slot::{Ours, Result, Theirs};

/// The ready-made layouts the Layout menu offers, with their names.
pub fn presets() -> Vec<(&'static str, Node)> {
    vec![
        (
            "Result first",
            split(
                Axis::Row,
                vec![Leaf(Result), split(Axis::Column, vec![Leaf(Ours), Leaf(Theirs)], &[0.5, 0.5])],
                &[0.56, 0.44],
            ),
        ),
        (
            "Three columns",
            split(Axis::Row, vec![Leaf(Ours), Leaf(Result), Leaf(Theirs)], &[0.3, 0.4, 0.3]),
        ),
        (
            "Sides over result",
            split(
                Axis::Column,
                vec![split(Axis::Row, vec![Leaf(Ours), Leaf(Theirs)], &[0.5, 0.5]), Leaf(Result)],
                &[0.5, 0.5],
            ),
        ),
        (
            "Result over sides",
            split(
                Axis::Column,
                vec![Leaf(Result), split(Axis::Row, vec![Leaf(Ours), Leaf(Theirs)], &[0.5, 0.5])],
                &[0.55, 0.45],
            ),
        ),
        (
            "Stacked",
            split(Axis::Column, vec![Leaf(Ours), Leaf(Result), Leaf(Theirs)], &[0.3, 0.4, 0.3]),
        ),
        (
            "Sides first",
            split(
                Axis::Row,
                vec![split(Axis::Column, vec![Leaf(Ours), Leaf(Theirs)], &[0.5, 0.5]), Leaf(Result)],
                &[0.44, 0.56],
            ),
        ),
    ]
}

impl Default for Node {
    fn default() -> Self {
        presets().remove(0).1
    }
}

impl Node {
    /// Every slot exactly once, every split with 2+ children and matching, positive shares.
    pub fn is_valid(&self) -> bool {
        let mut seen = Vec::new();
        self.check(&mut seen) && seen.len() == Slot::ALL.len()
    }

    fn check(&self, seen: &mut Vec<Slot>) -> bool {
        match self {
            Leaf(slot) => {
                if seen.contains(slot) {
                    return false;
                }
                seen.push(*slot);
                true
            }
            Node::Split { children, shares, .. } => {
                children.len() >= 2
                    && shares.len() == children.len()
                    && shares.iter().all(|s| s.is_finite() && *s > 0.0)
                    && children.iter().all(|c| c.check(seen))
            }
        }
    }

    /// Same arrangement, ignoring the sizes.
    pub fn same_shape(&self, other: &Node) -> bool {
        match (self, other) {
            (Leaf(a), Leaf(b)) => a == b,
            (Node::Split { axis: a, children: ca, .. }, Node::Split { axis: b, children: cb, .. }) => {
                a == b && ca.len() == cb.len() && ca.iter().zip(cb).all(|(x, y)| x.same_shape(y))
            }
            _ => false,
        }
    }

    /// Each slot's rectangle when the layout fills `rect`, leaving `gap` between siblings.
    pub fn rects(&self, rect: Rect, gap: f32) -> Vec<(Slot, Rect)> {
        let mut out = Vec::new();
        self.collect_rects(rect, gap, &mut out);
        out
    }

    fn collect_rects(&self, rect: Rect, gap: f32, out: &mut Vec<(Slot, Rect)>) {
        match self {
            Leaf(slot) => out.push((*slot, rect)),
            Node::Split { axis, children, shares } => {
                for (child, r) in children.iter().zip(child_rects(*axis, shares, rect, gap)) {
                    child.collect_rects(r, gap, out);
                }
            }
        }
    }

    /// Moves `pane` to `zone` of `target`: an edge splits the target there, the centre swaps the two.
    pub fn dock(&mut self, pane: Slot, target: Slot, zone: Zone) {
        if pane == target {
            return;
        }
        if zone == Zone::Center {
            self.for_each_leaf(&mut |slot| {
                *slot = if *slot == pane {
                    target
                } else if *slot == target {
                    pane
                } else {
                    *slot
                };
            });
            return;
        }
        if !self.remove(pane) {
            return;
        }
        let (axis, after) = match zone {
            Zone::Left => (Axis::Row, false),
            Zone::Right => (Axis::Row, true),
            Zone::Top => (Axis::Column, false),
            Zone::Bottom => (Axis::Column, true),
            Zone::Center => unreachable!(),
        };
        self.insert_beside(pane, target, axis, after);
        self.flatten();
    }

    /// Merges a split into its parent when both run the same way (a removal can leave them nested), so every
    /// boundary along a row or column is one splitter of one split.
    fn flatten(&mut self) {
        let Node::Split { axis, children, shares } = self else { return };
        children.iter_mut().for_each(Node::flatten);
        let mut new_children = Vec::with_capacity(children.len());
        let mut new_shares = Vec::with_capacity(shares.len());
        for (child, share) in children.drain(..).zip(shares.iter()) {
            match child {
                Node::Split {
                    axis: inner,
                    children: grand,
                    shares: inner_shares,
                } if inner == *axis => {
                    let total: f32 = inner_shares.iter().sum::<f32>().max(f32::EPSILON);
                    new_children.extend(grand);
                    new_shares.extend(inner_shares.iter().map(|s| share * s / total));
                }
                child => {
                    new_children.push(child);
                    new_shares.push(*share);
                }
            }
        }
        *children = new_children;
        *shares = new_shares;
    }

    fn for_each_leaf(&mut self, f: &mut impl FnMut(&mut Slot)) {
        match self {
            Leaf(slot) => f(slot),
            Node::Split { children, .. } => children.iter_mut().for_each(|c| c.for_each_leaf(f)),
        }
    }

    /// Removes `pane`'s leaf (a split left with one child is replaced by it). False if `pane` is the root leaf.
    fn remove(&mut self, pane: Slot) -> bool {
        let Node::Split { children, shares, .. } = self else { return false };
        if let Some(i) = children.iter().position(|c| *c == Leaf(pane)) {
            children.remove(i);
            let gone = shares.remove(i);
            // The freed space goes to the neighbour that took its place.
            let to = i.min(shares.len() - 1);
            shares[to] += gone;
            if children.len() == 1 {
                *self = children.remove(0);
            }
            return true;
        }
        children.iter_mut().any(|c| c.remove(pane))
    }

    /// Puts `pane` before / after `target` along `axis`: into the target's parent split if it runs that way,
    /// otherwise in a new split replacing the target. Halves the target's space.
    fn insert_beside(&mut self, pane: Slot, target: Slot, axis: Axis, after: bool) {
        if *self == Leaf(target) {
            let children = if after {
                vec![Leaf(target), Leaf(pane)]
            } else {
                vec![Leaf(pane), Leaf(target)]
            };
            *self = split(axis, children, &[0.5, 0.5]);
            return;
        }
        let Node::Split {
            axis: own,
            children,
            shares,
        } = self
        else {
            return;
        };
        if let Some(i) = children.iter().position(|c| *c == Leaf(target)) {
            if *own == axis {
                let half = shares[i] / 2.0;
                shares[i] = half;
                let at = if after { i + 1 } else { i };
                children.insert(at, Leaf(pane));
                shares.insert(at, half);
            } else {
                children[i].insert_beside(pane, target, axis, after);
            }
            return;
        }
        for c in children {
            c.insert_beside(pane, target, axis, after);
        }
    }
}

/// Children's rectangles of a split filling `rect`.
pub fn child_rects(axis: Axis, shares: &[f32], rect: Rect, gap: f32) -> Vec<Rect> {
    let total: f32 = shares.iter().sum::<f32>().max(f32::EPSILON);
    let len = match axis {
        Axis::Row => rect.width(),
        Axis::Column => rect.height(),
    };
    let usable = (len - gap * (shares.len().saturating_sub(1)) as f32).max(0.0);
    let mut at = 0.0;
    shares
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let size = if i + 1 == shares.len() {
                usable - at
            } else {
                (usable * s / total).round()
            };
            let start = at + gap * i as f32;
            at += size;
            match axis {
                Axis::Row => Rect::from_min_max(
                    pos2(rect.left() + start, rect.top()),
                    pos2(rect.left() + start + size, rect.bottom()),
                ),
                Axis::Column => Rect::from_min_max(pos2(rect.left(), rect.top() + start), pos2(rect.right(), rect.top() + start + size)),
            }
        })
        .collect()
}

/// Moves the boundary after child `i` by `delta` (a fraction of the split's length), keeping both neighbours at
/// least [`MIN_SHARE`] (of the whole split).
pub fn resize(shares: &mut [f32], i: usize, delta: f32) {
    if i + 1 >= shares.len() {
        return;
    }
    let total: f32 = shares.iter().sum();
    let min = MIN_SHARE * total;
    let delta = delta * total;
    let delta = delta.clamp(min - shares[i], shares[i + 1] - min);
    shares[i] += delta;
    shares[i + 1] -= delta;
}

/// Which zone of `rect` the pointer is in: the edges are the outer quarter of each side (nearest edge wins),
/// the rest is the centre.
pub fn zone_at(rect: Rect, pos: Pos2) -> Zone {
    let fx = (pos.x - rect.left()) / rect.width().max(1.0);
    let fy = (pos.y - rect.top()) / rect.height().max(1.0);
    let edges = [(fx, Zone::Left), (1.0 - fx, Zone::Right), (fy, Zone::Top), (1.0 - fy, Zone::Bottom)];
    let (d, zone) = edges.into_iter().min_by(|a, b| a.0.total_cmp(&b.0)).unwrap_or((1.0, Zone::Center));
    if d < 0.25 { zone } else { Zone::Center }
}

/// The part of `rect` a pane dropped in `zone` would take (the drop preview).
pub fn zone_rect(rect: Rect, zone: Zone) -> Rect {
    let (w, h) = (rect.width() / 2.0, rect.height() / 2.0);
    match zone {
        Zone::Left => Rect::from_min_size(rect.min, egui::vec2(w, rect.height())),
        Zone::Right => Rect::from_min_max(pos2(rect.left() + w, rect.top()), rect.max),
        Zone::Top => Rect::from_min_size(rect.min, egui::vec2(rect.width(), h)),
        Zone::Bottom => Rect::from_min_max(pos2(rect.left(), rect.top() + h), rect.max),
        Zone::Center => rect,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(node: &Node) -> String {
        match node {
            Leaf(s) => s.name()[..1].to_owned(),
            Node::Split { axis, children, .. } => {
                let inner: Vec<String> = children.iter().map(order).collect();
                let sep = if *axis == Axis::Row { "|" } else { "/" };
                format!("({})", inner.join(sep))
            }
        }
    }

    #[test]
    fn presets_are_valid_and_default_is_result_first() {
        for (name, p) in presets() {
            assert!(p.is_valid(), "{name}");
        }
        assert_eq!(order(&Node::default()), "(R|(O/T))");
    }

    #[test]
    fn docking_on_an_edge_splits_the_target() {
        let mut n = Node::default();
        // Ours to the left of the result: the column holding theirs collapses into it.
        n.dock(Ours, Result, Zone::Left);
        assert_eq!(order(&n), "(O|R|T)");
        assert!(n.is_valid());
        // Theirs above ours: a new column in place of ours.
        n.dock(Theirs, Ours, Zone::Top);
        assert_eq!(order(&n), "((T/O)|R)");
        n.dock(Result, Theirs, Zone::Bottom);
        assert_eq!(order(&n), "(T/R/O)");
        assert!(n.is_valid());
    }

    #[test]
    fn docking_on_the_centre_swaps_and_on_itself_does_nothing() {
        let mut n = Node::default();
        n.dock(Ours, Result, Zone::Center);
        assert_eq!(order(&n), "(O|(R/T))");
        let before = n.clone();
        n.dock(Ours, Ours, Zone::Left);
        assert_eq!(n, before);
    }

    #[test]
    fn shares_stay_summed_after_moves() {
        let mut n = presets().remove(1).1;
        n.dock(Theirs, Ours, Zone::Bottom);
        n.dock(Result, Theirs, Zone::Right);
        let sum = |n: &Node| match n {
            Node::Split { shares, .. } => shares.iter().sum::<f32>(),
            Leaf(_) => 1.0,
        };
        assert!((sum(&n) - 1.0).abs() < 1e-5, "{n:?}");
        assert!(n.is_valid());
    }

    #[test]
    fn invalid_trees_are_rejected() {
        assert!(!Leaf(Ours).is_valid());
        assert!(!split(Axis::Row, vec![Leaf(Ours), Leaf(Ours), Leaf(Theirs)], &[0.3, 0.3, 0.4]).is_valid());
        assert!(!split(Axis::Row, vec![Leaf(Ours), Leaf(Result), Leaf(Theirs)], &[0.5, 0.5]).is_valid());
    }

    #[test]
    fn resize_clamps_to_the_minimum() {
        let mut s = vec![0.5, 0.5];
        resize(&mut s, 0, 0.2);
        assert!((s[0] - 0.7).abs() < 1e-6);
        resize(&mut s, 0, 5.0);
        assert!((s[1] - MIN_SHARE).abs() < 1e-6);
    }

    #[test]
    fn rects_tile_the_area() {
        let r = Rect::from_min_size(Pos2::ZERO, egui::vec2(1000.0, 600.0));
        let rects = Node::default().rects(r, 1.0);
        let area: f32 = rects.iter().map(|(_, r)| r.area()).sum();
        assert!(area > 0.99 * r.area() && area <= r.area());
        assert_eq!(zone_at(r, pos2(10.0, 300.0)), Zone::Left);
        assert_eq!(zone_at(r, pos2(500.0, 590.0)), Zone::Bottom);
        assert_eq!(zone_at(r, pos2(500.0, 300.0)), Zone::Center);
    }
}
