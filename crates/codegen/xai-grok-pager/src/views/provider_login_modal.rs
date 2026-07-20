//! Native provider login and credential-management modal for `/login`.

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
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
    Header { logged_in: bool },
    XaiSession,
    Provider { provider: String },
    SavedCredential { credential_id: String },
}

#[derive(Debug, Clone)]
struct Entry {
    label: String,
    status: String,
    kind: EntryKind,
    credential_id: Option<String>,
    connected: bool,
}

impl Entry {
    fn header(label: impl Into<String>, logged_in: bool) -> Self {
        Self {
            label: label.into(),
            status: String::new(),
            kind: EntryKind::Header { logged_in },
            credential_id: None,
            connected: false,
        }
    }

    fn selectable(&self) -> bool {
        !matches!(&self.kind, EntryKind::Header { .. })
    }
}

#[derive(Debug, Clone)]
enum Mode {
    Browse,
    ApiKey(ApiKeyForm),
    WaitingForBrowser { provider: String },
    RemovingXaiSession,
    ConfirmLogout { target: LogoutTarget },
}

#[derive(Debug, Clone)]
enum LogoutTarget {
    XaiSession,
    SavedCredential { credential_id: String },
}

impl LogoutTarget {
    fn label(&self) -> &str {
        match self {
            Self::XaiSession => "xAI / Grok",
            Self::SavedCredential { credential_id } => credential_id,
        }
    }
}

#[derive(Debug, Clone)]
struct ApiKeyForm {
    provider: String,
    connection_name: String,
    base_url: Option<String>,
    api_key: String,
    model: String,
    /// Full catalog returned by the endpoint's OpenAI-compatible `/models`
    /// resource. It is saved with the connection so `/model` can use it.
    models: Vec<String>,
    /// Fixed endpoint used for model discovery by built-in providers. Custom
    /// and LiteLLM connections use their editable `base_url` instead.
    model_discovery_url: Option<String>,
    discovering_models: bool,
    discovery_request_id: Option<u64>,
    /// Only shown if `/models` is unavailable. Normal LiteLLM/custom setup
    /// never asks the user to know a model identifier.
    manual_model_entry: bool,
    field: usize,
}

impl ApiKeyForm {
    fn new(provider: String) -> Self {
        let preset = xai_grok_shell::agent::connection::api_key_provider_presets()
            .into_iter()
            .find(|preset| preset.id == provider);
        let (connection_name, base_url, model, model_discovery_url) = match preset {
            Some(preset) => (
                provider.clone(),
                (provider == "litellm")
                    .then(|| preset.connection.base_url.clone().unwrap_or_default()),
                (provider != "litellm" && provider != "openrouter")
                    .then(|| preset.default_model.to_owned())
                    .unwrap_or_default(),
                // OpenRouter's public catalog is huge. Let the user opt in to
                // only the model ids they want instead of importing it all.
                (provider != "openrouter")
                    .then(|| preset.connection.base_url.clone())
                    .flatten(),
            ),
            None => (
                "custom".to_owned(),
                Some(String::new()),
                String::new(),
                None,
            ),
        };
        Self {
            provider,
            connection_name,
            base_url,
            api_key: String::new(),
            model,
            models: Vec::new(),
            model_discovery_url,
            discovering_models: false,
            discovery_request_id: None,
            manual_model_entry: false,
            field: 0,
        }
    }

    fn field_count(&self) -> usize {
        2 + usize::from(self.base_url.is_some()) + usize::from(self.needs_model_field())
    }

    fn api_key_field(&self) -> usize {
        1 + usize::from(self.base_url.is_some())
    }

    fn needs_model_field(&self) -> bool {
        self.base_url.is_none() || self.manual_model_entry
    }

    fn active_text_mut(&mut self) -> &mut String {
        match self.field {
            0 => &mut self.connection_name,
            1 if self.base_url.is_some() => self.base_url.as_mut().expect("checked above"),
            field if self.needs_model_field() && field + 1 == self.field_count() => &mut self.model,
            field if field == self.api_key_field() => &mut self.api_key,
            _ => unreachable!("form field index is always valid"),
        }
    }

    fn invalidate_model_discovery(&mut self) {
        self.discovering_models = false;
        self.discovery_request_id = None;
    }

    fn model_field(&self) -> Option<usize> {
        self.needs_model_field().then(|| self.field_count() - 1)
    }

    fn model_discovery_base_url(&self) -> Option<&str> {
        self.base_url
            .as_deref()
            .or(self.model_discovery_url.as_deref())
    }

    fn models_to_save(&self) -> Vec<String> {
        let selected = self
            .model
            .split(',')
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .collect::<Vec<_>>();
        let mut models = self
            .models
            .iter()
            .map(|model| model.trim())
            .filter(|model| !model.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        for model in selected.into_iter().rev() {
            models.retain(|saved| saved != model);
            // The first manually-entered model remains the default while any
            // additional ids become selectable through `/model`.
            models.insert(0, model.to_owned());
        }
        models
    }

    fn save(&self) -> anyhow::Result<()> {
        let models = self.models_to_save();
        xai_grok_shell::agent::login_interactive::save_api_key_connection_for_provider(
            &self.provider,
            self.connection_name.trim(),
            self.api_key.trim(),
            &models,
            self.base_url.clone(),
        )
    }
}

/// State for the native `/login` modal. Credential values are never retained
/// or rendered; the state holds only ids, provider names, and connection state.
#[derive(Debug, Clone)]
pub struct ProviderLoginModalState {
    pub window: ModalWindowState,
    /// Captured during rendering so mouse input can target the form fields.
    content_area: Option<Rect>,
    entries: Vec<Entry>,
    selected: usize,
    scroll: usize,
    mode: Mode,
    notice: Option<String>,
    next_discovery_request_id: u64,
    logout_only: bool,
}

impl ProviderLoginModalState {
    pub fn new(preselected_provider: Option<String>) -> Self {
        let mut state = Self {
            window: ModalWindowState::new(),
            content_area: None,
            entries: build_entries(false),
            selected: 0,
            scroll: 0,
            mode: Mode::Browse,
            notice: None,
            next_discovery_request_id: 0,
            logout_only: false,
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

    /// Build the `/logout` view: only credentials that can actually be
    /// removed are shown, rather than presenting a misleading generic logout.
    pub fn new_logout() -> Self {
        let mut state = Self::new(None);
        state.logout_only = true;
        state.entries = build_entries(true);
        state.selected = 0;
        state.move_to_selectable(1);
        state
    }

    pub fn refresh(&mut self) {
        let selected_provider = self.selected_entry().and_then(|entry| match &entry.kind {
            EntryKind::Provider { provider } => Some(provider.clone()),
            EntryKind::SavedCredential { credential_id } => Some(credential_id.clone()),
            EntryKind::XaiSession => Some("xai".to_owned()),
            EntryKind::Header { .. } => None,
        });
        self.entries = build_entries(self.logout_only);
        self.selected = selected_provider
            .and_then(|needle| {
                self.entries.iter().position(|entry| match &entry.kind {
                    EntryKind::Provider { provider } => provider == &needle,
                    EntryKind::SavedCredential { credential_id } => credential_id == &needle,
                    EntryKind::XaiSession => needle == "xai",
                    EntryKind::Header { .. } => false,
                })
            })
            .unwrap_or(0);
        self.move_to_selectable(1);
    }

    /// Start a native model discovery request. Built-in OpenAI-compatible
    /// providers use their fixed endpoint; LiteLLM and custom connections use
    /// the editable base URL. The actual HTTP request runs as an async app
    /// effect.
    fn start_model_discovery(&mut self) -> Result<(), String> {
        let Mode::ApiKey(form) = &mut self.mode else {
            return Err("Open an API-key connection first.".to_owned());
        };
        let Some(base_url) = form.model_discovery_base_url() else {
            return Err("This provider does not expose a /models endpoint.".to_owned());
        };
        if base_url.trim().is_empty() {
            return Err("Enter an API base URL before loading models.".to_owned());
        }
        if form.api_key.trim().is_empty() {
            return Err("Enter an API key before loading models.".to_owned());
        }
        if form.discovering_models {
            return Err("Models are already loading.".to_owned());
        }
        self.next_discovery_request_id = self.next_discovery_request_id.wrapping_add(1).max(1);
        form.discovering_models = true;
        form.discovery_request_id = Some(self.next_discovery_request_id);
        self.notice = Some("Loading models from the endpoint…".to_owned());
        Ok(())
    }

    /// Return the transient credentials for the in-flight discovery effect.
    /// They are never rendered or persisted until the user saves the form.
    pub fn model_discovery_credentials(&self) -> Option<(u64, String, String)> {
        let Mode::ApiKey(form) = &self.mode else {
            return None;
        };
        if !form.discovering_models {
            return None;
        }
        let request_id = form.discovery_request_id?;
        let base_url = form
            .model_discovery_base_url()?
            .trim()
            .trim_end_matches('/');
        (!base_url.is_empty() && !form.api_key.trim().is_empty()).then(|| {
            (
                request_id,
                base_url.to_owned(),
                form.api_key.trim().to_owned(),
            )
        })
    }

    /// Apply a `/models` response to the focused form. The first discovered
    /// model becomes the default; all results are retained for `/model`.
    pub fn finish_model_discovery(&mut self, request_id: u64, result: Result<&[String], &str>) {
        let Mode::ApiKey(form) = &mut self.mode else {
            return;
        };
        if form.discovery_request_id != Some(request_id) {
            return;
        }
        form.discovering_models = false;
        form.discovery_request_id = None;
        match result {
            Ok(models) if !models.is_empty() => {
                form.models = models.to_vec();
                if !form.models.iter().any(|model| model == &form.model) {
                    form.model = form.models[0].clone();
                }
                self.notice = Some(format!("Loaded {} models from /models.", form.models.len()));
            }
            Ok(_) => {
                form.manual_model_entry = true;
                form.field = form.model_field().expect("manual field enabled");
                self.notice =
                    Some("The endpoint returned no models; enter an id manually.".to_owned());
            }
            Err(error) => {
                form.manual_model_entry = true;
                form.field = form.model_field().expect("manual field enabled");
                self.notice = Some(format!("Could not fetch /models: {error}"));
            }
        }
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
        let target = self.selected_entry().and_then(|entry| match &entry.kind {
            EntryKind::XaiSession => Some(LogoutTarget::XaiSession),
            EntryKind::Provider { .. } | EntryKind::SavedCredential { .. } => entry
                .credential_id
                .as_ref()
                .map(|credential_id| LogoutTarget::SavedCredential {
                    credential_id: credential_id.clone(),
                }),
            EntryKind::Header { .. } => None,
        });
        match target {
            Some(target) => self.mode = Mode::ConfirmLogout { target },
            None => {
                self.notice = Some(
                    "This provider uses an environment variable. Remove it from your shell to disconnect."
                        .to_owned(),
                );
            }
        }
    }

    fn remove_credential(&mut self, credential_id: &str) {
        let result =
            xai_grok_shell::agent::login_interactive::remove_saved_credential(credential_id);
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

    /// Finish removal of the first-party session after the ACP agent confirms
    /// it. Keeping the modal open lets the user continue managing other keys.
    pub fn finish_xai_logout(&mut self, result: Result<(), &str>) {
        self.mode = Mode::Browse;
        self.refresh();
        self.notice = Some(match result {
            Ok(()) => "Logged out of xAI / Grok.".to_owned(),
            Err(error) => format!("Could not log out of xAI / Grok: {error}"),
        });
    }
}

fn build_entries(logout_only: bool) -> Vec<Entry> {
    use xai_grok_shell::agent::credential_store::{Credential, CredentialStore};

    let path = CredentialStore::default_path();
    let store = CredentialStore::load(&path).unwrap_or_default();
    let xai_session =
        xai_grok_shell::auth::read_auth_json(&xai_grok_config::grok_home().join("auth.json"))
            .ok()
            .and_then(|store| {
                xai_grok_shell::auth::lookup_auth(
                    &store,
                    &xai_grok_shell::auth::GrokComConfig::default().auth_scope(),
                )
            })
            .is_some();

    let presets = xai_grok_shell::agent::connection::api_key_provider_presets();
    let known_ids: std::collections::HashSet<&str> = presets
        .iter()
        .map(|preset| preset.id)
        .chain(["anthropic", "openai-codex", "github-copilot"])
        .collect();

    let mut connected = Vec::new();
    if xai_session {
        connected.push(Entry {
            label: "xAI / Grok".to_owned(),
            status: "Logged in".to_owned(),
            kind: EntryKind::XaiSession,
            credential_id: None,
            connected: true,
        });
    }
    // NOTE: `anthropic-subscription` (Claude Pro/Max via reverse-engineered
    // OAuth) has been removed. Claude subscription use now runs through the
    // Claude Agent SDK harness — authenticate with `claude login` (surfaced via
    // `/login claude-agent`), not this OAuth modal.
    for (provider, label, credential_id) in [
        (
            "openai-codex",
            "ChatGPT Plus / Pro (Codex)",
            Some("openai-codex"),
        ),
        ("github-copilot", "GitHub Copilot", Some("github-copilot")),
    ] {
        if let Some(credential_id) = credential_id
            && let Some(credential) = store.get(credential_id)
        {
            let status = match credential {
                Credential::Oauth { .. } => "Logged in".to_owned(),
                Credential::ApiKey { .. } => "Saved API key".to_owned(),
            };
            connected.push(Entry {
                label: label.to_owned(),
                status,
                kind: EntryKind::Provider {
                    provider: provider.to_owned(),
                },
                credential_id: Some(credential_id.to_owned()),
                connected: true,
            });
        }
    }
    for preset in &presets {
        if let Some(credential) = store.get(preset.id) {
            let status = match credential {
                Credential::ApiKey { .. } => "Saved API key".to_owned(),
                Credential::Oauth { .. } => "Saved credential".to_owned(),
            };
            connected.push(Entry {
                label: preset.display_name.to_owned(),
                status,
                kind: EntryKind::Provider {
                    provider: preset.id.to_owned(),
                },
                credential_id: Some(preset.id.to_owned()),
                connected: true,
            });
        }
    }
    for credential_id in store
        .ids()
        .filter(|id| !known_ids.contains(*id))
        .map(str::to_owned)
    {
        let status = match store.get(&credential_id) {
            Some(Credential::ApiKey { .. }) => "Saved API key".to_owned(),
            Some(Credential::Oauth { .. }) => "Logged in".to_owned(),
            None => continue,
        };
        connected.push(Entry {
            label: credential_id.clone(),
            status,
            kind: EntryKind::SavedCredential {
                credential_id: credential_id.clone(),
            },
            credential_id: Some(credential_id),
            connected: true,
        });
    }

    let mut entries = vec![Entry::header("Logged in", true)];
    if connected.is_empty() {
        entries.push(Entry::header("No saved provider credentials.", false));
    } else {
        entries.extend(connected);
    }
    if logout_only {
        return entries;
    }

    entries.push(Entry::header("Other providers", false));
    for (provider, label, credential_id) in [
        ("xai", "xAI / Grok", None),
        (
            "openai-codex",
            "ChatGPT Plus / Pro (Codex)",
            Some("openai-codex"),
        ),
        ("github-copilot", "GitHub Copilot", Some("github-copilot")),
    ] {
        let already_connected = (provider == "xai" && xai_session)
            || credential_id.is_some_and(|id| store.get(id).is_some());
        if !already_connected {
            entries.push(Entry {
                label: label.to_owned(),
                status: "Subscription".to_owned(),
                kind: EntryKind::Provider {
                    provider: provider.to_owned(),
                },
                credential_id: None,
                connected: false,
            });
        }
    }
    for preset in presets {
        if store.get(preset.id).is_none() {
            let env_set = std::env::var(preset.env_key)
                .ok()
                .is_some_and(|value| !value.trim().is_empty());
            entries.push(Entry {
                label: preset.display_name.to_owned(),
                status: if env_set {
                    format!("Environment key ({})", preset.env_key)
                } else {
                    "API key".to_owned()
                },
                kind: EntryKind::Provider {
                    provider: preset.id.to_owned(),
                },
                credential_id: None,
                connected: false,
            });
        }
    }
    entries.push(Entry {
        label: "Custom OpenAI-compatible endpoint".to_owned(),
        status: "Add endpoint and API key".to_owned(),
        kind: EntryKind::Provider {
            provider: "custom".to_owned(),
        },
        credential_id: None,
        connected: false,
    });
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
        Mode::RemovingXaiSession => vec![Shortcut {
            label: "Removing credential…",
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
        state.content_area = None;
        return;
    };
    state.content_area = Some(content);
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
    if matches!(&state.mode, Mode::RemovingXaiSession) {
        buf.set_span(
            content.x,
            content.y + 1,
            &Span::styled(
                "Removing your xAI / Grok session…",
                Style::default().fg(theme.text_primary),
            ),
            content.width,
        );
        return;
    }

    let intro = if state.logout_only {
        "Saved credentials. Select one to remove it; credentials are never displayed."
    } else {
        "Connected accounts and API keys. Credentials are never displayed."
    };
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
        if let EntryKind::Header { logged_in } = &entry.kind {
            buf.set_span(
                content.x,
                y,
                &Span::styled(
                    &entry.label,
                    Style::default()
                        .fg(if *logged_in {
                            theme.accent_success
                        } else {
                            theme.accent_user
                        })
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
            &Span::styled(
                &entry.status,
                Style::default()
                    .fg(if entry.connected {
                        theme.accent_success
                    } else {
                        theme.gray
                    })
                    .bg(bg),
            ),
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
    if let Mode::ConfirmLogout { target } = &state.mode {
        let prompt = format!(
            "Log out of {}? Press y to remove the saved credential.",
            target.label()
        );
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
    let intro = if form.provider == "openrouter" {
        "Connect OpenRouter with the model ids you want to use."
    } else {
        "Connect a provider without leaving Atlas."
    };
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
    if form.needs_model_field() {
        fields.push((
            if form.provider == "openrouter" {
                "Models (comma-separated)"
            } else {
                "Model id"
            },
            form.model.as_str(),
            false,
        ));
    }
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
    if form.model_discovery_base_url().is_some() {
        let y = content.y + 2 + fields.len() as u16 * 2;
        let status = if form.discovering_models {
            "Models: loading from /models…".to_owned()
        } else if form.models.is_empty() {
            "Models: paste an API key to load the endpoint catalog.".to_owned()
        } else {
            format!(
                "Default model: {}  ({} discovered)",
                form.model,
                form.models.len()
            )
        };
        buf.set_span(
            content.x,
            y,
            &Span::styled(status, Style::default().fg(theme.gray)),
            content.width,
        );
    }
    let hint = if form.provider == "openrouter" {
        "Enter model ids separated by commas · Ctrl+S saves each one."
    } else {
        "Pasting an API key loads all models · Ctrl+R reloads · Ctrl+S saves."
    };
    let hint_y = content.y + content.height.saturating_sub(1);
    buf.set_span(
        content.x,
        hint_y,
        &Span::styled(hint, Style::default().fg(theme.gray)),
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
    if let Mode::ConfirmLogout { target } = &state.mode {
        let target = target.clone();
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => match target {
                LogoutTarget::XaiSession => {
                    state.mode = Mode::RemovingXaiSession;
                    return InputOutcome::Action(Action::LogoutXaiFromProviderModal);
                }
                LogoutTarget::SavedCredential { credential_id } => {
                    state.remove_credential(&credential_id)
                }
            },
            _ => state.mode = Mode::Browse,
        }
        return InputOutcome::Changed;
    }
    // Most terminals emit Cmd/Ctrl+V as Event::Paste, but Ghostty and
    // similar terminals can send it as a key chord. Support both paths.
    if matches!(state.mode, Mode::ApiKey(_)) && crate::input::key::is_paste_key(key) {
        return crate::clipboard::system_clipboard_get().map_or(InputOutcome::Unchanged, |text| {
            paste_and_maybe_discover_models(state, &text)
        });
    }
    if matches!(state.mode, Mode::ApiKey(_))
        && matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
    {
        return match state.start_model_discovery() {
            Ok(()) => InputOutcome::Action(Action::DiscoverProviderModels),
            Err(error) => {
                state.notice = Some(error);
                InputOutcome::Changed
            }
        };
    }
    if let Mode::ApiKey(form) = &mut state.mode {
        match key.code {
            KeyCode::Esc => {
                state.mode = Mode::Browse;
                return InputOutcome::Changed;
            }
            KeyCode::Tab | KeyCode::Enter => {
                let leaving_api_key = form.field == form.api_key_field();
                form.field = (form.field + 1) % form.field_count();
                let load_models = leaving_api_key
                    && form.models.is_empty()
                    && form.model_discovery_base_url().is_some()
                    && !form.discovering_models;
                if load_models {
                    // Drop the form borrow before starting the stateful async
                    // request below.
                } else {
                    return InputOutcome::Changed;
                }
            }
            KeyCode::BackTab => {
                form.field = form.field.checked_sub(1).unwrap_or(form.field_count() - 1);
                return InputOutcome::Changed;
            }
            KeyCode::Backspace => {
                form.invalidate_model_discovery();
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
                form.invalidate_model_discovery();
                form.active_text_mut().push(c);
                return InputOutcome::Changed;
            }
            _ => return InputOutcome::Unchanged,
        }
    }
    // Leaving the API-key field starts model discovery automatically. If
    // validation fails, keep the form open with a clear message and allow the
    // user to correct the missing value.
    if matches!(state.mode, Mode::ApiKey(_)) {
        return match state.start_model_discovery() {
            Ok(()) => InputOutcome::Action(Action::DiscoverProviderModels),
            Err(error) => {
                state.notice = Some(error);
                InputOutcome::Changed
            }
        };
    }
    if matches!(
        state.mode,
        Mode::WaitingForBrowser { .. } | Mode::RemovingXaiSession
    ) {
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
        KeyCode::Enter if state.logout_only => {
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
            Some(EntryKind::XaiSession) => {
                state.notice = Some("Press d to log out of this saved credential.".to_owned());
                InputOutcome::Changed
            }
            _ => InputOutcome::Unchanged,
        },
        _ => InputOutcome::Unchanged,
    }
}

/// Paste into the focused API-key form field. Terminal paste payloads can
/// include a trailing newline, which must not become part of a credential or
/// endpoint value.
pub fn handle_provider_login_paste(
    state: &mut ProviderLoginModalState,
    text: &str,
) -> InputOutcome {
    paste_and_maybe_discover_models(state, text)
}

fn paste_and_maybe_discover_models(
    state: &mut ProviderLoginModalState,
    text: &str,
) -> InputOutcome {
    let pasted = paste_into_active_field(state, text);
    if !matches!(pasted, InputOutcome::Changed) {
        return pasted;
    }
    let should_discover = matches!(&state.mode, Mode::ApiKey(form)
        if form.field == form.api_key_field()
            && form.model_discovery_base_url().is_some()
            && !form.api_key.trim().is_empty()
            && form.models.is_empty()
            && !form.discovering_models);
    if !should_discover {
        return pasted;
    }
    match state.start_model_discovery() {
        Ok(()) => InputOutcome::Action(Action::DiscoverProviderModels),
        Err(error) => {
            state.notice = Some(error);
            InputOutcome::Changed
        }
    }
}

fn paste_into_active_field(state: &mut ProviderLoginModalState, text: &str) -> InputOutcome {
    let cleaned: String = text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
    if cleaned.is_empty() {
        return InputOutcome::Unchanged;
    }
    let Mode::ApiKey(form) = &mut state.mode else {
        return InputOutcome::Unchanged;
    };
    form.invalidate_model_discovery();
    form.active_text_mut().push_str(&cleaned);
    InputOutcome::Changed
}

/// Handle mouse selection inside the provider modal. The common modal chrome
/// (close button and click-outside) is handled before this function.
pub fn handle_provider_login_mouse(
    state: &mut ProviderLoginModalState,
    kind: MouseEventKind,
    column: u16,
    row: u16,
) -> InputOutcome {
    if !matches!(kind, MouseEventKind::Down(MouseButton::Left)) {
        return InputOutcome::Unchanged;
    }
    let Some(content) = state.content_area else {
        return InputOutcome::Unchanged;
    };
    if !content.contains((column, row).into()) {
        return InputOutcome::Unchanged;
    }

    if let Mode::ApiKey(form) = &mut state.mode {
        let first_field_y = content.y.saturating_add(2);
        if row < first_field_y || (row - first_field_y) % 2 != 0 {
            return InputOutcome::Unchanged;
        }
        let field = ((row - first_field_y) / 2) as usize;
        if field < form.field_count() {
            form.field = field;
            return InputOutcome::Changed;
        }
        return InputOutcome::Unchanged;
    }

    if matches!(state.mode, Mode::Browse) {
        let first_row_y = content.y.saturating_add(2);
        if row >= first_row_y {
            let index = state.scroll + (row - first_row_y) as usize;
            if state.entries.get(index).is_some_and(Entry::selectable) {
                state.selected = index;
                return InputOutcome::Changed;
            }
        }
    }
    InputOutcome::Unchanged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_inserts_cleaned_text_into_the_focused_field() {
        let mut state = ProviderLoginModalState::new(None);
        state.mode = Mode::ApiKey(ApiKeyForm::new("litellm".to_owned()));
        if let Mode::ApiKey(form) = &mut state.mode {
            form.field = 2; // API key when LiteLLM includes the base URL field.
        }

        assert!(matches!(
            handle_provider_login_paste(&mut state, "sk-secret\r\n"),
            InputOutcome::Action(Action::DiscoverProviderModels)
        ));
        let Mode::ApiKey(form) = &state.mode else {
            panic!("form should stay open");
        };
        assert_eq!(form.api_key, "sk-secret");
    }

    #[test]
    fn stale_model_discovery_result_does_not_update_a_replaced_form() {
        let mut state = ProviderLoginModalState::new(None);
        state.mode = Mode::ApiKey(ApiKeyForm::new("litellm".to_owned()));
        if let Mode::ApiKey(form) = &mut state.mode {
            form.api_key = "secret".to_owned();
        }
        state.start_model_discovery().unwrap();
        let (request_id, _, _) = state.model_discovery_credentials().unwrap();

        state.mode = Mode::ApiKey(ApiKeyForm::new("custom".to_owned()));
        state.finish_model_discovery(request_id, Ok(&["stale-model".to_owned()]));

        let Mode::ApiKey(form) = &state.mode else {
            panic!("replacement form should stay open");
        };
        assert!(form.models.is_empty());
        assert!(form.model.is_empty());
    }

    #[test]
    fn openrouter_saves_only_the_manually_selected_models() {
        let form = ApiKeyForm::new("openrouter".to_owned());
        assert!(form.model_discovery_base_url().is_none());
        assert!(form.model.is_empty());

        let form = ApiKeyForm {
            model: "anthropic/claude-sonnet-4, openai/gpt-5, openai/gpt-5".to_owned(),
            ..form
        };
        assert_eq!(
            form.models_to_save(),
            vec![
                "anthropic/claude-sonnet-4".to_owned(),
                "openai/gpt-5".to_owned(),
            ]
        );
    }

    #[test]
    fn confirming_xai_logout_stays_in_the_provider_manager() {
        let mut state = ProviderLoginModalState::new_logout();
        state.mode = Mode::ConfirmLogout {
            target: LogoutTarget::XaiSession,
        };

        assert!(matches!(
            handle_provider_login_key(
                &mut state,
                &KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            ),
            InputOutcome::Action(Action::LogoutXaiFromProviderModal)
        ));
        assert!(matches!(state.mode, Mode::RemovingXaiSession));
    }

    #[test]
    fn clicking_a_form_row_focuses_that_field() {
        let mut state = ProviderLoginModalState::new(None);
        state.mode = Mode::ApiKey(ApiKeyForm::new("litellm".to_owned()));
        state.content_area = Some(Rect::new(10, 5, 80, 20));

        assert!(matches!(
            handle_provider_login_mouse(
                &mut state,
                MouseEventKind::Down(MouseButton::Left),
                20,
                11
            ),
            InputOutcome::Changed
        ));
        let Mode::ApiKey(form) = &state.mode else {
            panic!("form should stay open");
        };
        assert_eq!(form.field, 2); // content y + 2 + 2 * 2 = API key
    }
}
