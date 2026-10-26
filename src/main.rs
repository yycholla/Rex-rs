use gpui::{
    App, AppContext, Application, Bounds, Focusable, TitlebarOptions, WindowBounds, WindowOptions,
    px, size,
};
use rex_rs::ui::{WorkspaceView, actions};

fn main() {
    Application::new().run(|cx: &mut App| {
        actions::register(cx);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        if let Err(error) = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                app_id: Some("rex-rs".into()),
                titlebar: Some(TitlebarOptions {
                    title: Some("Rex-rs".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                window.set_window_title("Rex-rs");
                let view = cx.new(WorkspaceView::new);
                view.focus_handle(cx).focus(window);
                view
            },
        ) {
            eprintln!("Could not open Rex-rs: {error:#}");
            cx.quit();
        }
        cx.activate(true);
    });
}
