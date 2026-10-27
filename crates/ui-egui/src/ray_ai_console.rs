//! The native PhotoRay AI console. Provider authentication stays with the official CLI.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock, mpsc};

use egui::Context;
use serde_json::{Value, json};

use crate::PhotocraftApp;

const MAX_ACTIONS: usize = 8;
const MAX_CONTEXT_CHARS: usize = 48_000;

pub(crate) trait Host {
    fn ai_document_id(&self) -> String;
    fn ai_history_len(&self) -> usize;
    fn ai_snapshot(&mut self) -> Value;
    fn ai_commands(&mut self) -> Value;
    fn ai_run(&mut self, id: &str, params: Value) -> Result<(), String>;
}

impl Host for PhotocraftApp {
    fn ai_document_id(&self) -> String {
        self.session.active().map_or_else(|| "no-document".into(), |active| format!("doc:{}", active.doc.id.0))
    }

    fn ai_history_len(&self) -> usize {
        self.session.active().map_or(0, |active| active.history.past_len())
    }

    fn ai_snapshot(&mut self) -> Value {
        self.session.execute("document.inspect", json!({})).unwrap_or(Value::Null)
    }

    fn ai_commands(&mut self) -> Value {
        self.session.execute("command.list", json!({})).unwrap_or(Value::Null)
    }

    fn ai_run(&mut self, id: &str, params: Value) -> Result<(), String> {
        self.run(id, params).map(|_| ())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Provider {
    #[default]
    Codex,
    Claude,
    Gemini,
}

impl Provider {
    fn name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
            Self::Gemini => "Gemini",
        }
    }

    fn executable(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Gemini => "gemini",
        }
    }
}

#[derive(Clone)]
struct Action {
    command: String,
    params: Value,
    why: String,
}

struct Proposal {
    answer: String,
    actions: Vec<Action>,
}

struct Pending {
    receiver: mpsc::Receiver<Result<Proposal, String>>,
    provider: Provider,
    document: Option<String>,
}

#[derive(Default)]
struct State {
    open: bool,
    provider: Provider,
    question: String,
    share_document: bool,
    approved_document: Option<String>,
    approved_provider: Option<Provider>,
    prepared_context: String,
    answer: String,
    error: String,
    actions: Vec<Action>,
    pending: Option<Pending>,
    proposal_provider: Option<Provider>,
    proposal_document: Option<String>,
    undo_remaining: usize,
    expected_history_len: usize,
}

fn bounded(value: &Value) -> String {
    value.to_string().chars().take(MAX_CONTEXT_CHARS).collect()
}

fn make_prompt(question: &str, document: Option<&Value>, commands: Option<&Value>) -> String {
    let context = document.map_or_else(|| "Not shared".to_string(), bounded);
    let catalog = commands.map_or_else(|| "None. Give advice only; do not propose actions.".to_string(), bounded);
    format!(
        "You are a creative assistant in PhotoRay. Answer the user's question with useful, concise advice. \
         The app owns every edit. You can only propose commands from the supplied catalog; the user must review \
         and apply them. Never claim that an edit has already happened. Never propose file writes, exports, quits, \
         plugin execution, or actions outside the current document. Return ONLY JSON with fields answer (string) \
         and actions (array, at most 8). Each action has command (string), params (object), why (short string). \
         For opinions or explanations, use an empty actions array.\nDocument context: {context}\n\
         Command catalog: {catalog}\nUser question: {}",
        question.chars().take(4000).collect::<String>()
    )
}

fn find_executable(name: &str) -> Option<std::path::PathBuf> {
    let mut dirs: Vec<_> = std::env::var_os("PATH").map(|path| std::env::split_paths(&path).collect()).unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(std::path::PathBuf::from(home).join(".local/bin"));
    }
    dirs.push("/opt/homebrew/bin".into());
    dirs.push("/usr/local/bin".into());
    #[cfg(target_os = "macos")]
    dirs.push("/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS".into());
    for dir in dirs {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(target_os = "windows")]
        for suffix in ["exe", "cmd"] {
            let candidate = dir.join(format!("{name}.{suffix}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn ask(provider: Provider, prompt: &str) -> Result<Proposal, String> {
    let executable = find_executable(provider.executable())
        .ok_or_else(|| format!("{} CLI is missing. Install the official CLI and sign in to your own account first.", provider.name()))?;
    let mut command = Command::new(executable);
    match provider {
        Provider::Codex => {
            command.args(["exec", "--disable", "shell_tool", "--ignore-user-config", "--ephemeral", "--skip-git-repo-check", "-s", "read-only", "-"]);
        }
        Provider::Claude => {
            command.args([
                "-p",
                "--tools",
                "",
                "--strict-mcp-config",
                "--disable-slash-commands",
                "--no-session-persistence",
                "--output-format",
                "text",
                prompt,
            ]);
        }
        Provider::Gemini => {
            command.args(["-p", prompt, "--output-format", "text", "--approval-mode", "plan"]);
        }
    }
    command
        .current_dir(std::env::temp_dir())
        .stdin(if provider == Provider::Codex { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| format!("{} could not start: {error}", provider.name()))?;
    if provider == Provider::Codex
        && let Some(mut stdin) = child.stdin.take()
    {
        stdin.write_all(prompt.as_bytes()).map_err(|error| format!("Could not send the request: {error}"))?;
    }
    let stdout = child.stdout.take().ok_or("AI output stream unavailable")?;
    let stderr = child.stderr.take().ok_or("AI diagnostic stream unavailable")?;
    let output_reader = std::thread::spawn(move || read_limited(stdout, 1_000_000));
    let diagnostics_reader = std::thread::spawn(move || read_limited(stderr, 100_000));
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|error| format!("{} failed: {error}", provider.name()))? {
            break status;
        }
        if started.elapsed() > std::time::Duration::from_secs(180) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{} took too long. The document was not changed; please try again.", provider.name()));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    let output = output_reader.join().map_err(|_| "AI output reader failed")??;
    let diagnostics = diagnostics_reader.join().map_err(|_| "AI diagnostic reader failed")??;
    if !status.success() {
        let detail = format!("{diagnostics}\n{output}");
        return Err(friendly_error(provider, &detail));
    }
    parse(&output).map_err(|error| if looks_like_provider_error(&output) { friendly_error(provider, &output) } else { error })
}

fn read_limited(mut stream: impl Read, limit: usize) -> Result<String, String> {
    let mut buffer = [0_u8; 8192];
    let mut output = Vec::new();
    loop {
        let count = stream.read(&mut buffer).map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        let remaining = limit.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..count.min(remaining)]);
    }
    Ok(String::from_utf8_lossy(&output).into_owned())
}

fn looks_like_provider_error(detail: &str) -> bool {
    let lower = detail.to_lowercase();
    [
        "quota",
        "credits",
        "billing",
        "usage limit",
        "rate limit",
        "reached your limit",
        "hit your limit",
        "resource exhausted",
        "resource_exhausted",
        "too many requests",
        "429",
        "not logged in",
        "unauthorized",
        "authentication",
    ]
    .iter()
    .any(|part| lower.contains(part))
}

fn friendly_error(provider: Provider, detail: &str) -> String {
    let lower = detail.to_lowercase();
    let name = provider.name();
    if [
        "insufficient_quota",
        "out of credits",
        "no credits",
        "credit balance",
        "credits exhausted",
        "insufficient credits",
        "insufficient balance",
        "credit limit",
        "billing limit",
        "spending limit",
        "payment required",
        "payment_required",
        "insufficient funds",
        "balance exhausted",
    ]
    .iter()
    .any(|part| lower.contains(part))
    {
        return format!(
            "{name} has no available credits or has reached its spending limit. Check your {name} account's usage and billing, or choose another provider."
        );
    }
    if [
        "rate limit",
        "rate_limit",
        "usage limit",
        "usage_limit",
        "quota exceeded",
        "resource exhausted",
        "resource_exhausted",
        "too many requests",
        "daily limit",
        "usage cap",
        "reached your limit",
        "hit your limit",
        "maximum usage",
        "429",
    ]
    .iter()
    .any(|part| lower.contains(part))
    {
        return format!("{name} has reached a usage limit for now. Wait for the provider's reset, check your account usage, or choose another provider.");
    }
    if ["not logged in", "login required", "please login", "not authenticated", "unauthorized", "authentication failed", "invalid api key", "401"]
        .iter()
        .any(|part| lower.contains(part))
    {
        let command = match provider {
            Provider::Codex => "codex login",
            Provider::Claude => "claude auth login",
            Provider::Gemini => "gemini",
        };
        return format!("{name} needs you to sign in again. Run `{command}` in Terminal, then retry here.");
    }
    if ["temporarily unavailable", "service unavailable", "connection reset", "timed out", "timeout", "502", "503"].iter().any(|part| lower.contains(part)) {
        return format!("{name} is temporarily unavailable. The document was not changed; try again in a moment or choose another provider.");
    }
    format!("{name} could not complete the request. The document was not changed. Check your {name} CLI account and try again.")
}

fn parse(text: &str) -> Result<Proposal, String> {
    let trimmed = text.trim();
    let raw = if trimmed.starts_with("```") {
        trimmed.lines().skip(1).take_while(|line| !line.trim().starts_with("```")).collect::<Vec<_>>().join("\n")
    } else {
        trimmed.to_string()
    };
    let value: Value = serde_json::from_str(&raw).map_err(|_| "The AI did not return a structured answer. Nothing changed.".to_string())?;
    let answer = value.get("answer").and_then(Value::as_str).ok_or("The AI answer is missing text.")?.to_string();
    let proposed = value.get("actions").and_then(Value::as_array).ok_or("The AI answer is missing actions.")?;
    if proposed.len() > MAX_ACTIONS {
        return Err(format!("The AI proposed too many actions (maximum {MAX_ACTIONS})."));
    }
    let mut actions = Vec::with_capacity(proposed.len());
    for action in proposed {
        let command = action.get("command").and_then(Value::as_str).ok_or("An AI action has no command.")?;
        let params = action.get("params").filter(|params| params.is_object()).ok_or("An AI action has invalid parameters.")?;
        actions.push(Action {
            command: command.to_string(),
            params: params.clone(),
            why: action.get("why").and_then(Value::as_str).unwrap_or_default().to_string(),
        });
    }
    Ok(Proposal { answer, actions })
}

fn validate(action: &Action, catalog: &Value) -> Result<(), String> {
    if [
        "file.",
        "app.",
        "plugin.",
        "script.",
        "automation.",
        "session.",
        "project.",
        "document.",
        "settings.",
        "preferences.",
        "workspace.",
        "window.",
        "system.",
        "network.",
        "cloud.",
        "print.",
        "export.",
        "import.",
        "render.",
        "command.",
    ]
    .iter()
    .any(|prefix| action.command.starts_with(prefix))
    {
        return Err(format!("{} can affect files outside this document.", action.command));
    }
    if matches!(action.command.as_str(), "edit.undo" | "edit.redo") {
        return Err("Use the dedicated Undo control for history changes.".into());
    }
    let list = catalog.as_array().or_else(|| catalog.get("commands").and_then(Value::as_array));
    let enabled = list.is_some_and(|items| {
        items
            .iter()
            .any(|item| item.get("id").and_then(Value::as_str) == Some(action.command.as_str()) && item.get("enabled").and_then(Value::as_bool) != Some(false))
    });
    if !enabled {
        return Err(format!("{} is not currently available in this document.", action.command));
    }
    if has_external_target(&action.params, 0) {
        return Err("This proposed edit contains a file, URL, or external target. Nothing was applied.".into());
    }
    Ok(())
}

fn has_external_target(value: &Value, depth: usize) -> bool {
    if depth > 8 {
        return true;
    }
    match value {
        Value::Object(fields) => fields.iter().any(|(key, value)| {
            let key = key.to_ascii_lowercase();
            ["filepath", "file_path", "directory", "url", "uri", "endpoint", "script", "shell"].iter().any(|blocked| key == *blocked)
                || (key == "path"
                    && value
                        .as_str()
                        .is_some_and(|path| path.starts_with('/') || path.starts_with('\\') || path.contains("://") || path.get(1..2) == Some(":")))
                || has_external_target(value, depth + 1)
        }),
        Value::Array(items) => items.iter().any(|item| has_external_target(item, depth + 1)),
        _ => false,
    }
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

pub(crate) fn open() {
    state().lock().unwrap_or_else(std::sync::PoisonError::into_inner).open = true;
}

pub(crate) fn show(app: &mut impl Host, ctx: &Context) {
    let mut state = state().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if !state.open {
        return;
    }
    let current_identity = app.ai_document_id();
    if let Some((result, provider, document)) =
        state.pending.as_ref().and_then(|pending| pending.receiver.try_recv().ok().map(|result| (result, pending.provider, pending.document.clone())))
    {
        state.pending = None;
        if provider != state.provider || document.as_deref().is_some_and(|id| id != current_identity) {
            state.error = "The provider or document changed; the old response was discarded.".into();
        } else {
            match result {
                Ok(proposal) => {
                    state.answer = proposal.answer;
                    state.actions = if document.is_some() { proposal.actions } else { Vec::new() };
                    state.proposal_provider = Some(provider);
                    state.proposal_document = document;
                    state.error.clear();
                }
                Err(error) => state.error = error,
            }
        }
    }
    if state.approved_document.as_deref() != Some(&current_identity) || state.approved_provider != Some(state.provider) {
        state.approved_document = None;
        state.approved_provider = None;
        state.actions.clear();
    }
    if state.proposal_document.as_deref().is_some_and(|id| id != current_identity) || state.proposal_provider.is_some_and(|provider| provider != state.provider)
    {
        state.actions.clear();
    }
    let mut open = state.open;
    egui::Window::new("Ray AI Console").id(egui::Id::new("ray-ai-console")).open(&mut open).default_width(460.0).min_width(340.0).show(ctx, |ui| {
        ui.heading("Create with your AI assistant");
        ui.label("Choose the official CLI account to use on this computer.");
        let previous_provider = state.provider;
        ui.horizontal(|ui| {
            ui.selectable_value(&mut state.provider, Provider::Codex, "Codex");
            ui.selectable_value(&mut state.provider, Provider::Claude, "Claude");
            ui.selectable_value(&mut state.provider, Provider::Gemini, "Gemini");
        });
        if state.provider != previous_provider {
            state.approved_document = None;
            state.approved_provider = None;
            state.actions.clear();
            state.answer.clear();
            state.error.clear();
        }
        let installed = find_executable(state.provider.executable()).is_some();
        ui.horizontal(|ui| {
            ui.label(if installed { "Official CLI installed" } else { "Official CLI missing" });
            let sign_in = match state.provider {
                Provider::Codex => "codex login",
                Provider::Claude => "claude auth login",
                Provider::Gemini => "gemini",
            };
            if ui.button("Copy sign-in command").clicked() {
                ctx.copy_text(sign_in.to_string());
            }
        });
        if !installed {
            let install_url = match state.provider {
                Provider::Codex => "https://developers.openai.com/codex/cli",
                Provider::Claude => "https://code.claude.com/docs/en/overview",
                Provider::Gemini => "https://github.com/google-gemini/gemini-cli",
            };
            ui.hyperlink_to("Install the official CLI", install_url);
        }
        ui.separator();
        ui.label("Ask for feedback, propose an edit, or explain a tool");
        ui.add(egui::TextEdit::multiline(&mut state.question).desired_rows(3).hint_text("What would you like to create?"));
        if ui.checkbox(&mut state.share_document, "Share this document's structure with the selected provider").changed() {
            state.approved_document = None;
            state.approved_provider = None;
            state.actions.clear();
        }
        if state.share_document {
            ui.small("The preview can include text, layer names and selections. It is sent after you approve this document and provider. The official CLI runs with its own local permissions.");
            let snapshot = app.ai_snapshot();
            state.prepared_context = bounded(&snapshot);
            egui::CollapsingHeader::new("View the data sent with each question").show(ui, |ui| {
                egui::ScrollArea::vertical().max_height(140.0).show(ui, |ui| {
                    ui.monospace(&state.prepared_context);
                });
            });
            if current_identity == "no-document" {
                ui.small("Open a document to share its context, or turn sharing off to ask a general question.");
            } else if state.approved_document.is_none() && ui.button(format!("Allow {} to see this document", state.provider.name())).clicked() {
                state.approved_document = Some(current_identity.clone());
                state.approved_provider = Some(state.provider);
            }
            if state.approved_document.is_some() {
                ui.small(format!("Context approved for this document and {}", state.provider.name()));
            }
        }
        let busy = state.pending.is_some();
        let context_approved = state.approved_document.as_deref() == Some(&current_identity)
            && state.approved_provider == Some(state.provider);
        let can_ask = installed && !busy && !state.question.trim().is_empty() && (!state.share_document || context_approved);
        if ui.add_enabled(can_ask, egui::Button::new("Ask AI")).clicked() {
            let document = state.share_document.then(|| app.ai_snapshot());
            let commands = state.share_document.then(|| app.ai_commands());
            let request = make_prompt(&state.question, document.as_ref(), commands.as_ref());
            let provider = state.provider;
            let (sender, receiver) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = sender.send(ask(provider, &request));
            });
            state.pending = Some(Pending { receiver, provider, document: state.share_document.then(|| current_identity.clone()) });
            state.actions.clear();
            state.answer.clear();
            state.error.clear();
        }
        if busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Thinking…");
            });
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        if !state.error.is_empty() {
            ui.colored_label(egui::Color32::from_rgb(240, 105, 105), &state.error);
        }
        if !state.answer.is_empty() {
            ui.separator();
            egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                ui.label(&state.answer);
            });
        }
        if !state.actions.is_empty() {
            ui.separator();
            ui.heading("Proposed edits");
            for (index, action) in state.actions.iter().enumerate() {
                ui.label(format!("{}. {} — {}", index + 1, action.command, action.why));
                ui.small(action.params.to_string());
            }
            let proposal_current = state.proposal_document.as_deref() == Some(&current_identity)
                && state.proposal_provider == Some(state.provider)
                && state.approved_document.as_deref() == Some(&current_identity);
            if ui.add_enabled(proposal_current, egui::Button::new("Apply these edits")).clicked() {
                let catalog = app.ai_commands();
                let actions = state.actions.clone();
                let before = app.ai_history_len();
                let mut applied = 0;
                if let Some(error) = actions.iter().find_map(|action| validate(action, &catalog).err()) {
                    state.error = format!("Nothing was applied: {error}");
                } else {
                    for action in &actions {
                        let history_before = app.ai_history_len();
                        if let Err(error) = app.ai_run(&action.command, action.params.clone()) {
                            state.error = format!("Stopped after {applied} action(s): {error}");
                            break;
                        }
                        if app.ai_history_len() <= history_before {
                            state.error = format!(
                                "Stopped: {} did not create an undoable edit. Review the document before continuing.",
                                action.command
                            );
                            break;
                        }
                        applied += 1;
                    }
                }
                let after = app.ai_history_len();
                state.undo_remaining = after.saturating_sub(before);
                state.expected_history_len = after;
                state.actions.clear();
                if applied == actions.len() {
                    state.answer = format!("Applied {applied} edit(s). Check the canvas; you can use Undo to revert.");
                }
            }
        }
        let current_history_len = app.ai_history_len();
        if state.undo_remaining > 0 && current_history_len == state.expected_history_len && ui.button("Undo last AI edit").clicked() {
            match app.ai_run("edit.undo", json!({})) {
                Ok(_) => {
                    state.undo_remaining = state.undo_remaining.saturating_sub(1);
                    state.expected_history_len = state.expected_history_len.saturating_sub(1);
                    state.answer = "Undid one history step. Check the canvas.".into();
                    state.error.clear();
                }
                Err(error) => state.error = error,
            }
        }
    });
    state.open = open;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opinion_has_no_actions() {
        let proposal = parse(r#"{"answer":"Looks balanced","actions":[]}"#).unwrap();
        assert_eq!(proposal.answer, "Looks balanced");
        assert!(proposal.actions.is_empty());
    }

    #[test]
    fn invalid_or_external_actions_are_rejected() {
        assert!(parse("not json").is_err());
        let catalog = json!([{"id":"image.adjust", "enabled":true}, {"id":"file.save", "enabled":true}]);
        let action = Action { command: "file.save".into(), params: json!({}), why: String::new() };
        assert!(validate(&action, &catalog).is_err());
        let action = Action { command: "image.adjust".into(), params: json!({}), why: String::new() };
        assert!(validate(&action, &catalog).is_ok());
    }

    #[test]
    fn provider_limits_and_login_errors_give_next_steps() {
        assert!(friendly_error(Provider::Codex, "insufficient_quota").contains("credits"));
        assert!(friendly_error(Provider::Codex, "credits exhausted").contains("credits"));
        assert!(friendly_error(Provider::Claude, "Rate limit exceeded").contains("Wait"));
        assert!(friendly_error(Provider::Claude, "resource_exhausted").contains("usage limit"));
        assert!(friendly_error(Provider::Gemini, "not logged in").contains("gemini"));
        assert!(friendly_error(Provider::Gemini, "daily limit reached").contains("usage limit"));
        assert!(friendly_error(Provider::Codex, "payment required").contains("billing"));
    }

    #[test]
    fn external_targets_are_not_applied() {
        let catalog = json!([{"id":"image.adjust", "enabled":true}]);
        let action = Action { command: "image.adjust".into(), params: json!({"layer":{"path":"/tmp/private"}}), why: String::new() };
        assert!(validate(&action, &catalog).is_err());
    }

    #[test]
    fn history_and_session_commands_are_not_ai_edits() {
        let catalog = json!([
            {"id":"edit.undo", "enabled":true},
            {"id":"session.save", "enabled":true},
        ]);
        for command in ["edit.undo", "session.save"] {
            let action = Action { command: command.into(), params: json!({}), why: String::new() };
            assert!(validate(&action, &catalog).is_err());
        }
    }
}
