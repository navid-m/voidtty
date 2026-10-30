mod app;
mod config;
mod font;
mod input;
mod renderer;
mod terminal;
mod ui;

use anyhow::{Context, Result};
use glfw::{Context as _, GlfwReceiver, OpenGlProfileHint, WindowEvent, WindowHint, WindowMode};

use app::App;
use config::Config;
use renderer::Renderer;
use terminal::{Color, Style};
use ui::{Rect, UiMetrics, pane_rects, text_cols, text_rows};

const FALLBACK_FONT: &[u8] = include_bytes!("../assets/font.ttf");

fn main() -> Result<()> {
    let config = Config::load()?;

    let font = load_font(&config);
    eprintln!("voidtty: using font \"{}\"", font.family);

    let mut glfw = glfw::init(glfw::fail_on_errors).context("initialising GLFW")?;

    glfw.window_hint(WindowHint::ContextVersion(3, 3));
    glfw.window_hint(WindowHint::OpenGlProfile(OpenGlProfileHint::Core));
    #[cfg(target_os = "macos")]
    glfw.window_hint(WindowHint::OpenGlForwardCompat(true));
    glfw.window_hint(WindowHint::TransparentFramebuffer(true));
    glfw.window_hint(WindowHint::Decorated(true));

    let (mut window, events) = glfw
        .create_window(
            config.win_width,
            config.win_height,
            "voidtty",
            WindowMode::Windowed,
        )
        .context("creating GLFW window")?;

    let content_scale = window.get_content_scale();
    let mut ui = UiMetrics::new(content_scale.0.max(content_scale.1));

    window.make_current();
    window.set_key_polling(true);
    window.set_char_polling(true);
    window.set_framebuffer_size_polling(true);
    window.set_mouse_button_polling(true);
    window.set_scroll_polling(true);
    window.set_cursor_pos_polling(true);
    glfw.set_swap_interval(glfw::SwapInterval::Sync(1));

    gl::load_with(|s| window.get_proc_address(s));

    let (fw, fh) = window.get_framebuffer_size();
    let mut renderer = Renderer::new(
        &font.data,
        font.face_index,
        config.font_size,
        fw as u32,
        fh as u32,
    )
    .context("initialising renderer")?;
    ui = ui.with_tab_bar_height(renderer.line_height());

    let mut app = App::new(config)?;

    // Size the PTY grids to the actual panes before the first frame.
    resize_ptys(&mut app, &renderer, ui);

    while !window.should_close() && app.running {
        glfw.poll_events();
        process_events(&events, &mut window, &mut app, &mut renderer, ui)?;
        app.reap_exited_panes();
        if !app.running {
            break;
        }
        if (renderer.font_size - app.config.font_size).abs() > f32::EPSILON {
            renderer.set_font_size(app.config.font_size)?;
            ui = ui.with_tab_bar_height(renderer.line_height());
            resize_ptys(&mut app, &renderer, ui);
        }
        draw_frame(&mut renderer, &app, ui)?;
        window.swap_buffers();
    }

    Ok(())
}

fn process_events(
    events: &GlfwReceiver<(f64, WindowEvent)>,
    window: &mut glfw::Window,
    app: &mut App,
    renderer: &mut Renderer,
    ui: UiMetrics,
) -> Result<()> {
    for (_, event) in glfw::flush_messages(events) {
        match event {
            WindowEvent::FramebufferSize(w, h) => {
                renderer.resize(w as u32, h as u32);
                resize_ptys(app, renderer, ui);
            }
            WindowEvent::Char(c) => {
                input::handle_char(c, app)?;
            }
            WindowEvent::Scroll(_, y) => {
                let (mx, my) = window.get_cursor_pos();
                let rects = pane_rects(
                    renderer.win_w,
                    renderer.win_h,
                    app.active_tab().panes.len(),
                    ui.tab_bar_h,
                );
                let index = ui::hit_pane(mx as f32 * ui.scale, my as f32 * ui.scale, &rects)
                    .unwrap_or(app.active_tab().focused);
                let tab = app.active_tab_mut();
                if let Some(pane) = tab.panes.get(index) {
                    pane.pty
                        .screen
                        .lock()
                        .unwrap()
                        .scroll_by_lines(y as f32);
                }
            }
            WindowEvent::CursorPos(x, y) => {
                if let Some((tab_idx, pane_idx)) = app.scroll_drag {
                    if app
                        .tabs
                        .get(tab_idx)
                        .is_some_and(|t| pane_idx < t.panes.len())
                    {
                        let rects = pane_rects(
                            renderer.win_w,
                            renderer.win_h,
                            app.tabs[tab_idx].panes.len(),
                            ui.tab_bar_h,
                        );
                        let rect = rects[pane_idx];
                        let pane = &app.tabs[tab_idx].panes[pane_idx];
                        let (max, rows) = {
                            let screen = pane.pty.screen.lock().unwrap();
                            (screen.max_scroll(), screen.rows())
                        };
                        if let Some(geom) = scrollbar_geometry(&rect, ui, max, rows) {
                            let offset =
                                scrollbar_offset_at(&geom, max, y as f32 * ui.scale);
                            pane.pty
                                .screen
                                .lock()
                                .unwrap()
                                .set_scroll_offset(offset);
                        }
                    }
                }
                let _ = x;
            }
            WindowEvent::MouseButton(_, glfw::Action::Release, _) => {
                app.scroll_drag = None;
            }
            WindowEvent::MouseButton(_, glfw::Action::Press, _) => {
                let (mx, my) = window.get_cursor_pos();
                let mx = mx as f32 * ui.scale;
                let my = my as f32 * ui.scale;
                let active = app.active_tab;
                let rects = pane_rects(
                    renderer.win_w,
                    renderer.win_h,
                    app.tabs[active].panes.len(),
                    ui.tab_bar_h,
                );

                let mut grabbed_scrollbar = false;
                for (idx, rect) in rects.iter().enumerate() {
                    let (max, rows) = {
                        let screen = app.tabs[active].panes[idx].pty.screen.lock().unwrap();
                        (screen.max_scroll(), screen.rows())
                    };
                    let Some(geom) = scrollbar_geometry(rect, ui, max, rows) else {
                        continue;
                    };
                    if mx >= geom.x
                        && mx < geom.x + geom.w
                        && my >= geom.y
                        && my < geom.y + geom.track_h
                    {
                        let offset = scrollbar_offset_at(&geom, max, my);
                        app.tabs[active].focused = idx;
                        app.tabs[active].panes[idx]
                            .pty
                            .screen
                            .lock()
                            .unwrap()
                            .set_scroll_offset(offset);
                        app.scroll_drag = Some((active, idx));
                        grabbed_scrollbar = true;
                        break;
                    }
                }

                if !grabbed_scrollbar {
                    if let Some(idx) = ui::hit_pane(mx, my, &rects) {
                        app.tabs[active].focused = idx;
                    }
                }
            }
            ev => {
                input::handle_event(ev, app)?;
            }
        }
    }
    Ok(())
}

/// Dark blue / red / black-leaning palette used for tab indicators.
const TAB_COLORS: &[(f32, f32, f32)] = &[
    (0.19, 0.30, 0.55),
    (0.55, 0.17, 0.22),
    (0.20, 0.21, 0.27),
    (0.30, 0.24, 0.47),
    (0.43, 0.12, 0.17),
    (0.13, 0.20, 0.36),
];

fn tab_color(accent: usize, active: bool) -> (f32, f32, f32) {
    let (r, g, b) = TAB_COLORS[accent % TAB_COLORS.len()];
    let k = if active { 1.25 } else { 0.72 };
    ((r * k).min(1.0), (g * k).min(1.0), (b * k).min(1.0))
}

const DEFAULT_FG: (f32, f32, f32) = (0.90, 0.90, 0.90);
const DEFAULT_BG: (f32, f32, f32) = (0.0, 0.0, 0.0);

const ANSI_COLORS: [[f32; 3]; 16] = [
    [0.00, 0.00, 0.00], [0.80, 0.25, 0.25], [0.35, 0.70, 0.35], [0.80, 0.70, 0.30],
    [0.30, 0.50, 0.90], [0.70, 0.40, 0.85], [0.30, 0.70, 0.80], [0.80, 0.80, 0.80],
    [0.45, 0.45, 0.50], [1.00, 0.45, 0.45], [0.55, 0.95, 0.55], [1.00, 0.90, 0.45],
    [0.50, 0.70, 1.00], [0.90, 0.60, 1.00], [0.50, 0.90, 0.95], [1.00, 1.00, 1.00],
];

fn indexed_color(index: u8) -> (f32, f32, f32) {
    match index {
        0..=15 => {
            let c = ANSI_COLORS[index as usize];
            (c[0], c[1], c[2])
        }
        16..=231 => {
            let i = index - 16;
            let levels = [0.0, 95.0, 135.0, 175.0, 215.0, 255.0];
            let r = levels[(i / 36) as usize] / 255.0;
            let g = levels[((i % 36) / 6) as usize] / 255.0;
            let b = levels[(i % 6) as usize] / 255.0;
            (r, g, b)
        }
        _ => {
            let v = (8 + (index - 232) * 10) as f32 / 255.0;
            (v, v, v)
        }
    }
}

fn color_rgb(color: Color, bold: bool) -> Option<(f32, f32, f32)> {
    let c = match color {
        Color::Default => return None,
        Color::Indexed(n) => indexed_color(n),
        Color::Rgb(r, g, b) => (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0),
    };
    if bold {
        Some(((c.0 * 1.15).min(1.0), (c.1 * 1.15).min(1.0), (c.2 * 1.15).min(1.0)))
    } else {
        Some(c)
    }
}

/// Resolves a cell style into (foreground, background, needs_background_fill).
fn cell_colors(style: Style) -> ((f32, f32, f32), (f32, f32, f32), bool) {
    let mut fg = color_rgb(style.fg, style.bold).unwrap_or(DEFAULT_FG);
    let mut bg = color_rgb(style.bg, false).unwrap_or(DEFAULT_BG);
    if style.bold && style.fg == Color::Default {
        fg = (1.0, 1.0, 1.0);
    }
    if style.reverse {
        std::mem::swap(&mut fg, &mut bg);
    }
    let fill = style.reverse || style.bg != Color::Default;
    (fg, bg, fill)
}

struct ScrollbarGeom {
    x: f32,
    y: f32,
    w: f32,
    track_h: f32,
    thumb_h: f32,
}

fn scrollbar_geometry(
    rect: &Rect,
    ui: UiMetrics,
    max_scroll: usize,
    rows: usize,
) -> Option<ScrollbarGeom> {
    if max_scroll == 0 {
        return None;
    }
    let track_y = rect.y + ui.pane_pad;
    let track_h = rect.h - ui.pane_pad * 2.0;
    if track_h <= 0.0 {
        return None;
    }
    let total = (max_scroll + rows) as f32;
    let thumb_h = (track_h * rows as f32 / total)
        .clamp((16.0 * ui.scale).min(track_h), track_h);
    Some(ScrollbarGeom {
        x: rect.x + rect.w - ui.pane_pad - ui.scrollbar_w,
        y: track_y,
        w: ui.scrollbar_w,
        track_h,
        thumb_h,
    })
}

fn scrollbar_offset_at(geom: &ScrollbarGeom, max_scroll: usize, y: f32) -> usize {
    let travel = (geom.track_h - geom.thumb_h).max(0.0);
    if travel <= 0.0 {
        return max_scroll;
    }
    let frac = ((y - geom.y) / travel).clamp(0.0, 1.0);
    ((1.0 - frac) * max_scroll as f32).round() as usize
}

fn draw_frame(renderer: &mut Renderer, app: &App, ui: UiMetrics) -> Result<()> {
    let opacity = app.config.opacity;
    // Black background respecting transparency.
    renderer.clear(0.0, 0.0, 0.0, opacity);

    let tab = app.active_tab();
    let rects = pane_rects(
        renderer.win_w,
        renderer.win_h,
        tab.panes.len(),
        ui.tab_bar_h,
    );

    // Integrated tab strip: same black background as the terminal, with a
    // Unicode status dot and the terminal title for each tab.
    let tab_cw = renderer.cell_w();
    let tab_lh = renderer.line_height();
    let tab_text_y = (ui.tab_bar_h - tab_lh) * 0.5;
    let mut tab_x = ui.tab_bar_pad;

    for (i, t) in app.tabs.iter().enumerate() {
        let active = i == app.active_tab;
        let title = t
            .panes
            .get(t.focused)
            .or_else(|| t.panes.first())
            .map(|pane| pane.pty.screen.lock().unwrap().title().to_owned())
            .unwrap_or_else(|| "Terminal".to_owned());
        let title: String = title.chars().take(28).collect();
        let label = format!("{} {}", if active { "●" } else { "○" }, title);
        let cell_count = label.chars().count();
        let width = cell_count as f32 * tab_cw;
        let (r, g, b) = tab_color(t.accent, active);

        if active {
            let bg = ui.tab_bar_pad * 0.4;
            renderer.draw_round_rect(
                tab_x - bg,
                0.0,
                width + bg * 2.0,
                ui.tab_bar_h,
                4.0 * ui.scale,
                r * 0.5,
                g * 0.5,
                b * 0.5,
                0.12 * opacity,
            );
            renderer.draw_rect(
                tab_x,
                ui.tab_bar_h - ui.scale,
                width,
                ui.scale,
                r,
                g,
                b,
                0.9 * opacity,
            );
        }

        let mut colors = Vec::with_capacity(cell_count);
        colors.push((r, g, b));
        let fg = if active {
            (0.92, 0.92, 0.95)
        } else {
            (0.55, 0.55, 0.60)
        };
        for _ in 1..cell_count {
            colors.push(fg);
        }
        renderer.draw_text_styled(
            &label,
            &colors,
            tab_x,
            tab_text_y,
            if active { 1.0 } else { 0.85 },
        )?;

        tab_x += width + ui.tab_gap;
    }

    // Pane borders + content.
    for (i, (pane, rect)) in tab.panes.iter().zip(rects.iter()).enumerate() {
        let focused = i == tab.focused;

        // Pane background.
        renderer.draw_rect(rect.x, rect.y, rect.w, rect.h, 0.0, 0.0, 0.0, opacity);

        // Border highlight for focused pane.
        if focused {
            let t = 1.5 * ui.scale;
            renderer.draw_rect(rect.x, rect.y, rect.w, t, 0.3, 0.7, 1.0, 0.8);
            renderer.draw_rect(rect.x, rect.y, t, rect.h, 0.3, 0.7, 1.0, 0.8);
            renderer.draw_rect(rect.x, rect.y + rect.h - t, rect.w, t, 0.3, 0.7, 1.0, 0.8);
            renderer.draw_rect(rect.x + rect.w - t, rect.y, t, rect.h, 0.3, 0.7, 1.0, 0.8);
        } else {
            let t = ui.scale;
            let c = 0.2f32;
            renderer.draw_rect(rect.x, rect.y, rect.w, t, c, c, c, 0.6);
            renderer.draw_rect(rect.x, rect.y, t, rect.h, c, c, c, 0.6);
        }

        // Terminal cells: backgrounds first, then shaped text with per-cell color.
        let screen = pane.pty.screen.lock().unwrap();
        let cw = renderer.cell_w();
        let lh = renderer.line_height();
        let (cursor_row, cursor_col) = screen.cursor();
        let show_cursor = focused && screen.scroll_offset() == 0;

        for (row, cells) in screen.display_rows().enumerate() {
            let y = rect.y + ui.pane_pad + row as f32 * lh;
            let cursor_here = show_cursor && row == cursor_row;
            let cursor_cell = cursor_here.then(|| cursor_col.min(cells.len().saturating_sub(1)));

            let mut last = cells
                .iter()
                .rposition(|c| c.ch != ' ' || c.style != Style::default())
                .map(|i| i + 1)
                .unwrap_or(0);
            if let Some(col) = cursor_cell {
                last = last.max(col + 1);
            }
            if last == 0 {
                continue;
            }

            let mut colors = Vec::with_capacity(last);
            for (col, cell) in cells[..last].iter().enumerate() {
                let style = if Some(col) == cursor_cell {
                    // Solid, non-blinking block cursor.
                    Style {
                        fg: Color::Default,
                        bg: Color::Default,
                        bold: false,
                        underline: false,
                        reverse: true,
                    }
                } else {
                    cell.style
                };
                let (fg, bg, fill) = cell_colors(style);
                let x = rect.x + ui.pane_pad + col as f32 * cw;
                if fill {
                    renderer.draw_rect(x, y, cw, lh, bg.0, bg.1, bg.2, opacity);
                }
                if style.underline {
                    renderer.draw_rect(x, y + lh - 1.5, cw, 1.0, fg.0, fg.1, fg.2, opacity);
                }
                colors.push(fg);
            }

            let text: String = cells[..last].iter().map(|c| c.ch).collect();
            renderer.draw_text_styled(&text, &colors, rect.x + ui.pane_pad, y, 1.0)?;
        }

        // Vertical scrollbar, drawn inside the pane's right padding.
        let max_scroll = screen.max_scroll();
        if let Some(geom) = scrollbar_geometry(rect, ui, max_scroll, screen.rows()) {
            let radius = geom.w * 0.5;
            let frac = screen.scroll_offset() as f32 / max_scroll as f32;
            let thumb_y = geom.y + (geom.track_h - geom.thumb_h) * (1.0 - frac);

            renderer.draw_round_rect(
                geom.x, geom.y, geom.w, geom.track_h, radius,
                1.0, 1.0, 1.0, 0.05 * opacity,
            );
            renderer.draw_round_rect(
                geom.x, thumb_y, geom.w, geom.thumb_h, radius,
                0.55, 0.60, 0.70,
                if focused { 0.55 } else { 0.35 } * opacity,
            );
        }
    }

    Ok(())
}

/// Recalculates PTY cols/rows after a resize event.
fn resize_ptys(app: &mut App, renderer: &Renderer, ui: UiMetrics) {
    let cw = renderer.cell_w();
    let lh = renderer.line_height();
    for tab in app.tabs.iter_mut() {
        let rects = pane_rects(
            renderer.win_w,
            renderer.win_h,
            tab.panes.len(),
            ui.tab_bar_h,
        );
        for (pane, rect) in tab.panes.iter_mut().zip(rects.iter()) {
            let cols = text_cols(rect.w, cw, ui.pane_pad, ui.scrollbar_gutter).max(1) as u16;
            let rows = text_rows(rect.h, lh, ui.pane_pad).max(1) as u16;
            let _ = pane.pty.resize(cols, rows);
        }
    }
}

fn load_font(config: &Config) -> font::LoadedFont {
    if let Ok(path) = std::env::var("voidtty_FONT") {
        if let Ok(data) = std::fs::read(&path) {
            return font::LoadedFont {
                data,
                face_index: 0,
                family: path,
            };
        }
    }
    font::load(&config.font_family).unwrap_or_else(|| font::LoadedFont {
        data: FALLBACK_FONT.to_vec(),
        face_index: 0,
        family: "bundled fallback".into(),
    })
}
