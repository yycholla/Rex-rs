use gpui::{
    App, AppContext, Application, Bounds, Focusable, TitlebarOptions, WindowBounds, WindowOptions,
    px, size,
};
use rex_rs::ui::{Assets, WorkspaceView, actions};
use std::io::Read as _;

fn main() {
    let mut endpoint = None;
    let mut scripts = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--endpoint" => {
                let Some(value) = args.next().filter(|value| value.starts_with("unix:///")) else {
                    eprintln!("--endpoint requires an absolute unix:// socket address");
                    std::process::exit(2);
                };
                endpoint = Some(value);
            }
            "--client-script" => {
                let Some(path) = args.next() else {
                    eprintln!("--client-script requires a Lua file path");
                    std::process::exit(2);
                };
                let source = match std::fs::File::open(&path).and_then(|file| {
                    let mut source = String::new();
                    file.take(512 * 1024 + 1).read_to_string(&mut source)?;
                    Ok(source)
                }) {
                    Ok(source) if source.len() <= 512 * 1024 => source,
                    Ok(_) => {
                        eprintln!("Client script exceeds 512 KiB: {path}");
                        std::process::exit(2);
                    }
                    Err(error) => {
                        eprintln!("Cannot read client script {path}: {error}");
                        std::process::exit(2);
                    }
                };
                scripts.push(rex_rs::rex::ClientScript { name: path, source });
            }
            "--help" | "-h" => {
                println!(
                    "Usage: rex-rs [--endpoint unix:///path/to/rex/server.sock] [--client-script path.lua]...\n\nConnects to an existing Rex server. REX_RS_ENDPOINT and REX_SERVER also select the endpoint."
                );
                return;
            }
            _ => {
                eprintln!("Unknown option: {argument}. Use --help for usage.");
                std::process::exit(2);
            }
        }
    }
    Application::new()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
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
                    let view = cx.new(|cx| {
                        WorkspaceView::new_with_scripts(endpoint.clone(), scripts.clone(), cx)
                    });
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
