mod app;
mod config;
mod font;
mod input;
mod opener;
mod renderer;
mod terminal;
mod ui;

use anyhow::{Context, Result};
use glfw::{Context as _, GlfwReceiver, OpenGlProfileHint, WindowEvent, WindowHint, WindowMode};
use std::{
    env,
    path::{Path, PathBuf},
    time::Instant,
};

use app::{App, SplitDir};
use config::Config;
use renderer::Renderer;
use terminal::{Cell, Color, CursorShape, MouseMode, Style};
use ui::{Rect, UiMetrics, layout_rects, pane_content, text_cols, text_rows};

const FALLBACK_FONT: &[u8] = include_bytes!("../assets/font.ttf");

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    for arg in args {
        if arg == "--about" || arg == "-a" || arg == "-v" || arg == "--version" {
            eprintln!(
                "voidtty v{} ({})\nBy Navid Momtahen (C) - 2026",
                env!("CARGO_PKG_VERSION"),
                env!("VOIDTTY_GIT_COMMIT")
            );
            return Ok(());
        }
    }

    let config = Config::load()?;

    let font = load_font(&config);
    eprintln!("voidtty: using font \"{}\"", font.0.family);

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
    // Repaint when X11/compositor asks for it (expose, unminimise, focus).
    window.set_refresh_polling(true);
    window.set_focus_polling(true);
    window.set_iconify_polling(true);
    // Draw only when something changes and present immediately, so keystroke
    // echo is not held back until the next vertical blank.
    glfw.set_swap_interval(glfw::SwapInterval::None);

    gl::load_with(|s| window.get_proc_address(s));

    let (fw, fh) = window.get_framebuffer_size();
    let mut renderer = Renderer::new(
        &font.0.data,
        font.0.face_index,
        config.font_size,
        fw as u32,
        fh as u32,
        font.1,
    )
    .context("initialising renderer")?;
    ui = ui.with_tab_bar_height(renderer.line_height());

    let mut app = App::new(config)?;
    let mut clipboard = arboard::Clipboard::new().ok();

    // Size the PTY grids to the actual panes before the first frame.
    resize_ptys(&mut app, &renderer, ui);

    let blink_epoch = Instant::now();
    let mut last_blink_phase = true;
    let mut redraw = true;
    let mut was_iconified = window.is_iconified();
    while !window.should_close() && app.running {
        if redraw {
            glfw.poll_events();
        } else {
            // Block for input, waking briefly to pick up PTY output. Keystrokes
            // themselves wake this immediately.
            glfw.wait_events_timeout(0.002);
        }

        let events_changed = process_events(
            &events,
            &mut window,
            &mut app,
            &mut renderer,
            ui,
            &mut clipboard,
        )?;
        app.reap_exited_panes();
        if !app.running {
            break;
        }

        let mut layout_changed = false;
        if (renderer.font_size - app.config.font_size).abs() > f32::EPSILON {
            renderer.set_font_size(app.config.font_size)?;
            ui = ui.with_tab_bar_height(renderer.line_height());
            layout_changed = true;
        }
        if app.needs_resize || layout_changed {
            resize_ptys(&mut app, &renderer, ui);
            app.needs_resize = false;
            layout_changed = true;
        }

        let output_changed = app
            .panes
            .iter()
            .any(|pane| pane.tabs.iter().any(|tab| tab.pty.take_dirty()));

        // Some window managers do not deliver an expose event on unminimise, so
        // watch the iconified state as well and repaint when it clears.
        let iconified = window.is_iconified();
        let restored = was_iconified && !iconified;
        was_iconified = iconified;
        if restored {
            // Re-sync in case the WM skipped the framebuffer-size event.
            let (fw, fh) = window.get_framebuffer_size();
            if fw > 0 && fh > 0 && (fw as f32 != renderer.win_w || fh as f32 != renderer.win_h) {
                renderer.resize(fw as u32, fh as u32);
                resize_ptys(&mut app, &renderer, ui);
            }
        }

        redraw = events_changed || output_changed || layout_changed || restored;

        let blink_on = (blink_epoch.elapsed().as_millis() / 500).is_multiple_of(2);
        if blink_on != last_blink_phase {
            last_blink_phase = blink_on;
            if app.active_needs_blink_animation() {
                redraw = true;
            }
        }

        // DECSET 2026 asks the terminal to present the screen atomically.
        if redraw && app.active_synchronized_output() {
            redraw = false;
        }
        if redraw {
            draw_frame(&mut renderer, &app, ui, blink_on)?;
            window.swap_buffers();
        }
    }

    Ok(())
}

fn process_events(
    events: &GlfwReceiver<(f64, WindowEvent)>,
    window: &mut glfw::Window,
    app: &mut App,
    renderer: &mut Renderer,
    ui: UiMetrics,
    clipboard: &mut Option<arboard::Clipboard>,
) -> Result<bool> {
    let mut redraw = false;
    for (_, event) in glfw::flush_messages(events) {
        match event {
            WindowEvent::Focus(false) => {
                app.ctrl_down = false;
                app.hover_link = None;
                redraw = true;
            }
            WindowEvent::Refresh | WindowEvent::Focus(true) | WindowEvent::Iconify(false) => {
                // The compositor may have discarded the back buffer while the
                // window was hidden or iconified; force a full repaint.
                redraw = true;
            }
            WindowEvent::FramebufferSize(w, h) => {
                // Minimising can report a zero-sized framebuffer on X11; keep
                // the last real size instead of collapsing every pane.
                if w > 0 && h > 0 {
                    renderer.resize(w as u32, h as u32);
                    resize_ptys(app, renderer, ui);
                    app.selection = None;
                    redraw = true;
                }
            }
            WindowEvent::Char(c) => {
                if app.search.is_some() {
                    app.search_input(c);
                } else {
                    input::handle_char(c, app)?;
                }
                redraw = true;
            }
            WindowEvent::Scroll(_, y) => {
                let (mx, my) = window.get_cursor_pos();
                let mx = mx as f32 * ui.scale;
                let my = my as f32 * ui.scale;
                let rects = pane_layout(app, renderer);
                let id = ui::hit_pane(mx, my, &rects).unwrap_or(app.focused);
                let mut reported = false;
                if y != 0.0
                    && pane_mouse_mode(app, id) != MouseMode::Off
                    && let Some((_, rect)) = rects.iter().find(|(pane_id, _)| *pane_id == id)
                {
                    // xterm encodes the wheel as buttons 64 (up) and 65 (down).
                    let code = if y > 0.0 { 64 } else { 65 };
                    reported =
                        send_mouse_report(app, id, *rect, renderer, ui, code, false, mx, my)?;
                }
                if !reported && let Some(pane) = app.pane_mut(id) {
                    pane.active_mut()
                        .pty
                        .screen
                        .lock()
                        .unwrap()
                        .scroll_by_lines(y as f32);
                }
                app.selection = None;
                redraw = true;
                redraw |= update_hover_link(app, renderer, ui, mx, my);
            }
            WindowEvent::CursorPos(x, y) => {
                if let Some(drag) = app.split_drag.clone() {
                    let mx = x as f32 * ui.scale;
                    let my = y as f32 * ui.scale;
                    let ratio = match drag.dir {
                        SplitDir::Horizontal if drag.area.w > 0.0 => {
                            (mx - drag.area.x) / drag.area.w
                        }
                        SplitDir::Vertical if drag.area.h > 0.0 => (my - drag.area.y) / drag.area.h,
                        _ => 0.5,
                    };
                    if let Some(layout) = app.layout.as_mut()
                        && layout.set_ratio(&drag.path, ratio)
                    {
                        app.needs_resize = true;
                        redraw = true;
                    }
                } else if app.selecting
                    && let Some(mut selection) = app.selection
                {
                    let rects = pane_layout(app, renderer);
                    if let Some((_, rect)) = rects.iter().find(|(id, _)| *id == selection.pane)
                        && let Some(pane) = app.pane(selection.pane)
                        && let Some(cell) = cell_at(
                            pane,
                            *rect,
                            ui,
                            renderer.cell_w(),
                            renderer.line_height(),
                            x as f32 * ui.scale,
                            y as f32 * ui.scale,
                        )
                        && cell != selection.cursor
                    {
                        selection.cursor = cell;
                        app.selection = Some(selection);
                        redraw = true;
                    }
                } else if let Some(id) = app.scroll_drag {
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
                                redraw = true;
                            }
                        }
                    }
                } else {
                    // Mouse motion reporting for programs that asked for it.
                    let mx = x as f32 * ui.scale;
                    let my = y as f32 * ui.scale;
                    let rects = pane_layout(app, renderer);
                    if let Some(id) = ui::hit_pane(mx, my, &rects) {
                        let held = app
                            .mouse_pressed
                            .filter(|(pane_id, _)| *pane_id == id)
                            .map(|(_, code)| code);
                        let code = match (pane_mouse_mode(app, id), held) {
                            (MouseMode::Any, Some(code)) => Some(32 + code),
                            (MouseMode::Any, None) => Some(32 + 3),
                            (MouseMode::Button, Some(code)) => Some(32 + code),
                            _ => None,
                        };
                        if let Some(code) = code
                            && let Some((_, rect)) =
                                rects.iter().find(|(pane_id, _)| *pane_id == id)
                        {
                            send_mouse_report(app, id, *rect, renderer, ui, code, false, mx, my)?;
                        }
                    }
                }
                redraw |=
                    update_hover_link(app, renderer, ui, x as f32 * ui.scale, y as f32 * ui.scale);
            }
            WindowEvent::MouseButton(_, glfw::Action::Release, mods) => {
                if app.split_drag.take().is_some() {
                    redraw = true;
                    continue;
                }
                let mut reported = false;
                if let Some((id, code)) = app.mouse_pressed.take() {
                    let mode = pane_mouse_mode(app, id);
                    if mode != MouseMode::Off && mode != MouseMode::X10 {
                        let (mx, my) = window.get_cursor_pos();
                        let mx = mx as f32 * ui.scale;
                        let my = my as f32 * ui.scale;
                        let rects = pane_layout(app, renderer);
                        if let Some((_, rect)) = rects.iter().find(|(pane_id, _)| *pane_id == id) {
                            reported = send_mouse_report(
                                app,
                                id,
                                *rect,
                                renderer,
                                ui,
                                code + mouse_modifier_bits(mods),
                                true,
                                mx,
                                my,
                            )?;
                        }
                    }
                }
                if !reported {
                    app.scroll_drag = None;
                    if app.selecting {
                        app.selecting = false;
                        if let Some(selection) = app.selection
                            && selection.anchor == selection.cursor
                        {
                            app.selection = None;
                            redraw = true;
                        }
                    }
                }
            }
            WindowEvent::MouseButton(button, glfw::Action::Press, mods) => {
                let (mx, my) = window.get_cursor_pos();
                let mx = mx as f32 * ui.scale;
                let my = my as f32 * ui.scale;
                let rects = pane_layout(app, renderer);

                // Ctrl + left drag on a split border resizes the panes.
                if mods.contains(glfw::Modifiers::Control) && button == glfw::MouseButton::Button1 {
                    let hit = app.layout.as_ref().and_then(|layout| {
                        ui::hit_split(
                            layout,
                            Rect::new(0.0, 0.0, renderer.win_w, renderer.win_h),
                            mx,
                            my,
                            4.0 * ui.scale,
                        )
                    });
                    if let Some((path, dir, area)) = hit {
                        app.split_drag = Some(app::SplitDrag { path, dir, area });
                        app.selection = None;
                        app.selecting = false;
                        app.mouse_pressed = None;
                        redraw = true;
                        continue;
                    }
                }

                // Ctrl + left click on a path or URL opens it.
                if mods.contains(glfw::Modifiers::Control)
                    && button == glfw::MouseButton::Button1
                    && let Some(link) = link_at(app, renderer, ui, mx, my)
                {
                    app.focused = link.pane;
                    app.selection = None;
                    app.selecting = false;
                    app.mouse_pressed = None;
                    match &link.target {
                        app::LinkTarget::Url(url) => {
                            if let Err(e) = opener::open_url(url) {
                                eprintln!("voidtty: {e:#}");
                            }
                        }
                        app::LinkTarget::Path(path) => {
                            if let Err(e) = opener::reveal_path(path) {
                                eprintln!("voidtty: {e:#}");
                            }
                        }
                    }
                    app.hover_link = None;
                    redraw = true;
                    continue;
                }

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
                        app.selection = None;
                        grabbed_scrollbar = true;
                        redraw = true;
                        break;
                    }
                }

                if !grabbed_scrollbar {
                    match ui::hit_pane(mx, my, &rects) {
                        Some(id) => {
                            app.focused = id;
                            redraw = true;
                            let mut reported = false;
                            if let Some(code) = mouse_button_code(button)
                                && !mods.contains(glfw::Modifiers::Shift)
                                && let Some((_, rect)) =
                                    rects.iter().find(|(pane_id, _)| *pane_id == id)
                            {
                                let mode = pane_mouse_mode(app, id);
                                if mode != MouseMode::Off {
                                    reported = send_mouse_report(
                                        app,
                                        id,
                                        *rect,
                                        renderer,
                                        ui,
                                        code + mouse_modifier_bits(mods),
                                        false,
                                        mx,
                                        my,
                                    )?;
                                    // X10 reports presses only; remember the button for
                                    // release and drag motion otherwise.
                                    if reported && mode != MouseMode::X10 {
                                        app.mouse_pressed = Some((id, code));
                                    }
                                }
                            }
                            if reported {
                                app.selection = None;
                                app.selecting = false;
                            }
                            if !reported && button == glfw::MouseButton::Button1 {
                                app.selection = None;
                                app.selecting = false;
                                if let Some(rect) = rects
                                    .iter()
                                    .find(|(pane_id, _)| *pane_id == id)
                                    .map(|(_, rect)| *rect)
                                    && let Some(pane) = app.pane(id)
                                {
                                    let content = pane_content(rect, ui.tab_bar_h);
                                    let inside = mx >= content.x
                                        && mx < content.x + content.w
                                        && my >= content.y
                                        && my < content.y + content.h;
                                    if inside
                                        && let Some(cell) = cell_at(
                                            pane,
                                            rect,
                                            ui,
                                            renderer.cell_w(),
                                            renderer.line_height(),
                                            mx,
                                            my,
                                        )
                                    {
                                        let cols = pane.active().pty.screen.lock().unwrap().cols();
                                        let double = app.register_click(id, cell.0);
                                        if double {
                                            app.selection = Some(app::Selection {
                                                pane: id,
                                                anchor: (cell.0, 0),
                                                cursor: (cell.0, cols.saturating_sub(1)),
                                            });
                                        } else {
                                            app.selection = Some(app::Selection {
                                                pane: id,
                                                anchor: cell,
                                                cursor: cell,
                                            });
                                        }
                                        app.selecting = true;
                                    }
                                }
                            }
                        }
                        None => {
                            app.selection = None;
                            redraw = true;
                        }
                    }
                }
            }
            ev => {
                let ctrl_before = app.ctrl_down;
                input::handle_event(ev, app, clipboard)?;
                if app.ctrl_down != ctrl_before {
                    let (x, y) = window.get_cursor_pos();
                    update_hover_link(app, renderer, ui, x as f32 * ui.scale, y as f32 * ui.scale);
                }
                redraw = true;
            }
        }
    }
    if let Some(search) = app.search.as_mut() {
        search.ignore_next_char = false;
    }
    Ok(redraw)
}

/// Computes the current pane rectangles from the split tree.
fn pane_layout(app: &App, renderer: &Renderer) -> Vec<(usize, Rect)> {
    let area = Rect::new(0.0, 0.0, renderer.win_w, renderer.win_h);
    app.layout
        .as_ref()
        .map(|layout| layout_rects(layout, area))
        .unwrap_or_default()
}

/// Maps a window position to a cell in the pane's displayed grid.
fn cell_at(
    pane: &app::Pane,
    rect: Rect,
    ui: UiMetrics,
    cell_w: f32,
    line_h: f32,
    mx: f32,
    my: f32,
) -> Option<(usize, usize)> {
    if cell_w <= 0.0 || line_h <= 0.0 {
        return None;
    }
    let content = pane_content(rect, ui.tab_bar_h);
    let (rows, cols) = {
        let screen = pane.active().pty.screen.lock().unwrap();
        (screen.rows(), screen.cols())
    };
    if rows == 0 || cols == 0 {
        return None;
    }
    let col = ((mx - content.x - ui.pane_pad) / cell_w).floor();
    let row = ((my - content.y - ui.pane_pad) / line_h).floor();
    Some((
        row.clamp(0.0, (rows - 1) as f32) as usize,
        col.clamp(0.0, (cols - 1) as f32) as usize,
    ))
}

/// Updates the hovered link for the current pointer position. Returns whether
/// the hover state changed (so the frame needs repainting).
fn update_hover_link(app: &mut App, renderer: &Renderer, ui: UiMetrics, mx: f32, my: f32) -> bool {
    let next =
        if app.ctrl_down && app.split_drag.is_none() && !app.selecting && app.scroll_drag.is_none()
        {
            link_at(app, renderer, ui, mx, my)
        } else {
            None
        };
    let changed = next != app.hover_link;
    if changed {
        app.hover_link = next;
    }
    changed
}

/// Finds a path or URL token under a window position.
fn link_at(
    app: &App,
    renderer: &Renderer,
    ui: UiMetrics,
    mx: f32,
    my: f32,
) -> Option<app::HoverLink> {
    let rects = pane_layout(app, renderer);
    let id = ui::hit_pane(mx, my, &rects)?;
    let rect = rects
        .iter()
        .find(|(pane_id, _)| *pane_id == id)
        .map(|(_, rect)| *rect)?;
    let pane = app.pane(id)?;
    link_in_pane(
        pane,
        rect,
        ui,
        renderer.cell_w(),
        renderer.line_height(),
        mx,
        my,
    )
}

fn link_in_pane(
    pane: &app::Pane,
    rect: Rect,
    ui: UiMetrics,
    cell_w: f32,
    line_h: f32,
    mx: f32,
    my: f32,
) -> Option<app::HoverLink> {
    let content = pane_content(rect, ui.tab_bar_h);
    let text_x0 = content.x + ui.pane_pad;
    let text_y0 = content.y + ui.pane_pad;
    let text_x1 = content.x + content.w - ui.pane_pad - ui.scrollbar_gutter;
    let text_y1 = content.y + content.h - ui.pane_pad;
    if mx < text_x0 || mx >= text_x1 || my < text_y0 || my >= text_y1 {
        return None;
    }
    let (row, col) = cell_at(pane, rect, ui, cell_w, line_h, mx, my)?;
    if let Some((url, start_col, end_col)) = {
        let screen = pane.active().pty.screen.lock().unwrap();
        screen.link_at(row, col)
    } {
        return Some(app::HoverLink {
            pane: pane.id,
            row,
            start_col,
            end_col,
            target: app::LinkTarget::Url(url),
        });
    }
    let chars: Vec<char> = {
        let screen = pane.active().pty.screen.lock().unwrap();
        screen.display_row(row).iter().map(|cell| cell.ch).collect()
    };

    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        let end = i;
        if col < start || col >= end {
            continue;
        }
        let (target, token_start, token_end) = classify_token(&chars[start..end])?;
        return Some(app::HoverLink {
            pane: pane.id,
            row,
            start_col: start + token_start,
            end_col: start + token_end - 1,
            target,
        });
    }
    None
}

/// Classifies a whitespace-delimited token as a URL or an existing path,
/// trimming surrounding punctuation where needed.
fn classify_token(chars: &[char]) -> Option<(app::LinkTarget, usize, usize)> {
    let mut start = 0;
    let mut end = chars.len();
    while start < end && matches!(chars[start], '(' | '[' | '{' | '<' | '\'' | '"') {
        start += 1;
    }
    loop {
        let token: String = chars[start..end].iter().collect();
        if let Some(target) = classify_text(&token) {
            // URLs are prefix-matched, so strip trailing sentence punctuation
            // before accepting them (paths are already existence-checked).
            let needs_trim =
                matches!(target, app::LinkTarget::Url(_)) && trim_trailing(chars, start, end);
            if !needs_trim {
                return Some((target, start, end));
            }
        }
        if end <= start || !trim_trailing(chars, start, end) {
            break;
        }
        end -= 1;
    }
    None
}

fn classify_text(token: &str) -> Option<app::LinkTarget> {
    if let Some(rest) = token.strip_prefix("file://") {
        let path = expand_home(rest);
        if path.exists() {
            return Some(app::LinkTarget::Path(path));
        }
    }

    let lower = token.to_ascii_lowercase();
    let is_url = lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("ftp://")
        || lower.starts_with("www.")
        || lower.starts_with("mailto:");
    if is_url {
        return Some(app::LinkTarget::Url(token.to_owned()));
    }
    if let Some(pos) = token.find("://") {
        let scheme = &token[..pos];
        if !scheme.is_empty()
            && scheme
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        {
            return Some(app::LinkTarget::Url(token.to_owned()));
        }
    }

    let path = expand_home(token);
    if path.exists() {
        return Some(app::LinkTarget::Path(path));
    }
    None
}

/// Whether the final character can be stripped as sentence punctuation.
fn trim_trailing(chars: &[char], start: usize, end: usize) -> bool {
    let c = chars[end - 1];
    if c == ')' || c == ']' || c == '}' {
        let open = match c {
            ')' => '(',
            ']' => '[',
            _ => '{',
        };
        let opens = chars[start..end].iter().filter(|&&ch| ch == open).count();
        let closes = chars[start..end].iter().filter(|&&ch| ch == c).count();
        return closes > opens;
    }
    matches!(
        c,
        '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' | '`' | '>' | '…'
    )
}

fn expand_home(path: &str) -> PathBuf {
    let home = env::var("HOME").or_else(|_| env::var("USERPROFILE")).ok();
    if let Some(home) = home {
        if path == "~" {
            return PathBuf::from(home);
        }
        if let Some(rest) = path.strip_prefix("~/") {
            return Path::new(&home).join(rest);
        }
    }
    PathBuf::from(path)
}

/// xterm button code for a physical mouse button (wheels use `Scroll` events).
fn mouse_button_code(button: glfw::MouseButton) -> Option<u8> {
    match button {
        glfw::MouseButton::Button1 => Some(0),
        // GLFW numbers right before middle; xterm numbers middle before right.
        glfw::MouseButton::Button2 => Some(2),
        glfw::MouseButton::Button3 => Some(1),
        _ => None,
    }
}

fn mouse_modifier_bits(mods: glfw::Modifiers) -> u8 {
    let mut bits = 0;
    if mods.contains(glfw::Modifiers::Shift) {
        bits |= 4;
    }
    if mods.contains(glfw::Modifiers::Alt) {
        bits |= 8;
    }
    if mods.contains(glfw::Modifiers::Control) {
        bits |= 16;
    }
    bits
}

fn pane_mouse_mode(app: &App, id: usize) -> MouseMode {
    app.pane(id)
        .map(|pane| pane.active().pty.screen.lock().unwrap().mouse_mode())
        .unwrap_or(MouseMode::Off)
}

/// Forwards one mouse event to the program in pane `id`. Returns whether the
/// program accepted it (reporting enabled and the pointer is over its grid).
fn send_mouse_report(
    app: &mut App,
    id: usize,
    rect: Rect,
    renderer: &Renderer,
    ui: UiMetrics,
    code: u8,
    release: bool,
    mx: f32,
    my: f32,
) -> Result<bool> {
    let Some(pane) = app.pane(id) else {
        return Ok(false);
    };
    let content = pane_content(rect, ui.tab_bar_h);
    if mx < content.x
        || mx >= content.x + content.w
        || my < content.y
        || my >= content.y + content.h
    {
        return Ok(false);
    }
    let Some((row, col)) = cell_at(
        pane,
        rect,
        ui,
        renderer.cell_w(),
        renderer.line_height(),
        mx,
        my,
    ) else {
        return Ok(false);
    };
    let bytes = {
        let screen = pane.active().pty.screen.lock().unwrap();
        match screen.mouse_mode() {
            MouseMode::Off => return Ok(false),
            // X10 has no release events.
            MouseMode::X10 if release => return Ok(false),
            _ => {}
        }
        screen.encode_mouse(code, col, row, release)
    };
    if let Some(pane) = app.pane_mut(id) {
        pane.active_mut().pty.write(&bytes)?;
    }
    Ok(true)
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
const SELECTION_FG: (f32, f32, f32) = (0.95, 0.95, 1.0);
const SELECTION_BG: (f32, f32, f32) = (0.20, 0.34, 0.58);

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
    if style.dim {
        fg = (fg.0 * 0.55, fg.1 * 0.55, fg.2 * 0.55);
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

fn draw_plain_line(
    renderer: &mut Renderer,
    text: &str,
    x: f32,
    y: f32,
    max_chars: usize,
    color: (f32, f32, f32),
    alpha: f32,
) -> Result<()> {
    let line: String = text.chars().take(max_chars).collect();
    let colors = vec![color; line.chars().count()];
    renderer.draw_text_styled(&line, &colors, None, None, x, y, alpha)
}

fn draw_search(renderer: &mut Renderer, app: &App, ui: UiMetrics, opacity: f32) -> Result<()> {
    let Some(search) = app.search.as_ref() else {
        return Ok(());
    };
    // Flush the panes first so the overlay is painted strictly on top.
    renderer.flush();
    let cw = renderer.cell_w();
    let lh = renderer.line_height();
    let win_w = renderer.win_w;
    let win_h = renderer.win_h;
    let pad = 14.0 * ui.scale;
    let panel_w = (win_w * 0.7).clamp(320.0, 900.0).min(win_w - 16.0);
    let max_chars = ((panel_w - pad * 2.0) / cw).floor().max(1.0) as usize;
    let max_results = (((win_h * 0.7) / lh).floor() as usize).max(4) - 3;
    let visible = search.hits.len().min(max_results);
    let rows = 3 + visible.max(1);
    let panel_h = (rows as f32 * lh + pad * 2.0).min(win_h - 16.0);
    let x = (win_w - panel_w) * 0.5;
    let y = (win_h - panel_h) * 0.5;
    let tx = x + pad;
    let mut ty = y + pad;

    renderer.draw_rect(0.0, 0.0, win_w, win_h, 0.0, 0.0, 0.0, 0.55 * opacity);
    renderer.draw_round_rect(
        x,
        y,
        panel_w,
        panel_h,
        8.0 * ui.scale,
        0.08,
        0.09,
        0.13,
        0.98 * opacity,
    );

    let header = format!("History  ·  {} result(s)", search.hits.len());
    draw_plain_line(
        renderer,
        &header,
        tx,
        ty,
        max_chars,
        (0.55, 0.68, 0.90),
        opacity,
    )?;
    ty += lh;
    draw_plain_line(
        renderer,
        &format!("> {}", search.query),
        tx,
        ty,
        max_chars,
        (0.95, 0.96, 1.0),
        opacity,
    )?;
    ty += lh * 1.2;

    if search.hits.is_empty() {
        let message = if search.query.is_empty() {
            "Type to search scrollback across all panes"
        } else {
            "No matches"
        };
        draw_plain_line(
            renderer,
            message,
            tx,
            ty,
            max_chars,
            (0.55, 0.55, 0.60),
            opacity,
        )?;
    } else {
        let start = search
            .selected
            .saturating_sub(visible / 2)
            .min(search.hits.len().saturating_sub(visible));
        for (offset, hit) in search.hits.iter().skip(start).take(visible).enumerate() {
            let index = start + offset;
            if index == search.selected {
                renderer.draw_rect(
                    tx - pad * 0.4,
                    ty,
                    panel_w - pad * 1.2,
                    lh,
                    0.20,
                    0.34,
                    0.58,
                    opacity,
                );
            }
            let label = format!("[{}] {}", hit.pane, hit.text);
            let color = if index == search.selected {
                (0.95, 0.96, 1.0)
            } else {
                (0.72, 0.74, 0.80)
            };
            draw_plain_line(renderer, &label, tx, ty, max_chars, color, opacity)?;
            ty += lh;
        }
    }

    Ok(())
}

fn draw_frame(renderer: &mut Renderer, app: &App, ui: UiMetrics, blink_on: bool) -> Result<()> {
    let opacity = app.config.opacity;
    // Black background respecting transparency.
    renderer.clear(0.0, 0.0, 0.0, opacity);
    renderer.begin_frame();

    // Reused row buffers, so drawing does not hold a screen lock.
    let mut rows: Vec<Vec<Cell>> = Vec::new();
    for (id, rect) in pane_layout(app, renderer) {
        let Some(pane) = app.pane(id) else { continue };
        let selection = app.selection.filter(|selection| selection.pane == id);
        let hover = app.hover_link.as_ref().filter(|hover| hover.pane == id);
        draw_pane(
            renderer,
            pane,
            rect,
            id == app.focused,
            ui,
            opacity,
            &app.config,
            &mut rows,
            selection,
            hover,
            blink_on,
        )?;
    }

    draw_search(renderer, app, ui, opacity)?;

    renderer.flush();
    Ok(())
}

fn draw_pane(
    renderer: &mut Renderer,
    pane: &app::Pane,
    rect: Rect,
    focused: bool,
    ui: UiMetrics,
    opacity: f32,
    config: &Config,
    rows: &mut Vec<Vec<Cell>>,
    selection: Option<app::Selection>,
    hover: Option<&app::HoverLink>,
    blink_on: bool,
) -> Result<()> {
    // Pane background.
    renderer.draw_rect(rect.x, rect.y, rect.w, rect.h, 0.0, 0.0, 0.0, opacity);

    // Border highlight for the focused pane.
    if focused {
        let t = config.active_pane_thickness.max(0.0) * ui.scale;
        if t > 0.0 {
            let (r, g, b) = config.active_pane_colour;
            renderer.draw_rect(rect.x, rect.y, rect.w, t, r, g, b, 0.8);
            renderer.draw_rect(rect.x, rect.y, t, rect.h, r, g, b, 0.8);
            renderer.draw_rect(rect.x, rect.y + rect.h - t, rect.w, t, r, g, b, 0.8);
            renderer.draw_rect(rect.x + rect.w - t, rect.y, t, rect.h, r, g, b, 0.8);
        }
    } else {
        let t = ui.scale;
        let c = 0.2f32;
        renderer.draw_rect(rect.x, rect.y, rect.w, t, c, c, c, 0.6);
        renderer.draw_rect(rect.x, rect.y, t, rect.h, c, c, c, 0.6);
    }

    draw_tab_bar(renderer, pane, rect, ui, opacity)?;

    let content = pane_content(rect, ui.tab_bar_h);
    // Copy the visible grid out while briefly holding the lock; the PTY reader
    // thread stays unblocked while text is shaped and uploaded.
    let (
        (cursor_row, cursor_col),
        cursor_visible,
        cursor_style,
        scroll_offset,
        max_scroll,
        row_count,
    ) = {
        let screen = pane.active().pty.screen.lock().unwrap();
        screen.copy_visible(rows);
        (
            screen.cursor(),
            screen.cursor_visible(),
            screen.cursor_style(),
            screen.scroll_offset(),
            screen.max_scroll(),
            screen.rows(),
        )
    };
    let cw = renderer.cell_w();
    let lh = renderer.line_height();
    let cursor_phase_on = !cursor_style.blink || blink_on;
    let show_cursor = focused && scroll_offset == 0 && cursor_visible && cursor_phase_on;

    let mut text = String::new();
    let mut row_colors: Vec<(f32, f32, f32)> = Vec::new();
    let mut row_cells: Vec<usize> = Vec::new();
    let mut row_italic: Vec<bool> = Vec::new();

    for (row, cells) in rows.iter().enumerate() {
        let y = content.y + ui.pane_pad + row as f32 * lh;
        let cursor_here = show_cursor && row == cursor_row;
        let cursor_cell = (cursor_here && cursor_style.shape == CursorShape::Block)
            .then(|| cursor_col.min(cells.len().saturating_sub(1)));

        let mut last = cells
            .iter()
            .rposition(|cell| !cell.is_blank())
            .map(|i| i + 1)
            .unwrap_or(0);
        if let Some(col) = cursor_cell {
            last = last.max(col + 1);
        }
        // Extend the row to cover a selection even across blank cells.
        if let Some(selection) = selection {
            let (start, end) = selection.range();
            if row >= start.0 && row <= end.0 {
                let end_col = if row == end.0 { end.1 + 1 } else { cells.len() };
                last = last.max(end_col.min(cells.len()));
            }
        }
        if let Some(hover) = hover.filter(|hover| hover.row == row) {
            last = last.max((hover.end_col + 1).min(cells.len()));
        }
        if last == 0 {
            continue;
        }

        text.clear();
        row_colors.clear();
        row_colors.reserve(last);
        row_cells.clear();
        row_cells.reserve(last);
        row_italic.clear();
        row_italic.reserve(last);
        // Merge neighbouring cells that share a background into one rectangle.
        let mut run_start: Option<usize> = None;
        let mut run_color = (0.0f32, 0.0f32, 0.0f32);
        let flush_run =
            |renderer: &mut Renderer, start: Option<usize>, end: usize, color: (f32, f32, f32)| {
                if let Some(start) = start {
                    let x = content.x + ui.pane_pad + start as f32 * cw;
                    let w = (end - start) as f32 * cw;
                    renderer.draw_rect(x, y, w, lh, color.0, color.1, color.2, opacity);
                }
            };

        for (col, cell) in cells[..last].iter().enumerate() {
            let style = if Some(col) == cursor_cell {
                Style {
                    fg: Color::Default,
                    bg: Color::Default,
                    reverse: true,
                    ..Style::default()
                }
            } else {
                cell.style
            };
            let selected = selection.is_some_and(|selection| selection.contains(row, col));
            let (fg, bg, fill) = if selected {
                (SELECTION_FG, SELECTION_BG, true)
            } else {
                cell_colors(style)
            };
            if fill {
                if run_start.is_some() && run_color == bg {
                    // The run continues.
                } else {
                    flush_run(renderer, run_start, col, run_color);
                    run_start = Some(col);
                    run_color = bg;
                }
            } else {
                flush_run(renderer, run_start, col, run_color);
                run_start = None;
            }
            let x = content.x + ui.pane_pad + col as f32 * cw;
            let blink_off = style.blink && !blink_on && !selected;
            let hovered = hover.is_some_and(|hover| {
                hover.row == row && col >= hover.start_col && col <= hover.end_col
            });
            if !blink_off {
                if hovered {
                    let thickness = (1.5 * ui.scale).max(1.0);
                    renderer.draw_rect(
                        x,
                        y + lh - thickness,
                        cw,
                        thickness,
                        0.45,
                        0.75,
                        1.0,
                        opacity,
                    );
                } else {
                    if style.underline {
                        let thickness = (1.5 * ui.scale).max(1.0);
                        renderer.draw_rect(
                            x,
                            y + lh - thickness,
                            cw,
                            thickness,
                            fg.0,
                            fg.1,
                            fg.2,
                            opacity,
                        );
                    }
                    if style.strikethrough {
                        let thickness = ui.scale.max(1.0);
                        renderer.draw_rect(
                            x,
                            y + (lh - thickness) * 0.5,
                            cw,
                            thickness,
                            fg.0,
                            fg.1,
                            fg.2,
                            opacity,
                        );
                    }
                }
            }
            if blink_off || cell.wide {
                // Keep one character per cell so shaping stays column-aligned.
                text.push(' ');
                row_cells.push(col);
            } else if crate::renderer::is_block_element(cell.ch) {
                // End the background run so the block paints over its own cell.
                flush_run(renderer, run_start, col, run_color);
                run_start = None;
                renderer.draw_block_element(cell.ch, x, y, fg, opacity);
                // Keep the cell in the row so shaping still aligns to columns.
                text.push(' ');
                row_cells.push(col);
            } else {
                text.push(cell.ch);
                row_cells.push(col);
                if let Some(mark) = cell.combining {
                    text.push(mark);
                    row_cells.push(col);
                }
            }
            row_colors.push(fg);
            row_italic.push(style.italic && !blink_off);
        }
        flush_run(renderer, run_start, last, run_color);

        renderer.draw_text_styled(
            &text,
            &row_colors,
            Some(&row_cells),
            Some(&row_italic),
            content.x + ui.pane_pad,
            y,
            1.0,
        )?;
    }

    if show_cursor && cursor_style.shape != CursorShape::Block {
        let x = content.x + ui.pane_pad + cursor_col as f32 * cw;
        let y = content.y + ui.pane_pad + cursor_row as f32 * lh;
        let (r, g, b) = DEFAULT_FG;
        match cursor_style.shape {
            CursorShape::Bar => {
                let w = (cw * 0.15).max(2.0 * ui.scale);
                renderer.draw_rect(x, y, w, lh, r, g, b, opacity);
            }
            CursorShape::Underline => {
                let h = (lh * 0.1).max(2.0 * ui.scale);
                renderer.draw_rect(x, y + lh - h, cw, h, r, g, b, opacity);
            }
            CursorShape::Block => {}
        }
    }

    // Vertical scrollbar, drawn inside the pane's right padding.
    if let Some(geom) = scrollbar_geometry(&content, ui, max_scroll, row_count) {
        let radius = geom.w * 0.5;
        let frac = scroll_offset as f32 / max_scroll as f32;
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
            None,
            None,
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

fn load_font(config: &Config) -> (font::LoadedFont, font::FallbackFonts) {
    let loaded = if let Ok(path) = std::env::var("voidtty_FONT") {
        if let Ok(data) = std::fs::read(&path) {
            font::LoadedFont {
                data,
                face_index: 0,
                family: path,
            }
        } else {
            fallback_font(config)
        }
    } else {
        fallback_font(config)
    };
    let fallbacks = font::FallbackFonts::new(&loaded.family);
    (loaded, fallbacks)
}

fn fallback_font(config: &Config) -> font::LoadedFont {
    font::load(&config.font_family).unwrap_or_else(|| font::LoadedFont {
        data: FALLBACK_FONT.to_vec(),
        face_index: 0,
        family: "bundled fallback".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(value: &str) -> Option<(app::LinkTarget, usize, usize)> {
        let chars: Vec<char> = value.chars().collect();
        classify_token(&chars)
    }

    #[test]
    fn urls_strip_surrounding_punctuation() {
        let (target, start, end) = classify("(https://example.com).").unwrap();
        assert_eq!(target, app::LinkTarget::Url("https://example.com".into()));
        assert_eq!((start, end), (1, 20));
    }

    #[test]
    fn balanced_parentheses_remain_part_of_urls() {
        let url = "https://en.wikipedia.org/wiki/Foo_(bar)";
        let (target, _, end) = classify(url).unwrap();
        assert_eq!(target, app::LinkTarget::Url(url.into()));
        assert_eq!(end, url.chars().count());
    }

    #[test]
    fn existing_relative_paths_are_recognised() {
        let path = Path::new("Cargo.toml");
        if !path.exists() {
            return;
        }
        let (target, _, _) = classify("Cargo.toml").unwrap();
        assert_eq!(target, app::LinkTarget::Path(path.to_path_buf()));
    }
}
