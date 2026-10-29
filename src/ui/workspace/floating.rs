//! Server-owned floating layers with local drag previews and one commit on release.
use super::{WindowView, WorkspaceView};
use crate::{
    core::{Command, SessionId, WindowId},
    rex::workspace::LayerView,
    ui::components::{ButtonVariant, button},
};
use gpui::{
    AnyElement, Bounds, Context, CursorStyle, MouseButton, MouseMoveEvent, MouseUpEvent, Pixels,
    Point, SharedString, Window, canvas, div, prelude::*, px, relative, rgb,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DragKind {
    Move,
    Resize,
}

pub(super) struct LayerDrag {
    session: SessionId,
    window: WindowId,
    layer: String,
    instance: String,
    generation: u64,
    origin: Point<Pixels>,
    container: Bounds<Pixels>,
    initial: [f64; 4],
    preview: [f64; 4],
    kind: DragKind,
}

impl WorkspaceView {
    pub(super) fn layered_window(&self, tab: &WindowView, cx: &Context<Self>) -> AnyElement {
        if let Some(pane) = tab.zoomed_pane() {
            return self.pane(pane, true, false, cx);
        }
        let Some(session) = self.runtime.state().active_session() else {
            return div().into_any_element();
        };
        let session_id = session.id();
        let container_view = cx.entity().downgrade();
        let mut stack = div()
            .relative()
            .size_full()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .bg(rgb(self.theme.colors.surface));
        for layer in &tab.layers {
            if layer.is_floating() {
                stack = stack.child(self.floating_layer(session_id, tab.id(), layer, cx));
            } else if let Some(layout) = &layer.layout {
                stack = stack.child(div().absolute().inset_0().size_full().child(self.layout(
                    layout,
                    self.active_pane(),
                    layout.panes().len() > 1,
                    cx,
                )));
            }
        }
        stack
            .child(
                canvas(
                    move |bounds, _, cx| {
                        cx.defer(move |cx| {
                            let _ = container_view.update(cx, |view, _| {
                                view.floating_bounds = Some(bounds);
                            });
                        });
                    },
                    |_, (), _, _| {},
                )
                .absolute()
                .inset_0()
                .size_full(),
            )
            .into_any_element()
    }

    fn floating_header(
        &self,
        session: SessionId,
        window_id: WindowId,
        layer: &LayerView,
        cx: &Context<Self>,
    ) -> gpui::Div {
        let theme = self.theme;
        let move_id = layer.remote_id.clone();
        div()
            .flex()
            .items_center()
            .flex_shrink_0()
            .h(px(28.))
            .bg(rgb(theme.colors.raised))
            .child(
                div()
                    .id(SharedString::from(format!(
                        "layer-move-{}",
                        layer.remote_id
                    )))
                    .flex_1()
                    .h_full()
                    .flex()
                    .items_center()
                    .px_2()
                    .text_size(px(11.))
                    .text_color(rgb(theme.colors.muted))
                    .cursor(CursorStyle::OpenHand)
                    .child("Floating pane · drag to move")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, event: &gpui::MouseDownEvent, _, cx| {
                            view.begin_layer_drag(
                                session,
                                window_id,
                                move_id.clone(),
                                DragKind::Move,
                                event.position,
                                cx,
                            );
                            cx.stop_propagation();
                        }),
                    ),
            )
            .children(
                [
                    (
                        "raise",
                        "Front",
                        Command::RaiseLayer {
                            session,
                            layer: layer.remote_id.clone(),
                        },
                    ),
                    (
                        "lower",
                        "Back",
                        Command::LowerLayer {
                            session,
                            layer: layer.remote_id.clone(),
                        },
                    ),
                    (
                        "close",
                        "Close",
                        Command::CloseLayer {
                            session,
                            layer: layer.remote_id.clone(),
                        },
                    ),
                ]
                .into_iter()
                .map(|(id, label, command)| {
                    button(
                        SharedString::from(format!("layer-{id}-{}", layer.remote_id)),
                        label,
                        ButtonVariant::Quiet,
                        theme,
                    )
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.dispatch(command.clone(), window, cx);
                        cx.stop_propagation();
                    }))
                }),
            )
    }

    #[expect(
        clippy::cast_possible_truncation,
        reason = "Server layer fractions are finite and validated inside [0, 1]; GPUI uses f32 lengths."
    )]
    fn floating_layer(
        &self,
        session: SessionId,
        window_id: WindowId,
        layer: &LayerView,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let bounds = self
            .layer_drag
            .as_ref()
            .filter(|drag| drag.layer == layer.remote_id && self.layer_drag_valid(drag))
            .map_or(layer.bounds, |drag| drag.preview);
        let resize_id = layer.remote_id.clone();
        let header = self.floating_header(session, window_id, layer, cx);
        let body = layer.layout.as_ref().map_or_else(
            || {
                div()
                    .p_3()
                    .text_color(rgb(theme.colors.muted))
                    .child(
                        layer
                            .unsupported
                            .clone()
                            .unwrap_or_else(|| "Empty layer".into()),
                    )
                    .into_any_element()
            },
            |layout| self.layout(layout, self.active_pane(), layout.panes().len() > 1, cx),
        );
        div()
            .id(SharedString::from(format!("layer-{}", layer.remote_id)))
            .absolute()
            .left(relative(bounds[0] as f32))
            .top(relative(bounds[1] as f32))
            .w(relative(bounds[2] as f32))
            .h(relative(bounds[3] as f32))
            .flex()
            .flex_col()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .occlude()
            .border_1()
            .border_color(rgb(theme.colors.border))
            .bg(rgb(theme.colors.surface))
            .child(header)
            .child(div().relative().flex_1().min_w_0().min_h_0().child(body))
            .child(
                div()
                    .id(SharedString::from(format!(
                        "layer-resize-{}",
                        layer.remote_id
                    )))
                    .absolute()
                    .right_0()
                    .bottom_0()
                    .w(px(16.))
                    .h(px(16.))
                    .occlude()
                    .cursor(CursorStyle::ResizeUpLeftDownRight)
                    .bg(rgb(theme.colors.border))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, event: &gpui::MouseDownEvent, _, cx| {
                            view.begin_layer_drag(
                                session,
                                window_id,
                                resize_id.clone(),
                                DragKind::Resize,
                                event.position,
                                cx,
                            );
                            cx.stop_propagation();
                        }),
                    ),
            )
            .into_any_element()
    }

    fn begin_layer_drag(
        &mut self,
        session: SessionId,
        window_id: WindowId,
        layer: String,
        kind: DragKind,
        origin: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if !self.runtime.connected() || self.has_modal() || self.overview_session().is_some() {
            return;
        }
        let Some(container) = self.floating_bounds else {
            return;
        };
        if container.size.width <= px(0.) || container.size.height <= px(0.) {
            return;
        }
        let Some(instance) = self.runtime.server_id().map(str::to_owned) else {
            return;
        };
        let Some(current) = self
            .runtime
            .state()
            .active_session()
            .filter(|entry| entry.id() == session)
            .and_then(|entry| entry.active_window())
            .filter(|entry| entry.id() == window_id)
            .and_then(|entry| {
                entry
                    .layers
                    .iter()
                    .find(|entry| entry.remote_id == layer && entry.is_floating())
            })
        else {
            return;
        };
        self.split_drag = None;
        self.layer_drag = Some(LayerDrag {
            session,
            window: window_id,
            layer,
            instance,
            generation: self.runtime.connection_generation(),
            origin,
            container,
            initial: current.bounds,
            preview: current.bounds,
            kind,
        });
        cx.notify();
    }

    fn layer_drag_valid(&self, drag: &LayerDrag) -> bool {
        self.runtime.connected()
            && self.overview_session().is_none()
            && !self.has_modal()
            && self.floating_bounds == Some(drag.container)
            && self.runtime.server_id() == Some(drag.instance.as_str())
            && self.runtime.connection_generation() == drag.generation
            && self
                .runtime
                .state()
                .active_session()
                .filter(|session| session.id() == drag.session)
                .and_then(|session| session.active_window())
                .filter(|window| window.id() == drag.window && window.zoomed_pane().is_none())
                .is_some_and(|window| {
                    window.layers.iter().any(|layer| {
                        layer.remote_id == drag.layer
                            && layer.is_floating()
                            && layer.bounds.map(f64::to_bits) == drag.initial.map(f64::to_bits)
                    })
                })
    }

    pub(super) fn move_layer_drag(
        &mut self,
        event: &MouseMoveEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.dragging() {
            self.preview_layer_drag(event.position, cx);
        }
    }

    fn preview_layer_drag(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(drag) = &self.layer_drag else {
            return;
        };
        if !self.layer_drag_valid(drag) {
            self.layer_drag = None;
            cx.notify();
            return;
        }
        let delta = [
            f64::from(f32::from(position.x - drag.origin.x) / f32::from(drag.container.size.width)),
            f64::from(
                f32::from(position.y - drag.origin.y) / f32::from(drag.container.size.height),
            ),
        ];
        let preview = drag_bounds(drag.initial, delta, drag.kind);
        if let Some(drag) = &mut self.layer_drag {
            drag.preview = preview;
        }
        cx.notify();
    }

    pub(super) fn finish_layer_drag(
        &mut self,
        event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left {
            return;
        }
        self.preview_layer_drag(event.position, cx);
        let Some(drag) = self.layer_drag.take() else {
            return;
        };
        if self.layer_drag_valid(&drag)
            && drag.preview.map(f64::to_bits) != drag.initial.map(f64::to_bits)
        {
            self.dispatch(
                Command::ResizeLayer {
                    session: drag.session,
                    layer: drag.layer,
                    bounds: drag.preview,
                },
                window,
                cx,
            );
        }
        cx.notify();
    }
}

fn drag_bounds(initial: [f64; 4], delta: [f64; 2], kind: DragKind) -> [f64; 4] {
    if !delta.iter().all(|value| value.is_finite()) {
        return initial;
    }
    let [mut x, mut y, mut width, mut height] = initial;
    match kind {
        DragKind::Move => {
            x = (x + delta[0]).clamp(0.0, 1.0 - width);
            y = (y + delta[1]).clamp(0.0, 1.0 - height);
        }
        DragKind::Resize => {
            width = (width + delta[0]).clamp(0.05_f64.min(1.0 - x), 1.0 - x);
            height = (height + delta[1]).clamp(0.05_f64.min(1.0 - y), 1.0 - y);
        }
    }
    [x, y, width, height]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drag_move_preserves_size_and_clamps_to_window_edges() {
        let initial = [0.2, 0.3, 0.6, 0.4];
        assert_eq!(
            drag_bounds(initial, [-2., -2.], DragKind::Move),
            [0., 0., 0.6, 0.4]
        );
        assert_eq!(
            drag_bounds(initial, [2., 2.], DragKind::Move),
            [0.4, 0.6, 0.6, 0.4]
        );
        assert_eq!(
            drag_bounds(initial, [f64::NAN, 0.], DragKind::Move),
            initial
        );
    }

    #[test]
    fn drag_resize_preserves_origin_and_stays_positive_inside_window() {
        let initial = [0.2, 0.3, 0.6, 0.4];
        assert_eq!(
            drag_bounds(initial, [2., 2.], DragKind::Resize),
            [0.2, 0.3, 0.8, 0.7]
        );
        let small = drag_bounds(initial, [-2., -2.], DragKind::Resize);
        assert_eq!(small, [0.2, 0.3, 0.05, 0.05]);
        assert_eq!(
            drag_bounds(initial, [0., f64::INFINITY], DragKind::Resize),
            initial
        );
    }
}
