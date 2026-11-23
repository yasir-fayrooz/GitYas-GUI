//! Commit graph lane assignment (based on the original egui template's layout).
//!
//! Commits must be supplied newest-first in topological order (children before parents), as the engine sends them.
//! Each row gets a lane for its node plus the edges drawn within that row's bounds. Lane slots are stable: a branch
//! keeps its column until it merges or ends, and freed columns are reused (leftmost first).
//!
//! Robustness (the template assumed a complete, well-ordered list):
//! * A parent that is not in the list *below* the child (shallow clone, truncated history, or an ordering violation)
//!   never reserves a lane; otherwise that lane would run to the bottom of the graph. The row is flagged
//!   [`GraphRow::truncated`] and, when no other parent continues, gets a short dashed stub.
//! * Duplicate parent ids are drawn once.
//! * The WIP pseudo-row reserves lane 0 only if HEAD's commit is in the list; its link stays dashed through every row
//!   until it reaches HEAD (the template only dashed it when HEAD was the first row).
//!
//! Cost: O(n · lanes) time and edges (each row emits one edge per live lane), plus an O(n) id → row map.

use std::collections::HashMap;

use gityas_core::Oid;
use gityas_core::models::Commit;

/// A point inside a row: a lane index and a vertical fraction (0 = top, 1 = bottom).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Anchor {
    pub lane: usize,
    pub y: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Edge {
    pub from: Anchor,
    pub to: Anchor,
    /// Lane whose colour this edge is drawn in.
    pub color_lane: usize,
    /// WIP link, or a stub towards a parent outside the list.
    pub dashed: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GraphRow {
    pub lane: usize,
    pub edges: Vec<Edge>,
    /// Some parent of this commit is not in the list (shallow / truncated history); the WIP row sets it when HEAD's
    /// commit is not listed.
    pub truncated: bool,
}

#[derive(Clone, Debug, Default)]
pub struct GraphLayout {
    /// The WIP pseudo-row, if there are uncommitted changes.
    pub wip: Option<GraphRow>,
    pub rows: Vec<GraphRow>,
    pub max_lanes: usize,
}

pub const TOP: f32 = 0.0;
pub const MID: f32 = 0.5;
pub const BOTTOM: f32 = 1.0;
/// Where a stub towards a missing parent ends (short of the next row, so nothing connects to it).
pub const STUB_END: f32 = 0.85;

fn edge(from_lane: usize, from_y: f32, to_lane: usize, to_y: f32, color_lane: usize, dashed: bool) -> Edge {
    Edge {
        from: Anchor {
            lane: from_lane,
            y: from_y,
        },
        to: Anchor { lane: to_lane, y: to_y },
        color_lane,
        dashed,
    }
}

fn alloc_lane(lanes: &mut Vec<Option<Oid>>) -> usize {
    match lanes.iter().position(Option::is_none) {
        Some(i) => i,
        None => {
            lanes.push(None);
            lanes.len() - 1
        }
    }
}

pub fn layout(commits: &[Commit], wip_parent: Option<&Oid>) -> GraphLayout {
    let position: HashMap<&Oid, usize> = commits.iter().enumerate().map(|(i, c)| (&c.id, i)).collect();
    // Each slot holds the id of the commit that lane is waiting to reach.
    let mut lanes: Vec<Option<Oid>> = Vec::new();
    // The lane carrying the WIP row's link to HEAD (drawn dashed until HEAD is reached).
    let mut wip_lane: Option<usize> = None;
    let mut out = GraphLayout::default();

    if let Some(parent) = wip_parent {
        let listed = position.contains_key(parent);
        let mut row = GraphRow {
            truncated: !listed,
            ..GraphRow::default()
        };
        if listed {
            lanes.push(Some(parent.clone()));
            wip_lane = Some(0);
            row.edges.push(edge(0, MID, 0, BOTTOM, 0, true));
        }
        out.wip = Some(row);
        out.max_lanes = 1;
    }

    let mut drawable: Vec<&Oid> = Vec::new();
    for (index, commit) in commits.iter().enumerate() {
        let mut row = GraphRow::default();
        let col = match lanes.iter().position(|l| l.as_ref() == Some(&commit.id)) {
            Some(col) => col,
            None => alloc_lane(&mut lanes),
        };
        row.lane = col;

        // Top half: lanes flowing into this node (freed), and lanes passing straight through.
        for (i, slot) in lanes.iter_mut().enumerate() {
            let Some(target) = slot.as_ref() else { continue };
            let dashed = wip_lane == Some(i);
            if *target == commit.id {
                row.edges.push(edge(i, TOP, col, MID, i, dashed));
                *slot = None;
            } else {
                row.edges.push(edge(i, TOP, i, BOTTOM, i, dashed));
            }
        }
        if wip_lane.is_some_and(|l| lanes.get(l).is_some_and(Option::is_none)) {
            wip_lane = None;
        }

        // Bottom half: edges to the parents listed below this row. The first one continues in this lane; others
        // join a lane already waiting for them or open a new one. If another lane also awaits the first parent,
        // the two converge when it is reached (on the leftmost lane).
        drawable.clear();
        for parent in &commit.parents {
            if position.get(parent).is_some_and(|&p| p > index) && !drawable.contains(&parent) {
                drawable.push(parent);
            }
        }
        row.truncated = commit.parents.iter().any(|p| position.get(p).is_none_or(|&row| row <= index));
        for (n, parent) in drawable.iter().enumerate() {
            let j = if n == 0 {
                col
            } else if let Some(j) = lanes.iter().position(|l| l.as_ref() == Some(*parent)) {
                j
            } else {
                alloc_lane(&mut lanes)
            };
            lanes[j] = Some((*parent).clone());
            row.edges.push(edge(col, MID, j, BOTTOM, j, false));
        }
        if drawable.is_empty() && row.truncated {
            row.edges.push(edge(col, MID, col, STUB_END, col, true));
        }

        while matches!(lanes.last(), Some(None)) {
            lanes.pop();
        }
        out.max_lanes = out.max_lanes.max(lanes.len()).max(col + 1);
        out.rows.push(row);
    }

    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn id(name: &str) -> Oid {
        Oid::from_bytes(name.as_bytes()).unwrap_or_else(|e| panic!("{e}"))
    }

    fn c(name: &str, parents: &[&str]) -> Commit {
        Commit {
            id: id(name),
            parents: parents.iter().map(|p| id(p)).collect(),
            author_name: String::new(),
            author_email: String::new(),
            author_time: 0,
            committer_time: 0,
            summary: String::new(),
            description: String::new(),
            lossy: false,
        }
    }

    fn lanes_of(g: &GraphLayout) -> Vec<usize> {
        g.rows.iter().map(|r| r.lane).collect()
    }

    /// Structural invariants every layout must satisfy: lanes within `max_lanes`, edges into / out of the node
    /// meet the node's lane, and what leaves a row at the bottom enters the next row at the top (same lanes).
    fn check_invariants(g: &GraphLayout) {
        let rows: Vec<&GraphRow> = g.wip.iter().chain(&g.rows).collect();
        for (r, row) in rows.iter().enumerate() {
            assert!(row.lane < g.max_lanes.max(1), "row {r}: lane {} >= max {}", row.lane, g.max_lanes);
            for e in &row.edges {
                assert!(
                    e.from.lane < g.max_lanes && e.to.lane < g.max_lanes,
                    "row {r}: edge out of range {e:?}"
                );
                if e.to.y == MID {
                    assert_eq!(e.to.lane, row.lane, "row {r}: incoming edge misses the node");
                }
                if e.from.y == MID {
                    assert_eq!(e.from.lane, row.lane, "row {r}: outgoing edge misses the node");
                }
            }
            let leaving: BTreeSet<usize> = row.edges.iter().filter(|e| e.to.y == BOTTOM).map(|e| e.to.lane).collect();
            let entering: BTreeSet<usize> = rows
                .get(r + 1)
                .map(|next| next.edges.iter().filter(|e| e.from.y == TOP).map(|e| e.from.lane).collect())
                .unwrap_or_default();
            assert_eq!(leaving, entering, "row {r}: lanes leaving at the bottom must enter the next row");
        }
    }

    #[test]
    fn linear_history_uses_one_lane() {
        let commits = [c("c", &["b"]), c("b", &["a"]), c("a", &[])];
        let g = layout(&commits, None);
        assert_eq!(g.max_lanes, 1);
        assert_eq!(lanes_of(&g), [0, 0, 0]);
        assert!(g.rows.iter().all(|r| !r.truncated));
        check_invariants(&g);
    }

    #[test]
    fn merge_opens_and_closes_second_lane() {
        // m merges x into b; both descend from a.
        let commits = [c("m", &["b", "x"]), c("x", &["a"]), c("b", &["a"]), c("a", &[])];
        let g = layout(&commits, None);
        assert_eq!(g.max_lanes, 2);
        assert_eq!(lanes_of(&g), [0, 1, 0, 0]);
        check_invariants(&g);
    }

    #[test]
    fn octopus_merge_opens_one_lane_per_parent() {
        let commits = [
            c("m", &["p1", "p2", "p3"]),
            c("p3", &["base"]),
            c("p2", &["base"]),
            c("p1", &["base"]),
            c("base", &[]),
        ];
        let g = layout(&commits, None);
        assert_eq!(g.max_lanes, 3);
        let merge_edges: Vec<usize> = g.rows[0].edges.iter().filter(|e| e.from.y == MID).map(|e| e.to.lane).collect();
        assert_eq!(merge_edges, [0, 1, 2]);
        assert_eq!(lanes_of(&g), [0, 2, 1, 0, 0]);
        // All three lanes converge on the base commit.
        assert_eq!(g.rows[4].edges.iter().filter(|e| e.to.y == MID).count(), 3);
        assert_eq!(g.rows[4].lane, 0);
        check_invariants(&g);
    }

    #[test]
    fn many_branch_tips_get_their_own_lanes_and_free_them() {
        let names: Vec<String> = (0..10).map(|i| format!("tip{i}")).collect();
        let mut commits: Vec<Commit> = names.iter().map(|n| c(n, &["base"])).collect();
        commits.push(c("base", &["root"]));
        commits.push(c("root", &[]));
        let g = layout(&commits, None);
        assert_eq!(g.max_lanes, 10);
        assert_eq!(lanes_of(&g)[..10], (0..10).collect::<Vec<_>>()[..]);
        // Base gathers all ten lanes into lane 0; root is back to one lane.
        assert_eq!(g.rows[10].lane, 0);
        assert_eq!(g.rows[10].edges.iter().filter(|e| e.to.y == MID).count(), 10);
        assert_eq!(g.rows[11].edges.len(), 1);
        check_invariants(&g);
    }

    #[test]
    fn independent_roots_reuse_freed_lanes() {
        // Two unrelated histories (e.g. an orphan gh-pages branch) interleaved by date.
        let commits = [c("a2", &["a1"]), c("b2", &["b1"]), c("a1", &[]), c("b1", &[])];
        let g = layout(&commits, None);
        assert_eq!(lanes_of(&g), [0, 1, 0, 1]);
        assert_eq!(g.max_lanes, 2);
        assert!(
            g.rows[2].edges.iter().all(|e| e.to.y != BOTTOM || e.to.lane == 1),
            "root a1 ends lane 0"
        );
        check_invariants(&g);
        // After both roots nothing is reserved: a later unrelated commit would start at lane 0 again.
        let more = [c("x", &[]), c("y", &[])];
        assert_eq!(lanes_of(&layout(&more, None)), [0, 0]);
    }

    #[test]
    fn parents_outside_the_list_do_not_reserve_lanes() {
        // Shallow clone: the oldest listed commit's parent is missing; a side branch also points outside.
        let commits = [c("m", &["b", "gone"]), c("b", &["a"]), c("a", &["missing"])];
        let g = layout(&commits, None);
        assert_eq!(g.max_lanes, 1, "no lane for parents that never appear");
        assert!(g.rows[0].truncated && !g.rows[1].truncated && g.rows[2].truncated);
        let stub = g.rows[2].edges.last().copied();
        assert_eq!(stub.map(|e| (e.dashed, e.to.y)), Some((true, STUB_END)));
        check_invariants(&g);
    }

    #[test]
    fn parent_listed_above_its_child_is_treated_as_missing() {
        // Ordering violation (parent before child): must not leave a lane open to the bottom.
        let commits = [c("a", &[]), c("b", &["a"]), c("z", &[])];
        let g = layout(&commits, None);
        assert_eq!(g.max_lanes, 1);
        assert!(g.rows[1].truncated);
        check_invariants(&g);
    }

    #[test]
    fn duplicate_parents_are_drawn_once() {
        let commits = [c("m", &["a", "a"]), c("a", &[])];
        let g = layout(&commits, None);
        assert_eq!(g.max_lanes, 1);
        assert_eq!(g.rows[0].edges.len(), 1);
        check_invariants(&g);
    }

    #[test]
    fn wip_row_sits_on_head_lane() {
        let commits = [c("b", &["a"]), c("a", &[])];
        let g = layout(&commits, Some(&id("b")));
        assert_eq!(g.wip.as_ref().map(|w| w.lane), Some(0));
        assert_eq!(g.rows[0].lane, 0);
        assert!(g.rows[0].edges[0].dashed);
        check_invariants(&g);
    }

    #[test]
    fn wip_link_stays_dashed_until_head_when_head_is_not_the_newest_commit() {
        // Another branch tip `f` is newer than HEAD `h`.
        let commits = [c("f", &["a"]), c("h", &["a"]), c("a", &[])];
        let g = layout(&commits, Some(&id("h")));
        assert_eq!(lanes_of(&g), [1, 0, 0]);
        let pass = g.rows[0].edges.iter().find(|e| e.from.lane == 0).copied();
        assert_eq!(pass.map(|e| (e.dashed, e.to.y)), Some((true, BOTTOM)));
        let into_head = g.rows[1].edges.iter().find(|e| e.from.lane == 0 && e.from.y == TOP).copied();
        assert_eq!(into_head.map(|e| (e.dashed, e.to.y)), Some((true, MID)));
        // Below HEAD nothing is dashed.
        assert!(g.rows[2].edges.iter().all(|e| !e.dashed));
        check_invariants(&g);
    }

    #[test]
    fn wip_parent_outside_the_list_reserves_nothing() {
        let commits = [c("b", &["a"]), c("a", &[])];
        let g = layout(&commits, Some(&id("elsewhere")));
        let wip = g.wip.as_ref().unwrap_or_else(|| panic!("wip row"));
        assert!(wip.truncated && wip.edges.is_empty());
        assert_eq!(lanes_of(&g), [0, 0]);
        assert!(g.rows.iter().flat_map(|r| &r.edges).all(|e| !e.dashed));
    }

    #[test]
    fn random_dags_satisfy_the_invariants() {
        // Deterministic LCG: random DAGs in topological order (parents always later), with merges, roots, missing
        // parents and duplicates.
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = |n: u64| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (seed >> 33) % n.max(1)
        };
        for _ in 0..200 {
            let n = 1 + next(60) as usize;
            let names: Vec<String> = (0..n).map(|i| format!("c{i}")).collect();
            let commits: Vec<Commit> = (0..n)
                .map(|i| {
                    let parent_count = match next(10) {
                        0 => 0,
                        1 => 2,
                        2 => 3,
                        _ => 1,
                    };
                    let parents: Vec<String> = (0..parent_count)
                        .map(|_| {
                            if next(15) == 0 || i + 1 >= n {
                                "outside".to_owned()
                            } else {
                                names[i + 1 + next((n - i - 1) as u64) as usize].clone()
                            }
                        })
                        .collect();
                    let refs: Vec<&str> = parents.iter().map(String::as_str).collect();
                    c(&names[i], &refs)
                })
                .collect();
            let wip = (next(2) == 0).then(|| id(&names[next(n as u64) as usize]));
            let g = layout(&commits, wip.as_ref());
            assert_eq!(g.rows.len(), n);
            check_invariants(&g);
        }
    }
}
