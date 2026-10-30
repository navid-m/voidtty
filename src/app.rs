use anyhow::Result;
use crate::{
    config::{self, Config},
    terminal::Pty,
};

/// A single rectangular pane containing one PTY.
pub struct Pane {
    pub pty: Pty,
}

impl Pane {
    pub fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            pty: Pty::spawn(&config.shell, 80, 24, config.scrollback)?,
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
    /// Index into the tab indicator palette, stable for the life of the tab.
    pub accent: usize,
}

impl Tab {
    pub fn new(config: &Config, accent: usize) -> Result<Self> {
        let mut panes = Vec::new();
        panes.push(Pane::new(config)?);
        Ok(Self {
            panes,
            focused: 0,
            layout: Layout::Single,
            accent,
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
    /// `(tab, pane)` whose scrollbar thumb is currently being dragged.
    pub scroll_drag: Option<(usize, usize)>,
    next_accent: usize,
}

impl App {
    pub fn new(config: Config) -> Result<Self> {
        let first = Tab::new(&config, 0)?;
        Ok(Self {
            tabs: vec![first],
            active_tab: 0,
            config,
            running: true,
            scroll_drag: None,
            next_accent: 1,
        })
    }

    pub fn active_tab(&self) -> &Tab {
        &self.tabs[self.active_tab]
    }

    pub fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active_tab]
    }

    pub fn new_tab(&mut self) -> Result<()> {
        let accent = self.next_accent;
        self.next_accent = self.next_accent.wrapping_add(1);
        self.tabs.push(Tab::new(&self.config, accent)?);
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

    /// Removes panes whose shells have exited, then closes empty tabs. If no
    /// tabs remain the whole emulator is asked to shut down.
    pub fn reap_exited_panes(&mut self) {
        let mut i = 0;
        while i < self.tabs.len() {
            let empty = {
                let tab = &mut self.tabs[i];
                let mut j = tab.panes.len();
                while j > 0 {
                    j -= 1;
                    if !tab.panes[j].pty.is_alive() {
                        tab.panes.remove(j);
                        if tab.focused > j {
                            tab.focused -= 1;
                        } else if tab.focused == j {
                            tab.focused = tab.focused.saturating_sub(1);
                        }
                    }
                }
                if !tab.panes.is_empty() {
                    tab.focused = tab.focused.min(tab.panes.len() - 1);
                    tab.layout = match tab.panes.len() {
                        1 => Layout::Single,
                        2 => Layout::HSplit,
                        _ => Layout::Grid,
                    };
                }
                tab.panes.is_empty()
            };

            if empty {
                self.tabs.remove(i);
                if self.active_tab > i {
                    self.active_tab -= 1;
                }
            } else {
                i += 1;
            }
        }

        if self.tabs.is_empty() {
            self.running = false;
            self.active_tab = 0;
        } else if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
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
