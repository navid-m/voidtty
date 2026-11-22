#[cfg(any(target_os = "windows", target_os = "macos"))]
use anyhow::Context;
use anyhow::Result;
use std::{path::Path, process::Command};

/// Opens a URL in the system's default browser.
pub fn open_url(url: &str) -> Result<()> {
    let url = normalise_url(url);
    spawn_url(&url)
}

/// Reveals a path in the system's default file manager, selecting files where
/// the platform supports it.
pub fn reveal_path(path: &Path) -> Result<()> {
    spawn_path(path)
}

fn normalise_url(url: &str) -> String {
    if url.starts_with("www.") {
        format!("https://{url}")
    } else {
        url.to_owned()
    }
}

#[cfg(target_os = "windows")]
fn spawn_url(url: &str) -> Result<()> {
    Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn()
        .context("opening URL")?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn spawn_url(url: &str) -> Result<()> {
    Command::new("open")
        .arg(url)
        .spawn()
        .context("opening URL")?;
    Ok(())
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn spawn_url(url: &str) -> Result<()> {
    if Command::new("xdg-open").arg(url).spawn().is_ok() {
        return Ok(());
    }
    if Command::new("gio").args(["open", url]).spawn().is_ok() {
        return Ok(());
    }
    anyhow::bail!("no system URL opener found (tried xdg-open and gio)")
}

#[cfg(target_os = "windows")]
fn spawn_path(path: &Path) -> Result<()> {
    if path.is_dir() {
        Command::new("explorer")
            .arg(path)
            .spawn()
            .context("opening file manager")?;
    } else {
        Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .spawn()
            .context("opening file manager")?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn spawn_path(path: &Path) -> Result<()> {
    if path.is_dir() {
        Command::new("open")
            .arg(path)
            .spawn()
            .context("opening file manager")?;
    } else {
        Command::new("open")
            .args(["-R"])
            .arg(path)
            .spawn()
            .context("opening file manager")?;
    }
    Ok(())
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn spawn_path(path: &Path) -> Result<()> {
    let target = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    if Command::new("xdg-open").arg(target).spawn().is_ok() {
        return Ok(());
    }
    if Command::new("gio").arg("open").arg(target).spawn().is_ok() {
        return Ok(());
    }
    anyhow::bail!("no system file manager opener found (tried xdg-open and gio)")
}
