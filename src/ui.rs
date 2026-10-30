use crate::app::{LayoutNode, SplitDir};

/// Axis-aligned rectangle in pixel coordinates (top-left origin).
#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
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

/// The terminal content area of a pane, below that pane's own tab bar.
pub fn pane_content(rect: Rect, tab_bar_h: f32) -> Rect {
    let tab_bar_h = tab_bar_h.min(rect.h);
    Rect::new(rect.x, rect.y + tab_bar_h, rect.w, rect.h - tab_bar_h)
}

/// Tiles `area` according to the split tree, returning each pane's id and
/// rectangle (including its own tab bar) in layout order.
pub fn layout_rects(node: &LayoutNode, area: Rect) -> Vec<(usize, Rect)> {
    fn walk(node: &LayoutNode, area: Rect, out: &mut Vec<(usize, Rect)>) {
        match node {
            LayoutNode::Leaf(id) => out.push((*id, area)),
            LayoutNode::Split {
                dir,
                ratio,
                first,
                second,
            } => {
                let ratio = ratio.clamp(0.1, 0.9);
                match dir {
                    SplitDir::Horizontal => {
                        let w = area.w * ratio;
                        walk(first, Rect::new(area.x, area.y, w, area.h), out);
                        walk(
                            second,
                            Rect::new(area.x + w, area.y, area.w - w, area.h),
                            out,
                        );
                    }
                    SplitDir::Vertical => {
                        let h = area.h * ratio;
                        walk(first, Rect::new(area.x, area.y, area.w, h), out);
                        walk(
                            second,
                            Rect::new(area.x, area.y + h, area.w, area.h - h),
                            out,
                        );
                    }
                }
            }
        }
    }

    let mut out = Vec::new();
    walk(node, area, &mut out);
    out
}

/// Given a pixel position, returns the id of the pane it falls in (if any).
pub fn hit_pane(px: f32, py: f32, rects: &[(usize, Rect)]) -> Option<usize> {
    rects
        .iter()
        .find(|(_, r)| px >= r.x && px < r.x + r.w && py >= r.y && py < r.y + r.h)
        .map(|(id, _)| *id)
}
