use anyhow::Result;
use crate::{
    config::{self, Config},
    terminal::Pty,
};

/// One tab within a pane: a single PTY session.
pub struct Tab {
    pub pty: Pty,
    /// Index into the tab indicator palette, stable for the life of the tab.
    pub accent: usize,
}

/// A pane owns its own stack of tabs.
pub struct Pane {
    pub id: usize,
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
}

impl Pane {
    fn new(config: &Config, id: usize, accent: usize) -> Result<Self> {
        let pty = Pty::spawn(&config.shell, 80, 24, config.scrollback)?;
        Ok(Self {
            id,
            tabs: vec![Tab { pty, accent }],
            active_tab: 0,
        })
    }

    pub fn active(&self) -> &Tab {
        &self.tabs[self.active_tab]
    }

    pub fn active_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active_tab]
    }

    fn add_tab(&mut self, config: &Config, accent: usize) -> Result<()> {
        let pty = Pty::spawn(&config.shell, 80, 24, config.scrollback)?;
        self.tabs.push(Tab { pty, accent });
        self.active_tab = self.tabs.len() - 1;
        Ok(())
    }

    fn next_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.active_tab = (self.active_tab + 1) % self.tabs.len();
        }
    }

    fn prev_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.active_tab = (self.active_tab + self.tabs.len() - 1) % self.tabs.len();
        }
    }
}

/// Direction of a pane split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDir {
    /// Panes sit side by side, divided by a vertical line.
    Horizontal,
    /// Panes stack, divided by a horizontal line.
    Vertical,
}

/// Binary tree describing how panes tile the window.
pub enum LayoutNode {
    Leaf(usize),
    Split {
        dir: SplitDir,
        ratio: f32,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

impl LayoutNode {
    /// Replaces the leaf for `target` with a split holding the old pane and a
    /// new one. Returns whether the target was found.
    fn split_leaf(&mut self, target: usize, dir: SplitDir, new_id: usize) -> bool {
        match self {
            LayoutNode::Leaf(id) => {
                if *id != target {
                    return false;
                }
                let first = Box::new(LayoutNode::Leaf(target));
                let second = Box::new(LayoutNode::Leaf(new_id));
                *self = LayoutNode::Split {
                    dir,
                    ratio: 0.5,
                    first,
                    second,
                };
                true
            }
            LayoutNode::Split { first, second, .. } => {
                first.split_leaf(target, dir, new_id)
                    || second.split_leaf(target, dir, new_id)
            }
        }
    }
}

/// Removes a leaf, collapsing any split left with a single child. The returned
/// option is `None` when the whole tree became empty.
fn remove_leaf(node: LayoutNode, target: usize) -> (Option<LayoutNode>, bool) {
    match node {
        LayoutNode::Leaf(id) => {
            if id == target {
                (None, true)
            } else {
                (Some(LayoutNode::Leaf(id)), false)
            }
        }
        LayoutNode::Split {
            dir,
            ratio,
            first,
            second,
        } => {
            let (first, removed) = remove_leaf(*first, target);
            if removed {
                return match first {
                    Some(first) => (
                        Some(LayoutNode::Split {
                            dir,
                            ratio,
                            first: Box::new(first),
                            second,
                        }),
                        true,
                    ),
                    None => (Some(*second), true),
                };
            }
            let first = first.expect("unremoved first child survives");
            let (second, removed) = remove_leaf(*second, target);
            if removed {
                return match second {
                    Some(second) => (
                        Some(LayoutNode::Split {
                            dir,
                            ratio,
                            first: Box::new(first),
                            second: Box::new(second),
                        }),
                        true,
                    ),
                    None => (Some(first), true),
                };
            }
            (
                Some(LayoutNode::Split {
                    dir,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second.expect("unremoved second child survives")),
                }),
                false,
            )
        }
    }
}

/// Root application state.
pub struct App {
    pub panes: Vec<Pane>,
    pub layout: Option<LayoutNode>,
    /// Id of the pane receiving keyboard input.
    pub focused: usize,
    pub config: Config,
    pub running: bool,
    /// Set when panes/tabs change so PTY grids can be re-fitted.
    pub needs_resize: bool,
    /// Pane whose scrollbar thumb is currently being dragged.
    pub scroll_drag: Option<usize>,
    next_pane_id: usize,
    next_accent: usize,
}

impl App {
    pub fn new(config: Config) -> Result<Self> {
        let mut app = Self {
            panes: Vec::new(),
            layout: None,
            focused: 0,
            config,
            running: true,
            needs_resize: false,
            scroll_drag: None,
            next_pane_id: 0,
            next_accent: 0,
        };
        let id = app.alloc_pane_id();
        let accent = app.alloc_accent();
        let pane = Pane::new(&app.config, id, accent)?;
        app.focused = id;
        app.panes.push(pane);
        app.layout = Some(LayoutNode::Leaf(id));
        Ok(app)
    }

    fn alloc_pane_id(&mut self) -> usize {
        let id = self.next_pane_id;
        self.next_pane_id = self.next_pane_id.wrapping_add(1);
        id
    }

    fn alloc_accent(&mut self) -> usize {
        let accent = self.next_accent;
        self.next_accent = self.next_accent.wrapping_add(1);
        accent
    }

    pub fn pane(&self, id: usize) -> Option<&Pane> {
        self.panes.iter().find(|pane| pane.id == id)
    }

    pub fn pane_mut(&mut self, id: usize) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|pane| pane.id == id)
    }

    pub fn active_pane_mut(&mut self) -> &mut Pane {
        self.pane_mut(self.focused).expect("focused pane exists")
    }

    /// Opens a new tab in the currently focused pane.
    pub fn new_tab(&mut self) -> Result<()> {
        let accent = self.alloc_accent();
        let config = self.config.clone();
        if let Some(pane) = self.pane_mut(self.focused) {
            pane.add_tab(&config, accent)?;
        }
        self.needs_resize = true;
        Ok(())
    }

    /// Splits the focused pane, placing a new pane and focusing it.
    pub fn split(&mut self, dir: SplitDir) -> Result<()> {
        let id = self.alloc_pane_id();
        let accent = self.alloc_accent();
        let config = self.config.clone();
        let pane = Pane::new(&config, id, accent)?;

        let target = self.focused;
        self.panes.push(pane);
        if let Some(layout) = self.layout.as_mut() {
            layout.split_leaf(target, dir, id);
        } else {
            self.layout = Some(LayoutNode::Leaf(id));
        }
        self.focused = id;
        self.needs_resize = true;
        Ok(())
    }

    /// Closes the focused pane's active tab, or the pane when it was the last.
    pub fn close_tab(&mut self) {
        let Some(pane) = self.pane(self.focused) else {
            return;
        };
        if pane.tabs.len() <= 1 {
            self.close_focused_pane();
            return;
        }
        let pane = self.pane_mut(self.focused).expect("focused pane exists");
        pane.tabs.remove(pane.active_tab);
        pane.active_tab = pane.active_tab.saturating_sub(1);
    }

    /// Closes the focused pane (and all of its tabs).
    pub fn close_focused_pane(&mut self) {
        let id = self.focused;
        if self.panes.len() <= 1 {
            self.panes.clear();
            self.layout = None;
            self.running = false;
            return;
        }
        self.panes.retain(|pane| pane.id != id);
        if let Some(layout) = self.layout.take() {
            let (layout, _) = remove_leaf(layout, id);
            self.layout = layout;
        }
        if let Some(pane) = self.panes.first() {
            self.focused = pane.id;
        }
        self.needs_resize = true;
    }

    /// Removes exited tabs, then empty panes. If no panes remain the whole
    /// emulator is asked to shut down.
    pub fn reap_exited_panes(&mut self) {
        let mut emptied = Vec::new();
        for pane in &mut self.panes {
            let mut i = pane.tabs.len();
            while i > 0 {
                i -= 1;
                if !pane.tabs[i].pty.is_alive() {
                    pane.tabs.remove(i);
                }
            }
            if pane.tabs.is_empty() {
                emptied.push(pane.id);
            } else if pane.active_tab >= pane.tabs.len() {
                pane.active_tab = pane.tabs.len() - 1;
            }
        }

        if !emptied.is_empty() {
            self.needs_resize = true;
        }
        for id in emptied {
            self.panes.retain(|pane| pane.id != id);
            if let Some(layout) = self.layout.take() {
                let (layout, _) = remove_leaf(layout, id);
                self.layout = layout;
            }
            if self.focused == id
                && let Some(pane) = self.panes.first()
            {
                self.focused = pane.id;
            }
        }

        if self.panes.is_empty() {
            self.running = false;
            self.layout = None;
        }
    }

    pub fn next_tab(&mut self) {
        if let Some(pane) = self.pane_mut(self.focused) {
            pane.next_tab();
        }
    }

    pub fn prev_tab(&mut self) {
        if let Some(pane) = self.pane_mut(self.focused) {
            pane.prev_tab();
        }
    }

    /// Adjusts the font size by `delta` points, clamping to the allowed range
    /// and persisting the new value to settings.ini.
    pub fn change_font_size(&mut self, delta: f32) -> Result<()> {
        let size = (self.config.font_size + delta)
            .clamp(config::MIN_FONT_SIZE, config::MAX_FONT_SIZE);
        if (size - self.config.font_size).abs() < f32::EPSILON {
            return Ok(());
        }
        self.config.font_size = size;
        self.config.save()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf_ids(node: &LayoutNode, out: &mut Vec<usize>) {
        match node {
            LayoutNode::Leaf(id) => out.push(*id),
            LayoutNode::Split { first, second, .. } => {
                leaf_ids(first, out);
                leaf_ids(second, out);
            }
        }
    }

    fn ids(node: &LayoutNode) -> Vec<usize> {
        let mut out = Vec::new();
        leaf_ids(node, &mut out);
        out
    }

    #[test]
    fn split_places_new_pane_after_target() {
        let mut node = LayoutNode::Leaf(0);
        assert!(node.split_leaf(0, SplitDir::Horizontal, 1));
        assert_eq!(ids(&node), vec![0, 1]);
    }

    #[test]
    fn removing_leaf_collapses_split() {
        let mut node = LayoutNode::Leaf(0);
        node.split_leaf(0, SplitDir::Horizontal, 1);
        node.split_leaf(1, SplitDir::Vertical, 2);
        assert_eq!(ids(&node), vec![0, 1, 2]);

        let (node, removed) = remove_leaf(node, 1);
        assert!(removed);
        let node = node.expect("tree still has panes");
        assert_eq!(ids(&node), vec![0, 2]);

        let (node, removed) = remove_leaf(node, 2);
        assert!(removed);
        let node = node.expect("one pane remains");
        assert_eq!(ids(&node), vec![0]);

        let (node, removed) = remove_leaf(node, 1);
        assert!(!removed);
        assert_eq!(ids(&node.expect("untouched root")), vec![0]);
    }
}
