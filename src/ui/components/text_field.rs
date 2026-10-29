//! A bounded, single-line field with grapheme editing and native IME support.
use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, ElementInputHandler,
    EntityInputHandler, EventEmitter, FocusHandle, Focusable, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, PaintQuad, Pixels, Point, Render, ShapedLine, SharedString,
    Subscription, TextRun, UTF16Selection, UnderlineStyle, Window, canvas, div, fill, point,
    prelude::*, px, rgb, size,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::ui::theme::Theme;

const MAX_CHARACTERS: usize = 1024;
const LINE_HEIGHT: f32 = 24.;
/// Line height of the compact variant used for in-place editing in lists.
const COMPACT_LINE_HEIGHT: f32 = 18.;

#[derive(Clone, Debug)]
pub enum TextFieldEvent {
    Changed,
    Submitted,
    Cancelled,
}

pub struct TextField {
    value: String,
    placeholder: SharedString,
    focus: FocusHandle,
    anchor: usize,
    cursor: usize,
    marked: Option<Range<usize>>,
    selecting: bool,
    layout: Option<ShapedLine>,
    bounds: Option<Bounds<Pixels>>,
    scroll_x: Pixels,
    focus_subscriptions: Vec<Subscription>,
    compact: bool,
}

impl TextField {
    #[must_use]
    pub fn new(value: String, placeholder: String, cx: &mut Context<Self>) -> Self {
        let value = if value.chars().count() > MAX_CHARACTERS || value.chars().any(char::is_control)
        {
            single_line(&value, MAX_CHARACTERS)
        } else {
            value
        };
        let cursor = value.len();
        Self {
            value,
            placeholder: placeholder.into(),
            focus: cx.focus_handle(),
            anchor: cursor,
            cursor,
            marked: None,
            selecting: false,
            layout: None,
            bounds: None,
            scroll_x: px(0.),
            focus_subscriptions: Vec::new(),
            compact: false,
        }
    }

    /// A shorter field that fits inside a list row, for in-place renaming.
    #[must_use]
    pub fn compact(mut self) -> Self {
        self.compact = true;
        self
    }

    fn line_height(&self) -> f32 {
        if self.compact {
            COMPACT_LINE_HEIGHT
        } else {
            LINE_HEIGHT
        }
    }

    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    #[must_use]
    pub fn is_composing(&self) -> bool {
        self.marked.is_some()
    }

    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        self.anchor = 0;
        self.cursor = self.value.len();
        self.marked = None;
        cx.notify();
    }

    /// Replay input buffered before this modal acquired focus.
    pub(crate) fn replay_key(
        &mut self,
        input: &crate::core::input::KeyInput,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let event = KeyDownEvent {
            native_key: None,
            keystroke: gpui::Keystroke {
                key: input.key.clone(),
                key_char: input.text.clone(),
                modifiers: gpui::Modifiers {
                    shift: input.modifiers.shift,
                    control: input.modifiers.control,
                    alt: input.modifiers.alt,
                    platform: input.modifiers.super_key,
                    ..Default::default()
                },
            },
            is_held: input.repeat,
        };
        self.key_down(&event, window, cx);
        if !input.modifiers.control
            && !input.modifiers.alt
            && !input.modifiers.super_key
            && let Some(text) = input
                .text
                .as_deref()
                .filter(|text| !text.is_empty() && text.chars().all(|ch| !ch.is_control()))
        {
            self.replace_text_in_range(None, text, window, cx);
        }
    }

    fn selection(&self) -> Range<usize> {
        self.anchor.min(self.cursor)..self.anchor.max(self.cursor)
    }

    fn move_cursor(&mut self, offset: usize, extend: bool, cx: &mut Context<Self>) {
        self.cursor = offset.min(self.value.len());
        if !extend {
            self.anchor = self.cursor;
        }
        self.marked = None;
        cx.notify();
    }

    fn previous_boundary(&self) -> usize {
        self.value
            .grapheme_indices(true)
            .rev()
            .find_map(|(offset, _)| (offset < self.cursor).then_some(offset))
            .unwrap_or(0)
    }

    fn next_boundary(&self) -> usize {
        self.value
            .grapheme_indices(true)
            .find_map(|(offset, _)| (offset > self.cursor).then_some(offset))
            .unwrap_or(self.value.len())
    }

    fn previous_word(&self) -> usize {
        self.value
            .unicode_word_indices()
            .rev()
            .find_map(|(offset, _)| (offset < self.cursor).then_some(offset))
            .unwrap_or(0)
    }

    fn next_word(&self) -> usize {
        self.value
            .unicode_word_indices()
            .find_map(|(offset, text)| {
                (offset + text.len() > self.cursor).then_some(offset + text.len())
            })
            .unwrap_or(self.value.len())
    }

    fn replacement_range(&self, range: Option<&Range<usize>>) -> Range<usize> {
        range
            .map(|range| from_utf16(&self.value, range))
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selection())
    }

    fn replace(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) -> Range<usize> {
        let retained =
            self.value[..range.start].chars().count() + self.value[range.end..].chars().count();
        let text = single_line(text, MAX_CHARACTERS.saturating_sub(retained));
        let changed = self.value[range.clone()] != text;
        let end = range.start + text.len();
        self.value.replace_range(range.clone(), &text);
        self.cursor = end;
        self.anchor = end;
        self.marked = None;
        if changed {
            cx.emit(TextFieldEvent::Changed);
        }
        cx.notify();
        range.start..end
    }

    fn copy(&self, cx: &mut Context<Self>) {
        let range = self.selection();
        if !range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(self.value[range].to_owned()));
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = &event.keystroke;
        let command = key.modifiers.control || key.modifiers.platform;
        let extend = key.modifiers.shift;
        // The platform IME owns Return/Escape while a composition is active.
        if self.marked.is_some() && matches!(key.key.as_str(), "enter" | "escape") {
            return;
        }
        let handled = if command && !key.modifiers.alt {
            self.command_key(&key.key, window, cx)
        } else {
            false
        };
        if !handled && !key.modifiers.alt {
            match key.key.as_str() {
                "enter" => cx.emit(TextFieldEvent::Submitted),
                "escape" => cx.emit(TextFieldEvent::Cancelled),
                "backspace" => {
                    if self.selection().is_empty() {
                        self.anchor = if command {
                            self.previous_word()
                        } else {
                            self.previous_boundary()
                        };
                    }
                    self.replace(self.selection(), "", cx);
                }
                "delete" => {
                    if self.selection().is_empty() {
                        self.anchor = if command {
                            self.next_word()
                        } else {
                            self.next_boundary()
                        };
                    }
                    self.replace(self.selection(), "", cx);
                }
                "left" => {
                    let offset = if command {
                        self.previous_word()
                    } else if !extend && !self.selection().is_empty() {
                        self.selection().start
                    } else {
                        self.previous_boundary()
                    };
                    self.move_cursor(offset, extend, cx);
                }
                "right" => {
                    let offset = if command {
                        self.next_word()
                    } else if !extend && !self.selection().is_empty() {
                        self.selection().end
                    } else {
                        self.next_boundary()
                    };
                    self.move_cursor(offset, extend, cx);
                }
                "home" => self.move_cursor(0, extend, cx),
                "end" => self.move_cursor(self.value.len(), extend, cx),
                _ => return,
            }
        } else if !handled {
            return;
        }
        window.prevent_default();
        cx.stop_propagation();
    }

    fn command_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match key {
            "a" => self.select_all(cx),
            "c" => self.copy(cx),
            "x" => {
                self.copy(cx);
                self.replace(self.selection(), "", cx);
            }
            "v" => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    self.replace_text_in_range(None, &text, window, cx);
                }
            }
            _ => return false,
        }
        true
    }

    fn index_at(&self, position: Point<Pixels>) -> usize {
        let (Some(bounds), Some(layout)) = (self.bounds, self.layout.as_ref()) else {
            return 0;
        };
        if self.value.is_empty() {
            return 0;
        }
        let offset = layout
            .closest_index_for_x(position.x - bounds.left() + self.scroll_x)
            .min(self.value.len());
        self.value
            .grapheme_indices(true)
            .map(|(index, _)| index)
            .chain(std::iter::once(self.value.len()))
            .take_while(|index| *index <= offset)
            .last()
            .unwrap_or(0)
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window);
        let offset = self.index_at(event.position);
        self.move_cursor(offset, event.modifiers.shift, cx);
        if event.click_count >= 3 {
            self.select_all(cx);
        } else if event.click_count == 2 {
            self.anchor = self.previous_word();
            self.cursor = self.next_word();
        }
        self.selecting = true;
        cx.stop_propagation();
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting && event.dragging() {
            self.move_cursor(self.index_at(event.position), true, cx);
        }
    }

    fn prepare(&mut self, bounds: Bounds<Pixels>, window: &mut Window) -> FieldPaint {
        let theme = Theme::default();
        let style = window.text_style();
        let placeholder = self.value.is_empty();
        let text: SharedString = if placeholder {
            self.placeholder.clone()
        } else {
            self.value.clone().into()
        };
        let base = TextRun {
            len: text.len(),
            font: style.font(),
            color: rgb(if placeholder {
                theme.colors.muted
            } else {
                theme.colors.text
            })
            .into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = marked_runs(base, self.marked.as_ref());
        let line = window.text_system().shape_line(text, px(13.), &runs, None);
        let cursor_x = line.x_for_index(self.cursor);
        let available = (bounds.size.width - px(2.)).max(px(0.));
        if cursor_x < self.scroll_x {
            self.scroll_x = cursor_x;
        }
        if cursor_x > self.scroll_x + available {
            self.scroll_x = cursor_x - available;
        }
        let maximum_scroll = (line.x_for_index(self.value.len()) - available).max(px(0.));
        self.scroll_x = self.scroll_x.max(px(0.)).min(maximum_scroll);
        let origin = point(bounds.left() - self.scroll_x, bounds.top());
        let range = self.selection();
        let selection = (!range.is_empty()).then(|| {
            fill(
                Bounds::from_corners(
                    point(origin.x + line.x_for_index(range.start), bounds.top()),
                    point(origin.x + line.x_for_index(range.end), bounds.bottom()),
                ),
                rgb(theme.colors.accent_muted),
            )
        });
        let cursor = (range.is_empty() && self.focus.is_focused(window)).then(|| {
            fill(
                Bounds::new(
                    point(origin.x + cursor_x, bounds.top()),
                    size(px(1.5), bounds.size.height),
                ),
                rgb(theme.colors.accent),
            )
        });
        self.layout = Some(line.clone());
        self.bounds = Some(bounds);
        FieldPaint {
            line,
            origin,
            selection,
            cursor,
        }
    }
}

impl EventEmitter<TextFieldEvent> for TextField {}

impl Focusable for TextField {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EntityInputHandler for TextField {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = from_utf16(&self.value, &range);
        *adjusted = Some(to_utf16(&self.value, &range));
        Some(self.value[range].to_owned())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: to_utf16(&self.value, &self.selection()),
            reversed: self.cursor < self.anchor,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked
            .as_ref()
            .map(|range| to_utf16(&self.value, range))
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace(self.replacement_range(range.as_ref()), text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let inserted = self.replace(self.replacement_range(range.as_ref()), text, cx);
        if !inserted.is_empty() {
            self.marked = Some(inserted.clone());
        }
        if let Some(selected) = selected {
            let range = from_utf16(&self.value[inserted.clone()], &selected);
            self.anchor = inserted.start + range.start;
            self.cursor = inserted.start + range.end;
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.layout.as_ref()?;
        let bounds = self.bounds?;
        let range = from_utf16(&self.value, &range);
        Some(Bounds::from_corners(
            point(
                bounds.left() - self.scroll_x + layout.x_for_index(range.start),
                bounds.top(),
            ),
            point(
                bounds.left() - self.scroll_x + layout.x_for_index(range.end),
                bounds.bottom(),
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        self.bounds?.localize(&point)?;
        Some(self.value[..self.index_at(point)].encode_utf16().count())
    }
}

struct FieldPaint {
    line: ShapedLine,
    origin: Point<Pixels>,
    selection: Option<PaintQuad>,
    cursor: Option<PaintQuad>,
}

impl Render for TextField {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.focus_subscriptions.is_empty() {
            self.focus_subscriptions
                .push(cx.on_focus(&self.focus, window, |_, _, cx| cx.notify()));
            self.focus_subscriptions
                .push(cx.on_blur(&self.focus, window, |field, _, cx| {
                    field.selecting = false;
                    cx.notify();
                }));
        }
        let theme = Theme::default();
        let prepare = cx.entity();
        let paint = prepare.clone();
        let focused = self.focus.is_focused(window);
        let line_height = self.line_height();
        div()
            .id("text-field")
            .w_full()
            .when(self.compact, |field| {
                field.h(px(line_height + 6.)).px(px(6.)).py(px(2.))
            })
            .when(!self.compact, |field| field.h(px(36.)).px_3().py(px(5.)))
            .overflow_hidden()
            .track_focus(&self.focus)
            .key_context("TextField")
            .cursor(CursorStyle::IBeam)
            .rounded(px(if self.compact { 5. } else { 7. }))
            .border_1()
            .border_color(rgb(if focused {
                theme.colors.accent
            } else {
                theme.colors.border_strong
            }))
            .bg(rgb(theme.colors.terminal))
            .font_family("sans-serif")
            .text_size(px(13.))
            .line_height(px(line_height))
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|field, _, _, _| field.selecting = false),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|field, _, _, _| field.selecting = false),
            )
            .child(
                canvas(
                    move |bounds, window, cx| {
                        prepare.update(cx, |field, _| field.prepare(bounds, window))
                    },
                    move |bounds, state: FieldPaint, window, cx| {
                        let focus = paint.read(cx).focus.clone();
                        window.handle_input(
                            &focus,
                            ElementInputHandler::new(bounds, paint.clone()),
                            cx,
                        );
                        window.with_content_mask(Some(ContentMask { bounds }), |window| {
                            if let Some(selection) = state.selection {
                                window.paint_quad(selection);
                            }
                            if let Err(error) =
                                state.line.paint(state.origin, px(line_height), window, cx)
                            {
                                eprintln!("Could not paint text field: {error:#}");
                            }
                            if let Some(cursor) = state.cursor {
                                window.paint_quad(cursor);
                            }
                        });
                    },
                )
                .w_full()
                .h(px(line_height)),
            )
    }
}

fn single_line(text: &str, limit: usize) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(limit)
        .collect()
}

fn byte_from_utf16(text: &str, offset: usize) -> usize {
    let mut units = 0;
    for (index, character) in text.char_indices() {
        if units >= offset {
            return index;
        }
        units += character.len_utf16();
    }
    text.len()
}

fn from_utf16(text: &str, range: &Range<usize>) -> Range<usize> {
    let start = byte_from_utf16(text, range.start);
    let end = byte_from_utf16(text, range.end);
    start.min(end)..start.max(end)
}

fn to_utf16(text: &str, range: &Range<usize>) -> Range<usize> {
    text[..range.start].encode_utf16().count()..text[..range.end].encode_utf16().count()
}

fn marked_runs(base: TextRun, marked: Option<&Range<usize>>) -> Vec<TextRun> {
    let Some(marked) = marked else {
        return vec![base];
    };
    let length = base.len;
    vec![
        TextRun {
            len: marked.start,
            ..base.clone()
        },
        TextRun {
            len: marked.len(),
            underline: Some(UnderlineStyle {
                color: Some(base.color),
                thickness: px(1.),
                wavy: false,
            }),
            ..base.clone()
        },
        TextRun {
            len: length.saturating_sub(marked.end),
            ..base
        },
    ]
    .into_iter()
    .filter(|run| run.len > 0)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::{from_utf16, single_line, to_utf16};

    #[test]
    fn utf16_ranges_preserve_unicode_scalar_boundaries() {
        let text = "a😀界";
        assert_eq!(from_utf16(text, &(1..3)), 1..5);
        assert_eq!(from_utf16(text, &(2..3)), 5..5);
        assert_eq!(from_utf16(text, &(0..999)), 0..text.len());
        assert_eq!(to_utf16(text, &(1..5)), 1..3);
    }

    #[test]
    fn labels_are_bounded_and_single_line_without_splitting_utf8() {
        assert_eq!(single_line("界😀abc", 2), "界😀");
        assert_eq!(single_line("first\nsecond\r\t", 99), "first second  ");
    }
}
