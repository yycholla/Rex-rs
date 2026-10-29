//! Stateful terminal viewport: input, measured layout and selection around owned frames.
use std::{
    ops::Range,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use gpui::{
    App, Bounds, Context, CursorStyle, DispatchPhase, ElementInputHandler, EntityInputHandler,
    EventEmitter, FocusHandle, Focusable, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, ScrollWheelEvent, Subscription, Task,
    TextRun, UTF16Selection, Window, canvas, div, fill, font, point, prelude::*, px, rgb, rgba,
    size,
};

use super::{
    components::{
        TerminalPaintPhase, TerminalSurfaceCache, TerminalSurfacePreparation, terminal_surface,
    },
    theme::{TerminalMetrics, Theme},
};
use crate::core::{
    GridSize,
    frame::TerminalFrame,
    input::{
        KeyInput, Modifiers, MouseAction, MouseButton as TerminalMouseButton, MouseInput,
        MouseTracking,
    },
    interaction::{CellSpan, SelectionCommand, TerminalAnnotations},
};

#[derive(Clone, Debug)]
pub enum TerminalViewEvent {
    Focused,
    OpenLink {
        column: u16,
        row: u16,
        viewport_token: u64,
    },
    FocusChanged(bool),
    Mouse(MouseInput),
    /// Local display selection; never delivered to the terminal application.
    Selection(SelectionCommand),
    /// A physical click also fires when the terminal already has GPUI focus.
    Interacted,
    Key(KeyInput),
    Text(String),
    Resized {
        size: GridSize,
        cell_width: u32,
        cell_height: u32,
    },
    /// Positive values move toward older terminal history.
    Scroll(i32),
    /// Wheel input retains its location and modifiers for terminal mouse reporting.
    Wheel {
        lines: i32,
        column: u16,
        row: u16,
        modifiers: Modifiers,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct CellPoint {
    row: u16,
    column: u16,
}

/// Track only presses sent to an application, so Shift selection never generates
/// an unmatched release or drag. Releases remain deliverable after Shift changes.
#[derive(Default)]
struct PointerRouting {
    held: [Option<MouseInput>; 3],
    last_move: Option<MouseInput>,
}

impl PointerRouting {
    fn route(&mut self, input: MouseInput, enabled: bool, tracking: MouseTracking) -> bool {
        if !enabled || tracking == MouseTracking::None {
            return false;
        }
        let index = match input.button {
            TerminalMouseButton::Left => Some(0),
            TerminalMouseButton::Middle => Some(1),
            TerminalMouseButton::Right => Some(2),
            TerminalMouseButton::None => None,
        };
        match input.action {
            MouseAction::Press => {
                if input.modifiers.shift {
                    return false;
                }
                let Some(index) = index else { return false };
                if self.held[index].is_some() {
                    return false;
                }
                self.held[index] = Some(input);
                self.last_move = None;
                true
            }
            MouseAction::Release => {
                let Some(index) = index else { return false };
                self.last_move = None;
                self.held[index].take().is_some()
            }
            MouseAction::Move => {
                if tracking == MouseTracking::Press
                    || (tracking == MouseTracking::Drag && index.is_none())
                    || input.modifiers.shift
                    || index.is_some_and(|index| self.held[index].is_none())
                    || self.last_move == Some(input)
                {
                    return false;
                }
                self.last_move = Some(input);
                // Release on focus loss uses the latest application coordinates.
                for held in self.held.iter_mut().flatten() {
                    held.column = input.column;
                    held.row = input.row;
                    held.modifiers = input.modifiers;
                }
                true
            }
        }
    }

    fn release_all(&mut self) -> impl Iterator<Item = MouseInput> + use<> {
        self.last_move = None;
        std::mem::take(&mut self.held)
            .into_iter()
            .flatten()
            .map(|mut input| {
                input.action = MouseAction::Release;
                // Synthetic cleanup is not a Shift selection gesture.
                input.modifiers.shift = false;
                input
            })
    }

    fn dragging(&self) -> bool {
        self.held.iter().any(Option::is_some)
    }
}

/// Readiness gates delivery, but never discards a changed desired focus state.
fn take_focus_report(enabled: bool, desired: bool, reported: &mut Option<bool>) -> Option<bool> {
    if enabled && *reported != Some(desired) {
        *reported = Some(desired);
        Some(desired)
    } else {
        None
    }
}

fn terminal_button(button: MouseButton) -> Option<TerminalMouseButton> {
    match button {
        MouseButton::Left => Some(TerminalMouseButton::Left),
        MouseButton::Middle => Some(TerminalMouseButton::Middle),
        MouseButton::Right => Some(TerminalMouseButton::Right),
        MouseButton::Navigate(_) => None,
    }
}

fn terminal_modifiers(modifiers: gpui::Modifiers) -> Modifiers {
    Modifiers {
        shift: modifiers.shift,
        control: modifiers.control,
        alt: modifiers.alt,
        super_key: modifiers.platform,
    }
}

const BLINK_HALF_PERIOD: Duration = Duration::from_millis(500);

fn text_blink_elapsed() -> Duration {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed()
}

fn blink_on(elapsed: Duration) -> bool {
    (elapsed.as_millis() / BLINK_HALF_PERIOD.as_millis()).is_multiple_of(2)
}

fn blink_boundary_delay(elapsed: Duration) -> Duration {
    let period = BLINK_HALF_PERIOD.as_nanos();
    Duration::from_nanos(u64::try_from(period - elapsed.as_nanos() % period).unwrap_or(500_000_000))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum BlinkEligibility {
    #[default]
    Steady,
    Animate,
}

impl BlinkEligibility {
    fn from_state(visible: bool, enabled: bool, active: bool) -> Self {
        if visible && enabled && active {
            Self::Animate
        } else {
            Self::Steady
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct BlinkDemand {
    eligibility: BlinkEligibility,
    focused: bool,
    cursor: bool,
    text: bool,
}

impl BlinkDemand {
    fn needed(self) -> bool {
        self.eligibility == BlinkEligibility::Animate
            && (self.text || (self.focused && self.cursor))
    }

    fn phase(self, text_elapsed: Duration, cursor_elapsed: Duration) -> TerminalPaintPhase {
        if self.eligibility == BlinkEligibility::Steady {
            return TerminalPaintPhase::default();
        }
        TerminalPaintPhase {
            cursor_visible: !self.focused || !self.cursor || blink_on(cursor_elapsed),
            text_visible: !self.text || blink_on(text_elapsed),
        }
    }

    fn delay(self, text_elapsed: Duration, cursor_elapsed: Duration) -> Duration {
        let text = self.text.then(|| blink_boundary_delay(text_elapsed));
        let cursor = (self.focused && self.cursor).then(|| blink_boundary_delay(cursor_elapsed));
        match (text, cursor) {
            (Some(text), Some(cursor)) => text.min(cursor),
            (Some(delay), None) | (None, Some(delay)) => delay,
            (None, None) => BLINK_HALF_PERIOD,
        }
    }
}

/// Lifecycle and clock state are separate from terminal input/selection state.
struct BlinkAnimation {
    visible: bool,
    enabled: bool,
    window_active: bool,
    demand: BlinkDemand,
    phase: TerminalPaintPhase,
    cursor_epoch: Instant,
    generation: u64,
    // Retained task cancellation follows hide, disable and entity destruction.
    task: Option<Task<()>>,
}

impl Default for BlinkAnimation {
    fn default() -> Self {
        Self {
            visible: false,
            enabled: false,
            window_active: false,
            demand: BlinkDemand::default(),
            phase: TerminalPaintPhase::default(),
            cursor_epoch: Instant::now(),
            generation: 0,
            task: None,
        }
    }
}

pub struct TerminalView {
    frame: Arc<TerminalFrame>,
    theme: Theme,
    focus: FocusHandle,
    bounds: Option<Bounds<Pixels>>,
    last_resize: Option<(GridSize, u32, u32)>,
    scale_factor: f32,
    measured_metrics: Option<(&'static str, u32, u32)>,
    surface_cache: TerminalSurfaceCache,
    image_window: Option<gpui::AnyWindowHandle>,
    displayed_frame: Option<Arc<TerminalFrame>>,
    preparation: Option<TerminalSurfacePreparation>,
    annotations: Arc<TerminalAnnotations>,
    gesture_clock: Instant,
    last_selection_drag: Option<SelectionCommand>,
    selecting: bool,
    scroll_remainder: f32,
    marked_text: String,
    marked_selection: Range<usize>,
    focus_subscriptions: Vec<Subscription>,
    input_enabled: bool,
    mouse_tracking: MouseTracking,
    pointer: PointerRouting,
    desired_focus: bool,
    reported_focus: Option<bool>,
    animation: BlinkAnimation,
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
            measured_metrics: None,
            surface_cache: TerminalSurfaceCache::default(),
            image_window: None,
            displayed_frame: None,
            preparation: None,
            annotations: Arc::default(),
            gesture_clock: Instant::now(),
            last_selection_drag: None,
            selecting: false,
            scroll_remainder: 0.,
            marked_text: String::new(),
            marked_selection: 0..0,
            focus_subscriptions: Vec::new(),
            input_enabled: true,
            mouse_tracking: MouseTracking::None,
            pointer: PointerRouting::default(),
            desired_focus: false,
            reported_focus: None,
            animation: BlinkAnimation::default(),
        }
    }

    fn observe_input_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.focus_subscriptions.is_empty() {
            let handle = window.window_handle();
            self.focus_subscriptions
                .push(cx.on_release(move |view, cx| {
                    let _ =
                        handle.update(cx, |_, window, _| view.surface_cache.release_images(window));
                }));
            self.focus_subscriptions
                .push(cx.on_focus(&self.focus, window, |view, window, cx| {
                    cx.emit(TerminalViewEvent::Focused);
                    view.report_focus(window, cx);
                    cx.notify();
                }));
            self.focus_subscriptions
                .push(cx.on_blur(&self.focus, window, |view, window, cx| {
                    view.report_focus(window, cx);
                    cx.notify();
                }));
            self.focus_subscriptions.push(cx.observe_window_activation(
                window,
                |view, window, cx| {
                    view.report_focus(window, cx);
                    cx.notify();
                },
            ));
        }
    }

    pub fn set_frame(&mut self, frame: Arc<TerminalFrame>, cx: &mut Context<Self>) {
        if Arc::ptr_eq(&frame, &self.frame) {
            return;
        }
        if frame.size != self.frame.size {
            self.cancel_selection(cx);
        }
        let cursor_moved = self.frame.cursor != frame.cursor;
        self.frame = frame;
        if cursor_moved {
            self.reset_cursor_blink(cx);
        }
        cx.notify();
    }

    /// Hidden panes own no animation timer, even when native blink is enabled.
    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.animation.visible != visible {
            self.animation.visible = visible;
            if !visible && let Some(handle) = self.image_window {
                let entity = cx.entity().downgrade();
                cx.defer(move |cx| {
                    let _ = handle.update(cx, |_, window, cx| {
                        let _ = entity.update(cx, |view, _| {
                            if !view.animation.visible {
                                view.surface_cache.release_images(window);
                            }
                        });
                    });
                });
            }
            if visible {
                self.animation.cursor_epoch = Instant::now();
            }
            self.reconcile_blink(cx);
            cx.notify();
        }
    }

    /// Read-only history remains usable, but exited/disconnected panes are steady.
    pub fn set_animation_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.animation.enabled != enabled {
            self.animation.enabled = enabled;
            if enabled {
                self.animation.cursor_epoch = Instant::now();
            }
            self.reconcile_blink(cx);
            cx.notify();
        }
    }

    fn reset_cursor_blink(&mut self, cx: &mut Context<Self>) {
        self.animation.cursor_epoch = Instant::now();
        self.stop_blink_task();
        self.reconcile_blink(cx);
    }

    fn stop_blink_task(&mut self) {
        self.animation.generation = self.animation.generation.wrapping_add(1);
        self.animation.task = None;
    }

    fn update_blink_phase(&mut self, cx: &mut Context<Self>) {
        let phase = self
            .animation
            .demand
            .phase(text_blink_elapsed(), self.animation.cursor_epoch.elapsed());
        if phase != self.animation.phase {
            self.animation.phase = phase;
            // This only repaints retained geometry; no native frame or row copy.
            cx.notify();
        }
    }

    fn reconcile_blink(&mut self, cx: &mut Context<Self>) {
        let demand = BlinkDemand {
            eligibility: BlinkEligibility::from_state(
                self.animation.visible,
                self.animation.enabled,
                self.animation.window_active,
            ),
            focused: self.desired_focus,
            cursor: self
                .displayed_frame
                .as_ref()
                .and_then(|frame| frame.cursor)
                .is_some_and(|cursor| cursor.blinking),
            text: self.surface_cache.has_blinking_text(),
        };
        if demand != self.animation.demand {
            self.stop_blink_task();
            self.animation.demand = demand;
        }
        self.update_blink_phase(cx);
        if !demand.needed() || self.animation.task.is_some() {
            return;
        }
        let generation = self.animation.generation;
        self.animation.task = Some(cx.spawn(async move |this, cx| {
            loop {
                let delay = this.update(cx, |view, _| {
                    (view.animation.generation == generation && view.animation.demand.needed())
                        .then(|| {
                            view.animation
                                .demand
                                .delay(text_blink_elapsed(), view.animation.cursor_epoch.elapsed())
                        })
                });
                let Ok(Some(delay)) = delay else { return };
                cx.background_executor().timer(delay).await;
                let keep_running = this.update(cx, |view, cx| {
                    if view.animation.generation != generation {
                        return false;
                    }
                    view.update_blink_phase(cx);
                    true
                });
                if !keep_running.is_ok_and(|keep_running| keep_running) {
                    return;
                }
            }
        }));
    }

    /// Annotation updates repaint overlays without invalidating shaped glyph rows.
    pub fn set_annotations(
        &mut self,
        annotations: Arc<TerminalAnnotations>,
        cx: &mut Context<Self>,
    ) {
        if Arc::ptr_eq(&annotations, &self.annotations) {
            return;
        }
        if annotations.viewport_token == 0 {
            self.cancel_selection(cx);
        }
        self.annotations = annotations;
        cx.notify();
    }

    /// Announce the current measured viewport after this client gains control.
    pub fn reset_viewport(&mut self, cx: &mut Context<Self>) {
        self.cancel_selection(cx);
        self.last_resize = None;
        self.reported_focus = None;
        self.pointer = PointerRouting::default();
        cx.notify();
    }

    /// The local viewport stays independent of a different client's terminal size.
    #[must_use]
    pub fn measured_viewport(&self) -> Option<(GridSize, u32, u32)> {
        self.last_resize
    }

    pub fn set_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        if !size.is_finite() || self.theme.terminal.font_size.to_bits() == size.to_bits() {
            return;
        }
        self.theme.terminal.font_size = size;
        self.measured_metrics = None;
        self.last_resize = None;
        self.preparation = None;
        cx.notify();
    }

    pub fn set_input_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.input_enabled == enabled {
            return;
        }
        self.cancel_selection(cx);
        self.input_enabled = enabled;
        self.reported_focus = None;
        // A hidden terminal may never render after readiness recovers. Publish
        // its latest focus immediately rather than retaining stale server focus.
        self.emit_focus(cx);
        self.pointer = PointerRouting::default();
        self.marked_text.clear();
        self.marked_selection = 0..0;
        cx.notify();
    }

    pub fn set_mouse_tracking(&mut self, tracking: MouseTracking, cx: &mut Context<Self>) {
        if self.mouse_tracking != tracking {
            self.mouse_tracking = tracking;
            if tracking == MouseTracking::None {
                self.pointer = PointerRouting::default();
            } else {
                // A tracking-mode change must not lose a press awaiting release.
                self.pointer.last_move = None;
            }
            cx.notify();
        }
    }

    fn report_focus(&mut self, window: &Window, cx: &mut Context<Self>) {
        let focused = self.focus.is_focused(window) && window.is_window_active();
        // Focus and activation subscriptions keep this current even offline.
        if self.desired_focus != focused {
            self.animation.cursor_epoch = Instant::now();
        }
        self.desired_focus = focused;
        self.animation.window_active = window.is_window_active();
        self.reconcile_blink(cx);
        if !focused {
            self.cancel_selection(cx);
            for release in self.pointer.release_all() {
                if self.input_enabled && self.mouse_tracking != MouseTracking::None {
                    cx.emit(TerminalViewEvent::Mouse(release));
                }
            }
        }
        self.emit_focus(cx);
    }

    fn emit_focus(&mut self, cx: &mut Context<Self>) {
        if let Some(focused) = take_focus_report(
            self.input_enabled,
            self.desired_focus,
            &mut self.reported_focus,
        ) {
            cx.emit(TerminalViewEvent::FocusChanged(focused));
        }
    }

    fn send_mouse(&mut self, input: MouseInput, cx: &mut Context<Self>) -> bool {
        if self
            .pointer
            .route(input, self.input_enabled, self.mouse_tracking)
        {
            cx.emit(TerminalViewEvent::Mouse(input));
            true
        } else {
            false
        }
    }

    fn cancel_selection(&mut self, cx: &mut Context<Self>) {
        if self.selecting {
            self.selecting = false;
            self.last_selection_drag = None;
            cx.emit(TerminalViewEvent::Selection(SelectionCommand::Cancel));
            cx.notify();
        }
    }

    fn clear_selection(&mut self, cx: &mut Context<Self>) {
        if self.selecting || !self.annotations.selection.is_empty() {
            self.selecting = false;
            self.last_selection_drag = None;
            cx.emit(TerminalViewEvent::Selection(SelectionCommand::Clear));
            cx.notify();
        }
    }

    fn selection_position(&self, position: Point<Pixels>) -> Option<(f32, f32, u64)> {
        let frame = self.displayed_frame.as_ref()?;
        if !presentation_current(frame, &self.frame, &self.annotations) {
            return None;
        }
        let origin = self.bounds?.origin;
        let (column, row) = fractional_cell_point(position, origin, self.theme.terminal)?;
        Some((column, row, self.annotations.viewport_token))
    }

    fn drag_selection(&mut self, position: Point<Pixels>, rectangle: bool, cx: &mut Context<Self>) {
        if let Some((column, row, viewport_token)) = self.selection_position(position) {
            let command = SelectionCommand::Drag {
                column,
                row,
                rectangle,
                viewport_token,
            };
            if !self
                .last_selection_drag
                .is_some_and(|previous| same_drag_position(previous, command))
            {
                self.last_selection_drag = Some(command);
                cx.emit(TerminalViewEvent::Selection(command));
            }
        }
    }

    fn measure(&mut self, window: &mut Window) -> bool {
        let metrics = self.theme.terminal;
        let scale_factor = window.scale_factor();
        let key = (
            metrics.font_family,
            metrics.font_size.to_bits(),
            scale_factor.to_bits(),
        );
        if self.measured_metrics == Some(key) {
            return false;
        }
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
        self.scale_factor = scale_factor;
        self.measured_metrics = Some(key);
        true
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
        let frame = self.displayed_frame.as_ref()?;
        let bounds = self.bounds?;
        Some(cell_point(
            position,
            bounds.origin,
            frame.size,
            self.theme.terminal,
        ))
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window);
        // An already focused terminal still needs to reclaim interaction.
        cx.emit(TerminalViewEvent::Interacted);
        if event.button == MouseButton::Left
            && event.modifiers.control
            && !event.modifiers.shift
            && let (Some(cell), Some((_, _, viewport_token))) = (
                self.cell_at(event.position),
                self.selection_position(event.position),
            )
        {
            cx.emit(TerminalViewEvent::OpenLink {
                column: cell.column,
                row: cell.row,
                viewport_token,
            });
            cx.stop_propagation();
            return;
        }
        if let (Some(cell), Some(button)) =
            (self.cell_at(event.position), terminal_button(event.button))
            && self.send_mouse(
                MouseInput {
                    action: MouseAction::Press,
                    button,
                    column: cell.column,
                    row: cell.row,
                    modifiers: terminal_modifiers(event.modifiers),
                },
                cx,
            )
        {
            self.clear_selection(cx);
            cx.notify();
            cx.stop_propagation();
            return;
        }
        if event.button == MouseButton::Left
            && let Some((column, row, viewport_token)) = self.selection_position(event.position)
        {
            self.selecting = true;
            // Merely releasing at the press position must remain a click;
            // synthesizing a zero-distance native drag would mark it dragged.
            self.last_selection_drag = Some(SelectionCommand::Drag {
                column,
                row,
                rectangle: event.modifiers.alt,
                viewport_token,
            });
            let time_millis =
                u64::try_from(self.gesture_clock.elapsed().as_millis()).unwrap_or(u64::MAX);
            cx.emit(TerminalViewEvent::Selection(SelectionCommand::Press {
                column,
                row,
                time_millis,
                viewport_token,
            }));
            cx.notify();
        }
        cx.stop_propagation();
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting {
            if event.dragging() {
                self.drag_selection(event.position, event.modifiers.alt, cx);
            }
        } else if let Some(cell) = self.cell_at(event.position) {
            self.send_mouse(
                MouseInput {
                    action: MouseAction::Move,
                    button: event
                        .pressed_button
                        .and_then(terminal_button)
                        .unwrap_or(TerminalMouseButton::None),
                    column: cell.column,
                    row: cell.row,
                    modifiers: terminal_modifiers(event.modifiers),
                },
                cx,
            );
        }
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        if let (Some(cell), Some(button)) =
            (self.cell_at(event.position), terminal_button(event.button))
            && self.send_mouse(
                MouseInput {
                    action: MouseAction::Release,
                    button,
                    column: cell.column,
                    row: cell.row,
                    modifiers: terminal_modifiers(event.modifiers),
                },
                cx,
            )
        {
            cx.notify();
        }
        if self.selecting && event.button == MouseButton::Left {
            // Release has no viewport token and must stop autoscroll even while
            // a newer frame is being prepared or process input is unavailable.
            self.drag_selection(event.position, event.modifiers.alt, cx);
            self.selecting = false;
            self.last_selection_drag = None;
            cx.emit(TerminalViewEvent::Selection(SelectionCommand::Release));
            cx.notify();
        }
    }

    pub(crate) fn is_composing(&self) -> bool {
        !self.marked_text.is_empty()
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.input_enabled {
            cx.stop_propagation();
            return;
        }
        self.reset_cursor_blink(cx);
        // GPUI dispatches matching application keybindings before this raw
        // listener; unhandled keys belong to the terminal backend.
        let (input, _) = super::platform_input::shortcut_input(event);
        let modifiers = input.modifiers;
        let plain = !modifiers.control && !modifiers.alt && !modifiers.super_key;
        let printable = input.text.as_ref().is_some_and(|text| {
            !text.is_empty() && text.chars().all(|character| !character.is_control())
        });
        if plain && (printable || !self.marked_text.is_empty()) {
            // GPUI delivers committed text through EntityInputHandler. Emitting
            // here as well would duplicate printable keys and break IME input.
            return;
        }
        self.clear_selection(cx);
        cx.emit(TerminalViewEvent::Key(input));
        cx.stop_propagation();
    }

    fn scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        // Scrollback and selection belong to the local display replica. Disabling
        // process input must not disable browsing or copying already shown text.
        let height = self.theme.terminal.cell_height;
        let delta = event.delta.pixel_delta(px(height));
        self.scroll_remainder += vertical_wheel_delta(
            f32::from(delta.x),
            f32::from(delta.y),
            event.modifiers.shift,
        ) / height;
        let lines = bounded_scroll(self.scroll_remainder);
        if lines != 0 {
            // The bounded range is exactly representable by f32.
            self.scroll_remainder -= f32::from(lines);
            if let Some(cell) = self.cell_at(event.position) {
                cx.emit(TerminalViewEvent::Wheel {
                    lines: i32::from(lines),
                    column: cell.column,
                    row: cell.row,
                    modifiers: Modifiers {
                        shift: event.modifiers.shift,
                        control: event.modifiers.control,
                        alt: event.modifiers.alt,
                        super_key: event.modifiers.platform,
                    },
                });
            }
            cx.notify();
        }
        cx.stop_propagation();
    }

    fn advance_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let metrics = self.theme.terminal;
        if self
            .preparation
            .as_ref()
            .is_some_and(|pending| !pending.compatible(&self.frame, metrics))
        {
            self.preparation = None;
        }
        if self
            .displayed_frame
            .as_ref()
            .is_some_and(|frame| frame.size != self.frame.size)
            || self.surface_cache.metrics_changed(metrics)
        {
            self.displayed_frame = None;
        }
        if self.surface_cache.matches_rows(&self.frame, metrics) {
            // Cursor-only updates require no shaping or partial-screen staging.
            self.displayed_frame = Some(Arc::clone(&self.frame));
            self.preparation = None;
            return;
        }
        let pending = self.preparation.get_or_insert_with(|| {
            TerminalSurfacePreparation::new(&self.surface_cache, Arc::clone(&self.frame), metrics)
        });
        if pending.advance(window)
            && let Some(pending) = self.preparation.take()
        {
            let (cache, frame) = pending.finish();
            self.surface_cache = cache;
            self.displayed_frame = Some(frame);
        }
        if self.preparation.is_some()
            || self
                .displayed_frame
                .as_ref()
                .is_none_or(|frame| !Arc::ptr_eq(frame, &self.frame))
        {
            let view = cx.entity().downgrade();
            window.on_next_frame(move |_, cx| {
                let _ = view.update(cx, |_, cx| cx.notify());
            });
        }
    }

    fn annotation_overlay(&self) -> Option<impl IntoElement + use<>> {
        let frame = self.displayed_frame.as_ref()?;
        if !presentation_current(frame, &self.frame, &self.annotations) {
            return None;
        }
        let metrics = self.theme.terminal;
        let selection = span_rectangles(&self.annotations.selection, frame.size, metrics);
        let (matches, selected_match) = self.annotations.search.as_ref().map_or_else(
            || (Vec::new(), Vec::new()),
            |search| {
                (
                    span_rectangles(&search.matches, frame.size, metrics),
                    span_rectangles(&search.selected_match, frame.size, metrics),
                )
            },
        );
        if selection.is_empty() && matches.is_empty() && selected_match.is_empty() {
            return None;
        }
        let accent = self.theme.colors.accent;
        Some(
            canvas(
                |_, _, _| (),
                move |bounds, (), window, _| {
                    // Different opacity distinguishes ordinary search hits,
                    // the selected search hit, and the user clipboard selection.
                    for (highlights, color) in [
                        (matches, rgba((accent << 8) | 0x38)),
                        (selected_match, rgba((accent << 8) | 0x90)),
                        (selection, rgba((accent << 8) | 0x66)),
                    ] {
                        for highlight in highlights {
                            window.paint_quad(fill(
                                Bounds::new(bounds.origin + highlight.origin, highlight.size),
                                color,
                            ));
                        }
                    }
                },
            )
            .absolute()
            .inset_0()
            .size_full(),
        )
    }

    fn input_canvas(&self, metrics_changed: bool, cx: &Context<Self>) -> impl IntoElement + use<> {
        let viewport = cx.entity();
        let bounds_view = viewport.downgrade();
        let previous_bounds = self.bounds;
        let needs_resize = self.last_resize.is_none();
        let input_focus = self.focus.clone();
        let dragging = self.selecting || self.pointer.dragging();
        let move_view = viewport.downgrade();
        let release_view = viewport.downgrade();
        canvas(
            move |bounds, _, cx| {
                if metrics_changed || needs_resize || previous_bounds != Some(bounds) {
                    cx.defer(move |cx| {
                        let _ = bounds_view.update(cx, |view, cx| view.update_bounds(bounds, cx));
                    });
                }
            },
            move |bounds, (), window, cx| {
                if dragging {
                    // Div move listeners stop at their hitbox. Capture the
                    // active drag outside it; local selection retains fractional
                    // coordinates so its owner can drive history autoscroll.
                    window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                        if phase == DispatchPhase::Capture {
                            let _ =
                                move_view.update(cx, |view, cx| view.mouse_move(event, window, cx));
                        }
                    });
                    window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                        if phase == DispatchPhase::Capture {
                            let _ = release_view.update(cx, |view, cx| view.mouse_up(event, cx));
                        }
                    });
                }
                window.handle_input(&input_focus, ElementInputHandler::new(bounds, viewport), cx);
            },
        )
        // Explicit edges keep measurement/input coordinates at the viewport's
        // origin; auto-positioning would place this after the text canvas.
        .absolute()
        .inset_0()
        .size_full()
    }
}

// GPUI's X11 backend maps Shift-wheel onto x. Terminal Shift-wheel explicitly
// means local vertical history, while ordinary horizontal scrolling stays ignored.
fn vertical_wheel_delta(x: f32, y: f32, shift: bool) -> f32 {
    if shift && y == 0.0 { x } else { y }
}

impl Drop for TerminalView {
    fn drop(&mut self) {
        // Invalidate any queued wake before cancelling the retained task.
        self.stop_blink_task();
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
        self.image_window = Some(window.window_handle());
        let metrics_changed = self.measure(window);
        self.advance_surface(window, cx);
        self.report_focus(window, cx);
        self.observe_input_window(window, cx);
        let mut root = div()
            .id("terminal-viewport")
            .relative()
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .cursor(CursorStyle::IBeam)
            .track_focus(&self.focus)
            .bg(rgb(self
                .displayed_frame
                .as_ref()
                .map_or(self.frame.background, |frame| frame.background)
                .to_hex()))
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_scroll_wheel(cx.listener(Self::scroll));
        for button in [MouseButton::Left, MouseButton::Middle, MouseButton::Right] {
            root = root
                .on_mouse_down(button, cx.listener(Self::mouse_down))
                .on_mouse_up(
                    button,
                    cx.listener(|view, event, _, cx| view.mouse_up(event, cx)),
                )
                .on_mouse_up_out(
                    button,
                    cx.listener(|view, event, _, cx| view.mouse_up(event, cx)),
                );
        }
        if let Some(frame) = &self.displayed_frame {
            root = root.child(terminal_surface(
                &mut self.surface_cache,
                frame,
                self.desired_focus,
                self.animation.phase,
                self.theme,
                window,
            ));
        }
        if let Some(warning) = &self.frame.graphics_warning {
            root = root.child(
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .text_size(px(11.))
                    .bg(rgb(self.theme.colors.surface))
                    .text_color(rgb(self.theme.colors.muted))
                    .child(warning.clone()),
            );
        }
        root = root.when_some(self.annotation_overlay(), |root, overlay| {
            root.child(overlay)
        });
        if !self.marked_text.is_empty() && self.displayed_frame.is_some() {
            let cursor = self.displayed_frame.as_ref().and_then(|frame| frame.cursor);
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
        root.child(self.input_canvas(metrics_changed, cx))
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
        if self.input_enabled && !text.is_empty() {
            self.reset_cursor_blink(cx);
            self.clear_selection(cx);
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
        if !self.input_enabled {
            return;
        }
        let repaint = !self.marked_text.is_empty();
        self.marked_text.clear();
        self.marked_selection = 0..0;
        self.clear_selection(cx);
        if !text.is_empty() {
            self.reset_cursor_blink(cx);
            cx.emit(TerminalViewEvent::Text(text.to_owned()));
        }
        // Ordinary text commits do not alter visible terminal cells. Redraw
        // when the shell echoes, rather than painting a redundant pre-echo frame.
        if repaint {
            cx.notify();
        }
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        selection: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.input_enabled {
            return;
        }
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
        let cursor = self.displayed_frame.as_ref()?.cursor?;
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

/// Pointer and font metrics use logical pixels. Device scale only belongs in
/// the PTY cell-pixel dimensions sent by `update_bounds`, never in hit testing.
fn cell_point(
    position: Point<Pixels>,
    origin: Point<Pixels>,
    grid: GridSize,
    metrics: TerminalMetrics,
) -> CellPoint {
    let local = position - origin;
    CellPoint {
        column: bounded_u16_zero(f32::from(local.x) / metrics.cell_width).min(grid.columns() - 1),
        row: bounded_u16_zero(f32::from(local.y) / metrics.cell_height).min(grid.rows() - 1),
    }
}

fn presentation_current(
    displayed: &Arc<TerminalFrame>,
    current: &Arc<TerminalFrame>,
    annotations: &TerminalAnnotations,
) -> bool {
    annotations.viewport_token != 0 && Arc::ptr_eq(displayed, current)
}

fn fractional_cell_point(
    position: Point<Pixels>,
    origin: Point<Pixels>,
    metrics: TerminalMetrics,
) -> Option<(f32, f32)> {
    let local = position - origin;
    let column = f32::from(local.x) / metrics.cell_width;
    let row = f32::from(local.y) / metrics.cell_height;
    (column.is_finite() && row.is_finite()).then_some((column, row))
}

// Capture and hitbox listeners can see the same event, and a new frame can
// arrive between press/release. Neither should manufacture a native drag.
fn same_drag_position(previous: SelectionCommand, next: SelectionCommand) -> bool {
    match (previous, next) {
        (
            SelectionCommand::Drag {
                column: left_column,
                row: left_row,
                rectangle: left_rectangle,
                ..
            },
            SelectionCommand::Drag {
                column: right_column,
                row: right_row,
                rectangle: right_rectangle,
                ..
            },
        ) => {
            left_column.to_bits() == right_column.to_bits()
                && left_row.to_bits() == right_row.to_bits()
                && left_rectangle == right_rectangle
        }
        _ => false,
    }
}

fn span_rectangles(
    spans: &[CellSpan],
    grid: GridSize,
    metrics: TerminalMetrics,
) -> Vec<Bounds<Pixels>> {
    spans
        .iter()
        .filter_map(|span| {
            let first = span.start.min(grid.columns());
            let end = span.end.min(grid.columns());
            if span.row >= grid.rows() || first >= end {
                return None;
            }
            Some(Bounds::new(
                point(
                    px(f32::from(first) * metrics.cell_width),
                    px(f32::from(span.row) * metrics.cell_height),
                ),
                size(
                    px(f32::from(end - first) * metrics.cell_width),
                    px(metrics.cell_height),
                ),
            ))
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shift_wheel_recovers_x11_vertical_history_without_repurposing_normal_horizontal_scroll() {
        assert_eq!(vertical_wheel_delta(48., 0., true), 48.);
        assert_eq!(vertical_wheel_delta(48., 0., false), 0.);
        assert_eq!(vertical_wheel_delta(48., -24., true), -24.);
        assert_eq!(vertical_wheel_delta(0., -24., false), -24.);
    }

    #[test]
    fn hidden_terminal_reannounces_latest_focus_when_input_becomes_ready() {
        let mut reported = Some(true);
        // Losing focus offline cannot send a report yet, or latch it as sent.
        assert_eq!(take_focus_report(false, false, &mut reported), None);
        assert_eq!(reported, Some(true));
        assert_eq!(take_focus_report(true, false, &mut reported), Some(false));
        assert_eq!(take_focus_report(true, false, &mut reported), None);
        // A fresh connection must receive the state even if focus did not change.
        reported = None;
        assert_eq!(take_focus_report(true, false, &mut reported), Some(false));
        assert_eq!(take_focus_report(true, true, &mut reported), Some(true));
    }

    fn mouse(action: MouseAction, button: TerminalMouseButton) -> MouseInput {
        MouseInput {
            action,
            button,
            column: 4,
            row: 2,
            modifiers: Modifiers::default(),
        }
    }

    #[test]
    fn application_press_still_releases_after_shift_is_held() {
        let mut routing = PointerRouting::default();
        let press = mouse(MouseAction::Press, TerminalMouseButton::Left);
        assert!(routing.route(press, true, MouseTracking::Any));
        let mut release = MouseInput {
            action: MouseAction::Release,
            ..press
        };
        release.modifiers.shift = true;
        assert!(routing.route(release, true, MouseTracking::Any));
        assert!(!routing.route(release, true, MouseTracking::Any));
        assert!(!routing.dragging());
    }

    #[test]
    fn local_shift_selection_never_becomes_an_unmatched_application_drag() {
        let mut routing = PointerRouting::default();
        let mut press = mouse(MouseAction::Press, TerminalMouseButton::Left);
        press.modifiers.shift = true;
        assert!(!routing.route(press, true, MouseTracking::Any));
        let movement = mouse(MouseAction::Move, TerminalMouseButton::Left);
        assert!(!routing.route(movement, true, MouseTracking::Any));
        assert!(!routing.route(
            MouseInput {
                action: MouseAction::Release,
                ..movement
            },
            true,
            MouseTracking::Any
        ));
    }

    #[test]
    fn mouse_routing_requires_tracking_and_available_input() {
        for (enabled, reporting) in [
            (false, MouseTracking::None),
            (false, MouseTracking::Any),
            (true, MouseTracking::None),
        ] {
            let mut routing = PointerRouting::default();
            assert!(!routing.route(
                mouse(MouseAction::Press, TerminalMouseButton::Right),
                enabled,
                reporting
            ));
            assert!(!routing.route(
                mouse(MouseAction::Move, TerminalMouseButton::None),
                enabled,
                reporting
            ));
            assert!(!routing.dragging());
        }
    }

    #[test]
    fn motion_coalesces_same_cell_but_keeps_modifier_and_button_changes() {
        let mut routing = PointerRouting::default();
        let movement = mouse(MouseAction::Move, TerminalMouseButton::None);
        assert!(routing.route(movement, true, MouseTracking::Any));
        assert!(!routing.route(movement, true, MouseTracking::Any));
        let mut modified = movement;
        modified.modifiers.control = true;
        assert!(routing.route(modified, true, MouseTracking::Any));
        assert!(routing.route(
            mouse(MouseAction::Press, TerminalMouseButton::Middle),
            true,
            MouseTracking::Any
        ));
        assert!(routing.route(
            mouse(MouseAction::Move, TerminalMouseButton::Middle),
            true,
            MouseTracking::Any
        ));
    }

    #[test]
    fn losing_focus_releases_every_application_button_at_latest_position() {
        let mut routing = PointerRouting::default();
        assert!(routing.route(
            mouse(MouseAction::Press, TerminalMouseButton::Left),
            true,
            MouseTracking::Any
        ));
        assert!(routing.route(
            mouse(MouseAction::Press, TerminalMouseButton::Right),
            true,
            MouseTracking::Any
        ));
        let movement = MouseInput {
            column: 12,
            row: 9,
            ..mouse(MouseAction::Move, TerminalMouseButton::Right)
        };
        assert!(routing.route(movement, true, MouseTracking::Any));
        let releases: Vec<_> = routing.release_all().collect();
        assert_eq!(releases.len(), 2);
        assert!(
            releases
                .iter()
                .all(|input| input.action == MouseAction::Release
                    && input.column == 12
                    && input.row == 9)
        );
        assert!(!routing.dragging());
        assert_eq!(routing.release_all().count(), 0);
    }

    #[test]
    fn press_tracking_never_queues_hover_or_drag_motion() {
        let mut routing = PointerRouting::default();
        assert!(!routing.route(
            mouse(MouseAction::Move, TerminalMouseButton::None),
            true,
            MouseTracking::Press
        ));
        assert!(routing.route(
            mouse(MouseAction::Press, TerminalMouseButton::Left),
            true,
            MouseTracking::Press
        ));
        assert!(!routing.route(
            mouse(MouseAction::Move, TerminalMouseButton::Left),
            true,
            MouseTracking::Press
        ));
        assert!(routing.route(
            mouse(MouseAction::Release, TerminalMouseButton::Left),
            true,
            MouseTracking::Press
        ));
    }

    #[test]
    fn drag_tracking_requires_a_previously_reported_button() {
        let mut routing = PointerRouting::default();
        assert!(!routing.route(
            mouse(MouseAction::Move, TerminalMouseButton::None),
            true,
            MouseTracking::Drag
        ));
        assert!(!routing.route(
            mouse(MouseAction::Move, TerminalMouseButton::Left),
            true,
            MouseTracking::Drag
        ));
        assert!(routing.route(
            mouse(MouseAction::Press, TerminalMouseButton::Left),
            true,
            MouseTracking::Drag
        ));
        assert!(routing.route(
            mouse(MouseAction::Move, TerminalMouseButton::Left),
            true,
            MouseTracking::Drag
        ));
        // Changing a nonzero tracking mode keeps this matching release valid.
        assert!(routing.route(
            mouse(MouseAction::Release, TerminalMouseButton::Left),
            true,
            MouseTracking::Press
        ));
    }

    fn metrics() -> TerminalMetrics {
        TerminalMetrics {
            font_family: "test",
            font_size: 13.0,
            cell_width: 8.0,
            cell_height: 16.0,
        }
    }

    #[test]
    fn application_mouse_coordinates_clamp_to_visible_cells()
    -> Result<(), crate::core::InvalidGridSize> {
        let grid = GridSize::new(80, 24)?;
        let origin = point(px(100.0), px(200.0));
        assert_eq!(
            cell_point(point(px(112.0), px(242.0)), origin, grid, metrics()),
            CellPoint { row: 2, column: 1 }
        );
        assert_eq!(
            cell_point(point(px(99.0), px(199.0)), origin, grid, metrics()),
            CellPoint { row: 0, column: 0 }
        );
        assert_eq!(
            cell_point(point(px(10000.0), px(10000.0)), origin, grid, metrics()),
            CellPoint {
                row: 23,
                column: 79
            }
        );
        Ok(())
    }

    #[test]
    fn native_spans_clip_to_visible_cells_and_keep_exclusive_end()
    -> Result<(), crate::core::InvalidGridSize> {
        assert_eq!(
            span_rectangles(
                &[
                    CellSpan {
                        row: 0,
                        start: 7,
                        end: 20
                    },
                    CellSpan {
                        row: 2,
                        start: 0,
                        end: 2
                    },
                    CellSpan {
                        row: 3,
                        start: 0,
                        end: 10
                    },
                    CellSpan {
                        row: 1,
                        start: 8,
                        end: 7
                    },
                    CellSpan {
                        row: 1,
                        start: 10,
                        end: 11
                    },
                ],
                GridSize::new(10, 3)?,
                metrics()
            ),
            vec![
                Bounds::new(point(px(56.0), px(0.0)), size(px(24.0), px(16.0))),
                Bounds::new(point(px(0.0), px(32.0)), size(px(16.0), px(16.0))),
            ]
        );
        Ok(())
    }

    #[test]
    fn local_drag_coordinates_keep_fractional_and_outside_viewport_positions() {
        let origin = point(px(100.0), px(200.0));
        assert_eq!(
            fractional_cell_point(point(px(112.0), px(242.0)), origin, metrics())
                .map(|(column, row)| (column.to_bits(), row.to_bits())),
            Some((1.5_f32.to_bits(), 2.625_f32.to_bits()))
        );
        assert_eq!(
            fractional_cell_point(point(px(96.0), px(192.0)), origin, metrics())
                .map(|(column, row)| (column.to_bits(), row.to_bits())),
            Some(((-0.5_f32).to_bits(), (-0.5_f32).to_bits()))
        );
        assert_eq!(
            fractional_cell_point(point(px(900.0), px(1000.0)), origin, metrics())
                .map(|(column, row)| (column.to_bits(), row.to_bits())),
            Some((100.0_f32.to_bits(), 50.0_f32.to_bits()))
        );
        assert_eq!(
            fractional_cell_point(point(px(f32::NAN), px(0.0)), origin, metrics()),
            None
        );
    }
    #[test]
    fn annotations_only_target_the_current_displayed_frame()
    -> Result<(), crate::core::InvalidGridSize> {
        let frame = Arc::new(TerminalFrame {
            images: Arc::default(),
            graphics_warning: None,
            size: GridSize::new(2, 1)?,
            rows: Vec::new(),
            cursor: None,
            foreground: crate::core::frame::Rgb::default(),
            background: crate::core::frame::Rgb::default(),
        });
        let newer = Arc::new(frame.as_ref().clone());
        let annotations = TerminalAnnotations {
            viewport_token: 1,
            ..Default::default()
        };
        assert!(presentation_current(&frame, &frame, &annotations));
        // An old frame may still be visible while new glyph rows are prepared.
        assert!(!presentation_current(&frame, &newer, &annotations));
        assert!(!presentation_current(
            &frame,
            &frame,
            &TerminalAnnotations::default()
        ));
        Ok(())
    }
    #[test]
    fn frame_updates_do_not_turn_click_release_into_a_drag() {
        let press_position = SelectionCommand::Drag {
            column: 2.5,
            row: 3.5,
            rectangle: false,
            viewport_token: 1,
        };
        let same_position = SelectionCommand::Drag {
            column: 2.5,
            row: 3.5,
            rectangle: false,
            viewport_token: 2,
        };
        assert!(same_drag_position(press_position, same_position));
        assert!(!same_drag_position(
            press_position,
            SelectionCommand::Drag {
                column: 2.5,
                row: 3.5,
                rectangle: true,
                viewport_token: 2,
            }
        ));
        assert!(!same_drag_position(
            press_position,
            SelectionCommand::Drag {
                column: 2.75,
                row: 3.5,
                rectangle: false,
                viewport_token: 2,
            }
        ));
    }
    #[test]
    fn blink_demand_stops_for_hidden_inactive_disabled_and_unfocused_cursor_only() {
        let active = BlinkDemand {
            eligibility: BlinkEligibility::Animate,
            focused: true,
            cursor: true,
            text: false,
        };
        assert!(active.needed());
        for (visible, enabled, window_active) in [
            (false, true, true),
            (true, false, true),
            (true, true, false),
        ] {
            let stopped = BlinkDemand {
                eligibility: BlinkEligibility::from_state(visible, enabled, window_active),
                ..active
            };
            assert!(!stopped.needed());
            assert_eq!(
                stopped.phase(Duration::from_millis(750), Duration::from_millis(750)),
                TerminalPaintPhase::default()
            );
        }
        assert!(
            !BlinkDemand {
                focused: false,
                ..active
            }
            .needed()
        );
        assert!(
            !BlinkDemand {
                cursor: false,
                ..active
            }
            .needed()
        );
        let unfocused_text = BlinkDemand {
            focused: false,
            text: true,
            ..active
        };
        assert!(unfocused_text.needed());
        assert!(
            unfocused_text
                .phase(Duration::from_millis(750), Duration::from_millis(750))
                .cursor_visible
        );
    }

    #[test]
    fn text_phase_is_shared_and_cursor_reset_is_independent() {
        let demand = BlinkDemand {
            eligibility: BlinkEligibility::Animate,
            focused: true,
            cursor: true,
            text: true,
        };
        let before = demand.phase(Duration::from_millis(750), Duration::from_millis(750));
        let typing = demand.phase(Duration::from_millis(750), Duration::ZERO);
        assert!(!before.text_visible && !before.cursor_visible);
        assert!(!typing.text_visible && typing.cursor_visible);
        assert_eq!(
            demand.delay(Duration::from_millis(750), Duration::from_millis(100)),
            Duration::from_millis(250)
        );
        assert_eq!(
            demand.phase(Duration::from_millis(1000), Duration::from_millis(1000)),
            TerminalPaintPhase::default()
        );
        assert_eq!(
            blink_boundary_delay(Duration::from_millis(500)),
            Duration::from_millis(500)
        );
        assert_eq!(
            blink_boundary_delay(Duration::from_millis(499)),
            Duration::from_millis(1)
        );
    }
}
