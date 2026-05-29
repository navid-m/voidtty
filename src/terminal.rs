use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    io::{Read, Write},
    sync::{Arc, Mutex},
    thread,
};
use vte::{Parser, Perform};

/// Scrollback + visible screen buffer for one pane.
#[derive(Default, Clone)]
pub struct Screen {
    /// All lines ever received (scrollback included).
    pub lines: Vec<String>,
    pub cursor_row: usize,
    pub cursor_col: usize,
}

impl Screen {
    pub fn visible_lines(&self, rows: usize) -> &[String] {
        let len = self.lines.len();
        if len <= rows {
            &self.lines
        } else {
            &self.lines[len - rows..]
        }
    }
}

/// VTE performer that appends decoded text into a `Screen`.
struct Performer {
    screen: Arc<Mutex<Screen>>,
}

impl Perform for Performer {
    fn print(&mut self, c: char) {
        let mut screen = self.screen.lock().unwrap();
        if screen.lines.is_empty() {
            screen.lines.push(String::new());
        }
        let last = screen.lines.last_mut().unwrap();
        last.push(c);
        screen.cursor_col += 1;
    }

    fn execute(&mut self, byte: u8) {
        let mut screen = self.screen.lock().unwrap();
        match byte {
            b'\n' | b'\x0C' | b'\x0B' => {
                screen.lines.push(String::new());
                screen.cursor_row += 1;
                screen.cursor_col = 0;
            }
            b'\r' => {
                screen.cursor_col = 0;
            }
            b'\x08' => {
                if let Some(last) = screen.lines.last_mut() {
                    last.pop();
                    screen.cursor_col = screen.cursor_col.saturating_sub(1);
                }
            }
            _ => {}
        }
    }

    fn hook(&mut self, _: &vte::Params, _: &[u8], _: bool, _: char) {}
    fn put(&mut self, _: u8) {}
    fn unhook(&mut self) {}
    fn osc_dispatch(&mut self, _: &[&[u8]], _: bool) {}
    fn csi_dispatch(&mut self, params: &vte::Params, _: &[u8], _: bool, action: char) {
        let mut screen = self.screen.lock().unwrap();
        match action {
            'J' => {
                let p: u16 = params.iter().next().and_then(|s| s.first()).copied().unwrap_or(0);
                if p == 2 || p == 3 {
                    screen.lines.clear();
                    screen.cursor_row = 0;
                    screen.cursor_col = 0;
                }
            }
            'H' | 'f' => {
                let mut it = params.iter();
                let row = it.next().and_then(|s| s.first()).copied().unwrap_or(1).saturating_sub(1);
                let col = it.next().and_then(|s| s.first()).copied().unwrap_or(1).saturating_sub(1);
                screen.cursor_row = row as usize;
                screen.cursor_col = col as usize;
                while screen.lines.len() <= screen.cursor_row {
                    screen.lines.push(String::new());
                }
            }
            _ => {}
        }
    }
    fn esc_dispatch(&mut self, _: &[u8], _: bool, _: u8) {}
}

/// One running PTY session (one pane).
pub struct Pty {
    master: Box<dyn MasterPty + Send>,
    _child: Box<dyn Child + Send + Sync>,
    pub screen: Arc<Mutex<Screen>>,
    writer: Box<dyn Write + Send>,
}

impl Pty {
    /// Spawns `shell` in a PTY sized `cols×rows` and starts a reader thread.
    pub fn spawn(shell: &str, cols: u16, rows: u16, scrollback_limit: usize) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("opening PTY")?;

        let mut cmd = CommandBuilder::new(shell);
        cmd.env("TERM", "xterm-256color");

        let child = pair.slave.spawn_command(cmd).context("spawning shell")?;
        let writer = pair.master.take_writer().context("taking PTY writer")?;
        let mut reader = pair.master.try_clone_reader().context("cloning PTY reader")?;

        let screen = Arc::new(Mutex::new(Screen::default()));
        let screen_clone = Arc::clone(&screen);

        thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || {
                let mut parser = Parser::new();
                let mut performer = Performer { screen: screen_clone };
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            for &byte in &buf[..n] {
                                parser.advance(&mut performer, byte);
                            }
                            // Trim scrollback
                            let mut s = performer.screen.lock().unwrap();
                            if s.lines.len() > scrollback_limit {
                                let drain = s.lines.len() - scrollback_limit;
                                s.lines.drain(0..drain);
                            }
                        }
                    }
                }
            })
            .context("spawning reader thread")?;

        Ok(Self { master: pair.master, _child: child, screen, writer })
    }

    /// Sends raw bytes to the shell's stdin.
    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        self.writer.write_all(data).context("writing to PTY")?;
        self.writer.flush().context("flushing PTY writer")
    }

    /// Resizes the PTY to match the current pane size.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.master
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("resizing PTY")
    }
}
