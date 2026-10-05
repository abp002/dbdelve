//! The schema diagram: a full-window sheet over the workspace, like Settings,
//! rather than a kind of tab. Nothing about it is a statement or a result, so
//! it has no business in the tab machinery that exists to hold those, and it
//! is not restored with the session — it is a look at the schema, taken again
//! when wanted.
//!
//! Dragging the background pans, dragging a box's header moves the box,
//! scrolling pans, and secondary-scroll or a trackpad pinch zooms. It opens
//! fitted to the window. A click on a header that did not move opens that
//! table the way the explorer does.

use std::{cell::Cell, rc::Rc};

use gpui::{
    BorderStyle, Bounds, Hsla, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PathBuilder, PinchEvent, Pixels, Point, ScrollWheelEvent, canvas,
};

use super::*;

use crate::diagram::{self, BOX_WIDTH, Diagram, HEADER_HEIGHT, ROW_HEIGHT};

/// Low enough that fitting a hundred tables still fits them: past reading,
/// but the shape of the schema is what that view is for.
const ZOOM_MIN: f32 = 0.1;
const ZOOM_MAX: f32 = 2.0;
/// The pointer travel, in window pixels, past which a press on a header is a
/// drag of the box rather than a click to open it.
const CLICK_SLOP: f32 = 4.0;
/// The room left round the diagram when it is fitted to the window.
const FIT_MARGIN: f32 = 40.0;

pub(crate) struct DiagramView {
    pub(crate) schema: String,
    state: DiagramState,
    /// Where diagram point (0, 0) sits on the surface, in window pixels.
    pan: Point<f32>,
    zoom: f32,
    gesture: Option<Gesture>,
    /// The box under the pointer, whose lines are drawn in the accent.
    hovered: Option<usize>,
    /// Which load this view is waiting on, so a reopen for another schema
    /// while one was in flight cannot be overwritten by the slower answer.
    request: u64,
    /// The surface's bounds as last laid out, in window pixels. Written at
    /// prepaint, read by fitting and by zooming around a point, which are
    /// the two things that need to know how big the canvas is.
    surface: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// The Mermaid button just put the diagram on the clipboard, and says so
    /// until the next gesture.
    copied: bool,
}

impl DiagramView {
    /// Zoom and pan so the whole diagram is in view, never past 100%: a
    /// three-table schema blown up to fill a window reads as a mistake.
    fn fit(&mut self) {
        let (Some(bounds), DiagramState::Loaded(diagram)) = (self.surface.get(), &self.state)
        else {
            return;
        };
        let Some((left, top, right, bottom)) = diagram.tables.iter().fold(None, |extent, table| {
            let (x0, y0, x1, y1) = (
                table.x,
                table.y,
                table.x + BOX_WIDTH,
                table.y + table.height(),
            );
            Some(match extent {
                None => (x0, y0, x1, y1),
                Some((l, t, r, b)) => (
                    f32::min(l, x0),
                    f32::min(t, y0),
                    f32::max(r, x1),
                    f32::max(b, y1),
                ),
            })
        }) else {
            return;
        };
        let (width, height) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let zoom = f32::min(
            (width - 2.0 * FIT_MARGIN) / (right - left),
            (height - 2.0 * FIT_MARGIN) / (bottom - top),
        )
        .clamp(ZOOM_MIN, 1.0);
        self.zoom = zoom;
        self.pan = Point::new(
            width / 2.0 - (left + right) / 2.0 * zoom,
            height / 2.0 - (top + bottom) / 2.0 * zoom,
        );
    }

    /// A window position as a point on the surface.
    fn on_surface(&self, position: Point<Pixels>) -> Point<f32> {
        let origin = self
            .surface
            .get()
            .map_or(Point::default(), |bounds| bounds.origin);
        Point::new(
            f32::from(position.x - origin.x),
            f32::from(position.y - origin.y),
        )
    }

    /// The middle of the surface, which the zoom buttons zoom around.
    fn middle(&self) -> Point<f32> {
        self.surface.get().map_or(Point::default(), |bounds| {
            Point::new(
                f32::from(bounds.size.width) / 2.0,
                f32::from(bounds.size.height) / 2.0,
            )
        })
    }
}

/// One line's geometry, worked out at render so the paint closure owns plain
/// numbers and not the diagram.
#[derive(Clone, Copy)]
struct Line {
    /// On the referencing box's edge.
    start: Point<f32>,
    /// On the referenced box's edge.
    end: Point<f32>,
    /// Which way the line leaves each end: +1 rightward, -1 leftward.
    start_way: f32,
    end_way: f32,
    /// Half the height of each end's marks, smaller where ends share a row.
    start_half: f32,
    end_half: f32,
    highlighted: bool,
    one_to_one: bool,
    optional: bool,
}

enum DiagramState {
    Loading,
    Failed(String),
    Loaded(Diagram),
}

#[derive(Clone, Copy)]
enum Gesture {
    Pan {
        last: Point<Pixels>,
    },
    Move {
        table: usize,
        start: Point<Pixels>,
        last: Point<Pixels>,
        moved: bool,
    },
}

impl Workspace {
    pub(crate) fn open_diagram(
        &mut self,
        schema: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(names) = self.catalog().and_then(|catalog| {
            let schema = catalog.schemas.iter().find(|s| s.name == schema)?;
            Some(diagram_tables(schema))
        }) else {
            return;
        };
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(connection) = profile.connection() else {
            return;
        };
        let profile_id = profile.id.clone();
        let generation = profile.generation;
        let request = self.diagram.as_ref().map_or(1, |view| view.request + 1);
        self.diagram = Some(DiagramView {
            schema: schema.clone(),
            state: DiagramState::Loading,
            pan: Point::new(40.0, 40.0),
            zoom: 1.0,
            gesture: None,
            hovered: None,
            request,
            surface: self
                .diagram
                .as_ref()
                .map_or_else(Rc::default, |view| view.surface.clone()),
            copied: false,
        });
        // Out of whatever pane had the keyboard, so `escape` reaches the
        // workspace's dismiss and not an editor that would keep it.
        window.focus(&self.focus, cx);
        cx.notify();

        let task = cx
            .background_executor()
            .spawn(async move { diagram::load(&connection, &schema, names) });
        cx.spawn(async move |workspace, cx| {
            let result = task.await;
            workspace
                .update(cx, |workspace, cx| {
                    if workspace.issued_to(&profile_id, generation).is_none() {
                        return;
                    }
                    let Some(view) = workspace.diagram.as_mut() else {
                        return;
                    };
                    if view.request != request {
                        return;
                    }
                    view.state = match result {
                        Ok(diagram) => DiagramState::Loaded(diagram),
                        Err(error) => DiagramState::Failed(error.to_string()),
                    };
                    view.fit();
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    pub(crate) fn close_diagram(&mut self, cx: &mut Context<Self>) -> bool {
        if self.diagram.take().is_none() {
            return false;
        }
        self.refocus_front();
        cx.notify();
        true
    }

    fn diagram_open_table(&mut self, table: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = &self.diagram else {
            return;
        };
        let DiagramState::Loaded(diagram) = &view.state else {
            return;
        };
        let schema = view.schema.clone();
        let name = diagram.tables[table].name.clone();
        let kind = self
            .catalog()
            .and_then(|catalog| catalog_relation(catalog, &schema, &name))
            .map(|relation| relation.kind)
            .unwrap_or_default();
        self.diagram = None;
        let opened = OpenedObject::Relation {
            schema,
            name,
            kind,
            filter: String::new(),
            filters: Vec::new(),
        };
        if let Some(id) = self.open_object(opened, window, cx) {
            self.activate_tab(Tab::Object(id), window, cx);
            self.remember_profiles(cx);
        }
        cx.notify();
    }

    fn diagram_zoom_by(&mut self, factor: f32, around: Point<f32>, cx: &mut Context<Self>) {
        let Some(view) = self.diagram.as_mut() else {
            return;
        };
        let zoom = (view.zoom * factor).clamp(ZOOM_MIN, ZOOM_MAX);
        // Keep the diagram point under `around` where it is.
        let ratio = zoom / view.zoom;
        view.pan = Point::new(
            around.x - (around.x - view.pan.x) * ratio,
            around.y - (around.y - view.pan.y) * ratio,
        );
        view.zoom = zoom;
        cx.notify();
    }

    fn diagram_fit(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = self.diagram.as_mut() {
            view.fit();
            cx.notify();
        }
    }

    pub(crate) fn render_diagram(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let view = self.diagram.as_ref()?;
        let t = *theme(cx);

        let summary = match &view.state {
            DiagramState::Loading => tr("Reading the schema…").to_string(),
            DiagramState::Failed(_) => String::new(),
            DiagramState::Loaded(diagram) => {
                let mut parts = vec![trf!(
                    "{} tables, {} relationships",
                    diagram.tables.len(),
                    diagram.links.len()
                )];
                if diagram.left_out > 0 {
                    parts.push(trf!("{} not drawn", diagram.left_out));
                }
                if diagram.unreadable > 0 {
                    parts.push(trf!("{} unreadable", diagram.unreadable));
                }
                parts.join(" · ")
            }
        };

        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(layout::SPACE_MD))
            .h(px(layout::TITLEBAR_HEIGHT))
            // The sheet covers the titlebar, so it clears the window's own
            // buttons the way the titlebar does.
            .pl(px(layout::TITLEBAR_LEADING_INSET))
            .pr(px(layout::SPACE_LG))
            .bg(t.surface)
            .border_b_1()
            .border_color(t.border)
            .child(row_icon(t, icon::PLAN))
            .child(
                div()
                    .text_color(t.text)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(trf!("Diagram of {}", view.schema)),
            )
            .child(div().flex_1().text_color(t.text_muted).child(summary))
            .child(
                div()
                    .text_color(t.text_faint)
                    .text_size(px(layout::chrome(layout::TEXT_XS)))
                    .child(tr("Drag to move · scroll to pan · ⌘ + scroll or pinch to zoom · click a table to open it")),
            )
            .child(
                button(
                    "diagram-zoom-out",
                    "−",
                    Tone::Quiet,
                    Control::Compact,
                    t,
                )
                .on_click(cx.listener(|workspace, _: &ClickEvent, _, cx| {
                    let middle = workspace.diagram.as_ref().map(DiagramView::middle);
                    workspace.diagram_zoom_by(1.0 / 1.25, middle.unwrap_or_default(), cx);
                })),
            )
            .child(
                div()
                    .w(px(44.0))
                    .text_center()
                    .text_color(t.text_muted)
                    .child(format!("{:.0}%", view.zoom * 100.0)),
            )
            .child(
                button("diagram-zoom-in", "+", Tone::Quiet, Control::Compact, t).on_click(
                    cx.listener(|workspace, _: &ClickEvent, _, cx| {
                        let middle = workspace.diagram.as_ref().map(DiagramView::middle);
                        workspace.diagram_zoom_by(1.25, middle.unwrap_or_default(), cx);
                    }),
                ),
            )
            .child(
                button("diagram-fit", tr("Fit"), Tone::Quiet, Control::Compact, t).on_click(
                    cx.listener(|workspace, _: &ClickEvent, _, cx| workspace.diagram_fit(cx)),
                ),
            )
            .child(
                button(
                    "diagram-mermaid",
                    match view.copied {
                        true => tr("Copied"),
                        false => tr("Copy as Mermaid"),
                    },
                    Tone::Quiet,
                    Control::Compact,
                    t,
                )
                .on_click(cx.listener(|workspace, _: &ClickEvent, _, cx| {
                    let Some(view) = workspace.diagram.as_mut() else {
                        return;
                    };
                    let DiagramState::Loaded(diagram) = &view.state else {
                        return;
                    };
                    cx.write_to_clipboard(ClipboardItem::new_string(diagram.mermaid()));
                    view.copied = true;
                    cx.notify();
                })),
            )
            .child(
                button("diagram-close", tr("Close"), Tone::Primary, Control::Compact, t).on_click(
                    cx.listener(|workspace, _: &ClickEvent, _, cx| {
                        workspace.close_diagram(cx);
                    }),
                ),
            );

        // Measured in every state, so the size is known by the time a load
        // lands and the diagram can open fitted.
        let measured = view.surface.clone();
        let measure = canvas(
            move |bounds, _, _| measured.set(Some(bounds)),
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0();
        let content = match &view.state {
            DiagramState::Loading => centered(t, tr("Reading the schema…").to_string()),
            DiagramState::Failed(error) => centered(t, error.clone()),
            DiagramState::Loaded(diagram) if diagram.tables.is_empty() => {
                centered(t, tr("This schema has no tables.").to_string())
            }
            DiagramState::Loaded(diagram) => self.render_diagram_surface(view, diagram, t, cx),
        };

        Some(
            div()
                .id("diagram-sheet")
                .absolute()
                .inset_0()
                // The mouse as well as the keyboard, as Settings does: nothing
                // under the sheet should take a click meant for it.
                .occlude()
                .flex()
                .flex_col()
                .bg(t.bg)
                .text_size(px(layout::chrome(layout::TEXT_MD)))
                .child(header)
                .child(
                    div()
                        .relative()
                        .flex_1()
                        .min_h_0()
                        .child(measure)
                        .child(content),
                )
                .into_any_element(),
        )
    }

    fn render_diagram_surface(
        &self,
        view: &DiagramView,
        diagram: &Diagram,
        t: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (zoom, pan) = (view.zoom, view.pan);
        let place = |x: f32, y: f32| Point::new(pan.x + x * zoom, pan.y + y * zoom);

        // Each end in diagram units: (x, y, way) for the start and the end.
        let ends: Vec<(End, End)> = diagram
            .links
            .iter()
            .map(|link| {
                let (from, to) = (&diagram.tables[link.from], &diagram.tables[link.to]);
                let from_y = from.row_middle(link.from_row);
                let to_y = to.row_middle(link.to_row);
                let (from_x, to_x, start_way, end_way) = if link.from == link.to {
                    // A table pointing at itself: a loop off its right edge.
                    (from.x + BOX_WIDTH, from.x + BOX_WIDTH, 1.0, 1.0)
                } else if from.x + BOX_WIDTH / 2.0 >= to.x + BOX_WIDTH / 2.0 {
                    // Leave from whichever side faces the other box.
                    (from.x, to.x + BOX_WIDTH, -1.0, 1.0)
                } else {
                    (from.x + BOX_WIDTH, to.x, 1.0, -1.0)
                };
                ((from_x, from_y, start_way), (to_x, to_y, end_way))
            })
            .collect();
        let spread = spread_shared_ends(&ends);
        let lines: Vec<Line> = diagram
            .links
            .iter()
            .zip(&ends)
            .zip(spread)
            .map(
                |((link, &(start, end)), (start_shift, end_shift, start_half, end_half))| Line {
                    start: place(start.0, start.1 + start_shift),
                    end: place(end.0, end.1 + end_shift),
                    start_way: start.2,
                    end_way: end.2,
                    start_half: start_half * zoom,
                    end_half: end_half * zoom,
                    highlighted: view.hovered.is_some_and(|h| h == link.from || h == link.to),
                    one_to_one: link.one_to_one,
                    optional: link.optional,
                },
            )
            .collect();
        let (line, accent, paper) = (
            Hsla::from(t.border_strong),
            Hsla::from(t.accent),
            Hsla::from(t.bg),
        );
        let stroke = (1.5 * zoom).max(1.0);

        let links = canvas(
            |_, _, _| {},
            move |bounds: Bounds<Pixels>, _, window, _| {
                let origin = bounds.origin;
                let at = |x: f32, y: f32| origin + point(px(x), px(y));
                let segment = |window: &mut Window, a: (f32, f32), b: (f32, f32), color: Hsla| {
                    let mut path = PathBuilder::stroke(px(stroke));
                    path.move_to(at(a.0, a.1));
                    path.line_to(at(b.0, b.1));
                    if let Ok(path) = path.build() {
                        window.paint_path(path, color);
                    }
                };
                // Crow's-foot ends. The curve leaves and meets each box level,
                // so every mark is drawn square to a horizontal line.
                let (near, far) = (8.0 * zoom, 14.0 * zoom);
                // The accented ones last, so they lie on top.
                for pass in [false, true] {
                    for l in lines.iter().filter(|l| l.highlighted == pass) {
                        let color = if l.highlighted { accent } else { line };
                        let (start, end) = (l.start, l.end);
                        let reach = match start.x == end.x {
                            true => 60.0 * zoom,
                            false => ((end.x - start.x).abs() / 2.0).max(40.0 * zoom),
                        };
                        let mut path = PathBuilder::stroke(px(stroke));
                        path.move_to(at(start.x, start.y));
                        path.cubic_bezier_to(
                            at(end.x, end.y),
                            at(start.x + l.start_way * reach, start.y),
                            at(end.x + l.end_way * reach, end.y),
                        );
                        if let Ok(path) = path.build() {
                            window.paint_path(path, color);
                        }

                        // The referenced end: exactly one row, or none when
                        // the key takes NULL -- a bar, then a bar or a ring.
                        let bar = |window: &mut Window, x: f32, y: f32, half: f32| {
                            segment(window, (x, y - half), (x, y + half), color)
                        };
                        bar(window, end.x + l.end_way * near, end.y, l.end_half);
                        if l.optional {
                            let (radius, centre) =
                                (l.end_half.min(3.5 * zoom), end.x + l.end_way * far);
                            window.paint_quad(gpui::quad(
                                Bounds::new(
                                    at(centre - radius, end.y - radius),
                                    gpui::size(px(2.0 * radius), px(2.0 * radius)),
                                ),
                                px(radius),
                                paper,
                                px(stroke),
                                color,
                                BorderStyle::Solid,
                            ));
                        } else {
                            bar(window, end.x + l.end_way * far, end.y, l.end_half);
                        }

                        // The referencing end: one row when the key is unique
                        // by itself, else many -- the crow's foot.
                        if l.one_to_one {
                            bar(window, start.x + l.start_way * near, start.y, l.start_half);
                        } else {
                            let apex = (start.x + l.start_way * far, start.y);
                            for spread in [-l.start_half, 0.0, l.start_half] {
                                segment(window, apex, (start.x, start.y + spread), color);
                            }
                        }
                    }
                }
            },
        )
        .absolute()
        .inset_0();

        let text = |size: f32| px(size * zoom);
        let catalog = self.catalog();
        let boxes = diagram.tables.iter().enumerate().map(|(index, table)| {
            let origin = place(table.x, table.y);
            let hovered = view.hovered == Some(index);
            let rows =
                table.shown().iter().map(|field| {
                    let mark = match (field.primary, field.foreign) {
                        (true, _) => Some((icon::PRIMARY_KEY, t.accent)),
                        (false, true) => Some((icon::FOLLOW_KEY, t.text_muted)),
                        _ => None,
                    };
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0 * zoom))
                        .h(px(ROW_HEIGHT * zoom))
                        .px(px(8.0 * zoom))
                        .child(div().flex_none().w(px(12.0 * zoom)).children(mark.map(
                            |(path, color)| icon(path).size(px(11.0 * zoom)).text_color(color),
                        )))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_color(t.text)
                                .when(field.primary, |name| name.font_weight(FontWeight::SEMIBOLD))
                                .child(field.name.clone()),
                        )
                        .child(
                            div()
                                .flex_none()
                                .max_w(px(100.0 * zoom))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_color(t.text_faint)
                                .child(match field.nullable {
                                    true => format!("{}?", field.data_type),
                                    false => field.data_type.clone(),
                                }),
                        )
                });
            let more = (table.hidden() > 0).then(|| {
                div()
                    .h(px(ROW_HEIGHT * zoom))
                    .px(px(26.0 * zoom))
                    .flex()
                    .items_center()
                    .text_color(t.text_faint)
                    .child(trf!("… {} more", table.hidden()))
            });

            div()
                .id(("diagram-table", index))
                .absolute()
                .left(px(origin.x))
                .top(px(origin.y))
                .w(px(BOX_WIDTH * zoom))
                .text_size(text(layout::TEXT_SM))
                .bg(t.panel)
                .border_1()
                .border_color(if hovered {
                    t.accent.into()
                } else {
                    Hsla::from(t.border)
                })
                .rounded(px(layout::RADIUS_CONTROL * zoom))
                .overflow_hidden()
                .on_hover(cx.listener(move |workspace, hovering: &bool, _, cx| {
                    let Some(view) = workspace.diagram.as_mut() else {
                        return;
                    };
                    match (*hovering, view.hovered == Some(index)) {
                        (true, _) => view.hovered = Some(index),
                        (false, true) => view.hovered = None,
                        (false, false) => return,
                    }
                    cx.notify();
                }))
                .child(
                    div()
                        .id(("diagram-header", index))
                        .flex()
                        .items_center()
                        .gap(px(6.0 * zoom))
                        .h(px(HEADER_HEIGHT * zoom))
                        .px(px(8.0 * zoom))
                        .bg(t.surface)
                        .border_b_1()
                        .border_color(t.border)
                        .cursor_grab()
                        .child(
                            icon(icon::TABLE)
                                .size(px(12.0 * zoom))
                                .text_color(t.text_muted),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(t.text)
                                .child(table.name.clone()),
                        )
                        .children(statistics(catalog, &view.schema, &table.name).map(|label| {
                            div()
                                .flex_none()
                                .whitespace_nowrap()
                                .text_size(px(layout::TEXT_XS * zoom))
                                .font_weight(FontWeight::NORMAL)
                                .text_color(t.text_faint)
                                .child(label)
                        }))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |workspace, event: &MouseDownEvent, _, cx| {
                                if let Some(view) = workspace.diagram.as_mut() {
                                    view.gesture = Some(Gesture::Move {
                                        table: index,
                                        start: event.position,
                                        last: event.position,
                                        moved: false,
                                    });
                                }
                                // Not a pan of the surface underneath as well.
                                cx.stop_propagation();
                            }),
                        ),
                )
                .children(rows)
                .children(more)
        });

        div()
            .id("diagram-surface")
            .absolute()
            .inset_0()
            .overflow_hidden()
            .cursor(match view.gesture {
                Some(Gesture::Pan { .. }) | Some(Gesture::Move { .. }) => {
                    gpui::CursorStyle::ClosedHand
                }
                None => gpui::CursorStyle::Arrow,
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|workspace, event: &MouseDownEvent, _, _| {
                    if let Some(view) = workspace.diagram.as_mut() {
                        view.gesture = Some(Gesture::Pan {
                            last: event.position,
                        });
                        view.copied = false;
                    }
                }),
            )
            .on_mouse_move(cx.listener(|workspace, event: &MouseMoveEvent, _, cx| {
                let Some(view) = workspace.diagram.as_mut() else {
                    return;
                };
                let Some(gesture) = view.gesture.as_mut() else {
                    return;
                };
                // A button let go outside the window never sent its up.
                if event.pressed_button != Some(MouseButton::Left) {
                    view.gesture = None;
                    cx.notify();
                    return;
                }
                let zoom = view.zoom;
                match gesture {
                    Gesture::Pan { last } => {
                        let delta = event.position - *last;
                        *last = event.position;
                        view.pan.x += f32::from(delta.x);
                        view.pan.y += f32::from(delta.y);
                    }
                    Gesture::Move {
                        table,
                        start,
                        last,
                        moved,
                    } => {
                        let travel = event.position - *start;
                        if !*moved
                            && f32::from(travel.x).abs() < CLICK_SLOP
                            && f32::from(travel.y).abs() < CLICK_SLOP
                        {
                            return;
                        }
                        *moved = true;
                        let delta = event.position - *last;
                        *last = event.position;
                        let table = *table;
                        if let DiagramState::Loaded(diagram) = &mut view.state {
                            diagram.tables[table].x += f32::from(delta.x) / zoom;
                            diagram.tables[table].y += f32::from(delta.y) / zoom;
                        }
                    }
                }
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|workspace, _: &MouseUpEvent, window, cx| {
                    let Some(gesture) = workspace.diagram.as_mut().and_then(|v| v.gesture.take())
                    else {
                        return;
                    };
                    if let Gesture::Move {
                        table,
                        moved: false,
                        ..
                    } = gesture
                    {
                        workspace.diagram_open_table(table, window, cx);
                    }
                    cx.notify();
                }),
            )
            .on_scroll_wheel(
                cx.listener(|workspace, event: &ScrollWheelEvent, window, cx| {
                    let delta = event.delta.pixel_delta(window.line_height());
                    if event.modifiers.secondary() {
                        let factor = (1.0 + f32::from(delta.y) / 300.0).clamp(0.8, 1.25);
                        // Around the pointer, so what is under it stays there.
                        let Some(around) = workspace
                            .diagram
                            .as_ref()
                            .map(|view| view.on_surface(event.position))
                        else {
                            return;
                        };
                        workspace.diagram_zoom_by(factor, around, cx);
                        return;
                    }
                    if let Some(view) = workspace.diagram.as_mut() {
                        view.pan.x += f32::from(delta.x);
                        view.pan.y += f32::from(delta.y);
                        cx.notify();
                    }
                }),
            )
            .on_pinch(cx.listener(|workspace, event: &PinchEvent, _, cx| {
                let Some(around) = workspace
                    .diagram
                    .as_ref()
                    .map(|view| view.on_surface(event.position))
                else {
                    return;
                };
                workspace.diagram_zoom_by(1.0 + event.delta, around, cx);
            }))
            .child(links)
            .children(boxes)
            .into_any_element()
    }
}

/// One end of a line in diagram units: x, y, and which way it leaves (±1).
type End = (f32, f32, f32);
/// The start's and the end's vertical shift, then their mark half-heights.
type Spread = (f32, f32, f32, f32);
type Member = (usize, bool, f32);

/// Lines that land on the same row on the same side would draw their marks
/// on top of each other, into a glyph that means nothing. Fan them out down
/// the row instead, in the order of where their other ends are so they do not
/// cross on the way in, and shrink their marks to the room each gets.
///
/// Per line: the start's and the end's vertical shift, then the start's and
/// the end's mark half-height, all in diagram units.
fn spread_shared_ends(ends: &[(End, End)]) -> Vec<Spread> {
    const HALF: f32 = 6.0;
    let mut out = vec![(0.0, 0.0, HALF, HALF); ends.len()];
    // Keyed by the point and the way out: the same row, the same edge. Each
    // member is a line, whether this is its start, and its other end's y.
    let mut groups: HashMap<(i64, i64, i64), Vec<Member>> = HashMap::new();
    for (index, &(start, end)) in ends.iter().enumerate() {
        let key = |p: (f32, f32, f32)| (p.0.round() as i64, p.1.round() as i64, p.2 as i64);
        groups
            .entry(key(start))
            .or_default()
            .push((index, true, end.1));
        groups
            .entry(key(end))
            .or_default()
            .push((index, false, start.1));
    }
    for mut members in groups.into_values().filter(|members| members.len() > 1) {
        members.sort_by(|a, b| a.2.total_cmp(&b.2));
        let span = ROW_HEIGHT - 4.0;
        let step = span / members.len() as f32;
        let half = (step / 2.0 - 0.5).clamp(1.5, HALF);
        for (position, (index, is_start, _)) in members.into_iter().enumerate() {
            let shift = (position as f32 + 0.5) * step - span / 2.0;
            match is_start {
                true => (out[index].0, out[index].2) = (shift, half),
                false => (out[index].1, out[index].3) = (shift, half),
            }
        }
    }
    out
}

/// The relations a diagram draws: tables, not views, and not the partitions
/// of a partitioned table, which would draw its one shape once per partition.
fn diagram_tables(schema: &crate::db::Schema) -> Vec<String> {
    let mut names: Vec<String> = schema
        .relations
        .iter()
        .filter(|relation| {
            matches!(
                relation.kind,
                RelationKind::Table | RelationKind::PartitionedTable
            ) && relation.partition_of.is_none()
        })
        .map(|relation| relation.name.clone())
        .collect();
    names.sort();
    names
}

/// What the engine's statistics say about a table, for its header: rows, then
/// size, either alone, or nothing. Read from the catalog at render rather
/// than copied in at load, because the sizes land after the catalog does.
fn statistics(catalog: Option<&Catalog>, schema: &str, name: &str) -> Option<String> {
    let relation = catalog_relation(catalog?, schema, name)?;
    let rows = relation.rows.map(|rows| trf!("{} rows", short_count(rows)));
    let size = relation.size.map(human_bytes);
    match (rows, size) {
        (Some(rows), Some(size)) => Some(format!("{rows} · {size}")),
        (rows, size) => rows.or(size),
    }
}

/// `1234` → `1,234`, `48210` → `48.2k`, `3100000` → `3.1M`: a header has room
/// for a glance, not for every digit.
fn short_count(value: u64) -> String {
    match value {
        0..10_000 => group_thousands(value),
        10_000..1_000_000 => format!("{:.1}k", value as f64 / 1e3),
        _ => format!("{:.1}M", value as f64 / 1e6),
    }
}

fn centered(t: Theme, message: String) -> AnyElement {
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .p(px(layout::SPACE_LG))
        .text_color(t.text_muted)
        .child(message)
        .into_any_element()
}
