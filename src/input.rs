use anyhow::Result;
use glfw::{Action, Key, Modifiers, WindowEvent};

use crate::app::App;

/// Processes one GLFW window event. Returns `true` if consumed as a shortcut.
pub fn handle_event(event: WindowEvent, app: &mut App) -> Result<bool> {
    let WindowEvent::Key(key, _, action, mods) = event else { return Ok(false) };
    if action == Action::Release { return Ok(false); }

    let ctrl_shift = mods.contains(Modifiers::Control | Modifiers::Shift);
    let ctrl       = mods.contains(Modifiers::Control) && !mods.contains(Modifiers::Shift);

    // Ctrl + / Ctrl - (and numpad equivalents) zoom the text size.
    if ctrl || ctrl_shift {
        match key {
            Key::Equal | Key::KpAdd => {
                app.change_font_size(1.0)?;
                return Ok(true);
            }
            Key::Minus | Key::KpSubtract => {
                app.change_font_size(-1.0)?;
                return Ok(true);
            }
            _ => {}
        }
    }

    if ctrl_shift {
        match key {
            Key::T => { app.new_tab()?; return Ok(true); }
            Key::W => { app.close_tab(); return Ok(true); }
            Key::Right => { app.next_tab(); return Ok(true); }
            Key::Left  => { app.prev_tab(); return Ok(true); }
            Key::D => {
                let config = app.config.clone();
                app.active_tab_mut().add_pane(&config)?;
                return Ok(true);
            }
            Key::X => { app.active_tab_mut().close_focused_pane(); return Ok(true); }
            Key::Tab => { app.active_tab_mut().cycle_focus(true); return Ok(true); }
            Key::Q => { app.running = false; return Ok(true); }
            _ => {}
        }
    }

    let bytes: Vec<u8> = if ctrl {
        match key {
            Key::A  => vec![0x01], Key::B  => vec![0x02], Key::C  => vec![0x03],
            Key::D  => vec![0x04], Key::E  => vec![0x05], Key::F  => vec![0x06],
            Key::G  => vec![0x07], Key::H  => vec![0x08], Key::I  => vec![0x09],
            Key::J  => vec![0x0a], Key::K  => vec![0x0b], Key::L  => vec![0x0c],
            Key::M  => vec![0x0d], Key::N  => vec![0x0e], Key::O  => vec![0x0f],
            Key::P  => vec![0x10], Key::Q  => vec![0x11], Key::R  => vec![0x12],
            Key::S  => vec![0x13], Key::T  => vec![0x14], Key::U  => vec![0x15],
            Key::V  => vec![0x16], Key::W  => vec![0x17], Key::X  => vec![0x18],
            Key::Y  => vec![0x19], Key::Z  => vec![0x1a],
            Key::LeftBracket  => vec![0x1b],
            Key::Backslash    => vec![0x1c],
            Key::RightBracket => vec![0x1d],
            _ => vec![],
        }
    } else {
        key_to_bytes(key, mods)
    };

    if !bytes.is_empty() {
        app.active_tab_mut().focused_pane_mut().pty.write(&bytes)?;
    }
    Ok(false)
}

/// Handles a Unicode character event from GLFW (fires for printable chars).
pub fn handle_char(c: char, app: &mut App) -> Result<()> {
    let mut buf = [0u8; 4];
    let s = c.encode_utf8(&mut buf);
    app.active_tab_mut().focused_pane_mut().pty.write(s.as_bytes())
}

fn key_to_bytes(key: Key, _mods: Modifiers) -> Vec<u8> {
    match key {
        Key::Enter     => b"\r".to_vec(),
        Key::Backspace => vec![0x7f],
        Key::Tab       => b"\t".to_vec(),
        Key::Escape    => vec![0x1b],
        Key::Up        => b"\x1b[A".to_vec(),
        Key::Down      => b"\x1b[B".to_vec(),
        Key::Right     => b"\x1b[C".to_vec(),
        Key::Left      => b"\x1b[D".to_vec(),
        Key::Home      => b"\x1b[H".to_vec(),
        Key::End       => b"\x1b[F".to_vec(),
        Key::PageUp    => b"\x1b[5~".to_vec(),
        Key::PageDown  => b"\x1b[6~".to_vec(),
        Key::Delete    => b"\x1b[3~".to_vec(),
        Key::Insert    => b"\x1b[2~".to_vec(),
        Key::F1        => b"\x1bOP".to_vec(),
        Key::F2        => b"\x1bOQ".to_vec(),
        Key::F3        => b"\x1bOR".to_vec(),
        Key::F4        => b"\x1bOS".to_vec(),
        Key::F5        => b"\x1b[15~".to_vec(),
        Key::F6        => b"\x1b[17~".to_vec(),
        Key::F7        => b"\x1b[18~".to_vec(),
        Key::F8        => b"\x1b[19~".to_vec(),
        Key::F9        => b"\x1b[20~".to_vec(),
        Key::F10       => b"\x1b[21~".to_vec(),
        Key::F11       => b"\x1b[23~".to_vec(),
        Key::F12       => b"\x1b[24~".to_vec(),
        // Printable keys not handled via char callback (e.g. when num-lock is off).
        _ => vec![],
    }
}
