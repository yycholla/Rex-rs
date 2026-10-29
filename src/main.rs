use gpui::{
    App, AppContext, Application, Bounds, Focusable, TitlebarOptions, WindowBounds, WindowOptions,
    px, size,
};
use rex_rs::rex::servers::{NamedServer, ServerRoutes};
use rex_rs::ui::{Assets, ServerWorkspaceView, actions};
use std::{collections::BTreeMap, io::Read as _};

const MAX_SERVERS: usize = 16;

fn add_endpoint(endpoints: &mut Vec<String>, value: Option<String>) -> anyhow::Result<()> {
    let value = value.ok_or_else(|| anyhow::anyhow!("--endpoint requires an address"))?;
    let normalized = if rex_rs::rex::ssh::is_ssh_address(&value) {
        rex_rs::rex::ssh::SshTarget::parse(&value)?.canonical()
    } else {
        match rex_rs::rex::endpoint::Endpoint::parse(&value)? {
            rex_rs::rex::endpoint::Endpoint::Unix(path) => {
                let path: std::path::PathBuf = path.components().collect();
                format!("unix://{}", path.display())
            }
            rex_rs::rex::endpoint::Endpoint::Network(url) => url.to_string(),
        }
    };
    anyhow::ensure!(
        !endpoints.contains(&normalized),
        "duplicate Rex endpoint: {normalized}"
    );
    anyhow::ensure!(
        endpoints.len() < MAX_SERVERS,
        "at most {MAX_SERVERS} Rex servers may be opened"
    );
    endpoints.push(normalized);
    Ok(())
}

fn resolve_endpoints(
    mut endpoints: Vec<String>,
    default: impl FnOnce() -> anyhow::Result<String>,
) -> anyhow::Result<Vec<String>> {
    if endpoints.is_empty() {
        add_endpoint(&mut endpoints, Some(default()?))?;
    }
    Ok(endpoints)
}

fn add_named_server(
    endpoints: &mut Vec<String>,
    named: &mut BTreeMap<String, NamedServer>,
    routes: &ServerRoutes,
    value: Option<String>,
) -> anyhow::Result<()> {
    let value = value.ok_or_else(|| anyhow::anyhow!("--server requires NAME=ADDRESS"))?;
    let (name, address) = value
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("--server requires NAME=ADDRESS"))?;
    let mut next = endpoints.clone();
    add_endpoint(&mut next, Some(address.to_owned()))?;
    let server = routes.named(name)?;
    let endpoint = next
        .last()
        .ok_or_else(|| anyhow::anyhow!("missing named endpoint"))?
        .clone();
    named.insert(endpoint, server);
    *endpoints = next;
    Ok(())
}

fn main() {
    let mut endpoints = Vec::new();
    let mut scripts = Vec::new();
    let routes = ServerRoutes::default();
    let mut named = BTreeMap::new();
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--endpoint" => {
                if let Err(error) = add_endpoint(&mut endpoints, args.next()) {
                    eprintln!("Invalid --endpoint: {error:#}");
                    std::process::exit(2);
                }
            }
            "--server" => {
                if let Err(error) =
                    add_named_server(&mut endpoints, &mut named, &routes, args.next())
                {
                    eprintln!("Invalid --server: {error:#}");
                    std::process::exit(2);
                }
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
                    "Usage: rex-rs [--endpoint ADDRESS]... [--server NAME=ADDRESS]... [--client-script path.lua]...\n\nShows connected servers in one workspace. Saved Rex hosts are discovered automatically; use Add server in the app for another connection. Optional endpoints seed the server list (up to 16). Named servers opt into cross-server Lua routing. Client scripts apply to every connection.\nWithout --endpoint or --server, uses REX_RS_ENDPOINT, REX_RS_REX_ENDPOINT, REX_SERVER or the local socket.\nDisconnecting a server keeps its processes running. Quit detaches all connections."
                );
                return;
            }
            _ => {
                eprintln!("Unknown option: {argument}. Use --help for usage.");
                std::process::exit(2);
            }
        }
    }
    let endpoints = match resolve_endpoints(endpoints, rex_rs::rex::transport::default_endpoint) {
        Ok(endpoints) => endpoints,
        Err(error) => {
            eprintln!("Cannot resolve Rex endpoint: {error:#}");
            std::process::exit(2);
        }
    };
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
            let connections = endpoints
                .iter()
                .map(|endpoint| (endpoint.clone(), named.get(endpoint).cloned()))
                .collect();
            if let Err(error) = open_server_window(connections, scripts.clone(), cx) {
                eprintln!("Could not open Rex-rs: {error:#}");
            }
            if cx.windows().is_empty() {
                cx.quit();
            } else {
                cx.activate(true);
            }
        });
}

fn open_server_window(
    endpoints: Vec<(String, Option<NamedServer>)>,
    scripts: Vec<rex_rs::rex::ClientScript>,
    cx: &mut App,
) -> anyhow::Result<()> {
    let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
    cx.open_window(
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
            let view = cx.new(|cx| ServerWorkspaceView::new(endpoints, scripts, window, cx));
            view.focus_handle(cx).focus(window);
            view
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_servers_require_unique_names_and_normalized_addresses() -> anyhow::Result<()> {
        let routes = ServerRoutes::default();
        let mut endpoints = Vec::new();
        let mut named = BTreeMap::new();
        add_named_server(
            &mut endpoints,
            &mut named,
            &routes,
            Some("work=http://EXAMPLE.com:80".into()),
        )?;
        for value in [
            "work=http://other.example",
            "other=ws://example.com/",
            "missing",
            "=http://other.example",
        ] {
            assert!(
                add_named_server(&mut endpoints, &mut named, &routes, Some(value.into())).is_err()
            );
        }
        assert_eq!(endpoints, ["ws://example.com/"]);
        assert_eq!(named.len(), 1);
        Ok(())
    }

    #[test]
    fn explicit_servers_preserve_order_and_bypass_default() -> anyhow::Result<()> {
        let mut endpoints = Vec::new();
        add_endpoint(&mut endpoints, Some("unix:///tmp/first.sock".into()))?;
        add_endpoint(&mut endpoints, Some("https://EXAMPLE.com:443".into()))?;
        let resolved = resolve_endpoints(endpoints, || anyhow::bail!("default must not be read"))?;
        assert_eq!(resolved, ["unix:///tmp/first.sock", "wss://example.com/"]);
        Ok(())
    }

    #[test]
    fn default_is_resolved_once_and_validated() -> anyhow::Result<()> {
        assert_eq!(
            resolve_endpoints(Vec::new(), || Ok("http://localhost:8080".into()))?,
            ["ws://localhost:8080/"]
        );
        assert!(resolve_endpoints(Vec::new(), || Ok("relative.sock".into())).is_err());
        assert!(resolve_endpoints(Vec::new(), || anyhow::bail!("no endpoint")).is_err());
        Ok(())
    }

    #[test]
    fn missing_invalid_and_equivalent_endpoints_do_not_change_selection() -> anyhow::Result<()> {
        let mut endpoints = Vec::new();
        assert!(add_endpoint(&mut endpoints, None).is_err());
        assert!(add_endpoint(&mut endpoints, Some("--client-script".into())).is_err());
        assert!(
            add_endpoint(
                &mut endpoints,
                Some("https://user:secret@example.com".into())
            )
            .is_err()
        );
        add_endpoint(&mut endpoints, Some("http://EXAMPLE.com:80".into()))?;
        assert!(add_endpoint(&mut endpoints, Some("ws://example.com/".into())).is_err());
        add_endpoint(&mut endpoints, Some("unix:///tmp/test.sock".into()))?;
        assert!(add_endpoint(&mut endpoints, Some("unix:///tmp/./test.sock".into())).is_err());
        assert_eq!(endpoints.len(), 2);
        Ok(())
    }

    #[test]
    fn server_limit_is_enforced_without_losing_existing_servers() -> anyhow::Result<()> {
        let mut endpoints = Vec::new();
        for index in 0..MAX_SERVERS {
            add_endpoint(
                &mut endpoints,
                Some(format!("unix:///tmp/server-{index}.sock")),
            )?;
        }
        let previous = endpoints.clone();
        assert!(add_endpoint(&mut endpoints, Some("unix:///tmp/extra.sock".into())).is_err());
        assert_eq!(endpoints, previous);
        Ok(())
    }
}
