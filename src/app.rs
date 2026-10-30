use anyhow::Result;
use crate::{
    config::{self, Config},
    terminal::Pty,
};

/// A single rectangular pane containing one PTY.
pub struct Pane {
    pub pty: Pty,
    /// Human-readable title (set from OSC 2 / shell $PWD in a later iteration).
    pub title: String,
}

impl Pane {
    pub fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            pty: Pty::spawn(&config.shell, 80, 24, config.scrollback)?,
            title: config.shell.split('/').last().unwrap_or("shell").to_owned(),
        })
    }
}

/// Layout of panes inside a tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Single,
    /// Two panes side-by-side.
    HSplit,
/// 2×2 grid.
    Grid,
}

/// One tab holds between 1 and 4 panes in a chosen layout.
pub struct Tab {
    pub panes: Vec<Pane>,
    pub focused: usize,
    pub layout: Layout,
    pub title: String,
}

impl Tab {
    pub fn new(config: &Config, index: usize) -> Result<Self> {
        let mut panes = Vec::new();
        panes.push(Pane::new(config)?);
        Ok(Self {
            panes,
            focused: 0,
            layout: Layout::Single,
            title: format!("Tab {}", index + 1),
        })
    }

    /// Adds a pane if the current layout supports more; otherwise upgrades the layout.
    pub fn add_pane(&mut self, config: &Config) -> Result<()> {
        let next_layout = match self.layout {
            Layout::Single => Layout::HSplit,
            Layout::HSplit => Layout::Grid,
            
            Layout::Grid => return Ok(()),
        };
        self.panes.push(Pane::new(config)?);
        self.layout = next_layout;
        self.focused = self.panes.len() - 1;
        Ok(())
    }

    pub fn close_focused_pane(&mut self) {
        if self.panes.len() > 1 {
            self.panes.remove(self.focused);
            self.focused = self.focused.saturating_sub(1);
            self.layout = match self.panes.len() {
                1 => Layout::Single,
                2 => Layout::HSplit,
                _ => Layout::Grid,
            };
        }
    }

    pub fn focused_pane_mut(&mut self) -> &mut Pane {
        &mut self.panes[self.focused]
    }

    pub fn cycle_focus(&mut self, forward: bool) {
        let n = self.panes.len();
        if forward {
            self.focused = (self.focused + 1) % n;
        } else {
            self.focused = (self.focused + n - 1) % n;
        }
    }
}

/// Root application state.
pub struct App {
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
    pub config: Config,
    pub running: bool,
}

impl App {
    pub fn new(config: Config) -> Result<Self> {
        let first = Tab::new(&config, 0)?;
        Ok(Self { tabs: vec![first], active_tab: 0, config, running: true })
    }

    pub fn active_tab(&self) -> &Tab {
        &self.tabs[self.active_tab]
    }

    pub fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active_tab]
    }

    pub fn new_tab(&mut self) -> Result<()> {
        let idx = self.tabs.len();
        self.tabs.push(Tab::new(&self.config, idx)?);
        self.active_tab = self.tabs.len() - 1;
        Ok(())
    }

    pub fn close_tab(&mut self) {
        if self.tabs.len() > 1 {
            self.tabs.remove(self.active_tab);
            self.active_tab = self.active_tab.saturating_sub(1);
        } else {
            self.running = false;
        }
    }

    pub fn next_tab(&mut self) {
        self.active_tab = (self.active_tab + 1) % self.tabs.len();
    }

    pub fn prev_tab(&mut self) {
        let n = self.tabs.len();
        self.active_tab = (self.active_tab + n - 1) % n;
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
