/// Axis-aligned rectangle in pixel coordinates (top-left origin).
#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self { Self { x, y, w, h } }
}

/// Height of the tab bar in pixels.
pub const TAB_BAR_H: f32 = 28.0;

/// Splits the content area (below the tab bar) into pane rects for the given
/// number of panes. Supports 1, 2 (horizontal split), or 4 (2×2 grid).
pub fn pane_rects(win_w: f32, win_h: f32, pane_count: usize) -> Vec<Rect> {
    let content_y = TAB_BAR_H;
    let content_h = win_h - TAB_BAR_H;

    match pane_count {
        1 => vec![Rect::new(0.0, content_y, win_w, content_h)],
        2 => vec![
            Rect::new(0.0,       content_y, win_w / 2.0, content_h),
            Rect::new(win_w / 2.0, content_y, win_w / 2.0, content_h),
        ],
        3 => vec![
            Rect::new(0.0,       content_y, win_w / 2.0, content_h),
            Rect::new(win_w / 2.0, content_y, win_w / 2.0, content_h / 2.0),
            Rect::new(win_w / 2.0, content_y + content_h / 2.0, win_w / 2.0, content_h / 2.0),
        ],
        _ => {
            let hw = win_w / 2.0;
            let hh = content_h / 2.0;
            vec![
                Rect::new(0.0, content_y,        hw, hh),
                Rect::new(hw,  content_y,        hw, hh),
                Rect::new(0.0, content_y + hh,   hw, hh),
                Rect::new(hw,  content_y + hh,   hw, hh),
            ]
        }
    }
}

/// Given a pixel position, returns the pane index it falls in (if any).
pub fn hit_pane(px: f32, py: f32, rects: &[Rect]) -> Option<usize> {
    rects.iter().position(|r| {
        px >= r.x && px < r.x + r.w && py >= r.y && py < r.y + r.h
    })
}
