//! Owned terminal annotations and local interaction commands; no native handles.

/// One visible row interval, with an exclusive end column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellSpan {
    pub row: u16,
    pub start: u16,
    pub end: u16,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchState {
    pub generation: u64,
    pub query: String,
    pub complete: bool,
    pub total: u64,
    pub selected: Option<u64>,
    pub matches: Vec<CellSpan>,
    pub selected_match: Vec<CellSpan>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalAnnotations {
    pub revision: u64,
    pub viewport_token: u64,
    pub selection: Vec<CellSpan>,
    pub search: Option<SearchState>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchCommand {
    Set { query: String, generation: u64 },
    Next { generation: u64 },
    Previous { generation: u64 },
    Cancel { generation: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SelectionCommand {
    Press {
        column: f32,
        row: f32,
        time_millis: u64,
        viewport_token: u64,
    },
    Drag {
        column: f32,
        row: f32,
        rectangle: bool,
        viewport_token: u64,
    },
    Release,
    Cancel,
    Clear,
}

pub const MAX_SEARCH_QUERY_BYTES: usize = 1024;
pub const MAX_COPY_BYTES: usize = 4 * 1024 * 1024;
