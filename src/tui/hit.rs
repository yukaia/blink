//! Where things were last drawn: clickable areas, and how big a few
//! scrolling bodies came out.
//!
//! The render functions take `&App` and compute their layouts as they draw,
//! so nothing else knows where a pane or a list ended up. Each records its
//! area here as it draws; a mouse event is then matched against exactly
//! what is on screen. Recomputing the layouts in the mouse handler instead
//! would duplicate every one of them, and a duplicated layout drifts.
//!
//! The same records tell the keyboard how far a page is: `log_body`,
//! `viewer_body` and `help_body` are the rows those bodies showed (see
//! `App::log_rows` and `App::viewer_rows`). `log_body` also gives the
//! width the log wraps at; no click is matched against it, and the wheel
//! over the log is matched against `bottom_pane`.

use std::cell::Cell;

use ratatui::layout::{Position, Rect};

/// One slot per recorded area. Plain data, so [`HitMap::clear`] replaces
/// the lot and can't miss one added later.
#[derive(Debug, Default, Clone, Copy)]
pub struct Areas {
    pub local_pane: Option<Rect>,
    pub remote_pane: Option<Rect>,
    pub local_list: Option<Rect>,
    pub remote_list: Option<Rect>,
    pub transfers_tab: Option<Rect>,
    pub log_tab: Option<Rect>,
    pub transfer_list: Option<Rect>,
    /// The log's lines, when the bottom pane shows the log. Sizes the
    /// keyboard's page and gives the width the log wraps at; no click is
    /// matched against it.
    pub log_body: Option<Rect>,
    /// The whole bottom pane, border included, for focus and the log's
    /// wheel.
    pub bottom_pane: Option<Rect>,
    /// The viewer's text, for the wheel and the keyboard's page.
    pub viewer_body: Option<Rect>,
    /// The scrolling part of the help modal, without its footer, for the
    /// wheel and the keyboard's page.
    pub help_body: Option<Rect>,
    pub session_list: Option<Rect>,
}

/// The [`Areas`] of the last draw, cleared at the start of every draw so a
/// screen that no longer draws an area no longer answers for it.
#[derive(Debug, Default)]
pub struct HitMap(Cell<Areas>);

impl HitMap {
    pub fn clear(&self) {
        self.0.set(Areas::default());
    }

    /// The areas as last drawn.
    pub fn get(&self) -> Areas {
        self.0.get()
    }

    /// Record areas, as a draw does: `hit.set(|a| a.log_body = Some(area))`.
    pub fn set(&self, record: impl FnOnce(&mut Areas)) {
        let mut areas = self.0.get();
        record(&mut areas);
        self.0.set(areas);
    }
}

/// The row of `area` that the cell at (`col`, `row`) is on, counted from the
/// area's top, or `None` when the cell is outside it.
///
/// By [`Rect::contains`], whose edges saturate: adding the width to `x` by
/// hand would overflow for an area reaching the end of `u16`.
pub fn row_in(area: Option<Rect>, col: u16, row: u16) -> Option<usize> {
    let a = area?;
    a.contains(Position::new(col, row))
        .then(|| usize::from(row - a.y))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `clear` used to list every slot by hand, so one missed there would
    /// have kept answering clicks on screens that no longer draw it.
    #[test]
    fn clear_forgets_every_area() {
        let hit = HitMap::default();
        let r = Some(Rect::new(1, 1, 5, 5));
        hit.set(|a| {
            *a = Areas {
                local_pane: r,
                remote_pane: r,
                local_list: r,
                remote_list: r,
                transfers_tab: r,
                log_tab: r,
                transfer_list: r,
                log_body: r,
                bottom_pane: r,
                viewer_body: r,
                help_body: r,
                session_list: r,
            }
        });

        hit.clear();

        assert_eq!(
            format!("{:?}", hit.get()),
            format!("{:?}", Areas::default())
        );
    }

    #[test]
    fn a_cell_is_in_an_area_from_its_top_left_to_before_its_far_edges() {
        let a = Some(Rect::new(2, 3, 4, 2));
        assert_eq!(row_in(a, 2, 3), Some(0));
        assert_eq!(row_in(a, 5, 4), Some(1));
        assert_eq!(row_in(a, 6, 3), None, "right edge is outside");
        assert_eq!(row_in(a, 2, 5), None, "bottom edge is outside");
        assert_eq!(row_in(a, 1, 3), None);
        assert_eq!(row_in(None, 2, 3), None);
    }

    /// An area reaching the end of `u16`: `x + width` overflowed, which a
    /// debug build panics on.
    #[test]
    fn an_area_at_the_end_of_the_coordinate_space_doesnt_overflow() {
        let edge = Rect {
            x: u16::MAX - 1,
            y: u16::MAX - 1,
            width: 10,
            height: 10,
        };
        assert_eq!(row_in(Some(edge), u16::MAX - 1, u16::MAX - 1), Some(0));
    }
}
