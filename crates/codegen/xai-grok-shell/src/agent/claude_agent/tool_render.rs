//! Projection of Claude Agent SDK tool activity onto **native ACP tool cards**.
//!
//! The harness runs its own tools, so Atlas never builds a [`ToolInput`] for
//! them and none of `send_tool_call_start`'s typed rendering applies. Without a
//! translation layer the only thing left is transcript text, which is what the
//! first cut of this backend emitted — harness turns showed `⚙ Bash {...}` lines
//! instead of the Run / Read / Edit cards every other turn produces.
//!
//! This module is that translation layer: it maps a harness tool name plus its
//! JSON input onto the same wire shape the sampler path emits, so the TUI's
//! renderer cannot tell the two apart. Three things matter to that renderer,
//! and all three are reproduced here:
//!
//!   * **`kind`** picks the block type (`Execute` / `Read` / `Edit` / `Search` /
//!     `Fetch` / `Other`).
//!   * **`raw_input`** carries the display fields the block reads back
//!     (`command`, `file_path`, `pattern`, `query`, `url`, …) plus the
//!     `variant` tag that Atlas's [`ToolInput`] serialization stamps — the
//!     discriminator the TUI uses for write-vs-edit, todo suppression and
//!     web-search detection.
//!   * **`raw_output`** is the typed [`ToolOutput`] a block needs to show a
//!     body (command output, file contents, match counts). The harness only
//!     hands back display text, so the equivalent typed value is synthesized
//!     from it.
//!
//! Everything here is pure and total: unknown tools (including MCP tools the
//! harness exposes) fall through to a generic `Other` card rather than
//! disappearing.
//!
//! [`ToolInput`]: xai_grok_tools::types::input::ToolInput

use std::path::{Path, PathBuf};

use agent_client_protocol as acp;
use serde_json::{Map, Value};
use xai_grok_tools::types::output::{
    BashOutput, FileContent, GrepFileMatch, GrepLineMatch, GrepSearchOutput, ReadFileOutput,
    ToolOutput,
};

/// The `SessionUpdate::ToolCall` payload for one harness tool invocation.
#[derive(Debug, Clone)]
pub struct ToolCard {
    pub title: String,
    pub kind: acp::ToolKind,
    /// Harness input plus the `variant` tag, in the shape the TUI reads.
    pub raw_input: Value,
    pub locations: Vec<acp::ToolCallLocation>,
    /// Pre-execution content — the edit diff for `Edit`/`Write`/`MultiEdit`.
    pub content: Vec<acp::ToolCallContent>,
    /// File whose current text places [`content`](Self::content) diffs at real
    /// line numbers. `Some` only for targeted edits (a whole-file write starts
    /// at line 1 by definition); the caller reads it and calls
    /// [`anchor_diff_lines`].
    pub diff_anchor: Option<PathBuf>,
}

/// Build the tool card for a harness `tool_use`.
///
/// `cwd` is the session working directory, used to absolutize the relative
/// paths the harness may report and to peel a redundant leading `cd <cwd>` off
/// a command, exactly as the sampler path does.
pub fn tool_card(name: &str, input: &Value, cwd: &Path) -> ToolCard {
    match name {
        "Bash" => {
            let command = str_field(input, "command").unwrap_or_default();
            let display = xai_grok_tools::util::strip_redundant_session_cd(command, cwd);
            ToolCard {
                title: format!("Execute `{display}`"),
                kind: acp::ToolKind::Execute,
                raw_input: tagged(input, "Bash"),
                locations: Vec::new(),
                content: Vec::new(),
                diff_anchor: None,
            }
        }
        "BashOutput" | "KillShell" | "KillBash" => generic_card(name, input, None),
        "Read" => {
            let path = str_field(input, "file_path").unwrap_or_default();
            ToolCard {
                title: format!("Read `{path}`"),
                kind: acp::ToolKind::Read,
                raw_input: tagged(input, "ReadFile"),
                locations: vec![
                    acp::ToolCallLocation::new(path).line(
                        input
                            .get("offset")
                            .and_then(Value::as_u64)
                            .map(|o| o.max(1) as u32),
                    ),
                ],
                content: Vec::new(),
                diff_anchor: None,
            }
        }
        "Write" => {
            let path = str_field(input, "file_path").unwrap_or_default();
            let absolute = absolutize(cwd, path);
            ToolCard {
                title: format!("Write `{path}`"),
                kind: acp::ToolKind::Edit,
                raw_input: tagged(input, "Write"),
                locations: vec![acp::ToolCallLocation::new(path)],
                content: vec![acp::ToolCallContent::from(
                    acp::Diff::new(
                        absolute,
                        str_field(input, "content").unwrap_or_default().to_owned(),
                    )
                    // A write replaces the file wholesale; the renderer shows
                    // every line as added, matching `ToolInput::Write`.
                    .old_text(Some(String::new())),
                )],
                diff_anchor: None,
            }
        }
        "Edit" | "MultiEdit" | "NotebookEdit" => {
            let path = str_field(input, "file_path")
                .or_else(|| str_field(input, "notebook_path"))
                .unwrap_or_default();
            let absolute = absolutize(cwd, path);
            let content: Vec<acp::ToolCallContent> = edit_pairs(name, input)
                .into_iter()
                .map(|(old, new)| {
                    acp::ToolCallContent::from(
                        acp::Diff::new(absolute.clone(), new).old_text(Some(old)),
                    )
                })
                .collect();
            ToolCard {
                title: format!("Edit `{path}`"),
                kind: acp::ToolKind::Edit,
                // `SearchReplace` is Atlas's targeted-edit variant: it keeps the
                // renderer off the whole-file "Creating " path that `Write`
                // takes.
                raw_input: tagged(input, "SearchReplace"),
                locations: vec![acp::ToolCallLocation::new(path)],
                diff_anchor: (!content.is_empty()).then_some(absolute),
                content,
            }
        }
        "Grep" | "Glob" => {
            let pattern = str_field(input, "pattern").unwrap_or_default();
            ToolCard {
                title: pattern.to_owned(),
                kind: acp::ToolKind::Search,
                // Glob's `pattern`/`path` are a subset of Grep's input, so the
                // same variant renders both.
                raw_input: tagged(input, "Grep"),
                locations: Vec::new(),
                content: Vec::new(),
                diff_anchor: None,
            }
        }
        "WebSearch" => {
            let query = str_field(input, "query").unwrap_or_default();
            ToolCard {
                title: format!("Web search: \"{query}\""),
                kind: acp::ToolKind::Search,
                raw_input: tagged(input, "WebSearch"),
                locations: Vec::new(),
                content: Vec::new(),
                diff_anchor: None,
            }
        }
        "WebFetch" => {
            let url = str_field(input, "url").unwrap_or_default();
            ToolCard {
                title: format!("Fetch: {url}"),
                kind: acp::ToolKind::Fetch,
                raw_input: tagged(input, "WebFetch"),
                locations: Vec::new(),
                content: Vec::new(),
                diff_anchor: None,
            }
        }
        "TodoWrite" => ToolCard {
            // Suppressed from scrollback by the `TodoWrite` variant tag — the
            // plan pane renders it instead, fed by [`plan_update`].
            title: "Updating plan".to_owned(),
            kind: acp::ToolKind::Think,
            raw_input: tagged(input, "TodoWrite"),
            locations: Vec::new(),
            content: Vec::new(),
            diff_anchor: None,
        },
        "Task" => {
            let description = str_field(input, "description").unwrap_or("subagent");
            let label = match str_field(input, "subagent_type") {
                Some(kind) if !kind.is_empty() => format!("Task({kind}): {description}"),
                _ => format!("Task: {description}"),
            };
            // Deliberately *not* tagged `Task`: that variant tells the TUI to
            // suppress the card in favour of the subagent pane, which is driven
            // by spawn notifications Atlas never sees for harness subagents.
            generic_card_titled(label, input)
        }
        "Skill" => {
            let skill = str_field(input, "command")
                .or_else(|| str_field(input, "skill"))
                .unwrap_or_default();
            // `Skill: <name>` is the shape the TUI splits on for skill cards.
            generic_card_titled(format!("Skill: {skill}"), input)
        }
        "ExitPlanMode" => generic_card_titled("Plan: Exit".to_owned(), input),
        "EnterPlanMode" => generic_card_titled("Plan: Enter".to_owned(), input),
        other => generic_card(other, input, mcp_tool_label(other)),
    }
}

/// The `ToolCallUpdate` fields for a harness `tool_result`.
///
/// `text` is the harness's own rendering of the result; for the kinds whose
/// block needs structured data it is reshaped into the matching typed
/// [`ToolOutput`] so the card shows a real body instead of an empty header.
pub fn result_fields(
    name: &str,
    input: &Value,
    text: &str,
    is_error: bool,
    cwd: &Path,
) -> acp::ToolCallUpdateFields {
    let status = if is_error {
        acp::ToolCallStatus::Failed
    } else {
        acp::ToolCallStatus::Completed
    };
    let fields = acp::ToolCallUpdateFields::new().status(Some(status));
    match name {
        "Bash" => fields
            .content(Some(text_content(text)))
            .raw_output(to_value(ToolOutput::Bash(bash_output(
                input, text, is_error,
            )))),
        "Read" if !is_error => fields.raw_output(to_value(ToolOutput::ReadFile(
            ReadFileOutput::FileContent(file_content(input, text, cwd)),
        ))),
        "Read" => fields
            .content(Some(text_content(text)))
            .raw_output(to_value(ToolOutput::ReadFile(
                ReadFileOutput::FileReadError(text.to_owned()),
            ))),
        "Grep" | "Glob" if !is_error => {
            fields.raw_output(to_value(ToolOutput::GrepSearch(grep_output(name, text))))
        }
        // Edits keep the diff content posted at start; replacing it with the
        // harness's "applied" text would drop the rendered hunks.
        "Edit" | "MultiEdit" | "NotebookEdit" | "Write" if !is_error => fields,
        _ => fields.content(Some(text_content(text))),
    }
}

/// The plan notification for a harness `TodoWrite`, or `None` when the input
/// carries no todo list. The card itself is suppressed, so this is the only
/// surface harness todos reach.
pub fn plan_update(input: &Value) -> Option<acp::Plan> {
    let todos = input.get("todos")?.as_array()?;
    let entries = todos
        .iter()
        .filter_map(|todo| {
            // `activeForm` is the harness's in-progress phrasing of the same
            // item; `content` is the canonical one.
            let content = str_field(todo, "content")
                .or_else(|| str_field(todo, "activeForm"))?
                .to_owned();
            Some(acp::PlanEntry::new(
                content,
                acp::PlanEntryPriority::Medium,
                match str_field(todo, "status") {
                    Some("in_progress") => acp::PlanEntryStatus::InProgress,
                    Some("completed") => acp::PlanEntryStatus::Completed,
                    _ => acp::PlanEntryStatus::Pending,
                },
            ))
        })
        .collect();
    Some(acp::Plan::new(entries))
}

/// Place each diff in `content` at the line where its `old_text` actually
/// occurs in `file_text`, so hunks render with real line numbers instead of
/// the renderer's line-1 fallback. Diffs whose `old_text` is absent (a new
/// file, or text the harness already rewrote) keep the fallback.
pub fn anchor_diff_lines(content: &mut [acp::ToolCallContent], file_text: &str) {
    for item in content {
        let acp::ToolCallContent::Diff(diff) = item else {
            continue;
        };
        let Some(old) = diff.old_text.as_deref().filter(|s| !s.is_empty()) else {
            continue;
        };
        let Some(byte_pos) = file_text.find(old) else {
            continue;
        };
        let line = file_text[..byte_pos].matches('\n').count() + 1;
        let mut meta = diff.meta.take().unwrap_or_default();
        meta.insert("old_line".to_owned(), line.into());
        meta.insert("new_line".to_owned(), line.into());
        diff.meta = Some(meta);
    }
}

/// A card for a tool with no special rendering — named by `label` when the
/// harness name has a friendlier form (MCP tools), else by the name itself.
fn generic_card(name: &str, input: &Value, label: Option<String>) -> ToolCard {
    generic_card_titled(label.unwrap_or_else(|| name.to_owned()), input)
}

fn generic_card_titled(title: String, input: &Value) -> ToolCard {
    ToolCard {
        title,
        kind: acp::ToolKind::Other,
        // No `variant`: an unrecognized tag would send the renderer down a
        // typed path whose fields this input does not have.
        raw_input: input.clone(),
        locations: Vec::new(),
        content: Vec::new(),
        diff_anchor: None,
    }
}

/// `mcp__<server>__<tool>` rendered as `<server>: <tool>`.
fn mcp_tool_label(name: &str) -> Option<String> {
    let rest = name.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    Some(format!("{server}: {tool}"))
}

/// The `(old, new)` pairs an edit tool applies, one per rendered diff.
fn edit_pairs(name: &str, input: &Value) -> Vec<(String, String)> {
    if name == "NotebookEdit" {
        // Cell-level replacement: the old source is not in the input, so the
        // diff shows the new cell alone.
        return vec![(
            String::new(),
            str_field(input, "new_source")
                .unwrap_or_default()
                .to_owned(),
        )];
    }
    if let Some(edits) = input.get("edits").and_then(Value::as_array) {
        return edits
            .iter()
            .map(|edit| {
                (
                    str_field(edit, "old_string").unwrap_or_default().to_owned(),
                    str_field(edit, "new_string").unwrap_or_default().to_owned(),
                )
            })
            .collect();
    }
    vec![(
        str_field(input, "old_string")
            .unwrap_or_default()
            .to_owned(),
        str_field(input, "new_string")
            .unwrap_or_default()
            .to_owned(),
    )]
}

/// Synthesize the [`BashOutput`] an execute card renders from the harness's
/// result text. The harness reports no exit status, so failure is taken from
/// the `is_error` flag on the tool result.
fn bash_output(input: &Value, text: &str, is_error: bool) -> BashOutput {
    BashOutput {
        output: text.as_bytes().to_vec(),
        output_for_prompt: text.to_owned(),
        exit_code: i32::from(is_error),
        command: str_field(input, "command").unwrap_or_default().to_owned(),
        truncated: false,
        signal: None,
        timed_out: false,
        description: str_field(input, "description").map(str::to_owned),
        current_dir: String::new(),
        output_file: String::new(),
        total_bytes: text.len(),
        output_delta: None,
        was_bare_echo: false,
    }
}

/// Synthesize the [`FileContent`] a read card renders.
///
/// The harness returns the file with a line-number gutter, which the card would
/// otherwise syntax-highlight as part of the source, so the gutter is stripped
/// back off. `total_lines` is the last line actually returned: the harness never
/// reports the file's real length, and reporting a shorter range than was read
/// would clip the card's displayed range.
fn file_content(input: &Value, text: &str, cwd: &Path) -> FileContent {
    let body = strip_line_gutter(text);
    let offset = input
        .get("offset")
        .and_then(Value::as_u64)
        .map(|o| o as usize);
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .map(|l| l as usize);
    let returned_lines = body.lines().count();
    FileContent {
        content: body.clone(),
        content_concise: None,
        absolute_path: absolutize(cwd, str_field(input, "file_path").unwrap_or_default()),
        offset,
        limit,
        raw_output: body,
        total_lines: offset.unwrap_or(0) + returned_lines,
        extracted_images: Vec::new(),
    }
}

/// Drop the harness's `   12→` (or `   12\t`) read gutter from every line, and
/// with it any trailing system-reminder block, which is prompt plumbing rather
/// than file content. Lines without a gutter are passed through untouched, so
/// this is a no-op on a harness that stops emitting one.
fn strip_line_gutter(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        if line.trim_start().starts_with("<system-reminder>") {
            break;
        }
        out.push_str(strip_one_gutter(line).unwrap_or(line));
        out.push('\n');
    }
    // `lines()` dropped the trailing newline distinction; restore the common
    // case of a file that did not end in one.
    if !text.ends_with('\n') {
        out.pop();
    }
    out
}

/// `"   12→foo"` → `Some("foo")`; anything else → `None`.
fn strip_one_gutter(line: &str) -> Option<&str> {
    let rest = line.trim_start_matches(' ');
    let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return None;
    }
    let rest = &rest[digits..];
    rest.strip_prefix('\u{2192}')
        .or_else(|| rest.strip_prefix('\t'))
}

/// Synthesize the [`GrepSearchOutput`] a search card renders.
///
/// `Grep` in content mode returns `path:line:text` rows, which become per-file
/// line matches; every other shape (`Glob`, `files_with_matches`) is a plain
/// path list the card renders from `stdout`.
fn grep_output(name: &str, text: &str) -> GrepSearchOutput {
    let rows: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty() && !l.starts_with("Found "))
        .collect();
    let file_matches = if name == "Grep" {
        content_mode_matches(&rows)
    } else {
        Vec::new()
    };
    let match_count = if file_matches.is_empty() {
        rows.len()
    } else {
        file_matches.iter().map(|f| f.matches.len()).sum()
    };
    GrepSearchOutput {
        stdout: text.as_bytes().to_vec(),
        stderr: Vec::new(),
        exit_code: 0,
        match_count,
        file_matches,
    }
}

/// Group `path:line:text` rows by file. Returns empty when the rows are not in
/// that form (path-only output), leaving the card on its `stdout` path.
fn content_mode_matches(rows: &[&str]) -> Vec<GrepFileMatch> {
    let mut grouped: Vec<GrepFileMatch> = Vec::new();
    for row in rows {
        let Some((path, rest)) = split_path_prefix(row) else {
            return Vec::new();
        };
        let Some((line_no, content)) = rest.split_once(':') else {
            return Vec::new();
        };
        let Ok(line_number) = line_no.parse::<usize>() else {
            return Vec::new();
        };
        let entry = match grouped.iter_mut().find(|f| f.path == path) {
            Some(entry) => entry,
            None => {
                grouped.push(GrepFileMatch {
                    path: path.to_owned(),
                    matches: Vec::new(),
                });
                grouped.last_mut().expect("just pushed")
            }
        };
        entry.matches.push(GrepLineMatch {
            line_number,
            content: content.to_owned(),
        });
    }
    grouped
}

/// Split `path:rest`, tolerating a Windows drive letter (`C:\...`).
fn split_path_prefix(row: &str) -> Option<(&str, &str)> {
    let start = if row.len() > 2 && row.as_bytes()[1] == b':' {
        2
    } else {
        0
    };
    let idx = row[start..].find(':')? + start;
    Some((&row[..idx], &row[idx + 1..]))
}

fn text_content(text: &str) -> Vec<acp::ToolCallContent> {
    vec![acp::ToolCallContent::from(acp::ContentBlock::Text(
        acp::TextContent::new(text.to_owned()),
    ))]
}

fn to_value(output: ToolOutput) -> Option<Value> {
    serde_json::to_value(output).ok()
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key)?.as_str()
}

/// The harness input with Atlas's `variant` discriminator stamped on, so the
/// TUI's variant-keyed rendering rules apply to harness cards too.
fn tagged(input: &Value, variant: &str) -> Value {
    let mut obj = input.as_object().cloned().unwrap_or_else(Map::new);
    obj.insert("variant".to_owned(), Value::String(variant.to_owned()));
    Value::Object(obj)
}

fn absolutize(cwd: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cwd() -> &'static Path {
        Path::new("/proj")
    }

    fn variant(card: &ToolCard) -> Option<String> {
        card.raw_input
            .get("variant")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    #[test]
    fn bash_card_is_an_execute_card_with_the_command_readable() {
        let card = tool_card(
            "Bash",
            &json!({"command":"ls -la","description":"list"}),
            cwd(),
        );
        assert_eq!(card.kind, acp::ToolKind::Execute);
        assert_eq!(card.title, "Execute `ls -la`");
        assert_eq!(variant(&card).as_deref(), Some("Bash"));
        assert_eq!(
            card.raw_input.get("command").and_then(Value::as_str),
            Some("ls -la")
        );
    }

    #[test]
    fn bash_title_peels_a_redundant_session_cd() {
        let card = tool_card("Bash", &json!({"command":"cd /proj && echo hi"}), cwd());
        assert_eq!(card.title, "Execute `echo hi`");
    }

    #[test]
    fn read_card_carries_path_and_line() {
        let card = tool_card(
            "Read",
            &json!({"file_path":"src/main.rs","offset":40}),
            cwd(),
        );
        assert_eq!(card.kind, acp::ToolKind::Read);
        assert_eq!(card.title, "Read `src/main.rs`");
        assert_eq!(variant(&card).as_deref(), Some("ReadFile"));
        assert_eq!(card.locations[0].line, Some(40));
    }

    #[test]
    fn edit_card_emits_a_diff_anchored_to_the_absolute_path() {
        let card = tool_card(
            "Edit",
            &json!({"file_path":"a.rs","old_string":"let x = 1;","new_string":"let x = 2;"}),
            cwd(),
        );
        assert_eq!(card.kind, acp::ToolKind::Edit);
        assert_eq!(variant(&card).as_deref(), Some("SearchReplace"));
        assert_eq!(card.diff_anchor.as_deref(), Some(Path::new("/proj/a.rs")));
        let acp::ToolCallContent::Diff(diff) = &card.content[0] else {
            panic!("expected a diff, got {:?}", card.content[0]);
        };
        assert_eq!(diff.path, Path::new("/proj/a.rs"));
        assert_eq!(diff.old_text.as_deref(), Some("let x = 1;"));
        assert_eq!(diff.new_text, "let x = 2;");
    }

    #[test]
    fn multi_edit_emits_one_diff_per_edit() {
        let card = tool_card(
            "MultiEdit",
            &json!({
                "file_path":"a.rs",
                "edits":[
                    {"old_string":"one","new_string":"1"},
                    {"old_string":"two","new_string":"2"}
                ]
            }),
            cwd(),
        );
        assert_eq!(card.content.len(), 2);
    }

    #[test]
    fn write_card_is_a_whole_file_diff_tagged_write() {
        let card = tool_card(
            "Write",
            &json!({"file_path":"n.rs","content":"fn main(){}"}),
            cwd(),
        );
        assert_eq!(card.kind, acp::ToolKind::Edit);
        assert_eq!(variant(&card).as_deref(), Some("Write"));
        assert!(card.diff_anchor.is_none());
        let acp::ToolCallContent::Diff(diff) = &card.content[0] else {
            panic!("expected a diff");
        };
        assert_eq!(diff.old_text.as_deref(), Some(""));
        assert_eq!(diff.new_text, "fn main(){}");
    }

    #[test]
    fn grep_and_glob_share_the_search_card() {
        for name in ["Grep", "Glob"] {
            let card = tool_card(name, &json!({"pattern":"foo.*","path":"src"}), cwd());
            assert_eq!(card.kind, acp::ToolKind::Search, "{name}");
            assert_eq!(card.title, "foo.*", "{name}");
            assert_eq!(variant(&card).as_deref(), Some("Grep"), "{name}");
        }
    }

    #[test]
    fn web_tools_map_to_search_and_fetch_cards() {
        let search = tool_card("WebSearch", &json!({"query":"rust acp"}), cwd());
        assert_eq!(search.kind, acp::ToolKind::Search);
        assert_eq!(search.title, "Web search: \"rust acp\"");
        let fetch = tool_card("WebFetch", &json!({"url":"https://x.ai"}), cwd());
        assert_eq!(fetch.kind, acp::ToolKind::Fetch);
        assert_eq!(fetch.title, "Fetch: https://x.ai");
    }

    #[test]
    fn task_card_is_not_tagged_task_so_it_stays_visible() {
        let card = tool_card(
            "Task",
            &json!({"description":"find the bug","subagent_type":"Explore"}),
            cwd(),
        );
        assert_eq!(card.title, "Task(Explore): find the bug");
        assert!(variant(&card).is_none());
        assert_ne!(card.title, "Task");
    }

    #[test]
    fn unknown_and_mcp_tools_fall_through_to_generic_cards() {
        let unknown = tool_card("BrandNewTool", &json!({"a":1}), cwd());
        assert_eq!(unknown.kind, acp::ToolKind::Other);
        assert_eq!(unknown.title, "BrandNewTool");
        assert!(variant(&unknown).is_none());
        let mcp = tool_card("mcp__github__list_issues", &json!({}), cwd());
        assert_eq!(mcp.title, "github: list_issues");
    }

    #[test]
    fn bash_result_carries_the_output_as_typed_bash_output() {
        let fields = result_fields("Bash", &json!({"command":"ls"}), "a\nb\n", false, cwd());
        assert_eq!(fields.status, Some(acp::ToolCallStatus::Completed));
        let ToolOutput::Bash(bash) =
            serde_json::from_value::<ToolOutput>(fields.raw_output.clone().unwrap()).unwrap()
        else {
            panic!("expected bash output");
        };
        assert_eq!(String::from_utf8(bash.output).unwrap(), "a\nb\n");
        assert_eq!(bash.exit_code, 0);
        assert_eq!(bash.command, "ls");
    }

    #[test]
    fn failed_result_marks_the_card_failed() {
        let fields = result_fields("Bash", &json!({"command":"nope"}), "boom", true, cwd());
        assert_eq!(fields.status, Some(acp::ToolCallStatus::Failed));
    }

    #[test]
    fn read_result_strips_the_line_gutter_and_reports_the_range() {
        let text = "     1\u{2192}fn main() {\n     2\u{2192}}\n";
        let fields = result_fields("Read", &json!({"file_path":"a.rs"}), text, false, cwd());
        let ToolOutput::ReadFile(ReadFileOutput::FileContent(fc)) =
            serde_json::from_value::<ToolOutput>(fields.raw_output.clone().unwrap()).unwrap()
        else {
            panic!("expected file content");
        };
        assert_eq!(fc.raw_output, "fn main() {\n}\n");
        assert_eq!(fc.total_lines, 2);
        assert_eq!(fc.absolute_path, Path::new("/proj/a.rs"));
    }

    #[test]
    fn read_result_keeps_ungutted_text_verbatim() {
        let fields = result_fields("Read", &json!({"file_path":"a.rs"}), "plain", false, cwd());
        let ToolOutput::ReadFile(ReadFileOutput::FileContent(fc)) =
            serde_json::from_value::<ToolOutput>(fields.raw_output.clone().unwrap()).unwrap()
        else {
            panic!("expected file content");
        };
        assert_eq!(fc.raw_output, "plain");
    }

    #[test]
    fn read_result_drops_a_trailing_system_reminder() {
        let text = "     1\u{2192}x\n<system-reminder>noise</system-reminder>\n";
        let fields = result_fields("Read", &json!({"file_path":"a.rs"}), text, false, cwd());
        let ToolOutput::ReadFile(ReadFileOutput::FileContent(fc)) =
            serde_json::from_value::<ToolOutput>(fields.raw_output.clone().unwrap()).unwrap()
        else {
            panic!("expected file content");
        };
        assert_eq!(fc.raw_output, "x\n");
    }

    #[test]
    fn grep_content_rows_group_into_file_matches() {
        let text = "src/a.rs:12:let x = 1;\nsrc/a.rs:20:let y = 2;\nsrc/b.rs:3:fn f() {}\n";
        let fields = result_fields("Grep", &json!({"pattern":"let"}), text, false, cwd());
        let ToolOutput::GrepSearch(grep) =
            serde_json::from_value::<ToolOutput>(fields.raw_output.clone().unwrap()).unwrap()
        else {
            panic!("expected grep output");
        };
        assert_eq!(grep.match_count, 3);
        assert_eq!(grep.file_matches.len(), 2);
        assert_eq!(grep.file_matches[0].path, "src/a.rs");
        assert_eq!(grep.file_matches[0].matches[0].line_number, 12);
        assert_eq!(grep.file_matches[0].matches[0].content, "let x = 1;");
    }

    #[test]
    fn glob_path_rows_stay_a_plain_stdout_listing() {
        let text = "Found 2 files\n/proj/a.rs\n/proj/b.rs\n";
        let fields = result_fields("Glob", &json!({"pattern":"*.rs"}), text, false, cwd());
        let ToolOutput::GrepSearch(grep) =
            serde_json::from_value::<ToolOutput>(fields.raw_output.clone().unwrap()).unwrap()
        else {
            panic!("expected grep output");
        };
        assert!(grep.file_matches.is_empty());
        assert_eq!(grep.match_count, 2);
    }

    #[test]
    fn edit_result_keeps_the_diff_posted_at_start() {
        let fields = result_fields(
            "Edit",
            &json!({"file_path":"a.rs"}),
            "applied",
            false,
            cwd(),
        );
        assert_eq!(fields.status, Some(acp::ToolCallStatus::Completed));
        assert!(fields.content.is_none());
    }

    #[test]
    fn failed_edit_shows_the_harness_error_text() {
        let fields = result_fields(
            "Edit",
            &json!({"file_path":"a.rs"}),
            "no match",
            true,
            cwd(),
        );
        assert_eq!(fields.status, Some(acp::ToolCallStatus::Failed));
        assert!(fields.content.is_some());
    }

    #[test]
    fn todo_write_projects_a_plan() {
        let plan = plan_update(&json!({
            "todos":[
                {"content":"one","status":"completed","activeForm":"doing one"},
                {"content":"two","status":"in_progress","activeForm":"doing two"},
                {"content":"three","status":"pending","activeForm":"doing three"}
            ]
        }))
        .expect("todos should project");
        assert_eq!(plan.entries.len(), 3);
        assert_eq!(plan.entries[0].status, acp::PlanEntryStatus::Completed);
        assert_eq!(plan.entries[1].status, acp::PlanEntryStatus::InProgress);
        assert_eq!(plan.entries[2].status, acp::PlanEntryStatus::Pending);
        assert_eq!(plan.entries[0].content, "one");
    }

    #[test]
    fn plan_update_is_none_without_todos() {
        assert!(plan_update(&json!({"other":1})).is_none());
    }

    #[test]
    fn diffs_are_anchored_at_the_line_the_old_text_occupies() {
        let mut card = tool_card(
            "Edit",
            &json!({"file_path":"a.rs","old_string":"let y","new_string":"let z"}),
            cwd(),
        );
        anchor_diff_lines(&mut card.content, "let w\nlet x\nlet y\n");
        let acp::ToolCallContent::Diff(diff) = &card.content[0] else {
            panic!("expected a diff");
        };
        let meta = diff.meta.as_ref().expect("meta should be set");
        assert_eq!(meta.get("new_line").and_then(Value::as_u64), Some(3));
    }

    #[test]
    fn anchoring_is_a_no_op_when_the_old_text_is_absent() {
        let mut card = tool_card(
            "Edit",
            &json!({"file_path":"a.rs","old_string":"missing","new_string":"x"}),
            cwd(),
        );
        anchor_diff_lines(&mut card.content, "unrelated\n");
        let acp::ToolCallContent::Diff(diff) = &card.content[0] else {
            panic!("expected a diff");
        };
        assert!(diff.meta.is_none());
    }
}
