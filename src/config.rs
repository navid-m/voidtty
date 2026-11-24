use anyhow::{Context, Result};
use std::{fs, path::PathBuf};

/// Smallest font size the user can zoom out to, in points.
pub const MIN_FONT_SIZE: f32 = 6.0;
/// Largest font size the user can zoom in to, in points.
pub const MAX_FONT_SIZE: f32 = 72.0;

/// Application configuration loaded from ~/.voidtty/settings.ini.
#[derive(Debug, Clone)]
pub struct Config {
    /// Window opacity in [0.0, 1.0].
    pub opacity: f32,
    /// Shell to spawn in each pane.
    pub shell: String,
    /// Comma-separated preferred font families; empty means auto-select.
    pub font_family: String,
    /// Font size in points.
    pub font_size: f32,
    /// Scrollback buffer lines per pane.
    pub scrollback: usize,
    /// Whether shells are started with integration that reports their working
    /// directory, used for tab titles and inherited by new panes and tabs.
    pub shell_integration: bool,
    /// Initial window width in pixels.
    pub win_width: u32,
    /// Initial window height in pixels.
    pub win_height: u32,
    /// Thickness of the focused pane border, in logical pixels.
    pub active_pane_thickness: f32,
    /// RGB colour of the focused pane border.
    pub active_pane_colour: (f32, f32, f32),
    /// Whether typing in a scrolled-up pane jumps back to the live screen.
    pub active_keypress_snap: bool,
    /// Whether to show a brief confirmation after copying a selection.
    pub show_copy_notification: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            opacity: 0.90,
            shell: std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()),
            font_family: String::new(),
            font_size: 18.0,
            scrollback: 10_000,
            shell_integration: false,
            win_width: 1280,
            win_height: 800,
            active_pane_thickness: 1.5,
            active_pane_colour: (0.3, 0.7, 1.0),
            active_keypress_snap: true,
            show_copy_notification: true,
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join(".voidtty").join("settings.ini")
    }

    /// Loads config from disk, creating the file with defaults if absent.
    pub fn load() -> Result<Self> {
        let path = Self::path();
        if !path.exists() {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)
                    .with_context(|| format!("creating config dir {}", dir.display()))?;
            }
            let default = Self::default();
            fs::write(&path, default.to_ini())
                .with_context(|| format!("writing default config to {}", path.display()))?;
            return Ok(default);
        }
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        Ok(Self::parse_ini(&raw))
    }

    /// Writes the current configuration back to settings.ini.
    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)
                .with_context(|| format!("creating config dir {}", dir.display()))?;
        }
        fs::write(&path, self.to_ini())
            .with_context(|| format!("writing config to {}", path.display()))
    }

    fn parse_ini(src: &str) -> Self {
        let mut cfg = Self::default();
        for line in src.lines() {
            let line = line.trim();
            if line.starts_with('#') || line.starts_with(';') || line.starts_with('[') {
                continue;
            }
            let Some((key, val)) = line.split_once('=') else {
                continue;
            };
            let (key, val) = (key.trim(), val.trim());
            match key {
                "opacity" => {
                    if let Ok(v) = val.parse::<f32>() {
                        cfg.opacity = v.clamp(0.0, 1.0);
                    }
                }
                "shell" => {
                    cfg.shell = val.to_owned();
                }
                "font_family" => {
                    cfg.font_family = val.to_owned();
                }
                "font_size" => {
                    if let Ok(v) = val.parse::<f32>() {
                        cfg.font_size = v.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
                    }
                }
                "scrollback" => {
                    if let Ok(v) = val.parse::<usize>() {
                        cfg.scrollback = v;
                    }
                }
                "shell_integration" | "shell_integration_tabs" => {
                    if let Some(v) = parse_bool(val) {
                        cfg.shell_integration = v;
                    }
                }
                "win_width" => {
                    if let Ok(v) = val.parse::<u32>() {
                        cfg.win_width = v;
                    }
                }
                "win_height" => {
                    if let Ok(v) = val.parse::<u32>() {
                        cfg.win_height = v;
                    }
                }
                "active_pane_thickness" => {
                    if let Ok(v) = val.parse::<f32>() {
                        cfg.active_pane_thickness = v.max(0.0);
                    }
                }
                "active_pane_colour" => {
                    if let Some(colour) = parse_colour(val) {
                        cfg.active_pane_colour = colour;
                    }
                }
                "active_keypress_snap" => {
                    if let Some(v) = parse_bool(val) {
                        cfg.active_keypress_snap = v;
                    }
                }
                "show_copy_notification" => {
                    if let Some(v) = parse_bool(val) {
                        cfg.show_copy_notification = v;
                    }
                }
                _ => {}
            }
        }
        cfg
    }

    fn to_ini(&self) -> String {
        format!(
            "[general]\n\
             ; Window opacity: 0.0 (transparent) to 1.0 (opaque)\n\
             opacity = {:.2}\n\
             shell = {}\n\
             ; Preferred font families (comma-separated). Empty auto-selects a\n\
             ; monospace system font, preferring ligature fonts such as Fira Code.\n\
             font_family = {}\n\
             font_size = {:.1}\n\
             scrollback = {}\n\
             ; Start supported shells with OSC 7 integration. The reported\n\
             ; working directory is shown in tab titles and inherited by new\n\
             ; panes and tabs.\n\
             shell_integration = {}\n\
             win_width = {}\n\
             win_height = {}\n\
             ; Thickness and colour (#RRGGBB) of the focused pane border.\n\
             active_pane_thickness = {:.1}\n\
             active_pane_colour = {}\n\
             ; Jump back to the live screen when typing while scrolled up.\n\
             active_keypress_snap = {}\n\
             ; Show a brief confirmation after copying a selection.\n\
             show_copy_notification = {}\n",
            self.opacity,
            self.shell,
            self.font_family,
            self.font_size,
            self.scrollback,
            self.shell_integration,
            self.win_width,
            self.win_height,
            self.active_pane_thickness,
            colour_hex(self.active_pane_colour),
            self.active_keypress_snap,
            self.show_copy_notification,
        )
    }
}

/// Parses `#RRGGBB` or `#RGB` into display components in [0, 1].
fn parse_colour(value: &str) -> Option<(f32, f32, f32)> {
    let hex = value.trim().strip_prefix('#').unwrap_or(value.trim());
    if hex.len() != 3 && hex.len() != 6 {
        return None;
    }
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut chars = hex.chars();
    let r = if hex.len() == 3 {
        (chars.next()?.to_digit(16)? * 17) as u8
    } else {
        u8::from_str_radix(&hex[0..2], 16).ok()?
    };
    let g = if hex.len() == 3 {
        (chars.next()?.to_digit(16)? * 17) as u8
    } else {
        u8::from_str_radix(&hex[2..4], 16).ok()?
    };
    let b = if hex.len() == 3 {
        (chars.next()?.to_digit(16)? * 17) as u8
    } else {
        u8::from_str_radix(&hex[4..6], 16).ok()?
    };
    Some((r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0))
}

fn colour_hex((r, g, b): (f32, f32, f32)) -> String {
    let channel = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02X}{:02X}{:02X}", channel(r), channel(g), channel(b))
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_active_pane_and_snap_settings() {
        let cfg = Config::parse_ini(
            "active_pane_thickness = 3\n\
             active_pane_colour = #ff8800\n\
             active_keypress_snap = false\n",
        );
        assert!((cfg.active_pane_thickness - 3.0).abs() < f32::EPSILON);
        let (r, g, b) = cfg.active_pane_colour;
        assert!((r - 1.0).abs() < 0.01);
        assert!((g - 136.0 / 255.0).abs() < 0.01);
        assert!(b.abs() < 0.01);
        assert!(!cfg.active_keypress_snap);
    }

    #[test]
    fn copy_notification_defaults_on_and_can_be_disabled() {
        assert!(Config::default().show_copy_notification);
        let cfg = Config::parse_ini("show_copy_notification = false\n");
        assert!(!cfg.show_copy_notification);
        assert!(cfg.to_ini().contains("show_copy_notification = false"));
    }

    #[test]
    fn parses_shell_integration() {
        assert!(!Config::default().shell_integration);
        let cfg = Config::parse_ini("shell_integration = true\n");
        assert!(cfg.shell_integration);
    }

    #[test]
    fn accepts_legacy_shell_integration_tabs_key() {
        let cfg = Config::parse_ini("shell_integration_tabs = true\n");
        assert!(cfg.shell_integration);
    }

    #[test]
    fn default_active_pane_style_preserves_previous_border() {
        let cfg = Config::default();
        assert!((cfg.active_pane_thickness - 1.5).abs() < f32::EPSILON);
        let (r, g, b) = cfg.active_pane_colour;
        assert!((r - 0.3).abs() < 0.01);
        assert!((g - 0.7).abs() < 0.01);
        assert!((b - 1.0).abs() < 0.01);
        assert!(cfg.active_keypress_snap);
        assert!(cfg.to_ini().contains("active_pane_colour = #4DB3FF"));
    }
}
