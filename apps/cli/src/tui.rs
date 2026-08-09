use crate::{
    client::RoninApiClient,
    config::{load_config, PermissionMode, RoninConfig},
    context::init_content,
    permissions::PermissionController,
    run::{compact_session, run_prompt, RunEvent, RunRequest},
    storage::{LocalSession, SessionStore},
};
use async_trait::async_trait;
use crossterm::{
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent,
        KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use futures::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame, Terminal, TerminalOptions, Viewport,
};
use ronin_agent_core::{
    create_mutation_tools, diff_file_changes, undo_file_changes, AgentLoopEvent, AgentStopReason,
    AgentTool, ModelSummary, PermissionAuthorizer, SourceCitation, ToolPermissionDescription,
    UserAnswer, UserQuestion, UserQuestioner,
};
use serde_json::Value;
use std::{
    collections::HashSet,
    io,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

type Term = Terminal<CrosstermBackend<io::Stdout>>;

const LIVE_ROWS: u16 = 10;
// One Dark Pro palette, approximated with 256-color indexes so it renders
// consistently in terminals without truecolor support.
const FG: Color = Color::Indexed(249); // #abb2bf foreground
const BRIGHT: Color = Color::Indexed(253); // #d7dae0 emphasis
const COMMENT: Color = Color::Indexed(241); // #5c6370 comments / dim
const BLUE: Color = Color::Indexed(75); // #61afef
const GREEN: Color = Color::Indexed(114); // #98c379
const RED: Color = Color::Indexed(168); // #e06c75
const YELLOW: Color = Color::Indexed(180); // #e5c07b
const PURPLE: Color = Color::Indexed(176); // #c678dd
const CYAN: Color = Color::Indexed(73); // #56b6c2
const ACCENT: Color = BLUE;
const DIM: Color = COMMENT;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const VERBS: [&str; 10] = [
    "Thinking",
    "Honing",
    "Forging",
    "Scheming",
    "Meditating",
    "Wandering",
    "Unsheathing",
    "Pondering",
    "Tracing",
    "Weighing",
];

enum PermissionDecision {
    No,
    Yes,
    Always,
}
struct PermissionRequest {
    description: ToolPermissionDescription,
    response: oneshot::Sender<PermissionDecision>,
}
struct TuiAuthorizer {
    tx: mpsc::UnboundedSender<PermissionRequest>,
    policy: Arc<PermissionController>,
}

struct QuestionRequest {
    questions: Vec<UserQuestion>,
    response: oneshot::Sender<Result<Vec<UserAnswer>, String>>,
}

struct TuiQuestioner {
    tx: mpsc::UnboundedSender<QuestionRequest>,
}

#[async_trait]
impl UserQuestioner for TuiQuestioner {
    async fn ask(
        &self,
        questions: Vec<UserQuestion>,
        cancel: &CancellationToken,
    ) -> Result<Vec<UserAnswer>, String> {
        let (response, answer) = oneshot::channel();
        self.tx
            .send(QuestionRequest {
                questions,
                response,
            })
            .map_err(|_| "The question UI is unavailable.".to_string())?;
        tokio::select! {
            _ = cancel.cancelled() => Err("The user cancelled the question.".into()),
            value = answer => value.unwrap_or_else(|_| Err("The question UI closed unexpectedly.".into())),
        }
    }
}

#[derive(Default)]
struct AnswerDraft {
    selected: HashSet<String>,
    other: String,
}

struct QuestionPrompt {
    request: QuestionRequest,
    index: usize,
    cursor: usize,
    editing_other: bool,
    drafts: Vec<AnswerDraft>,
}

impl QuestionPrompt {
    fn new(request: QuestionRequest) -> Self {
        let drafts = (0..request.questions.len())
            .map(|_| AnswerDraft::default())
            .collect();
        Self {
            request,
            index: 0,
            cursor: 0,
            editing_other: false,
            drafts,
        }
    }

    fn question(&self) -> &UserQuestion {
        &self.request.questions[self.index]
    }

    fn option_count(&self) -> usize {
        self.question().options.len() + usize::from(self.question().allow_other)
    }

    fn has_answer(&self) -> bool {
        !self.drafts[self.index].selected.is_empty()
            || !self.drafts[self.index].other.trim().is_empty()
    }

    fn answers(&self) -> Vec<UserAnswer> {
        self.request
            .questions
            .iter()
            .zip(&self.drafts)
            .map(|(question, draft)| UserAnswer {
                question_id: question.id.clone(),
                selected_option_ids: question
                    .options
                    .iter()
                    .filter(|option| draft.selected.contains(&option.id))
                    .map(|option| option.id.clone())
                    .collect(),
                other_text: (!draft.other.trim().is_empty()).then(|| draft.other.trim().into()),
            })
            .collect()
    }
}
#[async_trait]
impl PermissionAuthorizer for TuiAuthorizer {
    async fn authorize(
        &self,
        tool: &dyn AgentTool,
        _: &Value,
        description: Option<&ToolPermissionDescription>,
    ) -> bool {
        if let Some(decision) = self.policy.preauthorize(tool, description) {
            return decision;
        }
        let Some(description) = description.cloned() else {
            return false;
        };
        let (response, answer) = oneshot::channel();
        if self
            .tx
            .send(PermissionRequest {
                description: description.clone(),
                response,
            })
            .is_err()
        {
            return false;
        }
        match answer.await.unwrap_or(PermissionDecision::No) {
            PermissionDecision::No => false,
            PermissionDecision::Yes => true,
            PermissionDecision::Always => self.policy.persist_grant(tool, &description).is_ok(),
        }
    }
}

struct Ui {
    tick: u64,
    history: Vec<String>,
}

struct TurnView {
    started: Instant,
    pending: String,
    segment_open: bool,
    reasoning: String,
    round: u32,
    total_micro: u64,
    context_percent: Option<f64>,
    running_tool: Option<String>,
    stopping: bool,
}
impl TurnView {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            pending: String::new(),
            segment_open: false,
            reasoning: String::new(),
            round: 1,
            total_micro: 0,
            context_percent: None,
            running_tool: None,
            stopping: false,
        }
    }
}

fn err(e: impl ToString) -> String {
    e.to_string()
}

#[allow(clippy::too_many_arguments)]
pub async fn run_tui(
    client: RoninApiClient,
    config: RoninConfig,
    store: Arc<SessionStore>,
    cwd: &Path,
    model: Option<String>,
    session: Option<LocalSession>,
    max_credits: Option<f64>,
    dangerous: bool,
    web_search: bool,
) -> Result<i32, String> {
    enable_raw_mode().map_err(err)?;
    let mut stdout = io::stdout();
    let _ = execute!(stdout, EnableBracketedPaste);
    let terminal = Terminal::with_options(
        CrosstermBackend::new(stdout),
        TerminalOptions {
            viewport: Viewport::Inline(LIVE_ROWS),
        },
    );
    let mut terminal = match terminal {
        Ok(v) => v,
        Err(e) => {
            let _ = disable_raw_mode();
            return Err(e.to_string());
        }
    };
    let result = tui_main(
        &mut terminal,
        client,
        config,
        store,
        cwd,
        model,
        session,
        max_credits,
        dangerous,
        web_search,
    )
    .await;
    let _ = terminal.clear();
    let _ = execute!(io::stdout(), DisableBracketedPaste);
    let _ = disable_raw_mode();
    let _ = terminal.show_cursor();
    result
}

#[allow(clippy::too_many_arguments)]
async fn tui_main(
    term: &mut Term,
    client: RoninApiClient,
    mut config: RoninConfig,
    store: Arc<SessionStore>,
    cwd: &Path,
    model: Option<String>,
    mut session: Option<LocalSession>,
    max_credits: Option<f64>,
    dangerous: bool,
    mut web_search: bool,
) -> Result<i32, String> {
    let mut events = EventStream::new();
    let mut ui = Ui {
        tick: 0,
        history: Vec::new(),
    };
    let mut model = match model {
        Some(value) => value,
        None => match model_picker(term, &mut events, &mut ui, &client, "").await? {
            Some(value) => value,
            None => return Ok(0),
        },
    };
    let mut permission_mode = if dangerous {
        PermissionMode::Yolo
    } else {
        match config.permission_mode {
            // The legacy config value alone never grants bypass privileges.
            PermissionMode::Yolo => PermissionMode::Default,
            mode => mode,
        }
    };
    banner(
        term,
        &model,
        cwd,
        session.as_ref(),
        permission_mode,
        dangerous,
    )?;
    loop {
        let status = session_status(&model, session.as_ref());
        let Some(input) =
            composer(term, &mut events, &mut ui, &status, &mut permission_mode).await?
        else {
            if session.is_some() {
                note(
                    term,
                    "Session saved — run `ronin --continue` to pick it back up.",
                    DIM,
                )?;
            }
            return Ok(0);
        };
        let value = input.trim().to_string();
        if value.is_empty() {
            continue;
        }
        commit_user_prompt(term, &value)?;
        match value.as_str() {
            "/help" => {
                commit_help(term)?;
                continue;
            }
            "/exit" | "/quit" => return Ok(0),
            "/cost" => {
                note(
                    term,
                    &format!(
                        "{:.4} credits spent in this session.",
                        session.as_ref().map_or(0, |s| s.cost_micro) as f64 / 1_000_000.0
                    ),
                    DIM,
                )?;
                continue;
            }
            "/clear" => {
                if let Some(s) = &mut session {
                    s.messages.clear();
                    s.context_percent = None;
                    s.last_usage = None;
                    s.last_turn_changes.clear();
                    s.last_turn_affected_paths.clear();
                    s.last_turn_commands.clear();
                    *s = store.save(s).map_err(err)?;
                }
                note(term, "Conversation history cleared.", DIM)?;
                continue;
            }
            "/diff" => {
                match session.as_ref() {
                    Some(current) if !current.last_turn_changes.is_empty() => {
                        commit_change_diff(term, &diff_file_changes(&current.last_turn_changes))?;
                    }
                    Some(current) if !current.last_turn_affected_paths.is_empty() => note(
                        term,
                        "The latest native edit was too large to retain a detailed diff.",
                        YELLOW,
                    )?,
                    _ => note(term, "No native file changes to show.", DIM)?,
                }
                continue;
            }
            "/undo" => {
                let Some(current) = session.as_mut() else {
                    note(term, "No native file changes to undo.", DIM)?;
                    continue;
                };
                if current.last_turn_changes.is_empty() {
                    note(
                        term,
                        if current.last_turn_affected_paths.is_empty() {
                            "No native file changes to undo."
                        } else {
                            "The latest native edit is not undoable because its contents were too large to retain."
                        },
                        YELLOW,
                    )?;
                    continue;
                }
                match undo_file_changes(cwd, &current.last_turn_changes) {
                    Ok(paths) => {
                        current.last_turn_changes.clear();
                        current.last_turn_affected_paths.clear();
                        current.last_turn_commands.clear();
                        *current = store.save(current).map_err(err)?;
                        note(
                            term,
                            &format!("Undid the latest turn's changes to {}.", paths.join(", ")),
                            GREEN,
                        )?;
                    }
                    Err(error) => note(term, &error, RED)?,
                }
                continue;
            }
            "/compact" => {
                let Some(current) = session.take() else {
                    note(term, "Nothing to compact yet.", YELLOW)?;
                    continue;
                };
                let internal = config.internal_model.as_deref().unwrap_or(&model);
                match compact_session(&client, &store, current.clone(), internal, max_credits).await
                {
                    Ok(next) => {
                        session = Some(next);
                        note(term, "Context compacted.", DIM)?;
                    }
                    Err(e) => {
                        session = Some(current);
                        note(term, &e, RED)?;
                    }
                }
                continue;
            }
            "/web" => {
                note(
                    term,
                    &format!(
                        "Web search is {}. Use /web on or /web off.",
                        if web_search { "on" } else { "off" }
                    ),
                    DIM,
                )?;
                continue;
            }
            "/web on" => {
                web_search = true;
                note(term, "Web search enabled for this CLI session.", GREEN)?;
                continue;
            }
            "/web off" => {
                web_search = false;
                note(term, "Web search disabled for this CLI session.", DIM)?;
                continue;
            }
            _ => {}
        }
        if value == "/models" || value == "/model" {
            match model_picker(term, &mut events, &mut ui, &client, &model).await {
                Ok(Some(next)) => {
                    model = next;
                    note(term, &format!("Model set to {model}."), DIM)?;
                }
                Ok(None) => note(term, &format!("Keeping {model}."), DIM)?,
                Err(e) => note(term, &e, RED)?,
            }
            continue;
        }
        if let Some(rest) = value.strip_prefix("/model ") {
            let next = rest.trim();
            model = next.into();
            note(term, &format!("Model set to {model}."), DIM)?;
            continue;
        }
        if let Some(rest) = value.strip_prefix("/init") {
            let force = rest.contains("--force");
            match run_init(
                term,
                &mut events,
                &mut ui,
                cwd,
                &config,
                permission_mode,
                dangerous,
                force,
            )
            .await
            {
                Ok(()) => note(term, "Created RONIN.md.", GREEN)?,
                Err(e) => note(term, &e, RED)?,
            }
            continue;
        }
        if value.starts_with('/') {
            note(
                term,
                &format!("Unknown command {value}. Try /help."),
                YELLOW,
            )?;
            continue;
        }
        if max_credits.is_none() {
            match reload_credit_limit(&mut config, cwd, &home_dir()) {
                Ok(true) => note(term, &format_credit_limit(config.max_credits), DIM)?,
                Ok(false) => {}
                Err(error) => {
                    note(term, &error, RED)?;
                    continue;
                }
            }
        }
        let before = session.clone();
        match run_turn(
            term,
            &mut events,
            &mut ui,
            &client,
            &config,
            store.clone(),
            cwd,
            &model,
            &value,
            session,
            max_credits,
            permission_mode,
            dangerous,
            web_search,
        )
        .await
        {
            Ok((code, next)) => {
                session = Some(next);
                if code != 0 {
                    note(term, "Turn stopped before completion.", YELLOW)?;
                }
            }
            Err(failure) => {
                let recovered = failure.session.is_some();
                session = failure.session.or(before);
                note(term, &failure.message, RED)?;
                if recovered {
                    note(
                        term,
                        "Progress checkpoint preserved; update max_credits if needed, then submit a continuation prompt.",
                        YELLOW,
                    )?;
                }
            }
        }
    }
}

fn reload_credit_limit(config: &mut RoninConfig, cwd: &Path, home: &Path) -> Result<bool, String> {
    let latest =
        load_config(cwd, home).map_err(|error| format!("Could not reload max_credits: {error}"))?;
    let changed = latest.max_credits != config.max_credits;
    config.max_credits = latest.max_credits;
    Ok(changed)
}

fn format_credit_limit(limit: Option<f64>) -> String {
    match limit {
        Some(limit) => format!("Reloaded invocation limit: {limit} credits."),
        None => "Reloaded invocation limit: no configured cap.".into(),
    }
}

// ---------------------------------------------------------------------------
// Transcript (scrollback) rendering
// ---------------------------------------------------------------------------

fn term_width(term: &Term) -> usize {
    term.size().map(|s| s.width as usize).unwrap_or(80).max(20)
}

fn commit(term: &mut Term, lines: Vec<Line<'static>>) -> Result<(), String> {
    if lines.is_empty() {
        return Ok(());
    }
    // Degenerate terminals (e.g. a PTY with no size) make insert_before loop forever.
    let size = term.size().map_err(err)?;
    if size.width == 0 || size.height == 0 {
        return Ok(());
    }
    term.insert_before(lines.len() as u16, |buf| {
        for (y, line) in lines.iter().enumerate() {
            buf.set_line(0, y as u16, line, buf.area.width);
        }
    })
    .map_err(err)
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let text = text.replace('\t', "  ");
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut len = 0usize;
    for word in text.split(' ') {
        let mut word = word;
        loop {
            let wlen = word.chars().count();
            if wlen <= width {
                break;
            }
            let room = width.saturating_sub(len + usize::from(len > 0));
            if room < 4 {
                lines.push(std::mem::take(&mut current));
                len = 0;
                continue;
            }
            let piece: String = word.chars().take(room).collect();
            if len > 0 {
                current.push(' ');
            }
            current.push_str(&piece);
            lines.push(std::mem::take(&mut current));
            len = 0;
            word = &word[piece.len()..];
        }
        let wlen = word.chars().count();
        if len > 0 && len + 1 + wlen > width {
            lines.push(std::mem::take(&mut current));
            len = 0;
        }
        if len > 0 {
            current.push(' ');
            len += 1;
        }
        current.push_str(word);
        len += wlen;
    }
    lines.push(current);
    lines
}

fn truncate_line(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.into();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn note(term: &mut Term, text: &str, color: Color) -> Result<(), String> {
    let width = term_width(term);
    let mut lines = vec![Line::default()];
    for (i, piece) in wrap(text, width.saturating_sub(4)).into_iter().enumerate() {
        let bullet = if i == 0 { "◆ " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(bullet.to_string(), Style::default().fg(color)),
            Span::styled(piece, Style::default().fg(color)),
        ]));
    }
    commit(term, lines)
}

fn banner(
    term: &mut Term,
    model: &str,
    cwd: &Path,
    session: Option<&LocalSession>,
    permission_mode: PermissionMode,
    dangerous: bool,
) -> Result<(), String> {
    let dim = Style::default().fg(DIM);
    let label = Style::default().fg(DIM);
    let value = Style::default().fg(FG);
    let mut lines = vec![
        Line::default(),
        Line::from(vec![
            Span::styled(
                "◆ ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "Ronin",
                Style::default().fg(PURPLE).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  v{}", env!("CARGO_PKG_VERSION")), dim),
        ]),
        Line::from(vec![
            Span::styled("  model      ", label),
            Span::styled(model.to_string(), value),
        ]),
        Line::from(vec![
            Span::styled("  workspace  ", label),
            Span::styled(cwd.display().to_string(), value),
        ]),
    ];
    if let Some(s) = session {
        lines.push(Line::from(vec![
            Span::styled("  session    ", label),
            Span::styled(
                format!(
                    "resumed {} · {} turns · {:.4} credits",
                    short_id(&s.id),
                    s.messages.len(),
                    s.cost_micro as f64 / 1_000_000.0
                ),
                value,
            ),
        ]));
    }
    let mode_color = if permission_mode == PermissionMode::Yolo {
        RED
    } else {
        mode_color(permission_mode)
    };
    let mode_suffix = if dangerous && permission_mode == PermissionMode::Yolo {
        " (--dangerously-skip-permissions)"
    } else {
        ""
    };
    lines.push(Line::from(vec![
        Span::styled("  mode       ", label),
        Span::styled(
            format!("{}{}", permission_mode.label(), mode_suffix),
            Style::default().fg(mode_color),
        ),
    ]));
    lines.push(Line::from(Span::styled(
        "  /help for commands · esc interrupts · ctrl+c quits",
        dim,
    )));
    commit(term, lines)
}

fn commit_help(term: &mut Term) -> Result<(), String> {
    let rows: [(&str, &str); 11] = [
        ("/help", "Show this help"),
        ("shift+tab", "Cycle Manual, Accept edits, Plan, and Auto"),
        ("/model [id]", "Pick a model (searchable) or set one by id"),
        ("/cost", "Show credits spent in this session"),
        ("/clear", "Clear the conversation history"),
        ("/compact", "Summarize the context to free space"),
        ("/diff", "Show native file changes from the latest turn"),
        ("/undo", "Undo native file changes from the latest turn"),
        (
            "/web on|off",
            "Enable or disable model-controlled web search",
        ),
        ("/init [--force]", "Generate RONIN.md for this workspace"),
        ("/exit", "Quit (ctrl+c or ctrl+d also work)"),
    ];
    let mut lines = vec![Line::default()];
    for (cmd, help) in rows {
        lines.push(Line::from(vec![
            Span::styled(format!("  {cmd:<17}"), Style::default().fg(ACCENT)),
            Span::styled(help.to_string(), Style::default().fg(FG)),
        ]));
    }
    commit(term, lines)
}

fn commit_change_diff(term: &mut Term, diff: &str) -> Result<(), String> {
    let mut lines = vec![Line::default()];
    for line in diff.lines() {
        let color = if line.starts_with("+++") || line.starts_with("---") {
            ACCENT
        } else if line.starts_with('+') {
            GREEN
        } else if line.starts_with('-') {
            RED
        } else if line.starts_with("@@") {
            PURPLE
        } else {
            FG
        };
        lines.push(Line::from(Span::styled(
            line.to_string(),
            Style::default().fg(color),
        )));
    }
    commit(term, lines)
}

fn commit_user_prompt(term: &mut Term, prompt: &str) -> Result<(), String> {
    let width = term_width(term);
    let mut lines = vec![Line::default()];
    for (i, piece) in wrap(prompt, width.saturating_sub(2))
        .into_iter()
        .enumerate()
    {
        let head = if i == 0 { "❯ " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(
                head.to_string(),
                Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                piece,
                Style::default().fg(BRIGHT).add_modifier(Modifier::BOLD),
            ),
        ]));
    }
    commit(term, lines)
}

fn commit_answer_line(term: &mut Term, view: &mut TurnView, text: &str) -> Result<(), String> {
    if !view.segment_open && text.trim().is_empty() {
        return Ok(());
    }
    let width = term_width(term);
    let mut lines = Vec::new();
    if !view.segment_open {
        lines.push(Line::default());
    }
    let pieces = wrap(text, width.saturating_sub(2));
    for (i, piece) in pieces.into_iter().enumerate() {
        let head = if !view.segment_open && i == 0 {
            Span::styled("● ", Style::default().fg(BRIGHT))
        } else {
            Span::raw("  ")
        };
        lines.push(Line::from(vec![head, Span::raw(piece)]));
    }
    view.segment_open = true;
    commit(term, lines)
}

fn flush_answer(term: &mut Term, view: &mut TurnView, all: bool) -> Result<(), String> {
    while let Some(pos) = view.pending.find('\n') {
        let line: String = view.pending.drain(..=pos).collect();
        commit_answer_line(term, view, line.trim_end_matches('\n'))?;
    }
    if all && !view.pending.is_empty() {
        let line = std::mem::take(&mut view.pending);
        commit_answer_line(term, view, &line)?;
    }
    Ok(())
}

fn tool_args_summary(args: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(args).ok();
    let summary = parsed
        .as_ref()
        .and_then(|v| {
            ["command", "file_path", "path", "pattern", "query", "url"]
                .iter()
                .find_map(|k| v.get(k).and_then(Value::as_str).map(str::to_string))
        })
        .unwrap_or_else(|| args.to_string());
    truncate_line(summary.replace('\n', " ").trim(), 90)
}

fn commit_tool_start(term: &mut Term, name: &str, args: &str) -> Result<(), String> {
    let lines = vec![
        Line::default(),
        Line::from(vec![
            Span::styled("● ", Style::default().fg(GREEN)),
            Span::styled(
                name.to_string(),
                Style::default().fg(BLUE).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("({})", tool_args_summary(args)),
                Style::default().fg(FG),
            ),
        ]),
    ];
    commit(term, lines)
}

fn commit_tool_end(term: &mut Term, result: &str, error: bool) -> Result<(), String> {
    let width = term_width(term).saturating_sub(7);
    let color = if error { RED } else { DIM };
    let all: Vec<&str> = result.lines().filter(|l| !l.trim().is_empty()).collect();
    let shown = all.iter().take(3).collect::<Vec<_>>();
    let mut lines = Vec::new();
    if shown.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("  ⎿  {}", if error { "failed" } else { "done" }),
            Style::default().fg(color),
        )));
    }
    for (i, text) in shown.iter().enumerate() {
        let head = if i == 0 { "  ⎿  " } else { "     " };
        lines.push(Line::from(Span::styled(
            format!("{head}{}", truncate_line(text.trim_end(), width)),
            Style::default().fg(color),
        )));
    }
    if all.len() > shown.len() {
        lines.push(Line::from(Span::styled(
            format!("     … +{} lines", all.len() - shown.len()),
            Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
        )));
    }
    commit(term, lines)
}

fn commit_source(term: &mut Term, source: &SourceCitation) -> Result<(), String> {
    let width = term_width(term).saturating_sub(8);
    let label = if source.title.trim().is_empty() {
        source.url.as_str()
    } else {
        source.title.as_str()
    };
    commit(
        term,
        vec![Line::from(vec![
            Span::styled("  ↗ ", Style::default().fg(CYAN)),
            Span::styled(truncate_line(label, width), Style::default().fg(CYAN)),
            Span::styled(format!("  {}", source.url), Style::default().fg(DIM)),
        ])],
    )
}

fn commit_turn_stats(
    term: &mut Term,
    view: &TurnView,
    session: &LocalSession,
) -> Result<(), String> {
    let stop = match session.stop_reason.as_ref() {
        Some(AgentStopReason::Complete) | None => "complete".to_string(),
        Some(AgentStopReason::MaxRounds) => "max rounds reached".into(),
        Some(AgentStopReason::BudgetExhausted) => "credit budget exhausted".into(),
        Some(AgentStopReason::ContextLimit) => "context limit reached".into(),
        Some(AgentStopReason::Aborted) => "aborted".into(),
        Some(AgentStopReason::Stopped) => "interrupted".into(),
    };
    let ok = matches!(session.stop_reason, Some(AgentStopReason::Complete) | None);
    let context = view
        .context_percent
        .or(session.context_percent)
        .map(|p| format!(" · {:.0}% context", p * 100.0))
        .unwrap_or_default();
    let line = Line::from(vec![
        Span::styled(
            format!("  {} ", if ok { "✔" } else { "◼" }),
            Style::default().fg(if ok { GREEN } else { YELLOW }),
        ),
        Span::styled(
            format!(
                "{stop} · {} round{} · {:.4} credits · {}s{context}",
                view.round,
                if view.round == 1 { "" } else { "s" },
                view.total_micro as f64 / 1_000_000.0,
                view.started.elapsed().as_secs(),
            ),
            Style::default().fg(DIM),
        ),
    ]);
    commit(term, vec![Line::default(), line])
}

// ---------------------------------------------------------------------------
// Live area (viewport) rendering
// ---------------------------------------------------------------------------

fn shimmer(text: &str, tick: u64) -> Vec<Span<'static>> {
    const WAVE: [Color; 4] = [FG, BRIGHT, FG, COMMENT];
    text.chars()
        .enumerate()
        .map(|(i, c)| {
            let phase = (tick as usize / 2 + WAVE.len() * 4096 - i) % WAVE.len();
            Span::styled(
                c.to_string(),
                Style::default()
                    .fg(WAVE[phase])
                    .add_modifier(Modifier::BOLD),
            )
        })
        .collect()
}

fn status_line(ui: &Ui, view: &TurnView) -> Line<'static> {
    let spin = SPINNER[ui.tick as usize % SPINNER.len()];
    let verb = if view.stopping {
        "Stopping".to_string()
    } else if let Some(tool) = &view.running_tool {
        format!("Running {tool}")
    } else {
        VERBS[(ui.tick / 28) as usize % VERBS.len()].to_string()
    };
    let mut spans = vec![Span::styled(
        format!("{spin} "),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )];
    spans.extend(shimmer(&format!("{verb}…"), ui.tick));
    let context = view
        .context_percent
        .map(|p| format!(" · {:.0}% context", p * 100.0))
        .unwrap_or_default();
    spans.push(Span::styled(
        format!(
            " ({}s · round {} · {:.4} credits{context} · esc to interrupt)",
            view.started.elapsed().as_secs(),
            view.round,
            view.total_micro as f64 / 1_000_000.0,
        ),
        Style::default().fg(DIM),
    ));
    Line::from(spans)
}

fn tail_lines(view: &TurnView, width: usize, max: usize) -> Vec<Line<'static>> {
    let (text, style) = if !view.pending.trim().is_empty() {
        (view.pending.as_str(), Style::default().fg(FG))
    } else if !view.reasoning.trim().is_empty() {
        (
            view.reasoning.as_str(),
            Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
        )
    } else {
        return Vec::new();
    };
    let mut wrapped = Vec::new();
    for raw in text.lines() {
        for piece in wrap(raw, width.saturating_sub(2)) {
            wrapped.push(piece);
        }
    }
    wrapped
        .into_iter()
        .rev()
        .take(max)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|piece| Line::from(vec![Span::raw("  "), Span::styled(piece, style)]))
        .collect()
}

fn draw_permission_panel(f: &mut Frame, area: Rect, description: &ToolPermissionDescription) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(YELLOW))
        .title(Span::styled(
            " Permission required ",
            Style::default().fg(YELLOW).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let mut lines = vec![Line::from(Span::styled(
        truncate_line(&description.summary, inner.width as usize),
        Style::default().fg(BRIGHT).add_modifier(Modifier::BOLD),
    ))];
    if let Some(warning) = &description.warning {
        lines.push(Line::from(Span::styled(
            truncate_line(warning, inner.width as usize),
            Style::default().fg(RED),
        )));
    }
    if let Some(preview) = &description.preview {
        let room = (inner.height as usize).saturating_sub(lines.len() + 1);
        for text in preview.lines().take(room) {
            lines.push(Line::from(Span::styled(
                truncate_line(text, inner.width as usize),
                Style::default().fg(DIM),
            )));
        }
    }
    lines.push(Line::from(vec![
        Span::styled("y", Style::default().fg(GREEN).add_modifier(Modifier::BOLD)),
        Span::styled(" allow   ", Style::default().fg(FG)),
        Span::styled("a", Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
        Span::styled(" always allow   ", Style::default().fg(FG)),
        Span::styled("n", Style::default().fg(RED).add_modifier(Modifier::BOLD)),
        Span::styled(" deny", Style::default().fg(FG)),
    ]));
    let shown = lines.len().min(inner.height as usize);
    f.render_widget(Paragraph::new(lines[..shown].to_vec()), inner);
}

fn draw_question_panel(f: &mut Frame, area: Rect, prompt: &QuestionPrompt) {
    let question = prompt.question();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            format!(
                " {} · {}/{} ",
                question.header,
                prompt.index + 1,
                prompt.request.questions.len()
            ),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let mut lines = vec![Line::from(Span::styled(
        truncate_line(&question.question, inner.width as usize),
        Style::default().fg(BRIGHT).add_modifier(Modifier::BOLD),
    ))];
    for (index, option) in question.options.iter().enumerate() {
        let active = prompt.cursor == index && !prompt.editing_other;
        let selected = prompt.drafts[prompt.index].selected.contains(&option.id);
        lines.push(Line::from(vec![
            Span::styled(
                if active { "❯ " } else { "  " },
                Style::default().fg(ACCENT),
            ),
            Span::styled(
                format!("{} {}. ", if selected { "◉" } else { "○" }, index + 1),
                Style::default().fg(if selected { GREEN } else { DIM }),
            ),
            Span::styled(
                truncate_line(&option.label, (inner.width as usize).saturating_sub(10)),
                Style::default().fg(if active { BRIGHT } else { FG }),
            ),
        ]));
        if active && !option.description.is_empty() && lines.len() < inner.height as usize {
            lines.push(Line::from(Span::styled(
                format!(
                    "      {}",
                    truncate_line(
                        &option.description,
                        (inner.width as usize).saturating_sub(6)
                    )
                ),
                Style::default().fg(DIM),
            )));
        }
    }
    if question.allow_other {
        let index = question.options.len();
        let active = prompt.cursor == index;
        let other = &prompt.drafts[prompt.index].other;
        lines.push(Line::from(vec![
            Span::styled(
                if active { "❯ " } else { "  " },
                Style::default().fg(ACCENT),
            ),
            Span::styled(
                format!(
                    "{} {}. ",
                    if other.trim().is_empty() {
                        "○"
                    } else {
                        "◉"
                    },
                    index + 1
                ),
                Style::default().fg(if other.trim().is_empty() { DIM } else { GREEN }),
            ),
            Span::styled(
                if prompt.editing_other {
                    format!("Other: {other}▏")
                } else if other.trim().is_empty() {
                    "Other…".into()
                } else {
                    format!("Other: {other}")
                },
                Style::default().fg(if active { BRIGHT } else { FG }),
            ),
        ]));
    }
    lines.push(Line::from(Span::styled(
        if prompt.editing_other {
            "type an answer · enter confirm · esc cancel turn"
        } else if question.multi_select {
            "↑↓ move · space select · enter confirm · esc cancel turn"
        } else {
            "↑↓ move · 1-5 select · enter confirm · esc cancel turn"
        },
        Style::default().fg(DIM),
    )));
    let shown = lines.len().min(inner.height as usize);
    f.render_widget(Paragraph::new(lines[..shown].to_vec()), inner);
}

fn draw_running(
    f: &mut Frame,
    ui: &Ui,
    view: &TurnView,
    permission: Option<&PermissionRequest>,
    question: Option<&QuestionPrompt>,
) {
    let area = f.area();
    if let Some(prompt) = question {
        let height = area.height.saturating_sub(1).clamp(6, LIVE_ROWS);
        let chunks = Layout::vertical([
            Constraint::Length(height),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(area);
        draw_question_panel(f, chunks[0], prompt);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  awaiting your answer…",
                Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
            ))),
            chunks[1],
        );
        return;
    }
    if let Some(request) = permission {
        let height = area.height.saturating_sub(1).clamp(4, 9);
        let chunks = Layout::vertical([
            Constraint::Length(height),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(area);
        draw_permission_panel(f, chunks[0], &request.description);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  awaiting your approval…",
                Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
            ))),
            chunks[1],
        );
        return;
    }
    let tail_max = area.height.saturating_sub(2) as usize;
    let tail = tail_lines(view, area.width as usize, tail_max);
    let chunks = Layout::vertical([
        Constraint::Length(tail.len() as u16),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(area);
    if !tail.is_empty() {
        f.render_widget(Paragraph::new(tail), chunks[0]);
    }
    f.render_widget(Paragraph::new(status_line(ui, view)), chunks[1]);
}

fn mode_color(mode: PermissionMode) -> Color {
    match mode {
        PermissionMode::Default => FG,
        PermissionMode::AcceptEdits => YELLOW,
        PermissionMode::Plan => PURPLE,
        PermissionMode::Auto => GREEN,
        PermissionMode::Yolo => RED,
    }
}

fn draw_composer(
    f: &mut Frame,
    chars: &[char],
    cursor: usize,
    status: &str,
    permission_mode: PermissionMode,
    flash: Option<&str>,
) {
    let area = f.area();
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(area);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("◆ ", Style::default().fg(ACCENT)),
            Span::styled(status.to_string(), Style::default().fg(DIM)),
            Span::styled(" · ", Style::default().fg(DIM)),
            Span::styled(
                format!("{} mode", permission_mode.label()),
                Style::default()
                    .fg(mode_color(permission_mode))
                    .add_modifier(Modifier::BOLD),
            ),
        ])),
        chunks[0],
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(DIM));
    let inner = block.inner(chunks[1]);
    f.render_widget(block, chunks[1]);
    let avail = (inner.width as usize).saturating_sub(3).max(1);
    let start = cursor
        .saturating_sub(avail.saturating_sub(1))
        .min(chars.len());
    let visible: String = chars.iter().skip(start).take(avail).collect();
    let mut spans = vec![Span::styled(
        "❯ ",
        Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
    )];
    if chars.is_empty() {
        spans.push(Span::styled(
            "Describe a task, or try /help",
            Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
        ));
    } else {
        spans.push(Span::styled(visible, Style::default().fg(BRIGHT)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), inner);
    f.set_cursor_position(Position::new(
        inner.x + 2 + (cursor - start) as u16,
        inner.y,
    ));
    let hint = flash
        .map(|text| {
            Line::from(Span::styled(
                format!("  {text}"),
                Style::default().fg(YELLOW),
            ))
        })
        .unwrap_or_else(|| {
            Line::from(Span::styled(
                "  enter send · shift+tab mode · ↑↓ history · /help commands · ctrl+c quit",
                Style::default().fg(DIM),
            ))
        });
    f.render_widget(Paragraph::new(hint), chunks[2]);
}

// ---------------------------------------------------------------------------
// Interaction loops
// ---------------------------------------------------------------------------

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn session_status(model: &str, session: Option<&LocalSession>) -> String {
    match session {
        Some(s) => format!(
            "{model} · session {} · {:.4} credits{}",
            short_id(&s.id),
            s.cost_micro as f64 / 1_000_000.0,
            s.context_percent
                .map(|p| format!(" · {:.0}% context", p * 100.0))
                .unwrap_or_default()
        ),
        None => format!("{model} · new session"),
    }
}

fn choose_question_option(prompt: &mut QuestionPrompt, index: usize) {
    if index >= prompt.option_count() {
        return;
    }
    let options_len = prompt.question().options.len();
    let multi_select = prompt.question().multi_select;
    prompt.cursor = index;
    if index == options_len {
        if !multi_select {
            prompt.drafts[prompt.index].selected.clear();
        }
        prompt.editing_other = true;
        return;
    }
    let option_id = prompt.question().options[index].id.clone();
    if multi_select {
        let selected = &mut prompt.drafts[prompt.index].selected;
        if !selected.insert(option_id.clone()) {
            selected.remove(&option_id);
        }
    } else {
        let draft = &mut prompt.drafts[prompt.index];
        draft.selected.clear();
        draft.selected.insert(option_id);
        draft.other.clear();
    }
}

fn handle_question_key(prompt: &mut QuestionPrompt, key: KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if prompt.editing_other {
        match key.code {
            KeyCode::Enter => {
                if prompt.drafts[prompt.index].other.trim().is_empty() {
                    return false;
                }
                prompt.editing_other = false;
            }
            KeyCode::Backspace => {
                prompt.drafts[prompt.index].other.pop();
                return false;
            }
            KeyCode::Char(c) if !ctrl => {
                prompt.drafts[prompt.index].other.push(c);
                return false;
            }
            _ => return false,
        }
    } else {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                prompt.cursor = prompt.cursor.saturating_sub(1);
                return false;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                prompt.cursor = (prompt.cursor + 1).min(prompt.option_count().saturating_sub(1));
                return false;
            }
            KeyCode::Char(c @ '1'..='5') => {
                let index = c as usize - '1' as usize;
                choose_question_option(prompt, index);
                return false;
            }
            KeyCode::Char(' ') => {
                choose_question_option(prompt, prompt.cursor);
                return false;
            }
            KeyCode::Enter => {
                if prompt.cursor == prompt.question().options.len()
                    && prompt.question().allow_other
                    && prompt.drafts[prompt.index].other.trim().is_empty()
                {
                    if !prompt.question().multi_select {
                        prompt.drafts[prompt.index].selected.clear();
                    }
                    prompt.editing_other = true;
                    return false;
                }
            }
            _ => return false,
        }
    }
    if !prompt.has_answer() {
        return false;
    }
    if prompt.index + 1 == prompt.request.questions.len() {
        true
    } else {
        prompt.index += 1;
        prompt.cursor = 0;
        prompt.editing_other = false;
        false
    }
}

async fn composer(
    term: &mut Term,
    events: &mut EventStream,
    ui: &mut Ui,
    status: &str,
    permission_mode: &mut PermissionMode,
) -> Result<Option<String>, String> {
    let mut chars: Vec<char> = Vec::new();
    let mut cursor = 0usize;
    let mut history_index: Option<usize> = None;
    let mut draft = String::new();
    let mut flash: Option<(String, Instant)> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    loop {
        if flash
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() > Duration::from_millis(2500))
        {
            flash = None;
        }
        term.draw(|f| {
            draw_composer(
                f,
                &chars,
                cursor,
                status,
                *permission_mode,
                flash.as_ref().map(|(m, _)| m.as_str()),
            )
        })
        .map_err(err)?;
        let event = tokio::select! {
            _ = tick.tick() => { ui.tick += 1; continue; }
            event = events.next() => event,
        };
        let Some(Ok(event)) = event else { continue };
        match event {
            Event::Paste(text) => {
                for c in text.chars() {
                    let c = if c == '\n' || c == '\r' { ' ' } else { c };
                    chars.insert(cursor, c);
                    cursor += 1;
                }
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::BackTab => {
                        *permission_mode = permission_mode.next_interactive();
                        flash = Some((
                            format!(
                                "{} mode — {}",
                                permission_mode.label(),
                                permission_mode.description()
                            ),
                            Instant::now(),
                        ));
                    }
                    KeyCode::Enter => {
                        let value: String = chars.iter().collect();
                        if value.trim().is_empty() {
                            continue;
                        }
                        if ui.history.last() != Some(&value) {
                            ui.history.push(value.clone());
                        }
                        return Ok(Some(value));
                    }
                    KeyCode::Char('c') if ctrl => {
                        if chars.is_empty() {
                            return Ok(None);
                        }
                        chars.clear();
                        cursor = 0;
                        history_index = None;
                        flash = Some((
                            "input cleared — ctrl+c again to quit".into(),
                            Instant::now(),
                        ));
                    }
                    KeyCode::Char('d') if ctrl && chars.is_empty() => return Ok(None),
                    KeyCode::Char('u') if ctrl => {
                        chars.drain(..cursor);
                        cursor = 0;
                    }
                    KeyCode::Char('k') if ctrl => {
                        chars.truncate(cursor);
                    }
                    KeyCode::Char('a') if ctrl => cursor = 0,
                    KeyCode::Char('e') if ctrl => cursor = chars.len(),
                    KeyCode::Char('w') if ctrl => {
                        let mut i = cursor;
                        while i > 0 && chars[i - 1] == ' ' {
                            i -= 1;
                        }
                        while i > 0 && chars[i - 1] != ' ' {
                            i -= 1;
                        }
                        chars.drain(i..cursor);
                        cursor = i;
                    }
                    KeyCode::Esc => {
                        chars.clear();
                        cursor = 0;
                        history_index = None;
                    }
                    KeyCode::Backspace => {
                        if cursor > 0 {
                            chars.remove(cursor - 1);
                            cursor -= 1;
                        }
                    }
                    KeyCode::Delete => {
                        if cursor < chars.len() {
                            chars.remove(cursor);
                        }
                    }
                    KeyCode::Left => cursor = cursor.saturating_sub(1),
                    KeyCode::Right => cursor = (cursor + 1).min(chars.len()),
                    KeyCode::Home => cursor = 0,
                    KeyCode::End => cursor = chars.len(),
                    KeyCode::Up => {
                        if ui.history.is_empty() {
                            continue;
                        }
                        let next = match history_index {
                            None => {
                                draft = chars.iter().collect();
                                ui.history.len() - 1
                            }
                            Some(0) => 0,
                            Some(i) => i - 1,
                        };
                        history_index = Some(next);
                        chars = ui.history[next].chars().collect();
                        cursor = chars.len();
                    }
                    KeyCode::Down => {
                        let Some(i) = history_index else { continue };
                        if i + 1 < ui.history.len() {
                            history_index = Some(i + 1);
                            chars = ui.history[i + 1].chars().collect();
                        } else {
                            history_index = None;
                            chars = draft.chars().collect();
                        }
                        cursor = chars.len();
                    }
                    KeyCode::Char(c) if !ctrl => {
                        chars.insert(cursor, c);
                        cursor += 1;
                        history_index = None;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn model_matches(model: &ModelSummary, query: &str) -> bool {
    let haystack = format!(
        "{} {} {}",
        model.model_id, model.display_name, model.category
    )
    .to_lowercase();
    query
        .split_whitespace()
        .all(|term| haystack.contains(&term.to_lowercase()))
}

fn model_row(model: &ModelSummary, selected: bool, current: &str, width: usize) -> Line<'static> {
    let marker = if selected { "❯ " } else { "  " };
    let name_style = if selected {
        Style::default().fg(BLUE).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(BRIGHT)
    };
    let mut spans = vec![
        Span::styled(
            marker.to_string(),
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
        ),
        Span::styled(truncate_line(&model.display_name, width / 2), name_style),
    ];
    if model.model_id == current {
        spans.push(Span::styled(" ● current", Style::default().fg(GREEN)));
    }
    let context = model.context_window / 1000;
    let price = model
        .credit_price_micro
        .map(|p| format!(" · {p} µc/tok"))
        .unwrap_or_default();
    spans.push(Span::styled(
        format!("  {} · {context}k ctx{price}", model.model_id),
        Style::default().fg(COMMENT),
    ));
    Line::from(spans)
}

#[allow(clippy::too_many_arguments)]
fn draw_picker(
    f: &mut Frame,
    query: &[char],
    filtered: &[&ModelSummary],
    selected: usize,
    offset: usize,
    total: usize,
    current: &str,
) {
    let area = f.area();
    let rows = area.height.saturating_sub(2) as usize;
    let mut lines = Vec::with_capacity(rows + 2);
    let query_text: String = query.iter().collect();
    let mut search = vec![Span::styled(
        "⌕ ",
        Style::default().fg(BLUE).add_modifier(Modifier::BOLD),
    )];
    if query.is_empty() {
        search.push(Span::styled(
            "type to search models",
            Style::default().fg(COMMENT).add_modifier(Modifier::ITALIC),
        ));
    } else {
        search.push(Span::styled(
            query_text.clone(),
            Style::default().fg(BRIGHT),
        ));
    }
    search.push(Span::styled(
        format!("  {} of {}", filtered.len(), total),
        Style::default().fg(COMMENT),
    ));
    lines.push(Line::from(search));
    if filtered.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no models match",
            Style::default().fg(COMMENT).add_modifier(Modifier::ITALIC),
        )));
    }
    for (i, model) in filtered.iter().enumerate().skip(offset).take(rows) {
        lines.push(model_row(
            model,
            i == selected,
            current,
            area.width as usize,
        ));
    }
    lines.push(Line::from(Span::styled(
        "  ↑↓ navigate · enter select · esc cancel · type to filter",
        Style::default().fg(COMMENT),
    )));
    let hint = lines.pop().unwrap();
    let hint_area = Rect {
        y: area.bottom().saturating_sub(1),
        height: 1,
        ..area
    };
    f.render_widget(Paragraph::new(lines), area);
    f.render_widget(Paragraph::new(hint), hint_area);
    f.set_cursor_position(Position::new(
        area.x + 2 + query.len().min(area.width as usize - 3) as u16,
        area.y,
    ));
}

async fn model_picker(
    term: &mut Term,
    events: &mut EventStream,
    ui: &mut Ui,
    client: &RoninApiClient,
    current: &str,
) -> Result<Option<String>, String> {
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    let fetch = client.models();
    tokio::pin!(fetch);
    let models = loop {
        let spin = SPINNER[ui.tick as usize % SPINNER.len()];
        term.draw(|f| {
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        format!("{spin} "),
                        Style::default().fg(BLUE).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("Loading models…", Style::default().fg(FG)),
                ])),
                f.area(),
            );
        })
        .map_err(err)?;
        tokio::select! {
            result = &mut fetch => break result.map_err(err)?,
            _ = tick.tick() => ui.tick += 1,
            input = events.next() => {
                if let Some(Ok(Event::Key(key))) = input {
                    if key.kind == KeyEventKind::Press && key.code == KeyCode::Esc {
                        return Ok(None);
                    }
                }
            }
        }
    };
    let eligible: Vec<ModelSummary> = models.into_iter().filter(|m| m.supports_tools).collect();
    if eligible.is_empty() {
        return Err("No tool-capable model is available.".into());
    }
    let total = eligible.len();
    let mut query: Vec<char> = Vec::new();
    let mut selected = eligible
        .iter()
        .position(|m| m.model_id == current)
        .unwrap_or(0);
    let mut offset = 0usize;
    loop {
        let query_text: String = query.iter().collect();
        let filtered: Vec<&ModelSummary> = eligible
            .iter()
            .filter(|m| model_matches(m, &query_text))
            .collect();
        selected = selected.min(filtered.len().saturating_sub(1));
        let rows = (term.size().map_err(err)?.height as usize)
            .saturating_sub(2)
            .max(1);
        if selected < offset {
            offset = selected;
        } else if selected >= offset + rows {
            offset = selected + 1 - rows;
        }
        offset = offset.min(filtered.len().saturating_sub(1));
        term.draw(|f| draw_picker(f, &query, &filtered, selected, offset, total, current))
            .map_err(err)?;
        let event = tokio::select! {
            _ = tick.tick() => { ui.tick += 1; continue; }
            event = events.next() => event,
        };
        let Some(Ok(Event::Key(key))) = event else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return Ok(None),
            KeyCode::Char('c') if ctrl => return Ok(None),
            KeyCode::Enter => {
                if let Some(model) = filtered.get(selected) {
                    return Ok(Some(model.model_id.clone()));
                }
            }
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Down => selected = (selected + 1).min(filtered.len().saturating_sub(1)),
            KeyCode::PageUp => selected = selected.saturating_sub(rows),
            KeyCode::PageDown => {
                selected = (selected + rows).min(filtered.len().saturating_sub(1));
            }
            KeyCode::Backspace => {
                query.pop();
                selected = 0;
                offset = 0;
            }
            KeyCode::Char('u') if ctrl => {
                query.clear();
                selected = 0;
                offset = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                query.push(c);
                selected = 0;
                offset = 0;
            }
            _ => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_turn(
    term: &mut Term,
    events: &mut EventStream,
    ui: &mut Ui,
    client: &RoninApiClient,
    config: &RoninConfig,
    store: Arc<SessionStore>,
    cwd: &Path,
    model: &str,
    prompt: &str,
    session: Option<LocalSession>,
    max_credits: Option<f64>,
    permission_mode: PermissionMode,
    dangerous: bool,
    web_search: bool,
) -> Result<(i32, LocalSession), TurnFailure> {
    let recovery_store = store.clone();
    let recovery_session_id = session.as_ref().map(|value| value.id.clone());
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (permission_tx, mut permission_rx) = mpsc::unbounded_channel();
    let (question_tx, mut question_rx) = mpsc::unbounded_channel();
    let bypass = dangerous && permission_mode == PermissionMode::Yolo;
    let mode = permission_mode;
    let policy = Arc::new(PermissionController::new(
        cwd,
        &home_dir(),
        RoninConfig {
            permission_mode: mode,
            ..config.clone()
        },
        bypass,
        false,
    )?);
    let authorizer = Arc::new(TuiAuthorizer {
        tx: permission_tx,
        policy,
    });
    let questioner = Arc::new(TuiQuestioner { tx: question_tx });
    let cancel = CancellationToken::new();
    // Run the agent loop on its own task so tool execution can never starve
    // the UI loop: streaming, tool headers, and the spinner stay live.
    let mut task = {
        let client = client.clone();
        let config = RoninConfig {
            permission_mode: mode,
            ..config.clone()
        };
        let prompt = prompt.to_string();
        let model = model.to_string();
        let cwd = cwd.to_path_buf();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            run_prompt(
                client,
                &config,
                store,
                RunRequest {
                    prompt: &prompt,
                    model: &model,
                    max_credits,
                    dangerous: bypass,
                    cwd: &cwd,
                    session,
                    cancel: Some(cancel),
                    event_tx: Some(event_tx),
                    authorizer: Some(authorizer),
                    web_search,
                    questioner: Some(questioner),
                    source: "cli",
                },
            )
            .await
        })
    };
    let mut view = TurnView::new();
    let mut permission: Option<PermissionRequest> = None;
    let mut question: Option<QuestionPrompt> = None;
    let mut checkpointed = false;
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    let result = loop {
        term.draw(|f| draw_running(f, ui, &view, permission.as_ref(), question.as_ref()))
            .map_err(err)?;
        tokio::select! {
            value = &mut task => break value.unwrap_or_else(|e| Err(e.to_string())),
            _ = tick.tick() => ui.tick += 1,
            Some(event) = event_rx.recv() => {
                checkpointed |= matches!(&event, RunEvent::Checkpoint(_));
                handle_agent_event(term, &mut view, event)?;
            },
            Some(request) = permission_rx.recv(), if permission.is_none() => permission = Some(request),
            Some(request) = question_rx.recv(), if question.is_none() => question = Some(QuestionPrompt::new(request)),
            input = events.next() => {
                if let Some(Ok(Event::Key(key))) = input {
                    if key.kind != KeyEventKind::Press { continue; }
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    let cancelling = key.code == KeyCode::Esc
                        || (key.code == KeyCode::Char('c') && ctrl);
                    if question.is_some() && !cancelling {
                        let complete = handle_question_key(question.as_mut().unwrap(), key);
                        if complete {
                            let prompt = question.take().unwrap();
                            let answers = prompt.answers();
                            let _ = prompt.request.response.send(Ok(answers));
                        }
                        continue;
                    }
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('c') if key.code == KeyCode::Esc || ctrl => {
                            if let Some(p) = permission.take() {
                                let _ = p.response.send(PermissionDecision::No);
                            }
                            if let Some(prompt) = question.take() {
                                let _ = prompt
                                    .request
                                    .response
                                    .send(Err("The user cancelled the question.".into()));
                            }
                            cancel.cancel();
                            view.stopping = true;
                        }
                        KeyCode::Char('y' | 'Y') if permission.is_some() => {
                            if let Some(p) = permission.take() {
                                let _ = p.response.send(PermissionDecision::Yes);
                            }
                        }
                        KeyCode::Char('a' | 'A') if permission.is_some() => {
                            if let Some(p) = permission.take() {
                                let _ = p.response.send(PermissionDecision::Always);
                            }
                        }
                        KeyCode::Char('n' | 'N') if permission.is_some() => {
                            if let Some(p) = permission.take() {
                                let _ = p.response.send(PermissionDecision::No);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    };
    while let Ok(event) = event_rx.try_recv() {
        checkpointed |= matches!(&event, RunEvent::Checkpoint(_));
        handle_agent_event(term, &mut view, event)?;
    }
    flush_answer(term, &mut view, true)?;
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(message) => {
            let session = checkpointed
                .then(|| {
                    recover_checkpointed_session(
                        &recovery_store,
                        cwd,
                        recovery_session_id.as_deref(),
                    )
                })
                .flatten();
            return Err(TurnFailure { message, session });
        }
    };
    let code = outcome.code;
    commit_turn_stats(term, &view, &outcome.session)?;
    commit_turn_summary(term, &outcome.affected_paths, &outcome.commands)?;
    let session = outcome.session;
    Ok((code, session))
}

fn commit_turn_summary(
    term: &mut Term,
    affected_paths: &[String],
    commands: &[String],
) -> Result<(), String> {
    if affected_paths.is_empty() && commands.is_empty() {
        return Ok(());
    }
    let mut parts = Vec::new();
    if !affected_paths.is_empty() {
        parts.push(format!(
            "{} native file edit{}: {}",
            affected_paths.len(),
            if affected_paths.len() == 1 { "" } else { "s" },
            affected_paths.join(", ")
        ));
    }
    if !commands.is_empty() {
        parts.push(format!(
            "{} command{} executed",
            commands.len(),
            if commands.len() == 1 { "" } else { "s" }
        ));
    }
    note(term, &parts.join(" · "), DIM)
}

#[derive(Debug)]
struct TurnFailure {
    message: String,
    session: Option<LocalSession>,
}

impl From<String> for TurnFailure {
    fn from(message: String) -> Self {
        Self {
            message,
            session: None,
        }
    }
}

fn recover_checkpointed_session(
    store: &SessionStore,
    cwd: &Path,
    session_id: Option<&str>,
) -> Option<LocalSession> {
    session_id
        .and_then(|id| store.load(id).ok())
        .or_else(|| store.latest(cwd))
}

fn handle_agent_event(term: &mut Term, view: &mut TurnView, event: RunEvent) -> Result<(), String> {
    let RunEvent::Agent(event) = event else {
        return Ok(());
    };
    match event {
        AgentLoopEvent::Delta(text) => {
            view.pending.push_str(&text);
            flush_answer(term, view, false)?;
        }
        AgentLoopEvent::Reasoning(text) => {
            view.reasoning.push_str(&text);
            let excess = view.reasoning.len().saturating_sub(2000);
            if excess > 0 {
                let cut = (0..=excess)
                    .rev()
                    .find(|i| view.reasoning.is_char_boundary(*i))
                    .unwrap_or(0);
                view.reasoning.drain(..cut);
            }
        }
        AgentLoopEvent::RoundStart(round) => view.round = round + 1,
        AgentLoopEvent::GenerationStart(_, _) => view.reasoning.clear(),
        AgentLoopEvent::ToolStart(name, args, _) => {
            flush_answer(term, view, true)?;
            view.segment_open = false;
            commit_tool_start(term, &name, &args)?;
            view.running_tool = Some(name);
        }
        AgentLoopEvent::ToolEnd(_, result, error, _, _, _) => {
            view.running_tool = None;
            commit_tool_end(term, &result, error)?;
        }
        AgentLoopEvent::Citation(source) => commit_source(term, &source)?,
        AgentLoopEvent::RoundCost(_, total) => view.total_micro = total,
        AgentLoopEvent::RoundUsage(_, percent) => view.context_percent = percent,
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_init(
    term: &mut Term,
    events: &mut EventStream,
    ui: &mut Ui,
    cwd: &Path,
    config: &RoninConfig,
    permission_mode: PermissionMode,
    dangerous: bool,
    force: bool,
) -> Result<(), String> {
    let content = init_content(cwd, force)?;
    let tool = create_mutation_tools(cwd)?.remove(0);
    let args = serde_json::json!({"path":"RONIN.md","content":content});
    let description = tool.describe_permission(&args).await?;
    let bypass = dangerous && permission_mode == PermissionMode::Yolo;
    let mode = permission_mode;
    let policy = PermissionController::new(
        cwd,
        &home_dir(),
        RoninConfig {
            permission_mode: mode,
            ..config.clone()
        },
        bypass,
        false,
    )?;
    let allowed = match policy.preauthorize(tool.as_ref(), description.as_ref()) {
        Some(decision) => decision,
        None => {
            let Some(desc) = description.clone() else {
                return Err("Permission denied for /init.".into());
            };
            match prompt_permission(term, events, ui, &desc).await? {
                PermissionDecision::No => false,
                PermissionDecision::Yes => true,
                PermissionDecision::Always => {
                    let _ = policy.persist_grant(tool.as_ref(), &desc);
                    true
                }
            }
        }
    };
    if !allowed {
        return Err("Permission denied for /init.".into());
    }
    let result = tool.execute(&args, &CancellationToken::new()).await;
    if result.is_error {
        Err(result.result)
    } else {
        Ok(())
    }
}

async fn prompt_permission(
    term: &mut Term,
    events: &mut EventStream,
    ui: &mut Ui,
    description: &ToolPermissionDescription,
) -> Result<PermissionDecision, String> {
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    loop {
        term.draw(|f| {
            let area = f.area();
            let height = area.height.saturating_sub(1).clamp(4, 9);
            let chunks =
                Layout::vertical([Constraint::Length(height), Constraint::Min(0)]).split(area);
            draw_permission_panel(f, chunks[0], description);
        })
        .map_err(err)?;
        let event = tokio::select! {
            _ = tick.tick() => { ui.tick += 1; continue; }
            event = events.next() => event,
        };
        if let Some(Ok(Event::Key(key))) = event {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('y' | 'Y') => return Ok(PermissionDecision::Yes),
                KeyCode::Char('a' | 'A') => return Ok(PermissionDecision::Always),
                KeyCode::Char('n' | 'N') | KeyCode::Esc => return Ok(PermissionDecision::No),
                _ => {}
            }
        }
    }
}

fn home_dir() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(Into::into)
        .unwrap_or_else(|| ".".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(multi_select: bool, allow_other: bool) -> QuestionPrompt {
        let (response, _) = oneshot::channel();
        QuestionPrompt::new(QuestionRequest {
            questions: vec![UserQuestion {
                id: "choice".into(),
                header: "Choice".into(),
                question: "Choose an option".into(),
                options: vec![
                    ronin_agent_core::UserQuestionOption {
                        id: "one".into(),
                        label: "One".into(),
                        description: String::new(),
                    },
                    ronin_agent_core::UserQuestionOption {
                        id: "two".into(),
                        label: "Two".into(),
                        description: String::new(),
                    },
                ],
                multi_select,
                allow_other,
            }],
            response,
        })
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn single_and_multi_select_keys_produce_stable_option_ids() {
        let mut single = prompt(false, false);
        assert!(!handle_question_key(&mut single, key(KeyCode::Char('2'))));
        assert!(handle_question_key(&mut single, key(KeyCode::Enter)));
        assert_eq!(single.answers()[0].selected_option_ids, ["two"]);

        let mut multi = prompt(true, false);
        assert!(!handle_question_key(&mut multi, key(KeyCode::Char('1'))));
        assert!(!handle_question_key(&mut multi, key(KeyCode::Down)));
        assert!(!handle_question_key(&mut multi, key(KeyCode::Char(' '))));
        assert!(handle_question_key(&mut multi, key(KeyCode::Enter)));
        assert_eq!(multi.answers()[0].selected_option_ids, ["one", "two"]);
    }

    #[test]
    fn other_answer_uses_inline_editor() {
        let mut value = prompt(false, true);
        handle_question_key(&mut value, key(KeyCode::Down));
        handle_question_key(&mut value, key(KeyCode::Down));
        assert!(!handle_question_key(&mut value, key(KeyCode::Enter)));
        assert!(value.editing_other);
        for character in "custom".chars() {
            handle_question_key(&mut value, key(KeyCode::Char(character)));
        }
        assert!(handle_question_key(&mut value, key(KeyCode::Enter)));
        assert_eq!(value.answers()[0].other_text.as_deref(), Some("custom"));
    }

    #[test]
    fn credit_limit_reloads_from_config_between_turns() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".ronin")).unwrap();
        std::fs::write(
            home.path().join(".ronin/config.toml"),
            "max_credits = 100\n",
        )
        .unwrap();
        let mut config = RoninConfig {
            max_credits: Some(1.0),
            ..RoninConfig::default()
        };

        assert!(reload_credit_limit(&mut config, cwd.path(), home.path()).unwrap());
        assert_eq!(config.max_credits, Some(100.0));
        assert!(!reload_credit_limit(&mut config, cwd.path(), home.path()).unwrap());
    }

    #[test]
    fn failed_turn_recovers_the_latest_checkpoint() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path());
        let mut checkpoint = store
            .create(
                cwd.path(),
                "provider/model",
                vec![ronin_agent_core::AgentMessage::system("instructions")],
            )
            .unwrap();
        checkpoint
            .messages
            .push(ronin_agent_core::AgentMessage::user("original task"));
        checkpoint = store.save(&checkpoint).unwrap();

        let recovered =
            recover_checkpointed_session(&store, cwd.path(), Some(&checkpoint.id)).unwrap();
        assert_eq!(recovered.messages, checkpoint.messages);

        let recovered_first_turn = recover_checkpointed_session(&store, cwd.path(), None).unwrap();
        assert_eq!(recovered_first_turn.id, checkpoint.id);
    }
}
