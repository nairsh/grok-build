//! Native provider login and credential-management modal for `/login`.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

use crate::app::actions::Action;
use crate::app::app_view::InputOutcome;
use crate::theme::Theme;
use crate::views::modal_window::{
    self, ModalContentArea, ModalSizing, ModalWindowConfig, ModalWindowState, Shortcut,
};

const TITLE: &str = "Providers & login";

#[derive(Debug, Clone)]
enum EntryKind {
    Header,
    Provider { provider: String },
    SavedCredential { credential_id: String },
}

#[derive(Debug, Clone)]
struct Entry {
    label: String,
    status: String,
    kind: EntryKind,
    credential_id: Option<String>,
}

impl Entry {
    fn header(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            status: String::new(),
            kind: EntryKind::Header,
            credential_id: None,
        }
    }

    fn selectable(&self) -> bool {
        !matches!(self.kind, EntryKind::Header)
    }
}

#[derive(Debug, Clone)]
enum Mode {
    Browse,
    ApiKey(ApiKeyForm),
    WaitingForBrowser { provider: String },
    ConfirmLogout { credential_id: String },
}

#[derive(Debug, Clone)]
struct ApiKeyForm {
    provider: String,
    connection_name: String,
    base_url: Option<String>,
    api_key: String,
    model: String,
    field: usize,
}

impl ApiKeyForm {
    fn new(provider: String) -> Self {
        let preset = xai_grok_shell::agent::connection::api_key_provider_presets()
            .into_iter()
            .find(|preset| preset.id == provider);
        let (connection_name, base_url, model) = match preset {
            Some(preset) => (
                provider.clone(),
                (provider == "litellm").then(|| preset.connection.base_url.unwrap_or_default()),
                preset.default_model.to_owned(),
            ),
            None => ("custom".to_owned(), Some(String::new()), String::new()),
        };
        Self {
            provider,
            connection_name,
            base_url,
            api_key: String::new(),
            model,
            field: 0,
        }
    }

    fn field_count(&self) -> usize {
        3 + usize::from(self.base_url.is_some())
    }

    fn active_text_mut(&mut self) -> &mut String {
        match self.field {
            0 => &mut self.connection_name,
            1 if self.base_url.is_some() => self.base_url.as_mut().expect("checked above"),
            field if field + 1 == self.field_count() => &mut self.model,
            _ => &mut self.api_key,
        }
    }

    fn save(&self) -> anyhow::Result<()> {
        xai_grok_shell::agent::login_interactive::save_api_key_connection_for_provider(
            &self.provider,
            self.connection_name.trim(),
            self.api_key.trim(),
            self.model.trim().to_owned(),
            self.base_url.clone(),
        )
    }
}

/// State for the native `/login` modal. Credential values are never retained
/// or rendered; the state holds only ids, provider names, and connection state.
#[derive(Debug, Clone)]
pub struct ProviderLoginModalState {
    pub window: ModalWindowState,
    entries: Vec<Entry>,
    selected: usize,
    scroll: usize,
    mode: Mode,
    notice: Option<String>,
}

impl ProviderLoginModalState {
    pub fn new(preselected_provider: Option<String>) -> Self {
        let mut state = Self {
            window: ModalWindowState::new(),
            entries: build_entries(),
            selected: 0,
            scroll: 0,
            mode: Mode::Browse,
            notice: None,
        };
        if let Some(provider) = preselected_provider {
            if let Some(index) = state.entries.iter().position(|entry| {
                matches!(&entry.kind, EntryKind::Provider { provider: id } if id == &provider)
            }) {
                state.selected = index;
            }
        }
        state.move_to_selectable(1);
        state
    }

    pub fn refresh(&mut self) {
        let selected_provider = self.selected_entry().and_then(|entry| match &entry.kind {
            EntryKind::Provider { provider } => Some(provider.clone()),
            EntryKind::SavedCredential { credential_id } => Some(credential_id.clone()),
            EntryKind::Header => None,
        });
        self.entries = build_entries();
        self.selected = selected_provider
            .and_then(|needle| {
                self.entries.iter().position(|entry| match &entry.kind {
                    EntryKind::Provider { provider } => provider == &needle,
                    EntryKind::SavedCredential { credential_id } => credential_id == &needle,
                    EntryKind::Header => false,
                })
            })
            .unwrap_or(0);
        self.move_to_selectable(1);
    }

    /// Finish an in-process browser login. This intentionally returns to the
    /// provider list instead of closing the modal, so a completed sign-in is
    /// immediately visible and the running session is never interrupted.
    pub fn finish_browser_login(&mut self, provider: &str, result: Result<(), &str>) {
        self.mode = Mode::Browse;
        self.refresh();
        self.notice = Some(match result {
            Ok(()) => format!("Connected {provider}. The new credential is ready to use."),
            Err(error) => format!("{provider} sign-in did not finish: {error}"),
        });
    }

    fn selected_entry(&self) -> Option<&Entry> {
        self.entries.get(self.selected)
    }

    fn move_to_selectable(&mut self, direction: isize) {
        if self.entries.is_empty() {
            return;
        }
        let len = self.entries.len() as isize;
        let mut index = self.selected as isize;
        for _ in 0..self.entries.len() {
            if self.entries[index as usize].selectable() {
                self.selected = index as usize;
                return;
            }
            index = (index + direction).rem_euclid(len);
        }
    }

    fn select_next(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.entries.len();
        self.move_to_selectable(1);
    }

    fn select_previous(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.selected = self
            .selected
            .checked_sub(1)
            .unwrap_or(self.entries.len() - 1);
        self.move_to_selectable(-1);
    }

    fn logout_selected(&mut self) {
        let Some(credential_id) = self
            .selected_entry()
            .and_then(|entry| entry.credential_id.clone())
        else {
            self.notice = Some(
                "This connection is not a saved credential. Remove its environment variable in your shell."
                    .to_owned(),
            );
            return;
        };
        self.mode = Mode::ConfirmLogout { credential_id };
    }

    fn remove_credential(&mut self, credential_id: &str) {
        let path = xai_grok_shell::agent::credential_store::CredentialStore::default_path();
        let result = (|| -> anyhow::Result<()> {
            let mut store = xai_grok_shell::agent::credential_store::CredentialStore::load(&path)?;
            anyhow::ensure!(
                store.remove(credential_id).is_some(),
                "credential no longer exists"
            );
            store.save(&path)?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.notice = Some(format!(
                    "Logged out of {credential_id}. Restart Atlas to remove it from active sessions."
                ));
                self.mode = Mode::Browse;
                self.refresh();
            }
            Err(error) => {
                self.notice = Some(format!("Could not log out: {error}"));
                self.mode = Mode::Browse;
            }
        }
    }
}

fn build_entries() -> Vec<Entry> {
    use xai_grok_shell::agent::credential_store::{Credential, CredentialStore};

    let path = CredentialStore::default_path();
    let store = CredentialStore::load(&path).unwrap_or_default();
    let mut entries = vec![Entry::header("Subscriptions")];
    for (provider, label, credential_id) in [
        ("xai", "xAI / Grok", None),
        (
            "anthropic-subscription",
            "Claude Pro / Max",
            Some("anthropic"),
        ),
        (
            "openai-codex",
            "ChatGPT Plus / Pro (Codex)",
            Some("openai-codex"),
        ),
        ("github-copilot", "GitHub Copilot", Some("github-copilot")),
    ] {
        let credential = credential_id.and_then(|id| store.get(id));
        let status = match credential {
            Some(Credential::Oauth { .. }) => "Logged in".to_owned(),
            Some(Credential::ApiKey { .. }) => "Saved API key".to_owned(),
            None if provider == "xai"
                && xai_grok_shell::agent::auth_method::has_xai_api_key_env() =>
            {
                "Environment key (XAI_API_KEY)".to_owned()
            }
            None if provider == "xai" => "Use /logout to manage account session".to_owned(),
            None => "Not connected".to_owned(),
        };
        entries.push(Entry {
            label: label.to_owned(),
            status,
            kind: EntryKind::Provider {
                provider: provider.to_owned(),
            },
            credential_id: credential
                .is_some()
                .then(|| credential_id.unwrap().to_owned()),
        });
    }

    entries.push(Entry::header("API-key providers"));
    let presets = xai_grok_shell::agent::connection::api_key_provider_presets();
    for preset in &presets {
        let credential = store.get(preset.id);
        let env_set = std::env::var(preset.env_key)
            .ok()
            .is_some_and(|value| !value.trim().is_empty());
        let status = match credential {
            Some(Credential::ApiKey { .. }) => "Saved API key".to_owned(),
            Some(Credential::Oauth { .. }) => "Saved credential".to_owned(),
            None if env_set => format!("Environment key ({})", preset.env_key),
            None => "Not connected".to_owned(),
        };
        entries.push(Entry {
            label: preset.display_name.to_owned(),
            status,
            kind: EntryKind::Provider {
                provider: preset.id.to_owned(),
            },
            credential_id: credential.is_some().then(|| preset.id.to_owned()),
        });
    }
    entries.push(Entry {
        label: "Custom OpenAI-compatible endpoint".to_owned(),
        status: "Add endpoint and API key".to_owned(),
        kind: EntryKind::Provider {
            provider: "custom".to_owned(),
        },
        credential_id: None,
    });

    let known_ids: std::collections::HashSet<&str> = presets
        .iter()
        .map(|preset| preset.id)
        .chain(["anthropic", "openai-codex", "github-copilot"])
        .collect();
    let saved: Vec<_> = store
        .ids()
        .filter(|id| !known_ids.contains(*id))
        .map(str::to_owned)
        .collect();
    if !saved.is_empty() {
        entries.push(Entry::header("Saved custom credentials"));
        for credential_id in saved {
            let status = match store.get(&credential_id) {
                Some(Credential::ApiKey { .. }) => "Saved API key".to_owned(),
                Some(Credential::Oauth { .. }) => "Logged in".to_owned(),
                None => continue,
            };
            entries.push(Entry {
                label: credential_id.clone(),
                status,
                kind: EntryKind::SavedCredential {
                    credential_id: credential_id.clone(),
                },
                credential_id: Some(credential_id),
            });
        }
    }
    entries
}

pub fn render_provider_login_modal(
    buf: &mut Buffer,
    full_area: Rect,
    state: &mut ProviderLoginModalState,
    compact: bool,
) {
    let theme = Theme::current();
    let shortcuts = match state.mode {
        Mode::Browse => vec![
            Shortcut {
                label: "↑/↓ navigate",
                clickable: false,
                id: 0,
            },
            Shortcut {
                label: "Enter connect",
                clickable: false,
                id: 0,
            },
            Shortcut {
                label: "d logout",
                clickable: false,
                id: 0,
            },
            Shortcut {
                label: "r refresh",
                clickable: false,
                id: 0,
            },
            Shortcut {
                label: "Esc close",
                clickable: false,
                id: 0,
            },
        ],
        Mode::ApiKey(_) => vec![
            Shortcut {
                label: "Tab next field",
                clickable: false,
                id: 0,
            },
            Shortcut {
                label: "Ctrl+S save",
                clickable: false,
                id: 0,
            },
            Shortcut {
                label: "Esc cancel",
                clickable: false,
                id: 0,
            },
        ],
        Mode::WaitingForBrowser { .. } => vec![Shortcut {
            label: "Esc close",
            clickable: false,
            id: 0,
        }],
        Mode::ConfirmLogout { .. } => vec![
            Shortcut {
                label: "y confirm logout",
                clickable: false,
                id: 0,
            },
            Shortcut {
                label: "n cancel",
                clickable: false,
                id: 0,
            },
        ],
    };
    let config = ModalWindowConfig {
        title: TITLE,
        tabs: None,
        shortcuts: &shortcuts,
        sizing: ModalSizing::large().with_compact(compact),
        fold_info: None,
    };
    let Some(ModalContentArea { content, .. }) =
        modal_window::render_modal_window(buf, full_area, &mut state.window, &config, &theme)
    else {
        return;
    };
    if content.height < 3 || content.width < 12 {
        return;
    }

    if let Mode::ApiKey(form) = &state.mode {
        render_api_key_form(buf, content, form, &theme);
        return;
    }
    if let Mode::WaitingForBrowser { provider } = &state.mode {
        let message = format!(
            "Waiting for {provider} sign-in in your browser. Atlas will stay open and refresh when it finishes."
        );
        buf.set_span(
            content.x,
            content.y + 1,
            &Span::styled(message, Style::default().fg(theme.text_primary)),
            content.width,
        );
        return;
    }

    let intro = "Connected accounts and API keys. Credentials are never displayed.";
    buf.set_span(
        content.x,
        content.y,
        &Span::styled(intro, Style::default().fg(theme.gray)),
        content.width,
    );
    let row_start = content.y + 2;
    let rows = content.height.saturating_sub(2) as usize;
    if state.selected < state.scroll {
        state.scroll = state.selected;
    }
    if state.selected >= state.scroll + rows {
        state.scroll = state.selected.saturating_sub(rows.saturating_sub(1));
    }
    for (offset, entry) in state
        .entries
        .iter()
        .enumerate()
        .skip(state.scroll)
        .take(rows)
    {
        let y = row_start + (offset - state.scroll) as u16;
        if y >= content.y + content.height {
            break;
        }
        if matches!(entry.kind, EntryKind::Header) {
            buf.set_span(
                content.x,
                y,
                &Span::styled(
                    &entry.label,
                    Style::default()
                        .fg(theme.accent_user)
                        .add_modifier(Modifier::BOLD),
                ),
                content.width,
            );
            continue;
        }
        let selected = offset == state.selected;
        let bg = if selected {
            theme.bg_visual
        } else {
            theme.bg_base
        };
        buf.set_style(
            Rect {
                x: content.x,
                y,
                width: content.width,
                height: 1,
            },
            Style::default().bg(bg),
        );
        let marker = if selected { "› " } else { "  " };
        buf.set_span(
            content.x,
            y,
            &Span::styled(
                format!("{marker}{}", entry.label),
                Style::default().fg(theme.text_primary).bg(bg),
            ),
            content.width,
        );
        let status_width = entry
            .status
            .len()
            .min(content.width.saturating_sub(4) as usize) as u16;
        let status_x = (content.x + content.width).saturating_sub(status_width);
        buf.set_span(
            status_x,
            y,
            &Span::styled(&entry.status, Style::default().fg(theme.gray).bg(bg)),
            status_width,
        );
    }
    if let Some(notice) = &state.notice {
        let y = content.y + content.height - 1;
        buf.set_span(
            content.x,
            y,
            &Span::styled(notice, Style::default().fg(theme.accent_error)),
            content.width,
        );
    }
    if let Mode::ConfirmLogout { credential_id } = &state.mode {
        let prompt = format!("Log out of {credential_id}? Press y to remove the saved credential.");
        let y = content.y + content.height.saturating_sub(1);
        buf.set_span(
            content.x,
            y,
            &Span::styled(
                prompt,
                Style::default()
                    .fg(theme.accent_error)
                    .add_modifier(Modifier::BOLD),
            ),
            content.width,
        );
    }
}

fn render_api_key_form(buf: &mut Buffer, content: Rect, form: &ApiKeyForm, theme: &Theme) {
    let intro = format!("Connect {} without leaving Atlas.", form.provider);
    buf.set_span(
        content.x,
        content.y,
        &Span::styled(intro, Style::default().fg(theme.gray)),
        content.width,
    );
    let mut fields = vec![("Connection name", form.connection_name.as_str(), false)];
    if let Some(base_url) = &form.base_url {
        fields.push(("API base URL", base_url.as_str(), false));
    }
    fields.push(("API key", form.api_key.as_str(), true));
    fields.push(("Model id", form.model.as_str(), false));
    for (index, (label, value, secret)) in fields.iter().enumerate() {
        let y = content.y + 2 + index as u16 * 2;
        if y >= content.y + content.height.saturating_sub(1) {
            break;
        }
        let selected = index == form.field;
        let shown = if *secret {
            "•".repeat(value.chars().count())
        } else {
            (*value).to_owned()
        };
        let marker = if selected { "›" } else { " " };
        let text = format!("{marker} {label}: {shown}");
        let style = if selected {
            Style::default().fg(theme.text_primary).bg(theme.bg_visual)
        } else {
            Style::default().fg(theme.text_primary)
        };
        buf.set_span(content.x, y, &Span::styled(text, style), content.width);
    }
    let hint_y = content.y + content.height.saturating_sub(1);
    buf.set_span(
        content.x,
        hint_y,
        &Span::styled(
            "Tab/Enter moves fields · Ctrl+S saves. LiteLLM/custom endpoints can use /models discovery later via `atlas login`. ",
            Style::default().fg(theme.gray),
        ),
        content.width,
    );
}

pub fn handle_provider_login_key(
    state: &mut ProviderLoginModalState,
    key: &KeyEvent,
) -> InputOutcome {
    if key.kind == KeyEventKind::Release {
        return InputOutcome::Unchanged;
    }
    if let Mode::ConfirmLogout { credential_id } = &state.mode {
        let credential_id = credential_id.clone();
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => state.remove_credential(&credential_id),
            _ => state.mode = Mode::Browse,
        }
        return InputOutcome::Changed;
    }
    if let Mode::ApiKey(form) = &mut state.mode {
        match key.code {
            KeyCode::Esc => {
                state.mode = Mode::Browse;
                return InputOutcome::Changed;
            }
            KeyCode::Tab | KeyCode::Enter => {
                form.field = (form.field + 1) % form.field_count();
                return InputOutcome::Changed;
            }
            KeyCode::BackTab => {
                form.field = form.field.checked_sub(1).unwrap_or(form.field_count() - 1);
                return InputOutcome::Changed;
            }
            KeyCode::Backspace => {
                form.active_text_mut().pop();
                return InputOutcome::Changed;
            }
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                match form.save() {
                    Ok(()) => {
                        let connection = form.connection_name.clone();
                        state.mode = Mode::Browse;
                        state.refresh();
                        state.notice = Some(format!(
                            "Connected {connection}. Models reload automatically; choose it with /model."
                        ));
                    }
                    Err(error) => state.notice = Some(format!("Could not save API key: {error}")),
                }
                return InputOutcome::Changed;
            }
            KeyCode::Char(c) if crate::input::key::is_text_input_key(key) => {
                form.active_text_mut().push(c);
                return InputOutcome::Changed;
            }
            _ => return InputOutcome::Unchanged,
        }
    }
    if matches!(state.mode, Mode::WaitingForBrowser { .. }) {
        return InputOutcome::Unchanged;
    }
    match key.code {
        KeyCode::Down | KeyCode::Char('j') => {
            state.select_next();
            InputOutcome::Changed
        }
        KeyCode::Up | KeyCode::Char('k') => {
            state.select_previous();
            InputOutcome::Changed
        }
        KeyCode::Char('r') => {
            state.refresh();
            state.notice = Some("Connection status refreshed.".to_owned());
            InputOutcome::Changed
        }
        KeyCode::Char('d') => {
            state.logout_selected();
            InputOutcome::Changed
        }
        KeyCode::Enter => match state.selected_entry().map(|entry| entry.kind.clone()) {
            Some(EntryKind::Provider { provider }) => {
                if xai_grok_shell::agent::connection::api_key_provider_presets()
                    .iter()
                    .any(|preset| preset.id == provider)
                    || provider == "custom"
                {
                    state.mode = Mode::ApiKey(ApiKeyForm::new(provider));
                    InputOutcome::Changed
                } else {
                    state.mode = Mode::WaitingForBrowser {
                        provider: provider.clone(),
                    };
                    InputOutcome::Action(Action::RunProviderLogin { provider })
                }
            }
            Some(EntryKind::SavedCredential { .. }) => {
                state.notice = Some("Press d to log out of this saved credential.".to_owned());
                InputOutcome::Changed
            }
            _ => InputOutcome::Unchanged,
        },
        _ => InputOutcome::Unchanged,
    }
}
