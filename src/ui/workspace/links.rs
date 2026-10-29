//! Open only explicit user-activated web/mail links; never execute terminal URIs.
use super::{Context, PaneId, WorkspaceView};

fn launchable_uri(uri: &str) -> anyhow::Result<String> {
    anyhow::ensure!(
        !uri.chars().any(char::is_control),
        "Link contains control characters"
    );
    let parsed = url::Url::parse(uri)?;
    anyhow::ensure!(
        matches!(parsed.scheme(), "http" | "https" | "mailto"),
        "Only web and mail links can be opened"
    );
    anyhow::ensure!(
        parsed.username().is_empty() && parsed.password().is_none(),
        "Links containing credentials are not opened"
    );
    Ok(parsed.into())
}

impl WorkspaceView {
    pub(super) fn open_terminal_link(
        &mut self,
        pane: PaneId,
        column: u16,
        row: u16,
        viewport_token: u64,
        cx: &mut Context<Self>,
    ) {
        if !self.presentation.visible {
            return;
        }
        let presentation = self.presentation.generation;
        let generation = self.runtime.connection_generation();
        let future = match self.runtime.hyperlink_at(pane, column, row, viewport_token) {
            Ok(future) => future,
            Err(error) => {
                self.report_error(format!("Open link: {error:#}"), cx);
                return;
            }
        };
        cx.spawn(async move |this, cx| {
            let result = future.await;
            let _ = this.update(cx, |view, cx| {
                if !view.presentation.visible
                    || view.presentation.generation != presentation
                    || !view.runtime.connected()
                    || view.runtime.connection_generation() != generation
                    || view.runtime.state().pane(pane).is_none()
                {
                    return;
                }
                match result.and_then(|value| value["uri"].as_str().map(launchable_uri).transpose())
                {
                    Ok(Some(uri)) => cx.open_url(&uri),
                    Ok(None) => {}
                    Err(error) => view.report_error(format!("Open link: {error:#}"), cx),
                }
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::launchable_uri;
    #[test]
    fn explicit_web_and_mail_links_do_not_launch_other_handlers() {
        assert!(launchable_uri("https://example.com/界").is_ok());
        assert!(launchable_uri("mailto:user@example.com").is_ok());
        for uri in [
            "file:///tmp/program",
            "javascript:alert(1)",
            "ssh://host",
            "https://user:secret@example.com",
            "https://example.com/\n",
        ] {
            assert!(launchable_uri(uri).is_err());
        }
    }
}
