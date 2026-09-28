//! Where clickable things were last drawn.
//!
//! The render functions take `&App` and compute their layouts as they draw,
//! so nothing else knows where a pane or a list ended up. Each records its
//! area here as it draws; a mouse event is then matched against exactly
//! what is on screen. Recomputing the layouts in the mouse handler instead
//! would duplicate every one of them, and a duplicated layout drifts.

use std::cell::Cell;

use ratatui::layout::Rect;

/// One slot per clickable area, cleared at the start of every draw so a
/// screen that no longer draws an area no longer answers for it.
#[derive(Debug, Default)]
pub struct HitMap {
    pub local_pane: Cell<Option<Rect>>,
    pub remote_pane: Cell<Option<Rect>>,
    pub local_list: Cell<Option<Rect>>,
    pub remote_list: Cell<Option<Rect>>,
    pub transfers_tab: Cell<Option<Rect>>,
    pub log_tab: Cell<Option<Rect>>,
    pub transfer_list: Cell<Option<Rect>>,
    pub viewer_body: Cell<Option<Rect>>,
    pub session_list: Cell<Option<Rect>>,
}

impl HitMap {
    pub fn clear(&self) {
        for cell in [
            &self.local_pane,
            &self.remote_pane,
            &self.local_list,
            &self.remote_list,
            &self.transfers_tab,
            &self.log_tab,
            &self.transfer_list,
            &self.viewer_body,
            &self.session_list,
        ] {
            cell.set(None);
        }
    }
}

/// The row of `area` that the cell at (`col`, `row`) is on, counted from the
/// area's top, or `None` when the cell is outside it.
pub fn row_in(area: Option<Rect>, col: u16, row: u16) -> Option<usize> {
    let a = area?;
    let inside = col >= a.x && col < a.x + a.width && row >= a.y && row < a.y + a.height;
    inside.then(|| usize::from(row - a.y))
}
