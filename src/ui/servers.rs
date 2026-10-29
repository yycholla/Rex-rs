//! One sidebar tree of servers, each owning its sessions, with an independent
//! workspace for every connection.
use super::{
    WorkspaceView,
    components::text_field::{TextField, TextFieldEvent},
    components::{
        ButtonVariant, Icon, ServerHeader, button, empty_state, icon_button, menu_item, menu_panel,
        menu_separator, modal, overlay_footer, overlay_header, server_header, server_state_line,
        sidebar_footer, tooltip,
    },
    theme::Theme,
};
use crate::rex::{
    ClientScript,
    endpoint::Endpoint,
    servers::NamedServer,
    ssh::{SshTarget, SshTunnel, is_ssh_address},
};
use anyhow::{Result, ensure};
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, FontWeight, MouseButton, Render,
    SharedString, Subscription, Task, Window, deferred, div, prelude::*, px, rgb,
};

const MAX_SERVERS: usize = 16;

struct ServerEntry {
    id: usize,
    label: String,
    /// Discovery may replace a label derived from the endpoint, never a chosen one.
    label_chosen: bool,
    endpoint: String,
    named: Option<NamedServer>,
    /// Servers reached over SSH connect through a forwarded local socket.
    ssh: Option<SshTarget>,
    tunnel: Option<SshTunnel>,
    /// The SSH forward being opened before the workspace can connect.
    opening: Option<Task<()>>,
    workspace: Option<Entity<WorkspaceView>>,
    observation: Option<Subscription>,
    connected: bool,
    error: Option<String>,
    expanded: bool,
}

/// What a server's sidebar section shows in place of, or above, its sessions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ServerState<'a> {
    Connected,
    Connecting,
    Failed(&'a str),
    Disconnected,
}

impl ServerEntry {
    fn state(&self) -> ServerState<'_> {
        match (&self.workspace, self.connected, &self.error) {
            (None, ..) if self.opening.is_some() => ServerState::Connecting,
            (Some(_), true, _) => ServerState::Connected,
            // Without a workspace, only a failed SSH forward leaves an error.
            (_, false, Some(error)) => ServerState::Failed(error),
            (None, ..) => ServerState::Disconnected,
            (Some(_), false, None) => ServerState::Connecting,
        }
    }

    /// Where the Rex client connects: the forwarded socket for SSH servers.
    fn connection_endpoint(&self) -> Option<String> {
        match &self.ssh {
            Some(_) => self.tunnel.as_ref().map(SshTunnel::endpoint),
            None => Some(self.endpoint.clone()),
        }
    }
}

struct AddServer {
    label: Entity<TextField>,
    endpoint: Entity<TextField>,
    _subscriptions: Vec<Subscription>,
    error: Option<String>,
}

/// Owns server connections independently of which workspace is currently shown.
pub struct ServerWorkspaceView {
    servers: Vec<ServerEntry>,
    active: Option<usize>,
    next_id: usize,
    scripts: Vec<ClientScript>,
    focus: FocusHandle,
    add: Option<AddServer>,
    /// Server whose actions menu is open.
    menu: Option<usize>,
    /// Server awaiting confirmation before it leaves the list.
    confirm_remove: Option<usize>,
    theme: Theme,
    discovery: Option<Task<()>>,
    discovering: bool,
    discovery_error: Option<String>,
}

fn normalize_endpoint(value: &str) -> Result<String> {
    if is_ssh_address(value) {
        return Ok(SshTarget::parse(value)?.canonical());
    }
    match Endpoint::parse(value.trim())? {
        Endpoint::Unix(path) => {
            let path: std::path::PathBuf = path.components().collect();
            Ok(format!("unix://{}", path.display()))
        }
        Endpoint::Network(url) => Ok(url.to_string()),
    }
}

/// A readable name for a server nobody has named: "Local" for a socket, else the host.
fn default_label(endpoint: &str) -> String {
    if let Ok(target) = SshTarget::parse(endpoint) {
        return target.host().to_owned();
    }
    match Endpoint::parse(endpoint) {
        Ok(Endpoint::Unix(_)) => "Local".into(),
        Ok(Endpoint::Network(url)) => url.host_str().unwrap_or(endpoint).to_owned(),
        Err(_) => endpoint.to_owned(),
    }
}

/// The compact address beside a server's name, and the icon for its transport.
fn endpoint_summary(endpoint: &str) -> (String, Icon) {
    if let Ok(target) = SshTarget::parse(endpoint) {
        return (format!("ssh {}", target.destination()), Icon::Terminal);
    }
    match Endpoint::parse(endpoint) {
        Ok(Endpoint::Unix(path)) => (
            path.file_name().map_or_else(
                || path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            ),
            Icon::Server,
        ),
        Ok(Endpoint::Network(url)) => {
            let host = url.host_str().unwrap_or_default();
            let address = url
                .port()
                .map_or_else(|| host.to_owned(), |port| format!("{host}:{port}"));
            let secure = matches!(url.scheme(), "https" | "wss");
            (address, if secure { Icon::Lock } else { Icon::Globe })
        }
        Err(_) => (endpoint.to_owned(), Icon::Server),
    }
}

/// The final cause of an error chain, such as "Name or service not known".
/// The full chain remains available in the tooltip.
fn concise_error(error: &str) -> String {
    let cause = error.rsplit(": ").next().unwrap_or(error).trim();
    let mut chars = cause.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

fn validate_server(
    label: &str,
    endpoint: &str,
    existing: &[ServerEntry],
) -> Result<(Option<String>, String)> {
    ensure!(
        existing.len() < MAX_SERVERS,
        "At most {MAX_SERVERS} servers can be added"
    );
    let endpoint = normalize_endpoint(endpoint)?;
    ensure!(
        !existing.iter().any(|entry| entry.endpoint == endpoint),
        "This server is already in the list"
    );
    let label = label.trim();
    ensure!(
        label.len() <= 128 && !label.chars().any(char::is_control),
        "Server name must be at most 128 bytes and contain no control characters"
    );
    Ok(((!label.is_empty()).then(|| label.to_owned()), endpoint))
}

fn selection_after_removal(
    active: Option<usize>,
    removed: usize,
    remaining: &[ServerEntry],
) -> Option<usize> {
    if active == Some(removed) {
        remaining.first().map(|entry| entry.id)
    } else {
        active
    }
}

fn footer_summary(servers: &[ServerEntry]) -> (String, bool) {
    let connected = servers.iter().filter(|entry| entry.connected).count();
    match (servers.len(), connected) {
        (0, _) => ("No servers added".into(), false),
        (_, 0) => ("No servers connected".into(), true),
        (1, 1) => ("Connected".into(), false),
        (total, connected) => (format!("{connected} of {total} servers connected"), false),
    }
}

impl ServerWorkspaceView {
    pub fn new(
        endpoints: Vec<(String, Option<NamedServer>)>,
        scripts: Vec<ClientScript>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self {
            servers: Vec::new(),
            active: None,
            next_id: 0,
            scripts,
            focus: cx.focus_handle(),
            add: None,
            menu: None,
            confirm_remove: None,
            theme: Theme::default(),
            discovery: None,
            discovering: false,
            discovery_error: None,
        };
        for (endpoint, named) in endpoints.into_iter().take(MAX_SERVERS) {
            let endpoint = normalize_endpoint(&endpoint).unwrap_or(endpoint);
            if view.servers.iter().any(|entry| entry.endpoint == endpoint) {
                continue;
            }
            let label = named.as_ref().map(|named| named.name().to_owned());
            let id = view.push_server(label, endpoint, named);
            view.connect(id, window, cx);
        }
        if let Some(first) = view.servers.first() {
            view.select(first.id, window, cx);
        }
        view.refresh_servers(window, cx);
        view
    }

    fn push_server(
        &mut self,
        label: Option<String>,
        endpoint: String,
        named: Option<NamedServer>,
    ) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        let label_chosen = label.is_some();
        let label = label.unwrap_or_else(|| {
            let label = default_label(&endpoint);
            // A second local socket is told apart by its file name.
            if self.servers.iter().any(|entry| entry.label == label) {
                endpoint_summary(&endpoint).0
            } else {
                label
            }
        });
        self.servers.push(ServerEntry {
            id,
            label_chosen,
            label,
            ssh: SshTarget::parse(&endpoint).ok(),
            tunnel: None,
            opening: None,
            endpoint,
            named,
            workspace: None,
            observation: None,
            connected: false,
            error: None,
            expanded: true,
        });
        id
    }

    fn connect(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.servers.iter_mut().find(|entry| entry.id == id) else {
            return;
        };
        if entry.workspace.is_some() || entry.opening.is_some() {
            return;
        }
        let Some(endpoint) = entry.connection_endpoint() else {
            if let Some(target) = entry.ssh.clone() {
                entry.error = None;
                entry.expanded = true;
                entry.opening = Some(Self::open_tunnel(target, id, window, cx));
                cx.notify();
            }
            return;
        };
        let label = SharedString::from(entry.label.clone());
        let workspace = cx.new(|cx| {
            let mut workspace = if let Some(named) = entry.named.clone() {
                WorkspaceView::new_with_server(endpoint, self.scripts.clone(), named, cx)
            } else {
                WorkspaceView::new_with_scripts(Some(endpoint), self.scripts.clone(), cx)
            };
            workspace.set_embedded(Some(label), cx);
            workspace.bind_host_window(window, cx);
            workspace.set_presented(false, window, cx);
            workspace
        });
        entry.connected = workspace.read(cx).connected();
        entry.error = workspace.read(cx).last_error().map(str::to_owned);
        entry.expanded = true;
        // The sidebar renders this server's sessions, so follow every change.
        entry.observation = Some(cx.observe(&workspace, move |host, child, cx| {
            let connected = child.read(cx).connected();
            let error = child.read(cx).last_error().map(str::to_owned);
            if let Some(entry) = host.servers.iter_mut().find(|entry| entry.id == id) {
                entry.connected = connected;
                entry.error = error;
            }
            cx.notify();
        }));
        entry.workspace = Some(workspace);
        cx.notify();
    }

    /// Start the SSH forward off the UI thread, then connect through it.
    fn open_tunnel(
        target: SshTarget,
        id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let open = cx
            .background_executor()
            .spawn(async move { SshTunnel::open(&target) });
        cx.spawn_in(window, async move |this, cx| {
            let result = open.await;
            let _ = this.update_in(cx, |view, window, cx| {
                let Some(entry) = view.servers.iter_mut().find(|entry| entry.id == id) else {
                    return;
                };
                entry.opening = None;
                match result {
                    Ok(tunnel) => {
                        entry.tunnel = Some(tunnel);
                        view.connect(id, window, cx);
                        if view.active == Some(id) {
                            view.select(id, window, cx);
                        }
                    }
                    Err(error) => entry.error = Some(format!("{error:#}")),
                }
                cx.notify();
            });
        })
    }

    fn select(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.servers.iter().any(|entry| entry.id == id) {
            return;
        }
        let sidebar_visible = self.sidebar_visible(cx);
        self.focus.focus(window);
        self.active = Some(id);
        for entry in &self.servers {
            if let Some(workspace) = &entry.workspace {
                workspace.update(cx, |view, cx| {
                    if entry.id == id {
                        // Switching servers keeps the sidebar where the user left it.
                        view.set_sidebar_visible(sidebar_visible, cx);
                    }
                    view.set_presented(entry.id == id && self.add.is_none(), window, cx);
                });
            }
        }
        if self.add.is_none()
            && let Some(workspace) = self
                .servers
                .iter()
                .find(|entry| entry.id == id)
                .and_then(|entry| entry.workspace.as_ref())
        {
            workspace.update(cx, |view, cx| view.focus_presented(window, cx));
        }
        cx.notify();
    }

    fn disconnect(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.active == Some(id) {
            self.focus.focus(window);
        }
        if let Some(entry) = self.servers.iter_mut().find(|entry| entry.id == id) {
            if let Some(workspace) = &entry.workspace {
                workspace.update(cx, |view, cx| view.set_presented(false, window, cx));
            }
            entry.observation = None;
            entry.workspace = None;
            // Closing the forward stops its ssh process; reconnecting opens a new one.
            entry.opening = None;
            entry.tunnel = None;
            entry.connected = false;
            entry.error = None;
            entry.expanded = false;
        }
        cx.notify();
    }

    fn reconnect(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.connect(id, window, cx);
        self.select(id, window, cx);
    }

    /// Start a fresh connection, discarding a failed one.
    fn retry(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.disconnect(id, window, cx);
        self.reconnect(id, window, cx);
    }

    fn remove(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.disconnect(id, window, cx);
        self.servers.retain(|entry| entry.id != id);
        // A pending refresh must not immediately resurrect a removed connection.
        self.discovery = None;
        self.discovering = false;
        let next = selection_after_removal(self.active, id, &self.servers);
        if self.active != next {
            self.active = next;
            if let Some(next) = next {
                self.select(next, window, cx);
            }
        }
        cx.notify();
    }

    fn new_session(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.select(id, window, cx);
        if let Some(workspace) = self.workspace(id) {
            workspace.update(cx, |view, cx| view.request_new_session(window, cx));
        }
    }

    fn toggle_expanded(&mut self, id: usize, cx: &mut Context<Self>) {
        if let Some(entry) = self.servers.iter_mut().find(|entry| entry.id == id) {
            entry.expanded = !entry.expanded;
            cx.notify();
        }
    }

    fn workspace(&self, id: usize) -> Option<Entity<WorkspaceView>> {
        self.servers
            .iter()
            .find(|entry| entry.id == id)
            .and_then(|entry| entry.workspace.clone())
    }

    fn active_workspace(&self) -> Option<Entity<WorkspaceView>> {
        self.active.and_then(|id| self.workspace(id))
    }

    /// The presented workspace owns the sidebar toggle shortcut.
    fn sidebar_visible(&self, cx: &App) -> bool {
        self.active_workspace()
            .is_none_or(|workspace| workspace.read(cx).sidebar_visible())
    }

    fn open_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.add.is_some() {
            return;
        }
        let label = cx.new(|cx| TextField::new(String::new(), "Name (optional)".into(), cx));
        let endpoint = cx.new(|cx| {
            TextField::new(
                String::new(),
                "user@host, https://host or unix:///path/server.sock".into(),
                cx,
            )
        });
        let subscriptions = [&label, &endpoint]
            .into_iter()
            .map(|field| {
                cx.subscribe_in(field, window, |view, _, event, window, cx| match event {
                    TextFieldEvent::Submitted => view.submit_add(window, cx),
                    TextFieldEvent::Cancelled => view.close_add(window, cx),
                    TextFieldEvent::Changed => {}
                })
            })
            .collect();
        self.add = Some(AddServer {
            label,
            endpoint: endpoint.clone(),
            _subscriptions: subscriptions,
            error: None,
        });
        if let Some(active) = self.active {
            self.select(active, window, cx);
        }
        endpoint.focus_handle(cx).focus(window);
        cx.notify();
    }

    fn close_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.add = None;
        if let Some(active) = self.active {
            self.select(active, window, cx);
        } else {
            self.focus.focus(window);
        }
        cx.notify();
    }

    fn submit_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &self.add else {
            return;
        };
        let result = validate_server(
            dialog.label.read(cx).value(),
            dialog.endpoint.read(cx).value(),
            &self.servers,
        );
        let (label, endpoint) = match result {
            Ok(value) => value,
            Err(error) => {
                if let Some(dialog) = &mut self.add {
                    dialog.error = Some(error.to_string());
                }
                cx.notify();
                return;
            }
        };
        let id = self.push_server(label, endpoint, None);
        self.add = None;
        self.connect(id, window, cx);
        self.select(id, window, cx);
    }

    fn refresh_servers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.discovering {
            return;
        }
        let Some(endpoint) = self
            .servers
            .iter()
            .find(|entry| Some(entry.id) == self.active)
            .or_else(|| self.servers.first())
            .and_then(ServerEntry::connection_endpoint)
        else {
            return;
        };
        self.discovering = true;
        self.discovery_error = None;
        let discovery = cx
            .background_executor()
            .spawn(async move { crate::rex::discovery::discover_servers(&endpoint) });
        self.discovery = Some(cx.spawn_in(window, async move |this, cx| {
            let result = discovery.await;
            let _ = this.update_in(cx, |view, window, cx| {
                view.discovering = false;
                match result {
                    Ok(servers) => view.add_discovered(servers, window, cx),
                    Err(error) => {
                        view.discovery_error = Some(format!("Cannot discover servers: {error:#}"));
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn add_discovered(
        &mut self,
        servers: Vec<crate::rex::discovery::DiscoveredServer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for server in servers {
            let Ok(normalized) = normalize_endpoint(&server.endpoint) else {
                continue;
            };
            if let Some(entry) = self
                .servers
                .iter_mut()
                .find(|entry| entry.endpoint == normalized)
            {
                if !entry.label_chosen && !server.name.is_empty() {
                    entry.label = server.name;
                    entry.label_chosen = true;
                    let label = SharedString::from(entry.label.clone());
                    if let Some(workspace) = &entry.workspace {
                        workspace.update(cx, |view, cx| view.set_embedded(Some(label), cx));
                    }
                }
                continue;
            }
            if self.servers.len() >= MAX_SERVERS {
                break;
            }
            let Ok((label, endpoint)) = validate_server(&server.name, &normalized, &self.servers)
            else {
                continue;
            };
            let id = self.push_server(label, endpoint, None);
            self.connect(id, window, cx);
        }
        if self.active.is_none()
            && let Some(first) = self.servers.first()
        {
            self.select(first.id, window, cx);
        }
    }

    fn add_dialog(&self, dialog: &AddServer, cx: &Context<Self>) -> gpui::Div {
        let theme = self.theme;
        modal(
            440.,
            div()
                .child(overlay_header(
                    "Add server",
                    Some(
                        "Connect to a Rex server. SSH addresses forward the remote server's \
                         socket using your SSH keys."
                            .into(),
                    ),
                    icon_button("close-add-server", Icon::Close, theme)
                        .on_click(cx.listener(|view, _, window, cx| view.close_add(window, cx)))
                        .into_any_element(),
                    theme,
                ))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .px_4()
                        .pb_4()
                        .child(dialog.label.clone())
                        .child(dialog.endpoint.clone())
                        .when_some(dialog.error.clone(), |body, error| {
                            body.child(div().text_color(rgb(theme.colors.danger)).child(error))
                        }),
                )
                .child(
                    overlay_footer(theme)
                        .child(
                            button("cancel-add-server", "Cancel", ButtonVariant::Quiet, theme)
                                .on_click(
                                    cx.listener(|view, _, window, cx| view.close_add(window, cx)),
                                ),
                        )
                        .child(
                            button("connect-server", "Connect", ButtonVariant::Primary, theme)
                                .on_click(
                                    cx.listener(|view, _, window, cx| view.submit_add(window, cx)),
                                ),
                        ),
                ),
            theme,
        )
    }

    fn remove_dialog(&self, entry: &ServerEntry, cx: &Context<Self>) -> gpui::Div {
        let theme = self.theme;
        let id = entry.id;
        let cancel = cx.listener(|view, _, _, cx| {
            view.confirm_remove = None;
            cx.notify();
        });
        modal(
            400.,
            div()
                .child(overlay_header(
                    format!("Remove {}?", entry.label),
                    Some(
                        "Its sessions keep running on the server. You can add it again later."
                            .into(),
                    ),
                    icon_button("close-remove-server", Icon::Close, theme)
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.confirm_remove = None;
                            cx.notify();
                        }))
                        .into_any_element(),
                    theme,
                ))
                .child(
                    overlay_footer(theme)
                        .child(
                            button(
                                "cancel-remove-server",
                                "Cancel",
                                ButtonVariant::Quiet,
                                theme,
                            )
                            .on_click(cancel),
                        )
                        .child(
                            button(
                                "confirm-remove-server",
                                "Remove",
                                ButtonVariant::Danger,
                                theme,
                            )
                            .on_click(cx.listener(
                                move |view, _, window, cx| {
                                    view.confirm_remove = None;
                                    view.remove(id, window, cx);
                                },
                            )),
                        ),
                ),
            theme,
        )
    }
}

impl Focusable for ServerWorkspaceView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if let Some(dialog) = &self.add {
            return dialog.endpoint.focus_handle(cx);
        }
        self.active_workspace().map_or_else(
            || self.focus.clone(),
            |workspace| workspace.focus_handle(cx),
        )
    }
}

impl ServerWorkspaceView {
    /// Reconnect, disconnect and remove, anchored below the header's "…" button.
    fn server_menu(&self, entry: &ServerEntry, cx: &Context<Self>) -> AnyElement {
        let theme = self.theme;
        let id = entry.id;
        let connection = if entry.workspace.is_some() {
            menu_item(
                ("server-disconnect", id),
                Icon::Power,
                "Disconnect",
                false,
                theme,
            )
            .on_click(cx.listener(move |view, _, window, cx| {
                view.menu = None;
                view.disconnect(id, window, cx);
            }))
        } else {
            menu_item(("server-connect", id), Icon::Power, "Connect", false, theme).on_click(
                cx.listener(move |view, _, window, cx| {
                    view.menu = None;
                    view.reconnect(id, window, cx);
                }),
            )
        };
        deferred(
            menu_panel(theme)
                .id(("server-menu", id))
                .absolute()
                .top(px(30.))
                .right(px(4.))
                .occlude()
                .on_mouse_down_out(cx.listener(|view, _, _, cx| {
                    view.menu = None;
                    cx.notify();
                }))
                .when(entry.workspace.is_some(), |menu| {
                    menu.child(
                        menu_item(
                            ("server-reconnect", id),
                            Icon::Refresh,
                            "Reconnect",
                            false,
                            theme,
                        )
                        .on_click(cx.listener(
                            move |view, _, window, cx| {
                                view.menu = None;
                                view.retry(id, window, cx);
                            },
                        )),
                    )
                })
                .child(connection)
                .child(menu_separator(theme))
                .child(
                    menu_item(("server-remove", id), Icon::Close, "Remove…", true, theme).on_click(
                        cx.listener(move |view, _, _, cx| {
                            view.menu = None;
                            view.confirm_remove = Some(id);
                            cx.notify();
                        }),
                    ),
                ),
        )
        .with_priority(1)
        .into_any_element()
    }

    fn server_section(&self, entry: &ServerEntry, cx: &mut Context<Self>) -> gpui::Div {
        let theme = self.theme;
        let colors = theme.colors;
        let id = entry.id;
        let state = entry.state();
        let (summary, transport) = endpoint_summary(&entry.endpoint);
        let detail = match (state, entry.expanded, &entry.workspace) {
            (ServerState::Connected, false, Some(workspace)) => {
                match workspace.read(cx).session_count() {
                    1 => "1 session".to_owned(),
                    count => format!("{count} sessions"),
                }
            }
            // A name derived from the host only needs the port beside it.
            _ => summary
                .strip_prefix(&entry.label)
                .unwrap_or(&summary)
                .to_owned(),
        };
        let status_color = match state {
            ServerState::Connected => colors.success,
            ServerState::Connecting => colors.warning,
            ServerState::Failed(_) => colors.danger,
            ServerState::Disconnected => colors.faint,
        };
        let actions = div()
            .when(state == ServerState::Connected, |actions| {
                actions.child(
                    icon_button(("server-new-session", id), Icon::Plus, theme)
                        .tooltip(tooltip("New session", theme))
                        .on_click(cx.listener(move |view, _, window, cx| {
                            cx.stop_propagation();
                            view.new_session(id, window, cx);
                        })),
                )
            })
            .child(
                icon_button(("server-actions", id), Icon::More, theme)
                    .tooltip(tooltip("Server actions", theme))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        cx.stop_propagation();
                        view.menu = Some(id);
                        cx.notify();
                    })),
            );
        let header = server_header(
            ("server-header", id),
            ServerHeader {
                name: entry.label.clone().into(),
                detail: detail.into(),
                transport,
                status_color,
                expanded: entry.expanded,
                dimmed: state == ServerState::Disconnected,
            },
            actions,
            theme,
        )
        .tooltip(tooltip(entry.endpoint.clone(), theme))
        .on_click(cx.listener(move |view, _, _, cx| view.toggle_expanded(id, cx)));
        let body = entry
            .expanded
            .then(|| self.server_body(entry, state, cx))
            .flatten();
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(1.))
            .child(header)
            .when(self.menu == Some(id), |section| {
                section.child(self.server_menu(entry, cx))
            })
            .children(body)
    }

    /// The session tree of a connected server, or its connection state.
    fn server_body(
        &self,
        entry: &ServerEntry,
        state: ServerState<'_>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = self.theme;
        let colors = theme.colors;
        let id = entry.id;
        let small = |id, label: &'static str| {
            button(id, label, ButtonVariant::Quiet, theme)
                .h(px(22.))
                .px_2()
                .text_size(px(11.))
        };
        match state {
            ServerState::Connected => entry.workspace.as_ref().map(|workspace| {
                let rows = workspace.update(cx, |view, cx| view.session_tree(cx));
                div()
                    .id(("server-tree", id))
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    // Sessions nest under their server's name.
                    .pl(px(14.))
                    // Acting on another server's tree presents that server first.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, _, window, cx| {
                            if view.active != Some(id) {
                                view.select(id, window, cx);
                            }
                        }),
                    )
                    .children(rows)
                    .into_any_element()
            }),
            ServerState::Connecting => {
                Some(server_state_line("Connecting…", colors.warning).into_any_element())
            }
            ServerState::Failed(error) => Some(
                server_state_line(concise_error(error), colors.danger)
                    .id(("server-error", id))
                    .tooltip(tooltip(error.to_owned(), theme))
                    .child(small(("server-retry", id), "Retry").on_click(
                        cx.listener(move |view, _, window, cx| view.retry(id, window, cx)),
                    ))
                    .into_any_element(),
            ),
            ServerState::Disconnected => Some(
                server_state_line("Disconnected", colors.faint)
                    .child(small(("server-connect-inline", id), "Connect").on_click(
                        cx.listener(move |view, _, window, cx| view.reconnect(id, window, cx)),
                    ))
                    .into_any_element(),
            ),
        }
    }

    /// Brand mark plus discovery and add-server controls; drags the window.
    fn sidebar_titlebar(&self, cx: &Context<Self>) -> gpui::Div {
        let theme = self.theme;
        let colors = theme.colors;
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_2()
            .h(px(44.))
            .pl(px(14.))
            .pr_2()
            .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(px(20.))
                    .rounded(px(5.))
                    .bg(rgb(colors.accent_solid))
                    .text_size(px(11.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(colors.accent_on_solid))
                    .child("R"),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Rex"),
            )
            .child(
                icon_button("refresh-servers", Icon::Refresh, theme)
                    // Keep the titlebar's window move from swallowing the click.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .when(self.discovering, |control| control.opacity(0.5))
                    .tooltip(tooltip(
                        if self.discovering {
                            "Discovering servers…"
                        } else {
                            "Discover servers"
                        },
                        theme,
                    ))
                    .on_click(cx.listener(|view, _, window, cx| view.refresh_servers(window, cx))),
            )
            .child(
                icon_button("add-server", Icon::Plus, theme)
                    // Keep the titlebar's window move from swallowing the click.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .tooltip(tooltip("Add server", theme))
                    .on_click(cx.listener(|view, _, window, cx| view.open_add(window, cx))),
            )
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> gpui::Div {
        let theme = self.theme;
        let mut tree = div()
            .id("server-tree")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .px_2()
            .pt_1()
            .gap(px(8.))
            .overflow_y_scroll();
        for entry in &self.servers {
            tree = tree.child(self.server_section(entry, cx));
        }
        let (message, error) = self
            .active_workspace()
            .and_then(|workspace| workspace.read(cx).status_notice())
            .or_else(|| self.discovery_error.clone().map(|error| (error, true)))
            .unwrap_or_else(|| footer_summary(&self.servers));
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(theme.sidebar_width))
            .h_full()
            .bg(rgb(theme.colors.sidebar))
            .child(self.sidebar_titlebar(cx))
            .child(tree)
            .child(sidebar_footer(SharedString::from(message), error, theme))
    }

    fn placeholder(&self, cx: &Context<Self>) -> AnyElement {
        let theme = self.theme;
        match self
            .active
            .and_then(|id| self.servers.iter().find(|entry| entry.id == id))
        {
            Some(entry) => {
                let id = entry.id;
                empty_state(
                    format!("{} is disconnected", entry.label),
                    "Its sessions keep running on the server until you reconnect.",
                    theme,
                )
                .child(
                    button(
                        "placeholder-connect",
                        "Connect",
                        ButtonVariant::Primary,
                        theme,
                    )
                    .on_click(
                        cx.listener(move |view, _, window, cx| view.reconnect(id, window, cx)),
                    ),
                )
                .into_any_element()
            }
            None => empty_state(
                "No servers",
                "Add a Rex server to see its sessions here.",
                theme,
            )
            .child(
                button(
                    "placeholder-add",
                    "Add server",
                    ButtonVariant::Primary,
                    theme,
                )
                .on_click(cx.listener(|view, _, window, cx| view.open_add(window, cx))),
            )
            .into_any_element(),
        }
    }
}

impl Render for ServerWorkspaceView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let colors = theme.colors;
        let content = self
            .active_workspace()
            .map_or_else(|| self.placeholder(cx), gpui::IntoElement::into_any_element);
        let confirm = self
            .confirm_remove
            .and_then(|id| self.servers.iter().find(|entry| entry.id == id))
            .map(|entry| self.remove_dialog(entry, cx));
        div()
            .relative()
            .flex()
            .size_full()
            .bg(rgb(colors.background))
            .text_color(rgb(colors.text))
            .text_size(px(12.))
            .track_focus(&self.focus)
            .when(self.sidebar_visible(cx), |root| {
                root.child(self.sidebar(cx))
            })
            .child(div().flex_1().min_w_0().h_full().child(content))
            .when_some(self.add.as_ref(), |root, dialog| {
                root.child(self.add_dialog(dialog, cx))
            })
            .children(confirm)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ServerEntry, ServerState, concise_error, default_label, endpoint_summary, footer_summary,
        normalize_endpoint, selection_after_removal, validate_server,
    };
    use crate::ui::components::Icon;

    fn entry(endpoint: &str) -> ServerEntry {
        let ssh = super::SshTarget::parse(endpoint).ok();
        ServerEntry {
            id: 0,
            label: endpoint.into(),
            label_chosen: false,
            endpoint: endpoint.into(),
            named: None,
            ssh,
            tunnel: None,
            opening: None,
            workspace: None,
            observation: None,
            connected: false,
            error: None,
            expanded: true,
        }
    }

    #[test]
    fn removing_another_server_preserves_selection_and_last_removal_clears_it() {
        let mut first = entry("unix:///tmp/first.sock");
        first.id = 4;
        let remaining = vec![first];
        assert_eq!(selection_after_removal(Some(4), 9, &remaining), Some(4));
        assert_eq!(selection_after_removal(Some(9), 9, &remaining), Some(4));
        assert_eq!(selection_after_removal(Some(9), 9, &[]), None);
        assert_eq!(selection_after_removal(None, 9, &remaining), None);
    }

    #[test]
    fn endpoint_aliases_are_rejected_before_starting_another_connection() -> anyhow::Result<()> {
        let existing = vec![entry("wss://example.com/")];
        assert!(validate_server("Other", " https://EXAMPLE.com:443 ", &existing).is_err());
        assert_eq!(
            normalize_endpoint("unix:///tmp/./rex.sock")?,
            "unix:///tmp/rex.sock"
        );
        Ok(())
    }

    #[test]
    fn server_form_validates_names_addresses_and_capacity() -> anyhow::Result<()> {
        assert!(validate_server("bad\nname", "http://localhost", &[]).is_err());
        assert!(validate_server("", "https://user:secret@host", &[]).is_err());
        let entries: Vec<_> = (0..16)
            .map(|index| entry(&format!("unix:///tmp/{index}.sock")))
            .collect();
        assert!(validate_server("next", "unix:///tmp/next.sock", &entries).is_err());
        assert_eq!(
            validate_server("  Work  ", "http://localhost:8080", &[])?,
            (Some("Work".into()), "ws://localhost:8080/".into())
        );
        assert_eq!(
            validate_server("  ", "http://localhost:8080", &[])?,
            (None, "ws://localhost:8080/".into())
        );
        Ok(())
    }

    #[test]
    fn unnamed_servers_get_readable_labels_and_compact_addresses() {
        assert_eq!(
            default_label("unix:///run/user/1000/rex/server.sock"),
            "Local"
        );
        assert_eq!(default_label("wss://devbox.lan:7420/"), "devbox.lan");
        assert_eq!(
            endpoint_summary("unix:///run/user/1000/rex/server.sock"),
            ("server.sock".into(), Icon::Server)
        );
        assert_eq!(
            endpoint_summary("wss://devbox.lan:7420/"),
            ("devbox.lan:7420".into(), Icon::Lock)
        );
        assert_eq!(
            endpoint_summary("ws://build:8080/"),
            ("build:8080".into(), Icon::Globe)
        );
    }

    #[test]
    fn servers_without_a_workspace_are_opening_failed_or_disconnected() {
        let mut server = entry("ssh://dev@host");
        assert_eq!(server.state(), ServerState::Disconnected);
        // SSH servers have nowhere to connect until their forward is open.
        assert_eq!(server.connection_endpoint(), None);
        server.ssh = None;
        assert_eq!(
            server.connection_endpoint().as_deref(),
            Some("ssh://dev@host")
        );
        server.error = Some("Host key not trusted".into());
        assert_eq!(server.state(), ServerState::Failed("Host key not trusted"));
    }

    #[test]
    fn ssh_servers_are_named_by_host_and_deduplicated_by_address() -> anyhow::Result<()> {
        assert_eq!(
            normalize_endpoint("dev@192.0.2.10")?,
            "ssh://dev@192.0.2.10"
        );
        let existing = vec![entry("ssh://dev@192.0.2.10")];
        assert!(validate_server("", "ssh://dev@192.0.2.10/", &existing).is_err());
        assert_eq!(default_label("ssh://dev@192.0.2.10"), "192.0.2.10");
        assert_eq!(
            endpoint_summary("ssh://dev@192.0.2.10"),
            ("ssh dev@192.0.2.10".into(), Icon::Terminal)
        );
        Ok(())
    }

    #[test]
    fn connection_errors_show_their_final_cause() {
        assert_eq!(
            concise_error(
                "Rex connection endpoint=wss://devbox.invalid:7420/: resolve Rex host: \
                 failed to lookup address information: Name or service not known"
            ),
            "Name or service not known"
        );
        assert_eq!(concise_error("refused"), "Refused");
        assert_eq!(concise_error(""), "");
    }

    #[test]
    fn footer_summarises_every_server_rather_than_one_workspace() {
        assert_eq!(footer_summary(&[]), ("No servers added".into(), false));
        let mut servers = vec![entry("unix:///a.sock"), entry("unix:///b.sock")];
        assert_eq!(
            footer_summary(&servers),
            ("No servers connected".into(), true)
        );
        servers[0].connected = true;
        assert_eq!(
            footer_summary(&servers),
            ("1 of 2 servers connected".into(), false)
        );
        servers.truncate(1);
        assert_eq!(footer_summary(&servers), ("Connected".into(), false));
    }
}
