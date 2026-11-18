//! Stateful terminal viewport: input, measured layout and selection around owned frames.
use std::{ops::Range, sync::Arc};

use gpui::{
    App, Bounds, Context, CursorStyle, ElementInputHandler, EntityInputHandler, EventEmitter,
    FocusHandle, Focusable, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels,
    Point, Render, ScrollWheelEvent, Subscription, TextRun, UTF16Selection, Window, canvas, div,
    font, point, prelude::*, px, rgb, size,
};

use super::{components::terminal_surface, theme::Theme};
use crate::core::{
    GridSize,
    frame::{CellWidth, TerminalFrame},
    input::{KeyInput, Modifiers},
};

#[derive(Clone, Debug)]
pub enum TerminalViewEvent {
    Focused,
    Key(KeyInput),
    Text(String),
    Resized {
        size: GridSize,
        cell_width: u32,
        cell_height: u32,
    },
    /// Positive values move toward older terminal history.
    Scroll(i32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct CellPoint {
    row: u16,
    column: u16,
}

pub struct TerminalView {
    frame: Arc<TerminalFrame>,
    theme: Theme,
    focus: FocusHandle,
    bounds: Option<Bounds<Pixels>>,
    last_resize: Option<(GridSize, u32, u32)>,
    scale_factor: f32,
    selection: Option<(CellPoint, CellPoint)>,
    selecting: bool,
    scroll_remainder: f32,
    marked_text: String,
    marked_selection: Range<usize>,
    focus_subscriptions: Vec<Subscription>,
}

impl TerminalView {
    pub fn new(frame: Arc<TerminalFrame>, theme: Theme, cx: &mut Context<Self>) -> Self {
        Self {
            frame,
            theme,
            focus: cx.focus_handle(),
            bounds: None,
            last_resize: None,
            scale_factor: 1.,
            selection: None,
            selecting: false,
            scroll_remainder: 0.,
            marked_text: String::new(),
            marked_selection: 0..0,
            focus_subscriptions: Vec::new(),
        }
    }

    pub fn set_frame(&mut self, frame: Arc<TerminalFrame>, cx: &mut Context<Self>) {
        if Arc::ptr_eq(&frame, &self.frame) {
            return;
        }
        if frame.size != self.frame.size {
            self.selection = None;
            self.selecting = false;
        }
        self.frame = frame;
        cx.notify();
    }

    #[must_use]
    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection_range()?;
        let mut lines = Vec::new();
        for row_index in start.row..=end.row {
            let row = self.frame.rows.get(usize::from(row_index))?;
            let first = if row_index == start.row {
                start.column
            } else {
                0
            };
            let last = if row_index == end.row {
                end.column
            } else {
                self.frame.size.columns() - 1
            };
            let mut text = String::new();
            for column in first..=last {
                let Some(cell) = row.get(usize::from(column)) else {
                    continue;
                };
                if cell.width == CellWidth::Continuation {
                    continue;
                }
                text.push_str(if cell.text.is_empty() {
                    " "
                } else {
                    &cell.text
                });
            }
            lines.push(text.trim_end().to_owned());
        }
        Some(lines.join("\n"))
    }

    fn selection_range(&self) -> Option<(CellPoint, CellPoint)> {
        let (anchor, head) = self.selection?;
        if anchor == head {
            return None;
        }
        let (mut start, mut end) = (anchor.min(head), anchor.max(head));
        let start_cell = self
            .frame
            .rows
            .get(usize::from(start.row))?
            .get(usize::from(start.column))?;
        if start_cell.width == CellWidth::Continuation {
            start.column = start.column.saturating_sub(1);
        }
        let end_cell = self
            .frame
            .rows
            .get(usize::from(end.row))?
            .get(usize::from(end.column))?;
        if end_cell.width == CellWidth::Wide {
            end.column = end
                .column
                .saturating_add(1)
                .min(self.frame.size.columns() - 1);
        }
        Some((start, end))
    }

    fn measure(&mut self, window: &mut Window) {
        let metrics = self.theme.terminal;
        let line = window.text_system().shape_line(
            "M".into(),
            px(metrics.font_size),
            &[TextRun {
                len: 1,
                font: font(metrics.font_family),
                color: rgb(self.theme.colors.text).into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        );
        self.theme.terminal.cell_width = f32::from(line.width).max(1.);
        self.theme.terminal.cell_height = f32::from(line.ascent + line.descent)
            .ceil()
            .max(metrics.font_size * 1.2);
        self.scale_factor = window.scale_factor();
    }

    fn update_bounds(&mut self, bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        self.bounds = Some(bounds);
        if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
            return;
        }
        let metrics = self.theme.terminal;
        let columns = bounded_u16(f32::from(bounds.size.width) / metrics.cell_width);
        let rows = bounded_u16(f32::from(bounds.size.height) / metrics.cell_height);
        let Ok(grid) = GridSize::new(columns, rows) else {
            return;
        };
        let width = u32::from(bounded_u16((metrics.cell_width * self.scale_factor).ceil()));
        let height = u32::from(bounded_u16(
            (metrics.cell_height * self.scale_factor).ceil(),
        ));
        let resize = (grid, width, height);
        if self.last_resize != Some(resize) {
            self.last_resize = Some(resize);
            cx.emit(TerminalViewEvent::Resized {
                size: grid,
                cell_width: width,
                cell_height: height,
            });
        }
    }

    fn cell_at(&self, position: Point<Pixels>) -> Option<CellPoint> {
        let bounds = self.bounds?;
        let local = position - bounds.origin;
        let column = bounded_u16_zero(f32::from(local.x) / self.theme.terminal.cell_width)
            .min(self.frame.size.columns() - 1);
        let row = bounded_u16_zero(f32::from(local.y) / self.theme.terminal.cell_height)
            .min(self.frame.size.rows() - 1);
        Some(CellPoint { row, column })
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window);
        if let Some(cell) = self.cell_at(event.position) {
            let anchor = if event.modifiers.shift {
                self.selection.map_or(cell, |(anchor, _)| anchor)
            } else {
                cell
            };
            self.selection = Some((anchor, cell));
            self.selecting = true;
            cx.notify();
        }
        cx.stop_propagation();
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting
            && event.dragging()
            && let (Some((anchor, _)), Some(head)) = (self.selection, self.cell_at(event.position))
        {
            self.selection = Some((anchor, head));
            cx.notify();
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let key = &event.keystroke;
        // GPUI dispatches matching application keybindings before this raw
        // listener; unhandled keys belong to the terminal backend.
        let modifiers = Modifiers {
            shift: key.modifiers.shift,
            control: key.modifiers.control,
            alt: key.modifiers.alt,
            super_key: key.modifiers.platform,
        };
        let plain = !modifiers.control && !modifiers.alt && !modifiers.super_key;
        let printable = key.key_char.as_ref().is_some_and(|text| {
            !text.is_empty() && text.chars().all(|character| !character.is_control())
        });
        if plain && (printable || !self.marked_text.is_empty()) {
            // GPUI delivers committed text through EntityInputHandler. Emitting
            // here as well would duplicate printable keys and break IME input.
            return;
        }
        cx.emit(TerminalViewEvent::Key(KeyInput {
            key: key.key.clone(),
            text: key.key_char.clone(),
            modifiers,
            repeat: event.is_held,
        }));
        cx.stop_propagation();
    }

    fn scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let height = self.theme.terminal.cell_height;
        self.scroll_remainder += f32::from(event.delta.pixel_delta(px(height)).y) / height;
        let lines = bounded_scroll(self.scroll_remainder);
        if lines != 0 {
            // The bounded range is exactly representable by f32.
            self.scroll_remainder -= f32::from(lines);
            self.selection = None;
            cx.emit(TerminalViewEvent::Scroll(i32::from(lines)));
        }
        cx.stop_propagation();
    }

    fn selection_rectangles(&self) -> Vec<gpui::Div> {
        let Some((start, end)) = self.selection_range() else {
            return Vec::new();
        };
        let metrics = self.theme.terminal;
        (start.row..=end.row)
            .map(|row| {
                let first = if row == start.row { start.column } else { 0 };
                let last = if row == end.row {
                    end.column
                } else {
                    self.frame.size.columns() - 1
                };
                div()
                    .absolute()
                    .left(px(f32::from(first) * metrics.cell_width))
                    .top(px(f32::from(row) * metrics.cell_height))
                    .w(px(f32::from(last - first + 1) * metrics.cell_width))
                    .h(px(metrics.cell_height))
                    .bg(rgb(self.theme.colors.accent))
                    .opacity(0.35)
            })
            .collect()
    }
}

impl EventEmitter<TerminalViewEvent> for TerminalView {}
impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.measure(window);
        if self.focus_subscriptions.is_empty() {
            self.focus_subscriptions
                .push(cx.on_focus(&self.focus, window, |_, _, cx| {
                    cx.emit(TerminalViewEvent::Focused);
                    cx.notify();
                }));
            self.focus_subscriptions
                .push(cx.on_blur(&self.focus, window, |_, _, cx| cx.notify()));
        }
        let viewport = cx.entity();
        let bounds_view = viewport.downgrade();
        let input_focus = self.focus.clone();
        let mut root = div()
            .id("terminal-viewport")
            .relative()
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .cursor(CursorStyle::IBeam)
            .track_focus(&self.focus)
            .bg(rgb(self.frame.background.to_hex()))
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|view, _, _, _| view.selecting = false),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|view, _, _, _| view.selecting = false),
            )
            .on_scroll_wheel(cx.listener(Self::scroll))
            .child(terminal_surface(
                &self.frame,
                self.focus.is_focused(window),
                self.theme,
            ))
            .children(self.selection_rectangles());
        if !self.marked_text.is_empty() {
            let cursor = self.frame.cursor;
            let column = cursor.map_or(0, |cursor| cursor.column);
            let row = cursor.map_or(0, |cursor| cursor.row);
            root = root.child(
                div()
                    .absolute()
                    .left(px(f32::from(column) * self.theme.terminal.cell_width))
                    .top(px(f32::from(row) * self.theme.terminal.cell_height))
                    .bg(rgb(self.theme.colors.surface))
                    .text_color(rgb(self.theme.colors.text))
                    .font_family(self.theme.terminal.font_family)
                    .text_size(px(self.theme.terminal.font_size))
                    .underline()
                    .child(self.marked_text.clone()),
            );
        }
        root.child(
            canvas(
                move |bounds, _, cx| {
                    cx.defer(move |cx| {
                        let _ = bounds_view.update(cx, |view, cx| view.update_bounds(bounds, cx));
                    });
                },
                move |bounds, (), window, cx| {
                    window.handle_input(
                        &input_focus,
                        ElementInputHandler::new(bounds, viewport),
                        cx,
                    );
                },
            )
            .absolute()
            .size_full(),
        )
    }
}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let utf16: Vec<_> = self.marked_text.encode_utf16().collect();
        let text = String::from_utf16(utf16.get(range.clone())?).ok()?;
        *actual = Some(range);
        Some(text)
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.marked_selection.clone(),
            reversed: false,
        })
    }
    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        (!self.marked_text.is_empty()).then(|| 0..self.marked_text.encode_utf16().count())
    }
    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let text = std::mem::take(&mut self.marked_text);
        self.marked_selection = 0..0;
        if !text.is_empty() {
            cx.emit(TerminalViewEvent::Text(text));
        }
        cx.notify();
    }
    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_text.clear();
        self.marked_selection = 0..0;
        self.selection = None;
        if !text.is_empty() {
            cx.emit(TerminalViewEvent::Text(text.to_owned()));
        }
        cx.notify();
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        selection: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        text.clone_into(&mut self.marked_text);
        let length = text.encode_utf16().count();
        self.marked_selection = selection.map_or(length..length, |range| {
            range.start.min(length)..range.end.min(length)
        });
        cx.notify();
    }
    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let cursor = self.frame.cursor?;
        Some(Bounds::new(
            bounds.origin
                + point(
                    px(f32::from(cursor.column) * self.theme.terminal.cell_width),
                    px(f32::from(cursor.row) * self.theme.terminal.cell_height),
                ),
            size(
                px(self.theme.terminal.cell_width),
                px(self.theme.terminal.cell_height),
            ),
        ))
    }
    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        self.cell_at(point).map(|_| 0)
    }
}

// Saturating casts are deliberate at the bounded pixel/grid boundary.
#[expect(
    clippy::cast_sign_loss,
    reason = "Value is clamped to the unsigned grid range."
)]
fn bounded_u16(value: f32) -> u16 {
    value.floor().clamp(1., f32::from(u16::MAX)) as u16
}
#[expect(
    clippy::cast_sign_loss,
    reason = "Value is clamped to the unsigned grid range."
)]
fn bounded_u16_zero(value: f32) -> u16 {
    value.floor().clamp(0., f32::from(u16::MAX)) as u16
}
fn bounded_scroll(value: f32) -> i16 {
    value.trunc().clamp(-32_767., 32_767.) as i16
}
