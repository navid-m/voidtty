use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    collections::HashMap,
    io::{ErrorKind, Read, Write},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};
use unicode_width::UnicodeWidthChar;
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
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub blink: bool,
    pub strikethrough: bool,
    pub reverse: bool,
}

/// One character cell in the visible grid.
#[derive(Debug, Clone, Copy)]
pub struct Cell {
    pub ch: char,
    pub style: Style,
    /// A combining mark rendered with `ch`.
    pub combining: Option<char>,
    /// True for the right half of a double-width character.
    pub wide: bool,
    /// Index into `Screen::links`, or 0 for no OSC 8 hyperlink.
    pub link: u32,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            style: Style::default(),
            combining: None,
            wide: false,
            link: 0,
        }
    }
}

impl Cell {
    fn plain(ch: char, style: Style, link: u32) -> Self {
        Self {
            ch,
            style,
            combining: None,
            wide: false,
            link,
        }
    }

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
            style: Style {
                fg,
                bg,
                ..Style::default()
            },
            combining: None,
            wide: false,
            link: 0,
        }
    }

    pub fn is_blank(&self) -> bool {
        self.ch == ' '
            && !self.wide
            && self.combining.is_none()
            && self.style == Style::default()
            && self.link == 0
    }

    /// Appends this cell's visible text. The right half of a wide character
    /// contributes no character of its own.
    pub fn push_text(&self, out: &mut String) {
        if self.wide {
            return;
        }
        out.push(self.ch);
        if let Some(mark) = self.combining {
            out.push(mark);
        }
    }
}

#[cfg(test)]
fn row_string(row: &[Cell]) -> String {
    let mut s = String::new();
    for cell in row {
        cell.push_text(&mut s);
    }
    let trimmed = s.trim_end().len();
    s.truncate(trimmed);
    s
}

/// DECSCUSR cursor shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Bar,
}

/// Cursor shape and blink requested by DECSCUSR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CursorStyle {
    pub shape: CursorShape,
    pub blink: bool,
}

/// Mouse reporting modes requested by the running program via DECSET.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseMode {
    /// Mouse events stay inside the emulator (selection, scrollback).
    #[default]
    Off,
    /// X10: button presses only.
    X10,
    /// DECSET 1000: presses and releases.
    Normal,
    /// DECSET 1002: presses, releases and drag motion.
    Button,
    /// DECSET 1003: presses, releases and all motion.
    Any,
}

/// State saved while an alternate screen buffer is active. Full-screen programs
/// such as Neovim and Helix switch to it and expect the normal screen (with the
/// shell prompt) to come back untouched when they exit.
#[derive(Clone)]
struct SavedScreen {
    grid: Vec<Vec<Cell>>,
    grid_wrapped: Vec<bool>,
    cursor_row: usize,
    cursor_col: usize,
    saved_row: usize,
    saved_col: usize,
    current_style: Style,
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
    /// `grid_wrapped[row]` is true when the logical line continues on the next
    /// row, so column resizes can reflow wrapped text instead of truncating it.
    grid_wrapped: Vec<bool>,
    cursor_row: usize,
    cursor_col: usize,
    saved_row: usize,
    saved_col: usize,
    scrollback: Vec<Vec<Cell>>,
    scrollback_limit: usize,
    /// Number of lines scrolled up from the live screen (0 = bottom).
    scroll_offset: usize,
    /// Fractional wheel delta accumulated between whole-line scrolls.
    scroll_pending: f32,
    current_style: Style,
    /// Normal-screen state while the alternate screen is active.
    alt_screen: Option<SavedScreen>,
    /// DECTCEM cursor visibility (`CSI ?25h` / `CSI ?25l`).
    cursor_visible: bool,
    /// DECSET 2004 bracketed paste.
    bracketed_paste: bool,
    /// Mouse reporting requested by the running program.
    mouse_mode: MouseMode,
    /// DECSET 1006 SGR mouse coordinate encoding.
    mouse_sgr: bool,
    title: String,
    /// Working directory reported by the shell via OSC 7.
    cwd: Option<String>,
    /// OSC 8 hyperlink URIs; index 0 is reserved for "no link".
    links: Vec<String>,
    link_ids: HashMap<String, u32>,
    current_link: u32,
    /// DECSCUSR cursor shape.
    cursor_style: CursorStyle,
    /// True once a blinking cell has been printed.
    blink_active: bool,
}

impl Screen {
    pub fn new(cols: usize, rows: usize, scrollback_limit: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            cols,
            rows,
            grid: vec![vec![Cell::default(); cols]; rows],
            grid_wrapped: vec![false; rows],
            cursor_row: 0,
            cursor_col: 0,
            saved_row: 0,
            saved_col: 0,
            scrollback: Vec::new(),
            scrollback_limit,
            scroll_offset: 0,
            scroll_pending: 0.0,
            current_style: Style::default(),
            alt_screen: None,
            cursor_visible: true,
            bracketed_paste: false,
            mouse_mode: MouseMode::Off,
            mouse_sgr: false,
            title: "Terminal".to_owned(),
            cwd: None,
            links: vec![String::new()],
            link_ids: HashMap::new(),
            current_link: 0,
            cursor_style: CursorStyle::default(),
            blink_active: false,
        }
    }

    /// Returns the visible grid as one string per row, trailing blanks trimmed.
    #[cfg(test)]
    pub fn visible_lines(&self) -> Vec<String> {
        self.grid.iter().map(|row| row_string(row)).collect()
    }

    /// Human-readable terminal title (from OSC 0/2).
    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn set_title(&mut self, title: String) {
        self.title = if title.is_empty() {
            "Terminal".to_owned()
        } else {
            title
        };
    }

    /// Working directory last reported with OSC 7.
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// Number of rows in the viewport.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Number of columns in the viewport.
    pub fn cols(&self) -> usize {
        self.cols
    }

    /// Cursor position as `(row, col)` within the live grid.
    pub fn cursor(&self) -> (usize, usize) {
        (self.cursor_row, self.cursor_col)
    }

    /// Whether the program running in the pane wants the cursor shown.
    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    pub fn cursor_style(&self) -> CursorStyle {
        self.cursor_style
    }

    /// Whether the cursor or a visible cell needs periodic repainting.
    pub fn needs_blink_animation(&mut self) -> bool {
        if self.cursor_visible && self.cursor_style.blink {
            return true;
        }
        if self.blink_active {
            let visible = self
                .display_rows()
                .any(|row| row.iter().any(|cell| cell.style.blink));
            self.blink_active = visible;
        }
        self.blink_active
    }

    /// Resolves an OSC 8 hyperlink id to its URI.
    pub fn link_uri(&self, link: u32) -> Option<&str> {
        if link == 0 {
            return None;
        }
        self.links.get(link as usize).map(String::as_str)
    }

    /// Returns the explicit OSC 8 link covering a display cell, and the range
    /// of cells sharing that link.
    pub fn link_at(&self, row: usize, col: usize) -> Option<(String, usize, usize)> {
        let cells = self.display_row(row);
        let link = cells.get(col)?.link;
        if link == 0 {
            return None;
        }
        let start = (0..col)
            .rev()
            .take_while(|&c| cells[c].link == link)
            .last()
            .unwrap_or(col);
        let end = (col + 1..cells.len())
            .take_while(|&c| cells[c].link == link)
            .last()
            .unwrap_or(col);
        Some((self.link_uri(link)?.to_owned(), start, end))
    }

    /// Maximum scroll offset (top of history).
    pub fn max_scroll(&self) -> usize {
        if self.alt_screen.is_some() {
            return 0;
        }
        self.scrollback.len()
    }

    /// Current scroll position, 0 = live screen, `max_scroll` = top.
    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset.min(self.scrollback.len())
    }

    /// Scrolls `lines` lines up (positive) or down (negative).
    pub fn scroll_by(&mut self, lines: isize) {
        if self.alt_screen.is_some() {
            return;
        }
        let max = self.scrollback.len() as isize;
        self.scroll_offset = (self.scroll_offset as isize + lines).clamp(0, max) as usize;
    }

    /// Accumulates fractional wheel deltas and scrolls whole lines.
    pub fn scroll_by_lines(&mut self, delta: f32) {
        self.scroll_pending += delta;
        let whole = self.scroll_pending.trunc();
        if whole != 0.0 {
            self.scroll_pending -= whole;
            self.scroll_by(whole as isize);
        }
    }

    pub fn set_scroll_offset(&mut self, offset: usize) {
        if self.alt_screen.is_some() {
            return;
        }
        self.scroll_offset = offset.min(self.scrollback.len());
    }

    /// Absolute history index of the first displayed row.
    pub fn view_top(&self) -> usize {
        self.scrollback.len().saturating_sub(self.scroll_offset())
    }

    /// Returns a row as currently displayed (history when scrolled up).
    pub fn display_row(&self, row: usize) -> &[Cell] {
        let index = self.view_top() + row;
        let cells = if index < self.scrollback.len() {
            &self.scrollback[index][..]
        } else {
            &self.grid[(index - self.scrollback.len()).min(self.rows - 1)][..]
        };
        &cells[..cells.len().min(self.cols)]
    }

    /// Returns a row by absolute history index, independent of the current
    /// scroll position.
    fn history_row(&self, index: usize) -> Option<&[Cell]> {
        let cells = if index < self.scrollback.len() {
            &self.scrollback[index][..]
        } else {
            self.grid.get(index - self.scrollback.len())?.as_slice()
        };
        Some(&cells[..cells.len().min(self.cols)])
    }

    /// Iterates over the rows currently visible in the viewport.
    pub fn display_rows(&self) -> impl Iterator<Item = &[Cell]> {
        (0..self.rows).map(move |row| self.display_row(row))
    }

    pub fn history_line_count(&self) -> usize {
        self.scrollback.len() + self.rows
    }

    /// Returns one line of scrollback or the live grid as plain text.
    pub fn history_line(&self, index: usize) -> String {
        let cells = if index < self.scrollback.len() {
            &self.scrollback[index][..]
        } else if index - self.scrollback.len() < self.grid.len() {
            &self.grid[index - self.scrollback.len()][..]
        } else {
            return String::new();
        };
        let mut text = String::new();
        for cell in cells {
            cell.push_text(&mut text);
        }
        text.trim_end().to_owned()
    }

    /// Searches scrollback and the live grid, newest lines first, appending
    /// `(line index, snippet)` pairs to `out`.
    pub fn search_history(&self, needle: &str, max_results: usize, out: &mut Vec<(usize, String)>) {
        if needle.is_empty() || max_results == 0 {
            return;
        }
        let needle_lower = needle.to_lowercase();
        let start = out.len();
        for index in (0..self.history_line_count()).rev() {
            let line = self.history_line(index);
            if line.is_empty() {
                continue;
            }
            let (pos, folded) = match line.find(needle) {
                Some(pos) => (pos, None),
                None => {
                    let folded = line.to_lowercase();
                    match folded.find(&needle_lower) {
                        Some(pos) => (pos, Some(folded)),
                        None => continue,
                    }
                }
            };
            let hit_char = folded.as_deref().unwrap_or(&line)[..pos].chars().count();
            let skip = hit_char.saturating_sub(40);
            let mut snippet: String = line.chars().skip(skip).take(180).collect();
            if skip > 0 {
                snippet.insert(0, '…');
            }
            out.push((index, snippet));
            if out.len() - start >= max_results {
                return;
            }
        }
    }

    /// Copies the displayed rows into `out` so the renderer can work without
    /// holding the screen lock. Reuses the buffers in `out` between frames.
    pub fn copy_visible(&self, out: &mut Vec<Vec<Cell>>) {
        out.resize_with(self.rows, Vec::new);
        for (dst, src) in out.iter_mut().zip(self.display_rows()) {
            dst.clear();
            dst.extend_from_slice(src);
        }
    }

    /// Extracts text between two absolute history cells (inclusive), one line
    /// per row. Unlike display coordinates these stay valid while scrolling.
    pub fn text_between(&self, start: (usize, usize), end: (usize, usize)) -> String {
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        let last_row = end.0.min(self.history_line_count().saturating_sub(1));
        let mut lines: Vec<String> = Vec::new();
        for row in start.0.min(last_row)..=last_row {
            let Some(cells) = self.history_row(row) else {
                lines.push(String::new());
                continue;
            };
            if cells.is_empty() {
                lines.push(String::new());
                continue;
            }
            let first = if row == start.0 {
                start.1.min(cells.len() - 1)
            } else {
                0
            };
            let last = if row == end.0 {
                end.1.min(cells.len() - 1)
            } else {
                cells.len() - 1
            };
            if last < first {
                lines.push(String::new());
                continue;
            }
            let mut line = String::new();
            for cell in &cells[first..=last] {
                cell.push_text(&mut line);
            }
            lines.push(line.trim_end().to_owned());
        }
        lines.join("\n")
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);

        if cols != self.cols {
            self.reflow(cols);
        }

        if rows < self.rows {
            let removed = self.rows - rows;
            for _ in 0..removed.min(self.grid.len()) {
                let line = self.grid.remove(0);
                if !self.grid_wrapped.is_empty() {
                    self.grid_wrapped.remove(0);
                }
                self.push_scrollback(line);
            }
            self.cursor_row = self.cursor_row.saturating_sub(removed);
            if let Some(last) = self.grid_wrapped.last_mut() {
                *last = false;
            }
        } else if rows > self.rows {
            self.grid
                .resize_with(rows, || vec![Cell::default(); self.cols]);
            self.grid_wrapped.resize(rows, false);
        }
        self.rows = rows;
        self.grid_wrapped.resize(self.rows, false);

        self.cursor_row = self.cursor_row.min(self.rows - 1);
        self.cursor_col = self.cursor_col.min(self.cols - 1);
        self.scroll_offset = self.scroll_offset.min(self.scrollback.len());
    }

    /// Rewraps the visible grid at a new column count. Soft-wrapped logical
    /// lines are re-flowed, and any rows that no longer fit move to scrollback
    /// instead of being truncated. (Scrollback keeps its original line breaks.)
    fn reflow(&mut self, new_cols: usize) {
        if new_cols == self.cols || self.grid.is_empty() {
            return;
        }
        let old_grid = std::mem::take(&mut self.grid);
        let old_wrapped = std::mem::take(&mut self.grid_wrapped);
        let old_cursor_row = self.cursor_row;
        let old_cursor_col = self.cursor_col;

        let mut rows: Vec<Vec<Cell>> = Vec::new();
        let mut wraps: Vec<bool> = Vec::new();
        let mut cursor = (
            old_cursor_row.min(old_grid.len().saturating_sub(1)),
            old_cursor_col,
        );

        // Trailing blank rows are just empty viewport space, not extra logical
        // lines; keeping them would make reflow push real content off the top.
        let mut end = old_grid.len();
        while end > 0 {
            let blank = old_grid[end - 1].iter().all(Cell::is_blank);
            if !blank {
                break;
            }
            end -= 1;
        }
        end = end.max(old_cursor_row + 1).min(old_grid.len());

        let mut i = 0;
        while i < end {
            let line_start = rows.len();
            let mut cells: Vec<Cell> = Vec::new();
            let mut cursor_offset = None;
            loop {
                let row = &old_grid[i];
                let wrapped = old_wrapped.get(i).copied().unwrap_or(false);
                let is_cursor = i == old_cursor_row;
                let content_len = if wrapped {
                    row.len()
                } else {
                    row.iter()
                        .rposition(|c| c.ch != ' ' || c.style != Style::default())
                        .map(|n| n + 1)
                        .unwrap_or(0)
                };
                let mut len = content_len;
                if is_cursor {
                    let col = old_cursor_col.min(row.len());
                    cursor_offset = Some(cells.len() + col);
                    len = len.max(col);
                }
                cells.extend_from_slice(&row[..len.min(row.len())]);
                i += 1;
                if !wrapped || i >= end {
                    break;
                }
            }

            let mut cursor_pos = None;
            let mut pos = 0;
            let mut last_pos = 0;
            while pos < cells.len() {
                last_pos = pos;
                let mut end = (pos + new_cols).min(cells.len());
                // Keep a double-width character and its continuation together.
                if end < cells.len() && cells[end].wide {
                    end -= 1;
                }
                if end == pos {
                    end = (pos + new_cols).min(cells.len());
                }
                let mut line = Vec::with_capacity(new_cols);
                line.extend_from_slice(&cells[pos..end]);
                line.resize(new_cols, Cell::default());
                rows.push(line);
                wraps.push(end < cells.len());
                if let Some(offset) = cursor_offset
                    && offset >= pos
                    && offset < end
                {
                    cursor_pos = Some((rows.len() - 1 - line_start, offset - pos));
                }
                pos = end;
            }

            if cells.is_empty() {
                rows.push(vec![Cell::default(); new_cols]);
                wraps.push(false);
            }

            if let Some(offset) = cursor_offset {
                let (line_off, col) = match cursor_pos {
                    Some(pos) => pos,
                    None if cells.is_empty() => (0, 0),
                    None => (
                        rows.len().saturating_sub(1).max(line_start) - line_start,
                        (offset - last_pos).min(new_cols - 1),
                    ),
                };
                cursor = (line_start + line_off, col);
            }
        }

        // Keep the viewport the same height even when reflow joins short rows.
        while rows.len() < self.rows {
            rows.push(vec![Cell::default(); new_cols]);
            wraps.push(false);
        }

        // Rows that no longer fit are history, not lost content.
        if rows.len() > self.rows {
            let overflow = rows.len() - self.rows;
            let removed: Vec<Vec<Cell>> = rows.drain(0..overflow).collect();
            wraps.drain(0..overflow);
            for line in removed {
                self.push_scrollback(line);
            }
            cursor.0 = cursor.0.saturating_sub(overflow);
        }

        self.grid = rows;
        self.grid_wrapped = wraps;
        self.cols = new_cols;
        self.cursor_row = cursor.0.min(self.rows - 1);
        self.cursor_col = cursor.1.min(new_cols - 1);
    }

    fn push_scrollback(&mut self, mut line: Vec<Cell>) {
        // The alternate screen has no scrollback of its own; keep the normal
        // screen's history intact so it reappears when the program exits.
        if self.alt_screen.is_some() || self.scrollback_limit == 0 {
            return;
        }
        while line.last().is_some_and(Cell::is_blank) {
            line.pop();
        }
        self.scrollback.push(line);
        if self.scrollback.len() > self.scrollback_limit {
            let drain = self.scrollback.len() - self.scrollback_limit;
            self.scrollback.drain(0..drain);
            self.scroll_offset = self.scroll_offset.saturating_sub(drain);
        }
        // Keep the view anchored to the same history while scrolled up.
        if self.scroll_offset > 0 {
            self.scroll_offset = (self.scroll_offset + 1).min(self.scrollback.len());
        }
    }

    fn print(&mut self, c: char) {
        let mut width = c.width().unwrap_or(1);
        if self.cols < 2 {
            width = 1;
        }
        if width == 0 {
            let (row, mut col) = if self.cursor_col > 0 {
                (self.cursor_row, self.cursor_col - 1)
            } else if self.cursor_row > 0 {
                (self.cursor_row - 1, self.cols - 1)
            } else {
                return;
            };
            if self.grid[row][col].wide && col > 0 {
                col -= 1;
            }
            // VS16 requests emoji presentation, which occupies two cells even
            // when the base character is narrow on its own.
            if c == '\u{FE0F}'
                && !self.grid[row][col].wide
                && col + 1 < self.cols
                && self.grid[row][col + 1].ch == ' '
                && !self.grid[row][col + 1].wide
                && self.grid[row][col + 1].combining.is_none()
            {
                let style = self.grid[row][col].style;
                let link = self.grid[row][col].link;
                self.grid[row][col + 1] = Cell {
                    ch: ' ',
                    style,
                    combining: None,
                    wide: true,
                    link,
                };
                if self.cursor_row == row && self.cursor_col == col + 1 {
                    self.cursor_col = (col + 2).min(self.cols);
                }
            }
            if self.grid[row][col].combining.is_none() {
                self.grid[row][col].combining = Some(c);
            }
            return;
        }

        if self.cursor_col >= self.cols {
            // Mark the row as soft-wrapped so a later column resize can join
            // it with the row the next character lands on.
            if let Some(wrapped) = self.grid_wrapped.get_mut(self.cursor_row) {
                *wrapped = true;
            }
            self.cursor_col = 0;
            self.linefeed();
        }

        // A double-width character cannot straddle the right margin.
        if width == 2 && self.cursor_col + 1 >= self.cols {
            if let Some(wrapped) = self.grid_wrapped.get_mut(self.cursor_row) {
                *wrapped = true;
            }
            self.cursor_col = 0;
            self.linefeed();
        }

        let row = self.cursor_row;
        let col = self.cursor_col;
        if self.grid[row][col].wide && col > 0 {
            self.grid[row][col - 1] = Cell::blank(self.current_style);
        }
        if col + 1 < self.cols && self.grid[row][col + 1].wide {
            self.grid[row][col + 1] = Cell::blank(self.current_style);
        }

        self.grid[row][col] = Cell::plain(c, self.current_style, self.current_link);
        if width == 2 {
            self.grid[row][col + 1] = Cell {
                ch: ' ',
                style: self.current_style,
                combining: None,
                wide: true,
                link: self.current_link,
            };
        }
        if self.current_style.blink {
            self.blink_active = true;
        }
        self.cursor_col += width;
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
                2 => self.current_style.dim = true,
                3 => self.current_style.italic = true,
                4 | 21 => self.current_style.underline = true,
                5 | 6 => self.current_style.blink = true,
                7 => self.current_style.reverse = true,
                9 => self.current_style.strikethrough = true,
                22 => {
                    self.current_style.bold = false;
                    self.current_style.dim = false;
                }
                23 => self.current_style.italic = false,
                24 => self.current_style.underline = false,
                25 => self.current_style.blink = false,
                27 => self.current_style.reverse = false,
                29 => self.current_style.strikethrough = false,
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
            let line = self.grid.remove(0);
            if !self.grid_wrapped.is_empty() {
                self.grid_wrapped.remove(0);
            }
            self.push_scrollback(line);
            self.grid.push(vec![Cell::default(); self.cols]);
            self.grid_wrapped.push(false);
        }
    }

    fn scroll_down(&mut self, n: usize) {
        for _ in 0..n.min(self.rows) {
            self.grid.pop();
            self.grid_wrapped.pop();
            self.grid.insert(0, vec![Cell::default(); self.cols]);
            self.grid_wrapped.insert(0, false);
        }
        if let Some(last) = self.grid_wrapped.last_mut() {
            *last = false;
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
                    self.grid_wrapped[row] = false;
                }
                self.grid_wrapped[self.cursor_row] = false;
            }
            1 => {
                for row in 0..self.cursor_row {
                    self.grid[row].fill(blank);
                    self.grid_wrapped[row] = false;
                }
                self.erase_line(1);
            }
            2 => {
                for row in &mut self.grid {
                    row.fill(blank);
                }
                self.grid_wrapped.fill(false);
                self.blink_active = false;
            }
            3 => {
                for row in &mut self.grid {
                    row.fill(blank);
                }
                self.grid_wrapped.fill(false);
                if self.alt_screen.is_none() {
                    self.scrollback.clear();
                }
                self.scroll_offset = 0;
                self.blink_active = false;
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
        if mode == 2 {
            self.grid_wrapped[self.cursor_row] = false;
        }
    }

    fn insert_lines(&mut self, n: usize) {
        for _ in 0..n.min(self.rows - self.cursor_row) {
            self.grid
                .insert(self.cursor_row, vec![Cell::default(); self.cols]);
            self.grid_wrapped.insert(self.cursor_row, false);
            self.grid.pop();
            self.grid_wrapped.pop();
        }
        if let Some(last) = self.grid_wrapped.last_mut() {
            *last = false;
        }
    }

    fn delete_lines(&mut self, n: usize) {
        for _ in 0..n.min(self.rows - self.cursor_row) {
            self.grid.remove(self.cursor_row);
            self.grid_wrapped.remove(self.cursor_row);
            self.grid.push(vec![Cell::default(); self.cols]);
            self.grid_wrapped.push(false);
        }
        // Do not accidentally join the row above with the line that moved up.
        if self.cursor_row > 0 {
            self.grid_wrapped[self.cursor_row - 1] = false;
        }
        if let Some(last) = self.grid_wrapped.last_mut() {
            *last = false;
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

    fn set_cursor_visible(&mut self, visible: bool) {
        self.cursor_visible = visible;
    }

    fn set_cursor_style(&mut self, style: CursorStyle) {
        self.cursor_style = style;
    }

    fn set_hyperlink(&mut self, uri: Option<&str>) {
        let Some(uri) = uri.filter(|uri| !uri.is_empty()) else {
            self.current_link = 0;
            return;
        };
        if let Some(&id) = self.link_ids.get(uri) {
            self.current_link = id;
            return;
        }
        // Bound the table so a runaway program cannot grow it forever.
        if self.links.len() >= 4096 {
            self.current_link = 0;
            return;
        }
        let id = self.links.len() as u32;
        self.links.push(uri.to_owned());
        self.link_ids.insert(uri.to_owned(), id);
        self.current_link = id;
    }

    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    fn set_bracketed_paste(&mut self, enabled: bool) {
        self.bracketed_paste = enabled;
    }

    /// Mouse reporting mode requested by the running program.
    pub fn mouse_mode(&self) -> MouseMode {
        self.mouse_mode
    }

    fn set_mouse_mode(&mut self, mode: MouseMode, enabled: bool) {
        self.mouse_mode = if enabled { mode } else { MouseMode::Off };
    }

    fn set_mouse_sgr(&mut self, enabled: bool) {
        self.mouse_sgr = enabled;
    }

    /// Encodes a mouse report for the program running in the pane.
    ///
    /// `code` is the xterm button code (with modifier bits and the motion bit
    /// already applied), `col`/`row` are zero-based cell coordinates. Releases
    /// use the legacy button-3 form unless SGR encoding was negotiated.
    pub fn encode_mouse(&self, code: u8, col: usize, row: usize, release: bool) -> Vec<u8> {
        let x = col + 1;
        let y = row + 1;
        if self.mouse_sgr {
            let terminator = if release { 'm' } else { 'M' };
            return format!("\x1b[<{code};{x};{y}{terminator}").into_bytes();
        }
        let code = if release { 3 } else { code };
        vec![
            b'\x1b',
            b'[',
            b'M',
            32 + code.min(223),
            32 + x.min(223) as u8,
            32 + y.min(223) as u8,
        ]
    }

    /// Switches between the normal and alternate screen buffers. Entering saves
    /// the normal screen and optionally clears the alternate one; leaving
    /// restores the normal screen exactly as it was before the program started.
    fn set_alt_screen(&mut self, enable: bool, clear: bool) {
        if enable {
            if self.alt_screen.is_some() {
                if clear {
                    for row in &mut self.grid {
                        row.fill(Cell::default());
                    }
                    self.grid_wrapped.fill(false);
                    self.cursor_row = 0;
                    self.cursor_col = 0;
                }
                return;
            }
            let saved = SavedScreen {
                grid: std::mem::take(&mut self.grid),
                grid_wrapped: std::mem::take(&mut self.grid_wrapped),
                cursor_row: self.cursor_row,
                cursor_col: self.cursor_col,
                saved_row: self.saved_row,
                saved_col: self.saved_col,
                current_style: self.current_style,
            };
            self.grid = vec![vec![Cell::default(); self.cols]; self.rows];
            self.grid_wrapped = vec![false; self.rows];
            self.alt_screen = Some(saved);
            self.cursor_row = 0;
            self.cursor_col = 0;
            self.saved_row = 0;
            self.saved_col = 0;
            self.scroll_offset = 0;
            self.scroll_pending = 0.0;
            self.current_style = Style::default();
            self.current_link = 0;
        } else if let Some(saved) = self.alt_screen.take() {
            let mut grid = saved.grid;
            grid.resize_with(self.rows, || vec![Cell::default(); self.cols]);
            for row in &mut grid {
                row.resize(self.cols, Cell::default());
            }
            let mut grid_wrapped = saved.grid_wrapped;
            grid_wrapped.resize(self.rows, false);
            self.grid = grid;
            self.grid_wrapped = grid_wrapped;
            self.cursor_row = saved.cursor_row.min(self.rows - 1);
            self.cursor_col = saved.cursor_col.min(self.cols - 1);
            self.saved_row = saved.saved_row;
            self.saved_col = saved.saved_col;
            self.current_style = saved.current_style;
            self.current_link = 0;
            self.scroll_offset = 0;
            self.scroll_pending = 0.0;
        }
    }

    fn reset(&mut self) {
        for row in &mut self.grid {
            row.fill(Cell::default());
        }
        self.grid_wrapped.fill(false);
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.saved_row = 0;
        self.saved_col = 0;
        self.scrollback.clear();
        self.scroll_offset = 0;
        self.scroll_pending = 0.0;
        self.current_style = Style::default();
        self.alt_screen = None;
        self.cursor_visible = true;
        self.bracketed_paste = false;
        self.mouse_mode = MouseMode::Off;
        self.mouse_sgr = false;
        self.current_link = 0;
        self.cursor_style = CursorStyle::default();
        self.blink_active = false;
        self.cwd = None;
    }
}

/// Extracts the local path from an OSC 7 `file://host/path` URI.
fn parse_osc7_uri(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let path = rest.find('/').map(|index| &rest[index..])?;
    let decoded = percent_decode(path);
    if decoded.is_empty() {
        None
    } else {
        Some(decoded)
    }
}

fn percent_decode(input: &str) -> String {
    if !input.contains('%') {
        return input.to_owned();
    }
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
        {
            out.push(high * 16 + low);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
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
    sync_output: Arc<AtomicBool>,
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

    fn osc_dispatch(&mut self, params: &[&[u8]], _: bool) {
        // OSC 0 / OSC 2 set the window (tab) title.
        if params.len() >= 2 && (params[0] == b"0" || params[0] == b"2") {
            if let Ok(title) = std::str::from_utf8(params[1]) {
                let title: String = title.chars().take(120).collect();
                self.screen.lock().unwrap().set_title(title);
            }
        }
        // OSC 7 reports the shell's current working directory.
        if params.first().is_some_and(|param| *param == b"7") {
            let mut uri = Vec::new();
            for (index, part) in params.iter().skip(1).enumerate() {
                if index > 0 {
                    uri.push(b';');
                }
                uri.extend_from_slice(part);
            }
            let cwd = std::str::from_utf8(&uri).ok().and_then(parse_osc7_uri);
            self.screen.lock().unwrap().cwd = cwd;
        }
        // OSC 8 ; params ; URI starts a hyperlink region. An empty URI closes it.
        if params.first().is_some_and(|param| *param == b"8") {
            let mut uri_bytes = Vec::new();
            for (index, part) in params.iter().skip(2).enumerate() {
                if index > 0 {
                    uri_bytes.push(b';');
                }
                uri_bytes.extend_from_slice(part);
            }
            let uri = std::str::from_utf8(&uri_bytes)
                .ok()
                .filter(|uri| !uri.is_empty() && !uri.chars().any(char::is_control));
            self.screen.lock().unwrap().set_hyperlink(uri);
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _: bool, action: char) {
        // DECSCUSR: `CSI Ps SP q` selects the cursor shape and blink.
        if action == 'q' && intermediates.contains(&b' ') {
            let ps = param_raw(params, 0);
            let style = CursorStyle {
                shape: match ps {
                    3 | 4 => CursorShape::Underline,
                    5 | 6 => CursorShape::Bar,
                    _ => CursorShape::Block,
                },
                blink: matches!(ps, 0 | 1 | 3 | 5),
            };
            self.screen.lock().unwrap().set_cursor_style(style);
            return;
        }

        // DEC private modes (`CSI ? Pm h` / `CSI ? Pm l`): alternate screen
        // buffer and cursor visibility. Full-screen editors rely on these.
        if intermediates.contains(&b'?') {
            let enable = match action {
                'h' => Some(true),
                'l' => Some(false),
                _ => None,
            };
            if let Some(enable) = enable {
                let mut screen = self.screen.lock().unwrap();
                for p in params.iter() {
                    match p.first().copied().unwrap_or(0) {
                        25 => screen.set_cursor_visible(enable),
                        47 => screen.set_alt_screen(enable, false),
                        1047 | 1049 => screen.set_alt_screen(enable, true),
                        1048 => {
                            if enable {
                                screen.save_cursor();
                            } else {
                                screen.restore_cursor();
                            }
                        }
                        2004 => screen.set_bracketed_paste(enable),
                        9 => screen.set_mouse_mode(MouseMode::X10, enable),
                        1000 => screen.set_mouse_mode(MouseMode::Normal, enable),
                        1002 => screen.set_mouse_mode(MouseMode::Button, enable),
                        1003 => screen.set_mouse_mode(MouseMode::Any, enable),
                        1006 => screen.set_mouse_sgr(enable),
                        2026 => self.sync_output.store(enable, Ordering::Release),
                        _ => {}
                    }
                }
                return;
            }
        }

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
                            format!("\x1b[{};{}R", screen.cursor_row + 1, screen.cursor_col + 1)
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

        // `CSI > 4 ; 1 m` (modifyOtherKeys) and other private `m` sequences are
        // not SGR; parsing them as such used to leave underline stuck on.
        if action == 'm' && intermediates.is_empty() {
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
            b'c' => {
                screen.reset();
                self.sync_output.store(false, Ordering::Release);
            }
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
    /// Set by the reader thread whenever the screen may have changed.
    dirty: Arc<AtomicBool>,
    /// Current PTY size, so no-op resizes do not signal the child with WINCH.
    size: Mutex<(u16, u16)>,
    /// DECSET 2026 state, readable without locking the screen.
    sync_output: Arc<AtomicBool>,
}

impl Pty {
    /// Spawns `shell` in a PTY sized `cols×rows` and starts a reader thread.
    /// A `cwd` reported by another pane is used when it still exists;
    /// otherwise the shell starts in the emulator's working directory.
    pub fn spawn(
        shell: &str,
        cols: u16,
        rows: u16,
        scrollback_limit: usize,
        shell_integration: bool,
        cwd: Option<&Path>,
    ) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("opening PTY")?;

        let mut cmd = CommandBuilder::new(shell);
        if shell_integration {
            crate::shell_integration::configure(&mut cmd, shell);
        }
        cmd.env("TERM", "xterm-256color");
        let start_dir = cwd
            .filter(|dir| dir.is_dir())
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok());
        if let Some(start_dir) = start_dir {
            cmd.cwd(start_dir);
        }

        let child = pair.slave.spawn_command(cmd).context("spawning shell")?;
        let writer: SharedWriter = Arc::new(Mutex::new(
            pair.master.take_writer().context("taking PTY writer")?,
        ));
        let mut reader = pair
            .master
            .try_clone_reader()
            .context("cloning PTY reader")?;

        let screen = Arc::new(Mutex::new(Screen::new(
            cols as usize,
            rows as usize,
            scrollback_limit,
        )));
        let shell_title = std::path::Path::new(shell)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("shell")
            .to_owned();
        screen.lock().unwrap().set_title(shell_title);

        let screen_clone = Arc::clone(&screen);
        let writer_clone = Arc::clone(&writer);
        let alive = Arc::new(AtomicBool::new(true));
        let alive_clone = Arc::clone(&alive);
        let dirty = Arc::new(AtomicBool::new(false));
        let dirty_clone = Arc::clone(&dirty);
        let sync_output = Arc::new(AtomicBool::new(false));
        let sync_clone = Arc::clone(&sync_output);

        thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || {
                let mut parser = Parser::new();
                let mut performer = Performer {
                    screen: screen_clone,
                    writer: writer_clone,
                    sync_output: sync_clone,
                };
                let mut buf = [0u8; 16384];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            for &byte in &buf[..n] {
                                parser.advance(&mut performer, byte);
                            }
                            dirty_clone.store(true, Ordering::Release);
                        }
                        Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                alive_clone.store(false, Ordering::SeqCst);
                dirty_clone.store(true, Ordering::Release);
            })
            .context("spawning reader thread")?;

        Ok(Self {
            master: pair.master,
            _child: child,
            screen,
            writer,
            alive,
            dirty,
            size: Mutex::new((cols, rows)),
            sync_output,
        })
    }

    /// Returns whether the screen changed since the last call. While DECSET
    /// 2026 is active, updates are coalesced until the program turns it off.
    pub fn take_dirty(&self) -> bool {
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return false;
        }
        !self.sync_output.load(Ordering::Acquire)
    }

    pub fn is_synchronized(&self) -> bool {
        self.sync_output.load(Ordering::Acquire)
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
        let mut size = self.size.lock().unwrap();
        if *size == (cols, rows) {
            return Ok(());
        }
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("resizing PTY")?;
        self.screen
            .lock()
            .unwrap()
            .resize(cols as usize, rows as usize);
        *size = (cols, rows);
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

    fn feed(parser: &mut Parser, performer: &mut Performer, bytes: &[u8]) {
        for byte in bytes {
            parser.advance(performer, *byte);
        }
    }

    fn performer() -> (Performer, Arc<Mutex<Vec<u8>>>) {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(SharedVec(Arc::clone(&output)))));
        (
            Performer {
                screen: Arc::new(Mutex::new(Screen::new(20, 4, 100))),
                writer,
                sync_output: Arc::new(AtomicBool::new(false)),
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

    fn styled_row(text: &str) -> Vec<Cell> {
        text.chars()
            .map(|ch| Cell {
                ch,
                ..Cell::default()
            })
            .collect()
    }

    #[test]
    fn scroll_view_shows_history_above_live_grid() {
        let mut screen = Screen::new(4, 2, 10);
        for c in "L0".chars() {
            screen.print(c);
        }
        screen.cursor_col = 0;
        screen.linefeed();
        for c in "L1".chars() {
            screen.print(c);
        }

        screen.push_scrollback(styled_row("H0"));
        screen.push_scrollback(styled_row("H1"));

        assert_eq!(row_string(screen.display_row(0)), "L0");
        screen.set_scroll_offset(1);
        assert_eq!(row_string(screen.display_row(0)), "H1");
        assert_eq!(row_string(screen.display_row(1)), "L0");
        screen.set_scroll_offset(2);
        assert_eq!(row_string(screen.display_row(0)), "H0");
        assert_eq!(row_string(screen.display_row(1)), "H1");
        screen.set_scroll_offset(99);
        assert_eq!(screen.scroll_offset(), 2);
        screen.scroll_by(-1);
        assert_eq!(screen.scroll_offset(), 1);
    }

    #[test]
    fn alternate_screen_restores_normal_buffer_on_exit() {
        let (mut performer, _) = performer();
        let mut parser = Parser::new();
        let feed = |parser: &mut Parser, performer: &mut Performer, bytes: &[u8]| {
            for byte in bytes {
                parser.advance(performer, *byte);
            }
        };

        feed(&mut parser, &mut performer, b"prompt");
        feed(&mut parser, &mut performer, b"\x1b[?1049h");
        feed(&mut parser, &mut performer, b"editor");
        let screen = performer.screen.lock().unwrap();
        assert_eq!(screen.visible_lines()[0], "editor");
        drop(screen);

        feed(&mut parser, &mut performer, b"\x1b[?1049l");
        let screen = performer.screen.lock().unwrap();
        assert_eq!(screen.visible_lines()[0], "prompt");
    }

    #[test]
    fn mouse_reporting_mode_and_sgr_encoding() {
        let (mut performer, _) = performer();
        let mut parser = Parser::new();
        let feed = |parser: &mut Parser, performer: &mut Performer, bytes: &[u8]| {
            for byte in bytes {
                parser.advance(performer, *byte);
            }
        };

        feed(&mut parser, &mut performer, b"\x1b[?1002h\x1b[?1006h");
        let screen = performer.screen.lock().unwrap();
        assert_eq!(screen.mouse_mode(), MouseMode::Button);
        assert_eq!(screen.encode_mouse(0, 4, 1, false), b"\x1b[<0;5;2M");
        assert_eq!(screen.encode_mouse(0, 4, 1, true), b"\x1b[<0;5;2m");
        drop(screen);

        feed(&mut parser, &mut performer, b"\x1b[?1002l");
        assert_eq!(
            performer.screen.lock().unwrap().mouse_mode(),
            MouseMode::Off
        );
    }

    #[test]
    fn mouse_encoding_falls_back_to_legacy_coordinates() {
        let screen = Screen::new(20, 4, 100);
        assert_eq!(screen.encode_mouse(0, 0, 0, false), b"\x1b[M\x20\x21\x21");
        assert_eq!(screen.encode_mouse(0, 0, 0, true), b"\x1b[M\x23\x21\x21");
    }

    #[test]
    fn dectcem_toggles_cursor_visibility() {
        let (mut performer, _) = performer();
        let mut parser = Parser::new();
        for byte in b"\x1b[?25l" {
            parser.advance(&mut performer, *byte);
        }
        assert!(!performer.screen.lock().unwrap().cursor_visible());

        for byte in b"\x1b[?25h" {
            parser.advance(&mut performer, *byte);
        }
        assert!(performer.screen.lock().unwrap().cursor_visible());
    }

    #[test]
    fn osc_updates_terminal_title() {
        let (mut performer, _) = performer();
        let mut parser = Parser::new();
        for byte in b"\x1b]2;my project\x07" {
            parser.advance(&mut performer, *byte);
        }
        let screen = performer.screen.lock().unwrap();
        assert_eq!(screen.title(), "my project");
    }

    #[test]
    fn applies_sgr_colors_and_attributes() {
        let (mut performer, _) = performer();
        let mut parser = Parser::new();
        for byte in b"\x1b[31;1mX\x1b[0mY\x1b[7mZ" {
            parser.advance(&mut performer, *byte);
        }

        let screen = performer.screen.lock().unwrap();
        let row = screen.display_row(0);
        assert_eq!(row[0].ch, 'X');
        assert_eq!(row[0].style.fg, Color::Indexed(1));
        assert!(row[0].style.bold);
        assert_eq!(row[1].style, Style::default());
        assert_eq!(row[2].ch, 'Z');
        assert!(row[2].style.reverse);
    }

    #[test]
    fn private_keyboard_mode_is_not_parsed_as_sgr() {
        let (mut performer, _) = performer();
        let mut parser = Parser::new();
        for byte in b"\x1b[>4;1mX" {
            parser.advance(&mut performer, *byte);
        }

        let screen = performer.screen.lock().unwrap();
        let row = screen.display_row(0);
        assert_eq!(row[0].ch, 'X');
        assert!(!row[0].style.underline);
        assert!(!row[0].style.bold);
    }

    #[test]
    fn applies_extended_sgr_colors() {
        let (mut performer, _) = performer();
        let mut parser = Parser::new();
        for byte in b"\x1b[38;5;196mA\x1b[48;2;1;2;3mB" {
            parser.advance(&mut performer, *byte);
        }

        let screen = performer.screen.lock().unwrap();
        let row = screen.display_row(0);
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
    fn resizing_columns_reflows_wrapped_text() {
        let mut screen = Screen::new(10, 4, 100);
        for c in "abcdefghijklm".chars() {
            screen.print(c);
        }

        screen.resize(5, 4);
        assert_eq!(screen.visible_lines(), vec!["abcde", "fghij", "klm", ""]);
        assert_eq!(screen.cursor(), (2, 3));

        screen.resize(10, 4);
        assert_eq!(screen.visible_lines(), vec!["abcdefghij", "klm", "", ""]);
        assert_eq!(screen.cursor(), (1, 3));
    }

    #[test]
    fn resizing_columns_moves_overflow_to_scrollback() {
        let mut screen = Screen::new(10, 2, 100);
        for c in "abcdefghijklmno".chars() {
            screen.print(c);
        }

        screen.resize(4, 2);
        assert_eq!(screen.visible_lines(), vec!["ijkl", "mno"]);
        screen.set_scroll_offset(2);
        assert_eq!(
            screen.display_rows().map(row_string).collect::<Vec<_>>(),
            vec!["abcd", "efgh"]
        );
    }

    #[test]
    fn resizing_columns_preserves_hard_line_breaks() {
        let mut screen = Screen::new(10, 4, 100);
        for c in "one".chars() {
            screen.print(c);
        }
        screen.cursor_col = 0;
        screen.linefeed();
        for c in "two".chars() {
            screen.print(c);
        }

        screen.resize(2, 4);
        assert_eq!(screen.visible_lines(), vec!["on", "e", "tw", "o"]);
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

    #[test]
    fn osc7_reports_the_working_directory() {
        let (mut performer, _) = performer();
        let mut parser = Parser::new();
        feed(
            &mut parser,
            &mut performer,
            b"\x1b]7;file://Resonance/home/navid/My%20Dir\x1b\\",
        );
        let screen = performer.screen.lock().unwrap();
        assert_eq!(screen.cwd(), Some("/home/navid/My Dir"));
    }

    #[test]
    fn osc7_bel_terminator_reports_the_working_directory() {
        let (mut performer, _) = performer();
        let mut parser = Parser::new();
        feed(
            &mut parser,
            &mut performer,
            b"\x1b]7;file://Resonance/tmp\x07",
        );
        let screen = performer.screen.lock().unwrap();
        assert_eq!(screen.cwd(), Some("/tmp"));
    }

    #[test]
    fn osc8_hyperlinks_attach_to_printed_cells() {
        let (mut performer, _) = performer();
        let mut parser = Parser::new();
        feed(
            &mut parser,
            &mut performer,
            b"\x1b]8;;https://example.com/a;b\x07",
        );
        performer.screen.lock().unwrap().print('x');
        let screen = performer.screen.lock().unwrap();
        let (uri, start, end) = screen.link_at(0, 0).expect("hyperlink on cell");
        assert_eq!(uri, "https://example.com/a;b");
        assert_eq!((start, end), (0, 0));
        drop(screen);

        feed(&mut parser, &mut performer, b"\x1b]8;;\x07");
        performer.screen.lock().unwrap().print('y');
        assert!(performer.screen.lock().unwrap().link_at(0, 1).is_none());
    }

    #[test]
    fn double_width_characters_advance_two_cells() {
        let mut screen = Screen::new(5, 2, 10);
        screen.print('😀');
        screen.print('x');
        assert_eq!(screen.visible_lines()[0], "😀x");
        assert_eq!(screen.cursor(), (0, 3));
        assert!(screen.display_row(0)[1].wide);
    }

    #[test]
    fn emoji_presentation_selector_widens_the_previous_cell() {
        let mut screen = Screen::new(5, 2, 10);
        screen.print('\u{2764}');
        screen.print('\u{FE0F}');
        screen.print('x');
        assert_eq!(screen.cursor(), (0, 3));
        assert!(screen.display_row(0)[1].wide);
    }

    #[test]
    fn combining_marks_do_not_advance_the_cursor() {
        let mut screen = Screen::new(5, 2, 10);
        screen.print('e');
        screen.print('\u{0301}');
        assert_eq!(screen.cursor(), (0, 1));
        assert_eq!(screen.visible_lines()[0], "e\u{0301}");
    }

    #[test]
    fn adds_dim_italic_blink_and_strikethrough_attributes() {
        let (mut performer, _) = performer();
        let mut parser = Parser::new();
        feed(&mut parser, &mut performer, b"\x1b[2;3;4;5;9mX");
        let screen = performer.screen.lock().unwrap();
        let style = screen.display_row(0)[0].style;
        assert!(style.dim && style.italic && style.underline);
        assert!(style.blink && style.strikethrough);
    }

    #[test]
    fn decscusr_selects_cursor_shape_and_blink() {
        let (mut performer, _) = performer();
        let mut parser = Parser::new();
        feed(&mut parser, &mut performer, b"\x1b[5 q");
        let style = performer.screen.lock().unwrap().cursor_style();
        assert_eq!(style.shape, CursorShape::Bar);
        assert!(style.blink);
    }

    #[test]
    fn decset_2026_toggles_synchronized_output() {
        let (mut performer, _) = performer();
        let mut parser = Parser::new();
        feed(&mut parser, &mut performer, b"\x1b[?2026h");
        assert!(
            performer
                .sync_output
                .load(std::sync::atomic::Ordering::Acquire)
        );
        feed(&mut parser, &mut performer, b"\x1b[?2026l");
        assert!(
            !performer
                .sync_output
                .load(std::sync::atomic::Ordering::Acquire)
        );
    }

    #[test]
    fn search_finds_scrollback_newest_first() {
        let mut screen = Screen::new(20, 4, 100);
        screen.push_scrollback(styled_row("first line"));
        screen.push_scrollback(styled_row("second Line"));
        let mut hits = Vec::new();
        screen.search_history("line", 10, &mut hits);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, 1);
        assert!(hits[0].1.contains("second Line"));
        assert_eq!(hits[1].0, 0);
    }

    #[test]
    fn scrolling_reaches_the_full_scrollback() {
        let mut screen = Screen::new(10, 2, 100);
        for i in 0..10 {
            screen.push_scrollback(styled_row(&format!("H{i}")));
        }
        screen.set_scroll_offset(5);
        assert_eq!(row_string(screen.display_row(0)), "H5");
        assert_eq!(row_string(screen.display_row(1)), "H6");
    }

    #[test]
    fn view_top_tracks_the_scroll_position() {
        let mut screen = Screen::new(10, 2, 100);
        for i in 0..5 {
            screen.push_scrollback(styled_row(&format!("H{i}")));
        }
        assert_eq!(screen.view_top(), 5);
        screen.set_scroll_offset(1);
        assert_eq!(screen.view_top(), 4);
        screen.set_scroll_offset(99);
        assert_eq!(screen.view_top(), 0);
    }

    #[test]
    fn selection_text_uses_absolute_history_rows() {
        let mut screen = Screen::new(4, 2, 10);
        for i in 0..4 {
            screen.push_scrollback(styled_row(&format!("H{i}")));
        }
        assert_eq!(screen.text_between((0, 0), (1, 1)), "H0\nH1");
        screen.set_scroll_offset(2);
        assert_eq!(screen.text_between((0, 0), (1, 1)), "H0\nH1");
    }
}
