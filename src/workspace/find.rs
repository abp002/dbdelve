//! Find in results: a strip above the status bar that looks for text in the
//! rows the grid holds, washes every cell it matches and walks them with
//! Enter and Shift-Enter. It searches what was fetched, not the table on the
//! server -- a filter is the way to ask the server.

use gpui::WeakEntity;
use gpui_component::{Sizable, table::TableState};

use super::*;

use crate::result_grid::ResultGrid;

/// Past this many matches the count reads "10,000+" and the scan stops: the
/// number is the point by then, not the cells.
const MAX_MATCHES: usize = 10_000;

pub(crate) struct FindBar {
    input: Entity<InputState>,
    /// The grid this was opened over. Another tab's grid is not searched by a
    /// bar left open from this one.
    target: WeakEntity<TableState<ResultGrid>>,
    matches: Vec<(usize, usize)>,
    /// Which match the ring is on, once Enter has moved it to one.
    current: Option<usize>,
}

impl Workspace {
    pub(crate) fn open_find(
        &mut self,
        _: &FindInResults,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(results) = self
            .profile()
            .and_then(|profile| profile.session.active_results().cloned())
        else {
            return;
        };
        // Pressed again with the bar up: back into its field, text selected,
        // the way every find box behaves.
        if let Some(find) = &self.find
            && find.target == results.downgrade()
        {
            let input = find.input.clone();
            input.update(cx, |input, cx| input.focus(window, cx));
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(tr("Find in results…")));
        cx.subscribe_in(
            &input,
            window,
            |workspace, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => workspace.refind(cx),
                InputEvent::PressEnter { shift, .. } => workspace.step_find(*shift, window, cx),
                _ => {}
            },
        )
        .detach();
        input.update(cx, |input, cx| input.focus(window, cx));
        self.find = Some(FindBar {
            input,
            target: results.downgrade(),
            matches: Vec::new(),
            current: None,
        });
        cx.notify();
    }

    pub(crate) fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(find) = self.find.take() else {
            return false;
        };
        if let Some(results) = find.target.upgrade() {
            results.update(cx, |table, cx| {
                table.delegate_mut().set_found(Vec::new());
                table.focus_handle(cx).focus(window, cx);
                cx.notify();
            });
        }
        cx.notify();
        true
    }

    /// Search again for what the field holds, and wash what matched.
    fn refind(&mut self, cx: &mut Context<Self>) {
        let Some(find) = self.find.as_mut() else {
            return;
        };
        let Some(results) = find.target.upgrade() else {
            return;
        };
        let needle = find.input.read(cx).value().to_string();
        let matches = results.read(cx).delegate().find(&needle, MAX_MATCHES);
        results.update(cx, |table, cx| {
            table.delegate_mut().set_found(matches.clone());
            cx.notify();
        });
        find.matches = matches;
        find.current = None;
        cx.notify();
    }

    /// Move the ring to the next match, or the previous one when `back`, and
    /// bring it into view. Wraps at either end.
    fn step_find(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
        // A run since the last keystroke replaced the rows, and the cells
        // found were in those.
        self.refind(cx);
        let Some(find) = self.find.as_mut() else {
            return;
        };
        let count = find.matches.len();
        if count == 0 {
            return;
        }
        let next = match (find.current, back) {
            (None, false) => 0,
            (None, true) => count - 1,
            (Some(at), false) => (at + 1) % count,
            (Some(at), true) => (at + count - 1) % count,
        };
        find.current = Some(next);
        let (row, col) = find.matches[next];
        let input = find.input.clone();
        if let Some(results) = find.target.upgrade() {
            results.update(cx, |table, cx| {
                table.delegate_mut().activate(row, col);
                table.set_selected_row(row, cx);
                if let Some(col) = table.delegate().table_col(col) {
                    table.scroll_to_col(col, cx);
                }
                cx.notify();
            });
        }
        // Selecting the row took focus into the grid; typing goes on here.
        input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    pub(crate) fn render_find_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let find = self.find.as_ref()?;
        let active = self
            .profile()
            .and_then(|profile| profile.session.active_results())?;
        if find.target != active.downgrade() {
            return None;
        }
        let t = *theme(cx);
        let count = find.matches.len();
        let readout = match (count, find.current) {
            (0, _) if find.input.read(cx).value().is_empty() => String::new(),
            (0, _) => tr("No matches").to_string(),
            (MAX_MATCHES, _) => trf!("{}+ matches", group_thousands(MAX_MATCHES as u64)),
            (_, Some(at)) => trf!("{} of {}", at + 1, group_thousands(count as u64)),
            (_, None) => trf!("{} matches", group_thousands(count as u64)),
        };
        Some(
            div()
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap(px(layout::SPACE_SM))
                .h(px(layout::chrome(layout::STATUS_HEIGHT)))
                .px(px(layout::SPACE_MD))
                .border_t_1()
                .border_color(t.border)
                .bg(t.surface)
                .text_size(px(layout::chrome(layout::TEXT_SM)))
                .child(
                    icon(icon::SEARCH)
                        .size(px(layout::chrome(layout::ICON_SIZE)))
                        .text_color(t.text_muted),
                )
                .child(div().w(px(260.)).child(Input::new(&find.input).small()))
                .child(div().min_w(px(90.)).text_color(t.text_muted).child(readout))
                .child(
                    icon_button(
                        "find-previous",
                        icon::CHEVRON_LEFT,
                        Tone::Quiet,
                        Control::Compact,
                        t,
                    )
                    .tooltip(tr("Previous match (Shift+Enter)"))
                    .on_click(cx.listener(
                        |workspace, _: &ClickEvent, window, cx| {
                            workspace.step_find(true, window, cx)
                        },
                    )),
                )
                .child(
                    icon_button(
                        "find-next",
                        icon::CHEVRON_RIGHT,
                        Tone::Quiet,
                        Control::Compact,
                        t,
                    )
                    .tooltip(tr("Next match (Enter)"))
                    .on_click(cx.listener(
                        |workspace, _: &ClickEvent, window, cx| {
                            workspace.step_find(false, window, cx)
                        },
                    )),
                )
                .child(div().flex_1())
                .child(
                    icon_button("find-close", icon::CLOSE, Tone::Quiet, Control::Compact, t)
                        .tooltip(tr("Close (Esc)"))
                        .on_click(cx.listener(|workspace, _: &ClickEvent, window, cx| {
                            workspace.close_find(window, cx);
                        })),
                )
                .into_any_element(),
        )
    }
}
