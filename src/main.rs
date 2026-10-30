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
use ui::{PANE_PAD, TAB_BAR_H, pane_rects, text_cols, text_rows};

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

    window.make_current();
    window.set_key_polling(true);
    window.set_char_polling(true);
    window.set_framebuffer_size_polling(true);
    window.set_mouse_button_polling(true);
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

    let mut app = App::new(config)?;

    // Size the PTY grids to the actual panes before the first frame.
    resize_ptys(&mut app, &renderer);

    while !window.should_close() && app.running {
        glfw.poll_events();
        process_events(&events, &mut window, &mut app, &mut renderer)?;
        if (renderer.font_size - app.config.font_size).abs() > f32::EPSILON {
            renderer.set_font_size(app.config.font_size)?;
            resize_ptys(&mut app, &renderer);
        }
        draw_frame(&renderer, &app);
        window.swap_buffers();
    }

    Ok(())
}

fn process_events(
    events: &GlfwReceiver<(f64, WindowEvent)>,
    window: &mut glfw::Window,
    app: &mut App,
    renderer: &mut Renderer,
) -> Result<()> {
    for (_, event) in glfw::flush_messages(events) {
        match event {
            WindowEvent::FramebufferSize(w, h) => {
                renderer.resize(w as u32, h as u32);
                resize_ptys(app, renderer);
            }
            WindowEvent::Char(c) => {
                input::handle_char(c, app)?;
            }
            WindowEvent::MouseButton(btn, glfw::Action::Press, _) => {
                let (mx, my) = window.get_cursor_pos();
                let rects =
                    pane_rects(renderer.win_w, renderer.win_h, app.active_tab().panes.len());
                if let Some(idx) = ui::hit_pane(mx as f32, my as f32, &rects) {
                    app.active_tab_mut().focused = idx;
                }
                let _ = btn;
            }
            ev => {
                input::handle_event(ev, app)?;
            }
        }
    }
    Ok(())
}

fn draw_frame(renderer: &Renderer, app: &App) {
    let opacity = app.config.opacity;
    // Dark background respecting transparency.
    renderer.clear(0.07, 0.07, 0.10, opacity);

    let tab = app.active_tab();
    let rects = pane_rects(renderer.win_w, renderer.win_h, tab.panes.len());

    // Tab bar background.
    renderer.draw_rect(
        0.0,
        0.0,
        renderer.win_w,
        TAB_BAR_H,
        0.10,
        0.10,
        0.14,
        opacity,
    );

    // Tab labels.
    let mut tab_x = 4.0f32;
    for (i, t) in app.tabs.iter().enumerate() {
        let active = i == app.active_tab;
        let (r, g, b) = if active {
            (1.0, 0.85, 0.2)
        } else {
            (0.5, 0.5, 0.55)
        };
        if active {
            renderer.draw_rect(
                tab_x - 4.0,
                0.0,
                t.title.len() as f32 * renderer.cell_w() + 16.0,
                TAB_BAR_H,
                0.18,
                0.18,
                0.24,
                opacity,
            );
        }
        renderer.draw_text(
            &t.title,
            tab_x,
            (TAB_BAR_H - renderer.line_height()) * 0.5,
            r,
            g,
            b,
            1.0,
        );
        tab_x += t.title.len() as f32 * renderer.cell_w() + 20.0;
    }

    // Pane borders + content.
    for (i, (pane, rect)) in tab.panes.iter().zip(rects.iter()).enumerate() {
        let focused = i == tab.focused;

        // Pane background.
        renderer.draw_rect(rect.x, rect.y, rect.w, rect.h, 0.05, 0.05, 0.08, opacity);

        // Border highlight for focused pane.
        if focused {
            let t = 1.5f32;
            renderer.draw_rect(rect.x, rect.y, rect.w, t, 0.3, 0.7, 1.0, 0.8);
            renderer.draw_rect(rect.x, rect.y, t, rect.h, 0.3, 0.7, 1.0, 0.8);
            renderer.draw_rect(rect.x, rect.y + rect.h - t, rect.w, t, 0.3, 0.7, 1.0, 0.8);
            renderer.draw_rect(rect.x + rect.w - t, rect.y, t, rect.h, 0.3, 0.7, 1.0, 0.8);
        } else {
            let t = 1.0f32;
            let c = 0.2f32;
            renderer.draw_rect(rect.x, rect.y, rect.w, t, c, c, c, 0.6);
            renderer.draw_rect(rect.x, rect.y, t, rect.h, c, c, c, 0.6);
        }

        // Terminal text.
        let screen = pane.pty.screen.lock().unwrap();
        let lines = screen.visible_lines();

        for (row, line) in lines.iter().enumerate() {
            let y = rect.y + PANE_PAD + row as f32 * renderer.line_height();
            renderer.draw_text(line, rect.x + PANE_PAD, y, 0.9, 0.9, 0.9, 1.0);
        }
    }
}

/// Recalculates PTY cols/rows after a resize event.
fn resize_ptys(app: &mut App, renderer: &Renderer) {
    let cw = renderer.cell_w();
    let lh = renderer.line_height();
    for tab in app.tabs.iter_mut() {
        let rects = pane_rects(renderer.win_w, renderer.win_h, tab.panes.len());
        for (pane, rect) in tab.panes.iter_mut().zip(rects.iter()) {
            let cols = text_cols(rect.w, cw).max(1) as u16;
            let rows = text_rows(rect.h, lh).max(1) as u16;
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
