use std::path::PathBuf;

use chrono::{DateTime, Utc};
use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame,
};

use crate::registry::{archive_dir, is_accountir_db, Business, Registry};
use crate::tui::theme::Theme;

/// Action a user selected in the picker that the outer App loop must handle.
#[derive(Debug, Clone)]
pub enum PickerAction {
    None,
    OpenBusiness(Business),
    AddNew(PathBuf),
    AddExisting(PathBuf),
    ImportFound(Vec<PathBuf>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PickerMode {
    List,
    ArchivedList,
    AddMenu,
    AddNewInput,
    AddOpenInput,
    RenameInput { biz_id: String },
    ConfirmArchive { biz_id: String },
    ConfirmRestore { biz_id: String },
    FirstRunImport,
}

pub struct BusinessPickerView {
    pub action: PickerAction,
    mode: PickerMode,
    list_state: ListState,
    add_menu_state: ListState,
    import_state: ListState,

    active: Vec<Business>,
    archived: Vec<Business>,
    import_candidates: Vec<PathBuf>,
    import_selected: Vec<bool>,

    input_buffer: String,
    error_message: Option<String>,
    status_message: Option<String>,
}

impl BusinessPickerView {
    /// Build a picker rooted at the given registry. Detects first-run state
    /// (empty registry, scan not yet performed) and offers to import existing
    /// .db files in cwd.
    pub fn new(registry: &Registry) -> Self {
        let active = registry.list_active().unwrap_or_default();
        let archived = registry.list_archived().unwrap_or_default();

        let (mode, candidates, selected, list_state, import_state) =
            if active.is_empty() && !registry.get_bool("first_run_scanned", false) {
                let cands = scan_cwd_for_dbs();
                if cands.is_empty() {
                    let _ = registry.set_bool("first_run_scanned", true);
                    (PickerMode::List, Vec::new(), Vec::new(), select_first(&active), ListState::default())
                } else {
                    let sel = vec![true; cands.len()];
                    let mut import_st = ListState::default();
                    import_st.select(Some(0));
                    (
                        PickerMode::FirstRunImport,
                        cands,
                        sel,
                        ListState::default(),
                        import_st,
                    )
                }
            } else {
                (
                    PickerMode::List,
                    Vec::new(),
                    Vec::new(),
                    select_first(&active),
                    ListState::default(),
                )
            };

        Self {
            action: PickerAction::None,
            mode,
            list_state,
            add_menu_state: {
                let mut s = ListState::default();
                s.select(Some(0));
                s
            },
            import_state,
            active,
            archived,
            import_candidates: candidates,
            import_selected: selected,
            input_buffer: String::new(),
            error_message: None,
            status_message: None,
        }
    }

    /// Re-read businesses from the registry. Called after open/add/archive/restore.
    pub fn refresh(&mut self, registry: &Registry) {
        self.active = registry.list_active().unwrap_or_default();
        self.archived = registry.list_archived().unwrap_or_default();
        // Keep selection within bounds.
        if let Some(i) = self.list_state.selected() {
            if i >= self.active.len() && !self.active.is_empty() {
                self.list_state.select(Some(self.active.len() - 1));
            } else if self.active.is_empty() {
                self.list_state.select(None);
            }
        } else if !self.active.is_empty() {
            self.list_state.select(Some(0));
        }
    }

    pub fn has_action(&self) -> bool {
        !matches!(self.action, PickerAction::None)
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_message = Some(msg.into());
    }

    /// Handle a key. `registry` is borrowed for in-place mutations
    /// (rename/archive/restore happen without leaving the picker).
    pub fn handle_key(&mut self, key: KeyCode, registry: &Registry) {
        // Always clear stale messages on input.
        let prior_mode = self.mode.clone();
        match &self.mode {
            PickerMode::List => self.handle_list_key(key, registry),
            PickerMode::ArchivedList => self.handle_archived_key(key, registry),
            PickerMode::AddMenu => self.handle_add_menu_key(key),
            PickerMode::AddNewInput | PickerMode::AddOpenInput => self.handle_path_input_key(key),
            PickerMode::RenameInput { .. } => self.handle_rename_input_key(key, registry),
            PickerMode::ConfirmArchive { .. } => self.handle_confirm_archive_key(key, registry),
            PickerMode::ConfirmRestore { .. } => self.handle_confirm_restore_key(key, registry),
            PickerMode::FirstRunImport => self.handle_import_key(key, registry),
        }
        if prior_mode != self.mode {
            self.error_message = None;
        }
    }

    fn handle_list_key(&mut self, key: KeyCode, _registry: &Registry) {
        let n = self.active.len();
        match key {
            KeyCode::Up | KeyCode::Char('k') => move_selection(&mut self.list_state, n, -1),
            KeyCode::Down | KeyCode::Char('j') => move_selection(&mut self.list_state, n, 1),
            KeyCode::Enter => {
                if let Some(b) = self.selected_business() {
                    self.action = PickerAction::OpenBusiness(b.clone());
                }
            }
            KeyCode::Char('+') | KeyCode::Char('n') => {
                self.mode = PickerMode::AddMenu;
                let mut s = ListState::default();
                s.select(Some(0));
                self.add_menu_state = s;
            }
            KeyCode::Char('a') => {
                if let Some(b) = self.selected_business() {
                    self.mode = PickerMode::ConfirmArchive { biz_id: b.id.clone() };
                }
            }
            KeyCode::Char('r') => {
                if let Some(b) = self.selected_business() {
                    let id = b.id.clone();
                    let buf = b.display_name.clone().unwrap_or_else(|| b.name.clone());
                    self.input_buffer = buf;
                    self.mode = PickerMode::RenameInput { biz_id: id };
                }
            }
            KeyCode::Char('v') => {
                self.mode = PickerMode::ArchivedList;
                self.list_state = select_first(&self.archived);
            }
            _ => {}
        }
    }

    fn handle_archived_key(&mut self, key: KeyCode, _registry: &Registry) {
        let n = self.archived.len();
        match key {
            KeyCode::Up | KeyCode::Char('k') => move_selection(&mut self.list_state, n, -1),
            KeyCode::Down | KeyCode::Char('j') => move_selection(&mut self.list_state, n, 1),
            KeyCode::Char('v') | KeyCode::Esc => {
                self.mode = PickerMode::List;
                self.list_state = select_first(&self.active);
            }
            KeyCode::Char('R') => {
                if let Some(b) = self.selected_archived() {
                    self.mode = PickerMode::ConfirmRestore { biz_id: b.id.clone() };
                }
            }
            _ => {}
        }
    }

    fn handle_add_menu_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc => {
                self.mode = PickerMode::List;
            }
            KeyCode::Up | KeyCode::Char('k') => move_selection(&mut self.add_menu_state, 2, -1),
            KeyCode::Down | KeyCode::Char('j') => move_selection(&mut self.add_menu_state, 2, 1),
            KeyCode::Enter => {
                let sel = self.add_menu_state.selected().unwrap_or(0);
                self.input_buffer = if sel == 0 {
                    suggest_new_db_name()
                } else {
                    String::new()
                };
                self.mode = if sel == 0 {
                    PickerMode::AddNewInput
                } else {
                    PickerMode::AddOpenInput
                };
            }
            _ => {}
        }
    }

    fn handle_path_input_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc => {
                self.mode = PickerMode::AddMenu;
                self.input_buffer.clear();
            }
            KeyCode::Enter => {
                let path = PathBuf::from(self.input_buffer.trim());
                if path.as_os_str().is_empty() {
                    self.error_message = Some("Please enter a file path".to_string());
                    return;
                }
                match self.mode {
                    PickerMode::AddNewInput => {
                        if path.exists() {
                            self.error_message = Some(
                                "File already exists. Use 'Import existing' instead.".to_string(),
                            );
                        } else {
                            self.action = PickerAction::AddNew(path);
                        }
                    }
                    PickerMode::AddOpenInput => {
                        if !path.exists() {
                            self.error_message = Some("File does not exist.".to_string());
                        } else {
                            self.action = PickerAction::AddExisting(path);
                        }
                    }
                    _ => {}
                }
            }
            KeyCode::Backspace => {
                self.input_buffer.pop();
            }
            KeyCode::Char(c) => self.input_buffer.push(c),
            _ => {}
        }
    }

    fn handle_rename_input_key(&mut self, key: KeyCode, registry: &Registry) {
        match key {
            KeyCode::Esc => {
                self.mode = PickerMode::List;
                self.input_buffer.clear();
            }
            KeyCode::Enter => {
                let biz_id = match &self.mode {
                    PickerMode::RenameInput { biz_id } => biz_id.clone(),
                    _ => return,
                };
                let trimmed = self.input_buffer.trim();
                let new_name: Option<&str> = if trimmed.is_empty() { None } else { Some(trimmed) };
                match registry.rename(&biz_id, new_name) {
                    Ok(()) => {
                        self.status_message = Some(if new_name.is_some() {
                            "Renamed".to_string()
                        } else {
                            "Cleared custom name (now mirrors company name)".to_string()
                        });
                        self.mode = PickerMode::List;
                        self.input_buffer.clear();
                        self.refresh(registry);
                    }
                    Err(e) => self.error_message = Some(format!("Rename failed: {}", e)),
                }
            }
            KeyCode::Backspace => {
                self.input_buffer.pop();
            }
            KeyCode::Char(c) => self.input_buffer.push(c),
            _ => {}
        }
    }

    fn handle_confirm_archive_key(&mut self, key: KeyCode, registry: &Registry) {
        let biz_id = match &self.mode {
            PickerMode::ConfirmArchive { biz_id } => biz_id.clone(),
            _ => return,
        };
        match key {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => match registry.archive(&biz_id) {
                Ok(new_path) => {
                    self.status_message = Some(format!("Archived to {}", new_path.display()));
                    self.mode = PickerMode::List;
                    self.refresh(registry);
                }
                Err(e) => {
                    self.error_message = Some(format!("Archive failed: {}", e));
                    self.mode = PickerMode::List;
                }
            },
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.mode = PickerMode::List;
            }
            _ => {}
        }
    }

    fn handle_confirm_restore_key(&mut self, key: KeyCode, registry: &Registry) {
        let biz_id = match &self.mode {
            PickerMode::ConfirmRestore { biz_id } => biz_id.clone(),
            _ => return,
        };
        match key {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => match registry.restore(&biz_id) {
                Ok(path) => {
                    self.status_message = Some(format!("Restored to {}", path.display()));
                    self.refresh(registry);
                    self.mode = if self.archived.is_empty() {
                        self.list_state = select_first(&self.active);
                        PickerMode::List
                    } else {
                        self.list_state = select_first(&self.archived);
                        PickerMode::ArchivedList
                    };
                }
                Err(e) => {
                    self.error_message = Some(format!("Restore failed: {}", e));
                    self.mode = PickerMode::ArchivedList;
                }
            },
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.mode = PickerMode::ArchivedList;
            }
            _ => {}
        }
    }

    fn handle_import_key(&mut self, key: KeyCode, registry: &Registry) {
        let n = self.import_candidates.len();
        match key {
            KeyCode::Up | KeyCode::Char('k') => move_selection(&mut self.import_state, n, -1),
            KeyCode::Down | KeyCode::Char('j') => move_selection(&mut self.import_state, n, 1),
            KeyCode::Char(' ') => {
                if let Some(i) = self.import_state.selected() {
                    if let Some(slot) = self.import_selected.get_mut(i) {
                        *slot = !*slot;
                    }
                }
            }
            KeyCode::Char('a') => {
                let all_on = self.import_selected.iter().all(|b| *b);
                for s in &mut self.import_selected {
                    *s = !all_on;
                }
            }
            KeyCode::Enter => {
                let picks: Vec<PathBuf> = self
                    .import_candidates
                    .iter()
                    .zip(self.import_selected.iter())
                    .filter(|(_, sel)| **sel)
                    .map(|(p, _)| p.clone())
                    .collect();
                let _ = registry.set_bool("first_run_scanned", true);
                if picks.is_empty() {
                    self.mode = PickerMode::List;
                } else {
                    self.action = PickerAction::ImportFound(picks);
                }
            }
            KeyCode::Esc => {
                let _ = registry.set_bool("first_run_scanned", true);
                self.mode = PickerMode::List;
            }
            _ => {}
        }
    }

    fn selected_business(&self) -> Option<&Business> {
        self.list_state.selected().and_then(|i| self.active.get(i))
    }

    fn selected_archived(&self) -> Option<&Business> {
        self.list_state.selected().and_then(|i| self.archived.get(i))
    }

    // --- drawing ---

    pub fn draw(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(6), // title
                Constraint::Min(8),    // body
                Constraint::Length(3), // footer
            ])
            .margin(2)
            .split(area);

        self.draw_title(frame, chunks[0], theme);

        match &self.mode {
            PickerMode::List => self.draw_list(frame, chunks[1], theme, /*archived=*/ false),
            PickerMode::ArchivedList => self.draw_list(frame, chunks[1], theme, true),
            PickerMode::AddMenu => self.draw_add_menu(frame, chunks[1], theme),
            PickerMode::AddNewInput | PickerMode::AddOpenInput => {
                self.draw_path_input(frame, chunks[1], theme)
            }
            PickerMode::RenameInput { .. } => self.draw_rename_input(frame, chunks[1], theme),
            PickerMode::ConfirmArchive { biz_id } => {
                self.draw_confirm(frame, chunks[1], theme, biz_id, /*restore=*/ false)
            }
            PickerMode::ConfirmRestore { biz_id } => {
                self.draw_confirm(frame, chunks[1], theme, biz_id, /*restore=*/ true)
            }
            PickerMode::FirstRunImport => self.draw_import(frame, chunks[1], theme),
        }

        self.draw_footer(frame, chunks[2], theme);
    }

    fn draw_title(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let subtitle = match self.mode {
            PickerMode::ArchivedList => "Archived businesses",
            PickerMode::FirstRunImport => "Import existing databases",
            _ => "Choose a business to work on",
        };
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "  A C C O U N T I R",
                Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                format!("  {}", subtitle),
                Style::default().fg(theme.header),
            )),
        ];
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn draw_list(&self, frame: &mut Frame, area: Rect, theme: &Theme, archived: bool) {
        let businesses = if archived { &self.archived } else { &self.active };
        let title = if archived {
            " Archived "
        } else {
            " Businesses "
        };

        if businesses.is_empty() {
            let msg = if archived {
                "No archived businesses. Press 'v' to return."
            } else {
                "No businesses yet. Press '+' to add one."
            };
            let para = Paragraph::new(Line::from(Span::styled(
                msg,
                Style::default().fg(theme.fg_dim),
            )))
            .block(Block::default().borders(Borders::ALL).title(title));
            frame.render_widget(para, area);
            return;
        }

        let now = Utc::now();
        let items: Vec<ListItem> = businesses
            .iter()
            .map(|b| {
                let last = b
                    .last_opened_at
                    .map(|t| format!("{:>14}", relative_time(now, t)))
                    .unwrap_or_else(|| format!("{:>14}", "never"));
                let path = if archived {
                    b.original_path
                        .as_ref()
                        .map(|p| abbrev_path(p))
                        .unwrap_or_else(|| abbrev_path(&b.db_path))
                } else {
                    abbrev_path(&b.db_path)
                };
                let label = b.label();
                let line = Line::from(vec![
                    Span::styled(
                        format!("{:<32}", truncate(label, 32)),
                        Style::default().fg(theme.fg),
                    ),
                    Span::styled(last, Style::default().fg(theme.fg_dim)),
                    Span::raw("  "),
                    Span::styled(path, Style::default().fg(theme.fg_dim)),
                ]);
                ListItem::new(line)
            })
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(theme.border_style()),
            )
            .highlight_style(theme.selected_style())
            .highlight_symbol("> ");

        frame.render_stateful_widget(list, area, &mut self.list_state.clone());
    }

    fn draw_add_menu(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let items = vec![
            ListItem::new("Create new database file"),
            ListItem::new("Import existing database file"),
        ];
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Add business "),
            )
            .highlight_style(theme.selected_style())
            .highlight_symbol("> ");
        frame.render_stateful_widget(list, area, &mut self.add_menu_state.clone());
    }

    fn draw_path_input(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let title = if self.mode == PickerMode::AddNewInput {
            " New database file path "
        } else {
            " Existing database file path "
        };
        let style = if self.error_message.is_some() {
            Style::default().fg(theme.error)
        } else {
            Style::default().fg(theme.input_active_fg)
        };
        let display = match &self.error_message {
            Some(e) => format!("{} ({})", self.input_buffer, e),
            None => format!("{}█", self.input_buffer),
        };
        let para = Paragraph::new(Line::from(Span::styled(display, style)))
            .block(Block::default().borders(Borders::ALL).title(title));
        frame.render_widget(para, area);
    }

    fn draw_rename_input(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let style = if self.error_message.is_some() {
            Style::default().fg(theme.error)
        } else {
            Style::default().fg(theme.input_active_fg)
        };
        let display = match &self.error_message {
            Some(e) => format!("{} ({})", self.input_buffer, e),
            None => format!("{}█", self.input_buffer),
        };
        let para = Paragraph::new(vec![
            Line::from(Span::styled(
                "Set a custom display name for this business (empty = use company name).",
                Style::default().fg(theme.fg_dim),
            )),
            Line::from(""),
            Line::from(Span::styled(display, style)),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Rename business "),
        );
        frame.render_widget(para, area);
    }

    fn draw_confirm(
        &self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        biz_id: &str,
        restore: bool,
    ) {
        let pool = if restore { &self.archived } else { &self.active };
        let biz = pool.iter().find(|b| b.id == biz_id);
        let (title, action_desc, dest_desc) = if restore {
            let dest = biz
                .and_then(|b| b.original_path.clone())
                .map(|p| abbrev_path(&p))
                .unwrap_or_else(|| "(unknown)".to_string());
            (
                " Restore business ",
                "restore",
                format!("Will move the file back to: {}", dest),
            )
        } else {
            let dest = archive_dir().display().to_string();
            (
                " Archive business ",
                "archive",
                format!("Will move the file into: {}", dest),
            )
        };
        let label = biz.map(|b| b.label().to_string()).unwrap_or_default();
        let para = Paragraph::new(vec![
            Line::from(""),
            Line::from(vec![
                Span::raw("  Are you sure you want to "),
                Span::styled(
                    action_desc,
                    Style::default().fg(theme.header).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" \""),
                Span::styled(label, Style::default().fg(theme.fg)),
                Span::raw("\"?"),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                format!("  {}", dest_desc),
                Style::default().fg(theme.fg_dim),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  [Y]es / [N]o / Esc to cancel",
                Style::default().fg(theme.accent),
            )),
        ])
        .block(Block::default().borders(Borders::ALL).title(title));
        frame.render_widget(para, area);
    }

    fn draw_import(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0)])
            .split(area);

        let header = Paragraph::new(vec![
            Line::from(Span::styled(
                format!(
                    "Found {} accountir database file(s) in the current directory.",
                    self.import_candidates.len()
                ),
                Style::default().fg(theme.fg),
            )),
            Line::from(Span::styled(
                "Choose which to import as businesses (Space toggles, 'a' toggles all, Enter confirms, Esc skips).",
                Style::default().fg(theme.fg_dim),
            )),
        ]);
        frame.render_widget(header, chunks[0]);

        let items: Vec<ListItem> = self
            .import_candidates
            .iter()
            .zip(self.import_selected.iter())
            .map(|(p, sel)| {
                let mark = if *sel { "[x]" } else { "[ ]" };
                ListItem::new(Line::from(vec![
                    Span::styled(mark, Style::default().fg(theme.accent)),
                    Span::raw("  "),
                    Span::raw(p.display().to_string()),
                ]))
            })
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Import candidates "),
            )
            .highlight_style(theme.selected_style())
            .highlight_symbol("> ");
        frame.render_stateful_widget(list, chunks[1], &mut self.import_state.clone());
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let hint = match self.mode {
            PickerMode::List => {
                "Enter open  +/n add  a archive  r rename  v archived  ?/help  q quit"
            }
            PickerMode::ArchivedList => "R restore  v back  q quit",
            PickerMode::AddMenu => "Enter select  Esc back",
            PickerMode::AddNewInput | PickerMode::AddOpenInput => "Enter confirm  Esc cancel",
            PickerMode::RenameInput { .. } => "Enter save (empty = clear)  Esc cancel",
            PickerMode::ConfirmArchive { .. } | PickerMode::ConfirmRestore { .. } => {
                "Y confirm  N/Esc cancel"
            }
            PickerMode::FirstRunImport => "Space toggle  a toggle-all  Enter import  Esc skip",
        };
        let msg = self
            .status_message
            .as_deref()
            .or(self.error_message.as_deref());
        let style = if self.error_message.is_some() {
            Style::default().fg(theme.error)
        } else {
            Style::default().fg(theme.fg_dim)
        };
        let lines = match msg {
            Some(m) => vec![
                Line::from(Span::styled(hint, Style::default().fg(theme.accent))),
                Line::from(Span::styled(m.to_string(), style)),
            ],
            None => vec![Line::from(Span::styled(
                hint,
                Style::default().fg(theme.accent),
            ))],
        };
        frame.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL)),
            area,
        );
    }
}

// --- helpers ---

fn select_first(items: &[Business]) -> ListState {
    let mut state = ListState::default();
    if !items.is_empty() {
        state.select(Some(0));
    }
    state
}

fn move_selection(state: &mut ListState, len: usize, delta: i32) {
    if len == 0 {
        state.select(None);
        return;
    }
    let i = state.selected().unwrap_or(0) as i32;
    let n = len as i32;
    let new_i = ((i + delta).rem_euclid(n)) as usize;
    state.select(Some(new_i));
}

fn suggest_new_db_name() -> String {
    // Place in cwd by default; user can edit. Match prior behavior.
    "accountir.db".to_string()
}

fn scan_cwd_for_dbs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(cwd) = std::env::current_dir() else {
        return out;
    };
    let Ok(rd) = std::fs::read_dir(&cwd) else {
        return out;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("db") && is_accountir_db(&path) {
            out.push(path);
        }
    }
    out.sort();
    out
}

fn relative_time(now: DateTime<Utc>, t: DateTime<Utc>) -> String {
    let delta = now.signed_duration_since(t);
    let secs = delta.num_seconds();
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else if secs < 86_400 * 30 {
        format!("{}d ago", secs / 86_400)
    } else if secs < 86_400 * 365 {
        format!("{}mo ago", secs / (86_400 * 30))
    } else {
        format!("{}y ago", secs / (86_400 * 365))
    }
}

fn abbrev_path(p: &std::path::Path) -> String {
    let home = dirs::home_dir();
    let s = p.display().to_string();
    if let Some(h) = home {
        let hs = h.display().to_string();
        if let Some(stripped) = s.strip_prefix(&hs) {
            return format!("~{}", stripped);
        }
    }
    s
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", truncated)
    }
}
