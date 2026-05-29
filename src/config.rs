use anyhow::{Context, Result};
use std::{fs, path::PathBuf};

/// Application configuration loaded from ~/.godtty/settings.ini.
#[derive(Debug, Clone)]
pub struct Config {
    /// Window opacity in [0.0, 1.0].
    pub opacity: f32,
    /// Shell to spawn in each pane.
    pub shell: String,
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
            font_size: 16.0,
            scrollback: 10_000,
            win_width: 1280,
            win_height: 800,
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join(".godtty").join("settings.ini")
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
                "font_size"   => { if let Ok(v) = val.parse::<f32>()  { cfg.font_size  = v; } }
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
             font_size = {:.1}\n\
             scrollback = {}\n\
             win_width = {}\n\
             win_height = {}\n",
            self.opacity, self.shell, self.font_size,
            self.scrollback, self.win_width, self.win_height,
        )
    }
}
