use clap::{Parser, Subcommand};
use ronin_runtime::{
    auth::{begin_device_login, CredentialManager},
    client::RoninApiClient,
    config::{load_config, PermissionMode, RoninConfig},
    permissions::PermissionController,
    run::{run_prompt, RunEvent, RunOutcome, RunRequest},
    storage::{
        load_credentials_for, save_credentials, Credentials, LocalSession, SessionScanEntry,
        SessionState, SessionStore,
    },
    terminal::TerminalPermissionAuthorizer,
    tui,
    update::{self, UpdateCheck, UpdateOutcome},
};
use std::{
    env,
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(
    name = "ronin",
    version,
    about = "Ronin coding agent — multi-model agentic work on your Ronin credits"
)]
struct Cli {
    #[arg(short = 'p', long = "print")]
    prompt: Option<String>,
    #[arg(short, long)]
    model: Option<String>,
    #[arg(long)]
    max_credits: Option<f64>,
    #[arg(long)]
    api_url: Option<String>,
    #[arg(long)]
    dangerously_skip_permissions: bool,
    #[arg(long)]
    no_web_search: bool,
    #[arg(long,default_value="text",value_parser=["text","json"])]
    output_format: String,
    #[arg(long = "continue")]
    continue_session: bool,
    #[arg(long, num_args = 0..=1, default_missing_value = "")]
    resume: Option<String>,
    #[command(subcommand)]
    command: Option<Commands>,
}
#[derive(Subcommand)]
enum Commands {
    Doctor,
    /// Update Ronin to the latest published version
    Update,
    Models {
        #[arg(long)]
        tools_only: bool,
    },
    Sessions {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        command: Option<SessionCommands>,
    },
    Permissions {
        #[command(subcommand)]
        command: PermissionCommands,
    },
    Login {
        #[arg(long)]
        dev_user: Option<String>,
    },
    Logout,
}

#[derive(Subcommand)]
enum SessionCommands {
    Rename {
        id: String,
        title: String,
    },
    Fork {
        id: String,
    },
    Archive {
        id: String,
    },
    Unarchive {
        id: String,
    },
    Delete {
        id: String,
    },
    Restore {
        id: String,
    },
    Trash {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    EmptyTrash {
        #[arg(long)]
        yes: bool,
    },
    Doctor {
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand)]
enum PermissionCommands {
    List {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    Revoke {
        id: String,
    },
    Reset {
        #[arg(long)]
        workspace: bool,
    },
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("{e}");
        if e.contains("insufficient_credits") {
            eprintln!("Top up at the Ronin web app.");
        }
        if e.contains("cli_upgrade_required") {
            eprintln!("Upgrade the Ronin CLI before running another agent task.");
        }
        std::process::exit(1)
    }
}
async fn run() -> Result<(), String> {
    let cli = Cli::parse();
    if matches!(&cli.command, Some(Commands::Update)) {
        return match update::update().await.map_err(|e| e.to_string())? {
            UpdateOutcome::Updated { from, to } => {
                println!("Updated Ronin from v{from} to v{to}.");
                Ok(())
            }
            UpdateOutcome::Current { version } => {
                println!("Ronin v{version} is already up to date.");
                Ok(())
            }
        };
    }
    if cli.command.is_none()
        && cli.prompt.is_none()
        && io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && io::stderr().is_terminal()
        && offer_startup_update().await?
    {
        return Ok(());
    }
    let cwd = env::current_dir().map_err(|e| e.to_string())?;
    let home = home();
    if cli.continue_session && cli.resume.is_some() {
        return Err("Use either --continue or --resume, not both.".into());
    }
    if let Some(url) = &cli.api_url {
        env::set_var("RONIN_API_URL", url)
    }
    let config = load_config(&cwd, &home).map_err(|e| e.to_string())?;
    let web_search = config.web_search && !cli.no_web_search;
    let manager = Arc::new(CredentialManager::new(&config.api_url, &home));
    let client = RoninApiClient::new(&config.api_url, manager.clone());
    match cli.command {
        Some(Commands::Login { dev_user }) => {
            if let Some(id) = dev_user {
                save_credentials(
                    &home,
                    &Credentials {
                        dev_user_id: Some(id.clone()),
                        ..Default::default()
                    },
                )
                .map_err(|e| e.to_string())?;
                println!("Stored dev user {id}.")
            } else {
                let login = begin_device_login(
                    &config.api_url,
                    &home,
                    ronin_runtime::client::ClientIdentity::cli(),
                )
                .await
                .map_err(|e| e.to_string())?;
                let info = login.info();
                println!(
                    "Open {}",
                    info.verification_uri_complete
                        .as_deref()
                        .unwrap_or(&info.verification_uri)
                );
                println!("Confirm code: {}", info.user_code);
                eprintln!("Waiting for authorization…");
                login
                    .complete(CancellationToken::new())
                    .await
                    .map_err(|e| e.to_string())?;
                println!("Logged in successfully.")
            }
            return Ok(());
        }
        Some(Commands::Logout) => {
            manager.logout().await.map_err(|e| e.to_string())?;
            println!("Logged out.");
            return Ok(());
        }
        Some(Commands::Doctor) => {
            let mut failed = false;
            println!("Rust: ok ({})", env!("CARGO_PKG_VERSION"));
            let credentials = load_credentials_for(&home, &config.api_url);
            if credentials.access_token.is_none() && credentials.dev_user_id.is_none() {
                println!("Credentials: missing");
                failed = true
            } else {
                println!("Credentials: ok")
            }
            if client.health().await {
                println!("API: ok")
            } else {
                println!("API: unreachable");
                failed = true
            }
            match client.balance().await {
                Ok(v) => println!("Balance: {}", v.display),
                Err(e) => {
                    println!("Balance: {e}");
                    failed = true
                }
            }
            match client.models().await {
                Ok(v) => println!("Models: {} available", v.len()),
                Err(e) => {
                    println!("Models: {e}");
                    failed = true
                }
            }
            println!("Search: native Rust engine");
            if failed {
                return Err("Doctor found one or more problems.".into());
            }
            return Ok(());
        }
        Some(Commands::Models { tools_only }) => {
            for m in client.models().await.map_err(|e| e.to_string())? {
                if tools_only && !m.supports_tools {
                    continue;
                }
                println!(
                    "{:<42} {:<24} {}{}",
                    m.model_id,
                    m.display_name,
                    if m.supports_tools {
                        "tools"
                    } else {
                        "no-tools"
                    },
                    m.credit_price_micro
                        .map(|p| format!(" · {p} µc/token"))
                        .unwrap_or_default()
                )
            }
            return Ok(());
        }
        Some(Commands::Sessions { all, json, command }) => {
            let store = SessionStore::new(&home);
            if let Some(command) = command {
                handle_session_command(&store, &cwd, command)?;
                return Ok(());
            }
            let list = store.list((!all).then_some(cwd.as_path()));
            if json {
                let summaries=list.iter().map(|s|serde_json::json!({"id":s.id,"workspace":s.cwd,"model":s.model,"state":s.state,"updatedAt":s.updated_at,"rounds":s.rounds,"compactionCount":s.compaction_count,"costCredits":s.cost_micro as f64/1_000_000.0})).collect::<Vec<_>>();
                println!("{}", serde_json::to_string(&summaries).unwrap())
            } else {
                if list.is_empty() {
                    println!(
                        "{}",
                        if all {
                            "No local sessions."
                        } else {
                            "No local sessions for this workspace."
                        }
                    );
                    return Ok(());
                }
                for s in list {
                    println!(
                        "{}  {:<11} {}",
                        s.id,
                        state_name(&s.state),
                        s.title.as_deref().unwrap_or("Untitled session")
                    );
                    println!(
                        "  {} · {} · {} rounds · {} compactions · {:.4} credits",
                        s.updated_at,
                        s.model,
                        s.rounds,
                        s.compaction_count,
                        s.cost_micro as f64 / 1_000_000.0
                    );
                    println!("  {}", s.cwd);
                }
            }
            return Ok(());
        }
        Some(Commands::Permissions { command }) => {
            let policy = PermissionController::new(
                &cwd,
                &home,
                config.clone(),
                false,
                io::stdin().is_terminal() && io::stderr().is_terminal(),
            )?;
            match command {
                PermissionCommands::List { all, json } => {
                    let cwd = fs_canonical_string(&cwd)?;
                    let grants = policy
                        .grants()
                        .into_iter()
                        .filter(|grant| all || grant.workspace_path == cwd)
                        .collect::<Vec<_>>();
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string(&grants).map_err(|error| error.to_string())?
                        );
                    } else if grants.is_empty() {
                        println!(
                            "No stored permission grants{}.",
                            if all { "" } else { " for this workspace" }
                        );
                    } else {
                        for grant in grants {
                            println!("{}  {:<7} {}", grant.id, grant.kind, grant.value);
                            println!("  {}", grant.workspace_path);
                        }
                    }
                }
                PermissionCommands::Revoke { id } => {
                    if !policy.revoke_grant(&id)? {
                        return Err(format!("Permission grant {id} was not found."));
                    }
                    println!("Revoked permission grant {id}.");
                }
                PermissionCommands::Reset { workspace } => {
                    if !workspace {
                        return Err(
                            "Pass --workspace to reset grants for the current workspace.".into(),
                        );
                    }
                    let removed = policy.reset_workspace()?;
                    println!("Removed {removed} permission grant(s) for this workspace.");
                }
            }
            return Ok(());
        }
        Some(Commands::Update) => unreachable!("update handled before configuration"),
        None => {}
    }
    let store = Arc::new(SessionStore::new(&home));
    let session = resolve_session(&store, &cwd, cli.continue_session, cli.resume.as_deref())?;
    if cli.prompt.is_none() {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(
                "Interactive mode requires a TTY. Use ronin -p \"...\" for scripts and pipes."
                    .into(),
            );
        }
        let model = cli
            .model
            .or_else(|| session.as_ref().map(|s| s.model.clone()))
            .or(config.default_model.clone());
        let code = tui::run_tui(
            client,
            config,
            store,
            &cwd,
            model,
            session,
            cli.max_credits,
            cli.dangerously_skip_permissions,
            web_search,
        )
        .await?;
        if code != 0 {
            std::process::exit(code)
        }
        return Ok(());
    }
    let model = cli
        .model
        .or_else(|| session.as_ref().map(|s| s.model.clone()))
        .or(config.default_model.clone())
        .ok_or("No model selected. Pass --model or set default_model in ronin.toml.")?;
    let mut prompt = cli.prompt.unwrap();
    if !io::stdin().is_terminal() {
        let mut piped = String::new();
        io::stdin()
            .read_to_string(&mut piped)
            .map_err(|e| e.to_string())?;
        if !piped.trim().is_empty() {
            prompt.push_str("\n\n<stdin>\n");
            prompt.push_str(piped.trim());
            prompt.push_str("\n</stdin>")
        }
    }
    let permission_mode = if cli.dangerously_skip_permissions {
        PermissionMode::Yolo
    } else {
        config.permission_mode
    };
    let policy = Arc::new(PermissionController::new(
        &cwd,
        &home,
        RoninConfig {
            permission_mode,
            ..config.clone()
        },
        cli.dangerously_skip_permissions,
        io::stdin().is_terminal() && io::stderr().is_terminal(),
    )?);
    for warning in policy.take_warnings() {
        eprintln!("Warning: {warning}");
    }
    let authorizer = Arc::new(TerminalPermissionAuthorizer::new(
        policy,
        io::stdin().is_terminal() && io::stderr().is_terminal(),
    ));
    let json_output = cli.output_format == "json";
    let (event_tx, render_task) = if json_output {
        (None, None)
    } else {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                match event {
                    RunEvent::Agent(ronin_agent_core::AgentLoopEvent::Delta(text)) => {
                        print!("{text}");
                        let _ = io::stdout().flush();
                    }
                    RunEvent::Agent(ronin_agent_core::AgentLoopEvent::ToolStart(name, _, _)) => {
                        eprintln!("\n→ {name}")
                    }
                    RunEvent::Agent(ronin_agent_core::AgentLoopEvent::ToolEnd(
                        name,
                        _,
                        error,
                        _,
                        _,
                        _,
                    )) => eprintln!("{} {name}", if error { "✗" } else { "✓" }),
                    _ => {}
                }
            }
        });
        (Some(tx), Some(task))
    };
    let outcome = run_prompt(
        client,
        &config,
        store,
        RunRequest {
            prompt: &prompt,
            model: &model,
            max_credits: cli.max_credits,
            dangerous: cli.dangerously_skip_permissions,
            cwd: &cwd,
            session,
            cancel: None,
            event_tx,
            authorizer: Some(authorizer),
            web_search,
            questioner: None,
            source: "cli",
        },
    )
    .await?;
    if let Some(task) = render_task {
        let _ = task.await;
    }
    print_outcome(&outcome, json_output, !json_output);
    if outcome.code != 0 {
        std::process::exit(outcome.code)
    }
    Ok(())
}

async fn offer_startup_update() -> Result<bool, String> {
    let check = match tokio::time::timeout(Duration::from_secs(3), update::check()).await {
        Ok(Ok(check)) => check,
        // Startup checks are best-effort. Offline and rate-limited users should
        // still reach Ronin without an update warning or delay beyond the cap.
        Ok(Err(_)) | Err(_) => return Ok(false),
    };
    let UpdateCheck::Available { current, latest } = check else {
        return Ok(false);
    };

    eprintln!();
    eprintln!("A new Ronin version is available: v{current} → v{latest}");
    eprint!("Install it now? [y/N] ");
    io::stderr().flush().map_err(|error| error.to_string())?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|error| error.to_string())?;
    if !accepts_update(&answer) {
        eprintln!("Skipped. Run `ronin update` whenever you're ready.");
        return Ok(false);
    }

    eprintln!("Downloading and verifying Ronin v{latest}…");
    match update::update().await {
        Ok(UpdateOutcome::Updated { from, to }) => {
            println!("Updated Ronin from v{from} to v{to}. Restarting…");
            restart_updated_cli()
        }
        Ok(UpdateOutcome::Current { version }) => {
            eprintln!("Ronin v{version} is already up to date. Continuing startup.");
            Ok(false)
        }
        Err(error) => {
            eprintln!("Could not install the update: {error}");
            eprintln!("Continuing with Ronin v{current}.");
            Ok(false)
        }
    }
}

fn accepts_update(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn restart_updated_cli() -> Result<bool, String> {
    let executable = env::current_exe().map_err(|error| error.to_string())?;
    let arguments = env::args_os().skip(1).collect::<Vec<_>>();
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = std::process::Command::new(executable)
            .args(arguments)
            .exec();
        Err(format!("Ronin was updated but could not restart: {error}"))
    }
    #[cfg(windows)]
    {
        std::process::Command::new(executable)
            .args(arguments)
            .spawn()
            .map_err(|error| format!("Ronin was updated but could not restart: {error}"))?;
        Ok(true)
    }
}

fn print_outcome(outcome: &RunOutcome, json: bool, streamed: bool) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "result": outcome.result.final_text,
                "stopReason": outcome.result.stop_reason,
                "rounds": outcome.result.rounds,
                "costCredits": outcome.invocation_cost_micro as f64 / 1_000_000.0,
                "usage": outcome.result.last_usage,
                "contextPercent": outcome.result.context_percent,
                "sources": outcome.result.sources,
                "changedFiles": outcome.affected_paths,
                "commands": outcome.commands,
            })
        );
        return;
    }
    if !streamed {
        print!("{}", outcome.result.final_text);
        let _ = io::stdout().flush();
    }
    println!();
    if !outcome.result.sources.is_empty() {
        println!("Sources:");
        for source in &outcome.result.sources {
            if source.title.is_empty() {
                println!("- {}", source.url);
            } else {
                println!("- {} — {}", source.title, source.url);
            }
        }
        println!();
    }
    for warning in &outcome.warnings {
        eprintln!("Warning: {}", warning.message);
    }
    if !outcome.affected_paths.is_empty() {
        eprintln!("Native file changes: {}", outcome.affected_paths.join(", "));
    }
    if !outcome.commands.is_empty() {
        eprintln!(
            "Executed {} command{}.",
            outcome.commands.len(),
            if outcome.commands.len() == 1 { "" } else { "s" }
        );
    }
    eprintln!(
        "\n{} round(s) · {:.4} credits{} · {}",
        outcome.result.rounds,
        outcome.invocation_cost_micro as f64 / 1_000_000.0,
        outcome
            .result
            .context_percent
            .map(|percent| format!(" · {:.1}% context", percent * 100.0))
            .unwrap_or_default(),
        stop_reason_name(&outcome.result.stop_reason)
    );
}

fn stop_reason_name(reason: &ronin_agent_core::AgentStopReason) -> &'static str {
    use ronin_agent_core::AgentStopReason;
    match reason {
        AgentStopReason::Complete => "complete",
        AgentStopReason::MaxRounds => "max_rounds",
        AgentStopReason::BudgetExhausted => "budget_exhausted",
        AgentStopReason::ContextLimit => "context_limit",
        AgentStopReason::Aborted => "aborted",
        AgentStopReason::Stopped => "stopped",
    }
}

fn resolve_session(
    store: &SessionStore,
    cwd: &Path,
    cont: bool,
    resume: Option<&str>,
) -> Result<Option<LocalSession>, String> {
    let session = if let Some(id) = resume {
        let id = if id.is_empty() {
            pick_session(store, cwd)?
        } else {
            id.to_string()
        };
        Some(store.load(&id).map_err(|e| e.to_string())?)
    } else if cont {
        store.latest(cwd)
    } else {
        None
    };
    if cont && session.is_none() {
        return Err("No local session exists for this workspace.".into());
    }
    if let Some(s) = &session {
        if s.cwd != cwd.to_string_lossy() {
            if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
                return Err(format!("Session {} belongs to {}; resume it from that workspace or use an interactive terminal.",s.id,s.cwd));
            }
            eprint!(
                "Session {} belongs to {}. Resume in {}? [y/N] ",
                s.id,
                s.cwd,
                cwd.display()
            );
            io::stderr().flush().ok();
            let mut line = String::new();
            io::stdin()
                .read_line(&mut line)
                .map_err(|e| e.to_string())?;
            if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                return Err("Session resume cancelled.".into());
            }
        }
    }
    Ok(session)
}

fn pick_session(store: &SessionStore, cwd: &Path) -> Result<String, String> {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Err("--resume without a session ID requires an interactive terminal.".into());
    }
    let sessions = store.list(Some(cwd));
    if sessions.is_empty() {
        return Err("No local session exists for this workspace.".into());
    }
    eprintln!("Select a session to resume:");
    for (index, session) in sessions.iter().enumerate() {
        eprintln!(
            "  {}) {} · {} · {}",
            index + 1,
            session.title.as_deref().unwrap_or("Untitled session"),
            session.model,
            session.updated_at
        );
        eprintln!("     {}", session.id);
    }
    eprint!("Session [1-{}]: ", sessions.len());
    io::stderr().flush().map_err(|error| error.to_string())?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|error| error.to_string())?;
    let index = answer
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|value| (1..=sessions.len()).contains(value))
        .ok_or("Session selection cancelled or invalid.")?;
    Ok(sessions[index - 1].id.clone())
}

fn handle_session_command(
    store: &SessionStore,
    cwd: &Path,
    command: SessionCommands,
) -> Result<(), String> {
    match command {
        SessionCommands::Rename { id, title } => {
            let session = store
                .rename(&id, &title)
                .map_err(|error| error.to_string())?;
            println!(
                "Renamed session {} to {}.",
                session.id,
                session.title.as_deref().unwrap_or("Untitled session")
            );
        }
        SessionCommands::Fork { id } => {
            let session = store.fork(&id).map_err(|error| error.to_string())?;
            println!("Forked session {id} as {}.", session.id);
        }
        SessionCommands::Archive { id } => {
            store
                .set_archived(&id, true)
                .map_err(|error| error.to_string())?;
            println!("Archived session {id}.");
        }
        SessionCommands::Unarchive { id } => {
            store
                .set_archived(&id, false)
                .map_err(|error| error.to_string())?;
            println!("Unarchived session {id}.");
        }
        SessionCommands::Delete { id } => {
            store.trash(&id).map_err(|error| error.to_string())?;
            println!("Moved session {id} to trash. It can be restored.");
        }
        SessionCommands::Restore { id } => {
            store.restore(&id).map_err(|error| error.to_string())?;
            println!("Restored session {id}.");
        }
        SessionCommands::Trash { all, json } => {
            let sessions = store.list_trash((!all).then_some(cwd));
            if json {
                let summaries = sessions.iter().map(session_json).collect::<Vec<_>>();
                println!(
                    "{}",
                    serde_json::to_string(&summaries).map_err(|error| error.to_string())?
                );
            } else if sessions.is_empty() {
                println!(
                    "Session trash is empty{}.",
                    if all { "" } else { " for this workspace" }
                );
            } else {
                for session in sessions {
                    println!(
                        "{}  {}  {}",
                        session.id,
                        session.title.as_deref().unwrap_or("Untitled session"),
                        session.updated_at
                    );
                    println!("  {}", session.cwd);
                }
            }
        }
        SessionCommands::EmptyTrash { yes } => {
            if !yes {
                return Err("Emptying session trash is permanent. Pass --yes to continue.".into());
            }
            let removed = store.empty_trash().map_err(|error| error.to_string())?;
            println!("Permanently removed {removed} session(s) from trash.");
        }
        SessionCommands::Doctor { all } => {
            let entries = store.scan((!all).then_some(cwd));
            let mut healthy = 0;
            let mut failed = 0;
            for entry in entries {
                match entry {
                    SessionScanEntry::Session(_) => healthy += 1,
                    SessionScanEntry::Unsupported { id, version } => {
                        failed += 1;
                        println!("Unsupported: {id} uses schema version {version}.");
                    }
                    SessionScanEntry::Quarantined { id, reason, path } => {
                        failed += 1;
                        println!("Quarantined: {id}: {reason}");
                        println!("  {path}");
                    }
                }
            }
            println!("Session store: {healthy} healthy, {failed} problem(s).");
            if failed > 0 {
                return Err("Session doctor found one or more problems.".into());
            }
        }
    }
    Ok(())
}

fn session_json(session: &LocalSession) -> serde_json::Value {
    serde_json::json!({
        "id": session.id,
        "title": session.title,
        "workspace": session.cwd,
        "model": session.model,
        "state": session.state,
        "updatedAt": session.updated_at,
        "rounds": session.rounds,
        "compactionCount": session.compaction_count,
        "costCredits": session.cost_micro as f64 / 1_000_000.0,
    })
}

fn fs_canonical_string(path: &Path) -> Result<String, String> {
    std::fs::canonicalize(path)
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|error| error.to_string())
}
fn home() -> PathBuf {
    env::var_os("HOME")
        .map(Into::into)
        .unwrap_or_else(|| PathBuf::from("."))
}
fn state_name(state: &SessionState) -> &'static str {
    match state {
        SessionState::Active => "active",
        SessionState::Completed => "completed",
        SessionState::Interrupted => "interrupted",
    }
}

#[cfg(test)]
mod tests {
    use super::{accepts_update, Cli, Commands, PermissionCommands, SessionCommands};
    use clap::Parser;

    #[test]
    fn startup_update_prompt_requires_explicit_confirmation() {
        assert!(accepts_update("y"));
        assert!(accepts_update(" YES \n"));
        assert!(!accepts_update(""));
        assert!(!accepts_update("n"));
        assert!(!accepts_update("later"));
    }

    #[test]
    fn resume_accepts_an_optional_session_id() {
        let picker = Cli::try_parse_from(["ronin", "--resume"]).unwrap();
        assert_eq!(picker.resume.as_deref(), Some(""));
        let explicit = Cli::try_parse_from(["ronin", "--resume", "session-1"]).unwrap();
        assert_eq!(explicit.resume.as_deref(), Some("session-1"));
    }

    #[test]
    fn workflow_management_subcommands_parse() {
        let session = Cli::try_parse_from(["ronin", "sessions", "fork", "session-1"]).unwrap();
        assert!(matches!(
            session.command,
            Some(Commands::Sessions {
                command: Some(SessionCommands::Fork { .. }),
                ..
            })
        ));
        let permissions =
            Cli::try_parse_from(["ronin", "permissions", "reset", "--workspace"]).unwrap();
        assert!(matches!(
            permissions.command,
            Some(Commands::Permissions {
                command: PermissionCommands::Reset { workspace: true }
            })
        ));
    }
}
