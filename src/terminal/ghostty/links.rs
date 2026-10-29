//! Owned OSC 8 hyperlink lookup; opening a URI belongs to the application UI.
use super::GhosttyTerminal;
use anyhow::{Context, Result, ensure};
use libghostty_vt::{
    screen::CellWide,
    terminal::{Point, PointCoordinate},
};

const MAX_URI_BYTES: usize = 8192;

impl GhosttyTerminal {
    /// Read the OSC 8 URI at a cell in the currently displayed viewport.
    /// Wide-character continuation cells resolve through their leading cell.
    /// This returns owned text and never opens a URI or sends process input.
    /// Callers choose which URI schemes they allow before opening the result.
    ///
    /// # Errors
    /// Rejects stale viewport tokens, coordinates outside the grid, URI values
    /// longer than 8 KiB, invalid UTF-8, control characters, and native failures.
    pub fn hyperlink_at(
        &mut self,
        mut column: u16,
        row: u16,
        viewport_token: u64,
    ) -> Result<Option<String>> {
        ensure!(
            column < self.size.columns() && row < self.size.rows(),
            "terminal hyperlink coordinates are outside the viewport"
        );
        ensure!(
            self.annotations()?.viewport_token == viewport_token,
            "terminal hyperlink viewport changed"
        );
        let location = |column| {
            self.terminal.grid_ref(Point::Viewport(PointCoordinate {
                x: column,
                y: u32::from(row),
            }))
        };
        if column > 0 && location(column)?.cell()?.wide()? == CellWide::SpacerTail {
            column -= 1;
        }
        let cell = location(column)?;
        if !cell.cell()?.has_hyperlink()? {
            return Ok(None);
        }
        let mut bytes = vec![0; MAX_URI_BYTES];
        let length = cell
            .hyperlink_uri(&mut bytes)
            .context("read terminal hyperlink (maximum 8 KiB)")?;
        ensure!(length <= MAX_URI_BYTES, "terminal hyperlink exceeds 8 KiB");
        bytes.truncate(length);
        if bytes.is_empty() {
            return Ok(None);
        }
        let uri = String::from_utf8(bytes).context("terminal hyperlink is not UTF-8")?;
        ensure!(
            !uri.chars().any(char::is_control),
            "terminal hyperlink contains control characters"
        );
        Ok(Some(uri))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{core::GridSize, terminal::ghostty::TerminalConfig};

    fn terminal() -> Result<GhosttyTerminal> {
        GhosttyTerminal::new(TerminalConfig {
            size: GridSize::new(20, 3)?,
            ..TerminalConfig::default()
        })
    }

    #[test]
    fn hyperlink_lookup_preserves_unicode_uri_and_wide_cells_without_input() -> Result<()> {
        let mut terminal = terminal()?;
        terminal.feed(
            "\x1b]8;id=test;https://example.org/界?q=é\x1b\\界ok\x1b]8;;\x1b\\ plain".as_bytes(),
        );
        let token = terminal.annotations()?.viewport_token;
        for column in 0..4 {
            assert_eq!(
                terminal.hyperlink_at(column, 0, token)?.as_deref(),
                Some("https://example.org/界?q=é")
            );
        }
        assert_eq!(terminal.hyperlink_at(5, 0, token)?, None);
        assert!(terminal.hyperlink_at(20, 0, token).is_err());
        assert!(terminal.hyperlink_at(0, 3, token).is_err());
        assert_eq!(terminal.take_pending_output(), Vec::<u8>::new());
        Ok(())
    }

    #[test]
    fn hyperlink_snapshot_and_history_lookup_reject_stale_viewport() -> Result<()> {
        let mut terminal = terminal()?;
        terminal
            .feed(b"\x1b]8;;https://example.org/history\x1b\\link\x1b]8;;\x1b\\\r\n2\r\n3\r\n4");
        let token = terminal.annotations()?.viewport_token;
        terminal.scroll_lines(1)?;
        assert!(terminal.hyperlink_at(0, 0, token).is_err());
        let token = terminal.annotations()?.viewport_token;
        assert_eq!(
            terminal.hyperlink_at(0, 0, token)?.as_deref(),
            Some("https://example.org/history")
        );
        let snapshot = terminal.snapshot_bytes()?;
        let mut restored = GhosttyTerminal::from_snapshot(&snapshot)?;
        assert!(restored.hyperlink_at(0, 0, token).is_err());
        // Snapshots restore the active screen; the client-local history offset
        // is not serialized. Return to the same history row before comparing.
        restored.scroll_lines(1)?;
        let restored_token = restored.annotations()?.viewport_token;
        assert_eq!(
            restored.hyperlink_at(0, 0, restored_token)?,
            terminal.hyperlink_at(0, 0, token)?
        );
        assert_eq!(restored.take_pending_output(), Vec::<u8>::new());
        Ok(())
    }
}
