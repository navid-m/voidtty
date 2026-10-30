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

const BASE_TAB_BAR_H: f32 = 28.0;
const BASE_TAB_BAR_PAD: f32 = 10.0;
const BASE_TAB_VPAD: f32 = 4.0;
const BASE_TAB_GAP: f32 = 16.0;
const BASE_PANE_PAD: f32 = 6.0;
const BASE_SCROLLBAR_W: f32 = 6.0;
const BASE_SCROLLBAR_GAP: f32 = 3.0;

/// UI metrics scaled for the display's DPI (Retina, HiDPI, …).
#[derive(Debug, Clone, Copy)]
pub struct UiMetrics {
    pub scale: f32,
    pub tab_bar_h: f32,
    pub tab_bar_pad: f32,
    pub tab_gap: f32,
    pub pane_pad: f32,
    pub scrollbar_w: f32,
    /// Horizontal space reserved to the right of the text for the scrollbar.
    pub scrollbar_gutter: f32,
}

impl UiMetrics {
    pub fn new(scale: f32) -> Self {
        let scale = if scale.is_finite() && scale > 0.0 {
            scale.clamp(1.0, 3.0)
        } else {
            1.0
        };
        Self {
            scale,
            tab_bar_h: BASE_TAB_BAR_H * scale,
            tab_bar_pad: BASE_TAB_BAR_PAD * scale,
            tab_gap: BASE_TAB_GAP * scale,
            pane_pad: BASE_PANE_PAD * scale,
            scrollbar_w: BASE_SCROLLBAR_W * scale,
            scrollbar_gutter: (BASE_SCROLLBAR_W + BASE_SCROLLBAR_GAP) * scale,
        }
    }

    /// Grows the tab bar so tab titles fit the current font.
    pub fn with_tab_bar_height(mut self, line_height: f32) -> Self {
        self.tab_bar_h = self
            .tab_bar_h
            .max(line_height + BASE_TAB_VPAD * self.scale * 2.0);
        self
    }
}

/// Number of text columns that fit in a pane of the given width.
pub fn text_cols(rect_w: f32, cell_w: f32, pane_pad: f32, scrollbar_gutter: f32) -> usize {
    ((rect_w - pane_pad * 2.0 - scrollbar_gutter).max(0.0) / cell_w).floor() as usize
}

/// Number of text rows that fit in a pane of the given height.
pub fn text_rows(rect_h: f32, line_height: f32, pane_pad: f32) -> usize {
    ((rect_h - pane_pad * 2.0).max(0.0) / line_height).floor() as usize
}

/// Splits the content area (below the tab bar) into pane rects for the given
/// number of panes. Supports 1, 2 (horizontal split), or 4 (2×2 grid).
pub fn pane_rects(win_w: f32, win_h: f32, pane_count: usize, tab_bar_h: f32) -> Vec<Rect> {
    let content_y = tab_bar_h;
    let content_h = win_h - tab_bar_h;

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
