//! Native workflow control center for Dynamic Workflows / UltraCode.
//!
//! This is deliberately a regular Atlas modal: it uses the shared modal
//! chrome, theme, keyboard conventions, focus styling, and clickable rows.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::theme::Theme;
use crate::views::modal_window::{
    self, ModalSizing, ModalWindowConfig, ModalWindowOutcome, ModalWindowState, Shortcut,
};
use xai_grok_tools::implementations::grok_build::workflow::supervisor::{
    WorkflowRunSnapshot, WorkflowRunStatus,
};

#[derive(Debug)]
pub struct WorkflowUiModel {
    pub runs: BTreeMap<String, WorkflowRunSnapshot>,
    pub ultracode_enabled: bool,
}

impl Default for WorkflowUiModel {
    fn default() -> Self {
        let ultracode_enabled = std::env::var("GROK_ULTRACODE").ok().is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        });
        Self {
            runs: BTreeMap::new(),
            ultracode_enabled,
        }
    }
}

impl WorkflowUiModel {
    pub fn apply_snapshot(&mut self, snapshot: WorkflowRunSnapshot) {
        self.runs.insert(snapshot.run_id.clone(), snapshot);
    }

    pub fn active_count(&self) -> usize {
        self.runs
            .values()
            .filter(|run| {
                matches!(
                    run.status,
                    WorkflowRunStatus::Queued | WorkflowRunStatus::Running
                )
            })
            .count()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowsTab {
    Runs,
    Saved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowsPage {
    List,
    RunDetail(String),
    Workers(String),
    WorkerDetail {
        run_id: String,
        worker_index: usize,
    },
    SavedPreview(usize),
    WorktreeReview {
        run_id: String,
        worker_id: String,
        worktree_path: String,
    },
}

#[derive(Debug, Clone)]
pub struct SavedWorkflow {
    pub name: String,
    pub path: PathBuf,
    pub project_scoped: bool,
    pub script: String,
}

#[derive(Debug, Clone, Default)]
pub struct WorkflowWorker {
    pub worker_id: String,
    pub label: Option<String>,
    pub phase: Option<String>,
    pub status: String,
    pub error: Option<String>,
    pub worktree_path: Option<String>,
    pub timestamp: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowWorktreeChange {
    pub path: String,
    #[serde(default)]
    pub additions: u64,
    #[serde(default)]
    pub deletions: u64,
    #[serde(default)]
    pub patch: Option<String>,
}

#[derive(Debug)]
pub struct WorkflowsModalState {
    pub window: ModalWindowState,
    pub tab: WorkflowsTab,
    pub page: WorkflowsPage,
    pub runs: Vec<WorkflowRunSnapshot>,
    pub saved: Vec<SavedWorkflow>,
    pub workers: Vec<WorkflowWorker>,
    pub worktree_changes: Vec<WorkflowWorktreeChange>,
    pub selected: usize,
    pub action_selected: usize,
    pub query: String,
    pub filter_focused: bool,
    pub loading: bool,
    pub pending_action: Option<String>,
    pub message: Option<(String, bool)>,
    pub ultracode_enabled: bool,
    pub row_hits: Vec<Rect>,
    pub action_hits: Vec<Rect>,
}

impl WorkflowsModalState {
    pub fn new(model: &WorkflowUiModel, cwd: &Path) -> Self {
        let mut runs: Vec<_> = model.runs.values().cloned().collect();
        runs.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Self {
            window: ModalWindowState::with_tabs(2),
            tab: WorkflowsTab::Runs,
            page: WorkflowsPage::List,
            runs,
            saved: discover_saved_workflows(cwd),
            workers: Vec::new(),
            worktree_changes: Vec::new(),
            selected: 0,
            action_selected: 0,
            query: String::new(),
            filter_focused: false,
            loading: true,
            pending_action: Some("list".into()),
            message: None,
            ultracode_enabled: model.ultracode_enabled,
            row_hits: Vec::new(),
            action_hits: Vec::new(),
        }
    }

    pub fn replace_runs(&mut self, mut runs: Vec<WorkflowRunSnapshot>) {
        runs.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        self.runs = runs;
        self.selected = self.selected.min(self.filtered_len().saturating_sub(1));
        self.loading = false;
        self.pending_action = None;
    }

    pub fn selected_run(&self) -> Option<&WorkflowRunSnapshot> {
        let indices = self.filtered_run_indices();
        indices
            .get(self.selected)
            .and_then(|idx| self.runs.get(*idx))
    }

    pub fn filtered_len(&self) -> usize {
        match self.tab {
            WorkflowsTab::Runs => self.filtered_run_indices().len(),
            WorkflowsTab::Saved => self.filtered_saved_indices().len(),
        }
    }

    fn selectable_len(&self) -> usize {
        match self.page {
            WorkflowsPage::Workers(_) => self.workers.len(),
            _ => self.filtered_len(),
        }
    }

    fn filtered_run_indices(&self) -> Vec<usize> {
        let q = self.query.trim().to_ascii_lowercase();
        self.runs
            .iter()
            .enumerate()
            .filter(|(_, run)| {
                q.is_empty()
                    || run.run_id.to_ascii_lowercase().contains(&q)
                    || run
                        .current_phase
                        .as_deref()
                        .unwrap_or("")
                        .to_ascii_lowercase()
                        .contains(&q)
            })
            .map(|(idx, _)| idx)
            .collect()
    }

    fn filtered_saved_indices(&self) -> Vec<usize> {
        let q = self.query.trim().to_ascii_lowercase();
        self.saved
            .iter()
            .enumerate()
            .filter(|(_, workflow)| q.is_empty() || workflow.name.to_ascii_lowercase().contains(&q))
            .map(|(idx, _)| idx)
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowUiAction {
    List,
    Inspect,
    Workers,
    Pause,
    Resume,
    CancelWorker,
    Cancel,
    SetUltracode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowUiRequest {
    pub action: WorkflowUiAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

#[derive(Debug)]
pub enum WorkflowsOutcome {
    Changed,
    Close,
    Request(WorkflowUiRequest),
    SetUltracode(bool),
    RunSaved(String),
    ReviewWorktree {
        run_id: String,
        worker_id: String,
        worktree_path: String,
    },
    ApplyWorktree {
        worktree_path: String,
    },
}

pub fn handle_key(state: &mut WorkflowsModalState, key: &KeyEvent) -> WorkflowsOutcome {
    if state.filter_focused {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => state.filter_focused = false,
            KeyCode::Backspace => {
                state.query.pop();
                state.selected = 0;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                state.query.push(c);
                state.selected = 0;
            }
            _ => {}
        }
        return WorkflowsOutcome::Changed;
    }

    if key.code == KeyCode::Esc && !matches!(state.page, WorkflowsPage::List) {
        return navigate_back(state);
    }

    if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
        let next = match (state.tab, key.code) {
            (WorkflowsTab::Runs, KeyCode::Tab) | (WorkflowsTab::Saved, KeyCode::BackTab) => {
                WorkflowsTab::Saved
            }
            _ => WorkflowsTab::Runs,
        };
        switch_tab(state, next);
        return WorkflowsOutcome::Changed;
    }

    let config = chrome_config(state);
    match modal_window::handle_modal_key(&mut state.window, key, &config) {
        ModalWindowOutcome::CloseRequested => return WorkflowsOutcome::Close,
        ModalWindowOutcome::ShortcutActivated(id) => return shortcut_outcome(state, id),
        ModalWindowOutcome::Handled => return WorkflowsOutcome::Changed,
        _ => {}
    }

    match key.code {
        KeyCode::Char('/') => {
            state.filter_focused = true;
            WorkflowsOutcome::Changed
        }
        KeyCode::Backspace => navigate_back(state),
        KeyCode::Up | KeyCode::Char('k') => {
            state.selected = state.selected.saturating_sub(1);
            WorkflowsOutcome::Changed
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.selected = (state.selected + 1).min(state.selectable_len().saturating_sub(1));
            WorkflowsOutcome::Changed
        }
        KeyCode::Left | KeyCode::Char('h') => {
            state.action_selected = state.action_selected.saturating_sub(1);
            WorkflowsOutcome::Changed
        }
        KeyCode::Right | KeyCode::Char('l') => {
            let max = if matches!(state.page, WorkflowsPage::WorkerDetail { .. }) {
                1
            } else {
                3
            };
            state.action_selected = (state.action_selected + 1).min(max);
            WorkflowsOutcome::Changed
        }
        KeyCode::Char('r') => refresh_runs(state),
        KeyCode::Char('x') => WorkflowsOutcome::SetUltracode(!state.ultracode_enabled),
        KeyCode::Enter => activate_selected(state),
        KeyCode::Char('p') if matches!(state.page, WorkflowsPage::RunDetail(_)) => {
            run_action(state, WorkflowUiAction::Pause)
        }
        KeyCode::Char('c') => {
            if let WorkflowsPage::WorkerDetail {
                ref run_id,
                worker_index,
            } = state.page
            {
                let Some(worker) = state.workers.get(worker_index) else {
                    return WorkflowsOutcome::Changed;
                };
                WorkflowsOutcome::Request(WorkflowUiRequest {
                    action: WorkflowUiAction::CancelWorker,
                    run_id: Some(run_id.clone()),
                    worker_id: Some(worker.worker_id.clone()),
                    enabled: None,
                })
            } else if matches!(state.page, WorkflowsPage::RunDetail(_)) {
                run_action(state, WorkflowUiAction::Cancel)
            } else {
                WorkflowsOutcome::Changed
            }
        }
        KeyCode::Char('u') if matches!(state.page, WorkflowsPage::RunDetail(_)) => {
            run_action(state, WorkflowUiAction::Resume)
        }
        KeyCode::Char('w') => open_workers(state),
        _ => WorkflowsOutcome::Changed,
    }
}

fn switch_tab(state: &mut WorkflowsModalState, tab: WorkflowsTab) {
    state.tab = tab;
    state.window.active_tab = usize::from(tab == WorkflowsTab::Saved);
    state.page = WorkflowsPage::List;
    state.selected = 0;
    state.action_selected = 0;
    state.query.clear();
}

fn navigate_back(state: &mut WorkflowsModalState) -> WorkflowsOutcome {
    match state.page.clone() {
        WorkflowsPage::List => WorkflowsOutcome::Close,
        WorkflowsPage::Workers(run_id) => {
            state.page = WorkflowsPage::RunDetail(run_id);
            state.selected = 0;
            WorkflowsOutcome::Changed
        }
        WorkflowsPage::WorkerDetail { run_id, .. }
        | WorkflowsPage::WorktreeReview { run_id, .. } => {
            state.page = WorkflowsPage::Workers(run_id);
            state.action_selected = 0;
            WorkflowsOutcome::Changed
        }
        WorkflowsPage::RunDetail(_) | WorkflowsPage::SavedPreview(_) => {
            state.page = WorkflowsPage::List;
            state.workers.clear();
            state.selected = 0;
            state.action_selected = 0;
            WorkflowsOutcome::Changed
        }
    }
}

pub fn handle_mouse(state: &mut WorkflowsModalState, mouse: &MouseEvent) -> WorkflowsOutcome {
    let config = chrome_config(state);
    let _ = config;
    match modal_window::handle_modal_mouse(&mut state.window, mouse.kind, mouse.column, mouse.row) {
        ModalWindowOutcome::CloseRequested => return WorkflowsOutcome::Close,
        ModalWindowOutcome::TabChanged(idx) => {
            switch_tab(
                state,
                if idx == 0 {
                    WorkflowsTab::Runs
                } else {
                    WorkflowsTab::Saved
                },
            );
            return WorkflowsOutcome::Changed;
        }
        ModalWindowOutcome::ShortcutActivated(id) => return shortcut_outcome(state, id),
        ModalWindowOutcome::Handled => return WorkflowsOutcome::Changed,
        _ => {}
    }
    if mouse.kind == MouseEventKind::Down(crossterm::event::MouseButton::Left) {
        if let Some(idx) = state
            .row_hits
            .iter()
            .position(|rect| rect.contains((mouse.column, mouse.row).into()))
        {
            state.selected = idx;
            return activate_selected(state);
        }
        if let Some(idx) = state
            .action_hits
            .iter()
            .position(|rect| rect.contains((mouse.column, mouse.row).into()))
        {
            state.action_selected = idx;
            return activate_detail_action(state);
        }
    }
    WorkflowsOutcome::Changed
}

fn activate_selected(state: &mut WorkflowsModalState) -> WorkflowsOutcome {
    match &state.page {
        WorkflowsPage::List => match state.tab {
            WorkflowsTab::Runs => {
                if let Some(run) = state.selected_run() {
                    state.page = WorkflowsPage::RunDetail(run.run_id.clone());
                    state.action_selected = 0;
                }
                WorkflowsOutcome::Changed
            }
            WorkflowsTab::Saved => {
                if let Some(idx) = state.filtered_saved_indices().get(state.selected).copied() {
                    state.page = WorkflowsPage::SavedPreview(idx);
                }
                WorkflowsOutcome::Changed
            }
        },
        WorkflowsPage::RunDetail(_) => activate_detail_action(state),
        WorkflowsPage::Workers(run_id) => {
            if state.workers.get(state.selected).is_none() {
                return WorkflowsOutcome::Changed;
            }
            state.page = WorkflowsPage::WorkerDetail {
                run_id: run_id.clone(),
                worker_index: state.selected,
            };
            state.action_selected = 0;
            WorkflowsOutcome::Changed
        }
        WorkflowsPage::WorkerDetail {
            run_id,
            worker_index,
        } => {
            let Some(worker) = state.workers.get(*worker_index) else {
                return WorkflowsOutcome::Changed;
            };
            if state.action_selected == 0 {
                if let Some(path) = &worker.worktree_path {
                    state.loading = true;
                    state.pending_action = Some("review_worktree".into());
                    WorkflowsOutcome::ReviewWorktree {
                        run_id: run_id.clone(),
                        worker_id: worker.worker_id.clone(),
                        worktree_path: path.clone(),
                    }
                } else {
                    state.message = Some((
                        "This worker has no isolated worktree to review".into(),
                        true,
                    ));
                    WorkflowsOutcome::Changed
                }
            } else {
                if worker.status != "running" {
                    state.message = Some(("Only a running worker can be cancelled".into(), true));
                    return WorkflowsOutcome::Changed;
                }
                state.pending_action = Some("cancel_worker".into());
                WorkflowsOutcome::Request(WorkflowUiRequest {
                    action: WorkflowUiAction::CancelWorker,
                    run_id: Some(run_id.clone()),
                    worker_id: Some(worker.worker_id.clone()),
                    enabled: None,
                })
            }
        }
        WorkflowsPage::SavedPreview(idx) => state
            .saved
            .get(*idx)
            .map(|saved| WorkflowsOutcome::RunSaved(saved.name.clone()))
            .unwrap_or(WorkflowsOutcome::Changed),
        WorkflowsPage::WorktreeReview { worktree_path, .. } => {
            state.loading = true;
            state.pending_action = Some("apply_worktree".into());
            WorkflowsOutcome::ApplyWorktree {
                worktree_path: worktree_path.clone(),
            }
        }
    }
}

fn activate_detail_action(state: &mut WorkflowsModalState) -> WorkflowsOutcome {
    if matches!(state.page, WorkflowsPage::WorkerDetail { .. }) {
        return activate_selected(state);
    }
    let action = match state.action_selected {
        0 => open_workers(state),
        1 => run_action(state, WorkflowUiAction::Pause),
        2 => run_action(state, WorkflowUiAction::Resume),
        _ => run_action(state, WorkflowUiAction::Cancel),
    };
    action
}

fn open_workers(state: &mut WorkflowsModalState) -> WorkflowsOutcome {
    if state.pending_action.is_some() {
        return WorkflowsOutcome::Changed;
    }
    let run_id = match &state.page {
        WorkflowsPage::RunDetail(id) => Some(id.clone()),
        _ => state.selected_run().map(|run| run.run_id.clone()),
    };
    let Some(run_id) = run_id else {
        return WorkflowsOutcome::Changed;
    };
    state.page = WorkflowsPage::Workers(run_id.clone());
    state.workers.clear();
    state.selected = 0;
    state.loading = true;
    state.pending_action = Some("workers".into());
    WorkflowsOutcome::Request(WorkflowUiRequest {
        action: WorkflowUiAction::Workers,
        run_id: Some(run_id),
        worker_id: None,
        enabled: None,
    })
}

fn run_action(state: &mut WorkflowsModalState, action: WorkflowUiAction) -> WorkflowsOutcome {
    let run_id = match &state.page {
        WorkflowsPage::RunDetail(id) | WorkflowsPage::Workers(id) => Some(id.clone()),
        _ => state.selected_run().map(|run| run.run_id.clone()),
    };
    let Some(run_id) = run_id else {
        return WorkflowsOutcome::Changed;
    };
    let Some(run) = state.runs.iter().find(|run| run.run_id == run_id) else {
        return WorkflowsOutcome::Changed;
    };
    if !run_action_enabled(run.status, action) {
        state.message = Some((
            format!(
                "{} is unavailable while this workflow is {}",
                action_label(action),
                status_label(run.status)
            ),
            true,
        ));
        return WorkflowsOutcome::Changed;
    }
    if state.pending_action.is_some() {
        return WorkflowsOutcome::Changed;
    }
    state.pending_action = Some(format!("{action:?}").to_ascii_lowercase());
    WorkflowsOutcome::Request(WorkflowUiRequest {
        action,
        run_id: Some(run_id),
        worker_id: None,
        enabled: None,
    })
}

fn shortcut_outcome(state: &mut WorkflowsModalState, id: usize) -> WorkflowsOutcome {
    match id {
        0 => {
            state.filter_focused = true;
            WorkflowsOutcome::Changed
        }
        1 => refresh_runs(state),
        2 => activate_selected(state),
        3 => WorkflowsOutcome::SetUltracode(!state.ultracode_enabled),
        _ => WorkflowsOutcome::Changed,
    }
}

fn refresh_runs(state: &mut WorkflowsModalState) -> WorkflowsOutcome {
    if state.pending_action.is_some() {
        return WorkflowsOutcome::Changed;
    }
    state.loading = true;
    state.pending_action = Some("refresh".into());
    state.message = None;
    WorkflowsOutcome::Request(WorkflowUiRequest {
        action: WorkflowUiAction::List,
        run_id: None,
        worker_id: None,
        enabled: None,
    })
}

fn chrome_config(state: &WorkflowsModalState) -> ModalWindowConfig<'static> {
    static TABS: [&str; 2] = ["Runs", "Saved"];
    static SHORTCUTS: [Shortcut<'static>; 4] = [
        Shortcut {
            label: "/ filter",
            clickable: true,
            id: 0,
        },
        Shortcut {
            label: "r refresh",
            clickable: true,
            id: 1,
        },
        Shortcut {
            label: "Enter select",
            clickable: true,
            id: 2,
        },
        Shortcut {
            label: "x UltraCode",
            clickable: true,
            id: 3,
        },
    ];
    let _ = state;
    ModalWindowConfig {
        title: "Workflows",
        tabs: Some(&TABS),
        shortcuts: &SHORTCUTS,
        sizing: ModalSizing::large(),
        fold_info: None,
    }
}

pub fn render(
    buf: &mut Buffer,
    area: Rect,
    state: &mut WorkflowsModalState,
    compact: bool,
    theme: &Theme,
) {
    let mut config = chrome_config(state);
    config.sizing = config.sizing.with_compact(compact);
    state.window.active_tab = match state.tab {
        WorkflowsTab::Runs => 0,
        WorkflowsTab::Saved => 1,
    };
    let Some(content) =
        modal_window::render_modal_window(buf, area, &mut state.window, &config, theme)
    else {
        return;
    };
    state.row_hits.clear();
    state.action_hits.clear();
    let inner = content.content;
    let mut y = inner.y;
    let bottom = inner.bottom();

    if y < bottom {
        let (icon, label, detail, color) = if state.ultracode_enabled {
            (
                "◆ ",
                "UltraCode on ",
                "xhigh reasoning · automatic workflow orchestration",
                theme.accent_user,
            )
        } else {
            (
                "◇ ",
                "UltraCode off ",
                "press x to enable automatic orchestration",
                theme.gray,
            )
        };
        buf.set_line(
            inner.x,
            y,
            &Line::from(vec![
                Span::styled(icon, Style::default().fg(color)),
                Span::styled(
                    label,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(detail, Style::default().fg(theme.text_secondary)),
            ]),
            inner.width,
        );
        y += 1;
    }
    if y < bottom {
        let prompt = if state.filter_focused {
            "Filter ❯ "
        } else {
            "Filter   "
        };
        let value = if state.query.is_empty() {
            "type / to search"
        } else {
            &state.query
        };
        let style = if state.filter_focused {
            Style::default().fg(theme.text_primary).bg(theme.bg_visual)
        } else {
            Style::default().fg(theme.gray)
        };
        buf.set_line(
            inner.x,
            y,
            &Line::from(vec![
                Span::styled(
                    prompt,
                    Style::default()
                        .fg(theme.accent_user)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(value.to_string(), style),
            ]),
            inner.width,
        );
        if state.filter_focused {
            let cursor_x = inner.x + prompt.len() as u16 + state.query.chars().count() as u16;
            if cursor_x < inner.right()
                && let Some(cell) = buf.cell_mut((cursor_x, y))
            {
                cell.set_style(Style::default().fg(theme.bg_base).bg(theme.text_primary));
            }
        }
        y += 2;
    }
    if let Some((message, error)) = &state.message
        && y < bottom
    {
        buf.set_string(
            inner.x,
            y,
            message,
            Style::default().fg(if *error {
                theme.accent_error
            } else {
                theme.accent_success
            }),
        );
        y += 2;
    }
    if let Some(action) = &state.pending_action
        && y < bottom
    {
        buf.set_line(
            inner.x,
            y,
            &Line::from(vec![
                Span::styled("◌ ", Style::default().fg(theme.accent_user)),
                Span::styled(
                    format!("{}…", action.replace('_', " ")),
                    Style::default().fg(theme.text_secondary),
                ),
            ]),
            inner.width,
        );
        y += 2;
    }

    match state.page.clone() {
        WorkflowsPage::List => render_list(buf, inner, y, bottom, state, theme),
        WorkflowsPage::RunDetail(run_id) => {
            render_run_detail(buf, inner, y, bottom, state, theme, &run_id)
        }
        WorkflowsPage::Workers(run_id) => {
            render_workers(buf, inner, y, bottom, state, theme, &run_id)
        }
        WorkflowsPage::WorkerDetail {
            run_id,
            worker_index,
        } => render_worker_detail(buf, inner, y, bottom, state, theme, &run_id, worker_index),
        WorkflowsPage::SavedPreview(idx) => {
            render_saved_preview(buf, inner, y, bottom, state, theme, idx)
        }
        WorkflowsPage::WorktreeReview {
            run_id,
            worker_id,
            worktree_path,
        } => render_worktree_review(
            buf,
            inner,
            y,
            bottom,
            state,
            theme,
            &run_id,
            &worker_id,
            &worktree_path,
        ),
    }
}

fn render_list(
    buf: &mut Buffer,
    inner: Rect,
    mut y: u16,
    bottom: u16,
    state: &mut WorkflowsModalState,
    theme: &Theme,
) {
    if state.loading && state.runs.is_empty() && state.tab == WorkflowsTab::Runs {
        buf.set_string(
            inner.x,
            y,
            "Loading workflow runs…",
            Style::default().fg(theme.gray),
        );
        return;
    }
    match state.tab {
        WorkflowsTab::Runs => {
            let indices = state.filtered_run_indices();
            if indices.is_empty() {
                buf.set_string(
                    inner.x,
                    y,
                    "No workflow runs yet.",
                    Style::default().fg(theme.gray),
                );
                return;
            }
            for (row, idx) in indices.into_iter().enumerate() {
                if y >= bottom {
                    break;
                }
                let run = &state.runs[idx];
                let selected = row == state.selected;
                let row_rect = Rect::new(inner.x, y, inner.width, 1);
                state.row_hits.push(row_rect);
                let bg = if selected {
                    theme.bg_visual
                } else {
                    theme.bg_base
                };
                buf.set_style(row_rect, Style::default().bg(bg));
                let status_color = status_color(run.status, theme);
                let phase = run.current_phase.as_deref().unwrap_or("starting");
                let progress = format!(
                    "{}/{} workers · {} active",
                    run.agents_completed + run.agents_failed,
                    run.agents_spawned.max(run.agents_requested),
                    run.active_agents
                );
                buf.set_line(
                    inner.x,
                    y,
                    &Line::from(vec![
                        Span::styled(
                            if selected { "❯ " } else { "  " },
                            Style::default().fg(theme.accent_user).bg(bg),
                        ),
                        Span::styled(
                            format!("{:<11}", status_label(run.status)),
                            Style::default()
                                .fg(status_color)
                                .bg(bg)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("{}  ", short_id(&run.run_id)),
                            Style::default().fg(theme.text_primary).bg(bg),
                        ),
                        Span::styled(
                            format!("{phase} · {progress}"),
                            Style::default().fg(theme.text_secondary).bg(bg),
                        ),
                    ]),
                    inner.width,
                );
                y += 1;
            }
        }
        WorkflowsTab::Saved => {
            let indices = state.filtered_saved_indices();
            if indices.is_empty() {
                buf.set_string(
                    inner.x,
                    y,
                    "No saved workflows in .grok/workflows.",
                    Style::default().fg(theme.gray),
                );
                return;
            }
            for (row, idx) in indices.into_iter().enumerate() {
                if y >= bottom {
                    break;
                }
                let saved = &state.saved[idx];
                let selected = row == state.selected;
                let row_rect = Rect::new(inner.x, y, inner.width, 1);
                state.row_hits.push(row_rect);
                let bg = if selected {
                    theme.bg_visual
                } else {
                    theme.bg_base
                };
                buf.set_style(row_rect, Style::default().bg(bg));
                buf.set_line(
                    inner.x,
                    y,
                    &Line::from(vec![
                        Span::styled(
                            if selected { "❯ " } else { "  " },
                            Style::default().fg(theme.accent_user).bg(bg),
                        ),
                        Span::styled(
                            &saved.name,
                            Style::default()
                                .fg(theme.text_primary)
                                .bg(bg)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            if saved.project_scoped {
                                "  project"
                            } else {
                                "  user"
                            },
                            Style::default().fg(theme.gray).bg(bg),
                        ),
                        Span::styled(
                            format!("  {}", saved.path.display()),
                            Style::default().fg(theme.gray_dim).bg(bg),
                        ),
                    ]),
                    inner.width,
                );
                y += 1;
            }
        }
    }
}

fn render_run_detail(
    buf: &mut Buffer,
    inner: Rect,
    mut y: u16,
    bottom: u16,
    state: &mut WorkflowsModalState,
    theme: &Theme,
    run_id: &str,
) {
    let Some(run) = state.runs.iter().find(|run| run.run_id == run_id) else {
        buf.set_string(
            inner.x,
            y,
            "Run no longer available.",
            Style::default().fg(theme.accent_error),
        );
        return;
    };
    let title = format!("← Run {}", run.run_id);
    buf.set_string(
        inner.x,
        y,
        title,
        Style::default()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
    );
    y += 2;
    let fields = [
        ("Status", status_label(run.status).to_string()),
        (
            "Phase",
            run.current_phase.clone().unwrap_or_else(|| "—".into()),
        ),
        (
            "Workers",
            format!(
                "{} completed · {} failed · {} active",
                run.agents_completed, run.agents_failed, run.active_agents
            ),
        ),
        (
            "Concurrency",
            format!(
                "{} peak / {} limit",
                run.max_active_agents, run.max_concurrency
            ),
        ),
        (
            "Tokens",
            match run.max_tokens {
                Some(max) => format!("{} / {}", run.tokens_used, max),
                None => format!("{}", run.tokens_used),
            },
        ),
        ("Cache", format!("{} hits", run.cache_hits)),
        ("Elapsed", format_duration_ms(run.duration_ms)),
    ];
    for (label, value) in fields {
        if y >= bottom.saturating_sub(3) {
            break;
        }
        buf.set_string(
            inner.x,
            y,
            format!("{label:<13}"),
            Style::default().fg(theme.gray),
        );
        buf.set_string(
            inner.x + 13,
            y,
            value,
            Style::default().fg(theme.text_primary),
        );
        y += 1;
    }
    if let Some(error) = &run.error
        && y < bottom.saturating_sub(3)
    {
        y += 1;
        buf.set_string(
            inner.x,
            y,
            format!("Error: {error}"),
            Style::default().fg(theme.accent_error),
        );
        y += 1;
    }
    if y < bottom.saturating_sub(1) {
        y += 1;
        let actions = ["Workers", "Pause", "Resume", "Cancel"];
        let mut x = inner.x;
        for (idx, action) in actions.iter().enumerate() {
            let text = format!(" {action} ");
            let width = text.len() as u16;
            if x + width > inner.right() {
                break;
            }
            let selected = idx == state.action_selected;
            let action_kind = match idx {
                0 => WorkflowUiAction::Workers,
                1 => WorkflowUiAction::Pause,
                2 => WorkflowUiAction::Resume,
                _ => WorkflowUiAction::Cancel,
            };
            let enabled =
                run_action_enabled(run.status, action_kind) && state.pending_action.is_none();
            let rect = Rect::new(x, y, width, 1);
            state.action_hits.push(rect);
            buf.set_string(
                x,
                y,
                text,
                if !enabled {
                    Style::default().fg(theme.gray_dim).bg(theme.bg_base)
                } else if selected {
                    Style::default()
                        .fg(theme.bg_base)
                        .bg(theme.accent_user)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(theme.text_secondary)
                        .bg(theme.bg_highlight)
                },
            );
            x += width + 2;
        }
    }
}

fn render_workers(
    buf: &mut Buffer,
    inner: Rect,
    mut y: u16,
    bottom: u16,
    state: &mut WorkflowsModalState,
    theme: &Theme,
    run_id: &str,
) {
    buf.set_string(
        inner.x,
        y,
        format!("← Workers · {}", short_id(run_id)),
        Style::default()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
    );
    y += 2;
    if state.loading {
        buf.set_string(
            inner.x,
            y,
            "Loading worker journal…",
            Style::default().fg(theme.gray),
        );
        return;
    }
    if state.workers.is_empty() {
        buf.set_string(
            inner.x,
            y,
            "No worker events recorded.",
            Style::default().fg(theme.gray),
        );
        return;
    }
    for (idx, worker) in state.workers.iter().enumerate() {
        if y >= bottom {
            break;
        }
        let selected = idx == state.selected;
        let rect = Rect::new(inner.x, y, inner.width, 1);
        state.row_hits.push(rect);
        let bg = if selected {
            theme.bg_visual
        } else {
            theme.bg_base
        };
        buf.set_style(rect, Style::default().bg(bg));
        let suffix = if worker.worktree_path.is_some() {
            " · worktree"
        } else {
            ""
        };
        buf.set_line(
            inner.x,
            y,
            &Line::from(vec![
                Span::styled(
                    if selected { "❯ " } else { "  " },
                    Style::default().fg(theme.accent_user).bg(bg),
                ),
                Span::styled(
                    format!("{:<10}", worker.status),
                    Style::default()
                        .fg(if worker.status == "failed" {
                            theme.accent_error
                        } else {
                            theme.text_secondary
                        })
                        .bg(bg),
                ),
                Span::styled(
                    worker.label.as_deref().unwrap_or(&worker.worker_id),
                    Style::default().fg(theme.text_primary).bg(bg),
                ),
                Span::styled(suffix, Style::default().fg(theme.accent_user).bg(bg)),
            ]),
            inner.width,
        );
        y += 1;
        if selected
            && let Some(error) = &worker.error
            && y < bottom
        {
            buf.set_string(
                inner.x + 4,
                y,
                error,
                Style::default().fg(theme.accent_error),
            );
            y += 1;
        }
    }
}

fn render_worker_detail(
    buf: &mut Buffer,
    inner: Rect,
    mut y: u16,
    bottom: u16,
    state: &mut WorkflowsModalState,
    theme: &Theme,
    run_id: &str,
    worker_index: usize,
) {
    let Some(worker) = state.workers.get(worker_index) else {
        return;
    };
    buf.set_string(
        inner.x,
        y,
        format!(
            "← Worker · {}",
            worker.label.as_deref().unwrap_or(&worker.worker_id)
        ),
        Style::default()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
    );
    y += 2;
    let fields = [
        ("Run", short_id(run_id).to_string()),
        ("Worker ID", worker.worker_id.clone()),
        ("Status", worker.status.clone()),
        ("Phase", worker.phase.clone().unwrap_or_else(|| "—".into())),
        (
            "Worktree",
            worker
                .worktree_path
                .clone()
                .unwrap_or_else(|| "none (read-only worker)".into()),
        ),
        (
            "Updated",
            worker.timestamp.clone().unwrap_or_else(|| "—".into()),
        ),
    ];
    for (label, value) in fields {
        if y >= bottom.saturating_sub(3) {
            break;
        }
        buf.set_string(
            inner.x,
            y,
            format!("{label:<12}"),
            Style::default().fg(theme.gray),
        );
        buf.set_string(
            inner.x + 12,
            y,
            value,
            Style::default().fg(theme.text_primary),
        );
        y += 1;
    }
    if let Some(error) = &worker.error
        && y < bottom.saturating_sub(3)
    {
        y += 1;
        buf.set_string(
            inner.x,
            y,
            format!("Error: {error}"),
            Style::default().fg(theme.accent_error),
        );
        y += 1;
    }
    if y < bottom {
        y += 1;
        let labels = [" Review diff ", " Cancel worker "];
        let mut x = inner.x;
        for (idx, label) in labels.into_iter().enumerate() {
            let width = label.len() as u16;
            let selected = idx == state.action_selected;
            let enabled = match idx {
                0 => worker.worktree_path.is_some(),
                _ => worker.status == "running",
            } && state.pending_action.is_none();
            let rect = Rect::new(x, y, width, 1);
            state.action_hits.push(rect);
            buf.set_string(
                x,
                y,
                label,
                if !enabled {
                    Style::default().fg(theme.gray_dim).bg(theme.bg_base)
                } else if selected {
                    Style::default()
                        .fg(theme.bg_base)
                        .bg(theme.accent_user)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(theme.text_secondary)
                        .bg(theme.bg_highlight)
                },
            );
            x += width + 2;
        }
    }
}

fn render_saved_preview(
    buf: &mut Buffer,
    inner: Rect,
    mut y: u16,
    bottom: u16,
    state: &WorkflowsModalState,
    theme: &Theme,
    idx: usize,
) {
    let Some(saved) = state.saved.get(idx) else {
        return;
    };
    buf.set_string(
        inner.x,
        y,
        format!("← {}", saved.name),
        Style::default()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
    );
    y += 1;
    buf.set_string(
        inner.x,
        y,
        saved.path.display().to_string(),
        Style::default().fg(theme.gray),
    );
    y += 2;
    for line in saved
        .script
        .lines()
        .take(bottom.saturating_sub(y + 2) as usize)
    {
        buf.set_string(inner.x, y, line, Style::default().fg(theme.text_secondary));
        y += 1;
    }
    if y < bottom {
        buf.set_string(
            inner.x,
            y,
            "Enter run with preview + approval",
            Style::default()
                .fg(theme.accent_user)
                .add_modifier(Modifier::BOLD),
        );
    }
}

fn render_worktree_review(
    buf: &mut Buffer,
    inner: Rect,
    mut y: u16,
    bottom: u16,
    state: &WorkflowsModalState,
    theme: &Theme,
    run_id: &str,
    worker_id: &str,
    worktree_path: &str,
) {
    buf.set_string(
        inner.x,
        y,
        format!("← Worktree review · {}", short_id(run_id)),
        Style::default()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
    );
    y += 2;
    let fields = [("Worker", worker_id), ("Path", worktree_path)];
    for (label, value) in fields {
        if y >= bottom {
            break;
        }
        buf.set_string(
            inner.x,
            y,
            format!("{label:<10}"),
            Style::default().fg(theme.gray),
        );
        buf.set_string(
            inner.x + 10,
            y,
            value,
            Style::default().fg(theme.text_primary),
        );
        y += 1;
    }
    y += 2;
    if state.loading && y < bottom {
        buf.set_string(inner.x, y, "Loading diff…", Style::default().fg(theme.gray));
        return;
    }
    for change in &state.worktree_changes {
        if y >= bottom.saturating_sub(1) {
            break;
        }
        buf.set_line(
            inner.x,
            y,
            &Line::from(vec![
                Span::styled(
                    &change.path,
                    Style::default().fg(theme.path).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  +{} -{}", change.additions, change.deletions),
                    Style::default().fg(theme.text_secondary),
                ),
            ]),
            inner.width,
        );
        y += 1;
        if let Some(patch) = &change.patch {
            for line in patch.lines().take(4) {
                if y >= bottom.saturating_sub(1) {
                    break;
                }
                let color = if line.starts_with('+') {
                    theme.accent_success
                } else if line.starts_with('-') {
                    theme.accent_error
                } else {
                    theme.gray
                };
                buf.set_string(inner.x + 2, y, line, Style::default().fg(color));
                y += 1;
            }
        }
    }
    if state.worktree_changes.is_empty() && y < bottom.saturating_sub(1) {
        buf.set_string(
            inner.x,
            y,
            "No uncommitted changes found.",
            Style::default().fg(theme.gray),
        );
        y += 1;
    }
    if y < bottom {
        buf.set_string(
            inner.x,
            y,
            "Enter apply changes · Esc back",
            Style::default()
                .fg(theme.accent_user)
                .add_modifier(Modifier::BOLD),
        );
    }
}

pub fn workers_from_journal(value: &Value) -> Vec<WorkflowWorker> {
    let events = value
        .get("events_newest_first")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut workers = BTreeMap::<String, WorkflowWorker>::new();
    for event in events.into_iter().rev() {
        let Some(worker_id) = event.get("subagent_id").and_then(Value::as_str) else {
            continue;
        };
        let kind = event.get("event").and_then(Value::as_str).unwrap_or("");
        let worker = workers
            .entry(worker_id.to_string())
            .or_insert_with(|| WorkflowWorker {
                worker_id: worker_id.to_string(),
                ..Default::default()
            });
        worker.label = event
            .get("label")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or(worker.label.take());
        worker.phase = event
            .get("phase")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or(worker.phase.take());
        worker.timestamp = event
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_owned);
        worker.error = event
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_owned);
        worker.worktree_path = event
            .get("worktree_path")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or(worker.worktree_path.take());
        worker.status = match kind {
            "agent_started" => "running",
            "agent_completed" => "completed",
            "agent_failed" => "failed",
            _ => worker.status.as_str(),
        }
        .to_string();
    }
    workers.into_values().collect()
}

fn discover_saved_workflows(cwd: &Path) -> Vec<SavedWorkflow> {
    let mut by_name = BTreeMap::new();
    if let Some(home) = dirs::home_dir() {
        load_saved_dir(&home.join(".grok/workflows"), false, &mut by_name);
    }
    load_saved_dir(&cwd.join(".grok/workflows"), true, &mut by_name);
    by_name.into_values().collect()
}

fn load_saved_dir(dir: &Path, project_scoped: bool, out: &mut BTreeMap<String, SavedWorkflow>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("js") {
            continue;
        }
        let Some(name) = path
            .file_stem()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
        else {
            continue;
        };
        let Ok(script) = std::fs::read_to_string(&path) else {
            continue;
        };
        out.insert(
            name.clone(),
            SavedWorkflow {
                name,
                path,
                project_scoped,
                script,
            },
        );
    }
}

fn status_label(status: WorkflowRunStatus) -> &'static str {
    match status {
        WorkflowRunStatus::Queued => "queued",
        WorkflowRunStatus::Running => "running",
        WorkflowRunStatus::Paused => "paused",
        WorkflowRunStatus::Completed => "completed",
        WorkflowRunStatus::Partial => "partial",
        WorkflowRunStatus::Failed => "failed",
        WorkflowRunStatus::Cancelled => "cancelled",
        WorkflowRunStatus::Interrupted => "interrupted",
    }
}

fn action_label(action: WorkflowUiAction) -> &'static str {
    match action {
        WorkflowUiAction::List => "Refresh",
        WorkflowUiAction::Inspect => "Inspect",
        WorkflowUiAction::Workers => "Workers",
        WorkflowUiAction::Pause => "Pause",
        WorkflowUiAction::Resume => "Resume",
        WorkflowUiAction::CancelWorker => "Cancel worker",
        WorkflowUiAction::Cancel => "Cancel",
        WorkflowUiAction::SetUltracode => "UltraCode",
    }
}

fn run_action_enabled(status: WorkflowRunStatus, action: WorkflowUiAction) -> bool {
    match action {
        WorkflowUiAction::Workers | WorkflowUiAction::Inspect => true,
        WorkflowUiAction::Pause => status == WorkflowRunStatus::Running,
        WorkflowUiAction::Resume => matches!(
            status,
            WorkflowRunStatus::Paused
                | WorkflowRunStatus::Interrupted
                | WorkflowRunStatus::Failed
                | WorkflowRunStatus::Partial
        ),
        WorkflowUiAction::Cancel => {
            matches!(
                status,
                WorkflowRunStatus::Queued | WorkflowRunStatus::Running
            )
        }
        WorkflowUiAction::List
        | WorkflowUiAction::CancelWorker
        | WorkflowUiAction::SetUltracode => false,
    }
}

fn status_color(status: WorkflowRunStatus, theme: &Theme) -> ratatui::style::Color {
    match status {
        WorkflowRunStatus::Running => theme.accent_user,
        WorkflowRunStatus::Completed => theme.accent_success,
        WorkflowRunStatus::Failed | WorkflowRunStatus::Cancelled => theme.accent_error,
        WorkflowRunStatus::Partial | WorkflowRunStatus::Paused | WorkflowRunStatus::Interrupted => {
            theme.warning
        }
        WorkflowRunStatus::Queued => theme.gray,
    }
}

fn short_id(id: &str) -> &str {
    id.get(..id.len().min(18)).unwrap_or(id)
}

fn format_duration_ms(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(status: WorkflowRunStatus) -> WorkflowRunSnapshot {
        WorkflowRunSnapshot {
            run_id: "run-123456789".into(),
            status,
            current_phase: Some("Verify".into()),
            phases: vec!["Discover".into(), "Verify".into()],
            agents_requested: 2,
            agents_spawned: 2,
            agents_completed: 1,
            agents_failed: 0,
            cache_hits: 0,
            active_agents: 1,
            max_active_agents: 1,
            max_concurrency: 4,
            max_agents: 10,
            tokens_used: 120,
            max_tokens: Some(1_000),
            duration_ms: 2_500,
            script_hash: "hash".into(),
            created_at: "2026-07-18T00:00:00Z".into(),
            updated_at: "2026-07-18T00:00:01Z".into(),
            run_dir: PathBuf::from("/tmp/run"),
            error: None,
        }
    }

    fn modal(status: WorkflowRunStatus) -> WorkflowsModalState {
        let mut model = WorkflowUiModel::default();
        model.apply_snapshot(snapshot(status));
        let mut state = WorkflowsModalState::new(&model, Path::new("/missing"));
        state.loading = false;
        state.pending_action = None;
        state
    }

    #[test]
    fn worker_journal_collapses_events_by_worker() {
        let value = serde_json::json!({
            "events_newest_first": [
                {"event":"agent_completed","subagent_id":"w1","label":"audit","worktree_path":"/tmp/w","timestamp":"2"},
                {"event":"agent_started","subagent_id":"w1","label":"audit","timestamp":"1"}
            ]
        });
        let workers = workers_from_journal(&value);
        assert_eq!(workers.len(), 1);
        assert_eq!(workers[0].status, "completed");
        assert_eq!(workers[0].worktree_path.as_deref(), Some("/tmp/w"));
    }

    #[test]
    fn project_saved_workflow_overrides_user_copy() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join(".grok/workflows");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("audit.js"), "return 'project';").unwrap();
        let mut by_name = BTreeMap::new();
        by_name.insert(
            "audit".into(),
            SavedWorkflow {
                name: "audit".into(),
                path: PathBuf::from("/user/audit.js"),
                project_scoped: false,
                script: "return 'user';".into(),
            },
        );
        load_saved_dir(&project, true, &mut by_name);
        assert!(by_name["audit"].project_scoped);
        assert!(by_name["audit"].script.contains("project"));
    }

    #[test]
    fn escape_navigates_back_before_closing_modal() {
        let mut state = modal(WorkflowRunStatus::Running);
        state.page = WorkflowsPage::RunDetail("run-123456789".into());
        let outcome = handle_key(&mut state, &KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(outcome, WorkflowsOutcome::Changed));
        assert!(matches!(state.page, WorkflowsPage::List));
    }

    #[test]
    fn tab_switches_between_native_modal_tabs() {
        let mut state = modal(WorkflowRunStatus::Running);
        state.query = "stale filter".into();
        let outcome = handle_key(&mut state, &KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(matches!(outcome, WorkflowsOutcome::Changed));
        assert_eq!(state.tab, WorkflowsTab::Saved);
        assert!(state.query.is_empty());
    }

    #[test]
    fn completed_run_disables_pause_and_cancel() {
        let mut state = modal(WorkflowRunStatus::Completed);
        state.page = WorkflowsPage::RunDetail("run-123456789".into());
        assert!(matches!(
            run_action(&mut state, WorkflowUiAction::Pause),
            WorkflowsOutcome::Changed
        ));
        assert!(state.message.as_ref().is_some_and(|(_, error)| *error));
        assert!(!run_action_enabled(
            WorkflowRunStatus::Completed,
            WorkflowUiAction::Cancel
        ));
    }

    #[test]
    fn ultracode_toggle_is_available_inside_modal() {
        let mut state = modal(WorkflowRunStatus::Running);
        let requested = !state.ultracode_enabled;
        let outcome = handle_key(
            &mut state,
            &KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        );
        assert!(matches!(
            outcome,
            WorkflowsOutcome::SetUltracode(enabled) if enabled == requested
        ));
    }
}
