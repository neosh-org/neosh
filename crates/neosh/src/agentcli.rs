//! `neosh agent …` — the workspace, from a script.
//!
//! # Who this is for
//!
//! A person has `^T`, a sidebar and a transcript. A program has this: start a conversation in a
//! directory, watch what it does, read what it produced, steer it, stop it, throw it away. The
//! shapes are the same because they are the same calls — see [`crate::control`] — so a verb here
//! is a thin translation of one [`ApiCall`] and nothing else, and anything the panel can do that
//! this cannot is a verb somebody has not written yet rather than a capability that is missing.
//!
//! # Why every verb has `--json`
//!
//! The plain output is for a person reading a terminal and is allowed to change. `--json` is the
//! contract: it is the wire type, verbatim, one object per line for anything streaming. A caller
//! that parses the pretty output is a caller that breaks when a column is widened, and the way to
//! not have that argument is to give the machine its own answer from the first day.
//!
//! # Ten at once
//!
//! Nothing here is a session of its own — the workspace holds the conversations and this connects
//! to it — so twenty `neosh agent` processes are twenty sockets onto one workspace rather than
//! twenty copies of neosh. Fanning work out is a loop around `start`, and joining it back up is
//! `wait`.

use std::path::PathBuf;

use clap::{Args, Subcommand};
use neosh_proto::{
    AgentCommand, ApiCall, ApiOk, ContentBlock, InstanceId, ModelId, ModelSelection, PluginEvent,
    Role, SessionId, SessionInfo, StopReason,
};

use crate::control::{Control, unwrap_ok};
use crate::paths::Paths;

#[derive(Subcommand, Debug)]
pub enum AgentCmd {
    /// Start a conversation, and say something in it.
    ///
    /// Answers with the conversation's id, which is what every other verb here takes. `--wait`
    /// turns that into one blocking call: start, run the turn, print what the agent said.
    Start(StartArgs),
    /// Every conversation this workspace is holding.
    #[command(visible_alias = "ls")]
    List(ListArgs),
    /// Say something in a conversation.
    ///
    /// While a turn is running this *steers* it, exactly as typing would: the message is held and
    /// taken in at the next gap. Several sent into one gap arrive as one message.
    Send(SendArgs),
    /// Print a conversation.
    #[command(visible_alias = "cat")]
    Read(ReadArgs),
    /// Follow what is happening, as it happens.
    Watch(WatchArgs),
    /// Block until a turn ends, and say how it ended.
    ///
    /// The exit status is the answer: `0` for a turn that finished, `1` for one that failed or was
    /// refused, `2` for one that was interrupted, `124` for a `--timeout` that ran out. So
    /// `neosh agent wait $id && do_the_next_thing` means what it looks like.
    Wait(WaitArgs),
    /// Ask a running turn to stop. The conversation survives it.
    Interrupt(OneArgs),
    /// Give a conversation a name, or take its name away.
    Rename(RenameArgs),
    /// Put a conversation away, or bring it back.
    Archive(ArchiveArgs),
    /// Delete a conversation and its file. Irreversible, so it asks.
    #[command(visible_alias = "rm")]
    Delete(DeleteArgs),
    /// Every model this workspace can reach.
    Models(ModelsArgs),
    /// Change which model a conversation uses. A running turn is told, and thinks the rest with it.
    Model(ModelArgs),
    /// Every command registered here — neosh's own and every plugin's.
    Commands(JsonArg),
    /// Run one of them by name.
    ///
    /// The escape hatch that makes a plugin's verbs scriptable without anybody adding a
    /// subcommand for them: a plugin registers `git.pull`, and this runs it.
    Run(RunArgs),
    /// Make one raw API call, as JSON.
    ///
    /// The bottom of this file: the whole plugin API, without a verb in front of it. Useful when
    /// something has no verb yet, and useful for finding out what a verb would look like.
    ///
    ///   neosh agent call '{"call":"session_list","include_archived":true}'
    Call(CallArgs),
}

#[derive(Args, Debug)]
pub struct StartArgs {
    /// What to say. Everything after the flags, joined with spaces; omit to start an empty one.
    prompt: Vec<String>,
    /// Where the conversation runs. Defaults to the current directory.
    #[arg(long, short = 'C')]
    cwd: Option<PathBuf>,
    /// Name it now, instead of letting the first message name it.
    #[arg(long)]
    title: Option<String>,
    /// `instance/model`, e.g. `anthropic/claude-opus-5`.
    #[arg(long)]
    model: Option<String>,
    /// Wait for the turn to finish and print what the agent said.
    #[arg(long)]
    wait: bool,
    /// With `--wait`, give up after this many seconds and exit 124.
    #[arg(long)]
    timeout: Option<u64>,
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
pub struct ListArgs {
    /// Include what has been archived.
    #[arg(long, short = 'a')]
    all: bool,
    /// Only the ones with a turn running.
    #[arg(long)]
    running: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
pub struct SendArgs {
    session: String,
    text: Vec<String>,
    /// Wait for the turn to finish and print what the agent said.
    #[arg(long)]
    wait: bool,
    #[arg(long)]
    timeout: Option<u64>,
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
pub struct ReadArgs {
    session: String,
    /// Only the last message.
    #[arg(long)]
    last: bool,
    /// What the agent said, and nothing else — no tool calls, no reasoning.
    #[arg(long)]
    text: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
pub struct WatchArgs {
    /// One conversation. Omit for every conversation in the workspace.
    session: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
pub struct WaitArgs {
    session: String,
    /// Give up after this many seconds and exit 124.
    #[arg(long)]
    timeout: Option<u64>,
    /// Print what the agent said when it finishes.
    #[arg(long)]
    print: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
pub struct OneArgs {
    session: String,
}

#[derive(Args, Debug)]
pub struct RenameArgs {
    session: String,
    /// The new name. Omit to take the name off and go back to the first message naming it.
    title: Vec<String>,
}

#[derive(Args, Debug)]
pub struct ArchiveArgs {
    session: String,
    /// Bring it back instead.
    #[arg(long)]
    restore: bool,
}

#[derive(Args, Debug)]
pub struct DeleteArgs {
    session: String,
    /// Do not ask.
    #[arg(long, short = 'y')]
    yes: bool,
}

#[derive(Args, Debug)]
pub struct ModelsArgs {
    /// Ask the endpoints again instead of using the workspace's catalogue.
    #[arg(long)]
    refresh: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
pub struct ModelArgs {
    session: String,
    /// `instance/model`.
    model: String,
}

#[derive(Args, Debug)]
pub struct JsonArg {
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    name: String,
    args: Vec<String>,
}

#[derive(Args, Debug)]
pub struct CallArgs {
    /// A JSON-encoded `ApiCall`. `-` reads it from stdin.
    call: String,
}

/// What a verb decided the process should exit with.
///
/// Carried rather than `std::process::exit`ed on the spot, so the socket is closed and anything
/// buffered is flushed on the ordinary path out.
pub struct Outcome(pub i32);

macro_rules! say {
    ($($arg:tt)*) => {{
        use std::io::Write;
        if writeln!(std::io::stdout(), $($arg)*).is_err() {
            std::process::exit(0);
        }
    }};
}

pub async fn run(
    paths: &Paths,
    cwd: &std::path::Path,
    config_dir: &Option<PathBuf>,
    cmd: &AgentCmd,
) -> anyhow::Result<Outcome> {
    let socket = paths.socket();
    // What to hand a workspace we have to start. The same list a terminal would pass, so a script
    // that runs first does not produce a differently-configured workspace from the one you would
    // have got by typing `neosh`.
    let args = crate::client::workspace_args(config_dir, cwd, &None, &[], &None);

    // Two kinds of verb. One does work in a workspace and is entitled to start one; the other only
    // asks a question, and starting a whole workspace — plugins, catalogues, a JS heap — to answer
    // `neosh agent ls` with an empty list would be a strange thing to have done.
    let asking_only = matches!(
        cmd,
        AgentCmd::List(_)
            | AgentCmd::Read(_)
            | AgentCmd::Watch(_)
            | AgentCmd::Wait(_)
            | AgentCmd::Models(_)
            | AgentCmd::Commands(_)
    );
    let mut c = match asking_only {
        true => match Control::existing(&socket).await? {
            Some(c) => c,
            None => {
                say!("no workspace running ({})", socket.display());
                return Ok(Outcome(1));
            }
        },
        false => Control::connect(&socket, &args).await?,
    };

    match cmd {
        AgentCmd::Start(a) => start(&mut c, cwd, a).await,
        AgentCmd::List(a) => list(&mut c, a).await,
        AgentCmd::Send(a) => send(&mut c, a).await,
        AgentCmd::Read(a) => read(&mut c, a).await,
        AgentCmd::Watch(a) => watch(&mut c, a).await,
        AgentCmd::Wait(a) => {
            let id = resolve(&mut c, &a.session).await?;
            let ended = await_turn(&mut c, &id, a.timeout, a.json).await?;
            if a.print && !a.json {
                print_last(&mut c, &id).await?;
            }
            Ok(Outcome(ended))
        }
        AgentCmd::Interrupt(a) => {
            let id = resolve(&mut c, &a.session).await?;
            one(&mut c, &id, AgentCommand::Interrupt).await?;
            say!("asked {} to stop", short(&id));
            Ok(Outcome(0))
        }
        AgentCmd::Rename(a) => {
            let id = resolve(&mut c, &a.session).await?;
            let title = a.title.join(" ");
            let title = (!title.trim().is_empty()).then_some(title);
            one(&mut c, &id, AgentCommand::Rename { title: title.clone() }).await?;
            match title {
                Some(t) => say!("{} is now {t}", short(&id)),
                None => say!("{} goes back to being named by its first message", short(&id)),
            }
            Ok(Outcome(0))
        }
        AgentCmd::Archive(a) => {
            let id = resolve(&mut c, &a.session).await?;
            one(&mut c, &id, AgentCommand::Archive { archived: !a.restore }).await?;
            match a.restore {
                true => say!("brought {} back", short(&id)),
                false => say!("archived {}", short(&id)),
            }
            Ok(Outcome(0))
        }
        AgentCmd::Delete(a) => delete(&mut c, a).await,
        AgentCmd::Models(a) => models(&mut c, a).await,
        AgentCmd::Model(a) => {
            let id = resolve(&mut c, &a.session).await?;
            let selection = parse_model(&a.model)?;
            let command = AgentCommand::SetModel {
                instance: selection.instance.0.clone(),
                model: selection.model.0.clone(),
            options: selection.options.clone(),
            };
            one(&mut c, &id, command).await?;
            say!("{} is now on {}/{}", short(&id), selection.instance, selection.model);
            Ok(Outcome(0))
        }
        AgentCmd::Commands(a) => {
            let commands = unwrap_ok!(
                c.call(ApiCall::CmdList).await?,
                ApiOk::Commands { commands } => commands,
                "a command list"
            )?;
            if a.json {
                say!("{}", serde_json::to_string(&commands)?);
            } else {
                for e in &commands {
                    say!("{:<28} {:<12} {}", e.name, e.plugin, e.desc.as_deref().unwrap_or(""));
                }
            }
            Ok(Outcome(0))
        }
        AgentCmd::Run(a) => {
            let value = unwrap_ok!(
                c.call(ApiCall::CmdCall { name: a.name.clone(), args: a.args.clone() }).await?,
                ApiOk::Json { value } => value,
                "what the command returned"
            )?;
            if !value.is_null() {
                say!("{}", serde_json::to_string(&value)?);
            }
            Ok(Outcome(0))
        }
        AgentCmd::Call(a) => {
            let text = match a.call.as_str() {
                "-" => std::io::read_to_string(std::io::stdin())?,
                other => other.to_string(),
            };
            let call: ApiCall = serde_json::from_str(&text)
                .map_err(|e| anyhow::anyhow!("that is not an ApiCall: {e}"))?;
            let value = c.call(call).await?;
            say!("{}", serde_json::to_string(&value)?);
            Ok(Outcome(0))
        }
    }
}

// ---------------------------------------------------------------------------
// The verbs
// ---------------------------------------------------------------------------

async fn start(c: &mut Control, cwd: &std::path::Path, a: &StartArgs) -> anyhow::Result<Outcome> {
    let dir = a.cwd.clone().unwrap_or_else(|| cwd.to_path_buf());
    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
    let prompt = a.prompt.join(" ");
    let prompt = (!prompt.trim().is_empty()).then_some(prompt);

    // Subscribed before the turn starts, not after. A `--wait` that subscribes afterwards is a
    // race a short turn wins — the `TurnEnded` it is waiting for has already been and gone, and
    // the wait then runs until the timeout with the answer sitting in the transcript.
    if a.wait && prompt.is_some() {
        c.subscribe().await?;
    }

    let info = unwrap_ok!(
        c.call(ApiCall::SessionNew {
            cwd: Some(dir.display().to_string()),
            title: a.title.clone(),
            // Never. Creating a conversation from a script must not move the transcript out from
            // under somebody reading one — and there may be nobody at a terminal at all.
            activate: false,
        })
        .await?,
        ApiOk::Session { session } => session,
        "the conversation it made"
    )?;

    if let Some(model) = &a.model {
        let selection = parse_model(model)?;
        let command = AgentCommand::SetModel {
            instance: selection.instance.0.clone(),
            model: selection.model.0.clone(),
            options: selection.options.clone(),
        };
        one(c, &info.id, command).await?;
    }

    if let Some(text) = &prompt {
        one(c, &info.id, AgentCommand::Send { text: text.clone(), images: Vec::new() }).await?;
    }

    if a.json {
        say!("{}", serde_json::to_string(&info)?);
    } else {
        say!("{}", info.id.0);
    }

    match (a.wait, prompt.is_some()) {
        (true, true) => {
            let ended = await_turn(c, &info.id, a.timeout, a.json).await?;
            if !a.json {
                print_last(c, &info.id).await?;
            }
            Ok(Outcome(ended))
        }
        // Nothing was asked, so there is nothing to wait for. Said out loud rather than waiting
        // for a timeout on a turn that was never going to start.
        (true, false) => {
            eprintln!("neosh: --wait with no prompt — nothing was asked, so nothing will end");
            Ok(Outcome(0))
        }
        _ => Ok(Outcome(0)),
    }
}

async fn list(c: &mut Control, a: &ListArgs) -> anyhow::Result<Outcome> {
    let mut sessions = unwrap_ok!(
        c.call(ApiCall::SessionList { include_archived: a.all }).await?,
        ApiOk::Sessions { sessions } => sessions,
        "a conversation list"
    )?;
    if a.running {
        sessions.retain(|s| s.active_turn.is_some());
    }
    if a.json {
        for s in &sessions {
            say!("{}", serde_json::to_string(s)?);
        }
        return Ok(Outcome(0));
    }
    if sessions.is_empty() {
        say!("no conversations");
        return Ok(Outcome(0));
    }
    for s in &sessions {
        say!(
            "{}  {:<9} {} {} {}",
            short(&s.id),
            state(s),
            cell(&s.project, 24),
            cell(&s.label, 28),
            s.cwd
        );
    }
    Ok(Outcome(0))
}

async fn send(c: &mut Control, a: &SendArgs) -> anyhow::Result<Outcome> {
    let id = resolve(c, &a.session).await?;
    let text = a.text.join(" ");
    if text.trim().is_empty() {
        anyhow::bail!("nothing to send");
    }
    // Before the send, for the reason `start` gives.
    if a.wait {
        c.subscribe().await?;
    }
    one(c, &id, AgentCommand::Send { text, images: Vec::new() }).await?;
    if !a.wait {
        return Ok(Outcome(0));
    }
    let ended = await_turn(c, &id, a.timeout, a.json).await?;
    if !a.json {
        print_last(c, &id).await?;
    }
    Ok(Outcome(ended))
}

async fn read(c: &mut Control, a: &ReadArgs) -> anyhow::Result<Outcome> {
    let id = resolve(c, &a.session).await?;
    let mut messages = unwrap_ok!(
        c.call(ApiCall::SessionMessages { session: Some(id) }).await?,
        ApiOk::Messages { messages } => messages,
        "a transcript"
    )?;
    if a.last {
        let last = messages.pop();
        messages = last.into_iter().collect();
    }
    if a.json {
        for m in &messages {
            say!("{}", serde_json::to_string(m)?);
        }
        return Ok(Outcome(0));
    }
    for m in &messages {
        if a.text {
            if m.role != Role::Assistant {
                continue;
            }
            for b in &m.content {
                if let ContentBlock::Text { text } = b {
                    say!("{text}");
                }
            }
            continue;
        }
        say!("── {:?}", m.role);
        for b in &m.content {
            match b {
                ContentBlock::Text { text } => say!("{text}"),
                ContentBlock::Thinking { text, .. } => say!("(thinking) {text}"),
                ContentBlock::ToolUse { name, input, .. } => {
                    say!("→ {name} {}", serde_json::to_string(input).unwrap_or_default())
                }
                ContentBlock::ToolResult { content, is_error, .. } => {
                    say!("{} {content}", if *is_error { "✗" } else { "←" })
                }
                other => say!("{}", serde_json::to_string(other).unwrap_or_default()),
            }
        }
    }
    Ok(Outcome(0))
}

async fn watch(c: &mut Control, a: &WatchArgs) -> anyhow::Result<Outcome> {
    let only = match &a.session {
        Some(s) => Some(resolve(c, s).await?),
        None => None,
    };
    c.subscribe().await?;
    while let Some(event) = c.event().await? {
        // Every turn event names the conversation it came from, so filtering is a comparison
        // rather than a subscription of its own. Events that are about the workspace instead of
        // about one conversation — an option changing, a plugin's own event — are not what this
        // was pointed at and would be noise under a session filter.
        if let (Some(want), Some(from)) = (&only, session_of(&event))
            && want != from
        {
            continue;
        }
        if a.json {
            say!("{}", serde_json::to_string(&event)?);
            continue;
        }
        match &event {
            PluginEvent::TurnStarted { session, .. } => say!("{} ▸ started", short(session)),
            PluginEvent::Token { text, .. } => {
                use std::io::Write;
                let mut out = std::io::stdout();
                if out.write_all(text.as_bytes()).is_err() || out.flush().is_err() {
                    return Ok(Outcome(0));
                }
            }
            PluginEvent::ToolStarted { session, call, .. } => {
                say!("\n{} → {}", short(session), call.name)
            }
            PluginEvent::ToolFinished { session, call, result, .. } => {
                say!("{} {} {}", short(session), if result.is_error { "✗" } else { "←" }, call.name)
            }
            PluginEvent::TurnEnded { session, stop_reason, .. } => {
                say!("\n{} ■ {}", short(session), why(stop_reason))
            }
            _ => {}
        }
    }
    Ok(Outcome(0))
}

async fn delete(c: &mut Control, a: &DeleteArgs) -> anyhow::Result<Outcome> {
    let id = resolve(c, &a.session).await?;
    // Irreversible, so it asks, and says what is at stake before it does. Everything else in this
    // file is undoable or harmless; this is the one verb that is not.
    if !a.yes {
        let sessions = unwrap_ok!(
            c.call(ApiCall::SessionList { include_archived: true }).await?,
            ApiOk::Sessions { sessions } => sessions,
            "a conversation list"
        )?;
        let it = sessions.iter().find(|s| s.id == id);
        let what = match it {
            Some(s) => format!("{} — {} message(s) in {}", s.label, s.message_count, s.cwd),
            None => format!("{} (a file this workspace has not loaded)", short(&id)),
        };
        if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            anyhow::bail!(
                "deleting {what} cannot be undone.\nNobody is here to ask, so pass --yes if you \
                 meant it."
            );
        }
        eprint!("Delete {what}? [y/N] ");
        use std::io::Write;
        let _ = std::io::stderr().flush();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            say!("left alone.");
            return Ok(Outcome(0));
        }
    }
    c.call(ApiCall::SessionClose { session: id.clone() }).await?;
    say!("deleted {}", short(&id));
    Ok(Outcome(0))
}

async fn models(c: &mut Control, a: &ModelsArgs) -> anyhow::Result<Outcome> {
    let models = unwrap_ok!(
        c.call(ApiCall::AgentListModels { instance: None, refresh: a.refresh }).await?,
        ApiOk::Models { models } => models,
        "a model list"
    )?;
    if a.json {
        for m in &models {
            say!("{}", serde_json::to_string(m)?);
        }
        return Ok(Outcome(0));
    }
    for m in &models {
        say!("{}/{}", m.instance, m.model.id);
    }
    Ok(Outcome(0))
}

// ---------------------------------------------------------------------------
// The shared parts
// ---------------------------------------------------------------------------

/// Do one [`AgentCommand`] to a conversation.
async fn one(c: &mut Control, id: &SessionId, command: AgentCommand) -> anyhow::Result<()> {
    c.call(ApiCall::AgentCommand { session: Some(id.clone()), command }).await?;
    Ok(())
}

/// Turn what somebody typed into a conversation id.
///
/// A full id, or a unique prefix of one, or `.` for whatever a terminal is looking at. Prefixes
/// because a conversation id is a uuid and nobody is going to type one twice: `neosh agent ls`
/// prints the first eight characters and every verb here takes them back.
///
/// An ambiguous prefix is refused with the candidates rather than resolved to the first match —
/// `delete` is on the other end of this function.
async fn resolve(c: &mut Control, given: &str) -> anyhow::Result<SessionId> {
    if given == "." {
        return unwrap_ok!(
            c.call(ApiCall::SessionCurrent).await?,
            ApiOk::Session { session } => session.id,
            "the conversation on screen"
        );
    }
    let sessions = unwrap_ok!(
        c.call(ApiCall::SessionList { include_archived: true }).await?,
        ApiOk::Sessions { sessions } => sessions,
        "a conversation list"
    )?;
    if let Some(exact) = sessions.iter().find(|s| s.id.0 == given) {
        return Ok(exact.id.clone());
    }
    let hits: Vec<&SessionInfo> = sessions.iter().filter(|s| s.id.0.starts_with(given)).collect();
    match hits.as_slice() {
        [one] => Ok(one.id.clone()),
        [] => anyhow::bail!("no conversation here starts with {given}"),
        many => {
            let names: Vec<String> =
                many.iter().map(|s| format!("{} ({})", short(&s.id), s.label)).collect();
            anyhow::bail!("{given} could be any of: {}", names.join(", "))
        }
    }
}

/// Wait for the current turn in `id` to end, and say how it ended.
///
/// The caller must have subscribed *before* whatever started the turn — see [`start`]. Returns the
/// exit status the process should use.
async fn await_turn(
    c: &mut Control,
    id: &SessionId,
    timeout: Option<u64>,
    json: bool,
) -> anyhow::Result<i32> {
    // Subscribing twice is harmless — the workspace keeps a list of senders and this connection
    // would simply appear on it again — but a caller that already did it does not need to.
    c.subscribe().await?;

    // Nothing running and nothing queued means there is nothing to wait for. Checked *after*
    // subscribing, so a turn that starts between the two is heard rather than missed.
    let sessions = unwrap_ok!(
        c.call(ApiCall::SessionList { include_archived: true }).await?,
        ApiOk::Sessions { sessions } => sessions,
        "a conversation list"
    )?;
    let running = sessions.iter().find(|s| s.id == *id).is_some_and(|s| s.active_turn.is_some());
    if !running {
        return Ok(0);
    }

    let deadline = timeout.map(|s| tokio::time::Instant::now() + std::time::Duration::from_secs(s));
    loop {
        let next = c.event();
        let event = match deadline {
            None => next.await?,
            Some(at) => match tokio::time::timeout_at(at, next).await {
                Ok(e) => e?,
                Err(_) => {
                    eprintln!(
                        "neosh: {} is still running after {}s",
                        short(id),
                        timeout.unwrap_or(0)
                    );
                    return Ok(124);
                }
            },
        };
        let Some(event) = event else {
            anyhow::bail!("the workspace stopped while {} was running", short(id));
        };
        if json {
            if let Some(from) = session_of(&event)
                && from == id
            {
                say!("{}", serde_json::to_string(&event)?);
            }
        }
        if let PluginEvent::TurnEnded { session, stop_reason, .. } = &event
            && session == id
        {
            return Ok(match stop_reason {
                StopReason::Cancelled => 2,
                StopReason::Error { .. } | StopReason::Refusal { .. } => 1,
                _ => 0,
            });
        }
    }
}

/// Print the last thing the agent said. What `--wait` is for.
async fn print_last(c: &mut Control, id: &SessionId) -> anyhow::Result<()> {
    let messages = unwrap_ok!(
        c.call(ApiCall::SessionMessages { session: Some(id.clone()) }).await?,
        ApiOk::Messages { messages } => messages,
        "a transcript"
    )?;
    let Some(last) = messages.iter().rev().find(|m| m.role == Role::Assistant) else {
        return Ok(());
    };
    for b in &last.content {
        if let ContentBlock::Text { text } = b {
            say!("{text}");
        }
    }
    Ok(())
}

/// Which conversation an event is about, when it is about one.
fn session_of(event: &PluginEvent) -> Option<&SessionId> {
    match event {
        PluginEvent::TurnStarted { session, .. }
        | PluginEvent::Token { session, .. }
        | PluginEvent::ThinkingToken { session, .. }
        | PluginEvent::ToolStarted { session, .. }
        | PluginEvent::ToolFinished { session, .. }
        | PluginEvent::TurnEnded { session, .. }
        | PluginEvent::SessionChanged { session, .. } => Some(session),
        _ => None,
    }
}

fn parse_model(given: &str) -> anyhow::Result<ModelSelection> {
    let Some((instance, model)) = given.split_once('/') else {
        anyhow::bail!("a model is `instance/model`, e.g. anthropic/claude-opus-5 — got {given}");
    };
    Ok(ModelSelection {
        instance: InstanceId(instance.to_string()),
        model: ModelId(model.to_string()),
        options: Default::default(),
    })
}

/// The first eight characters of an id, which is what every listing prints and every verb takes.
fn short(id: &SessionId) -> String {
    id.0.chars().take(8).collect()
}

fn state(s: &SessionInfo) -> &'static str {
    match (s.active_turn.is_some(), s.unread) {
        (true, _) => "running",
        (false, true) => "unread",
        _ => "idle",
    }
}

fn why(stop: &StopReason) -> String {
    match stop {
        StopReason::EndTurn => "done".into(),
        StopReason::ToolUse => "waiting on a tool".into(),
        StopReason::MaxTokens => "hit the length limit".into(),
        StopReason::StopSequence => "hit a stop sequence".into(),
        StopReason::Cancelled => "interrupted".into(),
        StopReason::Refusal { explanation, .. } => {
            format!("refused: {}", explanation.as_deref().unwrap_or("no reason given"))
        }
        StopReason::Error { message, .. } => format!("failed: {message}"),
    }
}

/// Clip to `cols` display columns, with an ellipsis when something was cut, and pad to fill.
///
/// Asked of `neosh-tui` rather than answered here, because that is where every display-width
/// question is answered — this is a column in a listing rather than a cell on a screen, but a
/// project is as likely to be named `neosh · fix/thing` as anything else and a CJK path is two
/// columns per character. A second answer to "how wide is this" is a second answer to get wrong,
/// and `{:<24}` is exactly that second answer: it pads on `chars().count()`, so the rows stop
/// lining up the first time somebody works in a directory with a non-Latin name in it.
fn cell(text: &str, cols: usize) -> String {
    let clipped = neosh_tui::width(text) > cols;
    let kept = match clipped {
        true => neosh_tui::truncate_to_width(text, cols.saturating_sub(1)),
        false => text,
    };
    let mut out = kept.to_string();
    if clipped {
        out.push('…');
    }
    let pad = cols.saturating_sub(neosh_tui::width(&out));
    out.extend(std::iter::repeat_n(' ', pad));
    out
}
