mod app;
mod config;
mod font;
mod input;
mod renderer;
mod terminal;
mod ui;

use std::env;
use anyhow::{Context, Result};
use glfw::{Context as _, GlfwReceiver, OpenGlProfileHint, WindowEvent, WindowHint, WindowMode};

use app::App;
use config::Config;
use renderer::Renderer;
use terminal::{Color, Style};
use ui::{Rect, UiMetrics, layout_rects, pane_content, text_cols, text_rows};

const FALLBACK_FONT: &[u8] = include_bytes!("../assets/font.ttf");

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    for arg in args {
        if arg == "--about" || arg == "-a" || arg == "-v" || arg == "--version" {
            eprintln!("voidtty v{}\nBy Navid Momtahen (C) - 2026", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
    }

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

    set_window_icon(&mut window);

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
            app.needs_resize = false;
        } else if app.needs_resize {
            resize_ptys(&mut app, &renderer, ui);
            app.needs_resize = false;
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
                let rects = pane_layout(app, renderer);
                let id = ui::hit_pane(mx as f32 * ui.scale, my as f32 * ui.scale, &rects)
                    .unwrap_or(app.focused);
                if let Some(pane) = app.pane_mut(id) {
                    pane.active_mut()
                        .pty
                        .screen
                        .lock()
                        .unwrap()
                        .scroll_by_lines(y as f32);
                }
            }
            WindowEvent::CursorPos(x, y) => {
                if let Some(id) = app.scroll_drag {
                    let rects = pane_layout(app, renderer);
                    if let Some((_, rect)) = rects.iter().find(|(pane_id, _)| *pane_id == id) {
                        let content = pane_content(*rect, ui.tab_bar_h);
                        if let Some(pane) = app.pane(id) {
                            let (max, rows) = {
                                let screen = pane.active().pty.screen.lock().unwrap();
                                (screen.max_scroll(), screen.rows())
                            };
                            if let Some(geom) = scrollbar_geometry(&content, ui, max, rows) {
                                let offset = scrollbar_offset_at(&geom, max, y as f32 * ui.scale);
                                pane.active()
                                    .pty
                                    .screen
                                    .lock()
                                    .unwrap()
                                    .set_scroll_offset(offset);
                            }
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
                let rects = pane_layout(app, renderer);

                let mut grabbed_scrollbar = false;
                for (id, rect) in &rects {
                    let content = pane_content(*rect, ui.tab_bar_h);
                    let Some(pane) = app.pane(*id) else { continue };
                    let (max, rows) = {
                        let screen = pane.active().pty.screen.lock().unwrap();
                        (screen.max_scroll(), screen.rows())
                    };
                    let Some(geom) = scrollbar_geometry(&content, ui, max, rows) else {
                        continue;
                    };
                    if mx >= geom.x
                        && mx < geom.x + geom.w
                        && my >= geom.y
                        && my < geom.y + geom.track_h
                    {
                        let offset = scrollbar_offset_at(&geom, max, my);
                        pane.active()
                            .pty
                            .screen
                            .lock()
                            .unwrap()
                            .set_scroll_offset(offset);
                        app.focused = *id;
                        app.scroll_drag = Some(*id);
                        grabbed_scrollbar = true;
                        break;
                    }
                }

                if !grabbed_scrollbar && let Some(id) = ui::hit_pane(mx, my, &rects) {
                    app.focused = id;
                }
            }
            ev => {
                input::handle_event(ev, app)?;
            }
        }
    }
    Ok(())
}

/// Computes the current pane rectangles from the split tree.
fn pane_layout(app: &App, renderer: &Renderer) -> Vec<(usize, Rect)> {
    let area = Rect::new(0.0, 0.0, renderer.win_w, renderer.win_h);
    app.layout
        .as_ref()
        .map(|layout| layout_rects(layout, area))
        .unwrap_or_default()
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
    [0.00, 0.00, 0.00],
    [0.80, 0.25, 0.25],
    [0.35, 0.70, 0.35],
    [0.80, 0.70, 0.30],
    [0.30, 0.50, 0.90],
    [0.70, 0.40, 0.85],
    [0.30, 0.70, 0.80],
    [0.80, 0.80, 0.80],
    [0.45, 0.45, 0.50],
    [1.00, 0.45, 0.45],
    [0.55, 0.95, 0.55],
    [1.00, 0.90, 0.45],
    [0.50, 0.70, 1.00],
    [0.90, 0.60, 1.00],
    [0.50, 0.90, 0.95],
    [1.00, 1.00, 1.00],
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
        Some((
            (c.0 * 1.15).min(1.0),
            (c.1 * 1.15).min(1.0),
            (c.2 * 1.15).min(1.0),
        ))
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
    let thumb_h = (track_h * rows as f32 / total).clamp((16.0 * ui.scale).min(track_h), track_h);
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

    for (id, rect) in pane_layout(app, renderer) {
        let Some(pane) = app.pane(id) else { continue };
        draw_pane(renderer, pane, rect, id == app.focused, ui, opacity)?;
    }

    Ok(())
}

fn draw_pane(
    renderer: &mut Renderer,
    pane: &app::Pane,
    rect: Rect,
    focused: bool,
    ui: UiMetrics,
    opacity: f32,
) -> Result<()> {
    // Pane background.
    renderer.draw_rect(rect.x, rect.y, rect.w, rect.h, 0.0, 0.0, 0.0, opacity);

    // Border highlight for the focused pane.
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

    draw_tab_bar(renderer, pane, rect, ui, opacity)?;

    let content = pane_content(rect, ui.tab_bar_h);
    let tab = pane.active();
    let screen = tab.pty.screen.lock().unwrap();
    let cw = renderer.cell_w();
    let lh = renderer.line_height();
    let (cursor_row, cursor_col) = screen.cursor();
    let show_cursor = focused && screen.scroll_offset() == 0 && screen.cursor_visible();

    for (row, cells) in screen.display_rows().enumerate() {
        let y = content.y + ui.pane_pad + row as f32 * lh;
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
            let x = content.x + ui.pane_pad + col as f32 * cw;
            if fill {
                renderer.draw_rect(x, y, cw, lh, bg.0, bg.1, bg.2, opacity);
            }
            if style.underline {
                renderer.draw_rect(x, y + lh - 1.5, cw, 1.0, fg.0, fg.1, fg.2, opacity);
            }
            colors.push(fg);
        }

        let text: String = cells[..last].iter().map(|c| c.ch).collect();
        renderer.draw_text_styled(&text, &colors, content.x + ui.pane_pad, y, 1.0)?;
    }

    // Vertical scrollbar, drawn inside the pane's right padding.
    let max_scroll = screen.max_scroll();
    if let Some(geom) = scrollbar_geometry(&content, ui, max_scroll, screen.rows()) {
        let radius = geom.w * 0.5;
        let frac = screen.scroll_offset() as f32 / max_scroll as f32;
        let thumb_y = geom.y + (geom.track_h - geom.thumb_h) * (1.0 - frac);

        renderer.draw_round_rect(
            geom.x,
            geom.y,
            geom.w,
            geom.track_h,
            radius,
            1.0,
            1.0,
            1.0,
            0.05 * opacity,
        );
        renderer.draw_round_rect(
            geom.x,
            thumb_y,
            geom.w,
            geom.thumb_h,
            radius,
            0.55,
            0.60,
            0.70,
            if focused { 0.55 } else { 0.35 } * opacity,
        );
    }

    Ok(())
}

/// Draws one pane's own tab strip along the top of its rectangle.
fn draw_tab_bar(
    renderer: &mut Renderer,
    pane: &app::Pane,
    rect: Rect,
    ui: UiMetrics,
    opacity: f32,
) -> Result<()> {
    let tab_cw = renderer.cell_w();
    let tab_lh = renderer.line_height();
    let tab_text_y = rect.y + (ui.tab_bar_h - tab_lh) * 0.5;
    let right = rect.x + rect.w;
    let mut tab_x = rect.x + ui.tab_bar_pad;

    for (i, t) in pane.tabs.iter().enumerate() {
        if tab_x >= right {
            break;
        }
        let active = i == pane.active_tab;
        let title = t.pty.screen.lock().unwrap().title().to_owned();
        let title: String = title.chars().take(28).collect();
        let label = format!("{} {}", if active { "●" } else { "○" }, title);
        let cell_count = label.chars().count();
        let width = cell_count as f32 * tab_cw;
        if tab_x + width > right {
            break;
        }
        let (r, g, b) = tab_color(t.accent, active);

        if active {
            let bg = ui.tab_bar_pad * 0.4;
            renderer.draw_round_rect(
                tab_x - bg,
                rect.y,
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
                rect.y + ui.tab_bar_h - ui.scale,
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

    Ok(())
}

/// Recalculates PTY cols/rows after a resize event.
fn resize_ptys(app: &mut App, renderer: &Renderer, ui: UiMetrics) {
    let cw = renderer.cell_w();
    let lh = renderer.line_height();
    for (id, rect) in pane_layout(app, renderer) {
        let content = pane_content(rect, ui.tab_bar_h);
        let cols = text_cols(content.w, cw, ui.pane_pad, ui.scrollbar_gutter).max(1) as u16;
        let rows = text_rows(content.h, lh, ui.pane_pad).max(1) as u16;
        if let Some(pane) = app.pane(id) {
            for tab in &pane.tabs {
                let _ = tab.pty.resize(cols, rows);
            }
        }
    }
}

/// Sets the window/taskbar icon from the bundled PNG. macOS uses the icon in
/// the app bundle instead, so this mainly affects Windows and Linux.
fn set_window_icon(window: &mut glfw::Window) {
    let bytes = include_bytes!("../assets/icon.png");
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let Ok(mut reader) = decoder.read_info() else {
        return;
    };
    let mut buffer = vec![0; reader.output_buffer_size()];
    let Ok(info) = reader.next_frame(&mut buffer) else {
        return;
    };
    if info.bit_depth != png::BitDepth::Eight {
        return;
    }
    let data = &buffer[..info.buffer_size()];
    let pixels: Vec<u32> = match info.color_type {
        png::ColorType::Rgba => data
            .chunks_exact(4)
            .map(|px| u32::from_ne_bytes([px[0], px[1], px[2], px[3]]))
            .collect(),
        png::ColorType::Rgb => data
            .chunks_exact(3)
            .map(|px| u32::from_ne_bytes([px[0], px[1], px[2], 0xff]))
            .collect(),
        _ => return,
    };
    window.set_icon_from_pixels(vec![glfw::PixelImage {
        width: info.width,
        height: info.height,
        pixels,
    }]);
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
