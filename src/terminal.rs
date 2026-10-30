use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    io::{ErrorKind, Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};
use vte::{Params, Parser, Perform};

/// A terminal color: the default foreground/background, an ANSI 256-color
/// palette index, or a 24-bit RGB value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// Visual attributes attached to a printed cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub underline: bool,
    pub reverse: bool,
}

/// One character cell in the visible grid.
#[derive(Debug, Clone, Copy)]
pub struct Cell {
    pub ch: char,
    pub style: Style,
}

impl Default for Cell {
    fn default() -> Self {
        Self { ch: ' ', style: Style::default() }
    }
}

impl Cell {
    /// A blank cell that keeps only the current background (BCE), so clearing a
    /// line does not leave every erased space tagged with a foreground color.
    fn blank(style: Style) -> Self {
        let (fg, bg) = if style.reverse {
            (Color::Default, style.fg)
        } else {
            (Color::Default, style.bg)
        };
        Self {
            ch: ' ',
            style: Style { fg, bg, ..Style::default() },
        }
    }
}

fn row_string(row: &[Cell]) -> String {
    let mut s: String = row.iter().map(|cell| cell.ch).collect();
    let trimmed = s.trim_end().len();
    s.truncate(trimmed);
    s
}

/// Scrollback + visible screen buffer for one pane.
///
/// The visible area is a fixed-size character grid with a cursor, so shells
/// that redraw their command line (fish, zsh, readline, …) work correctly.
#[derive(Clone)]
pub struct Screen {
    cols: usize,
    rows: usize,
    grid: Vec<Vec<Cell>>,
    cursor_row: usize,
    cursor_col: usize,
    saved_row: usize,
    saved_col: usize,
    scrollback: Vec<String>,
    scrollback_limit: usize,
    current_style: Style,
}

impl Screen {
    pub fn new(cols: usize, rows: usize, scrollback_limit: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            cols,
            rows,
            grid: vec![vec![Cell::default(); cols]; rows],
            cursor_row: 0,
            cursor_col: 0,
            saved_row: 0,
            saved_col: 0,
            scrollback: Vec::new(),
            scrollback_limit,
            current_style: Style::default(),
        }
    }

    /// Returns the visible grid as one string per row, trailing blanks trimmed.
    #[cfg(test)]
    pub fn visible_lines(&self) -> Vec<String> {
        self.grid.iter().map(|row| row_string(row)).collect()
    }

    /// Returns the styled visible grid rows.
    pub fn visible_cells(&self) -> &[Vec<Cell>] {
        &self.grid
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);

        if cols != self.cols {
            for row in &mut self.grid {
                row.resize(cols, Cell::default());
            }
            self.cols = cols;
        }

        if rows < self.rows {
            let removed = self.rows - rows;
            for _ in 0..removed.min(self.grid.len()) {
                let line = row_string(&self.grid.remove(0));
                self.push_scrollback(line);
            }
            self.cursor_row = self.cursor_row.saturating_sub(removed);
        } else if rows > self.rows {
            self.grid.resize_with(rows, || vec![Cell::default(); self.cols]);
        }
        self.rows = rows;

        self.cursor_row = self.cursor_row.min(self.rows - 1);
        self.cursor_col = self.cursor_col.min(self.cols - 1);
    }

    fn push_scrollback(&mut self, line: String) {
        if self.scrollback_limit == 0 {
            return;
        }
        self.scrollback.push(line.trim_end().to_owned());
        if self.scrollback.len() > self.scrollback_limit {
            let drain = self.scrollback.len() - self.scrollback_limit;
            self.scrollback.drain(0..drain);
        }
    }

    fn print(&mut self, c: char) {
        if self.cursor_col >= self.cols {
            self.cursor_col = 0;
            self.linefeed();
        }
        self.grid[self.cursor_row][self.cursor_col] = Cell { ch: c, style: self.current_style };
        self.cursor_col += 1;
    }

    fn apply_sgr(&mut self, params: &Params) {
        let values: Vec<usize> = params
            .iter()
            .map(|p| p.first().copied().unwrap_or(0) as usize)
            .collect();
        if values.is_empty() {
            self.current_style = Style::default();
            return;
        }

        let mut i = 0;
        while i < values.len() {
            let code = values[i];
            match code {
                0 => self.current_style = Style::default(),
                1 => self.current_style.bold = true,
                4 => self.current_style.underline = true,
                7 => self.current_style.reverse = true,
                22 => self.current_style.bold = false,
                24 => self.current_style.underline = false,
                27 => self.current_style.reverse = false,
                30..=37 => self.current_style.fg = Color::Indexed((code - 30) as u8),
                39 => self.current_style.fg = Color::Default,
                40..=47 => self.current_style.bg = Color::Indexed((code - 40) as u8),
                49 => self.current_style.bg = Color::Default,
                90..=97 => self.current_style.fg = Color::Indexed((code - 90 + 8) as u8),
                100..=107 => self.current_style.bg = Color::Indexed((code - 100 + 8) as u8),
                38 | 48 => {
                    if let Some((color, used)) = extended_color(&values[i + 1..]) {
                        if code == 38 {
                            self.current_style.fg = color;
                        } else {
                            self.current_style.bg = color;
                        }
                        i += used;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn linefeed(&mut self) {
        if self.cursor_row + 1 >= self.rows {
            self.scroll_up(1);
        } else {
            self.cursor_row += 1;
        }
    }

    fn reverse_index(&mut self) {
        if self.cursor_row == 0 {
            self.scroll_down(1);
        } else {
            self.cursor_row -= 1;
        }
    }

    fn scroll_up(&mut self, n: usize) {
        for _ in 0..n.min(self.rows) {
            let line = row_string(&self.grid.remove(0));
            self.push_scrollback(line);
            self.grid.push(vec![Cell::default(); self.cols]);
        }
    }

    fn scroll_down(&mut self, n: usize) {
        for _ in 0..n.min(self.rows) {
            self.grid.pop();
            self.grid.insert(0, vec![Cell::default(); self.cols]);
        }
    }

    fn cursor_up(&mut self, n: usize) {
        self.cursor_row = self.cursor_row.saturating_sub(n);
    }

    fn cursor_down(&mut self, n: usize) {
        self.cursor_row = (self.cursor_row + n).min(self.rows - 1);
    }

    fn cursor_forward(&mut self, n: usize) {
        self.cursor_col = (self.cursor_col + n).min(self.cols - 1);
    }

    fn cursor_back(&mut self, n: usize) {
        self.cursor_col = self.cursor_col.saturating_sub(n);
    }

    fn set_cursor(&mut self, row: usize, col: usize) {
        self.cursor_row = row.min(self.rows - 1);
        self.cursor_col = col.min(self.cols - 1);
    }

    fn save_cursor(&mut self) {
        self.saved_row = self.cursor_row;
        self.saved_col = self.cursor_col;
    }

    fn restore_cursor(&mut self) {
        self.set_cursor(self.saved_row, self.saved_col);
    }

    fn erase_display(&mut self, mode: usize) {
        let blank = Cell::blank(self.current_style);
        match mode {
            0 => {
                self.erase_line(0);
                for row in self.cursor_row + 1..self.rows {
                    self.grid[row].fill(blank);
                }
            }
            1 => {
                for row in 0..self.cursor_row {
                    self.grid[row].fill(blank);
                }
                self.erase_line(1);
            }
            2 => {
                for row in &mut self.grid {
                    row.fill(blank);
                }
            }
            3 => {
                for row in &mut self.grid {
                    row.fill(blank);
                }
                self.scrollback.clear();
            }
            _ => {}
        }
    }

    fn erase_line(&mut self, mode: usize) {
        let blank = Cell::blank(self.current_style);
        let row = &mut self.grid[self.cursor_row];
        match mode {
            0 => {
                let start = self.cursor_col.min(self.cols);
                row[start..].fill(blank);
            }
            1 => {
                let end = (self.cursor_col + 1).min(self.cols);
                row[..end].fill(blank);
            }
            2 => row.fill(blank),
            _ => {}
        }
    }

    fn insert_lines(&mut self, n: usize) {
        for _ in 0..n.min(self.rows - self.cursor_row) {
            self.grid.insert(self.cursor_row, vec![Cell::default(); self.cols]);
            self.grid.pop();
        }
    }

    fn delete_lines(&mut self, n: usize) {
        for _ in 0..n.min(self.rows - self.cursor_row) {
            self.grid.remove(self.cursor_row);
            self.grid.push(vec![Cell::default(); self.cols]);
        }
    }

    fn delete_chars(&mut self, n: usize) {
        let row = &mut self.grid[self.cursor_row];
        let start = self.cursor_col.min(self.cols);
        let end = (start + n).min(self.cols);
        row.drain(start..end);
        row.resize(self.cols, Cell::default());
    }

    fn insert_chars(&mut self, n: usize) {
        let row = &mut self.grid[self.cursor_row];
        let start = self.cursor_col.min(self.cols);
        for _ in 0..n.min(self.cols) {
            row.insert(start, Cell::default());
        }
        row.truncate(self.cols);
    }

    fn erase_chars(&mut self, n: usize) {
        let blank = Cell::blank(self.current_style);
        let row = &mut self.grid[self.cursor_row];
        let start = self.cursor_col.min(self.cols);
        let end = (start + n).min(self.cols);
        row[start..end].fill(blank);
    }

    fn reset(&mut self) {
        for row in &mut self.grid {
            row.fill(Cell::default());
        }
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.saved_row = 0;
        self.saved_col = 0;
        self.scrollback.clear();
        self.current_style = Style::default();
    }
}

/// Parses the extended color arguments following SGR 38/48.
/// Returns the color and how many parameters were consumed.
fn extended_color(args: &[usize]) -> Option<(Color, usize)> {
    match args.first().copied()? {
        5 => {
            let idx = *args.get(1)? as u8;
            Some((Color::Indexed(idx), 2))
        }
        2 => {
            let r = *args.get(1)? as u8;
            let g = *args.get(2)? as u8;
            let b = *args.get(3)? as u8;
            Some((Color::Rgb(r, g, b), 4))
        }
        _ => None,
    }
}

fn param(params: &Params, index: usize) -> Option<usize> {
    params
        .iter()
        .nth(index)
        .and_then(|p| p.first())
        .map(|&v| v as usize)
}

/// A parameter that defaults to `default` when missing or zero.
fn param_or(params: &Params, index: usize, default: usize) -> usize {
    param(params, index).filter(|&v| v != 0).unwrap_or(default)
}

/// A parameter where zero is meaningful (e.g. erase modes).
fn param_raw(params: &Params, index: usize) -> usize {
    param(params, index).unwrap_or(0)
}

type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// VTE performer that decodes terminal output into a `Screen` and answers the
/// capability queries shells issue at startup (DA, DSR, XTVERSION).
struct Performer {
    screen: Arc<Mutex<Screen>>,
    writer: SharedWriter,
}

impl Performer {
    fn respond(&self, bytes: &[u8]) {
        let mut writer = self.writer.lock().unwrap();
        let _ = writer.write_all(bytes);
        let _ = writer.flush();
    }
}

impl Perform for Performer {
    fn print(&mut self, c: char) {
        self.screen.lock().unwrap().print(c);
    }

    fn execute(&mut self, byte: u8) {
        let mut screen = self.screen.lock().unwrap();
        match byte {
            b'\n' | b'\x0B' | b'\x0C' => screen.linefeed(),
            b'\r' => screen.cursor_col = 0,
            b'\x08' => screen.cursor_col = screen.cursor_col.saturating_sub(1),
            b'\t' => {
                let next = (screen.cursor_col / 8 + 1) * 8;
                screen.cursor_col = next.min(screen.cols - 1);
            }
            _ => {}
        }
    }

    fn hook(&mut self, _: &Params, intermediates: &[u8], _: bool, action: char) {
        // Reply to XTVERSION so shells don't stall waiting for a terminal name.
        if action == 'q' && intermediates.contains(&b'>') {
            self.respond(b"\x1bP>|voidtty 0.1\x1b\\");
        }
    }

    fn put(&mut self, _: u8) {}

    fn unhook(&mut self) {}

    fn osc_dispatch(&mut self, _: &[&[u8]], _: bool) {}

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _: bool, action: char) {
        // Device / cursor queries reply on the shell's stdin.
        match action {
            'c' => {
                if intermediates.contains(&b'>') {
                    // Secondary device attributes (xterm-like).
                    self.respond(b"\x1b[>1;10;0c");
                } else {
                    // Primary device attributes (VT100 with advanced video).
                    self.respond(b"\x1b[?1;2c");
                }
                return;
            }
            'q' if intermediates.contains(&b'>') => {
                // XTVERSION query: answer with a DCS name/version string.
                self.respond(b"\x1bP>|voidtty 0.1\x1b\\");
                return;
            }
            'n' => {
                let request = param_raw(params, 0);
                let response = match request {
                    5 => Some(b"\x1b[0n".to_vec()),
                    6 => {
                        let screen = self.screen.lock().unwrap();
                        Some(
                            format!(
                                "\x1b[{};{}R",
                                screen.cursor_row + 1,
                                screen.cursor_col + 1
                            )
                            .into_bytes(),
                        )
                    }
                    _ => None,
                };
                if let Some(response) = response {
                    self.respond(&response);
                }
                return;
            }
            _ => {}
        }

        if action == 'm' {
            self.screen.lock().unwrap().apply_sgr(params);
            return;
        }

        let mut screen = self.screen.lock().unwrap();
        match action {
            'A' => screen.cursor_up(param_or(params, 0, 1)),
            'B' => screen.cursor_down(param_or(params, 0, 1)),
            'C' => screen.cursor_forward(param_or(params, 0, 1)),
            'D' => screen.cursor_back(param_or(params, 0, 1)),
            'E' => {
                screen.cursor_col = 0;
                screen.cursor_down(param_or(params, 0, 1));
            }
            'F' => {
                screen.cursor_col = 0;
                screen.cursor_up(param_or(params, 0, 1));
            }
            'G' | '`' => screen.cursor_col = param_or(params, 0, 1).saturating_sub(1),
            'd' => screen.cursor_row = param_or(params, 0, 1).saturating_sub(1),
            'H' | 'f' => {
                let row = param_or(params, 0, 1).saturating_sub(1);
                let col = param_or(params, 1, 1).saturating_sub(1);
                screen.set_cursor(row, col);
            }
            'J' => screen.erase_display(param_raw(params, 0)),
            'K' => screen.erase_line(param_raw(params, 0)),
            'L' => screen.insert_lines(param_or(params, 0, 1)),
            'M' => screen.delete_lines(param_or(params, 0, 1)),
            'P' => screen.delete_chars(param_or(params, 0, 1)),
            '@' => screen.insert_chars(param_or(params, 0, 1)),
            'X' => screen.erase_chars(param_or(params, 0, 1)),
            'S' => screen.scroll_up(param_or(params, 0, 1)),
            'T' => screen.scroll_down(param_or(params, 0, 1)),
            's' => screen.save_cursor(),
            'u' => screen.restore_cursor(),
            _ => {}
        }
        // Clamp an explicitly-set column/row that may have been out of range.
        screen.cursor_row = screen.cursor_row.min(screen.rows - 1);
        screen.cursor_col = screen.cursor_col.min(screen.cols - 1);
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        let mut screen = self.screen.lock().unwrap();
        match byte {
            b'7' => screen.save_cursor(),
            b'8' => screen.restore_cursor(),
            b'D' => screen.linefeed(),
            b'E' => {
                screen.cursor_col = 0;
                screen.linefeed();
            }
            b'M' => screen.reverse_index(),
            b'c' => screen.reset(),
            _ => {}
        }
    }
}

/// One running PTY session (one pane).
pub struct Pty {
    master: Box<dyn MasterPty + Send>,
    _child: Box<dyn Child + Send + Sync>,
    pub screen: Arc<Mutex<Screen>>,
    writer: SharedWriter,
    alive: Arc<AtomicBool>,
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
        let writer: SharedWriter =
            Arc::new(Mutex::new(pair.master.take_writer().context("taking PTY writer")?));
        let mut reader = pair.master.try_clone_reader().context("cloning PTY reader")?;

        let screen = Arc::new(Mutex::new(Screen::new(
            cols as usize,
            rows as usize,
            scrollback_limit,
        )));
        let screen_clone = Arc::clone(&screen);
        let writer_clone = Arc::clone(&writer);
        let alive = Arc::new(AtomicBool::new(true));
        let alive_clone = Arc::clone(&alive);

        thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || {
                let mut parser = Parser::new();
                let mut performer = Performer { screen: screen_clone, writer: writer_clone };
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            for &byte in &buf[..n] {
                                parser.advance(&mut performer, byte);
                            }
                        }
                        Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                alive_clone.store(false, Ordering::SeqCst);
            })
            .context("spawning reader thread")?;

        Ok(Self { master: pair.master, _child: child, screen, writer, alive })
    }

    /// True while the child shell is still running.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Sends raw bytes to the shell's stdin.
    ///
    /// Writes after the shell has exited are ignored instead of crashing the
    /// emulator; the pane will be reaped on the next event-loop tick.
    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        if !self.is_alive() {
            return Ok(());
        }
        let mut writer = self.writer.lock().unwrap();
        let result = writer.write_all(data).and_then(|()| writer.flush());
        match result {
            Ok(()) => Ok(()),
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::BrokenPipe
                        | ErrorKind::NotConnected
                        | ErrorKind::WriteZero
                        | ErrorKind::UnexpectedEof
                ) || e.raw_os_error() == Some(5) =>
            {
                self.alive.store(false, Ordering::SeqCst);
                Ok(())
            }
            Err(e) => Err(e).context("writing to PTY"),
        }
    }

    /// Resizes the PTY and the local screen grid to match the current pane size.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.master
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("resizing PTY")?;
        self.screen
            .lock()
            .unwrap()
            .resize(cols as usize, rows as usize);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    struct SharedVec(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedVec {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn performer() -> (Performer, Arc<Mutex<Vec<u8>>>) {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer: SharedWriter =
            Arc::new(Mutex::new(Box::new(SharedVec(Arc::clone(&output)))));
        (
            Performer {
                screen: Arc::new(Mutex::new(Screen::new(20, 4, 100))),
                writer,
            },
            output,
        )
    }

    #[test]
    fn responds_to_primary_device_attributes_query() {
        let (mut performer, output) = performer();
        let mut parser = Parser::new();
        for byte in b"\x1b[c" {
            parser.advance(&mut performer, *byte);
        }
        assert_eq!(&output.lock().unwrap()[..], b"\x1b[?1;2c");
    }

    #[test]
    fn applies_sgr_colors_and_attributes() {
        let (mut performer, _) = performer();
        let mut parser = Parser::new();
        for byte in b"\x1b[31;1mX\x1b[0mY\x1b[7mZ" {
            parser.advance(&mut performer, *byte);
        }

        let screen = performer.screen.lock().unwrap();
        let row = &screen.visible_cells()[0];
        assert_eq!(row[0].ch, 'X');
        assert_eq!(row[0].style.fg, Color::Indexed(1));
        assert!(row[0].style.bold);
        assert_eq!(row[1].style, Style::default());
        assert_eq!(row[2].ch, 'Z');
        assert!(row[2].style.reverse);
    }

    #[test]
    fn applies_extended_sgr_colors() {
        let (mut performer, _) = performer();
        let mut parser = Parser::new();
        for byte in b"\x1b[38;5;196mA\x1b[48;2;1;2;3mB" {
            parser.advance(&mut performer, *byte);
        }

        let screen = performer.screen.lock().unwrap();
        let row = &screen.visible_cells()[0];
        assert_eq!(row[0].style.fg, Color::Indexed(196));
        assert_eq!(row[1].style.bg, Color::Rgb(1, 2, 3));
    }

    #[test]
    fn carriage_return_overwrites_instead_of_appending() {
        let mut screen = Screen::new(20, 4, 100);
        for c in "ls".chars() {
            screen.print(c);
        }
        screen.cursor_col = 0;
        for c in "ll".chars() {
            screen.print(c);
        }
        assert_eq!(screen.visible_lines()[0], "ll");
    }

    #[test]
    fn erase_line_clears_previous_prompt() {
        let mut screen = Screen::new(20, 4, 100);
        for c in "old prompt".chars() {
            screen.print(c);
        }
        screen.cursor_col = 0;
        screen.erase_line(0);
        for c in "new".chars() {
            screen.print(c);
        }
        assert_eq!(screen.visible_lines()[0], "new");
    }

    #[test]
    fn linefeed_scrolls_at_bottom() {
        let mut screen = Screen::new(10, 2, 100);
        for c in "one".chars() {
            screen.print(c);
        }
        screen.cursor_col = 0;
        screen.linefeed();
        for c in "two".chars() {
            screen.print(c);
        }
        screen.cursor_col = 0;
        screen.linefeed();
        for c in "three".chars() {
            screen.print(c);
        }
        let lines = screen.visible_lines();
        assert_eq!(lines, vec!["two".to_owned(), "three".to_owned()]);
    }
}
