//! Workspaces → tabs → split tree of panes. Owned by orchd (so every client sees the same
//! layout and a detach never loses it); clients render it and ask orchd for changes.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dir {
    /// New pane to the right (side by side).
    Right,
    /// New pane below (stacked).
    Down,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Node {
    Pane {
        pane: String,
    },
    Split {
        dir: Dir,
        /// Share of the first child, 0.1 ..= 0.9.
        ratio: f32,
        a: Box<Node>,
        b: Box<Node>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tab {
    pub id: String,
    pub label: String,
    pub root: Node,
    pub focus: String,
    #[serde(default)]
    pub zoomed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub label: String,
    pub cwd: String,
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub version: u64,
    pub workspaces: Vec<Workspace>,
    #[serde(default)]
    pub next_id: u64,
}

/// Plain rectangle in terminal cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toward {
    Left,
    Right,
    Up,
    Down,
}

impl Node {
    pub fn leaf(pane: &str) -> Self {
        Node::Pane { pane: pane.into() }
    }

    pub fn panes(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<String>) {
        match self {
            Node::Pane { pane } => out.push(pane.clone()),
            Node::Split { a, b, .. } => {
                a.collect(out);
                b.collect(out);
            }
        }
    }

    pub fn contains(&self, pane: &str) -> bool {
        match self {
            Node::Pane { pane: p } => p == pane,
            Node::Split { a, b, .. } => a.contains(pane) || b.contains(pane),
        }
    }

    /// Replace `target` with a split of (target, new). Returns false if `target` is absent.
    pub fn split(&mut self, target: &str, new: &str, dir: Dir) -> bool {
        match self {
            Node::Pane { pane } if pane == target => {
                let old = std::mem::replace(self, Node::leaf(""));
                *self = Node::Split {
                    dir,
                    ratio: 0.5,
                    a: Box::new(old),
                    b: Box::new(Node::leaf(new)),
                };
                true
            }
            Node::Pane { .. } => false,
            Node::Split { a, b, .. } => a.split(target, new, dir) || b.split(target, new, dir),
        }
    }

    /// Remove `pane`; its sibling takes the parent's place. `None` = the tree is now empty.
    pub fn remove(self, pane: &str) -> Option<Node> {
        match self {
            Node::Pane { pane: p } if p == pane => None,
            leaf @ Node::Pane { .. } => Some(leaf),
            Node::Split { dir, ratio, a, b } => match (a.remove(pane), b.remove(pane)) {
                (Some(a), Some(b)) => Some(Node::Split {
                    dir,
                    ratio,
                    a: Box::new(a),
                    b: Box::new(b),
                }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            },
        }
    }

    /// Grow (`delta` > 0) or shrink the pane along the nearest split of direction `dir`.
    pub fn resize(&mut self, pane: &str, dir: Dir, delta: f32) -> bool {
        match self {
            Node::Pane { .. } => false,
            Node::Split {
                dir: d,
                ratio,
                a,
                b,
            } => {
                let in_a = a.contains(pane);
                let in_b = b.contains(pane);
                if !in_a && !in_b {
                    return false;
                }
                let deeper = if in_a {
                    a.resize(pane, dir, delta)
                } else {
                    b.resize(pane, dir, delta)
                };
                if deeper {
                    return true;
                }
                if *d != dir {
                    return false;
                }
                let signed = if in_a { delta } else { -delta };
                *ratio = (*ratio + signed).clamp(0.1, 0.9);
                true
            }
        }
    }

    /// Rectangles of every pane. Splits leave a 1-cell gap for the border between children.
    pub fn layout(&self, area: Rect) -> Vec<(String, Rect)> {
        let mut out = Vec::new();
        self.layout_into(area, &mut out);
        out
    }

    fn layout_into(&self, r: Rect, out: &mut Vec<(String, Rect)>) {
        match self {
            Node::Pane { pane } => out.push((pane.clone(), r)),
            Node::Split { dir, ratio, a, b } => {
                let (ra, rb) = split_rect(r, *dir, *ratio);
                a.layout_into(ra, out);
                b.layout_into(rb, out);
            }
        }
    }
}

fn split_rect(r: Rect, dir: Dir, ratio: f32) -> (Rect, Rect) {
    match dir {
        Dir::Right => {
            let usable = r.w.saturating_sub(1);
            let wa = ((usable as f32 * ratio).round() as u16).clamp(1.min(usable), usable);
            let wb = usable - wa;
            (
                Rect { w: wa, ..r },
                Rect {
                    x: r.x + wa + 1,
                    w: wb,
                    ..r
                },
            )
        }
        Dir::Down => {
            let usable = r.h.saturating_sub(1);
            let ha = ((usable as f32 * ratio).round() as u16).clamp(1.min(usable), usable);
            let hb = usable - ha;
            (
                Rect { h: ha, ..r },
                Rect {
                    y: r.y + ha + 1,
                    h: hb,
                    ..r
                },
            )
        }
    }
}

/// The pane whose rectangle lies next to `from` in direction `toward` (most overlap wins).
pub fn neighbor(rects: &[(String, Rect)], from: &str, toward: Toward) -> Option<String> {
    let (_, f) = rects.iter().find(|(p, _)| p == from)?;
    let overlap = |a0: u16, a1: u16, b0: u16, b1: u16| a1.min(b1) as i32 - a0.max(b0) as i32;
    rects
        .iter()
        .filter(|(p, _)| p != from)
        .filter_map(|(p, r)| {
            let (gap, ov) = match toward {
                Toward::Right => (
                    r.x as i32 - (f.x + f.w) as i32,
                    overlap(f.y, f.y + f.h, r.y, r.y + r.h),
                ),
                Toward::Left => (
                    f.x as i32 - (r.x + r.w) as i32,
                    overlap(f.y, f.y + f.h, r.y, r.y + r.h),
                ),
                Toward::Down => (
                    r.y as i32 - (f.y + f.h) as i32,
                    overlap(f.x, f.x + f.w, r.x, r.x + r.w),
                ),
                Toward::Up => (
                    f.y as i32 - (r.y + r.h) as i32,
                    overlap(f.x, f.x + f.w, r.x, r.x + r.w),
                ),
            };
            ((0..=2).contains(&gap) && ov > 0).then_some((gap, -ov, p.clone()))
        })
        .min()
        .map(|(_, _, p)| p)
}

impl Session {
    pub fn new_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}{}", self.next_id)
    }

    pub fn workspace(&self, key: &str) -> Option<&Workspace> {
        self.workspaces
            .iter()
            .find(|w| w.id == key)
            .or_else(|| self.workspaces.iter().find(|w| w.label == key))
    }

    pub fn workspace_mut(&mut self, key: &str) -> Option<&mut Workspace> {
        let idx = self
            .workspaces
            .iter()
            .position(|w| w.id == key)
            .or_else(|| self.workspaces.iter().position(|w| w.label == key))?;
        self.workspaces.get_mut(idx)
    }

    /// (workspace index, tab index) holding `pane`.
    pub fn locate(&self, pane: &str) -> Option<(usize, usize)> {
        for (wi, w) in self.workspaces.iter().enumerate() {
            for (ti, t) in w.tabs.iter().enumerate() {
                if t.root.contains(pane) {
                    return Some((wi, ti));
                }
            }
        }
        None
    }

    pub fn all_panes(&self) -> Vec<String> {
        self.workspaces
            .iter()
            .flat_map(|w| w.tabs.iter().flat_map(|t| t.root.panes()))
            .collect()
    }

    /// Remove a pane everywhere; empty tabs and workspaces disappear. Fixes focus/active tab.
    pub fn remove_pane(&mut self, pane: &str) -> bool {
        let Some((wi, ti)) = self.locate(pane) else {
            return false;
        };
        let ws = &mut self.workspaces[wi];
        let tab = &mut ws.tabs[ti];
        let root = std::mem::replace(&mut tab.root, Node::leaf(""));
        match root.remove(pane) {
            Some(rest) => {
                tab.root = rest;
                if tab.focus == pane {
                    tab.focus = tab.root.panes().into_iter().next().unwrap_or_default();
                }
                if tab.root.panes().len() < 2 {
                    tab.zoomed = false;
                }
            }
            None => {
                ws.tabs.remove(ti);
                if ws.tabs.is_empty() {
                    self.workspaces.remove(wi);
                } else {
                    ws.active_tab = ws.active_tab.min(ws.tabs.len() - 1);
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn split_layout_and_remove() {
        let mut n = Node::leaf("p1");
        assert!(n.split("p1", "p2", Dir::Right));
        assert!(n.split("p2", "p3", Dir::Down));
        assert_eq!(n.panes(), vec!["p1", "p2", "p3"]);
        let rects = n.layout(r(0, 0, 81, 21));
        assert_eq!(rects[0], ("p1".into(), r(0, 0, 40, 21)));
        assert_eq!(rects[1], ("p2".into(), r(41, 0, 40, 10)));
        assert_eq!(rects[2], ("p3".into(), r(41, 11, 40, 10)));
        // Every cell used exactly once (plus one border column/row per split).
        let n = n.remove("p2").unwrap();
        assert_eq!(n.panes(), vec!["p1", "p3"]);
        let n = n.remove("p1").unwrap();
        assert_eq!(n, Node::leaf("p3"));
        assert!(n.remove("p3").is_none());
    }

    #[test]
    fn neighbors_follow_geometry() {
        let mut n = Node::leaf("a");
        n.split("a", "b", Dir::Right);
        n.split("b", "c", Dir::Down);
        let rects = n.layout(r(0, 0, 81, 21));
        assert_eq!(neighbor(&rects, "a", Toward::Right).as_deref(), Some("b"));
        assert_eq!(neighbor(&rects, "b", Toward::Down).as_deref(), Some("c"));
        assert_eq!(neighbor(&rects, "c", Toward::Up).as_deref(), Some("b"));
        assert_eq!(neighbor(&rects, "c", Toward::Left).as_deref(), Some("a"));
        assert_eq!(neighbor(&rects, "a", Toward::Left), None);
    }

    #[test]
    fn resize_moves_the_right_split() {
        let mut n = Node::leaf("a");
        n.split("a", "b", Dir::Right);
        n.split("b", "c", Dir::Down);
        assert!(n.resize("a", Dir::Right, 0.1));
        assert!(n.resize("c", Dir::Down, 0.2)); // c is the second child: grows downward split's b
        let Node::Split { ratio, b, .. } = &n else {
            panic!()
        };
        assert!((*ratio - 0.6).abs() < 1e-6);
        let Node::Split { ratio: inner, .. } = b.as_ref() else {
            panic!()
        };
        assert!((*inner - 0.3).abs() < 1e-6);
        // Clamped.
        for _ in 0..20 {
            n.resize("a", Dir::Right, 0.1);
        }
        let Node::Split { ratio, .. } = &n else {
            panic!()
        };
        assert!((*ratio - 0.9).abs() < 1e-6);
    }

    #[test]
    fn session_remove_collapses_tabs_and_workspaces() {
        let mut s = Session::default();
        s.workspaces.push(Workspace {
            id: "w1".into(),
            label: "main".into(),
            cwd: "/".into(),
            tabs: vec![
                Tab {
                    id: "t1".into(),
                    label: "1".into(),
                    root: Node::leaf("p1"),
                    focus: "p1".into(),
                    zoomed: false,
                },
                Tab {
                    id: "t2".into(),
                    label: "2".into(),
                    root: Node::leaf("p2"),
                    focus: "p2".into(),
                    zoomed: false,
                },
            ],
            active_tab: 1,
        });
        assert_eq!(s.locate("p2"), Some((0, 1)));
        assert!(s.remove_pane("p2"));
        assert_eq!(s.workspaces[0].tabs.len(), 1);
        assert_eq!(s.workspaces[0].active_tab, 0);
        assert!(s.remove_pane("p1"));
        assert!(s.workspaces.is_empty());
        assert!(!s.remove_pane("p1"));
    }

    #[test]
    fn tiny_areas_do_not_panic() {
        let mut n = Node::leaf("a");
        n.split("a", "b", Dir::Right);
        n.split("b", "c", Dir::Down);
        for w in 0..4 {
            for h in 0..4 {
                let _ = n.layout(r(0, 0, w, h));
            }
        }
    }
}
