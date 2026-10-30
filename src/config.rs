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
    /// Initial window width in pixels.
    pub win_width: u32,
    /// Initial window height in pixels.
    pub win_height: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            opacity: 0.90,
            shell: std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()),
            font_family: String::new(),
            font_size: 18.0,
            scrollback: 10_000,
            win_width: 1280,
            win_height: 800,
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
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
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
            let Some((key, val)) = line.split_once('=') else { continue };
            let (key, val) = (key.trim(), val.trim());
            match key {
                "opacity"     => { if let Ok(v) = val.parse::<f32>()  { cfg.opacity    = v.clamp(0.0, 1.0); } }
                "shell"       => { cfg.shell = val.to_owned(); }
                "font_family" => { cfg.font_family = val.to_owned(); }
                "font_size"   => { if let Ok(v) = val.parse::<f32>()  { cfg.font_size  = v.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE); } }
                "scrollback"  => { if let Ok(v) = val.parse::<usize>() { cfg.scrollback = v; } }
                "win_width"   => { if let Ok(v) = val.parse::<u32>()  { cfg.win_width  = v; } }
                "win_height"  => { if let Ok(v) = val.parse::<u32>()  { cfg.win_height = v; } }
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
             win_width = {}\n\
             win_height = {}\n",
            self.opacity, self.shell, self.font_family, self.font_size,
            self.scrollback, self.win_width, self.win_height,
        )
    }
}
