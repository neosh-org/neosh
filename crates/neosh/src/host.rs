//! The host: one loop that owns the core and routes everything else.
//!
//! Four things arrive asynchronously — plugin messages, agent events, frontend input, and a redraw
//! deadline — and exactly one task applies them to [`Editor`]. That single-writer arrangement is
//! what makes ordering well-defined when two plugins mutate the same buffer, without a lock
//! anywhere in the core.
//!
//! Redraws are coalesced, not scheduled. The first mutation arms a ~16 ms deadline; everything
//! until then lands in the same batch. An idle session arms nothing and blocks in `select!`, so it
//! costs no CPU, and a burst of streamed tokens produces one frame rather than one per token.

use std::sync::Arc;
use std::time::Duration;

use neosh_agent::{Agent, AgentEvent, PluginBridge, Prompt};
use neosh_core::{CoreEffect, Editor};
use neosh_proto::{
    ApiCall, ApiError, ApiOk, ApiResponse, ApiResult, BufferId, ContentBlock, CursorMotion,
    Direction, Dock,
    Gravity,
    InputEvent,
    KeyCode, MessageLevel, Mode, OptionSpec, OptionType, OptionValue, PluginEvent, PluginId,
    PluginInbound, PluginOutbound, PluginResponse, RequestId, ScrollAmount, SelectShape, TextEdit,
    UiEvent, VirtTextPos, WindowId, WindowLayout,
};
use neosh_provider::catalog;
use neosh_script::{ScriptInbound, ScriptOutbound};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use unicode_segmentation::UnicodeSegmentation;

use crate::bridge::{PluginProvider, ScriptBridge};
use crate::cards::{self, Glyphs, ToolState};
use crate::config::{Config, Resolved};
use crate::frontend::Frontend;
use crate::services::{Services, Subscribers};
use crate::vim;

/// How long mutations accumulate before a frame is emitted.
const COALESCE: Duration = Duration::from_millis(16);

/// How often to check whether the other machines have been told something out of date.
///
/// Slow on purpose. What it carries is "which projects and conversations exist here", which moves
/// when a person does something rather than when a token arrives, and two seconds of lag on
/// somebody else's screen is not a thing anybody could notice. It also bounds the cost: the check
/// is a recompute-and-compare, and doing it on the host loop's every iteration would put it on the
/// token path.
const ROSTER_INTERVAL: Duration = Duration::from_secs(2);

/// The plugin id the host registers its own things under.
///
/// Not a special case in the core: the built-in UI, commands, keymaps and options are all owned by
/// a plugin that happens to be the host. If any of them needed a path around the registry, the API
/// would be missing something.
const BUILTIN: &str = "neosh";

/// Broadcast once every plugin has loaded. See [`Host::announce_ready`].
/// Where a theme plugin puts its theme. See [`Host::contributed_theme`].
const THEME_POINT: &str = "ui.theme";
/// The owner of the commands registered on a held plugin's behalf. See [`Host::wake`].
const LAZY: &str = "neosh.lazy";

/// A plugin discovered but not loaded, waiting for one of its triggers.
#[derive(Debug, Clone)]
struct Lazy {
    url: String,
    manifest: neosh_proto::PluginManifest,
    bundled: bool,
}
/// The kinds of the host's own buffers. See [`Editor::create_buffer_of_kind`].
pub const KIND_TRANSCRIPT: &str = "neosh.transcript";
pub const KIND_COMPOSER: &str = "neosh.composer";
pub const KIND_STATUS: &str = "neosh.status";
/// The strip across the top of the main region.
///
/// A kind like every other, which is what makes the bar replaceable: a plugin binds keys against
/// it, finds its window with `win.ofKind`, recolours it with `win.setHighlights`, or disables ours
/// entirely and docks its own `Top` window in its place. The host draws a default; a default is
/// something you turn off.
pub const KIND_TABLINE: &str = "neosh.tabline";
/// The panel that lists what the window prefix can do. A kind, like everything else here, so a
/// plugin can restyle it or bind against it.
pub const KIND_WINDOW_HINT: &str = "neosh.window_keys";
/// A pane running a shell. Bind against it to give a terminal pane a key of your own.
pub const KIND_TERMINAL: &str = "neosh.terminal";
/// The panel that asks what a new tab should hold.
pub const KIND_TAB_CHOICE: &str = "neosh.tab_choice";

/// What a new tab can be, in the order the panel offers them.
///
/// Three, and no more. A chooser is a thing you read every time you open a tab, so it earns its
/// place only by being shorter than the decision it saves — and "which of my forty conversations"
/// is a question `^T` already answers better than a list bolted onto this one would.
///
/// **Every label says what you get, not what it is.** The first cut called the third one
/// `Conversation`, meaning "the one you are already in" — which reads as *a* conversation, so it
/// was indistinguishable from `New conversation` on the row below it, and the first person to see
/// the panel asked what the difference was. A label you have to be told the meaning of has failed;
/// two labels one word apart describing opposite things is worse than either alone.
///
/// **The likeliest intent is first**, because the cursor starts there and Enter is what people
/// press. Opening a tab almost always means starting something. The third option is the one a
/// split already covers — it is offered because a *tab* of it is a whole screen where a split is
/// half of one, and it is last because that is a thing you want occasionally rather than by
/// default.
///
/// **And the first row says where it puts you.** A tab belongs to the conversation in it — see
/// [`neosh_proto::TabInfo::group`] — so a tab on a *new* conversation is that conversation's first
/// tab, and the bar it lands on is that conversation's rather than this one's. The two rows below
/// it stay on this bar, which is the difference worth printing: without it the row is a key whose
/// visible effect is the tabs you had open disappearing.
const TAB_CHOICES: &[(&str, &str)] = &[
    ("New conversation", "a fresh one here, with tabs of its own"),
    ("Terminal", "a shell, here in this directory"),
    ("This conversation again", "a second place to read it, with its own scroll"),
];

/// What follows the window prefix, and the command each runs.
///
/// Vim's letters wherever Vim has one, because the whole reason to have a prefix is that the
/// fingers of everyone who would want it already know these. `v`/`s` split, `hjkl` move, `HJKL`
/// resize — capitals for resizing rather than Vim's `<>+-`, which are punctuation a plain terminal
/// cannot reliably tell apart under Ctrl and which have no mnemonic anyway. `c` is a *chat* split,
/// which is the one verb here Vim has no opinion about.
///
/// Order matters: it is the order the hint panel lists them in, and the list is grouped by what you
/// are trying to do rather than alphabetically, because somebody reading it has a goal and not a
/// letter.
const WINDOW_KEYS: &[(&str, &str)] = &[
    ("v", "window.split.right"),
    ("s", "window.split.down"),
    ("c", "window.split.chat"),
    ("q", "window.close"),
    ("o", "window.only"),
    ("h", "window.left"),
    ("j", "window.down"),
    ("k", "window.up"),
    ("l", "window.right"),
    ("w", "window.next"),
    // Vim's resize keys, which are also the sidebar's — `<` and `>` narrow and widen a panel
    // everywhere else in this workspace, so they had better do it here. They were on tab-move,
    // which was wrong on both counts: against Vim, where `<C-w><` is narrower, and against the
    // program they are in, where the first thing anybody tries for "make this smaller" is `<`.
    // `<lt>`, not `<`. A bare `<` starts a special-key name in the notation this parses, so the
    // binding was rejected as an unterminated `<` and never registered — the key simply did
    // nothing, while `>` beside it worked. Vim spells the literal the same way and for the same
    // reason.
    ("<lt>", "window.narrower"),
    (">", "window.wider"),
    ("-", "window.shorter"),
    ("+", "window.taller"),
    // Shift-and-a-direction *moves the pane* that way, which is Vim's meaning and the one thing
    // that was missing: you could move between panes and not move a pane, so a layout that came
    // out the wrong way round had to be closed and rebuilt. These were a second set of resize keys
    // until `<`/`>`/`-`/`+` took that job back — a redundant alias was standing on the only
    // mnemonic the missing verb had.
    ("H", "window.move.left"),
    ("J", "window.move.down"),
    ("K", "window.move.up"),
    ("L", "window.move.right"),
    // Vim's exchange. `x` was tab-close, which is not a Vim key at all and had taken this one.
    ("x", "window.exchange"),
    ("=", "window.equalize"),
    ("t", "tab.open"),
    // Shift for the shell versions of the two keys next to them, which is the one mnemonic that
    // needs no explaining once you know `t` and `s`.
    ("T", "tab.new.term"),
    ("S", "window.split.term"),
    ("X", "tab.close"),
    ("n", "tab.next"),
    ("p", "tab.prev"),
    ("r", "tab.rename"),
    // Tab-move, off `<`/`>` now that resizing has them back. Rare enough to live on a key with no
    // mnemonic, and adjacent to the ones it used to have.
    (",", "tab.move.left"),
    (".", "tab.move.right"),
    ("?", "window.keys"),
];

/// How much weight one resize keypress moves. A tenth of an even share, so a pane crosses the
/// screen in about ten presses — few enough to be quick, many enough to stop where you meant.
const RESIZE_STEP: i16 = 10;
const READY_EVENT: &str = "neosh.ready";

/// The workspace var naming every conversation blocked on an answer.
///
/// Written by whatever serves `ask_user` — the `questions` plugin, normally — and read by whatever
/// draws conversations. The host reads it too, because "is anybody blocked on me" is exactly the
/// question a notification exists to answer, and putting it on a var is what stops there being two
/// answers.
const VAR_ASKING: &str = "question.asking";

/// The same, for conversations blocked on an approval rather than on a question.
///
/// Written by whatever serves `permission_pre` — the `approvals` plugin — and read here for the
/// same reason `question.asking` is: being blocked on a person is a fact about the conversation.
/// Two vars rather than one, because they are two different things to go and do and the panel that
/// draws them may want to say which.
const VAR_PERMITTING: &str = "permission.asking";

/// What the host is streaming into the chat buffer right now.
///
/// Distinguishing the two matters because switching between them has to break the line; appending
/// reasoning onto the end of a sentence of prose would be unreadable.
#[derive(Copy, Clone, PartialEq)]
enum Stream {
    Text,
    Thinking,
}

/// One answer being written into the transcript.
///
/// The rows it occupies are `start .. start + rows`, and nothing else ever writes inside that
/// range: everything appends after it, and the working line is gone by the time the first token
/// arrives. That is what lets a patch be applied as a range replacement rather than by finding the
/// text again — which is the same reasoning as the working line having no row index, arrived at the
/// other way round.
struct Answer {
    md: crate::markdown::Md,
    start: u32,
    rows: u32,
}

/// Startup, as a state machine.
///
/// Config runs before plugin discovery, so `init.ts` gets to decide what else loads — the same
/// relationship `init.lua` has with a Neovim plugin manager. Activation is asynchronous, so the
/// remaining steps hang off the `Loaded` message rather than running straight through.
#[derive(Default)]
struct Startup {
    /// Config scripts still to activate, innermost last.
    scripts: std::collections::VecDeque<(PluginId, std::path::PathBuf)>,
    /// The config script currently activating.
    active: Option<PluginId>,
    /// Directories to search once config has had its say.
    plugin_dirs: Vec<std::path::PathBuf>,
    /// Applied after config, so an explicit flag beats a config file.
    cli_model: Option<String>,
    /// `--plugin-dir`, kept so a reload does not silently drop it.
    cli_plugin_dirs: Vec<std::path::PathBuf>,
    /// Options naming something not declared yet — almost always a plugin's own setting. Retried
    /// each time a plugin finishes loading.
    deferred: Vec<(String, OptionValue)>,
    /// Plugin activations still outstanding, so we know when to give up on `deferred`.
    loading: usize,
    /// True once discovery has run; `rtp.add` after this point cannot take effect.
    discovered: bool,
    /// `--clean`: do not load the plugins embedded in the binary either.
    no_builtin_plugins: bool,
    /// Bumped on every reload and appended to entry URLs as `?v=N`.
    ///
    /// deno_core caches modules by URL, so without this a reload would dutifully re-import the
    /// same file and re-run the *previous* code — a reload that reports success and changes
    /// nothing, which is worse than no reload at all.
    generation: u32,
    /// Re-reads every config layer from disk. Owned by the caller, because the trust store and the
    /// XDG paths are not the host's business.
    reload: Option<Box<dyn Fn() -> anyhow::Result<Resolved>>>,
    /// Where `opt.save` writes. See [`Bootstrap::config_file`].
    config_file: Option<std::path::PathBuf>,
    /// An `agent.model` naming an instance nothing serves *yet*.
    ///
    /// Provider instances can come from plugins, and plugins load after configuration is applied.
    /// Reporting "no provider serves lab/brainy" at the moment the option is set would be wrong for
    /// the entirely ordinary case of a config selecting a model that a plugin is about to register.
    /// Held and retried, then reported only if it still does not resolve once everything has loaded
    /// — the same shape as a held option value.
    pending_model: Option<String>,
    /// A prompt from the command line, said once everything that could answer it has loaded.
    /// See [`Bootstrap::first_prompt`].
    first_prompt: Option<String>,
    /// Commands a frontend asked for before anything had registered them.
    ///
    /// Held for the same reason `deferred` and `reload_pending` are: a command belongs to a plugin,
    /// plugins load in parallel with the terminal becoming usable, and for the first moment of a
    /// session `session.new` is a name nothing answers to. "No such command" is a wrong answer to
    /// a right question — and once startup has settled, a name still nobody claims is genuinely
    /// unknown and is reported exactly as before.
    commands: Vec<(String, Vec<String>)>,
    /// A reload asked for while something was still loading.
    ///
    /// Queued rather than refused. Startup is asynchronous and now involves several bundled plugins,
    /// so "still loading, try again" is a real window a user can land in by pressing `<C-r>` shortly
    /// after launch — and telling someone to press a key again is a worse answer than remembering
    /// that they pressed it.
    reload_pending: bool,
    /// Provider instances that exist before any configuration is applied — the built-in catalogue,
    /// or the mock in tests.
    ///
    /// Reapplying configuration rebuilds from this rather than adding to whatever is there, which
    /// is the only way an instance you *deleted* from `config.toml` disappears, and the only way an
    /// override reverts to the built-in when you remove it.
    base_instances: Vec<neosh_proto::InstanceConfig>,
}

/// Everything the host needs to start, and to start again.
pub struct Bootstrap {
    pub resolved: Resolved,
    /// Called again on `config.reload`, so a reload reads the files as they are now rather than
    /// replaying what was parsed at launch.
    pub reload: Box<dyn Fn() -> anyhow::Result<Resolved>>,
    pub cli_model: Option<String>,
    pub cli_plugin_dirs: Vec<std::path::PathBuf>,
    /// The `config.toml` a setting chosen on screen is written into. `None` under `--clean`,
    /// which reads and writes nothing.
    pub config_file: Option<std::path::PathBuf>,
    /// Say this as soon as there is something that can answer it.
    ///
    /// `neosh "a prompt"` in a process that *is* its own workspace — `--no-daemon`, the stdio
    /// protocol, the tests. The attaching client cannot do it there, because there is nothing to
    /// attach to: this process is both ends. It waits for the end of startup rather than firing at
    /// launch, because until plugins have loaded there may be no provider that can serve the model
    /// the config asked for, and a turn started against nothing is an error message where an
    /// answer should be.
    pub first_prompt: Option<String>,
}

/// What the transcript reader is waiting for, after a key that cannot act on its own.
///
/// One press each, and the key that follows spends it whatever that key turns out to be — a
/// mistyped second key ends the sequence rather than leaving the next unrelated press to be
/// swallowed by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    /// `g`, waiting for `g`, `e`, `E`, `_` or `v`.
    G,
    /// `z`, waiting for where on the screen to put the cursor's line.
    Z,
    /// `y`, waiting for what to copy — one of the transcript's own, or a motion.
    Yank,
    /// `f`, `F`, `t` or `T`, waiting for a character. Takes the next key *literally*, which is why
    /// it is answered before the count and before every table: `f5` finds a five.
    Find { till: bool, back: bool },
    /// `i` or `a`, waiting for which text object. `yank` is the difference between `viw` and `yiw`.
    Object { around: bool, yank: bool },
}

/// Which text object a key names.
fn object_for(ch: &str) -> Option<vim::Object> {
    Some(match ch {
        "w" => vim::Object::Word { big: false },
        "W" => vim::Object::Word { big: true },
        "p" => vim::Object::Paragraph,
        "\"" => vim::Object::Quote('"'),
        "'" => vim::Object::Quote('\''),
        "`" => vim::Object::Quote('`'),
        "(" | ")" | "b" => vim::Object::Pair('(', ')'),
        "[" | "]" => vim::Object::Pair('[', ']'),
        "{" | "}" | "B" => vim::Object::Pair('{', '}'),
        "<" | ">" => vim::Object::Pair('<', '>'),
        _ => return None,
    })
}

/// One *pane's* place in the workspace.
///
/// Everything in here answers "where am I" rather than "what happened": which conversation is on
/// screen, how far up it you have read, what is half-typed in the composer, which card you have
/// opened, what you searched for. None of it is about the work — the turns, the sessions, the
/// quota, what a plugin registered — and all of that stays on [`Host`], shared by every terminal,
/// because the reason to open a second window is to see the same work from somewhere else.
///
/// **The transcript buffer is in here, not on the conversation.** Two terminals reading one
/// conversation get one set of messages rendered twice, which costs a render and buys the thing
/// that matters: folding a card open is *navigation*, so `⇥` on a diff must not move the rows
/// under somebody else who happens to be in the same conversation. The rows are the same; where
/// you are in them is yours.
///
/// **This used to be one per terminal, and splitting is what made it one per pane.** Every field
/// here was already about a single conversation being read in a single place — a scroll offset, a
/// draft, a half-typed count, a search. Two chats side by side are two of all of that, which is
/// why the split is where it is: nothing had to be divided up, the struct simply stopped being
/// unique. What is genuinely one per terminal — the status line, the quit arming — moved out to
/// [`ViewState`], and there turned out to be two such things.
struct PaneState {
    /// Which rectangle of the main region this is. See [`neosh_proto::PaneId`].
    ///
    /// Held rather than looked up, because every window this pane opens has to be docked *in* it
    /// and the id is what says so. A `PaneState` whose id is not in the editor's tree is a pane
    /// that has been closed and not yet cleaned up, which is a state that exists for exactly as
    /// long as one call takes to return.
    pane: neosh_proto::PaneId,
    /// Whether this pane is a shell rather than a conversation.
    ///
    /// A flag rather than two kinds of `PaneState`, because almost everything about *where you
    /// are* is the same either way — which pane, which window, whether it has the keyboard — and
    /// the handful of places that differ are the ones that write into a transcript. The terminal
    /// itself is not in here: it lives in [`Host::terminals`], keyed by pane, because it owns a
    /// thread and a child process and this struct is cloned and rebuilt freely.
    term: bool,
    /// The conversation on screen here.
    ///
    /// On the pane rather than in the store, which is the whole of what "two conversations at
    /// once" means. The store holds every conversation and the order they were last arrived in;
    /// who is looking at which one is this.
    session: neosh_proto::SessionId,
    chat: BufferId,
    chat_win: WindowId,
    /// Where the transcript is scrolled to, as the host intends it. `None` follows the newest.
    ///
    /// Not read back from the frontend's report: two page-ups inside one 16 ms coalescing window
    /// would both compute from the same stale value and the second would do nothing. The frontend
    /// reports what it drew; this is what we asked for.
    ///
    /// Following the tail is `None` and the first row is `Some(0)`, which are two different places
    /// — the frontend draws the last screenful for one and the first for the other. Sharing zero
    /// between them made the top of a long answer unreachable: `^U` up to it asked for row zero,
    /// which was read as "follow", and the transcript jumped back to the end.
    chat_top: Option<u32>,
    composer: BufferId,
    composer_win: WindowId,
    /// Every tool card in the transcript on screen, in row order.
    ///
    /// A body goes *under its own header*, not at the end of the transcript: an agent driver runs
    /// calls in parallel, so the result of the first can arrive after the header of the third, and
    /// appended it would sit under the wrong card. Inserting it moves every row below, which is
    /// why each card's row lives here rather than with whoever drew it. Rebuilt on every switch,
    /// because the transcript is redrawn from scratch and every row moves.
    cards: Vec<Card>,
    /// How many leading transcript rows the welcome block last occupied, so redrawing it replaces
    /// its own lines and a conversation is recognisable as "not that".
    welcome_rows: usize,
    /// Whether the working line is currently the last row of the chat buffer.
    ///
    /// A fact about the buffer on screen, not about a turn: what the line *says* lives on the
    /// [`Round`], because the turn it reports on may be running in a conversation you are not
    /// looking at.
    working: bool,
    /// How many rows of footer — the plan, and what is out — sit *under* the working line.
    ///
    /// Non-zero also means there is a blank line above the working line, which is the only other
    /// thing anyone needs to know to find either end of the block.
    ///
    /// Part of the working line and not of the transcript, because both are *state*: they say what
    /// is true now, not what happened. Torn down and rebuilt together by
    /// [`Self::below_working`], which is what keeps "the footer is the last rows of the buffer"
    /// true without anything having to remember where it is.
    plan_rows: u32,
    /// Where a question sits that nothing has answered yet, as the first row of its block.
    ///
    /// Set when a question is drawn and cleared by the first thing drawn under it, so it is
    /// exactly "the transcript ends with something you asked and nothing else". Two things need
    /// that fact and both of them were bugs.
    ///
    /// A turn's *closing* rows — its plan, what it left running, what it changed — are about the
    /// answer they close. Appended at the end of the buffer they landed **under** a question
    /// steered in after the last of that answer, so a message you had just typed was not the last
    /// thing in the transcript: two blocks belonging to the previous exchange sat below it. They
    /// go here instead, which is where the turn's own output stopped.
    ///
    /// And a question still unanswered when the turn ends got no reply at all — which is a thing
    /// to *say*, because a question with nothing under it is indistinguishable from one still
    /// being thought about. See [`Host::close_unanswered`].
    unanswered: Option<u32>,
    /// The turn whose time is in the right margin and is still moving, when times are on.
    ///
    /// Only ever the newest one. Every turn above it says a clock time and a duration, both of
    /// which are true forever, so they are written once and never looked at again; the newest also
    /// says *how long ago* it finished, which is the one thing a person walking back to their
    /// terminal actually wants and the one thing that stops being true a minute later. So exactly
    /// one row in the transcript is kept up to date, and it settles into the same fixed form as
    /// the rest the moment another question is asked below it.
    clocked: Option<Clocked>,
    /// When the turn before the one being drawn was asked.
    ///
    /// What decides whether a turn's margin leads with a date: a clock time on its own is a lie by
    /// omission the moment two turns are on different days, and repeating the date on every turn
    /// of an afternoon's work is a column spent saying what the row above it already said.
    /// Compared against the previous turn rather than against *today*, because "today" is a fact
    /// about when the row was drawn and these rows outlive their drawing.
    last_turn_at: Option<i64>,
    /// Set while output is streaming, so chunks append rather than starting a new line.
    streaming: Option<Stream>,
    /// The answer currently being written, and where it sits in the transcript.
    answer: Option<Answer>,
    /// What was in the composer when a change was last announced.
    ///
    /// Kept so [`neosh_proto::PluginEvent::ComposerChanged`] is about the draft rather than about
    /// the redraw.
    draft: String,
    /// A key being typed in, if one is.
    ///
    /// The host collects this itself rather than lending a plugin a text field, because a plugin
    /// field is a buffer, a buffer is drawn, and drawing it would put the key in a `UiEvent` — over
    /// a process boundary, into whatever the frontend logs. Here the value never leaves this
    /// struct: what gets drawn is a row of bullets.
    secret: Option<SecretPrompt>,
    /// What the mode was before `^S`, to be put back on the way out. Stored rather than assumed to
    /// be `Chat`: the door into reading is bound in every mode, so it is reachable from any of
    /// them, and a mode restored to a guess is a keyboard that quietly changed under you.
    mode_before_reading: Mode,
    /// Whether the keyboard is in the transcript rather than the composer.
    ///
    /// A place, not a focus: `j` moves there and types here, and one flag is a great deal easier to
    /// reason about than inferring intent from which window happens to be focused.
    reading: bool,
    /// What the transcript is waiting for, when a key has been pressed that cannot act on its own.
    ///
    /// Cleared by the key that follows, whatever it is, so a mistyped second key ends the sequence
    /// instead of leaving the next unrelated press to be swallowed by it.
    reading_pending: Option<Pending>,
    /// A count typed before a motion in the transcript — `5j`, `12G`.
    ///
    /// Kept as the digits rather than a number so that it can be shown while it is half typed: a
    /// count is two keystrokes with nothing between them, and the first one is otherwise a press
    /// that appears to have done nothing at all. Ended by whatever motion spends it, and by any
    /// key that is not one.
    reading_count: String,
    /// What the running selection means. `Exclusive` when there is none.
    ///
    /// The core's, not a flag of our own: linewise used to be an invariant this end re-established
    /// after every motion — three API calls to move an anchor that is only settable by standing on
    /// it — and every path that moved the cursor without knowing to run it left the wrong rows lit.
    reading_shape: SelectShape,
    /// The column `j` and `k` are trying to get back to, as a grapheme-cluster index.
    ///
    /// Ours rather than the window's, because reading resolves its own motions: `$` then `j` then
    /// `j` walks down the right-hand edge of the transcript in Vim, and a column recomputed from
    /// wherever the cursor landed on a short row cannot do that.
    reading_goal: Option<u32>,
    /// The last `f`/`F`/`t`/`T`, so `;` and `,` have something to repeat.
    reading_find: Option<(char, bool, bool)>,
    /// The selection `gv` puts back: its two ends and its shape, kept when one is given up.
    reading_last_select: Option<((u32, u32), (u32, u32), SelectShape)>,
    /// The search being typed in the transcript, if one is.
    search: Option<SearchPrompt>,
    /// A tab being named, if one is. Per pane rather than per view for the same reason the search
    /// is: it borrows *this* pane's composer, and giving it back has to put this pane's draft back.
    rename: Option<RenamePrompt>,
    /// The last thing searched for, so `n` still works after the prompt has closed.
    searched: String,
    /// Which way it was going, because `n` means "onwards" rather than "forwards": after `?foo`,
    /// `n` is up the transcript and `N` is down it.
    searched_back: bool,
}

impl PaneState {
    /// Furnish one pane: a transcript and a field to type in, both docked inside it.
    ///
    /// The two windows name the pane, which is the whole mechanism — the frontend resolves the
    /// pane's rectangle from the tree and then carves these off *it*, so a composer at the foot of
    /// a pane and a composer at the foot of the screen are the same call asked about different
    /// rectangles. Nothing here knows how many panes there are or where this one is.
    fn furnish(
        editor: &mut Editor,
        view: neosh_proto::ViewId,
        pane: neosh_proto::PaneId,
        session: neosh_proto::SessionId,
    ) -> Self {
        let chat = editor.create_buffer_of_kind("[chat]", KIND_TRANSCRIPT);
        let chat_win = editor.open_window_in(view, chat, WindowLayout::Docked {
            pane: Some(pane),
            dock: Dock::Main,
            size: None,
            // A conversation settles against the field you answer it in. Anchored to the top, a
            // two-line exchange floats at the ceiling with a screen of nothing under it, and the
            // eye has to travel the whole window between what was said and where you say the next
            // thing.
            gravity: Gravity::End,
            wrap: None,
        });

        let composer = editor.create_buffer_of_kind("[composer]", KIND_COMPOSER);
        // Four rows at rest: a rule, up to three lines of what you are writing, and the shortcut
        // row that lives on the last of them as a virtual line. The rule is what makes the
        // composer read as a field rather than as the last paragraph of the transcript. `wrap` is
        // what makes a long prompt fold and the window grow to show it, instead of running off the
        // right edge.
        let composer_win = editor.open_window_in(view, composer, WindowLayout::Docked {
            pane: Some(pane),
            dock: Dock::Bottom,
            size: Some(4),
            gravity: Gravity::Start,
            wrap: Some(true),
        });

        Self {
            pane,
            term: false,
            session,
            chat,
            chat_win,
            chat_top: None,
            composer,
            composer_win,
            cards: Vec::new(),
            welcome_rows: 0,
            working: false,
            plan_rows: 0,
            unanswered: None,
            clocked: None,
            last_turn_at: None,
            streaming: None,
            answer: None,
            draft: String::new(),
            secret: None,
            mode_before_reading: Mode::Chat,
            reading: false,
            reading_pending: None,
            reading_count: String::new(),
            reading_shape: SelectShape::Exclusive,
            reading_goal: None,
            reading_find: None,
            reading_last_select: None,
            search: None,
            rename: None,
            searched: String::new(),
            searched_back: false,
        }
    }

    /// Furnish a pane as a shell instead of a conversation.
    ///
    /// One window and no composer: a terminal *is* the field you type into, so a second one below
    /// it would be a box you type in that sends nothing anywhere. `chat_win` and `composer_win` are
    /// deliberately the same window — every path that asks "where does the keyboard rest here"
    /// gets the terminal, which is the true answer, and the couple of places that would write a
    /// transcript into it are guarded on [`PaneState::term`] instead.
    fn furnish_term(
        editor: &mut Editor,
        view: neosh_proto::ViewId,
        pane: neosh_proto::PaneId,
        session: neosh_proto::SessionId,
    ) -> Self {
        let buf = editor.create_buffer_of_kind("[terminal]", KIND_TERMINAL);
        let win = editor.open_window_in(view, buf, WindowLayout::Docked {
            pane: Some(pane),
            dock: Dock::Main,
            size: None,
            gravity: Gravity::Start,
            wrap: None,
        });
        Self {
            pane,
            term: true,
            session,
            chat: buf,
            chat_win: win,
            chat_top: None,
            composer: buf,
            composer_win: win,
            cards: Vec::new(),
            welcome_rows: 0,
            working: false,
            plan_rows: 0,
            unanswered: None,
            clocked: None,
            last_turn_at: None,
            streaming: None,
            answer: None,
            draft: String::new(),
            secret: None,
            mode_before_reading: Mode::Chat,
            reading: false,
            reading_pending: None,
            reading_count: String::new(),
            reading_shape: SelectShape::Exclusive,
            reading_goal: None,
            reading_find: None,
            reading_last_select: None,
            search: None,
            rename: None,
            searched: String::new(),
            searched_back: false,
        }
    }
}

/// One terminal's screen: its panes, and the two things that are its rather than any pane's.
///
/// The list is short on purpose. Everything that turned out to be about *a conversation being read
/// somewhere* stayed on [`PaneState`] when splitting arrived; what is left here is what a terminal
/// has exactly one of however many panes are open in it — the status strip along the bottom, and
/// whether `<C-c>` has been pressed once. The tab and pane *tree* is not here either: the editor
/// owns it, because a plugin can reshape it through the public API and a second copy in the host
/// would be a second copy to keep in step.
struct ViewState {
    /// What is in each pane, keyed by the editor's ids.
    ///
    /// A map rather than a tree: the shape is the editor's business and this only has to answer
    /// "what is in that rectangle". Which also means the two cannot disagree about the shape —
    /// there is only one shape — and a pane in here that the editor has closed is garbage this
    /// collects rather than a layout that renders wrong.
    panes: std::collections::BTreeMap<neosh_proto::PaneId, PaneState>,
    /// The strip along the bottom of the terminal.
    ///
    /// Docked to the *screen* rather than to a pane: it says what is true of this workspace — the
    /// model, the branch, what a plugin has put there — and four copies of it, one per pane, would
    /// be four rows saying the same sentence.
    status: BufferId,
    /// The tab strip across the top of the main region.
    ///
    /// One row, always drawn, even with a single tab open. A bar that appears when you make a
    /// second tab is a bar whose arrival moves every other row down by one — and, worse, one that
    /// nothing on screen has ever mentioned until the moment you happened to find the key. It is
    /// where you learn tabs exist.
    tabline: BufferId,
    tabline_win: WindowId,
    /// The conversation this terminal last *arrived* in.
    ///
    /// What `rehome` compares against to decide whether moving the keyboard is also an arrival.
    /// The obvious guard — "does the store's current differ" — is wrong, because `current_id()` is
    /// the front of the recency order rather than a pointer: unarchiving a conversation moves it
    /// there, so the guard fired and the arrival shoved the pane's own conversation back to the
    /// front, undoing the ordering the unarchive had just established.
    entered: Option<neosh_proto::SessionId>,
    /// When `<C-c>` was last pressed with nothing left to cancel.
    ///
    /// Per terminal, not per pane: the second press is about this keyboard, and having to press it
    /// twice *in the same pane* would make a key that already needs explaining need it twice.
    quit_armed: Option<std::time::Instant>,
}

impl ViewState {
    /// Give a terminal a screen: a status line, and one pane with a conversation in it.
    fn furnish(
        editor: &mut Editor,
        view: neosh_proto::ViewId,
        session: neosh_proto::SessionId,
    ) -> Self {
        // Docked before anything else at the bottom, so it takes the bottom-most row and every
        // pane's composer sits above it. Without a status line the startup screen renders literally
        // nothing — two empty buffers — and "it opened an empty terminal" is indistinguishable from
        // "it crashed".
        let status = editor.create_buffer_of_kind("[status]", KIND_STATUS);
        editor.open_window_in(view, status, WindowLayout::Docked {
            pane: None,
            dock: Dock::Bottom,
            size: Some(1),
            gravity: Gravity::Start,
            wrap: None,
        });

        let tabline = editor.create_buffer_of_kind("[tabs]", KIND_TABLINE);
        let tabline_win = editor.open_window_in(view, tabline, WindowLayout::Docked {
            pane: None,
            dock: Dock::Top,
            size: Some(1),
            gravity: Gravity::Start,
            wrap: None,
        });

        let pane = editor.active_pane(view);
        let session_id = session.clone();
        let first = PaneState::furnish(editor, view, pane, session);
        editor.set_mode(view, Mode::Chat);
        // The field you type in is where unpushed focus lands, so a key bound against
        // `neosh.composer` resolves at rest. See [`Editor::set_home`].
        editor.set_home(view, Some(first.composer_win));

        Self {
            panes: [(pane, first)].into_iter().collect(),
            status,
            tabline,
            tabline_win,
            entered: Some(session_id),
            quit_armed: None,
        }
    }
}

pub struct Host {
    /// Where each terminal is. See [`PaneState`].
    ///
    /// One entry per view somebody is attached to, made when a terminal arrives and taken down
    /// with its last terminal. Never empty: a workspace nobody is looking at keeps the view of
    /// whoever left last, because turns keep running and their output has to have somewhere to go
    /// for the terminal that comes back.
    views: std::collections::BTreeMap<neosh_proto::ViewId, ViewState>,
    editor: Editor,
    agent: Arc<Agent>,
    bridge: Arc<ScriptBridge>,
    frontend: Box<dyn Frontend>,
    /// Marks for the chrome around the composer: the prompt, the placeholder, the shortcut row.
    /// Its own namespace so redrawing it cannot disturb anything else on that buffer.
    composer_ns: neosh_proto::NamespaceId,
    /// What the row under the composer says, keyed by whoever put it there.
    hints: std::collections::BTreeMap<String, neosh_proto::Hint>,
    /// Whether `agent.model` chose the current selection. A chosen model is never silently
    /// replaced; a remembered one is.
    selection_pinned: bool,
    /// Drivers that run their own agent loop, kept so the permission mode can be mirrored into
    /// them and a request they cannot answer alone can reach a person.
    agent_drivers: Vec<Arc<dyn neosh_provider::drivers::AgentDriver>>,
    status_ns: neosh_proto::NamespaceId,
    /// Where the tab strip's own marks live. Its own namespace, so redrawing the bar several times
    /// a second cannot disturb anything else.
    tabline_ns: neosh_proto::NamespaceId,
    /// Where the transcript's own highlights live.
    ///
    /// Separate from the status line's so that redrawing the working line — which happens several
    /// times a second — cannot clear a mark on a message from ten turns ago.
    chat_ns: neosh_proto::NamespaceId,
    /// What plugins put in the status line, by key.
    ///
    /// A `BTreeMap` so iteration order is deterministic; the render sorts by priority anyway, but a
    /// stable base ordering is what stops two segments with equal priority swapping places between
    /// redraws.
    status_segments: std::collections::BTreeMap<String, neosh_proto::StatusSegment>,

    /// One entry per conversation with a turn in flight.
    ///
    /// A map rather than an `Option`, because a turn belongs to a conversation and not to the
    /// program. Switching used to be refused while one ran; now switching is just switching, and
    /// `<Esc>` cancels the turn of the conversation you are looking at.
    ///
    /// **An entry lives until [`AgentEvent::TurnEnded`] arrives, not until the turn is cancelled.**
    /// See [`Running::cancelling`] for what removing it early cost.
    turns: std::collections::HashMap<neosh_proto::SessionId, Running>,
    /// What each conversation with a turn in flight is doing, and what it has said so far in the
    /// round it is in the middle of.
    ///
    /// Kept for every running conversation, on screen or not: a round's text only reaches
    /// `session.messages` when its stream closes, so a transcript rebuilt from messages alone would
    /// show a conversation that is visibly working and has said nothing.
    rounds: std::collections::HashMap<neosh_proto::SessionId, Round>,
    /// What each driver said it accepts as a slash command, by conversation.
    ///
    /// Learned at the driver's handshake, not configured: which commands exist depends on the
    /// install — project commands, plugin commands and MCP prompts all count — so any list written
    /// down here would be wrong on the first machine that had one of its own. Outlives the turn
    /// that learned it, because the menu is opened between turns — and outlives the *workspace*
    /// through [`Host::remembered_commands`], because it is also opened before them. A driver only
    /// says this once its process is up, which is on the first turn; a conversation you have just
    /// opened has not had one, and every command the agent accepts would be missing from the menu
    /// at exactly the moment somebody typed `/` looking for it.
    driver_commands:
        std::collections::HashMap<neosh_proto::SessionId, Vec<neosh_proto::DriverCommand>>,
    /// Set while a multi-key sequence is half typed, so the loop knows to arm the timeout that
    /// eventually gives up on it.
    keys_pending: bool,
    /// The one screen a workspace nobody is watching keeps.
    ///
    /// Zero views is an ordinary state — closing the last terminal leaves the turns running — but
    /// it must not mean losing where you were, so the last one is kept rather than taken down and
    /// handed to the next terminal that attaches. What that buys is the case this design was written
    /// for: leave mid-answer, come back, and the answer is still arriving into the same transcript
    /// with the same scroll position, instead of a rebuild that is missing everything the turn has
    /// said and not yet committed.
    orphan: Option<neosh_proto::ViewId>,
    /// The pane being written into, when that is not the pane with the keyboard.
    ///
    /// Set only for the duration of [`Host::in_pane`], which is how a turn's output reaches the
    /// transcript reading its conversation while somebody types in the pane next door. `None` — the
    /// state it is in for every key press — means `v()` answers with wherever the person is.
    ///
    /// An override rather than a focus move, because those are different facts: focus is where keys
    /// go, and an answer arriving must never take the keyboard out of the field you are mid-sentence
    /// in. It is cleared by [`Host::in_view`] too, since a pane id from the terminal you have just
    /// left names nothing on the one you have arrived at.
    acting_pane: Option<neosh_proto::PaneId>,
    /// The window and buffer of the prefix panel, while it is up.
    ///
    /// On the host rather than on a pane: it is a hint about the keyboard, and there is one
    /// keyboard. Two of them on screen at once would be two answers to the same half-typed chord.
    window_hint: Option<(WindowId, BufferId)>,
    /// The "what goes in this tab" panel, while it is up.
    ///
    /// On the host rather than on a pane, like the prefix hint: it is a question about the keyboard
    /// and there is one keyboard.
    tab_choice: Option<TabChooser>,
    /// Every shell running in a pane, by pane. See [`crate::term`].
    terminals: crate::term::Terminals,
    /// A pane that has just been made and is to be furnished as a shell rather than a conversation.
    ///
    /// `sync_panes` is what furnishes a pane, and it runs after the fact — it sees a pane the tree
    /// has and this does not, with nothing to say which kind it should be. Set by the command that
    /// created it and taken by the furnishing, which is one turn of the loop later at most.
    ///
    /// It carries *which machine* as well, because a remote shell is furnished identically and
    /// differs only in what the pty is on the end of — see [`crate::term::Pipe`]. Pointing this at
    /// a node rather than adding a second pending slot keeps the one invariant that matters: at
    /// most one pane is mid-furnishing, and whichever command set it is the one that gets it.
    pending_term: Option<(neosh_proto::PaneId, ShellAt)>,
    /// The last terminal to send anything.
    ///
    /// The fallback for work that is nobody's key press and names no window — a plugin drawing on
    /// a timer, most of all. A guess, and said to be one: what makes it rarely wrong is that a
    /// plugin acting on its own almost always names a window, and what makes it never the only
    /// answer is that a plugin can say which view outright.
    last_input: neosh_proto::ViewId,
    /// Whose half-typed sequence it is.
    ///
    /// The timeout fires from a timer, and a timer is nobody's key press — so by the time it
    /// arrives the terminal that typed the `g` has to have been written down rather than asked
    /// for. Replayed into the wrong view, an abandoned prefix is a character appearing in a
    /// composer nobody was typing in.
    keys_view: neosh_proto::ViewId,
    /// A key was just fed, so the sequence timeout needs restarting.
    ///
    /// Every key restarts it, as in Neovim: the wait is for *the next* key, not for the whole
    /// sequence. Arming once at the start of a chord gives you less time for each key you type,
    /// which is exactly backwards.
    keys_touched: bool,
    /// Highlights for search hits. Its own namespace so clearing them cannot take the transcript's
    /// own colour with it.
    search_ns: neosh_proto::NamespaceId,
    startup: Startup,
    /// Set by the `stop` command — and by `quit`, when this host *is* the process; the run loop
    /// notices and shuts down cleanly.
    quitting: bool,
    /// Whether this shutdown is the second half of an update rather than somebody finishing.
    ///
    /// The same shutdown either way — conversations flushed, plugins torn down — and a completely
    /// different thing for the *terminal* to do next. It rides out on [`UiEvent::Shutdown`],
    /// because the far end cannot work it out: a closed socket is a closed socket.
    restarting: bool,
    /// What `quit` means for this host.
    ///
    /// Two different things, and they used to be the same thing because there was only ever one
    /// place to be. In a workspace serving a terminal, `^Q` closes the *terminal*: the turns keep
    /// running, the plugins stay loaded, and reattaching puts you back where you were. Stopping
    /// the workspace is a thing you say on purpose, with `neosh stop`.
    on_quit: OnQuit,
    /// Set by `quit` while serving; the run loop sends the viewer away and carries on.
    /// The terminal that asked to leave, if one has. `^Q` closes *that* window and no other.
    ///
    /// Which terminal is not a guess from whoever spoke most recently: input arrives already
    /// tagged with the client it came from, and this is set while that very event is being
    /// handled.
    detaching: Option<crate::clients::ClientId>,
    /// The terminal whose input is being handled right now, and the view it is looking at.
    ///
    /// [`crate::clients::Source::LOCAL`] between events and in a process that is its own terminal.
    /// A command run from a plugin or a signal is nobody's key press, and lands here as the local
    /// one — which in a served workspace names no terminal, and so closes none.
    from: crate::clients::Source,
    /// Where to report what this workspace is holding, when it is a workspace rather than a
    /// process somebody is watching.
    live: Option<std::sync::Arc<crate::daemon::Live>>,
    /// When that report was last made, so a streaming turn does not remake it per frame.
    live_at: Option<std::time::Instant>,
    /// How many turns were running when it was made. A change here skips the rate limit: whether
    /// something is running is the one thing a status query is actually about, and answering it a
    /// second late is answering the wrong question.
    live_turns: usize,
    /// Driver kinds each plugin registered, so unloading takes them with it.
    plugin_drivers: std::collections::HashMap<PluginId, Vec<neosh_proto::DriverKind>>,
    /// What each loaded plugin declared in its manifest.
    ///
    /// Consulted for capabilities enforced per plugin rather than per model action — repository
    /// writes today. Cleared and rebuilt on reload, so removing a permission from a manifest takes
    /// it away rather than leaving the previous grant in place.
    /// Plugins held until something asks for them. See [`neosh_proto::PluginManifest::activation`].
    lazy: std::collections::HashMap<PluginId, Lazy>,
    /// Every plugin discovered this generation, for `plugins.list`: its manifest, whether it is
    /// bundled, and what became of it.
    plugin_infos: std::collections::BTreeMap<String, neosh_proto::PluginInfo>,
    /// Commands that woke a plugin and are waiting for it to come up, to be run then.
    waking: std::collections::HashMap<PluginId, Vec<(String, Vec<String>)>>,
    /// What the bus was last told about focus and mode, per view, so a frame says only what moved.
    last_focus: std::collections::HashMap<neosh_proto::ViewId, Option<WindowId>>,
    last_mode: std::collections::HashMap<neosh_proto::ViewId, Mode>,
    plugin_permissions: std::collections::HashMap<PluginId, Vec<neosh_proto::PluginPermission>>,
    /// What each plugin said it offers — points, kinds, vars — from its manifest. Read by
    /// `ext.points()` and by the check that a contribution lands on a point somebody reads.
    plugin_provides: std::collections::HashMap<PluginId, neosh_proto::PluginProvides>,
    /// Models discovered from each endpoint, for the life of the session. Cleared on reload,
    /// because a reload may have added, removed or repointed an instance.
    model_cache: crate::services::ModelCache,
    /// Where conversations are saved. `None` under `--clean`, which reads and writes nothing.
    state_dir: Option<std::path::PathBuf>,
    /// Pictures attached to a message that has not been sent yet, per conversation.
    ///
    /// Per conversation for the reason the draft is: `^T` away mid-sentence and back again has to
    /// find what you were writing where you left it, and an image you pasted is part of what you
    /// were writing. Emptied when the message goes.
    attached: std::collections::HashMap<neosh_proto::SessionId, Vec<crate::images::Attachment>>,
    /// What plugins remember between runs: pinned projects, fold state, whatever a panel needs to
    /// still be arranged the way you left it.
    pstate: crate::pstate::PluginState,
    /// Whether there is a newer neosh, and what replacing this one would mean here. Held rather
    /// than made per call, because the check is network and the answer is kept.
    updater: crate::update::Updater,
    /// What *everybody* remembers about a project or a conversation — favourites, colours, tags.
    ///
    /// Separate from `pstate` because it is separate in kind: state is private to whoever wrote it,
    /// a var describes a thing and is shared. See [`crate::vars`].
    vars: crate::vars::Vars,
    /// What the other machines are running. Empty and inert unless `[swarm]` names a peer.
    swarm: crate::swarm::Swarm,
    /// The running node, if there is one. `None` when the swarm is off, which is the default.
    swarm_node: Option<neosh_swarm::SwarmHandle>,
    /// Commands sent to peers that have not been answered yet, and who to settle when they are.
    swarm_pending: std::collections::HashMap<String, (PluginId, RequestId)>,
    /// The last thing said to the peers about what is here, so a poll can say nothing when nothing
    /// has changed. `None` until the first publish.
    published: Option<(Vec<neosh_proto::AgentSummary>, Vec<neosh_proto::RemoteProject>)>,
    /// What each of those questions *was*.
    ///
    /// One waiting map for all of them, because a peer that goes away has to fail everything
    /// outstanding and a second map would be a second place to remember to do it. This is only
    /// consulted on the way *back*: a refusal arrives as `Refused { id }` with nothing on it to say
    /// what it is refusing, so the id is the only thing that tells a directory listing from a
    /// started conversation from a shell that was not allowed.
    swarm_asking: std::collections::HashMap<String, SwarmAsk>,
    swarm_next_command: u64,
    /// Shells this machine is running for other machines, by the handle it minted for each.
    ///
    /// The far side of [`crate::term::Pipe::Remote`]. Keyed by handle rather than by peer because
    /// that is what every message about one carries; the peer is kept beside it so a link going
    /// away can take its shells with it, which is the one cleanup nothing else would do — a laptop
    /// closing its lid must not leave a login shell running here for ever.
    swarm_serving: std::collections::HashMap<String, (neosh_proto::NodeId, crate::term::Serving)>,
    swarm_next_pty: u64,
    /// Panes waiting on a peer to answer [`ApiCall::SwarmShell`], by the request id.
    ///
    /// A pane exists from the moment the key is pressed and its terminal arrives one round trip
    /// later — so this is what turns the answer back into the pane it is for. Making the pane
    /// afterwards would mean a key that does nothing visible until the network answers, which on a
    /// slow link is indistinguishable from a key that is not bound.
    swarm_opening: std::collections::HashMap<
        String,
        (neosh_proto::ViewId, neosh_proto::PaneId, neosh_proto::NodeId),
    >,
    /// Local conversations somebody on another machine is watching.
    ///
    /// Kept so the turn path can ask "is anyone looking" with a hash lookup rather than building a
    /// wire event per token and handing it to the swarm to throw away. On a workspace nobody is
    /// watching — which is most of them, most of the time — streaming costs one set lookup per
    /// token and nothing else.
    ///
    /// *Which* machines, because a machine watching is somebody reading: a turn that ends in front
    /// of a watcher is not news to anybody, and a set that could only grow said every conversation
    /// ever opened from anywhere was being read for the rest of the workspace's life.
    /// Mirrors whose running turn has been drawn here from its start — by its `TurnStarted`, or
    /// whole out of a history — rather than joined at some token half way through.
    mirror_whole: std::collections::HashSet<neosh_proto::SessionId>,
    /// Mirrors whose last turn was drawn whole and live, so the history its end brings is already
    /// on screen. Spent by that history.
    mirror_drawn: std::collections::HashSet<neosh_proto::SessionId>,
    /// The pictures each mirror's transcript names, by the owner's path: the copy here, and the
    /// ones asked for and not yet arrived. See [`Host::localise_pictures`].
    mirror_pictures: std::collections::HashMap<neosh_proto::SessionId, MirrorPictures>,
    swarm_watched: std::collections::HashMap<
        neosh_proto::SessionId,
        std::collections::HashSet<neosh_proto::NodeId>,
    >,
    /// Machines that proved who they are and are not paired with.
    ///
    /// Kept rather than only notified about: pairing is a decision a person makes at their own
    /// pace, and a machine that asked while you were reading something else should still be
    /// waiting in the list when you look. Reset on restart, because a request that old is one to
    /// make again.
    swarm_strangers: std::collections::HashMap<neosh_proto::NodeId, neosh_proto::SwarmStranger>,
    /// The authorisation list, shared with the running node so pairing needs no restart.
    swarm_allowed: neosh_swarm::Allowed,
    /// The node's events, held here between construction and the loop taking them.
    swarm_events: Option<tokio::sync::mpsc::UnboundedReceiver<neosh_swarm::SwarmEvent>>,
    /// Whether `[swarm] listen` was written by hand — which decides how loud a failure to bind
    /// is. The default port being taken is the ordinary case of a second workspace; an address
    /// somebody chose failing to bind is a config problem they would want named.
    swarm_listen_configured: bool,
    /// What each account has left on its plan, kept between the moments anything says so.
    ///
    /// In the host and not in a plugin for the reason every capability here is: it reads a
    /// credential and it outlives every window that draws it. What a plugin gets is the numbers.
    quota: crate::quota::QuotaStore,
    /// Where a poll's answer comes back to. Polls are network round trips and process spawns, so
    /// they cannot happen on the host loop — the single writer for the editor — and cannot be
    /// awaited by whoever asked, because nobody asked: the interval did.
    quota_tx: tokio::sync::mpsc::UnboundedSender<crate::quota::Polled>,
    quota_rx: Option<tokio::sync::mpsc::UnboundedReceiver<crate::quota::Polled>>,
    /// Conversations whose agent has started saying something nobody asked for.
    ///
    /// Out of band, like the quota poll and for the same reason: it is news from a driver that
    /// arrives when no turn is running, so there is no stream for it to come back on. See
    /// [`neosh_provider::drivers::Unasked`].
    unasked_rx: Option<tokio::sync::mpsc::UnboundedReceiver<Unheard>>,
    unasked_tx: tokio::sync::mpsc::UnboundedSender<Unheard>,
    /// Questions and permission prompts on their way to or from another machine. See
    /// [`crate::relay`].
    relay_rx: Option<tokio::sync::mpsc::UnboundedReceiver<crate::relay::Relay>>,
    relay_tx: tokio::sync::mpsc::UnboundedSender<crate::relay::Relay>,
    /// Prompts open on this machine that a watcher could answer, by the id they went out under.
    offered: std::collections::HashMap<String, Offered>,
    /// Prompts from another machine's conversation that are up on this one, by the owner's id.
    relayed: std::collections::HashMap<String, Relayed>,
    /// What each paired machine can run a conversation with, as it last said. Read by the model
    /// picker, `^E` and the footer while a conversation of that machine's is on screen, so they
    /// show *its* models rather than this machine's.
    catalogues: std::collections::HashMap<neosh_proto::NodeId, neosh_proto::ModelCatalogue>,
    /// Calls waiting on a catalogue that is on its way, by the machine it is coming from.
    catalogue_waiting:
        std::collections::HashMap<neosh_proto::NodeId, Vec<(PluginId, RequestId, ApiCall)>>,
    /// Conversations that reported one of those while the turn it interrupted was still winding
    /// down, and which therefore still have to be heard out.
    ///
    /// The report cannot simply be acted on where it arrives. A driver notices an unasked turn
    /// because *nothing is reading its pipe*, and the moment nothing is reading is the moment the
    /// last turn let go — which is strictly **before** that turn's `TurnEnded` reaches this loop.
    /// So [`Host::hear_out`] is always called with the finished turn still in [`Self::turns`], sees
    /// a turn running, and declines; and because the driver only ever reports a *new* `init`,
    /// nothing would ever mention it again. What the agent said next then sat in the pipe until the
    /// next message was sent, where [`drain_between_turns`](neosh_provider) drops it by design —
    /// the answer to a question that was never put. Which is what it looked like from the keyboard:
    /// a turn ends, you type, something is instantly running, and whatever the agent said in
    /// between is gone under your message.
    ///
    /// Held here instead and spent by the ending that was in the way, so the report survives the
    /// gap rather than falling into it.
    unheard: std::collections::HashSet<neosh_proto::SessionId>,
    /// Set by a turn ending, cleared by the poll that follows it. See [`crate::quota::AFTER_TURN`].
    quota_due: bool,
    /// Conversations that were blocked on an answer as of the last `question.asking`.
    ///
    /// Kept so that the *newly* blocked ones can be told apart from the ones that already were:
    /// the var is rewritten whenever the set changes, so answering one of three questions
    /// re-announces the other two, and a notification per answer is a notification for something
    /// you are in the middle of doing.
    asking: Vec<neosh_proto::SessionId>,
    /// The same, for conversations blocked on an approval. See [`VAR_PERMITTING`].
    permitting: Vec<neosh_proto::SessionId>,
    /// What is worth interrupting somebody for, and what has already been said.
    ///
    /// Holds the gathered burst; the run loop is what delivers it, because only the run loop can
    /// ask the frontend whether anybody is looking. See [`crate::notify`].
    notifier: crate::notify::Notifier,
    /// Calls arriving from connections that have no screen. `None` when nothing is serving.
    ///
    /// See [`crate::daemon::Control`]. It is a channel of its own rather than a shape of
    /// `InputEvent` because an `InputEvent` is something a *view* did, and none of these were done
    /// by a view.
    control_rx: Option<tokio::sync::mpsc::UnboundedReceiver<crate::daemon::Control>>,
    /// Connections listening to everything the workspace broadcasts.
    ///
    /// Behind a lock rather than plain, because [`Host::broadcast`] is called from `&self` in a
    /// dozen places and the alternative — never pruning — is a workspace that accumulates one dead
    /// sender per script that has ever watched it, and clones every token into all of them.
    /// Shared, so a call running off the loop can reach it. A slow call is spawned with only its
    /// [`Services`], and an event it emits has to arrive at the control socket's subscribers as
    /// well as at the plugins — a script watching the stream sees what a plugin sees, or the two
    /// surfaces have come to disagree about what happened.
    subscribers: Subscribers,
    /// Where a picture that had to be fetched comes back to.
    ///
    /// Same reason as the quota channel above: what is on the clipboard is sometimes only the
    /// *address* of a picture, and going to get one is a round trip to a machine we have never met.
    /// The key press cannot wait for it on the host loop, and the conversation it belongs to is the
    /// one that was on screen when the key was pressed rather than whichever one is by the time it
    /// lands.
    image_tx: tokio::sync::mpsc::UnboundedSender<Fetched>,
    image_rx: Option<tokio::sync::mpsc::UnboundedReceiver<Fetched>>,
    /// Repositories by directory, discovered on demand.
    ///
    /// Keyed rather than singular because the *conversation* decides which repository is in play:
    /// a session opened in another checkout — or another worktree of the same one — must report
    /// that tree's branch and changes, or "switch to my other worktree" means nothing. Cached
    /// because `git rev-parse` on every status refresh is a subprocess per keystroke, and a
    /// directory does not stop being a repository while we are looking at it.
    repos: std::collections::HashMap<std::path::PathBuf, Option<neosh_vcs::Git>>,
    /// Where neosh was started, and the fallback for a session that names nowhere.
    cwd: std::path::PathBuf,
    /// What to call each directory a conversation lives in — and what checkout it is, so a list
    /// can group worktrees under the repository they belong to. Worked out once when the
    /// directory is first seen.
    projects: std::collections::HashMap<std::path::PathBuf, ProjectFacts>,
    /// What each directory is called *across machines*. See [`neosh_proto::ProjectKey`].
    ///
    /// Separate from `projects`, which is a label for this screen. This is an identity, resolved
    /// from the git remote once when a directory is first seen and remembered, because a workspace
    /// that ran `git remote get-url` on every inventory publish would run it several times a
    /// second while a turn is going.
    project_keys: std::collections::HashMap<std::path::PathBuf, neosh_proto::ProjectKey>,
    /// What to call this machine to other machines.
    swarm_name: String,
}

/// What `quit` does.
///
/// Not a setting: it follows from whether this host is the process you are looking at or a
/// workspace something else is looking at, and nothing else gets a vote.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OnQuit {
    /// This host is the program. `quit` ends it.
    Stop,
    /// This host is a workspace with a viewer. `quit` sends the viewer away.
    Detach,
}

/// A turn in flight, in whichever conversation it belongs to.
///
/// The working line exists because the gap between pressing Enter and the first token is the
/// longest silence in the program, and a still screen and a wedged process look identical. It
/// carries an elapsed clock for the same reason: "working" with no number is indistinguishable
/// from "stuck".
///
/// This is per conversation rather than per screen, so that switching away from a turn and back
/// shows the same clock it would have shown had you never left.
struct Round {
    /// The word this turn is using. Chosen once per turn — a verb that changes every tick is a
    /// slot machine, and you end up watching it instead of thinking.
    verb: &'static str,
    started: std::time::Instant,
    /// What it is waiting on, when that is something other than the model.
    note: Option<String>,
    /// Whether [`Self::note`] is a *wait* rather than a running tool.
    ///
    /// They end differently and only one of them ends by itself. A tool's note is cleared by the
    /// tool coming back; a wait has no matching event and is cleared by being overtaken — the next
    /// token is the usual way. Without telling them apart, an agent driver that says something
    /// while a command is still running wipes `Running cargo test` off the line and puts the
    /// generic verb back, which is the label flicker the fixed verb was chosen to avoid.
    note_is_wait: bool,
    /// Everything this turn has produced that the conversation does not hold yet.
    ///
    /// Cleared the moment the turn's messages land in the conversation, because from then on the
    /// messages are the truth and this would be a second copy of them. How long that takes is the
    /// whole reason this exists: for a model driver it is the end of each round, but an agent
    /// driver runs its entire loop inside one stream and commits nothing until the turn is over.
    /// Without a record of its own, switching away from such a turn and back left you looking at
    /// your own question and nothing else.
    said: Vec<Said>,
    /// What this turn has changed so far, per file, for the summary under its answer.
    ///
    /// Apart from `said`, which is cleared every time the conversation takes the round's messages
    /// — a model driver does that at the end of every round, and the summary is about the turn.
    changes: Vec<cards::FileStat>,
    /// The agent's own checklist, as of its last report.
    ///
    /// Whole, not patched: the driver sends the whole list every time it changes, and a plan
    /// assembled from increments is only right if every one of them arrived.
    plan: Vec<neosh_proto::PlanStep>,
    /// The sub-agents this turn has out, in the order they started.
    ///
    /// Kept after they finish rather than removed, so a late report about one that already ended
    /// lands somewhere instead of resurrecting it.
    tasks: Vec<Task>,
}

/// A turn's time in the right margin: the mark carrying it, and the instants it is made of.
///
/// The row is deliberately not in here. Text is inserted above this all the time — a card, a
/// diff summary, a plan — and an extmark moves with what it is attached to, so the buffer is
/// asked where it is rather than a number being kept in step with the buffer. That is the same
/// mistake [`PaneState::unanswered`] has to work around by hand, and here it does not have to.
#[derive(Clone, Copy, Debug)]
struct Clocked {
    mark: neosh_proto::ExtmarkId,
    stamp: crate::clock::Stamp,
}

/// One sub-agent, from the turn's point of view.
///
/// What it *found* is not here: that comes back as the result of the call that spawned it, on that
/// call's own card, which is where a reader is already looking. This is the part the card cannot
/// answer while it is still out — whether it is still going, and how long it has been.
struct Task {
    id: neosh_proto::TaskId,
    title: String,
    role: Option<String>,
    /// When this turn heard about it. Not when it started: a conversation resumed with a task
    /// already running hears about it at the first report, and a clock from then is honest about
    /// what it is measuring in a way one counting from an invented start would not be.
    since: std::time::Instant,
    status: neosh_proto::TaskStatus,
}

impl Task {
    /// Whether it belongs in the footer. Running, obviously — and backgrounded, because
    /// "something is still going that nothing is waiting for" is the one people ask about.
    fn listed(&self) -> bool {
        matches!(
            self.status,
            neosh_proto::TaskStatus::Running | neosh_proto::TaskStatus::Backgrounded
        )
    }
}

/// One call a card is holding: what was asked, and what came back.
struct Leg {
    id: neosh_proto::ToolCallId,
    name: String,
    input: serde_json::Value,
    result: Option<neosh_proto::ToolResult>,
    /// When the call started, for a card that was here to see it start.
    ///
    /// `None` for one rebuilt from the conversation: the messages say what happened and not when,
    /// so a restored transcript has no clock on it. Inventing one from the moment the buffer was
    /// drawn would put a number that means "how long ago you pressed `^T`" where a duration goes.
    started: Option<std::time::Instant>,
    /// How long it took, once it came back.
    took: Option<std::time::Duration>,
    /// What a failed call exited with, when its output said.
    exit: Option<i64>,
}

/// A tool card on screen: where it is, and what it shows.
///
/// What it shows is kept so it can be drawn again — folded or opened from the transcript, or
/// finished when the call comes back — without going looking for the call in the conversation,
/// which for a running agent turn does not hold it yet.
///
/// Usually one call. A *run* of calls of one kind, one after another with nothing drawn between
/// them, shares a card: reads, greps and listings fold to one row saying what they all looked at,
/// and commands fold to one row naming them with the last one's output under it. See
/// [`cards::group_header`] and [`cards::groups_with`]. An edit never shares — its diff is the
/// answer, and there is no summarising a diff into a list of names.
struct Card {
    /// One, or a run of read-only calls, in the order they were made.
    legs: Vec<Leg>,
    /// The header's row.
    row: u32,
    /// How many rows the body takes under it.
    body: u32,
    /// How much of the body is on screen: folded, previewed under the cursor, or opened.
    ///
    /// Two of the three are the *cursor's*, not the card's — see [`cards::Open`] and
    /// [`Host::refresh_preview`]. Kept here anyway, because this is what draws the card and
    /// because a run must not grow into a card somebody is looking at.
    open: cards::Open,
    /// Whether this card has already had its moment.
    ///
    /// A landing is an event, and an event happens once. Without this the row would light up again
    /// every time the card is redrawn for any other reason — opening it, folding it, the clock on
    /// the row above it ticking — and a flash that fires on a redraw is a flash that means
    /// "something was drawn", which is not news.
    flashed: bool,
}

impl Card {
    fn new(leg: Leg, row: u32) -> Self {
        Self { legs: vec![leg], row, body: 0, open: cards::Open::Folded, flashed: false }
    }

    /// Whether this card holds a call answering to `id`, and which one.
    fn leg_of(&self, id: &neosh_proto::ToolCallId) -> Option<usize> {
        self.legs.iter().rposition(|l| l.id == *id)
    }

    /// Whether every call on this card has come back.
    fn settled(&self) -> bool {
        self.legs.iter().all(|l| l.result.is_some())
    }

    /// Whether a call `input` could join this card.
    ///
    /// Both sides have to be the same kind of call — reads with reads, commands with commands —
    /// and the card has to have room for the idea: a card whose body is already on screen is a
    /// card the reader has opened, and growing it under them would move what they were reading.
    ///
    /// **A run does not continue past a failure.** A stack of commands shows the last one's
    /// output, so a failed call that is not the last one is a failure whose output folded away —
    /// and the fold is about noise. Ending the run there gives the failure its own card with its
    /// own body, which is what it would have had alone.
    fn takes(&self, input: &serde_json::Value) -> bool {
        self.open == cards::Open::Folded
            && !self.legs.iter().any(|l| l.result.as_ref().is_some_and(|r| r.is_error))
            && self.legs.last().is_some_and(|l| cards::groups_with(&l.input, input))
    }
}

/// A turn the host started and has not yet seen end.
struct Running {
    cancel: CancellationToken,
    /// The turn has been asked to stop — `<Esc>`, or a peer's `Interrupt` — and is winding down.
    ///
    /// It is a flag rather than the absence of an entry, and that is the whole point. Cancelling
    /// used to *remove* the entry, so "is a turn running in this conversation" went false while the
    /// turn was still ending: a message typed in that gap started a **second** turn in the same
    /// conversation, and then the first turn's `TurnEnded` — which removes by conversation, having
    /// no way to tell whose ending it is — tore down the second one's cancellation token and its
    /// [`Round`]. What that looked like was a turn nothing could interrupt, a spinner frozen at
    /// whatever second it had reached, and a third `\u{23ce}` starting yet another turn on a vendor
    /// CLI that keeps one process per conversation.
    ///
    /// Kept, the map is exactly "turns the host is waiting to hear the end of", every entry is
    /// removed by the ending it belongs to, and a message typed while a turn winds down is queued
    /// and started by that ending — which is what the leftover-steering path already did.
    cancelling: bool,
}

/// One thing a running turn has produced, in the order it produced it.
///
/// The same three shapes [`transcript`] draws from a finished conversation, which is the point:
/// a round replayed from here and the same round replayed from messages a second later have to
/// look identical, or switching conversations would rewrite what you had already read.
enum Said {
    Text(String),
    Tool { call: neosh_proto::ToolCall, result: Option<neosh_proto::ToolResult> },
}

/// What a round has said, as the messages it would have committed: the shape a model driver's
/// rounds land in, which is the shape [`transcript`] draws a card and its answer from.
fn said_messages(said: Vec<Said>, at: i64) -> Vec<neosh_proto::Message> {
    use neosh_proto::{ContentBlock, Message, Role};
    let mut out: Vec<Message> = Vec::new();
    let mut blocks: Vec<ContentBlock> = Vec::new();
    for item in said {
        match item {
            Said::Text(text) if text.is_empty() => {}
            Said::Text(text) => blocks.push(ContentBlock::Text { text }),
            Said::Tool { call, result } => {
                blocks.push(ContentBlock::ToolUse {
                    id: call.id.clone(),
                    name: call.name,
                    input: call.input,
                });
                if let Some(r) = result {
                    out.push(Message {
                        role: Role::Assistant,
                        content: std::mem::take(&mut blocks),
                        at: Some(at),
                    });
                    out.push(Message {
                        role: Role::User,
                        content: vec![ContentBlock::ToolResult {
                            tool_use_id: call.id,
                            content: r.content,
                            is_error: r.is_error,
                            images: r.images,
                        }],
                        at: Some(at),
                    });
                }
            }
        }
    }
    if !blocks.is_empty() {
        out.push(Message { role: Role::Assistant, content: blocks, at: Some(at) });
    }
    out
}

// Deliberately no row index. The working line is *always the last line of the transcript* and
// everything else inserts above it, which makes "where is it" a question with one answer instead of
// a number to keep in step with four different append paths. The first version tracked a row and
// went wrong under load, in the one way that is hard to see: a stale index deletes the wrong line.

// A token counter belongs here too, and is deliberately absent: usage is only reported at the end
// of a turn, and a count derived from characters would be a number that looks measured and is not.

/// Words a turn can be doing.
///
/// Deliberately a small list of ordinary present participles. The point is a word that reads as
/// activity without claiming anything about what the model is actually doing, which nobody knows.
const VERBS: &[&str] = &[
    "Thinking", "Working", "Pondering", "Considering", "Reasoning", "Puzzling", "Weighing",
    "Mulling", "Deliberating", "Reckoning", "Chewing", "Turning it over",
];

/// What a turn opened to *hear* the agent out is doing. See [`Host::hear_out`].
///
/// Not one of [`VERBS`], and that is the point: those all describe the agent answering something
/// you asked, and nobody asked this. It is also how the line is recognised when the turn stops
/// being a report and starts being an answer — see the `Steered` arm.
const REPORTING: &str = "Reporting back";

/// One of [`VERBS`], per turn.
///
/// Not random: the turn id would be ideal but is not known yet, and a clock read is the one varying
/// thing already to hand.
fn a_verb() -> &'static str {
    VERBS.get(now_secs().unsigned_abs() as usize % VERBS.len()).copied().unwrap_or("Working")
}

/// A key being typed in, and who is waiting to hear how it went.
/// A search being typed in the transcript.
///
/// It borrows the composer to be typed into, the same way entering a key does, because the
/// alternative is a float over the thing you are searching — which hides the answer at the moment
/// it appears.
struct SearchPrompt {
    query: String,
    /// Opened with `?` rather than `/`, so `n` goes backwards.
    back: bool,
    /// Where the cursor was when it opened. Esc puts it back, because an abandoned search should
    /// leave you where you were rather than wherever the incremental match wandered to.
    from: (u32, u32),
    /// The draft that was in the composer, put back afterwards.
    draft: String,
}

/// Where a shell about to be started actually runs.
///
/// A pane is a pane either way — same window, same surface, same keys — so this is the whole of the
/// difference and it is carried by the one slot that already said "furnish this as a terminal".
#[derive(Clone, Debug, PartialEq, Eq)]
enum ShellAt {
    /// On this machine. `cwd` overrides the pane's conversation's directory, which is what a key
    /// pressed on a *row* means: the sidebar's `t` is about the project the cursor is on, and that
    /// is very often not the conversation the pane is showing.
    Here { cwd: Option<std::path::PathBuf> },
    /// On a peer, in a directory on *its* disk. `cwd` is `None` for that user's home.
    There { node: neosh_proto::NodeId, cwd: Option<String> },
}

/// What a request sent to a peer was, for reading its answer.
///
/// A refusal comes back as a bare `Refused { id }` — the far end is saying no to a question, not
/// describing it — so the id is the only thing left that says which question. See
/// [`Host::swarm_asking`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SwarmAsk {
    /// A command aimed at one of that machine's conversations.
    Command,
    /// A directory listing.
    Browse,
    /// A shell.
    Shell,
    /// Something typed into another machine's conversation opened here, or `Esc` in it. Nobody is
    /// waiting on the answer except the person who pressed the key, so a refusal is said to them.
    Mirror,
    /// An answer to a question or prompt that machine offered. Refused only when somebody else
    /// answered first, which is not news — except a refusal on principle, which is.
    Relay,
    /// That machine's providers and models, for a mirror's model picker and footer.
    Catalogue,
    /// A picture a mirror's transcript names. Nobody pressed anything for it, so a refusal is not
    /// said: the transcript goes on showing the picture's name, which is what it showed before.
    Picture,
}

/// A mirror's pictures, as [`Host::mirror_pictures`] keeps them.
#[derive(Default)]
struct MirrorPictures {
    /// The owner's path, and the copy of it on this disk.
    have: std::collections::HashMap<String, neosh_proto::ImageFile>,
    /// Asked for and not yet here. Once each: a picture the owner will not send is not asked for
    /// again on every redraw.
    asked: std::collections::HashSet<String>,
}

/// A prompt's pictures, as the files they are.
fn picture_files(blocks: &[neosh_proto::ContentBlock]) -> Vec<neosh_proto::ImageFile> {
    blocks
        .iter()
        .filter_map(|b| match b {
            neosh_proto::ContentBlock::Image { path, media_type } => Some(neosh_proto::ImageFile {
                path: path.clone(),
                media_type: media_type.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// The most of a frame the pictures on one message may take, as base64 — under the swarm's
/// eight-megabyte cap with room for the words and the envelope.
const PICTURE_BUDGET: usize = 6 * 1024 * 1024;

/// Every picture a conversation's messages name, by path.
fn pictures_in(messages: &[neosh_proto::Message]) -> Vec<String> {
    use neosh_proto::ContentBlock as B;
    let mut out = Vec::new();
    for m in messages {
        for b in &m.content {
            match b {
                B::Image { path, .. } => out.push(path.clone()),
                B::ToolResult { images, .. } => out.extend(images.iter().map(|i| i.path.clone())),
                _ => {}
            }
        }
    }
    out
}

/// Point every picture in `messages` that has a copy here at the copy.
fn repoint_messages(
    messages: &mut [neosh_proto::Message],
    have: &std::collections::HashMap<String, neosh_proto::ImageFile>,
) {
    use neosh_proto::ContentBlock as B;
    for m in messages {
        for b in &mut m.content {
            match b {
                B::Image { path, media_type } => {
                    if let Some(f) = have.get(path.as_str()) {
                        *path = f.path.clone();
                        *media_type = f.media_type.clone();
                    }
                }
                B::ToolResult { images, .. } => repoint_files(images, have),
                _ => {}
            }
        }
    }
}

/// [`repoint_messages`], for a list of files.
fn repoint_files(
    files: &mut [neosh_proto::ImageFile],
    have: &std::collections::HashMap<String, neosh_proto::ImageFile>,
) {
    for f in files {
        if let Some(local) = have.get(f.path.as_str()) {
            *f = local.clone();
        }
    }
}

/// A prompt open here that the machines watching its conversation were offered.
struct Offered {
    session: neosh_proto::SessionId,
    reply: tokio::sync::oneshot::Sender<crate::relay::Reply>,
    /// Kept so a machine that starts watching while it waits is offered it too.
    offer: crate::relay::Offer,
}

/// Another machine's prompt, up on this one.
struct Relayed {
    /// The mirror it is shown in.
    local: neosh_proto::SessionId,
    offer: crate::relay::Offer,
    /// Asking the person here. Aborted when the prompt is settled somewhere else.
    task: Option<tokio::task::AbortHandle>,
}


/// The panel asking what a new tab should hold — see [`Host::begin_tab_choice`].
struct TabChooser {
    win: WindowId,
    buf: BufferId,
    /// Which row the cursor is on.
    at: usize,
}

/// Naming a tab, borrowing the composer to type in — see [`Host::begin_tab_rename`].
struct RenamePrompt {
    tab: neosh_proto::TabId,
    value: String,
    /// The draft that was in the composer, put back afterwards.
    draft: String,
}

struct SecretPrompt {
    instance: neosh_proto::InstanceId,
    label: String,
    /// What has been typed. Zeroed when the prompt closes, so it does not sit in freed memory.
    value: String,
    /// The draft that was in the composer, put back afterwards. Borrowing the composer costs
    /// nothing as long as it is given back.
    draft: String,
    /// The call to answer when this resolves, if a plugin asked for it.
    waiting: Option<(PluginId, RequestId)>,
}

impl Drop for SecretPrompt {
    fn drop(&mut self) {
        use secrecy::zeroize::Zeroize;
        self.value.zeroize();
    }
}

/// A picture that had to be fetched, arriving after the key press that asked for it.
///
/// Carries the conversation rather than reading one on arrival: switching conversations while a
/// server takes its time is an ordinary thing to do, and the picture belongs to the one you were
/// in when you pressed the key. `waiting` is a plugin owed an answer, if a plugin is what asked.
struct Fetched {
    session: neosh_proto::SessionId,
    waiting: Option<(PluginId, RequestId)>,
    result: Result<crate::images::Attachment, String>,
}

impl Host {
    pub fn new(
        agent: Arc<Agent>,
        bridge: Arc<ScriptBridge>,
        frontend: Box<dyn Frontend>,
    ) -> Self {
        let mut editor = Editor::new();
        // Whichever conversation the store was built with. There is exactly one terminal at this
        // point and it has to be somewhere; every other view is put somewhere as it attaches.
        let here = agent.sessions().current_id().clone();
        let (quota_tx, quota_rx) = tokio::sync::mpsc::unbounded_channel();
        let (unasked_tx, unasked_rx) = tokio::sync::mpsc::unbounded_channel();
        let (relay_tx, relay_rx) = tokio::sync::mpsc::unbounded_channel();
        let (image_tx, image_rx) = tokio::sync::mpsc::unbounded_channel();

        let screen = ViewState::furnish(&mut editor, neosh_proto::ViewId::LOCAL, here);

        let mut namespace = |name: &str| {
            match editor.apply(&PluginId::from(BUILTIN), ApiCall::NsCreate { name: name.into() }) {
                Ok(ApiOk::Ns { ns }) => ns,
                _ => neosh_proto::NamespaceId(0),
            }
        };
        let status_ns = namespace("neosh.status");
        let chat_ns = namespace("neosh.chat");
        let composer_ns = namespace("neosh.composer");
        let search_ns = namespace("neosh.search");
        let tabline_ns = namespace("neosh.tabline");

        let mut host = Self {
            editor,
            agent,
            bridge,
            frontend,
            views: [(neosh_proto::ViewId::LOCAL, screen)].into_iter().collect(),
            status_ns,
            chat_ns,
            composer_ns,
            search_ns,
            tabline_ns,
            status_segments: Default::default(),
            hints: Default::default(),
            selection_pinned: false,
            agent_drivers: Vec::new(),
            turns: std::collections::HashMap::new(),
            rounds: std::collections::HashMap::new(),
            driver_commands: std::collections::HashMap::new(),
            keys_pending: false,
            keys_touched: false,
            startup: Startup::default(),
            quitting: false,
            restarting: false,
            on_quit: OnQuit::Stop,
            detaching: None,
            from: crate::clients::Source::LOCAL,
            keys_view: neosh_proto::ViewId::LOCAL,
            last_input: neosh_proto::ViewId::LOCAL,
            // Nothing is attached yet, so the screen just furnished is one nobody is
            // watching — and the first terminal to arrive should be given it rather than a second
            // one beside it. In a process that is its own terminal nothing ever adopts it, which
            // is correct: there is one view and it is this.
            orphan: Some(neosh_proto::ViewId::LOCAL),
            acting_pane: None,
            window_hint: None,
            tab_choice: None,
            terminals: Default::default(),
            pending_term: None,
            live: None,
            live_at: None,
            live_turns: 0,
            plugin_drivers: Default::default(),
            plugin_permissions: Default::default(),
            plugin_provides: Default::default(),
            lazy: Default::default(),
            plugin_infos: Default::default(),
            waking: Default::default(),
            last_focus: Default::default(),
            last_mode: Default::default(),
            model_cache: Default::default(),
            state_dir: None,
            attached: Default::default(),
            pstate: crate::pstate::PluginState::new(None),
            // From the build identity captured at startup: a workspace outlives the file it was
            // loaded from, so `current_exe` asked later is a path with `(deleted)` on the end.
            updater: crate::update::Updater::new(
                crate::build::exe_path().unwrap_or_default(),
                crate::build::launch_path(),
                crate::build::capture().version.clone(),
            ),
            vars: crate::vars::Vars::new(None),
            swarm: Default::default(),
            swarm_node: None,
            swarm_pending: Default::default(),
            published: None,
            swarm_asking: Default::default(),
            swarm_serving: Default::default(),
            swarm_next_pty: 0,
            swarm_opening: Default::default(),
            swarm_next_command: 0,
            swarm_watched: Default::default(),
            mirror_whole: Default::default(),
            mirror_drawn: Default::default(),
            mirror_pictures: Default::default(),
            swarm_strangers: Default::default(),
            swarm_allowed: neosh_swarm::Allowed::load(None),
            swarm_events: None,
            swarm_listen_configured: false,
            quota: crate::quota::QuotaStore::new(None),
            quota_tx,
            quota_rx: Some(quota_rx),
            unasked_rx: Some(unasked_rx),
            unasked_tx,
            relay_rx: Some(relay_rx),
            relay_tx,
            offered: Default::default(),
            relayed: Default::default(),
            catalogues: Default::default(),
            catalogue_waiting: Default::default(),
            unheard: std::collections::HashSet::new(),
            quota_due: false,
            asking: Vec::new(),
            permitting: Vec::new(),
            notifier: crate::notify::Notifier::new(Default::default()),
            control_rx: None,
            subscribers: Default::default(),
            image_tx,
            image_rx: Some(image_rx),
            repos: Default::default(),
            cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            projects: Default::default(),
            project_keys: Default::default(),
            swarm_name: machine_name(),
        };
        // Before anything reads one. Conversations are restored — and therefore drawn — before the
        // configuration is resolved, and an option that has not been declared yet reads as the
        // zero of its type: `chat.show_tools` came back false, so a restart showed a transcript
        // with every tool card missing and only the errors left, which is the one thing that
        // setting is documented not to do. `apply_config` declares them again on every reload,
        // which is what keeps a rebound default from surviving the config that replaced it.
        host.declare_builtin_options();
        host
    }

    /// Everything a spawned call needs, snapshotted at the moment of the call.
    ///
    /// `gen.model` is read here rather than stored, so changing it in `config.toml` and reloading
    /// takes effect on the next generation without the host caching a stale copy.
    fn services(&self, plugin: &PluginId) -> Services {
        // The directory of the conversation on screen here, so git answers about the tree you are
        // working in.
        let cwd = self.session_cwd(&self.active_session());
        let git = self.repos.get(&cwd).cloned().flatten();
        Services {
            agent: self.agent.clone(),
            git,
            cwd,
            gen_model: self.editor.options().str("gen.model").unwrap_or_default().to_string(),
            may_write_vcs: self
                .plugin_permissions
                .get(plugin)
                .is_some_and(|p| p.contains(&neosh_proto::PluginPermission::VcsWrite)),
            caller: plugin.0.clone(),
            state_dir: self.state_dir.clone(),
            bridge: self.bridge.clone(),
            subscribers: self.subscribers.clone(),
            model_cache: self.model_cache.clone(),
            updater: self.updater.clone(),
        }
    }

    /// Bring a conversation in from disk, if that is where it still is.
    ///
    /// `RESTORE_LIMIT` makes the store a *window* onto the sessions directory rather than the whole
    /// of it: past a few hundred conversations, a perfectly real one is a file and nothing else. So
    /// every verb that names a conversation — open it, put it back, read it, delete it — asks this
    /// first, and a conversation you found in the archive behaves like any other instead of
    /// answering "no such session" to four keys out of five.
    ///
    /// A no-op for anything already loaded, which is all of them until somebody goes looking. It
    /// loads rather than switching: what to do with it afterwards is the caller's, and `switch` on
    /// something that is merely being read would move the workspace.
    fn hydrate(&mut self, id: &neosh_proto::SessionId) -> bool {
        let known = { self.agent.sessions().get(id).is_some() };
        if known {
            return true;
        }
        let Some(dir) = self.state_dir.clone() else {
            return false;
        };
        let Some(session) = crate::sessions::read(&dir, id) else {
            return false;
        };
        self.agent.sessions().insert(session);
        true
    }

    /// Whether this call has to run off the host loop, whoever asked for it.
    ///
    /// One list, read by the plugin path and by the control socket, so the two cannot come to
    /// disagree about which calls are allowed to block every terminal in the workspace.
    fn is_slow(call: &ApiCall) -> bool {
        matches!(
            call,
            ApiCall::GitStatus { .. }
                | ApiCall::GitBranches { .. }
                | ApiCall::GitWorktrees { .. }
                | ApiCall::GitLog { .. }
                | ApiCall::GitDiff { .. }
                | ApiCall::GitDefaultBranch
                | ApiCall::GitCreateBranch { .. }
                | ApiCall::GitCheckout { .. }
                | ApiCall::GitStage { .. }
                | ApiCall::GitUnstage { .. }
                | ApiCall::GitCommit { .. }
                // Network, and the reason this list exists: a fetch against a remote that is
                // slow, unreachable or asking for a key it will not get takes as long as a TCP
                // timeout, and on the loop that is every terminal in the workspace frozen for it.
                | ApiCall::GitFetch { .. }
                // Somebody else's server, over the network, through another program.
                | ApiCall::ForgePulls { .. }
                | ApiCall::GitPull { .. }
                | ApiCall::GitAddWorktree { .. }
                | ApiCall::GitRemoveWorktree { .. }
                // The slowest of the lot by a wide margin: a large history is minutes of network,
                // and on the loop that is a workspace frozen for every terminal in it.
                | ApiCall::GitClone { .. }
                | ApiCall::GenComplete { .. }
                | ApiCall::AgentListModels { .. }
                | ApiCall::PermissionCheck { .. }
                | ApiCall::PathComplete { .. }
                | ApiCall::AskUser { .. }
                | ApiCall::UsageHistory { .. }
                | ApiCall::SessionsStored
                // Network, and a download that can run for a minute. On the loop either of these
                // would stop every terminal attached to this workspace for the length of an HTTP
                // request to somebody else's server.
                | ApiCall::UpdateCheck { .. }
                | ApiCall::UpdateApply
        )
    }

    /// Run a slow call off the host loop, answering the plugin when it finishes.
    ///
    /// The host loop is the single writer for the editor. Awaiting `git status` inside it freezes
    /// typing for the length of a subprocess; awaiting a model round-trip freezes it for seconds.
    /// Returning `true` means this call has been taken over and must not also be dispatched.
    fn spawn_slow(&mut self, plugin: &PluginId, id: Option<RequestId>, call: &ApiCall) -> bool {
        if !Self::is_slow(call) {
            return false;
        }
        let svc = self.services(plugin);
        let plugin = plugin.clone();
        let call = call.clone();
        let bridge = self.bridge.clone();
        tokio::spawn(async move {
            let result = run_slow(svc, call).await;
            match id {
                Some(id) => {
                    let response: ApiResponse = result.into();
                    let _ = bridge.script().send(ScriptInbound::Plugin {
                        plugin,
                        msg: PluginInbound::Response { id, response },
                    });
                }
                // A notify has nowhere to return an error, so surface it rather than losing it.
                None => {
                    if let Err(e) = result {
                        tracing::warn!(%plugin, "{e}");
                    }
                }
            }
        });
        true
    }

    /// Every contribution point, with who reads it and who writes to it.
    ///
    /// Readers come from manifests (`[provides] points`), writers from the registry. The host's
    /// own point — `ui.theme` — is declared here, because the host has no manifest to say so in.
    fn points(&self) -> Vec<neosh_proto::PointInfo> {
        let mut readers: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        readers.entry(THEME_POINT.to_string()).or_default().push(BUILTIN.to_string());
        for (plugin, provides) in &self.plugin_provides {
            for point in &provides.points {
                readers.entry(point.clone()).or_default().push(plugin.0.clone());
            }
        }
        let mut contributors: std::collections::BTreeMap<String, Vec<String>> =
            self.editor.contributors().into_iter().collect();
        let mut names: Vec<String> = readers.keys().chain(contributors.keys()).cloned().collect();
        names.sort();
        names.dedup();
        names
            .into_iter()
            .map(|point| {
                let mut r = readers.remove(&point).unwrap_or_default();
                r.sort();
                neosh_proto::PointInfo {
                    readers: r,
                    contributors: contributors.remove(&point).unwrap_or_default(),
                    point,
                }
            })
            .collect()
    }

    /// Say, once the workspace is up, which contributions landed on a point nothing reads.
    ///
    /// A point is a bare string and contributing to a misspelt one succeeds silently forever.
    /// Checked at `neosh.ready` rather than at contribute time, because the reader may load after
    /// the writer — and a plugin that reads a point without saying so in its manifest is only
    /// ever a warning, never a refusal, because nothing here is a rule a plugin had to follow
    /// before today.
    fn report_unread_points(&mut self) {
        let points = self.points();
        let known: Vec<String> =
            points.iter().filter(|q| !q.readers.is_empty()).map(|q| q.point.clone()).collect();
        for p in points {
            if !p.readers.is_empty() || p.contributors.is_empty() {
                continue;
            }
            let hint = nearest(&p.point, &known)
                .map(|n| format!(" — did you mean {n:?}?"))
                .unwrap_or_default();
            let msg = format!(
                "{} contributes to {:?}, which no plugin declares it reads{hint}",
                p.contributors.join(", "),
                p.point
            );
            tracing::warn!("{msg}");
            self.editor_message(MessageLevel::Warn, msg);
        }
    }

    /// A contributed theme by name: the variant it is laid over, and its groups.
    ///
    /// A theme is a contribution on `ui.theme` — `{ base: "dark", groups: { Comment: { link:
    /// "NonText" }, Normal: { fg: … } } }` — which is what makes it a thing that can be listed,
    /// chosen in the option sheet, and taken away with its plugin. Each group is checked rather
    /// than trusted; one that does not parse is skipped and named in the log, because a theme
    /// that refuses to load over one bad colour is a theme nobody can fix from inside neosh.
    fn contributed_theme(
        &mut self,
        name: &str,
    ) -> Option<(neosh_core::palette::Variant, Vec<(String, neosh_proto::HighlightDef)>)> {
        let item = self.theme_contributions().into_iter().find(|c| c.id == name)?.item;
        let base = item
            .get("base")
            .and_then(|v| v.as_str())
            .and_then(neosh_core::palette::Variant::parse)
            .unwrap_or_default();
        let mut groups = Vec::new();
        if let Some(map) = item.get("groups").and_then(|g| g.as_object()) {
            for (group, def) in map {
                match parse_highlight(def) {
                    Some(def) => groups.push((group.clone(), def)),
                    None => tracing::warn!(theme = name, group, "theme group does not parse; skipped"),
                }
            }
        }
        Some((base, groups))
    }

    fn theme_contributions(&mut self) -> Vec<neosh_proto::Contribution> {
        let plugin = PluginId::from(BUILTIN);
        match self.editor.apply(&plugin, ApiCall::ExtList { point: THEME_POINT.into() }) {
            Ok(ApiOk::Contributions { contributions }) => contributions,
            _ => Vec::new(),
        }
    }

    /// Keep `ui.theme`'s list of values equal to the themes that exist.
    ///
    /// Re-declared, under the host's own id, so the option sheet offers every contributed theme
    /// beside `dark` and `light`; the registry keeps the chosen value across a re-declaration as
    /// long as it still fits. A theme in use that has just gone — its plugin disabled — no longer
    /// fits, which is what puts the workspace back on a built-in variant rather than leaving it
    /// drawn in colours nothing defines any more.
    fn sync_themes(&mut self) {
        let mut values = vec!["dark".to_string(), "light".to_string()];
        for c in self.theme_contributions() {
            if !values.contains(&c.id) {
                values.push(c.id);
            }
        }
        let plugin = PluginId::from(BUILTIN);
        let current = self.editor.options().get("ui.theme").cloned();
        let Some(entry) = self.editor.options().entry("ui.theme") else { return };
        let spec = OptionSpec {
            name: entry.name,
            ty: OptionType::Enum { values: values.clone() },
            default: entry.default,
            description: entry.description,
        };
        if let Err(e) = self.editor.apply(&plugin, ApiCall::OptDeclare { spec }) {
            tracing::warn!("ui.theme: {e}");
            return;
        }
        match current {
            Some(OptionValue::Str(name)) if !values.contains(&name) => {
                // The theme in use was withdrawn. Back to the built-in default — the
                // re-declaration already dropped the value that no longer fits, so the registry
                // reads `dark`; this is what makes the screen, and every plugin listening, agree.
                let _ = self.editor.apply(&plugin, ApiCall::OptReset { name: "ui.theme".into() });
                self.drain_effects();
            }
            Some(OptionValue::Str(name)) if self.editor.custom_theme() == Some(name.as_str()) => {
                // The theme in use was re-contributed — its plugin reloaded, or its author is
                // iterating on it. Apply the new groups.
                self.apply_option("ui.theme", &OptionValue::Str(name));
            }
            _ => {}
        }
    }

    /// Answer `cmd.call`: run the command where it lives and hand back what it returned.
    ///
    /// The host's own commands are answered here and now with `null` — they return nothing and
    /// run on this loop anyway. A plugin's is asked over the bridge off the loop, and the caller's
    /// request stays open until the owner answers, throws, unloads or times out.
    fn begin_command_call(
        &mut self,
        caller: PluginId,
        id: RequestId,
        name: String,
        args: Vec<String>,
    ) {
        let response: ApiResponse = match self.editor.command_owner(&name) {
            None => Err(ApiError::NotFound { what: format!("command {name}") }).into(),
            Some(owner) if owner.0 == BUILTIN => {
                self.run_builtin(&name, args, None);
                Ok(ApiOk::Json { value: serde_json::Value::Null }).into()
            }
            Some(owner) => {
                let bridge = self.bridge.clone();
                tokio::spawn(async move {
                    let result = bridge.call_command(&owner, &name, args).await;
                    let response: ApiResponse = result
                        .map(|value| ApiOk::Json { value })
                        .map_err(|message| ApiError::Internal { message })
                        .into();
                    let _ = bridge.script().send(ScriptInbound::Plugin {
                        plugin: caller,
                        msg: PluginInbound::Response { id, response },
                    });
                });
                return;
            }
        };
        let _ = self.bridge.script().send(ScriptInbound::Plugin {
            plugin: caller,
            msg: PluginInbound::Response { id, response },
        });
    }

    fn chat_line(&mut self, text: impl Into<String>) {
        // Through the same insertion point as everything else, so a line written mid-turn lands
        // above the working line rather than under it.
        self.chat_push(vec![text.into()]);
    }

    fn composer_text(&self) -> String {
        self.editor
            .buffer(self.v().composer)
            .map(|b| b.get_lines(0, b.line_count()).join("\n"))
            .unwrap_or_default()
    }

    fn set_composer(&mut self, text: &str) {
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::BufSetLines {
            buf: self.v().composer,
            start: 0,
            end: -1,
            lines: vec![text.to_string()],
        });
        // Keep the caret at the end of what has been typed. Columns are UTF-8 byte offsets, so this
        // is the byte length — not the character count, which would put the caret inside a
        // multi-byte character the moment anyone types anything but ASCII.
        let last = text.lines().count().saturating_sub(1) as u32;
        let col = text.rsplit('\n').next().unwrap_or("").len() as u32;
        let _ = self.editor.apply(&plugin, ApiCall::WinSetCursor {
            win: self.v().composer_win,
            row: last,
            col,
        });
        // A selection over text that no longer exists is a highlight in mid-air.
        let _ = self
            .editor
            .apply(&plugin, ApiCall::WinSelect { win: self.v().composer_win, on: false });
        // The placeholder appears and disappears with the text, so the chrome has to follow every
        // write and not just every keystroke.
        self.refresh_composer();
    }

    /// What a plugin must have declared in its `plugin.toml` to make this call, if anything.
    ///
    /// Most of the API needs nothing: drawing a window, reading a buffer and binding a key are no
    /// more privileged than what the person sitting there can already do. These four are the ones
    /// that are *about* something other than the screen, and each was already declarable —
    /// [`neosh_proto::PluginPermission`] has listed them since it existed, `neosh plugins` prints
    /// them, and a user reading a manifest was entitled to believe them.
    ///
    /// They were not checked. Only `vcs_write` ever was, which made the rest documentation that
    /// looked like a boundary: a plugin declaring nothing could register a tool the model calls,
    /// take a blocking hook and veto every turn, or stand in as a model provider. That is the wrong
    /// way round for a workspace that runs other people's code and, on the swarm, takes commands
    /// from other machines.
    ///
    /// A non-blocking hook needs nothing: an observer cannot refuse anything, which is exactly the
    /// distinction `hooks_blocking` names.
    fn permission_for(call: &ApiCall) -> Option<neosh_proto::PluginPermission> {
        use neosh_proto::PluginPermission as P;
        match call {
            ApiCall::ToolRegister { .. } => Some(P::Tools),
            ApiCall::HookRegister { blocking: true, .. } => Some(P::HooksBlocking),
            ApiCall::ProviderRegisterDriver { .. } => Some(P::Providers),
            ApiCall::SurfaceClaim { .. } => Some(P::RawCells),
            _ => None,
        }
    }

    /// Whether `plugin` declared `want`.
    ///
    /// The host's own calls are not a plugin's and are not gated: `BUILTIN` is the binary drawing
    /// its own UI, and it has no manifest to declare anything in.
    fn may(&self, plugin: &PluginId, want: neosh_proto::PluginPermission) -> bool {
        plugin.0 == BUILTIN
            || self.plugin_permissions.get(plugin).is_some_and(|p| p.contains(&want))
    }

    /// Route a plugin call: UI-domain calls to the core, agent-domain calls to the agent layer.
    async fn dispatch(&mut self, plugin: &PluginId, call: ApiCall) -> ApiResult {
        if let Some(want) = Self::permission_for(&call)
            && !self.may(plugin, want)
        {
            let name = serde_json::to_value(want)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| format!("{want:?}"));
            // Says the fix, not just the refusal. A plugin author reading this in a log has one
            // line to act on rather than a capability name and a guess about where it goes.
            return Err(ApiError::NotPermitted {
                capability: format!(
                    "{name} (plugin {plugin} did not declare it \u{2014} add \"{name}\" to the \
                     permissions list in its plugin.toml)"
                ),
            });
        }
        if Editor::handles(&call) {
            let view = self.view_for(&call);
            let r = self.editor.apply_in(view, plugin, call);
            self.drain_effects();
            return r;
        }
        self.agent_call(plugin, call).await
    }

    async fn agent_call(&mut self, plugin: &PluginId, call: ApiCall) -> ApiResult {
        match call {
            ApiCall::AgentSend { text, images } => {
                // A plugin's own images go on the row first, so they travel with whatever is
                // already there and in the order they were added — the send is one message.
                for path in &images {
                    if let Err(e) = self.attach(std::path::Path::new(path)) {
                        return Err(ApiError::InvalidArgument { message: e });
                    }
                }
                self.start_turn(text);
                Ok(ApiOk::Unit)
            }
            ApiCall::AgentCommand { session, command } => {
                let session = session.unwrap_or_else(|| self.active_session());
                let session = self
                    .apply_agent_command(&session, command)
                    .map_err(|r| match r {
                        neosh_proto::Refusal::NoSuchAgent => ApiError::NotFound {
                            what: format!("conversation {session}"),
                        },
                        neosh_proto::Refusal::NotPermitted { what } => {
                            ApiError::Denied { reason: what }
                        }
                        neosh_proto::Refusal::Busy { message }
                        | neosh_proto::Refusal::Failed { message } => {
                            ApiError::InvalidArgument { message }
                        }
                    })?;
                Ok(ApiOk::MaybeSession { session })
            }
            ApiCall::ChatAttach { path } => {
                // `None` means the clipboard, and the clipboard is sometimes only the *address* of
                // a picture — so it is taken over before dispatch, where the request id still is,
                // and the plugin is answered when the fetch lands. Anything arriving here names a
                // file and is read now.
                let Some(path) = path else {
                    return Err(ApiError::Internal {
                        message: "the clipboard is answered off the loop".to_string(),
                    });
                };
                let got = self
                    .attach(std::path::Path::new(&path))
                    .map_err(|message| ApiError::InvalidArgument { message })?;
                Ok(ApiOk::Attachments { attachments: vec![info_of(&got)] })
            }
            ApiCall::ChatAttachments => Ok(ApiOk::Attachments {
                attachments: self
                    .attached
                    .get(&self.active_session())
                    .map(|v| v.iter().map(info_of).collect())
                    .unwrap_or_default(),
            }),
            ApiCall::ChatDetach { index } => {
                let here = self.active_session();
                let row = self.attached.entry(here).or_default();
                // The newest by default: it is the one you just added, and therefore the one you
                // meant. Out of range is not an error — the row may have gone out with a send
                // between asking and answering, and there is nothing to put right.
                let gone = match index {
                    Some(i) if (i as usize) < row.len() => Some(row.remove(i as usize)),
                    Some(_) => None,
                    None => row.pop(),
                };
                self.refresh_composer();
                Ok(ApiOk::Attachments {
                    attachments: gone.iter().map(info_of).collect(),
                })
            }
            ApiCall::ChatDetachAll => {
                let here = self.active_session();
                let gone = self.attached.remove(&here).unwrap_or_default();
                self.refresh_composer();
                Ok(ApiOk::Attachments { attachments: gone.iter().map(info_of).collect() })
            }
            ApiCall::AgentCancel => {
                self.cancel_turn_here();
                Ok(ApiOk::Unit)
            }
            ApiCall::ExtPoints => Ok(ApiOk::Points { points: self.points() }),
            ApiCall::PluginList => {
                Ok(ApiOk::Plugins { plugins: self.plugin_infos.values().cloned().collect() })
            }
            // A call with an answer is taken over before dispatch, where the request id still is.
            // Arriving here means it came as a notify — nobody is waiting — so it is an exec.
            ApiCall::CmdCall { name, args } => {
                self.editor.exec_command(&name, args);
                self.drain_effects();
                Ok(ApiOk::Unit)
            }
            ApiCall::AgentGetSelection => {
                Ok(ApiOk::Selection { selection: self.agent.selection() })
            }
            ApiCall::AgentSetSelection { selection } => {
                // Another machine's conversation takes the model over there, which is where it is
                // checked: this machine not having a provider says nothing about that one.
                let here = self.active_session();
                if let Some(m) = self.mirror_meta(&here) {
                    self.agent.set_selection(selection.clone());
                    // The options too, when they were chosen off that machine's own catalogue —
                    // which is the only catalogue `^E` reads in a mirror that has one.
                    let knows = self.swarm.peer(&m.node).is_some_and(|p| p.capabilities.catalogue);
                    self.command_mirror(&m, neosh_proto::AgentCommand::SetModel {
                        instance: selection.instance.to_string(),
                        model: selection.model.to_string(),
                        options: if knows { selection.options.clone() } else { Vec::new() },
                    });
                    self.broadcast(PluginEvent::SelectionChanged { selection });
                    self.refresh_status();
                    return Ok(ApiOk::Unit);
                }
                if self.agent.providers().resolve(&selection).is_none() {
                    return Err(ApiError::NotFound {
                        what: format!("no driver serves instance {}", selection.instance),
                    });
                }
                self.select_model(selection);
                Ok(ApiOk::Unit)
            }
            ApiCall::AgentDriverCommands => {
                let here = self.active_session();
                let commands = match self.driver_commands.get(&here) {
                    Some(c) => c.clone(),
                    // Not this conversation's own answer, but the last one given for the same
                    // driver in the same project — which is what its handshake is about to say
                    // again. Being one install-change out of date is the cost; the alternative is a
                    // menu that knows none of the agent's commands until you have already sent
                    // something, which is the state the menu is least useful in.
                    None => self.remembered_commands(),
                };
                Ok(ApiOk::DriverCommands { commands })
            }
            ApiCall::ChatSetDraft { text } => {
                self.set_composer(&text);
                self.refresh_composer();
                Ok(ApiOk::Unit)
            }
            ApiCall::AgentListInstances => Ok(ApiOk::Instances {
                instances: self.agent.providers().usable_instances().cloned().collect(),
            }),
            // ---- updating itself ---------------------------------------
            // On the loop, unlike the check and the download, because the question it has to
            // answer is "is anything running *here*" and `self.rounds` is the only authoritative
            // answer to it. Workspace-wide, not per view: a restart ends turns belonging to
            // terminals the one that asked cannot see.
            ApiCall::UpdateRestart { force } => {
                let store = self.agent.sessions();
                let running: Vec<String> = self
                    .rounds
                    .keys()
                    .filter_map(|id| store.get(id).map(|s| s.label()))
                    .collect();
                drop(store);
                if !running.is_empty() && !force {
                    // `Busy` rather than `Denied`: nothing refused this on principle, and it will
                    // work in a minute. The names are in the message because "something is
                    // running" sends somebody hunting through conversations for which one.
                    return Err(ApiError::Busy {
                        message: format!(
                            "{} still running. Wait for it, interrupt it, or restart anyway.",
                            running.join(", ")
                        ),
                    });
                }
                // The same flag `stop` sets, so the workspace leaves the way it always does —
                // conversations flushed, plugins torn down. Restarting *in place* is not this
                // process's to do: it is the terminal that runs `neosh`, and `neosh` starts a
                // workspace when there is none, which by then is the new binary.
                //
                // Which is why the terminals have to be *told* it was a restart. From a closed
                // socket a workspace that stopped and one that is coming straight back look
                // identical, and a terminal that guesses wrong either exits out of an update
                // half-finished or refuses to let go of a `neosh stop`.
                self.quitting = true;
                self.restarting = true;
                Ok(ApiOk::Unit)
            }
            // ---- plugin state ------------------------------------------
            // Keyed by the caller, which the plugin does not get to choose. Cheap enough to answer
            // on the host loop: reads come from memory after the first touch, and a write is one
            // small file that a panel only produces when you press a key.
            ApiCall::StateGet { key } => {
                Ok(ApiOk::Json { value: self.pstate.get(&plugin.0, &key) })
            }
            ApiCall::StateSet { key, value } => {
                self.pstate.set(&plugin.0, key, value);
                Ok(ApiOk::Unit)
            }
            ApiCall::StateDelete { key } => {
                self.pstate.delete(&plugin.0, &key);
                Ok(ApiOk::Unit)
            }
            // ---- shared variables ---------------------------------------
            // Not keyed by the caller, which is the entire difference from state above: a var
            // describes a project or a conversation, and a second panel that could not see the
            // first one's favourites would be a second panel nobody can switch to.
            ApiCall::VarGet { scope, key } => {
                Ok(ApiOk::Json { value: self.vars.get(&scope, &key) })
            }
            ApiCall::VarSet { scope, key, value } => {
                self.vars.set(&scope, key.clone(), value.clone());
                // Which conversations are blocked on a person already travels this way — it was
                // put on a var precisely because it is a fact about them rather than about
                // whichever panel happened to learn it — so this is where the host learns it too,
                // and no second channel has to be invented for the notification.
                match key.as_str() {
                    VAR_ASKING => self.alert_asking(&value, crate::notify::AlertReason::Question),
                    VAR_PERMITTING => {
                        self.alert_asking(&value, crate::notify::AlertReason::Permission)
                    }
                    _ => {}
                }
                self.broadcast(PluginEvent::VarChanged {
                    scope,
                    key,
                    value: Some(value),
                });
                Ok(ApiOk::Unit)
            }
            ApiCall::VarDelete { scope, key } => {
                self.vars.delete(&scope, &key);
                // Nobody is asking any more. Cleared rather than left, or the next question in a
                // conversation that had one would look like a conversation that already did — and
                // would therefore be the one question that never notified.
                match key.as_str() {
                    VAR_ASKING => self.asking.clear(),
                    VAR_PERMITTING => self.permitting.clear(),
                    _ => {}
                }
                self.broadcast(PluginEvent::VarChanged { scope, key, value: None });
                Ok(ApiOk::Unit)
            }
            ApiCall::VarAll { scope } => Ok(ApiOk::Vars { vars: self.vars.all(&scope) }),
            // ---- where HEAD is -------------------------------------------
            // Three small reads per directory and no subprocess, which is what makes it an answer
            // a panel can ask for on every redraw. See `neosh_vcs::read_head`.
            ApiCall::GitHeads { cwds } => Ok(ApiOk::Heads {
                heads: cwds.iter().map(|c| neosh_vcs::read_head(std::path::Path::new(c))).collect(),
            }),
            // ---- the settings screen's verb ---------------------------------
            // Set first, through the same call anybody else would make, so a wrong type is refused
            // *before* anything is written — a file with a value the registry would reject is a
            // workspace that complains on its next start about something nobody typed. Then
            // written, so the value is still the value tomorrow.
            ApiCall::OptPersist { name, value } => {
                let Some(file) = self.startup.config_file.clone() else {
                    return Err(ApiError::Denied {
                        reason: "this workspace reads no config file (--clean), so there is \
                                 nowhere to save a setting; it is set for this run only"
                            .into(),
                    });
                };
                let set = match &value {
                    Some(v) => ApiCall::OptSet { name: name.clone(), value: v.clone() },
                    None => ApiCall::OptReset { name: name.clone() },
                };
                self.editor.apply(&plugin, set)?;
                self.drain_effects();
                crate::config::write_option(&file, &name, value.as_ref())
                    .map_err(|e| ApiError::Internal { message: e.to_string() })?;
                Ok(ApiOk::Unit)
            }
            // ---- the swarm ----------------------------------------------
            ApiCall::SwarmSelf => Ok(ApiOk::SwarmSelf { node: self.swarm_self() }),
            ApiCall::SwarmNodes => Ok(ApiOk::SwarmNodes {
                nodes: self
                    .swarm
                    .peers()
                    .map(|p| neosh_proto::SwarmNode {
                        // The name a plugin sees is the name the *user* gave it, with the
                        // announced one behind it. Substituted here, once, rather than left for
                        // every panel to remember: a rename that only the computers panel honoured
                        // would be a machine with two names on one screen.
                        info: neosh_proto::NodeInfo {
                            name: p.display_name(),
                            ..p.info.clone()
                        },
                        capabilities: p.capabilities.clone(),
                        up: p.up(),
                        link: p.link.clone(),
                        reason: p.reason.clone(),
                        agents: p.agents.clone(),
                    })
                    .collect(),
            }),
            ApiCall::SwarmAgents => Ok(ApiOk::SwarmAgents {
                agents: self
                    .swarm
                    .agents()
                    .map(|(node, agent)| neosh_proto::SwarmAgent {
                        node: node.clone(),
                        agent: agent.clone(),
                    })
                    .collect(),
            }),
            ApiCall::SwarmHostsOf { project } => Ok(ApiOk::Names {
                names: self.swarm.hosts_of(&project).into_iter().map(str::to_string).collect(),
            }),
            ApiCall::SwarmMirror { node, session, view } => {
                let local = self.open_mirror(&node, &session)?;
                let view = view.unwrap_or_else(|| self.serving());
                let moved = self.in_view(view, |me| {
                    me.arrive_in(&local).map_err(|e| ApiError::NotFound { what: e.to_string() })
                });
                moved?;
                // No `work_in`: the directory is on the other machine, and there is nothing here
                // to read a repository out of.
                self.in_view(view, |me| me.enter_session());
                // Asked every time, found or made: subscribing is what sends the history, and a
                // conversation opened again after a while should be what it is now.
                if let Some(h) = &self.swarm_node {
                    h.send(neosh_swarm::SwarmRequest::Subscribe { node, session });
                }
                let info = self.agent.sessions().get(&local).map(|s| s.info());
                match info {
                    Some(info) => Ok(ApiOk::Session { session: self.named(info) }),
                    None => Err(ApiError::NotFound { what: "that conversation went away".into() }),
                }
            }
            ApiCall::SwarmSubscribe { node, session } => {
                match &self.swarm_node {
                    Some(h) => {
                        h.send(neosh_swarm::SwarmRequest::Subscribe { node, session });
                        Ok(ApiOk::Unit)
                    }
                    None => Err(ApiError::NotFound { what: "the swarm is not running".into() }),
                }
            }
            ApiCall::SwarmUnsubscribe { node, session } => {
                if let Some(h) = &self.swarm_node {
                    h.send(neosh_swarm::SwarmRequest::Unsubscribe { node, session });
                }
                Ok(ApiOk::Unit)
            }
            ApiCall::SwarmStrangers => Ok(ApiOk::SwarmStrangers {
                strangers: self.swarm_strangers.values().cloned().collect(),
            }),
            // ---- the plan ------------------------------------------------
            // Answered from what the store kept, which is what makes this safe to call on every
            // redraw. Nothing here asks a vendor anything except `QuotaRefresh`, and that one
            // answers before its own answer arrives.
            ApiCall::QuotaList => Ok(ApiOk::Quotas { quotas: self.quota.list() }),
            ApiCall::QuotaRefresh { instance } => {
                self.refresh_quota(instance);
                Ok(ApiOk::Unit)
            }
            // A plugin publishing for a provider it registered. Its own instances only: a plugin
            // that could report for `claude-cli` could put any number it liked in the strip, and
            // the strip is a thing people are going to trust before spending an afternoon.
            ApiCall::QuotaReport { snapshot } => {
                if !self.plugin_drivers_own(&plugin, &snapshot.instance) {
                    return Err(ApiError::NotPermitted {
                        capability: format!(
                            "quota.report for {} (plugin {} did not register a driver serving it)",
                            snapshot.instance, plugin.0
                        ),
                    });
                }
                self.take_quota(
                    snapshot.instance.clone(),
                    neosh_proto::QuotaSource::Plugin,
                    neosh_provider::quota::Quota {
                        plan: snapshot.plan,
                        windows: snapshot.windows,
                        credits: snapshot.credits,
                    },
                );
                Ok(ApiOk::Unit)
            }
            ApiCall::QuotaHistory { instance, since, until } => Ok(ApiOk::QuotaHistory {
                samples: self.quota.history(instance.as_ref(), since, until),
            }),
            // `UsageHistory` is not here: it reads thousands of files somebody else wrote, so it
            // goes through `spawn_slow` with git and generation, for the same reason they do.
            ApiCall::SwarmPair { node, name, addr } => {
                let Some(h) = &self.swarm_node else {
                    return Err(ApiError::NotFound { what: "the swarm is not running".into() });
                };
                // Whatever it called itself wins over whatever we were told to call it: a machine
                // knows its own name, and a label typed into a dialog is a guess made before
                // anybody had asked.
                let name = self
                    .swarm_strangers
                    .get(&node)
                    .map(|s| s.info.name.clone())
                    .filter(|n| !n.is_empty())
                    .unwrap_or(name);
                let addr = addr.or_else(|| {
                    self.swarm_strangers.get(&node).and_then(|s| s.addr.clone())
                });
                h.send(neosh_swarm::SwarmRequest::Pair {
                    id: node.clone(),
                    name: name.clone(),
                    addr: addr.clone(),
                });
                // On the board immediately, as "connecting" when we are the one dialling — the
                // moment after pairing is exactly when somebody is watching for it to appear.
                self.swarm.seed(node.clone(), name.clone(), addr.is_some());
                self.swarm_strangers.remove(&node);
                self.editor_message(MessageLevel::Info, format!("paired with {name}"));
                self.broadcast(PluginEvent::SwarmChanged);
                Ok(ApiOk::Unit)
            }
            ApiCall::SwarmUnpair { node } => {
                let Some(h) = &self.swarm_node else {
                    return Err(ApiError::NotFound { what: "the swarm is not running".into() });
                };
                // Refused rather than silently ignored for a machine the config declared: that
                // line would come back on the next reload, and a removal that undoes itself is
                // worse than one that says why it cannot.
                if self
                    .swarm_allowed
                    .get(&node)
                    .is_some_and(|p| p.source == neosh_swarm::Source::Config)
                {
                    return Err(ApiError::Denied {
                        reason: "that machine is in your config file; remove its `[[swarm.peers]]` entry"
                            .into(),
                    });
                }
                h.send(neosh_swarm::SwarmRequest::Unpair { id: node });
                self.broadcast(PluginEvent::SwarmChanged);
                Ok(ApiOk::Unit)
            }
            ApiCall::SwarmRename { node, name } => {
                if self.swarm_node.is_none() {
                    return Err(ApiError::NotFound { what: "the swarm is not running".into() });
                }
                // The store is the record and the board is the picture. Written first, so a rename
                // that the config refuses never reaches the screen at all.
                let renamed = self
                    .swarm_allowed
                    .rename(&node, name.clone())
                    .map_err(|e| ApiError::Denied { reason: e.to_string() })?;
                if !renamed {
                    return Err(ApiError::NotFound { what: format!("no such machine: {}", node.short()) });
                }
                self.swarm.set_alias(&node, name);
                let now = self.swarm.peer(&node).map(|p| p.display_name()).unwrap_or_default();
                self.bridge.broadcast(PluginEvent::SwarmChanged);
                self.editor_message(MessageLevel::Info, format!("that computer is now {now}"));
                Ok(ApiOk::Unit)
            }
            ApiCall::SwarmReconnect { node } => {
                let Some(h) = &self.swarm_node else {
                    return Err(ApiError::NotFound { what: "the swarm is not running".into() });
                };
                h.send(neosh_swarm::SwarmRequest::Redial { id: node.clone() });
                // The board says "connecting" from the keystroke, not from the first failure:
                // the state changed the moment somebody asked, and the events catch up.
                if let Some(name) = self.swarm.peer(&node).map(|p| p.info.name.clone()) {
                    self.swarm.seed(node.clone(), name, true);
                    self.swarm.dial_failed(&node, 0, "dialling…".into());
                }
                self.broadcast(PluginEvent::SwarmChanged);
                Ok(ApiOk::Unit)
            }
            ApiCall::SwarmDisconnect { node } => {
                let Some(h) = &self.swarm_node else {
                    return Err(ApiError::NotFound { what: "the swarm is not running".into() });
                };
                h.send(neosh_swarm::SwarmRequest::Disconnect { id: node });
                Ok(ApiOk::Unit)
            }
            ApiCall::SwarmShell { node, cwd } => {
                if self.swarm_node.is_none() {
                    return Err(ApiError::NotFound { what: "the swarm is not running".into() });
                }
                let peer = self.swarm.peer(&node);
                // Checked here as well as at the far end, and for what a person can *do* about it:
                // "not connected" is fixed from this keyboard and "shells are off over there" is
                // fixed from the other one, and a round trip that cannot happen should not be
                // spent finding that out.
                let Some(peer) = peer else {
                    return Err(ApiError::NotFound { what: format!("no machine {}", node.short()) });
                };
                if !peer.up() {
                    return Err(ApiError::Denied {
                        reason: format!("{} is not connected", peer.display_name()),
                    });
                }
                if !peer.capabilities.shells {
                    return Err(ApiError::Denied {
                        reason: format!(
                            "{} does not open shells for other machines — set `accepts_shells = \
                             true` under `[swarm]` there",
                            peer.display_name()
                        ),
                    });
                }
                // A tab rather than a split, which is what `<C-w>T` does locally: a shell on
                // another computer is a place to work, not a strip beside what you were reading.
                let view = self.view_now();
                self.editor.tab_new(view, None, true);
                self.pending_term = self
                    .editor
                    .active_pane_of(view)
                    .map(|p| (p, ShellAt::There { node, cwd }));
                self.sync_panes();
                self.refresh_status();
                Ok(ApiOk::Unit)
            }
            // Answered off the loop: it opens a socket to somewhere that may not answer.
            ApiCall::SwarmProbe { .. } => {
                Err(ApiError::Internal { message: "swarm.probe is answered asynchronously".into() })
            }
            // Taken over before dispatch, because their answers come from another machine.
            ApiCall::SwarmCommand { .. } => Err(ApiError::Internal {
                message: "swarm.command is answered asynchronously".into(),
            }),
            ApiCall::SwarmBrowse { .. } => Err(ApiError::Internal {
                message: "swarm.browse is answered asynchronously".into(),
            }),
            // ---- events -------------------------------------------------
            // `from` is stamped here rather than taken from the call, so "who said this" is one of
            // the few things in a plugin message that cannot be forged.
            ApiCall::EventEmit { name, data } => {
                self.wake_on_event(&name);
                self.broadcast(PluginEvent::Event {
                    name,
                    data,
                    from: plugin.0.clone(),
                });
                Ok(ApiOk::Unit)
            }
            // ---- sessions ----------------------------------------------
            ApiCall::SessionList { include_archived } => {
                let mut list = self.agent.sessions().list_with(include_archived);
                // Whose screen the answer is about. The store's guess is the most recently arrived
                // in anywhere, which was the same thing when there was one terminal and is a row
                // marked in the wrong panel now: `▸` in this column means "the conversation this
                // window is reading" and every window has its own answer.
                let here = self.active_session();
                // Every pane, not every terminal: `on_screen` means somebody can see it, and a
                // conversation in the right-hand half of a split is as visible as one in a
                // terminal of its own.
                let seen: Vec<_> = self.panes().map(|p| p.session.clone()).collect();
                for info in &mut list {
                    info.is_active = info.id == here;
                    info.on_screen = seen.contains(&info.id);
                }
                Ok(ApiOk::Sessions {
                    sessions: list.into_iter().map(|s| self.named(s)).collect(),
                })
            }
            ApiCall::SessionCurrent => {
                let here = self.active_session();
                let info = {
                    let store = self.agent.sessions();
                    let mut i = store
                        .get(&here)
                        .map(|s| s.info())
                        .ok_or_else(|| ApiError::NotFound { what: format!("session {here}") })?;
                    i.is_active = true;
                    i.on_screen = true;
                    i
                };
                Ok(ApiOk::Session { session: self.named(info) })
            }
            ApiCall::SessionNew { cwd, title, activate } => {
                let cwd = cwd.map(std::path::PathBuf::from).unwrap_or_else(|| self.cwd.clone());
                // Canonicalised so two names for one directory do not become two projects.
                let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
                if !cwd.is_dir() {
                    return Err(ApiError::NotFound { what: format!("directory {}", cwd.display()) });
                }
                self.remember_repo(cwd.clone()).await;
                let mut session = neosh_agent::Session::new(cwd);
                session.title = title.filter(|t| !t.trim().is_empty());
                session.created_at = now_secs();
                session.updated_at = session.created_at;
                // A new conversation inherits the model you are using. Starting it on whatever the
                // catalogue lists first would silently change what you are talking to.
                session.selection = self.agent.selection();
                session.system = self.agent.session().system.clone();
                let id = session.id.clone();

                self.agent.sessions().insert(session);
                if activate {
                    self.arrive_in(&id).map_err(|e| ApiError::NotFound { what: e.to_string() })?;
                }
                let info = {
                    let store = self.agent.sessions();
                    // The session was just inserted, so this cannot miss; the `?` is here rather
                    // than an unwrap because a future refactor could make it miss.
                    let mut i = store
                        .get(&id)
                        .map(|s| s.info())
                        .ok_or_else(|| ApiError::Internal { message: "session vanished".into() })?;
                    i.is_active = self.v().session == id;
                    i.on_screen = self.panes().any(|p| p.session == id);
                    i
                };
                let info = self.named(info);
                if activate {
                    // Before the redraw: the banner names the directory, and the file tools have
                    // to be pointed at it before the first turn rather than after it.
                    let here = self.session_cwd(&id);
                    self.work_in(here).await;
                    self.enter_session();
                }
                self.persist_sessions();
                Ok(ApiOk::Session { session: info })
            }
            // Switching is never refused, including while a turn is running. A turn belongs to its
            // conversation: it keeps streaming into that one's transcript, and what you see is
            // rebuilt from the conversation you switched to. Waiting for a model to finish before
            // you may look at something else is the workspace refusing to be a workspace.
            ApiCall::SessionSwitch { session, view } => {
                // A conversation past the restore cap is a file until somebody asks for it, and
                // opening one from the archive is exactly that ask. Before the view moves, so the
                // terminal arrives somewhere that is in the store.
                self.hydrate(&session);
                // In the terminal that asked, unless the caller named another. `^T` in one window
                // must not move the window next door — which is the whole of what independent
                // views are — and an orchestrator steering somebody else's screen says so.
                let view = view.unwrap_or_else(|| self.serving());
                let moved = self.in_view(view, |me| {
                    me.arrive_in(&session)
                        .map_err(|e| ApiError::NotFound { what: e.to_string() })
                });
                moved?;
                let cwd = self.session_cwd(&session);
                self.work_in(cwd).await;
                self.in_view(view, |me| me.enter_session());
                Ok(ApiOk::Unit)
            }
            ApiCall::ViewList => {
                let here = self.serving();
                Ok(ApiOk::Views {
                    views: self
                        .views
                        .iter()
                        // Not the one being kept for whoever attaches next. Nobody is at it, and a
                        // plugin that furnished a panel into it would have that panel rehomed on
                        // top of the one it opens when a terminal really does arrive.
                        .filter(|(id, _)| Some(**id) != self.orphan)
                        .map(|(id, v)| neosh_proto::ViewInfo {
                            view: *id,
                            // The conversation this terminal's keyboard is in. A split shows
                            // several at once and this field can name one, so it names the one a
                            // key pressed here would go to — which is what every caller of it
                            // means by "the conversation in that terminal".
                            session: self
                                .editor
                                .active_pane_of(*id)
                                .and_then(|p| v.panes.get(&p))
                                .or_else(|| v.panes.values().next())
                                .map(|p| p.session.clone())
                                .unwrap_or_default(),
                            current: *id == here,
                        })
                        .collect(),
                })
            }
            // Deleting is that machine's to do, at that machine: a conversation there is its files,
            // its history and very possibly somebody's afternoon, and nothing over the swarm is
            // allowed to destroy what it cannot put back. Archiving is, and says so.
            ApiCall::SessionClose { session } if self.mirror_meta(&session).is_some() => {
                let machine =
                    self.mirror_meta(&session).map(|m| m.machine).unwrap_or_default();
                Err(ApiError::Denied {
                    reason: format!(
                        "that conversation is on {machine} — archive it from here, or delete it there"
                    ),
                })
            }
            ApiCall::SessionClose { session } => {
                // A conversation this workspace never loaded is still a conversation.
                //
                // `RESTORE_LIMIT` means the store is a window onto the directory rather than the
                // whole of it, so past a few hundred conversations `close` on a perfectly real one
                // answered "no such session" — and the archive, which is where anybody would go to
                // deal with exactly those, had no verb that could touch them. One delete, whether
                // the store is holding it or only the disk is. Everything below this line is about
                // a *loaded* conversation: a turn to cancel, a driver to close, a terminal to step
                // out of. A file has none of that, and no view can be sitting in it.
                let known = { self.agent.sessions().get(&session).is_some() };
                if !known {
                    let Some(dir) = self.state_dir.clone() else {
                        return Err(ApiError::NotFound { what: format!("session {}", session.0) });
                    };
                    if !crate::sessions::stored(&dir, &session) {
                        return Err(ApiError::NotFound { what: format!("session {}", session.0) });
                    }
                    crate::sessions::forget(&dir, &session);
                    self.vars.forget_session(&session);
                    return Ok(ApiOk::Unit);
                }
                // Closing the conversation you are in has to land you somewhere first, because
                // afterwards this terminal would name something that is not in the store. Same
                // line as the archive path below and for the same reason: being unable to delete
                // the conversation you have finished with, because it is the only one, is not a
                // rule anybody wanted.
                let closing_active = self.step_out_of(&session);
                // Closing is the one case that does stop a turn: there is about to be nowhere to
                // put its answer, and a stream writing into a conversation that no longer exists is
                // a subprocess nobody can reach.
                // Removed rather than marked: the conversation itself is going, so there is
                // nothing left for its ending to be about and nothing that can start another turn
                // in it. `TurnEnded` arriving later finds no entry, which is the right no-op.
                if let Some(t) = self.turns.remove(&session) {
                    t.cancel.cancel();
                }
                self.rounds.remove(&session);
                // Nothing left to hear out: the transcript this would have been drawn into is about
                // to be deleted, and the CLI holding it is closed a few lines below.
                self.unheard.remove(&session);
                self.driver_commands.remove(&session);
                // A vendor CLI held open for this conversation has nothing left to answer. Left
                // alone it would sit there for the rest of the program, holding a conversation
                // whose transcript is about to be deleted from disk.
                for d in &self.agent_drivers {
                    d.close_conversation(&session);
                }
                self.agent.sessions().remove(&session).map_err(|e| match e {
                    neosh_agent::store::StoreError::LastSession => {
                        ApiError::InvalidArgument { message: e.to_string() }
                    }
                    other => ApiError::NotFound { what: other.to_string() },
                })?;
                if let Some(dir) = &self.state_dir {
                    crate::sessions::forget(dir, &session);
                }
                // Whatever anybody tagged this conversation with goes too. Left behind it is a row
                // in `vars.json` per conversation ever deleted, and a future id collision inherits
                // somebody's stale colour.
                self.vars.forget_session(&session);
                // And its bar comes with you. A deleted conversation is a group nothing can ever
                // show again, so a shell you had open in a second tab of it would keep running
                // where no key could reach it — a process with no way back, which is the one thing
                // hiding a tab must never turn into. Deleting is the irreversible one; archiving
                // deliberately does not do this, because going back to the conversation is what
                // brings its tabs back with it.
                self.adopt_tabs_of(&session);
                if closing_active {
                    // Where you landed is a conversation of its own, and it may be in another
                    // directory: the banner names one, the file tools are pointed at one, and
                    // both were still the deleted conversation's until this said otherwise.
                    let here = self.session_cwd(&self.active_session());
                    self.work_in(here).await;
                    self.enter_session();
                }
                self.persist_sessions();
                Ok(ApiOk::Unit)
            }
            // Archiving does not stop a turn. Putting a conversation away is about the list you
            // read, not about the work: it keeps its messages, and it keeps whatever it was in the
            // middle of saying.
            ApiCall::SessionArchive { session, archived } if self.mirror_meta(&session).is_some() => {
                let Some(m) = self.mirror_meta(&session) else { return Ok(ApiOk::Unit) };
                self.command_mirror(&m, neosh_proto::AgentCommand::Archive { archived });
                // Put away over there, so there is nothing left to look at here: the same step out
                // archiving one of yours takes, and the mirror is let go once no pane shows it.
                if archived && self.step_out_of(&session) {
                    let here = self.session_cwd(&self.active_session());
                    self.work_in(here).await;
                    self.enter_session();
                }
                Ok(ApiOk::Unit)
            }
            ApiCall::SessionArchive { session, archived } => {
                // Putting one *back* is the commonest thing done to a conversation the store has
                // never held: everything past the cap is, by definition, something nobody has
                // opened lately.
                self.hydrate(&session);
                // Archiving the only conversation you have is a reasonable thing to want, and
                // "there is nowhere to go" is a bad answer to it. Somewhere to go is one line.
                let leaving = archived && self.step_out_of(&session);
                let at = now_secs();
                self.agent.sessions().archive(&session, archived, at).map_err(|e| match e {
                    neosh_agent::store::StoreError::LastSession => {
                        ApiError::InvalidArgument { message: e.to_string() }
                    }
                    other => ApiError::NotFound { what: other.to_string() },
                })?;
                if leaving {
                    let here = self.session_cwd(&self.active_session());
                    self.work_in(here).await;
                    self.enter_session();
                }
                self.persist_sessions();
                Ok(ApiOk::Unit)
            }
            // ---- credentials -------------------------------------------
            // The list says where each key comes from and never what it is; `ProviderSetCredential`
            // is answered elsewhere, because the host has to read the keyboard before it can reply.
            ApiCall::ProviderCredentials => {
                Ok(ApiOk::Credentials { credentials: self.agent.providers().credentials() })
            }
            ApiCall::ProviderForgetCredential { instance } => {
                neosh_provider::credentials::credentials().forget(&instance);
                self.editor_message(MessageLevel::Info, format!("forgot the key for {instance}"));
                Ok(ApiOk::Unit)
            }
            ApiCall::ProviderSetCredential { .. } => Err(ApiError::Internal {
                message: "credential prompts are answered by the host loop".into(),
            }),
            // On the host loop rather than through `spawn_slow`, and these two are the only git
            // writes that are. `git branch -m` is one ref write — the same order of cost as the
            // `rev-parse` `remember_repo` already awaits here — and what follows it cannot be done
            // from a spawned task: the branch is half of what [`ProjectFacts`] cached the first
            // time this directory was seen, so a rename that did not reach the host would leave
            // every project list drawing the old name until the workspace restarted. The republish
            // rule, from the other end: a value the host *kept* has to be put right by whatever
            // changed it.
            ApiCall::GitRenameBranch { old, new, cwd } => {
                let svc = self.services(plugin);
                svc.git_rename_branch(old.clone(), new, cwd.clone()).await?;
                self.rebranded(&old, cwd.as_deref()).await;
                Ok(ApiOk::Unit)
            }
            // The other one, and here for a stronger version of the same reason: a branch name is
            // one field the host cached, and a directory is the *key* of four maps, the
            // conversations in it and a file on disk. See [`Self::relocated`], which is the half of
            // this call that could not be done from a spawned task.
            //
            // `git worktree move` is a rename — it refuses a cross-device move rather than copying
            // — so there is no slow case here to keep off the loop.
            ApiCall::GitMoveWorktree { path, dest, cwd } => {
                let from = std::path::PathBuf::from(&path);
                let to = std::path::PathBuf::from(&dest);
                // Refused rather than survived. A vendor CLI holds its working directory from the
                // moment it is spawned; renaming a tree an agent is mid-turn in can land the file
                // it is writing in the old place, with nothing on screen to say it went there. One
                // `<Esc>` is a much better price than a silently misplaced edit, and the message
                // says which conversation to press it in.
                // The ids first and the store second. `sessions()` is a plain `Mutex` guard and a
                // `std::sync::Mutex` is not reentrant, so asking `sessions_under` while already
                // holding one is not a borrow error, it is a workspace that stops answering.
                let under = self.sessions_under(&from);
                let busy: Vec<String> = {
                    let store = self.agent.sessions();
                    under
                        .iter()
                        .filter_map(|id| store.get(id))
                        .filter(|s| s.active_turn.is_some())
                        .map(|s| {
                            s.title.clone().unwrap_or_else(|| {
                                s.cwd
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| s.id.0.clone())
                            })
                        })
                        .collect()
                };
                if !busy.is_empty() {
                    return Err(ApiError::InvalidArgument {
                        message: format!(
                            "a turn is running in this worktree — interrupt it with <Esc> and \
                             try again ({})",
                            busy.join(", "),
                        ),
                    });
                }
                let svc = self.services(plugin);
                svc.git_move_worktree(path, dest, cwd).await?;
                self.relocated(&from, &to).await;
                Ok(ApiOk::Unit)
            }
            ApiCall::SessionRename { session, title } => {
                // Named there, because that is where it is listed and kept; the copy here follows
                // at once so the footer does not wait for the roster.
                if let Some(m) = self.mirror_meta(&session) {
                    let _ = self.agent.sessions().rename(&session, title.clone());
                    self.command_mirror(&m, neosh_proto::AgentCommand::Rename { title });
                    self.refresh_status();
                    return Ok(ApiOk::Unit);
                }
                self.agent
                    .sessions()
                    .rename(&session, title)
                    .map_err(|e| ApiError::NotFound { what: e.to_string() })?;
                self.refresh_status();
                self.persist_sessions();
                Ok(ApiOk::Unit)
            }
            ApiCall::SessionMessages { session } => {
                // Reading one does not move the workspace, but it does need the conversation in
                // hand — and an archive that can list something it cannot read is one whose export
                // and preview verbs fail on exactly the oldest rows.
                if let Some(id) = session.clone() {
                    self.hydrate(&id);
                }
                let here = self.active_session();
                let store = self.agent.sessions();
                let target = store.get(session.as_ref().unwrap_or(&here));
                match target {
                    Some(s) => Ok(ApiOk::Messages { messages: s.messages.clone() }),
                    None => Err(ApiError::NotFound {
                        what: format!("session {}", session.map(|s| s.0).unwrap_or_default()),
                    }),
                }
            }

            ApiCall::PermissionGetMode => {
                Ok(ApiOk::PermissionMode { mode: self.agent.permission_mode(&self.active_session()) })
            }
            ApiCall::PermissionSetMode { mode } if self.mirror_meta(&self.active_session()).is_some() => {
                // Refused *before* the footer changes, rather than by the far end afterwards: a
                // mode is every future prompt answered in advance, and one that said "full
                // access" here while that machine went on asking would be the footer lying about
                // the one thing it exists to say.
                let here = self.active_session();
                let Some(m) = self.mirror_meta(&here) else { return Ok(ApiOk::Unit) };
                if !self.swarm.peer(&m.node).is_some_and(|p| p.capabilities.accepts_approvals) {
                    return Err(ApiError::Denied {
                        reason: format!(
                            "{} decides its own permissions — set `accepts_approvals = true` \
                             under `[swarm]` there to change them from here",
                            m.machine
                        ),
                    });
                }
                self.agent.set_permission_mode(&here, mode);
                self.command_mirror(&m, neosh_proto::AgentCommand::SetMode { mode });
                self.refresh_status();
                Ok(ApiOk::PermissionMode { mode })
            }
            ApiCall::PermissionSetMode { mode } => {
                // The conversation's, and saved with it. A mode is a property of what you are
                // working on — a scratch question in someone else's checkout and the branch you
                // have been on all week are not the same risk — and one that reset every time you
                // reopened the workspace would be a setting you had to re-make every morning.
                self.set_permission_mode(mode);
                Ok(ApiOk::PermissionMode { mode })
            }
            // News from a plugin. Declared, because it can interrupt somebody who is not looking
            // at neosh — see `PluginPermission::Notify`. Whether it *does* interrupt
            // them is still the host's decision and not the caller's: a plugin cannot know which
            // conversation is on screen or whether a terminal has focus.
            ApiCall::Alert { level, title, message, session } => {
                if !self
                    .plugin_permissions
                    .get(plugin)
                    .is_some_and(|p| p.contains(&neosh_proto::PluginPermission::Notify))
                {
                    return Err(ApiError::NotPermitted {
                        capability: "notify — add it to plugin.toml. `neosh.notify` draws in the \
                                     corner and needs nothing."
                            .into(),
                    });
                }
                // A conversation the user is looking at is one they can see already, whoever
                // raised it. The plugin does not get to override that, because the whole reason
                // this goes through the host is that the plugin cannot know.
                if session.as_ref().is_some_and(|s| self.can_see(s)) {
                    self.editor_message(level, message);
                    return Ok(ApiOk::Unit);
                }
                self.alert(
                    crate::notify::AlertReason::Plugin,
                    session.as_ref(),
                    level,
                    title,
                    message,
                );
                Ok(ApiOk::Unit)
            }
            ApiCall::HintSet { key, hint } => {
                self.hints.insert(format!("{plugin}:{key}"), hint);
                self.refresh_composer();
                Ok(ApiOk::Unit)
            }
            ApiCall::HintClear { key } => {
                self.hints.remove(&format!("{plugin}:{key}"));
                self.refresh_composer();
                Ok(ApiOk::Unit)
            }

            ApiCall::StatusSet { key, segment } => {
                // Namespaced by plugin, so two plugins choosing the same key cannot fight and
                // unloading one takes only its own segments.
                self.status_segments.insert(format!("{plugin}:{key}"), segment);
                self.refresh_status();
                Ok(ApiOk::Unit)
            }
            ApiCall::StatusClear { key } => {
                self.status_segments.remove(&format!("{plugin}:{key}"));
                self.refresh_status();
                Ok(ApiOk::Unit)
            }

            ApiCall::ToolRegister { def } => {
                self.agent.tools_mut().register_plugin(plugin, def)?;
                Ok(ApiOk::Unit)
            }
            ApiCall::ToolUnregister { name } => {
                self.agent.tools_mut().unregister(plugin, &name);
                Ok(ApiOk::Unit)
            }
            ApiCall::ToolList => Ok(ApiOk::Tools { tools: self.agent.tools().defs() }),
            ApiCall::HookRegister { hook, blocking, timeout_ms } => {
                self.agent.hooks_mut().register(hook, plugin, blocking, timeout_ms);
                Ok(ApiOk::Unit)
            }
            ApiCall::HookUnregister { hook } => {
                self.agent.hooks_mut().unregister(hook, plugin);
                Ok(ApiOk::Unit)
            }
            ApiCall::ProviderRegisterDriver { driver, instances, agent_loop } => {
                let provider = Arc::new(PluginProvider::new(
                    driver.clone(),
                    plugin.clone(),
                    self.bridge.clone(),
                    agent_loop,
                ));
                {
                    let mut reg = self.agent.providers_mut();
                    reg.register_driver(provider);
                    for i in instances {
                        reg.add_instance(i);
                    }
                }
                // Remembered so unloading the plugin takes its driver with it. A driver whose
                // plugin is gone answers every request with a dead channel.
                self.plugin_drivers.entry(plugin.clone()).or_default().push(driver);
                // An `agent.model` held for this instance is applied *now*, not when the last
                // plugin finishes loading. The two are not the same moment: a plugin that has
                // registered its provider is a plugin whose model can be talked to, and every
                // other plugin still starting up is beside the point. Between them the selection
                // is the fallback — so a turn sent in that gap was answered by a model the user
                // did not choose, and nothing on screen said so.
                self.apply_pending_model_if_servable();
                Ok(ApiOk::Unit)
            }
            ApiCall::ProviderEmit { stream, event } => {
                self.bridge.emit_provider(&stream, event);
                Ok(ApiOk::Unit)
            }

            ApiCall::RtpAdd { path } => {
                let expanded =
                    std::path::PathBuf::from(shellexpand::tilde(&path).into_owned());
                if self.startup.discovered {
                    // Silently ignoring this would leave the user with a plugin that never loads
                    // and no reason why.
                    return Err(ApiError::InvalidArgument {
                        message: format!(
                            "rtp.add({path:?}) came after plugin discovery; call it from your \
                             config, which runs first"
                        ),
                    });
                }
                if !self.startup.plugin_dirs.contains(&expanded) {
                    self.startup.plugin_dirs.push(expanded);
                }
                Ok(ApiOk::Unit)
            }
            ApiCall::RtpList => Ok(ApiOk::Paths {
                paths: self
                    .startup
                    .plugin_dirs
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect(),
                denied: None,
            }),
            other => Err(ApiError::Internal {
                message: format!("unroutable call: {other:?}"),
            }),
        }
    }

    /// The conversation on screen *in the terminal being served right now*.
    ///
    /// Cloned rather than borrowed: everything that asks this goes on to touch the store again,
    /// and holding the lock across that is how a deadlock is written — which is also why this no
    /// longer reads the store at all. It is a fact about a view.
    fn active_session(&self) -> neosh_proto::SessionId {
        self.v().session.clone()
    }

    // ---- which terminal ---------------------------------------------------

    /// The terminal being served right now.
    ///
    /// `self.from` is set for the whole of handling one input event, and cleared to
    /// [`crate::clients::Source::LOCAL`] between them — so anything the host does on nobody's
    /// behalf (a timer, a signal, an agent event) has to say which terminal it means, which is
    /// what [`Self::in_view`] is for. The fallback exists because a view can go away between an
    /// event being queued and being handled, and a panic in the run loop takes the workspace with
    /// it.
    /// The pane the keyboard is in, in the terminal being served.
    ///
    /// Still spelled `v()` at close to two hundred call sites, and deliberately so: every one of
    /// them means *where the person is*, which was the view when a terminal had one conversation
    /// and is the pane now that it can have four. Renaming them all to `p()` would have been two
    /// hundred lines of diff saying nothing, and the one that got missed would be a call that
    /// silently read the wrong pane.
    fn v(&self) -> &PaneState {
        let view = self.view_now();
        let pane = self.pane_now(view);
        self.views
            .get(&view)
            .and_then(|v| v.panes.get(&pane).or_else(|| v.panes.values().next()))
            .expect("a workspace always keeps at least one pane")
    }

    fn vm(&mut self) -> &mut PaneState {
        let view = self.view_now();
        let pane = self.pane_now(view);
        let pane = match self.views.get(&view).is_some_and(|v| v.panes.contains_key(&pane)) {
            true => pane,
            false => self
                .views
                .get(&view)
                .and_then(|v| v.panes.keys().next().copied())
                .expect("a workspace always keeps at least one pane"),
        };
        self.views
            .get_mut(&view)
            .and_then(|v| v.panes.get_mut(&pane))
            .expect("just looked it up")
    }

    /// Which pane `v()` means: the one being written into if something said so, otherwise the one
    /// with the keyboard.
    ///
    /// The override exists because writing and typing are different questions — see
    /// [`Self::in_pane`]. Only honoured when the pane is actually in this terminal, so a stale
    /// override cannot make `v()` answer with a transcript from another screen.
    fn pane_now(&self, view: neosh_proto::ViewId) -> neosh_proto::PaneId {
        self.acting_pane
            .filter(|p| self.views.get(&view).is_some_and(|v| v.panes.contains_key(p)))
            .or_else(|| self.editor.active_pane_of(view))
            .or_else(|| self.views.get(&view).and_then(|v| v.panes.keys().next().copied()))
            // A pane the editor knows about that this has no state for cannot happen from the
            // keyboard — panes and their state are made together — but it can from a plugin
            // splitting, in the frame between the tree changing and the host furnishing it.
            .unwrap_or_default()
    }

    /// The terminal being served, or any terminal if that one has gone.
    fn view_now(&self) -> neosh_proto::ViewId {
        match self.views.contains_key(&self.from.view) {
            true => self.from.view,
            false => *self.views.keys().next().expect("a workspace always keeps at least one view"),
        }
    }

    /// The screen the keyboard is on — its status line, its panes, its quit arming.
    fn vs(&self) -> &ViewState {
        let view = self.view_now();
        self.views.get(&view).expect("a workspace always keeps at least one view")
    }

    fn vsm(&mut self) -> &mut ViewState {
        let view = self.view_now();
        self.views.get_mut(&view).expect("a workspace always keeps at least one view")
    }

    /// Every pane of every terminal, which is what "on screen anywhere" now means.
    ///
    /// The counterpart to `views.values()` before splitting. A conversation is being looked at if
    /// *a pane* is showing it, and a terminal with a chat in each half is two answers rather than
    /// one — which decides unread marks, where a question opens, and whether a notification leaves
    /// the terminal at all.
    fn panes(&self) -> impl Iterator<Item = &PaneState> {
        self.views.values().flat_map(|v| v.panes.values())
    }

    /// Which terminal a plugin's call is about.
    ///
    /// A plugin is not a keyboard, so nothing about the call itself says where it should land, and
    /// with several terminals "the screen" is not an answer. Three rules, in order, and the first
    /// two are exact:
    ///
    /// 1. **A window names its own view.** Anything anchored to one — a float over a panel — goes
    ///    where that panel is, and a plugin tidying up after a terminal that has gone is the
    ///    ordinary case rather than a mistake.
    /// 2. **A buffer shown in exactly one terminal names it.** A picker put over the sidebar's
    ///    buffer belongs to the sidebar somebody opened.
    /// 3. **Otherwise, the terminal whose input is being served.** For a command run from a key
    ///    that is exact; for a plugin acting on its own — a timer, an agent event it subscribed to
    ///    — it is the last terminal to have pressed anything, which is a guess, and the reason
    ///    [`neosh_proto::KeyContext::view`] exists is so a plugin never has to rely on it.
    fn view_for(&self, call: &ApiCall) -> neosh_proto::ViewId {
        // Said outright beats every rule below it. This is the escape hatch a plugin reaches for
        // when it means a terminal other than the one it is being run from.
        if let ApiCall::WinOpen { view: Some(view), .. }
        | ApiCall::FloatOpen { view: Some(view), .. } = call
        {
            return *view;
        }
        let anchored = match call {
            ApiCall::FloatOpen { config: neosh_proto::FloatConfig { anchor, .. }, .. } => {
                match anchor {
                    neosh_proto::Anchor::Window { win } => self.editor.view_of(*win),
                    _ => None,
                }
            }
            _ => None,
        };
        let by_buffer = || {
            let buf = match call {
                ApiCall::WinOpen { buf, .. } | ApiCall::FloatOpen { buf, .. } => *buf,
                _ => return None,
            };
            let mut showing = self.editor.windows().filter(|w| w.buf == buf);
            match (showing.next(), showing.next()) {
                (Some(w), None) => Some(w.view),
                _ => None,
            }
        };
        anchored.or_else(by_buffer).unwrap_or_else(|| self.serving())
    }

    /// The terminal being served, or the one that spoke most recently when nobody is.
    ///
    /// Never a view that has gone. A window opened into one is a window nothing can draw and
    /// nothing can close — the panel is simply missing, and the buffer behind it looks perfectly
    /// healthy to anything that inspects buffers rather than screens.
    fn serving(&self) -> neosh_proto::ViewId {
        for id in [self.from.view, self.last_input] {
            if self.views.contains_key(&id) {
                return id;
            }
        }
        self.views.keys().next_back().copied().unwrap_or(neosh_proto::ViewId::LOCAL)
    }

    /// Every terminal, in a stable order.
    fn view_ids(&self) -> Vec<neosh_proto::ViewId> {
        self.views.keys().copied().collect()
    }

    /// Give a terminal that has just arrived somewhere to be.
    ///
    /// **The most recent conversation nobody else is looking at.** A second terminal is opened
    /// because the first one is busy, so putting it in the same conversation would make it a copy
    /// of the thing you were trying to get away from. When every conversation is already on
    /// somebody's screen — or there is only one — it lands in the most recent, and two views of
    /// one conversation is a perfectly good thing to be: same transcript, own scroll, own draft.
    ///
    /// `want` overrides all of that, and is how `neosh "a prompt"` lands you in the conversation
    /// it just started rather than wherever the rule above would have put you.
    fn open_view(&mut self, view: neosh_proto::ViewId, want: Option<neosh_proto::SessionId>) {
        // Only one this workspace actually has. A conversation deleted between the client asking
        // and the client arriving is not an error worth refusing an attach over.
        let want = want.filter(|id| self.agent.sessions().get(id).is_some());
        // A workspace nobody was watching kept the screen of whoever left last, mid-answer and
        // all. This is the terminal coming back to it: it gets that screen rather than a
        // rebuilt-from-messages copy, which would be missing everything the running turn has said
        // and not yet committed.
        if let Some(old) = self.orphan.take() {
            self.editor.rehome(old, view);
            if let Some(state) = self.views.remove(&old) {
                self.views.insert(view, state);
            }
            // The kept screen is somebody else's conversation, and this terminal asked for one of
            // its own. Moved rather than left, or `neosh "a prompt"` against a workspace that has
            // been sitting detached shows you last week's transcript with a turn running out of
            // sight.
            if let Some(want) = want
                && self.in_view(view, |me| me.arrive_in(&want).is_ok())
            {
                self.in_view(view, |me| me.enter_session());
            }
            return;
        }
        let taken: Vec<_> = self.panes().map(|p| p.session.clone()).collect();
        let session = want.unwrap_or_else(|| {
            let store = self.agent.sessions();
            store
                .list()
                .into_iter()
                .map(|s| s.id)
                .find(|id| !taken.contains(id))
                .or_else(|| taken.first().cloned())
                .unwrap_or_else(|| store.current_id().clone())
        });
        let screen = ViewState::furnish(&mut self.editor, view, session.clone());
        self.views.insert(view, screen);
        // Drawn as an arrival rather than left empty: the transcript is rebuilt from the
        // conversation's messages, the mark is cleared, and the composer says what `\u{23ce}` does.
        self.in_view(view, |me| {
            me.enter_session();
            me.refresh_status();
            me.refresh_composer();
        });
    }

    /// Take down a terminal's screen when its last client goes.
    ///
    /// Its windows close with it — a window belongs to exactly one view, so nothing else is
    /// looking at them — and its buffers go too, because a transcript is per view. The
    /// conversation is untouched: what the agent produced is the workspace's, and the turn in it
    /// keeps running whether or not anybody is watching.
    ///
    /// Never the last one. A workspace with no views is a workspace where a turn ending has
    /// nowhere to draw and nothing to remember about where you were, so the view of whoever left
    /// last is kept and the next terminal to attach is given it.
    fn close_view(&mut self, view: neosh_proto::ViewId) {
        if !self.views.contains_key(&view) {
            return;
        }
        if self.views.len() <= 1 {
            // Kept for whoever attaches next, but no longer a screen anybody is at: it is out of
            // `view.list` from here, so a plugin loading meanwhile does not furnish a panel into a
            // terminal that does not exist.
            //
            // The host's own chrome stays — that is what coming back to where you were means — and
            // everything a plugin put there is closed now rather than left for the plugin to close
            // when it hears about this. A panel closed asynchronously is a panel still on screen
            // when the next terminal arrives and its replacement opens beside it.
            let chrome = [self.v().chat, self.v().composer, self.vs().status];
            self.editor.close_others_in(view, &chrome);
            self.orphan = Some(view);
        } else {
            self.editor.close_view(view);
            self.views.remove(&view);
            self.orphan = self.orphan.filter(|o| *o != view);
        }
        // So a plugin can let go of whatever it was keeping about that panel.
        self.broadcast(neosh_proto::PluginEvent::ViewClosed { view });
    }

    /// Do something as though the key had been pressed in `view`.
    ///
    /// The mechanism for everything the host does that is not a key press: a turn producing
    /// output, a clock ticking, a plugin's call. Each of those is about some set of terminals and
    /// has to say which, and swapping `from` for the duration means the several dozen places that
    /// already ask "where am I" keep working unchanged and are correct for whichever one is being
    /// drawn.
    fn in_view<R>(&mut self, view: neosh_proto::ViewId, f: impl FnOnce(&mut Self) -> R) -> R {
        let was = self.from;
        // A view switch is not a pane switch: an override left standing would name a pane in the
        // terminal we have just left, and `v()` would answer with a transcript from another screen.
        let pane = self.acting_pane.take();
        self.from.view = view;
        let out = f(self);
        self.from = was;
        self.acting_pane = pane;
        out
    }

    /// Put this terminal in a conversation. What *moves*; [`Self::enter_session`] is what draws.
    ///
    /// The conversation being left is named rather than worked out by the store, because the store
    /// has no idea how many terminals there are: leaving a placeholder is what drops it, and a
    /// placeholder somebody else is still sitting in is not one anybody left.
    ///
    /// **And the tab comes with you.** A tab belongs to the conversation in it — see
    /// [`neosh_proto::TabInfo::group`] — so switching is what moves this tab onto the new
    /// conversation's bar, beside whatever was already open there and away from the shell you left
    /// running in the old one. Guarded on the conversation actually changing, because this is also
    /// the call [`Self::rehome`] makes when the keyboard merely moves *between* panes: a tab split
    /// across two conversations would otherwise change which bar it is on every time you crossed
    /// the boundary, and the strip would flicker between two conversations under `<C-w>l`.
    fn arrive_in(
        &mut self,
        session: &neosh_proto::SessionId,
    ) -> Result<(), neosh_agent::StoreError> {
        let leaving = self.abandoning(session);
        let switching = self.v().session != *session;
        // Another machine's conversation nobody is looking at any more stops being streamed here.
        let left_mirror = leaving
            .as_ref()
            .and_then(|l| self.agent.sessions().get(l).and_then(|s| s.mirror.clone()));
        self.agent.sessions().enter(session, leaving.as_ref())?;
        if let Some(m) = left_mirror {
            self.let_go_of_mirror(&m);
        }
        self.vm().session = session.clone();
        if switching {
            self.file_tab(session.clone());
        }
        Ok(())
    }

    /// Put the tab this pane is in on a conversation's bar.
    ///
    /// The tab *containing the acting pane*, which is not always the one the terminal is showing:
    /// an orchestrator steering another view, or a call that named a pane, is arriving somewhere
    /// that may be a tab along. Filing the tab on screen instead would leave the pane that actually
    /// moved on a bar about a conversation it is no longer in.
    fn file_tab(&mut self, group: neosh_proto::SessionId) {
        let view = self.view_now();
        let Some(pane) = self.acting_pane.or_else(|| self.editor.active_pane_of(view)) else {
            return;
        };
        let Some(tab) =
            self.editor.tabs_of(view).iter().find(|t| t.root.contains(pane)).map(|t| t.id)
        else {
            return;
        };
        self.editor.tab_set_group(view, tab, Some(group.0));
    }

    /// The conversation this pane is leaving, if nothing else is showing it.
    ///
    /// Counted in **panes**, not terminals. A scratch conversation is dropped when the last thing
    /// looking at it stops — and with two panes side by side, "another window has it" and "another
    /// terminal has it" stopped being the same question. Asked per terminal, moving from the left
    /// half to the right half reported the left half's conversation as abandoned while it was still
    /// on screen a column away.
    fn abandoning(
        &self,
        going_to: &neosh_proto::SessionId,
    ) -> Option<neosh_proto::SessionId> {
        let leaving = self.v().session.clone();
        if leaving == *going_to {
            return None;
        }
        let here = self.pane_now(self.view_now());
        let elsewhere = self
            .panes_showing(&leaving)
            .into_iter()
            .any(|(v, p)| v != self.from.view || p != here);
        (!elsewhere).then_some(leaving)
    }

    /// Where a conversation is, without holding the store's lock across anything else.
    fn session_cwd(&self, session: &neosh_proto::SessionId) -> std::path::PathBuf {
        let store = self.agent.sessions();
        store.get(session).map(|s| s.cwd.clone()).unwrap_or_else(|| self.cwd.clone())
    }

    /// Do something as though the key had been pressed in a particular pane.
    ///
    /// [`Self::in_view`] one level down, and for exactly the same reason: a turn's output belongs
    /// to the pane reading that conversation, which is very often not the pane the keyboard is in.
    /// Two chats side by side means the answer arriving in the right-hand one must be written into
    /// the right-hand one's transcript while you are typing into the left.
    ///
    /// Overriding rather than *moving* focus is the whole point. Focus is where the person is;
    /// this is where the writing goes, and a turn finishing must not take the keyboard away from
    /// the pane somebody is mid-sentence in.
    fn in_pane<R>(
        &mut self,
        view: neosh_proto::ViewId,
        pane: neosh_proto::PaneId,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let was = self.from;
        let prev = self.acting_pane.replace(pane);
        self.from.view = view;
        let out = f(self);
        self.from = was;
        self.acting_pane = prev;
        out
    }

    /// Which pane a window belongs to, for an event that names a window and nothing else.
    ///
    /// The frontend reports geometry per window and knows nothing about panes, so every one of
    /// those has to be turned back into "which half of the screen is this about" before it can be
    /// acted on.
    fn pane_of_window(
        &self,
        win: WindowId,
    ) -> Option<(neosh_proto::ViewId, neosh_proto::PaneId)> {
        self.views.iter().find_map(|(view, v)| {
            v.panes
                .values()
                .find(|p| p.chat_win == win || p.composer_win == win)
                .map(|p| (*view, p.pane))
        })
    }

    /// Every pane showing this conversation, and which terminal each is in.
    ///
    /// The question that replaced "is this the one on screen", and then replaced "which terminals
    /// are showing it" when a terminal stopped being able to show only one. A turn's output goes
    /// to whoever is looking at it, which may be nobody, may be two people, and may be twice on one
    /// screen — a conversation open in both halves of a split is two transcripts, and an answer
    /// that arrives in one of them is a redraw the other never gets.
    fn panes_showing(
        &self,
        session: &neosh_proto::SessionId,
    ) -> Vec<(neosh_proto::ViewId, neosh_proto::PaneId)> {
        self.views
            .iter()
            .flat_map(|(id, v)| {
                v.panes
                    .values()
                    .filter(|p| p.session == *session)
                    .map(move |p| (*id, p.pane))
            })
            .collect()
    }

    /// Every terminal with a pane showing this conversation, each named once.
    fn views_showing(&self, session: &neosh_proto::SessionId) -> Vec<neosh_proto::ViewId> {
        let mut out: Vec<neosh_proto::ViewId> =
            self.panes_showing(session).into_iter().map(|(v, _)| v).collect();
        out.dedup();
        out
    }

    /// Do something in every pane showing this conversation, which may be none.
    fn each_view_showing(
        &mut self,
        session: &neosh_proto::SessionId,
        f: impl Fn(&mut Self),
    ) {
        for (view, pane) in self.panes_showing(session) {
            self.in_pane(view, pane, |me| f(me));
        }
    }

    /// Send what is in the composer: the words, and whatever is on the attachment row.
    ///
    /// The row is emptied here rather than by whoever put something on it, because this is the
    /// moment the attachment stops being a draft — steered into a running turn or starting a new
    /// one, either way it has been said and is not yours to take back any more.
    fn start_turn(&mut self, text: String) {
        let here = self.active_session();
        let images = self.take_attachments(&here);
        self.start_turn_in(here, Prompt { text, images });
    }

    /// Take the attachment row for a conversation, as the blocks it becomes.
    fn take_attachments(&mut self, session: &neosh_proto::SessionId) -> Vec<ContentBlock> {
        self.attached.remove(session).unwrap_or_default().iter().map(|a| a.block()).collect()
    }

    /// Where attached images are kept. Beside the conversations they belong to, so clearing the
    /// state directory clears both and neither outlives the other.
    ///
    /// Under `--clean` there is no state directory at all, and a temporary one is the honest
    /// answer: nothing is being kept, which is what `--clean` means, and an image you paste still
    /// has to survive as far as the request.
    fn image_store(&self) -> std::path::PathBuf {
        match &self.state_dir {
            Some(dir) => dir.join("images"),
            None => std::env::temp_dir().join(format!("neosh-images-{}", std::process::id())),
        }
    }

    /// Put an image from a file on the composer's attachment row.
    ///
    /// Answers with what it attached, so the caller can say so — a chip appearing at the bottom of
    /// the screen is not enough on its own when the thing you pressed might equally have found
    /// nothing.
    fn attach(&mut self, path: &std::path::Path) -> Result<crate::images::Attachment, String> {
        let store = self.image_store();
        let got = crate::images::from_path(&store, path)?;
        let here = self.active_session();
        self.hold(&here, got.clone());
        Ok(got)
    }

    /// Put an attachment on a conversation's row, redrawing the chips if that is the one on screen.
    fn hold(&mut self, session: &neosh_proto::SessionId, got: crate::images::Attachment) {
        self.attached.entry(session.clone()).or_default().push(got);
        if *session == self.active_session() {
            self.refresh_composer();
        }
    }

    /// `^V`, and `chat.attach` with nothing named: attach the picture the clipboard is holding.
    ///
    /// Two shapes of answer, because a clipboard has two ways of holding a picture. Bytes are here
    /// and the chip appears in the same breath, silently — the chip *is* the report. An address is
    /// a round trip to somebody else's server, which cannot happen on this loop and cannot be
    /// silent either: a key press with nothing on screen after it is a key press that did nothing,
    /// so it says where it has gone and says again when it arrives.
    fn begin_paste(&mut self, waiting: Option<(PluginId, RequestId)>) {
        let store = self.image_store();
        let session = self.active_session();
        match crate::images::from_clipboard(&store) {
            Err(e) => self.finish_paste(session, waiting, Err(e), true),
            Ok(crate::images::Clipped::Image(got)) => {
                self.finish_paste(session, waiting, Ok(got), false)
            }
            Ok(crate::images::Clipped::Remote(url)) => {
                let where_ = short_url(&url);
                // Progress, not a message: it stops being true the moment the picture lands, and
                // pushing it onto a stack meant the row that superseded it sat *underneath* the
                // one it superseded. Keyed by conversation, so two fetches at once are two rows
                // and each is taken away by the one that finishes it.
                self.editor_progress(&format!("image:{session}"), format!("fetching {where_}\u{2026}"));
                let tx = self.image_tx.clone();
                tokio::spawn(async move {
                    let result = crate::images::from_url(&store, &url).await;
                    let _ = tx.send(Fetched { session, waiting, result });
                });
            }
        }
    }

    /// A picture that was asked for has arrived, or has not.
    ///
    /// `announce` is whether this one owes a sentence: a fetch already said "fetching…", and a line
    /// left saying that about a picture which is now on screen is the kind of stale that makes
    /// people stop reading the line at all.
    fn finish_paste(
        &mut self,
        session: neosh_proto::SessionId,
        waiting: Option<(PluginId, RequestId)>,
        result: Result<crate::images::Attachment, String>,
        announce: bool,
    ) {
        // Whatever this one was, it is no longer being fetched.
        self.editor_progress_done(&format!("image:{session}"));
        match &result {
            Ok(got) => {
                let elsewhere = session != self.active_session();
                // The conversation may have been archived or deleted while a server took its time.
                // A row on one that is gone is a row nothing will ever draw or send.
                if self.agent.sessions().get(&session).is_none() {
                    self.editor_message(
                        MessageLevel::Warn,
                        format!("{} arrived after its conversation went", got.label()),
                    );
                } else {
                    self.hold(&session, got.clone());
                    if announce || elsewhere {
                        // Named when it landed somewhere you are not looking, because the chip that
                        // usually does the reporting is on a screen you cannot see.
                        let where_ = match elsewhere {
                            true => format!(" in {}", self.session_name(&session)),
                            false => String::new(),
                        };
                        self.editor_message(
                            MessageLevel::Info,
                            format!("attached {}{where_}", got.label()),
                        );
                    }
                }
            }
            Err(e) => self.editor_message(MessageLevel::Warn, e.clone()),
        }
        if let Some((plugin, id)) = waiting {
            let response: ApiResponse = result
                .map(|got| ApiOk::Attachments { attachments: vec![info_of(&got)] })
                .map_err(|message| ApiError::InvalidArgument { message })
                .into();
            let _ = self.bridge.script().send(ScriptInbound::Plugin {
                plugin,
                msg: PluginInbound::Response { id, response },
            });
        }
    }

    /// What to call a conversation in a sentence about it.
    fn session_name(&self, session: &neosh_proto::SessionId) -> String {
        self.agent
            .sessions()
            .get(session)
            .and_then(|s| s.title.clone())
            .unwrap_or_else(|| "another conversation".to_string())
    }

    /// Whether the conversation on screen has an image attached that has not been sent.
    fn has_attachment(&self) -> bool {
        self.attached.get(&self.active_session()).is_some_and(|v| !v.is_empty())
    }

    /// Take the last attached image back off, saying which one went.
    ///
    /// Said out loud because the chip it removes may have been the only one on screen: a row that
    /// silently becomes no row is indistinguishable from a key that did nothing.
    fn drop_attachment(&mut self) {
        let here = self.active_session();
        match self.attached.entry(here).or_default().pop() {
            None => self.editor_message(MessageLevel::Info, "nothing attached"),
            Some(a) => {
                self.editor_message(MessageLevel::Info, format!("took off {}", a.label()));
                self.refresh_composer();
            }
        }
    }

    /// Start a turn in a named conversation, drawing it in whichever terminals are showing it.
    ///
    /// Named rather than implied, because the caller is not always the keyboard: a turn that ends
    /// with something still queued starts the next one, and by then you may be reading something
    /// else entirely — or somebody at the terminal next door may be reading it instead.
    fn start_turn_in(&mut self, session: neosh_proto::SessionId, prompt: Prompt) {
        // Another machine's conversation: the turn is that machine's to run.
        let mirror = self.agent.sessions().get(&session).and_then(|s| s.mirror.clone());
        if let Some(mirror) = mirror {
            self.send_to_mirror(&session, &mirror, prompt);
            return;
        }
        // Typing while it works is steering, not an error. The old behaviour — refuse, and make you
        // wait with the sentence already written — is the one moment in the program where you know
        // exactly what you want to say and cannot say it.
        //
        // A turn that is *winding down* after `<Esc>` counts as running for exactly the same
        // reason: its process has not finished with the conversation yet, so this is queued and
        // becomes the next turn through the leftover path in `TurnEnded`. Starting one here
        // instead would put two turns on a driver that keeps one process per conversation.
        if self.turns.contains_key(&session) {
            self.agent.steer(&session, prompt);
            self.each_view_showing(&session, |me| {
                // Sending is asking to see what happens, whether it starts a turn or joins one.
                // Without this a message steered in while the transcript was scrolled back — which
                // is where reading it leaves you — landed below the screen, and what you could
                // still see was older text with nothing new in it.
                me.scroll_chat(0);
                me.draw_working();
                me.refresh_composer();
            });
            return;
        }
        // Asked here, so asked for anybody watching from another machine — before the turn starts,
        // which is the order the question and its answer are drawn in.
        if self.watched(&session) && (!prompt.text.is_empty() || !prompt.images.is_empty()) {
            self.stream_out(&session, neosh_proto::StreamEvent::Asked {
                text: prompt.text.clone(),
                images: prompt.images.len() as u32,
                pictures: picture_files(&prompt.images),
            });
        }
        let token = CancellationToken::new();
        self.turns
            .insert(session.clone(), Running { cancel: token.clone(), cancelling: false });
        self.rounds.insert(session.clone(), Round {
            // Per turn, so it does not change under you mid-answer.
            verb: a_verb(),
            started: std::time::Instant::now(),
            note: None,
            note_is_wait: false,
            said: Vec::new(),
            changes: Vec::new(),
            plan: Vec::new(),
            tasks: Vec::new(),
        });
        // Stamped here rather than inside the agent so `Session` stays clock-free and testable.
        // The turn id itself is set by the agent a moment later; a list only reads this while the
        // turn id is present, so the ordering is not observable.
        if let Some(s) = self.agent.sessions().get_mut(&session) {
            s.turn_started_at = Some(now_secs());
        }
        self.each_view_showing(&session, |me| {
            // Asking a question means you want to see the answer: scrolled-back state does not
            // survive sending, or the reply arrives somewhere off screen.
            me.scroll_chat(0);
            // Unless nothing was asked. A turn opened to hold something the agent had already
            // started saying has an answer and no question, and a blank question row above it is
            // a message the transcript claims you sent. See [`Self::hear_out`].
            if !prompt.text.is_empty() || !prompt.images.is_empty() {
                me.chat_question(&prompt);
            }
            me.begin_working();
            // `\u{23ce}` steers from here until the turn is over, and the empty field is where that
            // has to be said — it is the one thing on screen you are looking at when you press it.
            me.refresh_composer();
        });

        let agent = self.agent.clone();
        let bridge = self.bridge.clone();
        tokio::spawn(async move {
            agent.run_turn_in(&*bridge, session, prompt, token).await;
        });
    }

    /// Ask the turn in the conversation on screen to stop, if there is one.
    ///
    /// Answers whether there was a turn to interrupt — which is what tells `<Esc>` it has been
    /// spent, so a second press does not fall through to clearing the draft. The entry stays until
    /// the turn actually ends; see [`Running::cancelling`].
    /// `None` when there was no turn here; `Some(true)` when this press is what stopped it, and
    /// `Some(false)` when it was already winding down — the key is spent either way, but only one
    /// of them is news worth saying out loud.
    fn cancel_turn_here(&mut self) -> Option<bool> {
        let here = self.active_session();
        // Another machine's conversation: asking it to stop is asking that machine.
        let mirror = self.agent.sessions().get(&here).and_then(|s| s.mirror.clone());
        if let Some(m) = mirror {
            let t = self.turns.get_mut(&here)?;
            if t.cancelling {
                return Some(false);
            }
            t.cancelling = true;
            self.command_mirror(&m, neosh_proto::AgentCommand::Interrupt);
            return Some(true);
        }
        let t = self.turns.get_mut(&here)?;
        if t.cancelling {
            return Some(false);
        }
        t.cancelling = true;
        t.cancel.cancel();
        Some(true)
    }

    /// Make sure leaving `session` has somewhere to land, minting a placeholder if it does not.
    ///
    /// A conversation you are finished with is a conversation you can close, and that includes the
    /// last one: "the last session cannot be closed" is the store refusing the only thing left to
    /// do in a workspace you have just cleared out. The store always has an active conversation —
    /// there has to be somewhere for the next message to go — so "nothing left" is spelled as a
    /// [placeholder](neosh_agent::Session::ephemeral): the composer types into it and the
    /// transcript shows the welcome, but no list contains it and nothing is written to disk. An
    /// emptied workspace should look empty, and one conversation named after nothing is not empty.
    ///
    /// Asked of [`list`](neosh_agent::store::SessionStore::list), which leaves out what is
    /// archived, because landing in something you deliberately put away is the one destination
    /// that is definitely wrong — a workspace whose every other conversation is archived is as
    /// alone as one with no other conversation at all.
    fn ensure_somewhere_to_go(&mut self, leaving: &neosh_proto::SessionId) {
        let alone = { self.agent.sessions().list().iter().all(|s| &s.id == leaving) };
        if alone {
            // In the directory you are leaving rather than the one neosh was started in: the next
            // thing you type is almost certainly about the project you were just in, and a
            // placeholder somewhere else would silently move the file tools with it.
            let id = self.new_session_in(self.cwd.clone());
            if let Some(s) = self.agent.sessions().get_mut(&id) {
                s.ephemeral = true;
            }
        }
    }

    /// Take this terminal out of a conversation that is about to go away, and say whether it moved.
    ///
    /// The store used to do this itself, as part of closing or archiving: it held one session out
    /// of the map and called it active, so "step away" had exactly one thing to move. With a view
    /// per terminal there is no single "you" — two of them may be in the conversation being
    /// deleted, and each has to be told separately — so the move is the host's and the store only
    /// answers where to go.
    ///
    /// Call it *before* the conversation leaves the store: afterwards this view names something
    /// that is not there.
    fn step_out_of(&mut self, going: &neosh_proto::SessionId) -> bool {
        if self.v().session != *going {
            return false;
        }
        self.ensure_somewhere_to_go(going);
        let Some(next) = self.agent.sessions().next_after(going) else { return false };
        self.arrive_in(&next).is_ok()
    }

    /// Start an empty conversation without switching to it, and say which one it is.
    ///
    /// Exists so that closing or archiving the only conversation you have has somewhere to land.
    /// It inherits the model and system prompt for the same reason a new conversation does:
    /// changing what you are talking to as a side effect of tidying up would be a strange thing to
    /// do.
    fn new_session_in(&mut self, cwd: std::path::PathBuf) -> neosh_proto::SessionId {
        self.new_session_titled(cwd, None)
    }

    /// The same, with a name given up front.
    ///
    /// A caller that is *making* conversations — a peer over ASCP, an orchestrator plugin fanning
    /// work out — knows what each one is for at the moment it asks for it, and that is the only
    /// moment it is cheap to say. The name was accepted on the wire and dropped on the floor, so
    /// three conversations started by one command came back labelled by whatever their first
    /// message happened to begin with.
    fn new_session_titled(
        &mut self,
        cwd: std::path::PathBuf,
        title: Option<String>,
    ) -> neosh_proto::SessionId {
        let mut session = neosh_agent::Session::new(cwd);
        session.created_at = now_secs();
        session.updated_at = session.created_at;
        session.selection = self.agent.selection();
        session.system = self.agent.session().system.clone();
        // Blank is not a name. An empty string would render as a row with nothing on it, which is
        // worse than the label a first message gives it.
        session.title = title.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
        let id = session.id.clone();
        self.agent.sessions().insert(session);
        id
    }

    /// Say something in the corner, as a reply to a key that was just pressed.
    ///
    /// The default kind, and what all sixty-odd of the host's own call sites mean: this is
    /// feedback for a keystroke, it is not news, and it never leaves the terminal. See, for the
    /// two things that are not this, [`Host::editor_progress`] and [`Host::alert`].
    fn editor_message(&mut self, level: MessageLevel, text: impl Into<String>) {
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::Notify {
            level,
            message: text.into(),
            kind: neosh_proto::NoticeKind::Reply,
            key: None,
        });
    }

    /// Say what is happening, in a row that will be replaced rather than stacked.
    ///
    /// Ends with [`Host::editor_progress_done`] on the same key. A progress row that is never
    /// finished falls off after a minute rather than staying forever, but relying on that is a
    /// row that lies for up to sixty seconds.
    fn editor_progress(&mut self, key: &str, text: impl Into<String>) {
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::Notify {
            level: MessageLevel::Info,
            message: text.into(),
            kind: neosh_proto::NoticeKind::Progress,
            key: Some(key.to_string()),
        });
    }

    fn editor_progress_done(&mut self, key: &str) {
        let _ = self
            .editor
            .apply(&PluginId::from(BUILTIN), ApiCall::NotifyDone { key: key.to_string() });
    }

    /// News: something happened that the user did not cause.
    ///
    /// Drawn in the corner *and*, if they turn out not to be looking, raised outside the terminal.
    /// Whether they are looking is settled later — in the run loop, which is the only place that
    /// can ask the frontend — so this is the offer rather than the decision.
    fn alert(
        &mut self,
        reason: crate::notify::AlertReason,
        session: Option<&neosh_proto::SessionId>,
        level: MessageLevel,
        title: impl Into<String>,
        body: impl Into<String>,
    ) {
        let title = title.into();
        let body = body.into();
        // On screen either way. An alert the user *can* see is still worth a row in the corner —
        // it is the leaving-the-terminal part that is conditional, not the saying of it.
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::Notify {
            level,
            message: body.clone(),
            kind: neosh_proto::NoticeKind::Alert,
            key: None,
        });
        self.notifier.offer(reason, session, crate::notify::Alert { title, body, level });
    }

    /// Say what another machine's conversations did that nobody saw: a turn that finished while
    /// nobody was reading it anywhere, and one that stopped to ask something.
    ///
    /// The owner raises these on its own screen, which for a machine in a cupboard is nobody's —
    /// so the person who would have been told, sitting here, was not, and found out by the row
    /// turning amber whenever they next looked at the panel. Read off the roster, because that is
    /// the one thing every paired machine hears about every conversation: a turn that ended and is
    /// `unread` is one nobody was watching, which is exactly the one a notification is for. One
    /// that is open here is left to the mirror, whose own ending — or relayed question — already
    /// says it.
    fn alert_remote_turns(
        &mut self,
        node: &neosh_proto::NodeId,
        before: &[neosh_proto::AgentSummary],
    ) {
        use neosh_proto::AgentState as A;
        let Some(peer) = self.swarm.peer(node) else { return };
        let machine = peer.display_name();
        let now: Vec<neosh_proto::AgentSummary> = peer.agents.clone();
        for a in now {
            let Some(was) = before.iter().find(|b| b.session == a.session) else { continue };
            if self.mirror_of(node, &a.session).is_some() {
                continue;
            }
            let turning = |s: A| matches!(s, A::Running | A::Blocked);
            if turning(was.state) && !turning(a.state) && a.unread {
                let took = was.turn_started_at.map(|at| now_secs().saturating_sub(at).max(0));
                let min = self.notifier.config().min_turn.as_secs() as i64;
                if took.is_some_and(|secs| secs < min) {
                    continue;
                }
                let (reason, level, what) = if a.interrupted {
                    (crate::notify::AlertReason::Failure, MessageLevel::Warn, "was cut off")
                } else {
                    (crate::notify::AlertReason::TurnDone, MessageLevel::Info, "finished")
                };
                self.alert(reason, Some(&a.session), level, a.label.clone(), format!(
                    "{what} on {machine}"
                ));
            } else if was.state != A::Blocked && a.state == A::Blocked {
                self.alert(
                    crate::notify::AlertReason::Question,
                    Some(&a.session),
                    MessageLevel::Warn,
                    a.label.clone(),
                    format!("is waiting for you on {machine}"),
                );
            }
        }
    }

    /// A conversation is blocked on you answering something.
    ///
    /// The strongest case there is for a notification: the turn is not merely finished, it is
    /// *stopped*, and it stays stopped until you go and look. Only for conversations that were not
    /// already asking — the var is rewritten whenever the set changes, including when one of three
    /// is answered, and re-announcing the other two then would be a notification per answer.
    fn alert_asking(&mut self, value: &serde_json::Value, reason: crate::notify::AlertReason) {
        let Some(ids) = value.as_array() else { return };
        let now: Vec<neosh_proto::SessionId> = ids
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| neosh_proto::SessionId(s.to_string()))
            .collect();
        let seen = match reason {
            crate::notify::AlertReason::Permission => &mut self.permitting,
            _ => &mut self.asking,
        };
        let fresh: Vec<neosh_proto::SessionId> =
            now.iter().filter(|s| !seen.contains(*s)).cloned().collect();
        *seen = now;
        for session in fresh {
            // A question in the conversation you are looking at opens its panel over the composer,
            // which is louder than any notification and already has your attention.
            if self.can_see(&session) {
                continue;
            }
            let label = self
                .agent
                .sessions()
                .get(&session)
                .map(|s| s.label())
                .unwrap_or_else(|| "a conversation".into());
            self.alert(reason, Some(&session), MessageLevel::Warn, label, match reason {
                crate::notify::AlertReason::Permission => "is waiting for permission",
                _ => "is asking you something",
            });
        }
    }

    /// Read the whole `notify.*` family into the notifier, and tell the terminal how to raise one.
    ///
    /// An unparseable enum keeps the default rather than failing: these arrive from a declared
    /// `OptionType::Enum`, so the set has already been checked at set time, and the only way to be
    /// here with a word we do not know is a version skew — where a notification nobody expected is
    /// better than no notifications at all.
    fn reload_notify(&mut self) {
        let opts = self.editor.options();
        let str_of = |name: &str| opts.str(name).unwrap_or_default().to_string();
        let cfg = crate::notify::NotifyConfig {
            enabled: opts.bool("notify.enabled").unwrap_or(true),
            when: opts.list("notify.when").unwrap_or_else(|| {
                crate::notify::AlertReason::all().iter().map(|r| r.key().to_string()).collect()
            }),
            away: crate::notify::AwayPolicy::parse(&str_of("notify.away")).unwrap_or_default(),
            desktop: crate::notify::DesktopPolicy::parse(&str_of("notify.desktop"))
                .unwrap_or_default(),
            idle_after: Duration::from_secs(opts.int("notify.idle_after").unwrap_or(60).max(1) as u64),
            min_turn: Duration::from_secs(opts.int("notify.min_turn").unwrap_or(10).max(0) as u64),
            bell: opts.bool("notify.bell").unwrap_or(false),
        };
        let osc = neosh_tui::terminal::AlertOsc::parse(&str_of("notify.osc"))
            .unwrap_or_else(neosh_tui::terminal::AlertOsc::detect);
        let bell = cfg.bell;
        self.notifier.configure(cfg);
        // A no-op unless this process owns its terminal. A client attached over a socket detects
        // its own and is overridden by `NEOSH_NOTIFY_OSC`, because the terminal may be on another
        // machine entirely and a workspace's configuration is its own.
        self.frontend.configure_alerts(osc, bell);
    }

    /// A turn ended. Decide whether that is news, and say so if it is.
    ///
    /// Three tests, and it is not a notification unless it passes all of them: it happened
    /// somewhere you are not looking, it took long enough that you had time to look away, and the
    /// kind of ending is one you asked to hear about.
    fn alert_turn_ended(
        &mut self,
        session: &neosh_proto::SessionId,
        stop_reason: &neosh_proto::StopReason,
        label: &str,
        took: Option<i64>,
    ) {
        use neosh_proto::StopReason;

        // The screen in front of you. The unread mark's rule, and the only one of the three that is
        // about where the person is rather than about the turn.
        if self.can_see(session) {
            return;
        }
        // Or on the one in front of you on another machine, which says so itself: its mirror of
        // this conversation ends the same turn, through the same path.
        if self.watched(session) {
            return;
        }
        // `<Esc>` is you ending the turn, so a notification saying it ended is a notification about
        // something you just did. `ToolUse` is not an ending at all — the loop is between rounds.
        if matches!(stop_reason, StopReason::Cancelled | StopReason::ToolUse) {
            return;
        }
        let (reason, level, what) = match stop_reason {
            StopReason::Error { message } => (
                crate::notify::AlertReason::Failure,
                MessageLevel::Error,
                format!("failed: {message}"),
            ),
            StopReason::Refusal { explanation, .. } => (
                crate::notify::AlertReason::Failure,
                MessageLevel::Warn,
                match explanation {
                    Some(why) => format!("stopped: {why}"),
                    None => "stopped without answering".to_string(),
                },
            ),
            // Not an error, and not a clean finish either: the answer is cut off mid-sentence and
            // saying "finished" about it would be wrong.
            StopReason::MaxTokens => (
                crate::notify::AlertReason::TurnDone,
                MessageLevel::Warn,
                "ran out of room mid-answer".to_string(),
            ),
            _ => (
                crate::notify::AlertReason::TurnDone,
                MessageLevel::Info,
                "finished".to_string(),
            ),
        };
        // A turn that took three seconds ended while you were still looking at the key that
        // started it. Failures are exempt: a turn that died in two seconds died *because* it went
        // wrong immediately, which is the one you most need telling about.
        let min = self.notifier.config().min_turn.as_secs() as i64;
        if reason == crate::notify::AlertReason::TurnDone
            && took.is_some_and(|secs| secs < min)
        {
            return;
        }
        self.alert(reason, Some(session), level, label, what);
    }

    /// Send the gathered burst, if its window has closed.
    async fn deliver_alerts(&mut self) -> anyhow::Result<()> {
        let Some(alert) = self.notifier.take_due() else { return Ok(()) };
        self.raise(alert).await
    }

    /// Send whatever is waiting, window or no window. For shutdown.
    async fn deliver_alerts_now(&mut self) -> anyhow::Result<()> {
        let Some(alert) = self.notifier.flush() else { return Ok(()) };
        self.raise(alert).await
    }

    /// Get one notification in front of a person, by whichever route can reach them.
    ///
    /// The only place that asks where they are, because the frontend is the only thing that knows
    /// and this is the only place with a frontend. Three answers and three channels:
    ///
    /// - **Watching** — a terminal is attached and in front of somebody. The corner already said
    ///   it, and nothing else should: a notification for something on screen is what teaches
    ///   people to dismiss notifications.
    /// - **Away** — attached, unfocused. `UiEvent::Alert` goes to the views and each writes the
    ///   escape its own terminal speaks, which is the only way it comes out where the person is
    ///   rather than where the process is.
    /// - **Detached** — nothing attached, so there is no stream and no wrong machine to be on.
    async fn raise(&mut self, alert: crate::notify::Alert) -> anyhow::Result<()> {
        use crate::frontend::Presence;
        use crate::notify::DesktopPolicy;

        let cfg = self.notifier.config().clone();
        if !cfg.enabled || cfg.away == crate::notify::AwayPolicy::Never {
            return Ok(());
        }
        let presence = self.frontend.presence(cfg.idle_after).await;
        // `offscreen` is for a tiled layout somebody keeps neosh visible in and does not watch:
        // by the time an alert exists the host has already established it is about a conversation
        // they are not in, so being attached is as far as the question goes.
        let away = match cfg.away {
            crate::notify::AwayPolicy::Offscreen => presence != Presence::Detached,
            _ => presence == Presence::Away,
        };

        if away {
            // Untagged: an alert is about the workspace, and every terminal that cannot see it
            // is one that should raise it.
            self.frontend
                .send(vec![
                    (None, UiEvent::Alert {
                        title: alert.title.clone(),
                        body: alert.body.clone(),
                        level: alert.level,
                    }),
                    (None, UiEvent::Flush),
                ])
                .await?;
        }
        let desktop = match cfg.desktop {
            DesktopPolicy::Never => false,
            DesktopPolicy::Always => away || presence == Presence::Detached,
            DesktopPolicy::WhenDetached => presence == Presence::Detached,
        };
        if desktop {
            crate::notify::desktop(&alert);
        }
        Ok(())
    }

    /// Whether the user can already see what is happening in this conversation.
    ///
    /// The whole test. A turn ending in the conversation on screen is the screen in front of you,
    /// and the `unread` mark says the same thing — but it counts *detached* as on screen, on the
    /// reasoning that reattaching lands you in that conversation with the answer in it. True for
    /// the mark and false for the notification: with nothing attached, a finished turn is the most
    /// newsworthy thing a workspace can produce. Attachment is asked separately, in the run loop.
    fn can_see(&self, session: &neosh_proto::SessionId) -> bool {
        *session == self.active_session()
    }

    fn drain_effects(&mut self) {
        for effect in self.editor.drain_effects() {
            match effect {
                CoreEffect::InvokeCommand { plugin, name, args, key } => {
                    // The host registers its own commands through the same registry a plugin uses,
                    // so they are listable and rebindable like any other. It just answers them
                    // itself rather than sending them to a JS plugin that does not exist.
                    if plugin.0 == BUILTIN {
                        // The key is carried through, because two of these read it: `<C-w>3` is
                        // `tab.select` with the digit in the press rather than in an argument, and
                        // a command invoked from `^K` has no key at all and says so.
                        self.run_builtin(&name, args, key.as_ref());
                    } else if plugin.0 == LAZY {
                        // A key for a plugin that is not up yet: load it, and press again for it.
                        self.wake_on_command(&name, args);
                    } else {
                        self.bridge.notify(&plugin, PluginEvent::CommandInvoked { name, args, key });
                    }
                }
                CoreEffect::UnhandledKey { key, mode, view: _ } => {
                    self.handle_unbound_key(key, mode)
                }
                CoreEffect::OptionChanged { name, value } => {
                    self.apply_option(&name, &value);
                    // Broadcast, not just to the owner: a setting is shared state, and the plugin
                    // that declared `ui.theme` is rarely the one that changes it.
                    self.broadcast(PluginEvent::OptionChanged { name, value });
                }
                CoreEffect::ContributionsChanged { point } => {
                    // A theme came or went: the option that lists them has to say so, and a
                    // theme in use that has just been withdrawn has to give way.
                    if point == THEME_POINT {
                        self.sync_themes();
                    }
                    // Broadcast for the same reason: the plugin that *renders* a point is never the
                    // one that just contributed to it, and it is the one that has to redraw.
                    self.broadcast(PluginEvent::ContributionsChanged { point });
                }
                CoreEffect::HighlightsChanged { names } => {
                    self.broadcast(PluginEvent::HighlightChanged { names });
                }
                CoreEffect::KindSeen { kind } => self.wake_on_kind(&kind),
            }
        }
        self.sync_panes();
    }

    /// Bring what is *in* each pane back into step with the tree that says where the panes are.
    ///
    /// The editor owns the shape and the host owns the contents, and the two are changed by
    /// different things: a `<C-w>v` and a plugin's `pane.split` both reshape the tree, and neither
    /// knows how to build a transcript. Rather than every one of those paths remembering to furnish
    /// and tear down, this runs after every apply and makes the difference true — which is the same
    /// bargain `drain_effects` itself is, and it means a pane created by a plugin nobody has heard
    /// of is furnished exactly like one created by a key.
    ///
    /// Cheap enough to run unconditionally: it is a couple of `BTreeMap` scans over single digits
    /// of panes, and it does nothing at all in the overwhelmingly common case where nothing moved.
    fn sync_panes(&mut self) {
        let views: Vec<neosh_proto::ViewId> = self.views.keys().copied().collect();
        for view in views {
            let live = self.editor.panes_of(view);
            let Some(state) = self.views.get(&view) else { continue };

            // A pane the tree no longer has. Its windows went with it — the editor closes those —
            // and its buffers go here, because a transcript nothing can open is memory that grows
            // every time somebody closes a split.
            let gone: Vec<neosh_proto::PaneId> =
                state.panes.keys().copied().filter(|p| !live.contains(p)).collect();
            for pane in gone {
                let Some(dead) = self.views.get_mut(&view).and_then(|v| v.panes.remove(&pane))
                else {
                    continue;
                };
                let plugin = PluginId::from(BUILTIN);
                for buf in [dead.chat, dead.composer] {
                    let _ = self.editor.apply(&plugin, ApiCall::BufDelete { buf });
                }
            }

            // A pane the tree has that nothing has furnished — a plugin's split, or the far side of
            // a key's. It shows what the pane with the keyboard is showing, which is Vim's answer
            // for `:split` and the only one that needs no dialog: a split that stopped to ask which
            // conversation would be a split you cannot bind to a key.
            let Some(state) = self.views.get(&view) else { continue };
            let fresh: Vec<neosh_proto::PaneId> =
                live.iter().copied().filter(|p| !state.panes.contains_key(p)).collect();
            if !fresh.is_empty() {
                let inherit = self
                    .editor
                    .active_pane_of(view)
                    .and_then(|p| state.panes.get(&p))
                    .or_else(|| state.panes.values().next())
                    .map(|p| p.session.clone())
                    .unwrap_or_else(|| self.agent.sessions().current_id().clone());
                for pane in fresh {
    let as_term = match &self.pending_term {
                        Some((p, at)) if *p == pane => Some(at.clone()),
                        _ => None,
                    };
                    if as_term.is_some() {
                        self.pending_term = None;
                    }
                    let furnished = match as_term.is_some() {
                        true => PaneState::furnish_term(&mut self.editor, view, pane, inherit.clone()),
                        false => PaneState::furnish(&mut self.editor, view, pane, inherit.clone()),
                    };
                    if let Some(v) = self.views.get_mut(&view) {
                        v.panes.insert(pane, furnished);
                    }
                    if let Some(at) = as_term {
                        self.start_terminal(view, pane, at);
                        continue;
                    }
                    // Drawn as an arrival, exactly as a terminal attaching is: the transcript is
                    // rebuilt from the conversation's messages and the composer says what `⏎` does.
                    // Left to the next redraw it would be a pane of nothing until you typed.
                    self.in_pane(view, pane, |me| {
                        me.enter_session();
                        me.refresh_composer();
                    });
                }
            }

            self.settle_tab_groups(view);
            self.rehome(view);
        }
        self.refresh_tabline();
    }

    /// Hand a gone conversation's tabs to the conversation each terminal landed in.
    ///
    /// Every view, not only the one that asked: the same conversation can have had a tab open in
    /// two terminals, and a tab left on a group nothing will ever show again is a pane, its
    /// windows, and whatever is running in it, all unreachable. Where they land is that terminal's
    /// own bar, which by the time this runs is the conversation it stepped out into.
    fn adopt_tabs_of(&mut self, gone: &neosh_proto::SessionId) {
        let views: Vec<neosh_proto::ViewId> = self.views.keys().copied().collect();
        for view in views {
            let Some(here) = self.editor.tab_group_of(view) else { continue };
            if here == gone.0 {
                // The terminal is still standing in the conversation that has gone — nothing has
                // moved it, so its tabs are all together and all on screen. Moving them somewhere
                // would be inventing a destination this does not have.
                continue;
            }
            let orphans: Vec<neosh_proto::TabId> = self
                .editor
                .tabs_of(view)
                .iter()
                .filter(|t| t.group.as_deref() == Some(gone.0.as_str()))
                .map(|t| t.id)
                .collect();
            for tab in orphans {
                self.editor.tab_set_group(view, tab, Some(here.clone()));
            }
        }
    }

    /// File any tab nobody has filed yet under the conversation in it.
    ///
    /// The editor makes the first tab of a terminal before anything knows which conversation that
    /// terminal is opening in, and a workspace restored from disk puts a conversation into a pane
    /// without ever *arriving* in it — so both leave a tab on the group `None` is. Left there, the
    /// first shell you opened would inherit `None` and go on being drawn beside every conversation,
    /// which is the whole of what this change is about.
    ///
    /// Only ever a tab with no group at all: it is a settling, not a reconciliation. Re-deriving
    /// every tab's group from its active pane on each pass would make a tab split across two
    /// conversations change bars whenever the keyboard crossed the split — see [`Self::arrive_in`],
    /// which is the other and only place a tab moves.
    fn settle_tab_groups(&mut self, view: neosh_proto::ViewId) {
        let unfiled: Vec<(neosh_proto::TabId, neosh_proto::PaneId)> = self
            .editor
            .tabs_of(view)
            .iter()
            .filter(|t| t.group.is_none())
            .map(|t| (t.id, t.active_pane))
            .collect();
        for (tab, pane) in unfiled {
            let Some(session) =
                self.views.get(&view).and_then(|v| v.panes.get(&pane)).map(|p| p.session.clone())
            else {
                continue;
            };
            self.editor.tab_set_group(view, tab, Some(session.0));
        }
    }

    /// Start a shell in a pane, and claim the surface it draws on.
    ///
    /// Sized from the window's viewport when the frontend has reported one, and from a sane default
    /// when it has not — the first `ViewportChanged` resizes it, which is one frame later and is
    /// exactly the same path a person dragging the terminal's corner takes.
    fn start_terminal(
        &mut self,
        view: neosh_proto::ViewId,
        pane: neosh_proto::PaneId,
        at: ShellAt,
    ) {
        let Some(state) = self.views.get(&view).and_then(|v| v.panes.get(&pane)) else { return };
        let (win, session) = (state.chat_win, state.session.clone());
        let (cols, rows) = self
            .editor
            .window(win)
            .and_then(|w| w.viewport)
            .map(|v| (v.width, v.height))
            .unwrap_or((80, 24));
        // A shell *in the conversation's directory* is on the machine the conversation is on: the
        // directory is a path on that disk, and the reason for a shell beside an agent is to run
        // something where it has just been working.
        let at = match (at, self.mirror_meta(&session)) {
            (ShellAt::Here { cwd: None }, Some(m)) => {
                ShellAt::There { node: m.node, cwd: Some(self.session_cwd(&session).display().to_string()) }
            }
            (at, _) => at,
        };
        let cwd = match at {
            ShellAt::There { node, cwd } => {
                self.ask_for_shell(view, pane, win, node, cwd, rows, cols);
                return;
            }
            ShellAt::Here { cwd } => cwd.unwrap_or_else(|| self.session_cwd(&session)),
        };

        match crate::term::Term::spawn(&cwd, rows, cols) {
            Ok(t) => {
                self.terminals.insert(pane, t);
                let plugin = PluginId::from(BUILTIN);
                let _ = self.editor.apply_in(view, &plugin, ApiCall::SurfaceClaim {
                    win,
                    rect: neosh_proto::Rect { row: 0, col: 0, width: cols, height: rows },
                });
                self.scaffold_terminal(view, pane, rows, cols);
                self.draw_terminal(view, pane);
            }
            // Said out loud and the pane left empty rather than silently blank: a shell that would
            // not start is a thing about the machine — no pty, a `$SHELL` that is not there — and
            // the person has to be told which.
            Err(e) => {
                self.editor_message(MessageLevel::Error, format!("could not start a shell: {e}"));
            }
        }
    }

    /// Ask a peer for a shell, and claim the pane's surface while it answers.
    ///
    /// The surface is claimed *now* rather than when the pty arrives, so the pane is a terminal
    /// from the frame the key was pressed in. What is in it until the far end answers is a
    /// sentence saying who is being waited on — because the alternative is an empty black
    /// rectangle, and on a slow link that is indistinguishable from a shell that started and
    /// printed nothing.
    #[allow(clippy::too_many_arguments)]
    fn ask_for_shell(
        &mut self,
        view: neosh_proto::ViewId,
        pane: neosh_proto::PaneId,
        win: neosh_proto::WindowId,
        node: neosh_proto::NodeId,
        cwd: Option<String>,
        rows: u16,
        cols: u16,
    ) {
        let Some(handle) = self.swarm_node.clone() else {
            self.editor_message(MessageLevel::Error, "the swarm is not running");
            self.close_pane_quietly(view, pane);
            return;
        };
        // What `swarm.shell` checks before it makes a pane, checked here too for the panes made
        // some other way — `<C-w>T` in another machine's conversation is one. A request to a
        // machine that is not there waits for an answer nothing will send.
        let refused = match self.swarm.peer(&node) {
            None => Some(format!("no machine {}", node.short())),
            Some(p) if !p.up() => {
                Some(format!("{} is not connected — c on its row in ^T reconnects", p.display_name()))
            }
            Some(p) if !p.capabilities.shells => Some(format!(
                "{} does not open shells for other machines — set `accepts_shells = true` under \
                 `[swarm]` there",
                p.display_name()
            )),
            Some(_) => None,
        };
        if let Some(why) = refused {
            self.editor_message(MessageLevel::Warn, why);
            self.close_pane_quietly(view, pane);
            return;
        }
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply_in(view, &plugin, ApiCall::SurfaceClaim {
            win,
            rect: neosh_proto::Rect { row: 0, col: 0, width: cols, height: rows },
        });
        self.scaffold_terminal(view, pane, rows, cols);
        let name = self
            .swarm
            .peer(&node)
            .map(|p| p.display_name())
            .unwrap_or_else(|| node.short().to_string());
        self.say_on_surface(view, pane, &format!("opening a shell on {name}…"));
        // Shares the counter every other request to a peer uses, so the id spaces cannot collide —
        // which matters precisely because a refusal is a bare `Refused { id }`.
        self.swarm_next_command += 1;
        let id = format!("s{}", self.swarm_next_command);
        self.swarm_asking.insert(id.clone(), SwarmAsk::Shell);
        self.swarm_opening.insert(id.clone(), (view, pane, node.clone()));
        handle.send(neosh_swarm::SwarmRequest::PtyOpen { node, id, cwd, cols, rows });
    }

    /// One line of text into a pane's raw-cell surface.
    ///
    /// For the gap between a remote shell being asked for and arriving, which is a round trip and
    /// on a link worth having a shell over can be a noticeable one. The alternative is an empty
    /// black rectangle, which is exactly what a shell that started and printed nothing looks like —
    /// so the one thing somebody cannot tell from the pane is whether anything is happening at all.
    ///
    /// Not a buffer line: the pane's buffer is a scaffold of spaces *under* the surface, and the
    /// surface is what is painted. Overwritten wholesale the moment the terminal attaches, because
    /// `Term::cells` emits every cell every time — which is the same property that stops the last
    /// frame of one program showing through the next.
    fn say_on_surface(&mut self, view: neosh_proto::ViewId, pane: neosh_proto::PaneId, text: &str) {
        let win = self.pane_window(view, pane);
        let Some(surface) = self.editor.surface_on(win) else { return };
        let plugin = PluginId::from(BUILTIN);
        let cells = text
            .graphemes(true)
            .enumerate()
            .map(|(i, g)| neosh_proto::SurfaceCell {
                row: 0,
                col: i as u16,
                grapheme: g.to_string(),
                fg: None,
                bg: None,
                attrs: neosh_proto::Attrs { dim: true, ..Default::default() },
            })
            .collect();
        let _ = self.editor.apply_in(view, &plugin, ApiCall::SurfacePut { surface, cells });
    }

    /// A peer answered [`ApiCall::SwarmShell`]: attach the pane's terminal, or say why not.
    fn shell_answered(&mut self, id: &str, pty: Result<String, String>) {
        let Some((view, pane, node)) = self.swarm_opening.remove(id) else { return };
        self.swarm_asking.remove(id);
        let name = self
            .swarm
            .peer(&node)
            .map(|p| p.display_name())
            .unwrap_or_else(|| node.short().to_string());
        let pty = match pty {
            Ok(pty) => pty,
            Err(reason) => {
                // Loud, and the pane goes. A terminal you cannot type into is worse than no
                // terminal: the tab is on the bar, the keys go somewhere, and nothing on screen
                // ever says why the prompt never came.
                self.editor_message(MessageLevel::Error, format!("{name}: {reason}"));
                self.close_pane_quietly(view, pane);
                return;
            }
        };
        let Some(handle) = self.swarm_node.clone() else { return };
        let (cols, rows) = self
            .views
            .get(&view)
            .and_then(|v| v.panes.get(&pane))
            .and_then(|st| self.editor.window(st.chat_win))
            .and_then(|w| w.viewport)
            .map(|v| (v.width, v.height))
            .unwrap_or((80, 24));
        let term = crate::term::Term::attach(handle, node, pty, &name, rows, cols);
        self.terminals.insert(pane, term);
        self.draw_terminal(view, pane);
    }

    /// Which pane is drawing a given machine's pty, if any.
    ///
    /// A linear walk over the panes rather than a map keyed by handle, because there are as many
    /// terminals as somebody has opened panes and a second map to keep in step is a second map to
    /// get wrong when a pane closes. Both halves of the pair are compared: a handle is minted per
    /// machine and two machines can perfectly well both be on `t3`.
    fn pane_of_pty(
        &self,
        node: &neosh_proto::NodeId,
        pty: &str,
    ) -> Option<(neosh_proto::ViewId, neosh_proto::PaneId)> {
        self.views.iter().find_map(|(view, v)| {
            v.panes
                .keys()
                .find(|pane| {
                    self.terminals.get(**pane).and_then(|t| t.remote())
                        == Some((node, pty))
                })
                .map(|pane| (*view, *pane))
        })
    }

    /// Every pane drawing a shell on `node`. See [`Self::pane_of_pty`] for why this is a walk.
    fn panes_of_node(
        &self,
        node: &neosh_proto::NodeId,
    ) -> Vec<(neosh_proto::ViewId, neosh_proto::PaneId)> {
        self.views
            .iter()
            .flat_map(|(view, v)| v.panes.keys().map(move |pane| (*view, *pane)))
            .filter(|(_, pane)| {
                self.terminals.get(*pane).and_then(|t| t.remote()).is_some_and(|(n, _)| n == node)
            })
            .collect()
    }

    /// Take a pane away without the ceremony closing one usually has.
    ///
    /// For the one case that is not somebody closing a pane: a terminal that never started. There
    /// is nothing in it to lose, nothing to confirm, and the message saying why has already been
    /// said — what is left is to stop drawing a rectangle that will never have anything in it.
    fn close_pane_quietly(&mut self, view: neosh_proto::ViewId, pane: neosh_proto::PaneId) {
        self.terminals.remove(pane);
        self.editor.pane_close(view, pane);
        if let Some(v) = self.views.get_mut(&view) {
            v.panes.remove(&pane);
        }
        self.sync_panes();
        self.refresh_status();
    }

    /// A peer wants a shell here. Answer exactly once, whatever happens.
    ///
    /// The node has already refused this if `accepts_shells` is off — it does that rather than
    /// waking the host, for the reason it refuses a command to a read-only node — so reaching here
    /// means the setting says yes and what is left is whether a pty can actually be had.
    fn run_swarm_pty_open(
        &mut self,
        node: neosh_proto::NodeId,
        id: String,
        cwd: Option<String>,
        cols: u16,
        rows: u16,
    ) {
        let Some(handle) = self.swarm_node.clone() else { return };
        // A directory that is not there is the ordinary failure here — a worktree removed by hand,
        // a project moved — and it has to be *named*, because `posix_spawn` reports it exactly as
        // it reports a missing shell. The home directory is the fallback rather than an error:
        // `None` means "wherever" and a caller that named nowhere should get a prompt.
        let dir = cwd
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_dir())
            .or_else(|| std::env::var_os("HOME").map(std::path::PathBuf::from))
            .unwrap_or_else(|| std::path::PathBuf::from("/"));
        self.swarm_next_pty += 1;
        let pty = format!("t{}", self.swarm_next_pty);
        let result = crate::term::Serving::spawn(
            &dir,
            rows,
            cols,
            handle.clone(),
            node.clone(),
            pty.clone(),
        );
        match result {
            Ok(serving) => {
                let name = self
                    .swarm
                    .peer(&node)
                    .map(|p| p.display_name())
                    .unwrap_or_else(|| node.short().to_string());
                // Said out loud on this machine, once per shell. Somebody else has just opened a
                // prompt here with this user's credentials, and a workspace that let that happen
                // silently would be one where the setting is the only evidence it ever did.
                self.editor_message(
                    MessageLevel::Info,
                    format!("{name} opened a shell here in {}", dir.display()),
                );
                self.swarm_serving.insert(pty.clone(), (node.clone(), serving));
                handle.send(neosh_swarm::SwarmRequest::PtyOpened { node, id, result: Ok(pty) });
            }
            Err(e) => {
                handle.send(neosh_swarm::SwarmRequest::PtyOpened {
                    node,
                    id,
                    result: Err(neosh_proto::Refusal::Failed { message: e.to_string() }),
                });
            }
        }
    }

    /// Close every shell this machine is running for `node`.
    ///
    /// The cleanup nothing else does. A shell has no timeout and no idea the link it was opened
    /// over has gone, so without this a laptop closing its lid leaves a login shell running here
    /// until the workspace stops — with that user's environment, in one of their directories,
    /// reachable by nothing.
    fn close_shells_for(&mut self, node: &neosh_proto::NodeId) {
        // `Serving`'s `Drop` kills the child, so taking them out of the map is the whole of it.
        self.swarm_serving.retain(|_, (owner, _)| owner != node);
    }

    /// Paint whatever the shell has drawn, if it has drawn anything since last time.
    fn draw_terminal(&mut self, view: neosh_proto::ViewId, pane: neosh_proto::PaneId) {
        let Some(t) = self.terminals.get(pane) else { return };
        let cells = t.cells();
        let at = t.cursor();
        let win = self.pane_window(view, pane);
        let Some(surface) = self.editor.surface_on(win) else { return };
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply_in(view, &plugin, ApiCall::SurfacePut { surface, cells });
        // The caret goes where the *shell* put it. The frontend draws it from the window's cursor,
        // and a terminal window's buffer is empty — so without this the caret sits at the top-left
        // of the pane while you type at a prompt further down, which reads as a terminal that is
        // not receiving anything.
        if let Some((row, col)) = at {
            let _ = self.editor.apply_in(view, &plugin, ApiCall::WinSetCursor {
                win,
                row: row as u32,
                col: col as u32,
            });
        }
    }

    /// Give a terminal's buffer somewhere for the caret to be.
    ///
    /// A surface paints cells and a *caret* is placed from the buffer underneath — the frontend
    /// resolves it against the rows a window actually rendered, so an empty buffer is a window with
    /// nowhere to put one, and the caret sat wherever the fallback left it while you typed at a
    /// prompt further down. Spaces rather than empty lines, because the column is clamped to the
    /// length of the row it lands on and every row of an empty buffer is zero long.
    ///
    /// Rewritten on claim and on resize only. It is a scaffold, not content: the surface is drawn
    /// over every cell of it, and nothing reads it back.
    fn scaffold_terminal(
        &mut self,
        view: neosh_proto::ViewId,
        pane: neosh_proto::PaneId,
        rows: u16,
        cols: u16,
    ) {
        let Some(buf) = self.views.get(&view).and_then(|v| v.panes.get(&pane)).map(|p| p.chat)
        else {
            return;
        };
        let plugin = PluginId::from(BUILTIN);
        let line = " ".repeat(cols as usize);
        let _ = self.editor.apply_in(view, &plugin, ApiCall::BufSetLines {
            buf,
            start: 0,
            end: -1,
            lines: vec![line; rows.max(1) as usize],
        });
    }

    fn pane_window(
        &self,
        view: neosh_proto::ViewId,
        pane: neosh_proto::PaneId,
    ) -> WindowId {
        self.views
            .get(&view)
            .and_then(|v| v.panes.get(&pane))
            .map(|p| p.chat_win)
            .unwrap_or_default()
    }

    /// Push every shell that has drawn something since the last frame, and reap the dead.
    ///
    /// Polled rather than pushed: the reader threads set a flag, and this is the one place that
    /// turns flags into frames. A channel per terminal waking the loop would be the same work with
    /// a wake-up per byte of output, which for a build log is thousands a second.
    fn tick_terminals(&mut self) -> bool {
        let mut drew = false;
        let live: Vec<(neosh_proto::ViewId, neosh_proto::PaneId)> = self
            .views
            .iter()
            .flat_map(|(v, s)| s.panes.values().filter(|p| p.term).map(move |p| (*v, p.pane)))
            .collect();
        for (view, pane) in live {
            let dirty = self.terminals.get(pane).is_some_and(|t| t.take_dirty());
            if !dirty {
                continue;
            }
            self.draw_terminal(view, pane);
            drew = true;
            // A shell that has exited leaves a pane showing its last frame forever. Said once, in
            // the pane, rather than closing it: what it printed before it went is often the reason
            // you would want to look.
            if self.terminals.get_mut(pane).is_some_and(|t| t.finished()) {
                self.terminals.remove(pane);
                self.in_pane(view, pane, |me| {
                    me.editor_message(MessageLevel::Info, "the shell exited — <C-w>q closes the pane");
                });
            }
        }
        drew
    }

    /// Point the keyboard at the active pane, and put the mode back to what that pane was in.
    ///
    /// Two facts that are per pane meeting one that is per view. `home` is the window keys go to at
    /// rest, so it has to follow the active pane or every binding on `neosh.composer` resolves
    /// against a field in the half of the screen you are not in. The *mode* is the editor's and
    /// there is one per terminal — so a pane you left while reading has to put `Normal` back when
    /// you return to it, or `j` types a letter into a transcript.
    fn rehome(&mut self, view: neosh_proto::ViewId) {
        let Some(pane) = self.editor.active_pane_of(view) else { return };
        let Some(state) = self.views.get(&view).and_then(|v| v.panes.get(&pane)) else { return };
        let (composer_win, chat_win) = (state.composer_win, state.chat_win);
        let (reading, was) = (state.reading, state.mode_before_reading);
        let session = state.session.clone();

        // **Moving the keyboard into a pane is arriving in the conversation it is reading.**
        //
        // Which model answers, how hard it thinks, what the context meter is measuring and what
        // `⇧⇥` sets are all facts about a *conversation*, and the store keeps one "current" that
        // every one of them reads. Nothing told it the keyboard had moved — so with two chats side
        // by side, `^P` showed the model of whichever pane you last *entered a conversation in*,
        // `^E` opened that one's options, and the footer named its model while you typed into the
        // other. The pane you are in is the conversation you are in; there is no third thing for
        // these to be about.
        //
        // Guarded, because `sync_panes` runs after every apply and `enter` is a history: moving to
        // the front of the recency order on every redraw would make the sidebar's ordering mean
        // "most recently drawn" rather than "most recently arrived in".
        if self.views.get(&view).and_then(|v| v.entered.as_ref()) != Some(&session) {
            if let Some(v) = self.views.get_mut(&view) {
                v.entered = Some(session.clone());
            }
            let _ = self.in_pane(view, pane, |me| me.arrive_in(&session));
        }

        self.editor.set_home(view, Some(composer_win));
        let want = if reading { Mode::Normal } else { was };
        if self.editor.mode(view) != want {
            self.editor.set_mode(view, want);
        }
        // Reading is a place *in* a pane, and the focus stack is per view — so arriving in a pane
        // that was left mid-read has to put the transcript back on top of it, and arriving in one
        // that was not has to take it off. Without this, moving from a reading pane to a composing
        // one left the keys going to a transcript in the other half of the screen.
        let plugin = PluginId::from(BUILTIN);
        let focused = self.editor.focused(view);
        if reading && focused != Some(chat_win) {
            let _ = self.editor.apply_in(view, &plugin, ApiCall::FocusPush { win: chat_win });
        } else if !reading && focused == Some(chat_win) {
            let _ = self.editor.apply_in(view, &plugin, ApiCall::FocusPop);
        }
    }

    // ---- the swarm -------------------------------------------------------

    /// Start a node, if the configuration asks for one.
    ///
    /// Off unless somebody said otherwise: a workspace must not open a port because it was
    /// upgraded. Every failure here is a warning rather than an error — a swarm that cannot start
    /// is a feature that is unavailable, not a reason the editor should refuse to run.
    fn start_swarm(&mut self, cfg: &crate::config::SwarmConfig) {
        if !cfg.enabled {
            return;
        }
        // The swarm runs whether or not anything was configured — that is what makes "add this
        // computer" from another machine work on a fresh install. The difference `configured`
        // makes is only how loud to be when it cannot run.
        let configured = !cfg.peers.is_empty() || cfg.listen.is_some();
        let Some(state) = self.state_dir.clone() else {
            if configured {
                self.editor_message(
                    MessageLevel::Warn,
                    "the swarm needs somewhere to keep its key, and --clean has nowhere",
                );
            }
            return;
        };

        let path = neosh_swarm::identity::key_path(&state);
        let identity = match neosh_swarm::Identity::load_or_create(&path) {
            Ok(i) => i,
            Err(e) => {
                self.editor_message(MessageLevel::Error, format!("swarm identity: {e}"));
                return;
            }
        };

        // Whatever pairing has remembered, plus whatever the config declared. The two are kept
        // apart by `Source`, so a machine added here can be removed from the UI and one written by
        // hand cannot — that line would simply come back on the next reload.
        let allowed = neosh_swarm::Allowed::load(Some(&state));
        let mut peers = Vec::new();
        for (id, p) in allowed.list() {
            // A row on the board from the first frame, so a machine being dialled reads as
            // "connecting" rather than as a silence to explain.
            self.swarm.seed(id.clone(), p.name.clone(), p.addr.is_some());
            // And under the name this user gave it, from that same first frame: a rename that only
            // takes effect once the machine has answered is one that looks lost across a restart.
            self.swarm.set_alias(&id, p.alias.clone());
            if let Some(addr) = p.addr {
                peers.push(neosh_swarm::PeerAddress { addr, expect: Some(id) });
            }
        }
        for p in &cfg.peers {
            let name = p.name.clone().unwrap_or_else(|| p.addr.clone());
            match &p.id {
                Some(id) => {
                    let id = neosh_proto::NodeId(id.clone());
                    allowed.add(id.clone(), neosh_swarm::AllowedPeer {
                        name: name.clone(),
                        // A config peer's name is the config's to set, which is also why
                        // `swarm.rename` refuses one: this file is reloaded and an alias stored
                        // beside it would be a rename that lasts until `^R`.
                        alias: None,
                        addr: Some(p.addr.clone()),
                        source: neosh_swarm::Source::Config,
                    });
                    self.swarm.seed(id.clone(), name, true);
                    peers.push(neosh_swarm::PeerAddress {
                        addr: p.addr.clone(),
                        expect: Some(id),
                    });
                }
                // An address with no key is not an authorisation. Dialled anyway, so the machine
                // there shows up as somebody asking to pair — which is the useful thing to do with
                // a half-written entry, rather than refusing to look at it.
                None => {
                    peers.push(neosh_swarm::PeerAddress { addr: p.addr.clone(), expect: None });
                    self.editor_message(
                        MessageLevel::Info,
                        format!("swarm peer {} has no id yet — ^J to pair with it", p.addr),
                    );
                }
            }
        }
        self.swarm_allowed = allowed.clone();

        let listen = match cfg.listen.as_deref() {
            // The default: reachable, so adding this computer from another one works before
            // anything is configured. Being reachable is not being joined — pairing still takes a
            // yes on both machines — and a taken port downgrades to dial-only with a warning.
            None => Some(std::net::SocketAddr::from(([0, 0, 0, 0], 7717))),
            // Dial-only, by choice. The empty string is the off switch rather than a missing
            // key, because missing now means the default.
            Some("") => None,
            Some(s) => match s.parse::<std::net::SocketAddr>() {
                Ok(a) => Some(a),
                Err(e) => {
                    self.editor_message(
                        MessageLevel::Warn,
                        format!("swarm.listen is not an address: {e}"),
                    );
                    None
                }
            },
        };

        if let Some(name) = &cfg.name {
            self.swarm_name = name.clone();
        }
        self.swarm_listen_configured = cfg.listen.as_deref().is_some_and(|s| !s.is_empty());
        let node_cfg = neosh_swarm::SwarmConfig {
            name: self.swarm_name.clone(),
            listen,
            peers,
            accepts_commands: cfg.accepts_commands,
            accepts_approvals: cfg.accepts_approvals,
            accepts_shells: cfg.accepts_shells,
            heartbeat: Duration::from_secs(cfg.heartbeat_secs.max(1)),
        };
        let (handle, events) = neosh_swarm::spawn(
            std::sync::Arc::new(identity),
            allowed,
            node_cfg,
            env!("CARGO_PKG_VERSION").to_string(),
        );
        // Only when somebody wrote a `[swarm]` section. On by default means this would
        // otherwise be a line every workspace prints at every start, about a feature the
        // person may never have met — the id is in `^J`, where the question gets asked.
        if configured {
            self.editor_message(
                MessageLevel::Info,
                format!("swarm: this node is {}", handle.id().short()),
            );
        }
        self.swarm_node = Some(handle);
        self.swarm_events = Some(events);
    }

    /// A fact about this workspace, published where every plugin can read it and hear it change —
    /// the same channel `question.asking` travels on, and for the same reason: it is about the
    /// workspace, not about whichever panel happened to learn it.
    fn set_global_var(&mut self, key: &str, value: serde_json::Value) {
        let scope = neosh_proto::VarScope::Global;
        self.vars.set(&scope, key.to_string(), value.clone());
        self.broadcast(PluginEvent::VarChanged {
            scope,
            key: key.to_string(),
            value: Some(value),
        });
    }

    fn swarm_self(&self) -> Option<neosh_proto::NodeInfo> {
        let h = self.swarm_node.as_ref()?;
        Some(neosh_proto::NodeInfo {
            id: h.id().clone(),
            name: self.swarm_name.clone(),
            os: std::env::consts::OS.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        })
    }

    /// Whose conversation this is, when it is another machine's.
    fn mirror_meta(&self, session: &neosh_proto::SessionId) -> Option<neosh_proto::MirrorOf> {
        self.agent.sessions().get(session).and_then(|s| s.mirror.clone())
    }

    /// The conversation here that is a window onto `(node, remote)`, if one is open.
    fn mirror_of(
        &self,
        node: &neosh_proto::NodeId,
        remote: &neosh_proto::SessionId,
    ) -> Option<neosh_proto::SessionId> {
        self.agent
            .sessions()
            .iter()
            .find(|s| s.mirror.as_ref().is_some_and(|m| &m.node == node && &m.session == remote))
            .map(|s| s.id.clone())
    }

    /// Find or make the local conversation that shows another machine's.
    ///
    /// Made from what that machine last said about it — its title and its directory, which is a
    /// path on *that* disk and is only ever shown — with no messages: those arrive as the history
    /// the subscription sends, and a guess drawn first would be replaced in front of you.
    fn open_mirror(
        &mut self,
        node: &neosh_proto::NodeId,
        remote: &neosh_proto::SessionId,
    ) -> Result<neosh_proto::SessionId, ApiError> {
        if let Some(local) = self.mirror_of(node, remote) {
            return Ok(local);
        }
        let Some(peer) = self.swarm.peer(node) else {
            return Err(ApiError::NotFound { what: "that computer is not paired".into() });
        };
        let Some(agent) = peer.agents.iter().find(|a| &a.session == remote) else {
            return Err(ApiError::NotFound {
                what: format!("no such conversation on {}", peer.display_name()),
            });
        };
        let machine = peer.display_name();
        let mut s = neosh_agent::Session::new(std::path::PathBuf::from(&agent.cwd));
        s.title = Some(agent.label.clone());
        s.created_at = now_secs();
        s.updated_at = agent.updated_at;
        // What that machine says the conversation is using, so the footer, `^P` and `⇧⇥` open on
        // the answer over there rather than on whatever this machine would have chosen.
        s.selection = remote_selection(agent);
        s.permission_mode = agent.mode;
        s.usage = agent.usage.clone();
        s.context_tokens = agent.context_tokens;
        s.context_window = agent.context_window;
        s.mirror = Some(neosh_proto::MirrorOf {
            node: node.clone(),
            session: remote.clone(),
            machine,
        });
        let id = s.id.clone();
        self.agent.sessions().insert(s);
        // Asked as the conversation opens rather than when `^P` is pressed, so the footer is right
        // on the first frame after it arrives and the picker opens on a list already here.
        self.ask_catalogue(node, false);
        Ok(id)
    }

    /// Ask a machine for its catalogue. One at a time per machine, unless it is a refresh.
    fn ask_catalogue(&mut self, node: &neosh_proto::NodeId, refresh: bool) {
        if !self.swarm.peer(node).is_some_and(|p| p.up() && p.capabilities.catalogue) {
            return;
        }
        let Some(handle) = self.swarm_node.clone() else { return };
        self.swarm_next_command += 1;
        let id = format!("m{}", self.swarm_next_command);
        self.swarm_asking.insert(id.clone(), SwarmAsk::Catalogue);
        handle.send(neosh_swarm::SwarmRequest::Catalogue { node: node.clone(), id, refresh });
    }

    /// Answer a model-picker call from the catalogue of the machine whose conversation is on
    /// screen, when it is another machine's and that machine can say. `true` when it was taken.
    ///
    /// The whole of "`^P` in another machine's conversation looks like `^P` in one of yours": the
    /// model plugin asks what it always asks — the providers, their models, the selection — and is
    /// answered about the machine the conversation runs on. No plugin had to learn what a mirror
    /// is, and nothing it draws can come to disagree with the machine it is drawing.
    fn answer_from_catalogue(&mut self, plugin: &PluginId, id: &RequestId, call: &ApiCall) -> bool {
        let refresh = match call {
            ApiCall::AgentListModels { refresh, .. } => *refresh,
            ApiCall::ProviderCredentials => false,
            _ => return false,
        };
        let Some(m) = self.mirror_meta(&self.active_session()) else { return false };
        // An older neosh cannot say, and a disconnected one cannot be asked: what this machine
        // has is the answer it always gave.
        if !self.swarm.peer(&m.node).is_some_and(|p| p.capabilities.catalogue) {
            return false;
        }
        if !refresh
            && let Some(catalogue) = self.catalogues.get(&m.node)
        {
            let answer = catalogue_answer(catalogue, call);
            self.respond_plugin(plugin.clone(), id.clone(), answer);
            return true;
        }
        if !self.swarm.peer(&m.node).is_some_and(|p| p.up()) {
            return false;
        }
        let waiting = self.catalogue_waiting.entry(m.node.clone()).or_default();
        let first = waiting.is_empty();
        waiting.push((plugin.clone(), id.clone(), call.clone()));
        if first || refresh {
            self.ask_catalogue(&m.node, refresh);
        }
        true
    }

    /// A catalogue arrived — or will not, which is `None`.
    fn took_catalogue(
        &mut self,
        node: neosh_proto::NodeId,
        catalogue: Option<neosh_proto::ModelCatalogue>,
    ) {
        let waiting = self.catalogue_waiting.remove(&node).unwrap_or_default();
        match catalogue {
            Some(catalogue) => {
                self.catalogues.insert(node.clone(), catalogue);
                // A mirror whose owner never said which provider serves its model — an older one,
                // or a conversation that has not chosen — is named from the list it serves now.
                self.name_mirror_instances(&node);
                let catalogue = self.catalogues.get(&node).cloned().unwrap_or_default();
                for (plugin, id, call) in waiting {
                    let answer = catalogue_answer(&catalogue, &call);
                    self.respond_plugin(plugin, id, answer);
                }
                // The footer drew before this was here; it redraws on a selection changing.
                if let Some(selection) = self.agent.selection() {
                    self.broadcast(PluginEvent::SelectionChanged { selection });
                }
            }
            None => {
                for (plugin, id, call) in waiting {
                    if !self.spawn_slow(&plugin, Some(id.clone()), &call) {
                        let services = self.services(&plugin);
                        let bridge = self.bridge.clone();
                        tokio::spawn(async move {
                            let response: ApiResponse = run_slow(services, call).await.into();
                            let _ = bridge.script().send(ScriptInbound::Plugin {
                                plugin,
                                msg: PluginInbound::Response { id, response },
                            });
                        });
                    }
                }
            }
        }
    }

    /// Fill in the provider of any mirror of `node` that only knows its model's name.
    fn name_mirror_instances(&mut self, node: &neosh_proto::NodeId) {
        let Some(catalogue) = self.catalogues.get(node) else { return };
        let mut store = self.agent.sessions();
        let ids: Vec<neosh_proto::SessionId> = store
            .iter()
            .filter(|s| s.mirror.as_ref().is_some_and(|m| &m.node == node))
            .map(|s| s.id.clone())
            .collect();
        for id in ids {
            let Some(s) = store.get_mut(&id) else { continue };
            let Some(sel) = s.selection.as_mut() else { continue };
            if !sel.instance.0.is_empty() {
                continue;
            }
            if let Some(e) = catalogue.models.iter().find(|e| e.model.id == sel.model) {
                sel.instance = e.instance.clone();
            }
        }
    }

    /// Another machine wants our catalogue: its model picker is about to show it.
    fn serve_catalogue(&mut self, node: neosh_proto::NodeId, id: String, refresh: bool) {
        let credentials = self.agent.providers().credentials();
        let services = self.services(&PluginId::from(BUILTIN));
        let Some(handle) = self.swarm_node.clone() else { return };
        tokio::spawn(async move {
            // Off the loop: an endpoint that has never been asked what it serves is a network
            // round trip, and every one after that is this machine's cache.
            let models = match services.list_models(None, refresh).await {
                Ok(ApiOk::Models { models }) => models,
                _ => Vec::new(),
            };
            handle.send(neosh_swarm::SwarmRequest::Catalogued {
                node,
                id,
                result: Ok(neosh_proto::ModelCatalogue { credentials, models }),
            });
        });
    }

    /// Answer a plugin's call, from the loop.
    fn respond_plugin(&self, plugin: PluginId, id: RequestId, answer: Result<ApiOk, ApiError>) {
        let response: ApiResponse = answer.into();
        let _ = self.bridge.script().send(ScriptInbound::Plugin {
            plugin,
            msg: PluginInbound::Response { id, response },
        });
    }

    /// Keep every mirror of `node`'s conversations saying what that machine says about them: the
    /// title, the model and the mode, which can each be changed at either end. A roster rather than
    /// a message, so a rename at that keyboard reaches this footer on the next inventory.
    fn follow_mirrors(&mut self, node: &neosh_proto::NodeId) {
        let Some(peer) = self.swarm.peer(node) else { return };
        let agents = peer.agents.clone();
        let mut moved = false;
        let mut reselected = false;
        {
            let mut store = self.agent.sessions();
            let mirrors: Vec<(neosh_proto::SessionId, neosh_proto::SessionId)> = store
                .iter()
                .filter_map(|s| {
                    s.mirror.as_ref().filter(|m| &m.node == node).map(|m| (s.id.clone(), m.session.clone()))
                })
                .collect();
            for (local, remote) in mirrors {
                let Some(agent) = agents.iter().find(|a| a.session == remote) else { continue };
                let Some(s) = store.get_mut(&local) else { continue };
                let title = Some(agent.label.clone());
                let mut selection = remote_selection(agent).or_else(|| s.selection.clone());
                // An older owner names no provider; keep the one its catalogue told us.
                if let (Some(new), Some(old)) = (selection.as_mut(), s.selection.as_ref())
                    && new.instance.0.is_empty()
                    && new.model == old.model
                {
                    new.instance = old.instance.clone();
                }
                let mode = agent.mode.or(s.permission_mode);
                if s.selection != selection {
                    reselected = true;
                }
                if s.title != title || s.selection != selection || s.permission_mode != mode {
                    s.title = title;
                    s.selection = selection;
                    s.permission_mode = mode;
                    moved = true;
                }
                // The meter, when that machine can say — never over a turn streaming here, which
                // is moving it already.
                if agent.context_tokens > 0 && s.active_turn.is_none() {
                    if s.context_tokens != agent.context_tokens || s.context_window != agent.context_window {
                        s.context_tokens = agent.context_tokens;
                        if agent.context_window.is_some() {
                            s.context_window = agent.context_window;
                        }
                        moved = true;
                    }
                }
            }
        }
        if moved {
            self.refresh_status();
        }
        // Chosen at that keyboard, or by `^P` here coming back as that machine's answer: either
        // way the footer and the picker read the selection, and redraw when it changes.
        if reselected && let Some(selection) = self.agent.selection() {
            self.broadcast(PluginEvent::SelectionChanged { selection });
        }
    }

    /// Stop streaming a mirror nothing shows, and forget it. It is that machine's conversation, and
    /// opening it again asks for it afresh.
    fn let_go_of_mirror(&mut self, m: &neosh_proto::MirrorOf) {
        if let Some(h) = &self.swarm_node {
            h.send(neosh_swarm::SwarmRequest::Unsubscribe {
                node: m.node.clone(),
                session: m.session.clone(),
            });
        }
        if let Some(local) = self.mirror_of(&m.node, &m.session) {
            // Only when nothing shows it: `enter` drops a mirror the pane left, and a pane closed
            // some other way is caught by `prune_mirrors`.
            if self.panes_showing(&local).is_empty() {
                let up: Vec<String> = self
                    .relayed
                    .iter()
                    .filter(|(_, r)| r.local == local)
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in up {
                    self.unrelay(&id);
                }
                let _ = self.agent.sessions().remove(&local);
                self.turns.remove(&local);
                self.rounds.remove(&local);
                self.mirror_whole.remove(&local);
                self.mirror_drawn.remove(&local);
                // The copies of its pictures were only ever for drawing it here.
                if self.mirror_pictures.remove(&local).is_some() {
                    let _ = std::fs::remove_dir_all(self.mirror_picture_dir(&local));
                }
            }
        }
    }

    /// Every mirror no pane is showing, let go. On the roster clock, so a pane closed by anything
    /// at all — `<C-w>q`, a tab, a terminal going away — stops its stream within seconds.
    fn prune_mirrors(&mut self) {
        let unseen: Vec<neosh_proto::MirrorOf> = self
            .agent
            .sessions()
            .iter()
            .filter_map(|s| s.mirror.clone().map(|m| (s.id.clone(), m)))
            .collect::<Vec<_>>()
            .into_iter()
            .filter(|(id, _)| self.panes_showing(id).is_empty())
            .map(|(_, m)| m)
            .collect();
        for m in unseen {
            self.let_go_of_mirror(&m);
        }
    }

    /// Where the copies of a mirror's pictures go: its own directory, removed when it is let go.
    fn mirror_picture_dir(&self, local: &neosh_proto::SessionId) -> std::path::PathBuf {
        self.image_store().join("remote").join(&local.0)
    }

    /// Ask the owner for every picture this mirror's transcript names that there is no copy of
    /// here yet — in its messages, and in what the turn in flight has run.
    ///
    /// A path in the owner's transcript is a file on the owner's disk, and drawn here as it stood it
    /// was the picture's *name*: a screenshot the agent read, or the one somebody pasted over there,
    /// was on every screen but the one watching.
    fn localise_pictures(&mut self, local: &neosh_proto::SessionId) {
        let Some(m) = self.mirror_meta(local) else { return };
        if !self.swarm.peer(&m.node).is_some_and(|p| p.up() && p.capabilities.live) {
            return;
        }
        let mut named = self
            .agent
            .sessions()
            .get(local)
            .map(|s| pictures_in(&s.messages))
            .unwrap_or_default();
        if let Some(r) = self.rounds.get(local) {
            for item in &r.said {
                if let Said::Tool { result: Some(res), .. } = item {
                    named.extend(res.images.iter().map(|i| i.path.clone()));
                }
            }
        }
        let pics = self.mirror_pictures.entry(local.clone()).or_default();
        let mine: std::collections::HashSet<String> =
            pics.have.values().map(|f| f.path.clone()).collect();
        let wanted: Vec<String> = named
            .into_iter()
            .filter(|p| !mine.contains(p) && !pics.have.contains_key(p))
            .filter(|p| pics.asked.insert(p.clone()))
            .collect();
        for path in wanted {
            self.command_mirror_as(&m, neosh_proto::AgentCommand::Picture { path }, SwarmAsk::Picture);
        }
    }

    /// A picture the owner sent, because this mirror asked for it: kept, and every place the
    /// transcript names it pointed at the copy.
    fn took_picture(&mut self, local: &neosh_proto::SessionId, path: String, media_type: String, data: &str) {
        use base64::Engine as _;
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) else { return };
        let dir = self.mirror_picture_dir(local);
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let ext = media_type.strip_prefix("image/").unwrap_or("png");
        let file = dir.join(format!("{}.{ext}", uuid::Uuid::new_v4()));
        if std::fs::write(&file, bytes).is_err() {
            return;
        }
        let copy = neosh_proto::ImageFile { path: file.to_string_lossy().into_owned(), media_type };
        let pics = self.mirror_pictures.entry(local.clone()).or_default();
        pics.asked.remove(&path);
        pics.have.insert(path, copy);
        let have = pics.have.clone();
        if let Some(s) = self.agent.sessions().get_mut(local) {
            repoint_messages(&mut s.messages, &have);
        }
        if let Some(r) = self.rounds.get_mut(local) {
            for item in &mut r.said {
                if let Said::Tool { result: Some(res), .. } = item {
                    repoint_files(&mut res.images, &have);
                }
            }
        }
        // Drawn again, where you are: a picture is rows, and the rows it takes are only known once
        // it is here.
        self.each_view_showing(local, |me| me.resync_transcript());
    }

    /// Ask again for every conversation of `node`'s that is open here.
    fn resubscribe_mirrors(&mut self, node: &neosh_proto::NodeId) {
        let Some(h) = self.swarm_node.clone() else { return };
        let remotes: Vec<neosh_proto::SessionId> = self
            .agent
            .sessions()
            .iter()
            .filter_map(|s| s.mirror.as_ref().filter(|m| &m.node == node).map(|m| m.session.clone()))
            .collect();
        for session in remotes {
            h.send(neosh_swarm::SwarmRequest::Subscribe { node: node.clone(), session });
        }
    }

    /// Stop drawing every turn of `node`'s that is running here, without ending it: nothing is
    /// known about how it ended, and the history that arrives on the next subscribe is the whole
    /// truth about it — including, when it is still going, the part of it drawn so far.
    fn drop_mirror_turns(&mut self, node: &neosh_proto::NodeId) {
        let running: Vec<neosh_proto::SessionId> = self
            .agent
            .sessions()
            .iter()
            .filter(|s| s.mirror.as_ref().is_some_and(|m| &m.node == node))
            .map(|s| s.id.clone())
            .filter(|id| self.turns.contains_key(id))
            .collect();
        for local in running {
            self.each_view_showing(&local, |me| {
                me.close_answer();
                me.end_working();
                me.vm().streaming = None;
            });
            self.turns.remove(&local);
            self.rounds.remove(&local);
            self.mirror_whole.remove(&local);
            if let Some(s) = self.agent.sessions().get_mut(&local) {
                s.active_turn = None;
                s.turn_started_at = None;
            }
            self.each_view_showing(&local, |me| me.refresh_composer());
        }
    }

    /// The turn running in `session`, as far as this machine has drawn it — what a machine that
    /// opens the conversation now is owed besides its messages. See [`neosh_proto::LiveTurn`].
    fn live_turn(&self, session: &neosh_proto::SessionId) -> Option<neosh_proto::LiveTurn> {
        if !self.turns.contains_key(session) {
            return None;
        }
        let (turn, started_at) = {
            let store = self.agent.sessions();
            let s = store.get(session)?;
            let turn = s.active_turn.clone()?;
            (turn.0, s.turn_started_at.or(s.running_since).unwrap_or_else(now_secs))
        };
        let round = self.rounds.get(session);
        let said = round
            .map(|r| {
                r.said
                    .iter()
                    .map(|item| match item {
                        Said::Text(text) => neosh_proto::LiveSaid::Text { text: text.clone() },
                        Said::Tool { call, result } => neosh_proto::LiveSaid::Tool {
                            call: call.clone(),
                            result: result.clone(),
                        },
                    })
                    .collect()
            })
            .unwrap_or_default();
        let plan = round.map(|r| r.plan.clone()).unwrap_or_default();
        Some(neosh_proto::LiveTurn { turn, started_at, said, plan })
    }

    /// Send a command to the machine a mirror belongs to, and say so if it refuses.
    fn command_mirror(&mut self, m: &neosh_proto::MirrorOf, command: neosh_proto::AgentCommand) {
        self.command_mirror_as(m, command, SwarmAsk::Mirror);
    }

    /// [`Self::command_mirror`], filed under `ask` so the answer is read the right way.
    fn command_mirror_as(
        &mut self,
        m: &neosh_proto::MirrorOf,
        command: neosh_proto::AgentCommand,
        ask: SwarmAsk,
    ) {
        // A command only a newer neosh can parse, to one that cannot: an unknown tag fails the
        // frame and takes the link with it, which is a worse answer than this sentence.
        let newer = matches!(
            command,
            neosh_proto::AgentCommand::Answer { .. } | neosh_proto::AgentCommand::SetMode { .. }
        );
        // Newer still, and asked for by nobody in particular: a node that cannot read the tag is
        // simply not asked, and the transcript goes on showing the picture's name.
        if matches!(command, neosh_proto::AgentCommand::Picture { .. })
            && !self.swarm.peer(&m.node).is_some_and(|p| p.capabilities.live)
        {
            return;
        }
        if newer && !self.swarm.peer(&m.node).is_some_and(|p| p.capabilities.rich_stream) {
            self.editor_message(
                MessageLevel::Warn,
                format!("{} runs an older neosh that cannot take that — update it there", m.machine),
            );
            return;
        }
        // A machine that is not connected drops a command on the floor rather than refusing it, and
        // a message that went nowhere without a word is the worst answer there is.
        if !self.swarm.peer(&m.node).is_some_and(|p| p.up()) {
            self.editor_message(
                MessageLevel::Warn,
                format!("{} is not connected — c on its row in ^T reconnects", m.machine),
            );
            return;
        }
        let Some(handle) = &self.swarm_node else {
            self.editor_message(MessageLevel::Warn, "the swarm is not running");
            return;
        };
        self.swarm_next_command += 1;
        let id = format!("c{}", self.swarm_next_command);
        self.swarm_asking.insert(id.clone(), ask);
        handle.send(neosh_swarm::SwarmRequest::Command {
            node: m.node.clone(),
            id,
            session: m.session.clone(),
            command,
        });
    }

    /// What you typed into another machine's conversation, sent there.
    ///
    /// The question is drawn when that machine says it was asked (`StreamEvent::Asked`), which is
    /// the one order everybody watching sees it in — mid-turn it is steered in at the next gap, and
    /// drawing it here first would put it above an answer it was never part of. A machine too old to
    /// say so has it drawn here straight away instead, since it never will.
    fn send_to_mirror(
        &mut self,
        local: &neosh_proto::SessionId,
        m: &neosh_proto::MirrorOf,
        prompt: Prompt,
    ) {
        // The pictures go as bytes, to a machine that reads them — the owner keeps them the way it
        // keeps one pasted there, and says where in the `Asked` that draws the question here. As
        // many as one frame carries; the rest, and all of them for an older machine, are said.
        let takes = self.swarm.peer(&m.node).is_some_and(|p| p.capabilities.live);
        let mut images: Vec<neosh_proto::ImageData> = Vec::new();
        let mut left = 0usize;
        let mut room = PICTURE_BUDGET;
        for f in picture_files(&prompt.images) {
            use base64::Engine as _;
            let data = match std::fs::read(&f.path) {
                Ok(bytes) if takes => base64::engine::general_purpose::STANDARD.encode(bytes),
                _ => {
                    left += 1;
                    continue;
                }
            };
            if data.len() > room {
                left += 1;
                continue;
            }
            room -= data.len();
            images.push(neosh_proto::ImageData { media_type: f.media_type, data });
        }
        if left > 0 {
            let what = if left == 1 { "a picture".to_string() } else { format!("{left} pictures") };
            let why = if takes { "too big to send" } else { "runs an older neosh" };
            self.editor_message(
                MessageLevel::Warn,
                format!("{what} stayed on this computer — {} {why}", m.machine),
            );
        }
        if prompt.text.trim().is_empty() && images.is_empty() {
            return;
        }
        // Nothing drawn for a message that is not going anywhere; `command_mirror` says why, and
        // what you wrote goes back in the field rather than into the void.
        if !self.swarm.peer(&m.node).is_some_and(|p| p.up()) {
            self.command_mirror(m, neosh_proto::AgentCommand::Send {
                text: prompt.text.clone(),
                images: Vec::new(),
            });
            if *local == self.active_session() {
                self.set_composer(&prompt.text);
                self.refresh_composer();
            }
            return;
        }
        let rich = self.swarm.peer(&m.node).is_some_and(|p| p.capabilities.rich_stream);
        if !rich {
            let text = prompt.text.clone();
            if let Some(s) = self.agent.sessions().get_mut(local) {
                s.push_user(&Prompt::text(text), Some(now_secs()));
            }
            let shown = Prompt::text(prompt.text.clone());
            self.each_view_showing(local, |me| {
                me.scroll_chat(0);
                me.chat_question(&shown);
            });
        }
        self.each_view_showing(local, |me| me.refresh_composer());
        self.command_mirror(m, neosh_proto::AgentCommand::Send { text: prompt.text, images });
    }

    /// A turn over there, begun here: the round the working line and the cards are drawn from.
    ///
    /// Called by the first event of a turn rather than only by `TurnStarted`, because a
    /// conversation opened mid-turn joins it at a token.
    fn mirror_turn(&mut self, local: &neosh_proto::SessionId, turn: neosh_proto::TurnId) {
        self.mirror_turn_drawn(local, turn, true);
    }

    /// [`Self::mirror_turn`], leaving the drawing to the caller when `draw` is off — a turn taken
    /// whole from a history is drawn by the rebuild that follows, working line and all.
    fn mirror_turn_drawn(
        &mut self,
        local: &neosh_proto::SessionId,
        turn: neosh_proto::TurnId,
        draw: bool,
    ) {
        if self.turns.contains_key(local) {
            return;
        }
        self.turns
            .insert(local.clone(), Running { cancel: CancellationToken::new(), cancelling: false });
        self.rounds.insert(local.clone(), Round {
            verb: a_verb(),
            started: std::time::Instant::now(),
            note: None,
            note_is_wait: false,
            said: Vec::new(),
            changes: Vec::new(),
            plan: Vec::new(),
            tasks: Vec::new(),
        });
        if let Some(s) = self.agent.sessions().get_mut(local) {
            s.active_turn = Some(turn.clone());
            s.turn_started_at = Some(now_secs());
        }
        if draw {
            self.each_view_showing(local, |me| {
                me.scroll_chat(0);
                me.begin_working();
                me.refresh_composer();
            });
        }
        self.on_agent_event(AgentEvent::TurnStarted { session: local.clone(), turn });
    }

    /// A conversation's history arriving from the machine it belongs to — when it opens, when a
    /// turn there ends, and whenever anybody else starts watching it.
    ///
    /// Taken as the truth about the conversation, and drawn only when it says something this
    /// screen does not already show. A turn watched here from its first event was drawn by the
    /// same code that drew it over there, closing rows and all — its plan, what it changed, what it
    /// left running — and rebuilding from the messages at its end is what took every one of those
    /// back off the screen, which is exactly the difference between the two machines a person
    /// sitting at both could see.
    fn mirror_history(
        &mut self,
        node: &neosh_proto::NodeId,
        local: &neosh_proto::SessionId,
        messages: Vec<neosh_proto::Message>,
        live: Option<neosh_proto::LiveTurn>,
        interrupted: bool,
    ) {
        // Whether "nothing running" is something the owner said, or only something it left out.
        let says_live = self.swarm.peer(node).is_some_and(|p| p.capabilities.live);
        let drawn = self.mirror_drawn.remove(local);
        // The pictures already here, pointed at before anything compares: the copy here names the
        // copies, and the owner's names its own files.
        let mut messages = messages;
        let mut live = live;
        if let Some(pics) = self.mirror_pictures.get(local) {
            repoint_messages(&mut messages, &pics.have);
            if let Some(l) = live.as_mut() {
                for item in &mut l.said {
                    if let neosh_proto::LiveSaid::Tool { result: Some(r), .. } = item {
                        repoint_files(&mut r.images, &pics.have);
                    }
                }
            }
        }
        let changed = {
            let mut store = self.agent.sessions();
            let Some(s) = store.get_mut(local) else { return };
            let changed = s.messages != messages || s.interrupted != interrupted;
            s.messages = messages;
            s.interrupted = interrupted;
            changed
        };
        match live {
            Some(l) => {
                // The turn over there as far as it has got, which replaces whatever this screen
                // drew of it: that may have started half way through, and this did not.
                let turn = neosh_proto::TurnId(l.turn.clone());
                self.mirror_turn_drawn(local, turn.clone(), false);
                let said: Vec<Said> = l
                    .said
                    .into_iter()
                    .map(|item| match item {
                        neosh_proto::LiveSaid::Text { text } => Said::Text(text),
                        neosh_proto::LiveSaid::Tool { call, result } => Said::Tool { call, result },
                    })
                    .collect();
                let mut changes: Vec<cards::FileStat> = Vec::new();
                for item in &said {
                    if let Said::Tool { call, result: Some(r) } = item
                        && !r.is_error
                    {
                        cards::tally(&mut changes, &cards::edits_of(&call.input));
                    }
                }
                let ago = u64::try_from(now_secs().saturating_sub(l.started_at)).unwrap_or(0);
                if let Some(r) = self.rounds.get_mut(local) {
                    r.said = said;
                    r.plan = l.plan;
                    r.changes = changes;
                    r.started = std::time::Instant::now()
                        .checked_sub(std::time::Duration::from_secs(ago))
                        .unwrap_or_else(std::time::Instant::now);
                }
                if let Some(s) = self.agent.sessions().get_mut(local) {
                    s.active_turn = Some(turn);
                    s.turn_started_at = Some(l.started_at);
                }
                self.mirror_whole.insert(local.clone());
                self.each_view_showing(local, |me| me.resync_transcript());
            }
            None if self.turns.contains_key(local) => {
                // An owner too old to say what is running, mid-turn: the turn is being drawn live,
                // and the re-sync at its end puts the rest right.
                if !says_live {
                    return;
                }
                // Nothing is running over there, so its ending never reached this machine — a
                // link that dropped between the two. Put down without drawing an ending nobody
                // here saw, and drawn from what was recorded.
                self.turns.remove(local);
                self.rounds.remove(local);
                self.mirror_whole.remove(local);
                if let Some(s) = self.agent.sessions().get_mut(local) {
                    s.active_turn = None;
                    s.turn_started_at = None;
                }
                self.each_view_showing(local, |me| {
                    me.resync_transcript();
                    me.refresh_composer();
                });
                self.refresh_status();
            }
            None => {
                if changed && !drawn {
                    self.each_view_showing(local, |me| me.resync_transcript());
                }
            }
        }
    }

    /// Rebuild the transcript on screen from the conversation's messages and the turn in flight,
    /// leaving where you are alone — the scroll, the cursor, the draft. [`Self::enter_session`]
    /// without the arriving.
    fn resync_transcript(&mut self) {
        if self.v().term {
            return;
        }
        self.vm().answer = None;
        self.vm().streaming = None;
        self.vm().unanswered = None;
        self.vm().working = false;
        self.vm().plan_rows = 0;
        self.redraw_transcript();
        // The buffer was replaced whole, so neither the welcome nor anything else it drew is
        // still where it was.
        self.vm().welcome_rows = 0;
        self.draw_welcome();
        self.replay_round();
    }

    /// One event of another machine's conversation, drawn in the one here that mirrors it.
    ///
    /// Translated into the agent's own events and handed to the same handler a turn here goes
    /// through, which is the whole design: the transcript, the cards, the working line, the timing
    /// and the notifications are drawn by one piece of code whichever machine the agent is on, so a
    /// conversation over there cannot come to look different from one here.
    fn on_mirror_stream(
        &mut self,
        node: &neosh_proto::NodeId,
        remote: &neosh_proto::SessionId,
        event: &neosh_proto::StreamEvent,
    ) {
        use neosh_proto::StreamEvent as S;
        let Some(local) = self.mirror_of(node, remote) else { return };
        let turn = |t: &str| neosh_proto::TurnId(t.to_string());
        match event.clone() {
            S::History { messages, live, interrupted } => {
                self.mirror_history(node, &local, messages, live, interrupted);
                self.localise_pictures(&local);
            }
            S::TurnStarted { turn: t } => {
                // From its first event, so everything it draws here is drawn by the path that drew
                // it over there — which is what lets its end leave the screen alone.
                if !self.turns.contains_key(&local) {
                    self.mirror_turn(&local, turn(&t));
                    self.mirror_whole.insert(local);
                }
            }
            S::Token { turn: t, text } => {
                self.mirror_turn(&local, turn(&t));
                self.on_agent_event(AgentEvent::Token { session: local, turn: turn(&t), text });
            }
            S::Thinking { turn: t, text } => {
                self.mirror_turn(&local, turn(&t));
                self.on_agent_event(AgentEvent::Thinking { session: local, turn: turn(&t), text });
            }
            S::Activity { turn: t, activity } => {
                self.mirror_turn(&local, turn(&t));
                self.on_agent_event(AgentEvent::Activity {
                    session: local,
                    turn: turn(&t),
                    activity,
                });
            }
            S::ToolStarted { turn: t, call } => {
                self.mirror_turn(&local, turn(&t));
                self.on_agent_event(AgentEvent::ToolStarted { session: local, turn: turn(&t), call });
            }
            S::ToolFinished { turn: t, call, mut result } => {
                self.mirror_turn(&local, turn(&t));
                if let Some(pics) = self.mirror_pictures.get(&local) {
                    repoint_files(&mut result.images, &pics.have);
                }
                let pictures = !result.images.is_empty();
                self.on_agent_event(AgentEvent::ToolFinished {
                    session: local.clone(),
                    turn: turn(&t),
                    call,
                    result,
                });
                if pictures {
                    self.localise_pictures(&local);
                }
            }
            S::Asked { text, images, pictures } => {
                // The pictures themselves when the owner said where they are — asked for below and
                // drawn when they land — and a sentence standing in for them when it did not.
                let shown = match (images, pictures.is_empty()) {
                    (_, false) | (0, _) => text.clone(),
                    (1, _) => format!("{text}\n[a picture, on the other computer]"),
                    (n, _) => format!("{text}\n[{n} pictures, on the other computer]"),
                };
                let mut pictures = pictures;
                if let Some(pics) = self.mirror_pictures.get(&local) {
                    repoint_files(&mut pictures, &pics.have);
                }
                let blocks: Vec<neosh_proto::ContentBlock> = pictures
                    .into_iter()
                    .map(|f| neosh_proto::ContentBlock::Image { path: f.path, media_type: f.media_type })
                    .collect();
                let asked_pictures = !blocks.is_empty();
                let running = self.turns.contains_key(&local);
                // Steered in mid-turn: over there, what the turn had said so far was committed
                // before the question was taken in, and a rebuild here — a switch away and back —
                // has to find it above the question rather than replayed under it.
                let before = if running {
                    self.rounds
                        .get_mut(&local)
                        .map(|r| said_messages(std::mem::take(&mut r.said), now_secs()))
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                let prompt = Prompt { text: shown, images: blocks };
                if let Some(s) = self.agent.sessions().get_mut(&local) {
                    s.messages.extend(before);
                    s.push_user(&prompt, Some(now_secs()));
                }
                self.each_view_showing(&local, |me| {
                    me.scroll_chat(0);
                    me.chat_question(&prompt);
                    if running {
                        me.draw_working();
                    }
                    me.refresh_composer();
                });
                if asked_pictures {
                    self.localise_pictures(&local);
                }
            }
            S::TurnEnded { turn: t, stop_reason, usage } => {
                if self.turns.contains_key(&local) {
                    // Drawn whole and live, so the screen already says what the owner's does and
                    // the history this ending brings needs no rebuild. A turn joined half way
                    // through, or streamed by a machine that sends no tool events, does.
                    let rich = self.swarm.peer(node).is_some_and(|p| p.capabilities.rich_stream);
                    if self.mirror_whole.remove(&local) && rich {
                        self.mirror_drawn.insert(local.clone());
                    }
                    if let Some(s) = self.agent.sessions().get_mut(&local) {
                        s.active_turn = None;
                        s.turn_started_at = None;
                    }
                    self.on_agent_event(AgentEvent::TurnEnded {
                        session: local,
                        turn: turn(&t),
                        stop_reason,
                        usage,
                    });
                }
                // And ask for the whole conversation again, which is what makes the copy here the
                // same as the one over there: what was drawn live is what happened, and the history
                // is what it was *recorded* as — the thing a redraw, a resize or a switch rebuilds
                // from. An older machine that sends no tool events is put right by exactly this.
                if let Some(h) = &self.swarm_node {
                    h.send(neosh_swarm::SwarmRequest::Subscribe {
                        node: node.clone(),
                        session: remote.clone(),
                    });
                }
            }
            S::Question { id, questions } => {
                self.relay_here(&local, node, id, crate::relay::Offer::Question(questions));
            }
            S::Permission { id, title, capability, options } => {
                let offer = crate::relay::Offer::Permission { title, capability, options };
                self.relay_here(&local, node, id, offer);
            }
            S::Settled { id } => self.unrelay(&id),
            S::Image { path, media_type, data } => {
                self.took_picture(&local, path, media_type, &data);
            }
            S::Notice { level, text } => {
                let machine = self
                    .agent
                    .sessions()
                    .get(&local)
                    .and_then(|s| s.mirror.as_ref().map(|m| m.machine.clone()))
                    .unwrap_or_default();
                self.editor_message(level, format!("{machine}: {text}"));
            }
            S::Blocked { prompt, .. } => {
                let machine = self
                    .agent
                    .sessions()
                    .get(&local)
                    .and_then(|s| s.mirror.as_ref().map(|m| m.machine.clone()))
                    .unwrap_or_default();
                self.editor_message(
                    MessageLevel::Info,
                    format!("waiting for somebody at {machine}: {prompt}"),
                );
            }
        }
    }

    /// A prompt from another machine's conversation, put in front of the person here.
    ///
    /// Through the very hooks a prompt of this machine's own goes through — the questions panel,
    /// the approvals picker — raised for the mirror, so it opens in the terminal reading it and
    /// looks exactly like one from an agent on this computer. Nothing is decided here: that
    /// machine's policy already said to ask, and this machine's mode is about its own
    /// conversations.
    fn relay_here(
        &mut self,
        local: &neosh_proto::SessionId,
        node: &neosh_proto::NodeId,
        id: String,
        offer: crate::relay::Offer,
    ) {
        use crate::relay::{Offer, Relay, Reply};
        // Offered again to every watcher when another one subscribes.
        if self.relayed.contains_key(&id) {
            return;
        }
        let machine = self
            .agent
            .sessions()
            .get(local)
            .and_then(|s| s.mirror.as_ref().map(|m| m.machine.clone()))
            .unwrap_or_default();
        let cwd = self.session_cwd(local);
        let tx = self.relay_tx.clone();
        let task = match &offer {
            Offer::Question(questions) => {
                // Nothing here to show it with. Said, and left to that machine to answer: sending
                // back "nobody answered" would be this machine answering for a person it never
                // asked.
                if self.agent.hooks().blocking(neosh_proto::HookName::AskUser).next().is_none() {
                    let first = questions.first().map(|q| q.question.clone()).unwrap_or_default();
                    self.editor_message(
                        MessageLevel::Info,
                        format!("{machine} is asking: {first} — answer it there"),
                    );
                    None
                } else {
                    let asker = neosh_agent::DriverQuestioner::new(&self.agent, self.bridge.clone());
                    let request = neosh_provider::ask::QuestionRequest {
                        questions: questions.clone(),
                        conversation: local.clone(),
                        cwd,
                    };
                    let id = id.clone();
                    Some(tokio::spawn(async move {
                        use neosh_provider::ask::QuestionAnswers as A;
                        let answers = match neosh_provider::ask::QuestionAsker::ask(&asker, request)
                            .await
                        {
                            A::Answered(a) => Some(a),
                            // Dismissed is an answer — `Esc` there would have said the same.
                            A::Cancelled { reason } if reason == A::dismissed_reason() => None,
                            // Timed out, or the panel went away: nobody here answered, which is
                            // not the same as answering "nobody".
                            A::Cancelled { .. } => return,
                        };
                        let _ = tx.send(Relay::Answered { id, reply: Reply::Answers(answers) });
                    }))
                }
            }
            Offer::Permission { title, capability, options } => {
                let sentence = title.clone().unwrap_or_else(|| crate::relay::describe(capability));
                // Asked of the *capability* the owner advertised, before a picker is drawn whose
                // answer would be refused: a yes is a write to that machine's disk, and it has to
                // have said it takes them from here.
                let approves = self.swarm.peer(node).is_some_and(|p| p.capabilities.accepts_approvals);
                let hooked =
                    self.agent.hooks().blocking(neosh_proto::HookName::PermissionPre).next().is_some();
                if !approves || !hooked {
                    let why = if approves { "answer it there" } else {
                        "answer it there, or set `accepts_approvals = true` under `[swarm]` on it"
                    };
                    self.editor_message(
                        MessageLevel::Info,
                        format!("{machine} is asking: {sentence} — {why}"),
                    );
                    None
                } else {
                    let asker = neosh_agent::DriverAsker::new(&self.agent, self.bridge.clone());
                    let request = neosh_provider::approval::PermissionRequest {
                        title: sentence,
                        capability: capability.clone(),
                        options: options.clone(),
                        cwd,
                        conversation: Some(local.clone()),
                    };
                    let id = id.clone();
                    Some(tokio::spawn(async move {
                        let answer = asker.prompt(request).await;
                        let _ = tx.send(Relay::Answered { id, reply: Reply::Approve(answer) });
                    }))
                }
            }
        };
        self.relayed.insert(id, Relayed {
            local: local.clone(),
            offer,
            task: task.map(|t| t.abort_handle()),
        });
    }

    /// Another machine's prompt was settled, wherever it was: take it down here.
    fn unrelay(&mut self, id: &str) {
        let Some(r) = self.relayed.remove(id) else { return };
        if let Some(task) = r.task {
            task.abort();
            self.broadcast(PluginEvent::Event {
                name: "neosh.prompt.withdrawn".to_string(),
                data: Some(crate::relay::withdrawn(&r.local, &r.offer)),
                from: BUILTIN.to_string(),
            });
        }
    }

    /// News from the relay. See [`crate::relay`].
    fn on_relay(&mut self, relay: crate::relay::Relay) {
        use crate::relay::{Relay, Reply};
        match relay {
            Relay::Opened { id, session, offer, reply } => {
                self.stream_out(&session, offer_event(&id, &offer));
                self.offered.insert(id, Offered { session, reply, offer });
            }
            Relay::Closed { id, session, offer, elsewhere } => {
                self.offered.remove(&id);
                self.stream_out(&session, neosh_proto::StreamEvent::Settled { id });
                // Answered on another machine, so the panel here is still up over a prompt that
                // has gone — and a key pressed in it would answer nothing.
                if elsewhere {
                    self.broadcast(PluginEvent::Event {
                        name: "neosh.prompt.withdrawn".to_string(),
                        data: Some(crate::relay::withdrawn(&session, &offer)),
                        from: BUILTIN.to_string(),
                    });
                }
            }
            Relay::Answered { id, reply } => {
                // Settled while the person here was answering: nothing is waiting for it.
                let Some(r) = self.relayed.remove(&id) else { return };
                let mirror = self.agent.sessions().get(&r.local).and_then(|s| s.mirror.clone());
                let Some(m) = mirror else { return };
                let command = match reply {
                    Reply::Answers(answers) => neosh_proto::AgentCommand::Answer { id, answers },
                    Reply::Approve(answer) => {
                        use neosh_provider::approval::PermissionAnswer as P;
                        let (decision, option) = match answer {
                            P::Option(o) => (neosh_proto::PermissionDecision::Allow, Some(o)),
                            P::Allow => (neosh_proto::PermissionDecision::Allow, None),
                            P::Deny => (
                                neosh_proto::PermissionDecision::Deny { reason: "denied".into() },
                                None,
                            ),
                        };
                        neosh_proto::AgentCommand::Approve { decision, id: Some(id), option }
                    }
                };
                self.command_mirror_as(&m, command, SwarmAsk::Relay);
            }
        }
    }

    /// A watcher's answer to a prompt this machine offered.
    fn settle_offer(
        &mut self,
        session: &neosh_proto::SessionId,
        id: &str,
        reply: crate::relay::Reply,
    ) -> Result<Option<neosh_proto::SessionId>, neosh_proto::Refusal> {
        let fits = |o: &Offered| {
            &o.session == session
                && matches!(
                    (&o.offer, &reply),
                    (crate::relay::Offer::Question(_), crate::relay::Reply::Answers(_))
                        | (crate::relay::Offer::Permission { .. }, crate::relay::Reply::Approve(_))
                )
        };
        match self.offered.get(id) {
            Some(o) if fits(o) => {}
            Some(_) => {
                return Err(neosh_proto::Refusal::Failed {
                    message: "that is not what this conversation is waiting on".into(),
                });
            }
            None => {
                return Err(neosh_proto::Refusal::Failed {
                    message: "that has already been answered".into(),
                });
            }
        }
        // Removed here, so a second watcher answering in the same instant is told it was late.
        // `Closed` still arrives from the relay and says `Settled` to everybody.
        if let Some(o) = self.offered.remove(id) {
            let _ = o.reply.send(reply);
        }
        Ok(None)
    }

    /// Send a command to a peer and remember who is waiting for the answer.
    fn begin_swarm_command(
        &mut self,
        node: neosh_proto::NodeId,
        session: neosh_proto::SessionId,
        command: neosh_proto::AgentCommand,
        waiting: (PluginId, RequestId),
    ) {
        // Addressed at this machine, which is a thing to *do* rather than a thing to dial. A
        // fleet plugin iterating over `swarm.nodes()` reaches its own node like any other, and
        // sending ourselves a packet to ask ourselves a question would need the swarm to be
        // running before a plugin could steer the conversation it is sitting in.
        if self.swarm_self().is_some_and(|me| me.id == node) {
            let answer = self
                .apply_agent_command(&session, command)
                .map(|_| ApiOk::Unit)
                .map_err(|r| ApiError::Denied { reason: format!("{r:?}") });
            self.answer_swarm(waiting, answer);
            return;
        }
        let Some(handle) = &self.swarm_node else {
            self.answer_swarm(waiting, Err(ApiError::NotFound {
                what: "the swarm is not running".into(),
            }));
            return;
        };
        // Ids are per host rather than random: a counter is enough to correlate within one
        // connection, and it makes a packet capture readable.
        self.swarm_next_command += 1;
        let id = format!("c{}", self.swarm_next_command);
        self.swarm_pending.insert(id.clone(), waiting);
        self.swarm_asking.insert(id.clone(), SwarmAsk::Command);
        handle.send(neosh_swarm::SwarmRequest::Command { node, id, session, command });
    }

    /// Ask a peer which of its directories start with `prefix`, and remember who is waiting.
    ///
    /// Shares [`Self::swarm_next_command`] with the command path so the two id spaces cannot
    /// collide — which matters because a refusal comes back as a bare `Refused { id }` with nothing
    /// on it to say which question it is refusing, and the id is the only thing that tells them
    /// apart.
    fn begin_swarm_browse(
        &mut self,
        node: neosh_proto::NodeId,
        prefix: String,
        waiting: (PluginId, RequestId),
    ) {
        // This machine, reached the way any other is. A picker walking `swarm.nodes()` finds its
        // own node in the list, and asking ourselves over a socket would make a local directory
        // listing depend on the swarm being up — and fail outright with `--clean`, which has
        // nowhere to keep the key that is this machine's identity.
        if self.swarm_self().is_some_and(|me| me.id == node) {
            let svc = self.services(&PluginId::from(BUILTIN));
            let bridge = self.bridge.clone();
            let (plugin, id) = waiting;
            tokio::spawn(async move {
                let result = svc.path_complete(prefix).await;
                let response: ApiResponse = result.into();
                let _ = bridge.script().send(ScriptInbound::Plugin {
                    plugin,
                    msg: PluginInbound::Response { id, response },
                });
            });
            return;
        }
        let Some(handle) = &self.swarm_node else {
            self.answer_swarm(waiting, Err(ApiError::NotFound {
                what: "the swarm is not running".into(),
            }));
            return;
        };
        self.swarm_next_command += 1;
        let id = format!("b{}", self.swarm_next_command);
        self.swarm_pending.insert(id.clone(), waiting);
        self.swarm_asking.insert(id.clone(), SwarmAsk::Browse);
        handle.send(neosh_swarm::SwarmRequest::Browse { node, id, prefix });
    }

    /// A peer asked which of our directories start with `prefix`. Answer exactly once, whatever
    /// happens.
    ///
    /// Off the loop, because it is a `read_dir` and the loop is what draws. Directory names only —
    /// the same listing the local completer produces, which never opens a file and never says
    /// whether one is there.
    fn run_swarm_browse(&mut self, node: neosh_proto::NodeId, id: String, prefix: String) {
        let Some(handle) = self.swarm_node.as_ref() else { return };
        let handle = handle.clone();
        let svc = self.services(&PluginId::from(BUILTIN));
        tokio::spawn(async move {
            let result = match svc.path_complete(prefix).await {
                // A directory the *far* machine was not allowed to read comes back as a refusal
                // rather than as an empty listing, so browsing somebody else's Mac says what went
                // wrong instead of drawing a home directory with nothing in it. `Refusal::Failed`
                // already travels, so this costs no wire change and no version bump.
                Ok(ApiOk::Paths { denied: Some(message), .. }) => {
                    Err(neosh_proto::Refusal::Failed { message })
                }
                Ok(ApiOk::Paths { paths, .. }) => Ok(paths),
                Ok(_) => Ok(Vec::new()),
                Err(e) => Err(neosh_proto::Refusal::Failed { message: format!("{e:?}") }),
            };
            handle.send(neosh_swarm::SwarmRequest::Browsed { node, id, result });
        });
    }

    /// Find out what machine is at an address, off the loop.
    ///
    /// Given a short deadline of its own: the address came from somebody typing, and a wrong one is
    /// the ordinary case rather than the exceptional one. A dialog that hangs for the TCP default
    /// on a typo is a dialog people learn to be afraid of.
    fn begin_swarm_probe(&mut self, addr: String, waiting: (PluginId, RequestId)) {
        let Some(info) = self.swarm_self() else {
            self.answer_swarm(waiting, Err(ApiError::NotFound {
                what: "the swarm is not running".into(),
            }));
            return;
        };
        let Some(state) = self.state_dir.clone() else {
            self.answer_swarm(waiting, Err(ApiError::NotFound { what: "no state directory".into() }));
            return;
        };
        // A probe says hello and hangs up, so everything here is `false`: it is not offering to do
        // anything for the machine it is asking about, and a capability it advertised would be one
        // it is about to stop being able to honour.
        let caps = neosh_proto::NodeCapabilities {
            accepts_commands: false,
            accepts_approvals: false,
            streams: false,
            browse: false,
            shells: false,
            rich_stream: false,
            catalogue: false,
            live: false,
            projects: Vec::new(),
        };
        let bridge = self.bridge.clone();
        let (plugin, id) = waiting;
        tokio::spawn(async move {
            let result = async {
                let identity =
                    neosh_swarm::Identity::load_or_create(&neosh_swarm::identity::key_path(&state))
                        .map_err(|e| ApiError::Internal { message: e.to_string() })?;
                let found = tokio::time::timeout(
                    Duration::from_secs(5),
                    neosh_swarm::probe(&addr, &identity, &info, &caps),
                )
                .await
                .map_err(|_| ApiError::NotFound {
                    what: format!("nothing answered at {addr}"),
                })?
                .map_err(|e| ApiError::NotFound { what: format!("{addr}: {e}") })?;
                Ok(ApiOk::SwarmSelf { node: Some(found) })
            }
            .await;
            let response: ApiResponse = result.into();
            let _ = bridge.script().send(ScriptInbound::Plugin {
                plugin,
                msg: PluginInbound::Response { id, response },
            });
        });
    }

    fn answer_swarm(&self, waiting: (PluginId, RequestId), result: Result<ApiOk, ApiError>) {
        let (plugin, id) = waiting;
        let response: ApiResponse = result.into();
        let _ = self.bridge.script().send(ScriptInbound::Plugin {
            plugin,
            msg: PluginInbound::Response { id, response },
        });
    }

    /// Send one event to whoever is watching this conversation.
    ///
    /// The node drops it if nobody is; the set here is the cheaper half of the same question, asked
    /// before the event is built.
    fn stream_out(&self, session: &neosh_proto::SessionId, event: neosh_proto::StreamEvent) {
        if let Some(h) = &self.swarm_node {
            h.send(neosh_swarm::SwarmRequest::Stream { session: session.clone(), event });
        }
    }

    /// Whether anything would come of streaming this conversation.
    fn watched(&self, session: &neosh_proto::SessionId) -> bool {
        self.swarm_node.is_some() && self.swarm_watched.get(session).is_some_and(|w| !w.is_empty())
    }

    /// Everything this machine is running, as the wire describes it.
    ///
    /// Recomputed whole rather than tracked incrementally. A workspace has tens of conversations,
    /// not thousands, and a delta scheme whose bookkeeping can drift is a board that is subtly
    /// wrong on the machine you are not sitting at — which is the one case you cannot check.
    fn local_inventory(&mut self) -> Vec<neosh_proto::AgentSummary> {
        let sessions = self.agent.sessions().list_with(false);
        let mut marks = self.row_decorations();
        sessions
            .into_iter()
            .map(|s| {
                let info = self.named(s);
                let key = self.project_key(std::path::Path::new(&info.cwd));
                // This conversation's own, not the one on screen: every summary used to carry the
                // model of whatever this machine happened to be looking at.
                let selection = self
                    .agent
                    .sessions()
                    .get(&info.id)
                    .and_then(|s| s.selection.clone())
                    .or_else(|| self.agent.selection());
                let mode = self.agent.permission_mode(&info.id);
                let mut summary = crate::swarm::summarise(&info, key, selection.as_ref(), Some(mode));
                // Waiting on a person, which the store cannot know and a panel must: a row that
                // says *working* over a question is the one row in the list that most needs
                // finding, drawn as the nineteen that do not.
                if self.asking.contains(&info.id) || self.permitting.contains(&info.id) {
                    summary.state = neosh_proto::AgentState::Blocked;
                }
                summary.decorations = marks.remove(&format!("session:{}", info.id.0)).unwrap_or_default();
                summary
            })
            .collect()
    }

    /// Every `sidebar.decoration` contribution, by the row it is about — `project:<cwd>`,
    /// `session:<id>` — in the registry's order, which is the order a panel merges them in.
    ///
    /// What this machine's panel wears on each row, as data another machine's panel can wear too.
    /// The key is read the way `decorationKey` in `@neosh/api/ui` reads it: the target's first
    /// string field by name.
    fn row_decorations(&mut self) -> std::collections::HashMap<String, Vec<serde_json::Value>> {
        let plugin = PluginId::from(BUILTIN);
        let contributions =
            match self.editor.apply(&plugin, ApiCall::ExtList { point: "sidebar.decoration".into() }) {
                Ok(ApiOk::Contributions { contributions }) => contributions,
                _ => Vec::new(),
            };
        let mut out: std::collections::HashMap<String, Vec<serde_json::Value>> = Default::default();
        for c in contributions {
            let Some(target) = c.item.get("target").and_then(|t| t.as_object()) else { continue };
            let mut fields: Vec<(&String, &str)> =
                target.iter().filter_map(|(k, v)| v.as_str().map(|v| (k, v))).collect();
            fields.sort_by(|a, b| a.0.cmp(b.0));
            let Some((k, v)) = fields.first() else { continue };
            out.entry(format!("{k}:{v}")).or_default().push(c.item.clone());
        }
        out
    }

    /// What a directory is called across machines. See [`neosh_proto::ProjectKey`].
    ///
    /// Cached with the repository, because it is one `git remote get-url` and the answer does not
    /// change while a checkout is open.
    fn project_key(&self, cwd: &std::path::Path) -> neosh_proto::ProjectKey {
        if let Some(key) = self.project_keys.get(cwd) {
            return key.clone();
        }
        // Not cached: a directory whose remote has not been read yet would otherwise keep the
        // guess for ever, and `remember_repo` fills the real answer in a moment later.
        neosh_proto::ProjectKey::from_dir_name(
            &cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        )
    }


    /// The checkouts this machine can start something in, for a peer's "new conversation over
    /// there" menu.
    ///
    /// Never a scan of the disk: a list of every git repository on somebody's laptop is slow to
    /// produce and far more than a peer needs to know. It is the list this machine's own panel
    /// shows — the places you work — which is two sources rather than one.
    ///
    /// The live conversations are the obvious half and were once the whole of it, which made the
    /// answer wrong in a way that only showed up from the other machine: a project is a place you
    /// work rather than a property of the work in it, so a repository you cleared out this morning
    /// vanished from every other computer's "start something over there", and the checkout you have
    /// had open for a month but are not mid-turn in was never on it at all. It also made
    /// [`neosh_proto::RemoteProject::active`] a constant — everything on the list had a
    /// conversation, so everything said `true`.
    ///
    /// So the other half is `sidebar.projects`, the workspace var that *is* the panel's list. Read
    /// here for the same reason `question.asking` is: it is a fact about this workspace rather than
    /// about whichever panel happened to write it down, which is what a var is for. A workspace
    /// whose panel is switched off simply falls back to the conversations, which is what this
    /// answered before.
    fn local_projects(
        &mut self,
        agents: &[neosh_proto::AgentSummary],
    ) -> Vec<neosh_proto::RemoteProject> {
        let mut out = self.local_project_list(agents);
        let mut marks = self.row_decorations();
        for p in &mut out {
            p.decorations = marks.remove(&format!("project:{}", p.cwd)).unwrap_or_default();
        }
        out
    }

    /// [`Self::local_projects`], undecorated.
    fn local_project_list(
        &mut self,
        agents: &[neosh_proto::AgentSummary],
    ) -> Vec<neosh_proto::RemoteProject> {
        let mut out: Vec<neosh_proto::RemoteProject> = Vec::new();
        for a in agents {
            match out.iter_mut().find(|p| p.cwd == a.cwd) {
                Some(p) => {
                    p.sessions += 1;
                    p.running += u32::from(matches!(
                        a.state,
                        neosh_proto::AgentState::Running | neosh_proto::AgentState::Blocked
                    ));
                }
                None => out.push(neosh_proto::RemoteProject {
                    key: a.project.clone(),
                    name: a.project_name.clone(),
                    cwd: a.cwd.clone(),
                    repo_root: a.repo_root.clone(),
                    branch: a.branch.clone(),
                    // Which version of the repository this is, so a machine with the same checkout
                    // four commits behind can be told apart from one that is level. Off the files,
                    // not out of `git`: this runs on the inventory timer. See `neosh_vcs::read_head`.
                    head: neosh_vcs::read_head(std::path::Path::new(&a.cwd))
                        .and_then(|h| h.commit),
                    active: true,
                    sessions: 1,
                    running: u32::from(matches!(
                        a.state,
                        neosh_proto::AgentState::Running | neosh_proto::AgentState::Blocked
                    )),
                    decorations: Vec::new(),
                }),
            }
        }

        // And the quiet ones. `sidebar.name` is what the host called each directory when it last
        // had something in it — a worktree's `neosh (fix/thing)` rather than its `wt-fe3c0d93` —
        // so an emptied project keeps the name the other machine already knows it by.
        let known: Vec<String> = serde_json::from_value(
            self.vars.get(&neosh_proto::VarScope::Global, "sidebar.projects"),
        )
        .unwrap_or_default();
        for cwd in known {
            if out.iter().any(|p| p.cwd == cwd) {
                continue;
            }
            let path = std::path::Path::new(&cwd);
            // Only somewhere that is still there. A project on the list whose directory has been
            // deleted is a row on another machine that can only ever fail to open.
            if !path.is_dir() {
                continue;
            }
            let name = self
                .vars
                .get(&neosh_proto::VarScope::Project { cwd: cwd.clone() }, "sidebar.name")
                .as_str()
                .map(str::to_string)
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| {
                    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
                });
            // The same two facts a live conversation carries, out of the same cache: a tree with
            // nothing open in it is still a tree, and a panel that nests it only while somebody is
            // working in it is one that rearranges itself for a reason nobody can see.
            let facts = self.projects.get(path);
            // Read now rather than remembered: the branch in `facts` is the one this directory was
            // on the first time the host looked, and a checkout nobody is working in is exactly the
            // one somebody switched in a shell last week.
            let head = neosh_vcs::read_head(path);
            out.push(neosh_proto::RemoteProject {
                key: self.project_key(path),
                name,
                cwd,
                repo_root: facts.and_then(|f| f.repo_root.clone()),
                branch: match &head {
                    Some(h) => h.branch.clone(),
                    None => facts.and_then(|f| f.branch.clone()),
                },
                head: head.and_then(|h| h.commit),
                active: false,
                sessions: 0,
                running: 0,
                decorations: Vec::new(),
            });
        }

        // Somewhere being worked in first, and alphabetical inside that: the list is read by
        // somebody looking for where to start something, and "there is already an agent in here"
        // is the most useful thing a row can say about itself.
        out.sort_by(|a, b| b.active.cmp(&a.active).then_with(|| a.name.cmp(&b.name)));
        out
    }

    /// Tell every peer what changed here. Cheap enough to call on any session event.
    pub fn publish_inventory(&mut self) {
        if self.swarm_node.is_none() {
            return;
        }
        let agents = self.local_inventory();
        let mut keys = std::collections::HashMap::new();
        for a in &agents {
            keys.insert(a.project.clone(), a.project_name.clone());
        }
        self.swarm.set_local_projects(keys);
        let projects = self.local_projects(&agents);
        self.published = Some((agents.clone(), projects.clone()));
        if let Some(h) = &self.swarm_node {
            h.send(neosh_swarm::SwarmRequest::Publish { full: true, agents, gone: Vec::new() });
            h.send(neosh_swarm::SwarmRequest::Projects { projects });
        }
    }

    /// The same, but only when the answer has moved since the last time it was said.
    ///
    /// Called on a timer, so this is the comparison that keeps an idle workspace silent: two
    /// vectors of small structs against the two that were last sent, and nothing on the wire when
    /// they match. `AgentSummary` carries `updated_at` and `usage`, so a turn producing tokens
    /// moves it and peers watching a board see the count climb — which is the point of a roster
    /// rather than a snapshot.
    fn publish_inventory_if_changed(&mut self) {
        if self.swarm_node.is_none() {
            return;
        }
        let agents = self.local_inventory();
        let projects = self.local_projects(&agents);
        if self.published.as_ref().is_some_and(|(a, p)| a == &agents && p == &projects) {
            return;
        }
        self.publish_inventory();
    }

    /// One event from the swarm.
    pub fn on_swarm_event(&mut self, event: neosh_swarm::SwarmEvent) {
        use neosh_swarm::SwarmEvent as E;
        match event {
            E::Listening { addr } => {
                self.set_global_var("swarm.listen", serde_json::json!({ "addr": addr.to_string() }));
            }
            E::ListenFailed { addr, error } => {
                self.set_global_var(
                    "swarm.listen",
                    serde_json::json!({ "addr": null, "error": error.clone() }),
                );
                // Loud only for a listen somebody asked for by name. The default port being held
                // by another workspace on this machine is the ordinary case of running two, and
                // everything else — dialling, pairing outward — still works.
                if self.swarm_listen_configured {
                    self.editor_message(
                        MessageLevel::Warn,
                        format!("swarm could not listen on {addr}: {error}"),
                    );
                }
            }
            E::PeerUp { node, capabilities } => {
                let first = self.swarm.peer(&node.id).is_none_or(|p| !p.up());
                self.swarm.peer_up(node.clone(), capabilities);
                if first {
                    self.editor_message(
                        MessageLevel::Info,
                        format!("{} joined the swarm", node.name),
                    );
                    // It has just connected and knows nothing about us until we say so.
                    self.publish_inventory();
                    // Nor about what we were watching: a connection starts with no subscriptions,
                    // so every conversation of theirs open here would otherwise go on showing the
                    // moment the link dropped, and never move again.
                    self.resubscribe_mirrors(&node.id);
                }
                self.broadcast(PluginEvent::SwarmChanged);
            }
            E::PeerDown { node, reason, will_retry } => {
                let name = self
                    .swarm
                    .peer(&node)
                    .map(|p| p.info.name.clone())
                    .unwrap_or_else(|| node.short().to_string());
                // "Lost" is news about a machine that was here. A retry failing, a disconnect of
                // something already down — those move the board, not the corner.
                let was_up = self.swarm.peer(&node).is_some_and(|p| p.up());
                self.swarm.peer_down(&node, reason, will_retry);
                if was_up {
                    self.editor_message(MessageLevel::Warn, format!("lost {name}"));
                }
                // Nobody over there is reading anything of ours now. A new connection starts with
                // no subscriptions, so this is also what stops a conversation it had open counting
                // as read for the rest of the workspace's life.
                self.swarm_watched.retain(|_, w| {
                    w.remove(&node);
                    !w.is_empty()
                });
                // And a turn of theirs we were drawing will send no ending: the working line
                // would spin for ever over a transcript that stopped. Put down here and picked up
                // again, as it stands, when the link comes back and the mirror resubscribes.
                self.drop_mirror_turns(&node);
                // Every shell that machine opened here. `Serving`'s `Drop` kills the child, so
                // this is the whole of it — and it is the one cleanup nothing else would do.
                self.close_shells_for(&node);
                // And every shell of *theirs* we are drawing. The pane stays: what it last printed
                // is usually why you were looking, which is the same reason a local shell that
                // exits leaves its final frame up.
                for (view, pane) in self.panes_of_node(&node) {
                    if let Some(t) = self.terminals.get(pane) {
                        t.ended(&format!("{name} went away"));
                    }
                    self.draw_terminal(view, pane);
                }
                // Anything still waiting on that machine will never be answered by it.
                let orphaned: Vec<String> = self.swarm_pending.keys().cloned().collect();
                for id in orphaned {
                    self.swarm_asking.remove(&id);
                    if let Some(waiting) = self.swarm_pending.remove(&id) {
                        self.answer_swarm(waiting, Err(ApiError::NotFound {
                            what: format!("{name} went away before answering"),
                        }));
                    }
                }
                // A pane opened for a shell the machine will now never send. Settled through the
                // same path a refusal takes, so there is one place that decides what happens to a
                // terminal that never arrived.
                let unanswered: Vec<String> = self.swarm_opening.keys().cloned().collect();
                for id in unanswered {
                    if self.swarm_opening.get(&id).is_some_and(|(_, _, n)| *n == node) {
                        self.shell_answered(&id, Err(format!("{name} went away before answering")));
                    }
                }
                self.broadcast(PluginEvent::SwarmChanged);
            }
            E::DialFailed { node, attempt, error, .. } => {
                // Only a machine we can name has a row to update. An address with no identity
                // yet has nothing on the board — it will appear as a stranger the moment it
                // answers, and as a peer the moment it is paired.
                if let Some(id) = node {
                    self.swarm.dial_failed(&id, attempt, error);
                    self.broadcast(PluginEvent::SwarmChanged);
                }
            }
            E::Unapproved { node, capabilities, .. } => {
                // Said once per machine, not once per dial. The dialler retries at the heartbeat
                // for as long as pairing stays half-done — which can be the whole time somebody
                // walks to the other computer — and a notification every ten seconds about a state
                // that has not changed is the definition of noise. The panel row is the standing
                // record; this is the nudge that sends you to it.
                let first = !matches!(
                    self.swarm.peer(&node.id).map(|p| p.link.clone()),
                    Some(neosh_proto::LinkState::Waiting)
                );
                self.swarm.unapproved(node.clone(), capabilities);
                if first {
                    let name = self
                        .swarm
                        .peer(&node.id)
                        .map(|p| p.display_name())
                        .unwrap_or_else(|| node.name.clone());
                    self.editor_message(
                        MessageLevel::Info,
                        format!("{name} has not allowed this computer yet — press ^J over there"),
                    );
                }
                self.bridge.broadcast(PluginEvent::SwarmChanged);
            }
            E::Inventory { node, full, agents, gone } => {
                let before: Vec<neosh_proto::AgentSummary> =
                    self.swarm.peer(&node).map(|p| p.agents.clone()).unwrap_or_default();
                self.swarm.inventory(&node, full, agents, gone);
                self.follow_mirrors(&node);
                self.alert_remote_turns(&node, &before);
                self.broadcast(PluginEvent::SwarmChanged);
            }
            E::Stream { node, session, event } => {
                self.on_mirror_stream(&node, &session, &event);
                self.broadcast(PluginEvent::SwarmStream { node, session, event });
            }
            E::Stranger { node, dialled } => {
                let first = !self.swarm_strangers.contains_key(&node.id);
                // The address only means anything when we were the one dialling. For a machine
                // that dialled us, where it happened to arrive from is not somewhere we could
                // usefully dial back.
                let addr = dialled
                    .then(|| self.swarm_allowed.get(&node.id).and_then(|p| p.addr))
                    .flatten();
                let name = node.name.clone();
                self.swarm_strangers.insert(
                    node.id.clone(),
                    neosh_proto::SwarmStranger { info: node, dialled, addr },
                );
                // Said once. A machine that is dialling us every ten seconds must not produce a
                // notification every ten seconds.
                if first && !dialled {
                    self.editor_message(
                        MessageLevel::Info,
                        format!("{name} wants to join this workspace — ^J to allow it"),
                    );
                }
                self.broadcast(PluginEvent::SwarmChanged);
            }
            E::Answer { id, result, node } => {
                // A catalogue refused or unreachable: whoever was waiting on it is answered from
                // this machine's own, which is what a mirror of an older neosh has always shown.
                if self.swarm_asking.get(&id) == Some(&SwarmAsk::Catalogue) {
                    self.swarm_asking.remove(&id);
                    let _ = result;
                    self.took_catalogue(node, None);
                    return;
                }
                if self.swarm_asking.get(&id) == Some(&SwarmAsk::Picture) {
                    self.swarm_asking.remove(&id);
                    return;
                }
                if self.swarm_asking.get(&id) == Some(&SwarmAsk::Relay) {
                    self.swarm_asking.remove(&id);
                    // Somebody over there answered first, or the turn moved on: the prompt is
                    // gone either way and the panel has already come down. A refusal on
                    // *principle* is the one worth saying, because the next prompt will be
                    // refused as well.
                    if let Err(neosh_proto::Refusal::NotPermitted { what }) = result {
                        self.editor_message(
                            MessageLevel::Warn,
                            format!("the other computer did not take the answer: {what}"),
                        );
                    }
                    return;
                }
                if self.swarm_asking.get(&id) == Some(&SwarmAsk::Mirror) {
                    self.swarm_asking.remove(&id);
                    if let Err(r) = result {
                        self.editor_message(
                            MessageLevel::Warn,
                            format!("the other computer did not take it: {}", refusal_text(&r)),
                        );
                    }
                    return;
                }
                // A shell that was refused arrives here rather than as `PtyOpened`, because a
                // refusal says no to a question without describing it. The id is what tells them
                // apart, which is the whole reason `swarm_asking` exists.
                if self.swarm_asking.get(&id) == Some(&SwarmAsk::Shell) {
                    let why = match result {
                        Ok(_) => "answered a shell with an ack".to_string(),
                        Err(r) => refusal_text(&r),
                    };
                    self.shell_answered(&id, Err(why));
                    return;
                }
                let (ok, message, session) = match result {
                    Ok(session) => (true, String::new(), session),
                    Err(r) => (false, refusal_text(&r), None),
                };
                self.on_swarm_reply(&id, ok, message, session);
            }
            E::PtyOpen { node, id, cwd, cols, rows } => {
                self.run_swarm_pty_open(node, id, cwd, cols, rows);
            }
            E::PtyOpened { id, result, .. } => {
                self.shell_answered(&id, result.map_err(|r| refusal_text(&r)));
            }
            E::PtyData { node, pty, data } => {
                // Both directions arrive here, and which one this is decided by which map has the
                // handle: a shell we own is being typed into, and one we are watching has printed.
                // Never both — a handle is minted by one machine and known to two.
                if let Some((_, serving)) = self.swarm_serving.get_mut(&pty) {
                    serving.write(&data);
                    return;
                }
                if let Some((view, pane)) = self.pane_of_pty(&node, &pty) {
                    if let Some(t) = self.terminals.get(pane) {
                        t.feed(&data);
                    }
                    self.draw_terminal(view, pane);
                }
            }
            E::PtyResize { pty, cols, rows, .. } => {
                // Only ever about a shell we own: the side that draws is the side that decides how
                // big it is, so this never travels the other way.
                if let Some((_, serving)) = self.swarm_serving.get_mut(&pty) {
                    serving.resize(rows, cols);
                }
            }
            E::PtyClose { node, pty } => {
                if self.swarm_serving.remove(&pty).is_some() {
                    return;
                }
                if let Some((view, pane)) = self.pane_of_pty(&node, &pty) {
                    if let Some(t) = self.terminals.get(pane) {
                        t.ended("the shell was closed");
                    }
                    self.draw_terminal(view, pane);
                }
            }
            E::PtyExit { node, pty, status } => {
                if let Some((view, pane)) = self.pane_of_pty(&node, &pty) {
                    let why = match status {
                        Some(0) | None => "the shell exited".to_string(),
                        Some(code) => format!("the shell exited {code}"),
                    };
                    if let Some(t) = self.terminals.get(pane) {
                        t.ended(&why);
                    }
                    self.draw_terminal(view, pane);
                }
            }
            E::Browse { node, id, prefix } => self.run_swarm_browse(node, id, prefix),
            E::Catalogue { node, id, refresh } => self.serve_catalogue(node, id, refresh),
            E::Catalogued { node, id, catalogue } => {
                self.swarm_asking.remove(&id);
                self.took_catalogue(node, Some(catalogue));
            }
            E::Browsed { id, result, .. } => {
                self.on_swarm_browsed(&id, result.map_err(|r| refusal_text(&r)));
            }
            E::Subscribed { node, session } => {
                self.swarm_watched.entry(session.clone()).or_default().insert(node);
                // Opening a conversation from another machine is arriving in it: the answer that
                // made the row amber is on that screen now, and a mark that only a keyboard *here*
                // could clear would sit on every machine's panel until somebody walked over to this
                // one. Said at once rather than on the roster clock, because the row is what
                // somebody just pressed `↵` on.
                if self.agent.sessions().mark_read(&session) {
                    self.persist_session(&session);
                    self.publish_inventory();
                }
                // Everything said so far — on **every** subscribe, not only the first. A watcher
                // that only received deltas would show an empty conversation until the next token,
                // which for an idle agent is never; and "only the first" meant the second machine
                // to open a conversation, and every re-sync after a turn, got nothing at all. It
                // goes to everybody watching, who each take it as the truth about the whole
                // conversation, which it is.
                //
                // With the turn in flight, as far as it has got: the messages hold nothing of it
                // until it commits, which for an agent driver is when its whole loop is over — so a
                // machine opening a conversation mid-turn saw the question, no working line, and
                // nothing under it for as long as the turn ran.
                let (messages, interrupted) = self
                    .agent
                    .sessions()
                    .get(&session)
                    .map(|s| (s.messages.clone(), s.interrupted))
                    .unwrap_or_default();
                let live = self.live_turn(&session);
                let (messages, live) = crate::swarm::fit_history(messages, live);
                self.stream_out(&session, neosh_proto::StreamEvent::History {
                    messages,
                    live,
                    interrupted,
                });
                // And anything it is waiting on. A machine that opens a conversation mid-question
                // is very often opening it *because* of the question.
                let waiting: Vec<neosh_proto::StreamEvent> = self
                    .offered
                    .iter()
                    .filter(|(_, o)| o.session == session)
                    .map(|(id, o)| offer_event(id, &o.offer))
                    .collect();
                for event in waiting {
                    self.stream_out(&session, event);
                }
            }
            E::Unsubscribed { node, session } => {
                // Only that machine's interest: another may still be watching, and the node is
                // what decides who each event goes to anyway.
                if let Some(w) = self.swarm_watched.get_mut(&session) {
                    w.remove(&node);
                    if w.is_empty() {
                        self.swarm_watched.remove(&session);
                    }
                }
            }
            E::Command { node, id, session, command } => {
                self.run_swarm_command(node, id, session, command);
            }
        }
    }

    /// A peer asked us to do something. Answer exactly once, whatever happens.
    fn run_swarm_command(
        &mut self,
        node: neosh_proto::NodeId,
        id: String,
        session: neosh_proto::SessionId,
        command: neosh_proto::AgentCommand,
    ) {
        let result = self.apply_agent_command(&session, command);
        if let Some(h) = &self.swarm_node {
            h.send(neosh_swarm::SwarmRequest::Answer { node, id, result });
        }
    }

    /// Do one [`neosh_proto::AgentCommand`] to a conversation on this machine.
    ///
    /// The single implementation behind both ways of asking: a peer over ASCP, and a plugin here
    /// through [`ApiCall::AgentCommand`]. It was written for the first and reachable only from it,
    /// which is how the swarm API came to be able to do things to a conversation that the local
    /// API could not — see the docs on `ApiCall::AgentCommand`. One body means the two cannot
    /// drift, and that a refusal is refused for the same reason on both paths.
    ///
    /// Answers with the conversation it acted on when that is news, which is only
    /// [`neosh_proto::AgentCommand::NewSession`].
    fn apply_agent_command(
        &mut self,
        session: &neosh_proto::SessionId,
        command: neosh_proto::AgentCommand,
    ) -> Result<Option<neosh_proto::SessionId>, neosh_proto::Refusal> {
        use neosh_proto::AgentCommand as C;
        let session = session.clone();
        let known = self.agent.sessions().list_with(true).iter().any(|s| s.id == session);
        match command {
            C::NewSession { cwd, title } => {
                let dir = cwd
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| self.cwd.clone());
                if dir.is_dir() {
                    // The one just made, not whatever is on screen here: `new_session_titled`
                    // inserts without switching, so the active id is the conversation this machine
                    // happens to be looking at and the caller would be handed somebody else's.
                    let id = self.new_session_titled(dir, title);
                    self.publish_inventory();
                    Ok(Some(id))
                } else {
                    Err(neosh_proto::Refusal::Failed {
                        message: format!("{} is not a directory here", dir.display()),
                    })
                }
            }
            _ if !known => Err(neosh_proto::Refusal::NoSuchAgent),
            // Words, and pictures as bytes. A peer on another machine naming a path would be naming
            // a file on *its* disk, and this side would read whatever happened to be at that path
            // here — so what arrives is kept the way a picture pasted here is, and the prompt names
            // the copy.
            C::Send { text, images } => {
                let store = self.image_store();
                let mut blocks = Vec::new();
                for image in images {
                    use base64::Engine as _;
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(image.data.as_bytes())
                        .map_err(|_| neosh_proto::Refusal::Failed {
                            message: "a picture that is not base64".into(),
                        })?;
                    let kept = crate::images::from_bytes(&store, bytes)
                        .map_err(|message| neosh_proto::Refusal::Failed { message })?;
                    blocks.push(kept.block());
                }
                self.start_turn_in(session.clone(), Prompt { text, images: blocks });
                Ok(None)
            }
            // A picture this conversation's transcript names, and only one: the path is a file on
            // this disk, and a watcher that could name any other would be reading this machine
            // through a conversation.
            C::Picture { path } => {
                let named = {
                    let store = self.agent.sessions();
                    let mut named = store
                        .get(&session)
                        .map(|s| pictures_in(&s.messages))
                        .unwrap_or_default();
                    if let Some(r) = self.rounds.get(&session) {
                        for item in &r.said {
                            if let Said::Tool { result: Some(res), .. } = item {
                                named.extend(res.images.iter().map(|i| i.path.clone()));
                            }
                        }
                    }
                    named.contains(&path)
                };
                if !named {
                    return Err(neosh_proto::Refusal::NotPermitted {
                        what: "a picture this conversation does not show".into(),
                    });
                }
                let bytes = std::fs::read(&path).map_err(|e| neosh_proto::Refusal::Failed {
                    message: format!("could not read the picture: {e}"),
                })?;
                use base64::Engine as _;
                let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
                if data.len() > PICTURE_BUDGET {
                    return Err(neosh_proto::Refusal::Failed {
                        message: "the picture is too big to send".into(),
                    });
                }
                let media_type = crate::images::media_type_of(&bytes).unwrap_or("image/png");
                self.stream_out(&session, neosh_proto::StreamEvent::Image {
                    path,
                    media_type: media_type.to_string(),
                    data,
                });
                Ok(None)
            }
            C::Interrupt => {
                // The same idempotent ask `<Esc>` makes, and for the same reason: the entry stays
                // until the turn ends, so two peers interrupting once each is one interruption.
                if let Some(t) = self.turns.get_mut(&session)
                    && !t.cancelling
                {
                    t.cancelling = true;
                    t.cancel.cancel();
                }
                Ok(None)
            }
            C::Rename { title } => {
                let _ = self.agent.sessions().rename(&session, title);
                self.publish_inventory();
                Ok(None)
            }
            C::Archive { archived } => {
                let at = now_secs();
                let _ = self.agent.sessions().archive(&session, archived, at);
                self.publish_inventory();
                Ok(None)
            }
            C::SetModel { instance, model, options } => {
                let selection = neosh_proto::ModelSelection {
                    instance: neosh_proto::InstanceId(instance),
                    model: neosh_proto::ModelId(model),
                    // Only ever sent by a machine that chose them from *this* machine's catalogue
                    // (`AscpMessage::Catalogue`) — `^E` over there drew this machine's knobs — so
                    // they mean here what they meant there. An older node sends none, and the
                    // model's own defaults apply, which is what used to happen to every switch.
                    options,
                };
                // Checked here rather than taken on trust: the far end is naming a model from *its*
                // catalogue, and this machine may not have that provider configured at all. A
                // selection nothing can resolve would leave the conversation unable to take a turn,
                // with nothing on screen to say why.
                if self.agent.providers().resolve(&selection).is_none() {
                    Err(neosh_proto::Refusal::Failed {
                        message: format!(
                            "{}/{} is not a model this machine has",
                            selection.instance, selection.model
                        ),
                    })
                } else {
                    if let Some(s) = self.agent.sessions().get_mut(&session) {
                        s.selection = Some(selection.clone());
                    }
                    // A running turn is told, the same as it would be for `^P` here: the agent
                    // thinks the rest of it with the new model rather than finishing with the old.
                    for d in &self.agent_drivers {
                        d.set_model(&session, &selection);
                    }
                    self.publish_inventory();
                    Ok(None)
                }
            }
            // Declared in the protocol and refused by name, because on this host the prompt does
            // not belong to the host. Approvals are a *blocking plugin hook* — the approvals plugin
            // owns the question and the answer, and the host has nothing to say yes with. Answering
            // this properly means a plugin registering as the swarm's approver, which is a surface
            // that does not exist yet. A silent success would be a remote machine believing it had
            // authorised a write to this filesystem when it had not, so it says no instead.
            // One that names the prompt it answers: a watcher answering what this machine offered
            // it, which is the only kind of approval that has anything here waiting for it. The
            // node has already checked `accepts_approvals`.
            C::Approve { decision, id: Some(id), option } => {
                let answer = match (decision, option) {
                    (neosh_proto::PermissionDecision::Deny { .. }, _) => {
                        neosh_provider::approval::PermissionAnswer::Deny
                    }
                    (_, Some(o)) => neosh_provider::approval::PermissionAnswer::Option(o),
                    _ => neosh_provider::approval::PermissionAnswer::Allow,
                };
                self.settle_offer(&session, &id, crate::relay::Reply::Approve(answer))
            }
            C::Answer { id, answers } => {
                self.settle_offer(&session, &id, crate::relay::Reply::Answers(answers))
            }
            C::SetMode { mode } => {
                self.set_permission_mode_of(&session, mode);
                self.publish_inventory();
                Ok(None)
            }
            // The older form, which names no prompt. Nothing here is waiting on an answer that is
            // not about anything, and a silent success would be a remote machine believing it had
            // authorised a write to this filesystem when it had not.
            C::Approve { id: None, .. } => Err(neosh_proto::Refusal::NotPermitted {
                what: "an approval has to name the prompt it answers".into(),
            }),
        }
    }

    /// Settle a command we sent, when the far end answers.
    ///
    /// `session` is what a [`neosh_proto::AgentCommand::NewSession`] made, and is the whole of what
    /// makes starting something on another machine reversible into looking at it: the far end knows
    /// the id and says it, and until this carried it the caller could start a conversation over
    /// there and then had nothing to open.
    pub fn on_swarm_reply(
        &mut self,
        id: &str,
        ok: bool,
        message: String,
        session: Option<neosh_proto::SessionId>,
    ) {
        let Some(waiting) = self.swarm_pending.remove(id) else { return };
        let asked = self.swarm_asking.remove(id);
        let result = if !ok {
            Err(ApiError::Denied { reason: message })
        } else if asked == Some(SwarmAsk::Browse) {
            // A `Browsed` settles through `on_swarm_browsed`; reaching here with `ok` means the
            // peer answered a directory listing with a bare `Ack`, which no version of it does.
            Ok(ApiOk::Paths { paths: Vec::new(), denied: None })
        } else {
            Ok(ApiOk::SwarmCommanded { session })
        };
        self.answer_swarm(waiting, result);
    }

    /// Settle a directory listing we asked a peer for.
    pub fn on_swarm_browsed(&mut self, id: &str, result: Result<Vec<String>, String>) {
        let Some(waiting) = self.swarm_pending.remove(id) else { return };
        self.swarm_asking.remove(id);
        self.answer_swarm(waiting, match result {
            Ok(paths) => Ok(ApiOk::Paths { paths, denied: None }),
            Err(reason) => Err(ApiError::Denied { reason }),
        });
    }

    /// Act on an option the host itself owns. Everything else is a plugin's business.
    fn apply_option(&mut self, name: &str, value: &OptionValue) {
        match name {
            "agent.model" => {
                let Some(spec) = value.as_str().filter(|s| !s.is_empty()) else { return };
                let Some((instance, model)) = Config::parse_model(spec) else {
                    self.editor_message(
                        MessageLevel::Error,
                        format!("agent.model: {spec:?} is not `instance/model`"),
                    );
                    return;
                };
                let selection = neosh_proto::ModelSelection {
                    instance: instance.into(),
                    model: neosh_proto::ModelId::from(model),
                    options: Vec::new(),
                };
                if self.agent.providers().resolve(&selection).is_none() {
                    if self.startup.loading > 0 || !self.startup.discovered {
                        self.startup.pending_model = Some(spec.to_string());
                    } else {
                        self.editor_message(
                            MessageLevel::Error,
                            format!(
                                "agent.model: no provider serves {spec}. `neosh --list-models` \
                                 shows what is reachable."
                            ),
                        );
                    }
                    return;
                }
                self.startup.pending_model = None;
                self.select_model(selection);
                // Chosen, not remembered: from here on the fallback leaves it alone even if it
                // cannot authenticate, because the error is more use than a substitution.
                self.selection_pinned = true;
                self.refresh_status();
                self.draw_welcome();
            }
            "ui.theme" => {
                let Some(name) = value.as_str() else { return };
                let changed = if let Some(variant) = neosh_core::palette::Variant::parse(name) {
                    self.editor.set_theme(variant)
                } else if let Some((base, groups)) = self.contributed_theme(name) {
                    self.editor.set_custom_theme(name, base, groups)
                } else {
                    // Unreachable through the option registry, which validates the enum; reachable
                    // if the host ever sets this itself.
                    self.editor_message(
                        MessageLevel::Error,
                        format!("ui.theme: {name:?} is not a theme"),
                    );
                    return;
                };
                if changed {
                    self.refresh_status();
                }
            }
            "worktree.root" => {
                // Expanded here, once, so nothing downstream has to know what `~` means. A path
                // half the readers expand is a path that works until the day it does not.
                let raw = value.as_str().unwrap_or_default();
                if let Some(full) = expand_tilde(raw)
                    && full != raw
                {
                    self.set_option_or_defer(
                        "worktree.root".into(),
                        OptionValue::Str(full),
                    );
                }
            }
            "ui.hints" => self.refresh_composer(),
            "ui.motion" => {
                // Applied by stripping the animation from the groups that have one, so a frontend
                // needs no concept of the setting: it animates what carries an animation, and with
                // motion off nothing does.
                if self.editor.set_motion(value.as_bool().unwrap_or(true)) {
                    self.refresh_status();
                }
            }
            "agent.system_prompt" => {
                let text = value.as_str().unwrap_or_default();
                self.agent.session().system =
                    if text.is_empty() { None } else { Some(text.to_string()) };
            }
            "agent.append_prompt" => {
                self.agent.set_appended_prompt(value.as_str().unwrap_or_default());
            }
            // Re-read whole rather than patched field by field: they are eight settings describing
            // one behaviour, they are all cheap to read, and a partial update is how two of them
            // end up disagreeing about whether notifications are on.
            n if n.starts_with("notify.") => self.reload_notify(),
            _ => {}
        }
    }

    /// Redraw the tab strip.
    ///
    /// Two things on one row, and the second is why the row is always there. On the left, the tabs:
    /// numbered, so `<C-w>1` is readable straight off the bar rather than being a fact you have to
    /// have been told. On the right, the keys — which is the whole of "you can tell what to do by
    /// looking at it", and the reason this is not hidden behind a single-tab check.
    ///
    /// A tab with no name is named by what is in it: the conversation in its active pane, or how
    /// many panes it has when they disagree. Resolved on every redraw rather than stored, because a
    /// name worked out once is a tab still called by the conversation you closed an hour ago.
    fn refresh_tabline(&mut self) {
        let views: Vec<neosh_proto::ViewId> = self.views.keys().copied().collect();
        for view in views {
            self.in_view(view, |me| me.draw_tabline());
        }
    }

    fn draw_tabline(&mut self) {
        let view = self.view_now();
        let Some(buf) = self.views.get(&view).map(|v| v.tabline) else { return };
        let width = self
            .views
            .get(&view)
            .map(|v| v.tabline_win)
            .and_then(|w| self.editor.window(w))
            .and_then(|w| w.viewport.map(|v| v.width as usize))
            .unwrap_or(0);
        // A width of zero is *unknown*, not narrow — the frontend has not reported one yet, or is
        // one that never does. The tabs are written anyway and the legend is what waits: which tabs
        // are open does not depend on how wide the row is, and returning early here left the bar
        // empty until something happened to resize it, which for a client that reports no
        // viewports at all is never.
        let known = width > 0;

        // This conversation's tabs, not this terminal's. A tab belongs to the conversation in it —
        // see [`neosh_proto::TabInfo::group`] — so the shell you left running in another one is not
        // on this bar, and the numbers here are the numbers `<C-w>1` takes.
        let (tabs, active) = (self.editor.visible_tabs_of(view), self.editor.active_tab_of(view));
        // Text and its colours, built together and written as one line plus range marks — the way
        // the status strip does it, and for the same reason: virtual text on an empty line cannot
        // be selected, searched or read back, and this is a row people will want to screenshot.
        let mut text = String::new();
        let mut marks: Vec<(usize, usize, String)> = Vec::new();
        // The tabs get the room first and the legend takes what is left. A hint is worth a lot and
        // is worth less than the thing it is a hint about: crowded out by three key names, the bar
        // said `+1` where a conversation's title belongs, which is the one row on screen whose job
        // is telling you which conversations are open.
        //
        // The first hint is the exception and is reserved for: it is `<C-w>`, and everything else
        // here — including the way back to a tab whose name did not fit — is behind it.
        let keys = self.tabline_keys();
        let cost = |(k, l): &(String, String)| display_width(k) + display_width(l) + 2;
        // Two spaces between the keys and the last tab, so the bar never reads as one long run.
        let reserved = keys.first().map(cost).unwrap_or(0) + 2;

        // `1` rather than `0`: the key is `<C-w>1`, and a bar counting from zero is a bar whose
        // numbers are not the numbers you type.
        let labels: Vec<String> = tabs
            .iter()
            .enumerate()
            .map(|(n, tab)| format!(" {} {} ", n + 1, self.tab_title(view, tab)))
            .collect();
        let here = active
            .and_then(|id| tabs.iter().position(|t| t.id == id))
            .unwrap_or(0);
        let mine = labels.get(here).map(|l| display_width(l)).unwrap_or(0);
        let room = match known {
            // The reservation gives way rather than the tab you are in. `<C-w> windows` is fourteen
            // columns, and a pane narrow enough that holding them leaves nothing behind is a pane
            // where the bar read `+2` — three tabs open and not one of them named, the hint about
            // the bar having crowded out the bar. A legend that does not fit is dropped by its own
            // fitting below, so taking the room back here costs it nothing it could have used.
            true => width.saturating_sub(reserved).max(mine.min(width)),
            false => usize::MAX,
        };
        // What was left out, on the side it was left out from. `+2` on the left and `+2` on the
        // right are different sentences about where the tabs you cannot see are, and one counter at
        // the end can only ever say the second of them.
        let more = |n: usize| format!(" +{n} ");
        // The room a window of tabs needs, counting what it leaves behind on either side.
        let need = |lo: usize, hi: usize| -> usize {
            let mut w: usize = labels[lo..=hi].iter().map(|l| display_width(l)).sum();
            if lo > 0 {
                w += display_width(&more(lo));
            }
            if hi + 1 < labels.len() {
                w += display_width(&more(labels.len() - hi - 1));
            }
            w
        };

        // The tab you are in is what the room is spent on first, and the rest grow outward from it.
        //
        // Filled left to right and cut at the end, the bar dropped the one tab it exists to tell
        // you about: four tabs open with the keyboard in the fourth read `1 foo  2 bar  +2`, which
        // says where you are not. Nothing else on screen answers "which tab is this", so the answer
        // has to survive the fitting rather than be the first thing it discards.
        if !labels.is_empty() {
            let (mut lo, mut hi) = (here, here);
            loop {
                let mut grew = false;
                // Leftwards first, so the numbers on the bar keep counting up from wherever it
                // starts rather than jumping.
                if lo > 0 && need(lo - 1, hi) <= room {
                    lo -= 1;
                    grew = true;
                }
                if hi + 1 < labels.len() && need(lo, hi + 1) <= room {
                    hi += 1;
                    grew = true;
                }
                if !grew {
                    break;
                }
            }

            let left = (lo > 0).then(|| more(lo));
            let right = (hi + 1 < labels.len()).then(|| more(labels.len() - hi - 1));
            // What everything other than the tab you are in takes. The counters are dropped before
            // that label is: a bar reading `+2` and nothing else has spent its only row saying how
            // many answers it is not giving, and the number on the label — `3` — already says there
            // are at least three tabs.
            let others = need(lo, hi) - mine;
            let counters: usize = [&left, &right]
                .iter()
                .flat_map(|m| m.iter())
                .map(|m| display_width(m))
                .sum();
            let (left, right, others) = match room >= others + 1 {
                true => (left, right, others),
                false => (None, None, others - counters),
            };
            if let Some(left) = left {
                let from = text.len();
                text.push_str(&left);
                marks.push((from, text.len(), "Tabline.Tab".into()));
            }
            // Nothing is half-drawn. A tab label cut in the middle reads as a conversation with a
            // different name — so every tab but the one you are in is dropped whole and counted.
            // The one you are in is the exception, because dropping it is what this fitting exists
            // to prevent: it is elided, with the `…` that says so, rather than left out.
            let spare = room.saturating_sub(others);
            for (n, label) in labels.iter().enumerate().take(hi + 1).skip(lo) {
                let label = match n == here {
                    true => elide(label, spare),
                    false => label.clone(),
                };
                let from = text.len();
                text.push_str(&label);
                let group = match n == here {
                    true => "Tabline.Active",
                    false => "Tabline.Tab",
                };
                marks.push((from, text.len(), group.into()));
            }
            if let Some(right) = right {
                let from = text.len();
                text.push_str(&right);
                marks.push((from, text.len(), "Tabline.Tab".into()));
            }
        }

        // The legend, hard against the right edge. What it says depends on where you are, because a
        // strip listing keys that do nothing here is worse than no strip: it is a promise about a
        // keyboard that is not being kept.
        //
        // As many as fit, in the order they are worth knowing — and whole ones, never half of one:
        // `<C-w>v spl` is a key that appears to exist and do something else.
        let used = display_width(&text);
        let mut shown: Vec<(String, String)> = Vec::new();
        let mut spent = 0usize;
        for (n, k) in keys.iter().enumerate() {
            // At an unknown width, the prefix and nothing else. There is no right-hand edge to
            // place a legend against yet — but *that this bar leads somewhere* is the one thing it
            // has to say from the first frame, and making that contingent on a frontend having
            // reported geometry would mean a client that reports none never learns the prefix
            // exists. The rest arrives with the first viewport report, which redraws this.
            let fits = match known {
                true => used + spent + cost(k) + 1 <= width,
                false => n == 0,
            };
            if !fits {
                break;
            }
            spent += cost(k);
            shown.push(k.clone());
        }
        let pad = width.saturating_sub(used + spent);
        if known && pad > 0 {
            text.push_str(&" ".repeat(pad));
        }
        for (k, label) in shown {
            let from = text.len();
            text.push_str(&format!(" {k}"));
            marks.push((from, text.len(), "Tabline.Key".into()));
            let from = text.len();
            text.push_str(&format!(" {label}"));
            marks.push((from, text.len(), "Tabline.Hint".into()));
        }

        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply_in(view, &plugin, ApiCall::BufSetLines {
            buf,
            start: 0,
            end: -1,
            lines: vec![text],
        });
        // Cleared first. A row rewritten several times a second accumulates marks otherwise, and at
        // equal priority the *narrower* one wins — so the bar ends up painted by the shortest label
        // it had a few seconds ago.
        let _ = self.editor.apply_in(view, &plugin, ApiCall::MarkClear {
            ns: self.tabline_ns,
            buf,
            start: None,
            end: None,
        });
        for (from, to, hl) in marks {
            let _ = self.editor.apply_in(view, &plugin, ApiCall::MarkSet {
                ns: self.tabline_ns,
                buf,
                row: 0,
                col: from as u32,
                opts: neosh_proto::ExtmarkOpts {
                    end_col: Some(to as u32),
                    hl_group: Some(hl),
                    line_hl_group: None,
                    virt_text: vec![],
                    virt_text_pos: neosh_proto::VirtTextPos::Eol,
                    on_delete: neosh_proto::OnDelete::Clamp,
                    priority: 0,
                    image: None,
                },
            });
        }
    }

    /// What a tab is called: its own name, or the conversation in it.
    fn tab_title(&self, view: neosh_proto::ViewId, tab: &neosh_proto::TabInfo) -> String {
        if let Some(name) = &tab.title {
            return name.clone();
        }
        let panes = self.editor.panes_in_tab(view, tab.id);
        // Several panes and no name of its own: the count is the honest label. Naming a tab after
        // one of four conversations picks a winner nobody asked it to pick, and the pane you are in
        // is already the one with the keyboard — the bar is for the tabs you are *not* in.
        if panes.len() > 1 {
            return format!("{} panes", panes.len());
        }
        let pane = self.views.get(&view).and_then(|v| v.panes.get(&tab.active_pane));
        // A shell is not in a conversation, so naming it after one would be naming it after
        // whatever it happened to inherit. The command it is running is the true answer and is
        // short, which the bar also wants.
        if pane.is_some_and(|p| p.term) {
            let shell = self.terminals.get(tab.active_pane).map(|t| t.command.clone());
            let name = shell
                .as_deref()
                .and_then(|c| c.rsplit('/').next())
                .unwrap_or("shell")
                .to_string();
            return name;
        }
        let title = pane
            .map(|p| p.session.clone())
            .and_then(|s| self.agent.sessions().get(&s).and_then(|s| s.title.clone()));
        match title {
            Some(t) if !t.trim().is_empty() => t,
            _ => "new".to_string(),
        }
    }

    /// The keys the tab strip advertises, for wherever the keyboard is.
    ///
    /// Short, and different per place. The full list is `<C-w>` — which is on this strip, and is
    /// the thing that makes the rest discoverable without any of them being printed.
    fn tabline_keys(&self) -> Vec<(String, String)> {
        let view = self.view_now();
        let panes = self
            .editor
            .active_tab_of(view)
            .map(|t| self.editor.panes_in_tab(view, t).len())
            .unwrap_or(1);
        let tabs = self.editor.tab_count_of(view);

        let mut out = vec![(self.chord("<C-w>"), "windows".to_string())];
        // Only once there is somewhere to go. A "next tab" key on a workspace with one tab is a
        // key that does nothing, printed where you are most likely to read it.
        if tabs > 1 {
            out.push((self.chord("<C-w>w"), "next tab".to_string()));
        }
        if panes > 1 {
            out.push((self.chord("<C-w>hjkl"), "move".to_string()));
        } else {
            out.push((self.chord("<C-w>v"), "split".to_string()));
        }
        out
    }

    /// How a chord should be *printed*, which is whatever it is currently bound to.
    ///
    /// Read out of the live keymap rather than spelled here, so a legend is a promise about the
    /// keyboard somebody actually has. `init.ts` moving the window prefix moves what the strip
    /// says, and a hint row that still advertises the default after a rebinding is worse than no
    /// hint row at all.
    fn chord(&self, default: &str) -> String {
        let prefix = self.window_prefix();
        match default.strip_prefix("<C-w>") {
            Some(rest) => format!("{prefix}{rest}"),
            None => default.to_string(),
        }
    }

    /// How wide the status strip is, or zero if the frontend has not said yet.
    fn status_width(&self) -> usize {
        self.editor
            .windows()
            .find(|w| w.buf == self.vs().status)
            .and_then(|w| w.viewport.map(|v| v.width as usize))
            .unwrap_or(0)
    }

    /// Redraw the status line.
    ///
    /// Rewritten wholesale rather than patched: it is one short line, and a diffing status bar is a
    /// famous source of stale text that nobody notices until a screenshot.
    /// Redraw the status line from the host's own facts plus whatever plugins contributed.
    ///
    /// The host owns the strip — one line, always visible, never scrolled — and plugins own what is
    /// in it. That split is what lets the model switcher and the git plugin each put something
    /// there without either knowing the other exists, and it is why the footer is not hard-coded
    /// here.
    fn refresh_status(&mut self) {
        // Reading is not a `Mode` — it is a place the keyboard is, and the whole reason it shows
        // here is that a key doing something different needs to say so before you press it.
        let mode = if self.v().secret.is_some() {
            // Named in the status line because it is the one state where a keystroke does not do
            // what it usually does, and you should be able to see that before you press one.
            "key"
        } else if self.v().reading {
            // And which *kind* of reading. A selection is the one state here where the same key
            // does a different thing — `y` copies rows rather than characters, `esc` gives the
            // selection up rather than the mode — and Vim says so in the same place for the same
            // reason. `reading` when there is none, because that is the mode, not the absence of
            // a selection.
            match (self.chat_anchored(), self.v().reading_shape) {
                (true, SelectShape::Line) => "visual line",
                (true, _) => "visual",
                _ => "reading",
            }
        } else {
            match self.editor.mode(self.from.view) {
                Mode::Chat => "chat",
                Mode::Normal => "normal",
                Mode::Insert => "insert",
                Mode::Visual => "visual",
            }
        };

        let mut left: Vec<(i32, String, neosh_proto::StatusSegment)> = Vec::new();
        let mut right: Vec<(i32, String, neosh_proto::StatusSegment)> = Vec::new();
        for (key, seg) in &self.status_segments {
            let entry = (seg.priority, key.clone(), seg.clone());
            match seg.align {
                neosh_proto::StatusAlign::Left => left.push(entry),
                neosh_proto::StatusAlign::Right => right.push(entry),
            }
        }
        // Ties break by key so the order is stable across redraws rather than depending on which
        // plugin happened to load first.
        left.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        right.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

        // Make it fit: shorten what can be shortened, then drop what still does not fit, least
        // important first both times.
        //
        // Both sides at once, because the two halves are competing for one line and the loser is
        // whichever segment is worth least, not whichever side it happens to be on. Without this,
        // a wide left half and a right half that no longer fits means the right half is written
        // straight over the end of the left one — two segments in the same columns, which reads
        // as a corrupted line rather than as a full one.
        //
        // Nothing is truncated. Half a token count is a wrong token count, and a bar cut short is
        // a bar reading a level nothing is at. But dropping was the *only* other answer, and it
        // is too blunt on its own: a segment then costs its full width or nothing, so the widest
        // one in the strip is the one most likely to vanish — and the widest one here is the
        // context meter, which is also the number people are watching. Worse, a turn starting adds
        // a segment, so the strip got a column narrower at exactly the moment there was something
        // to watch, and on a terminal a few columns short of the whole line the meter disappeared
        // for the length of the turn and came back when it ended.
        //
        // A short form is not a truncation and is not this code's to invent: the segment carries
        // it, because only the thing that wrote `███░░░░░ 34% of 200k` knows that the denominator
        // is the part it can do without. See [`neosh_proto::StatusSegment::short`].
        {
            let width = self.status_width();
            // The mode word, the space each side of the line, and the single column that keeps the
            // two halves from touching.
            let mut used = display_width(mode) + 3;
            let cost = |seg: &neosh_proto::StatusSegment| {
                2 + display_width(&seg.text)
                    + seg
                        .keys
                        .as_deref()
                        .filter(|k| !k.is_empty())
                        .map_or(0, |k| 1 + display_width(k))
            };
            let mut order: Vec<(i32, String)> =
                left.iter().chain(right.iter()).map(|(p, k, _)| (*p, k.clone())).collect();
            // Least important last in the strip, so least important first out of it.
            order.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
            for (_, key) in &order {
                let seg = left
                    .iter()
                    .chain(right.iter())
                    .find(|(_, k, _)| k == key)
                    .map(|(_, _, s)| s)
                    .expect("came from those lists");
                used += cost(seg);
            }
            // One at a time and stopping the moment it fits, so the strip gives up as little as
            // will do rather than shrinking everything the first time anything is tight.
            if width > 0 {
                let mut given_up: Vec<(String, String)> = Vec::new();
                for (_, key) in order.iter() {
                    if used <= width {
                        break;
                    }
                    let Some(seg) = left
                        .iter_mut()
                        .chain(right.iter_mut())
                        .find(|(_, k, _)| k == key)
                        .map(|(_, _, s)| s)
                    else {
                        continue;
                    };
                    // Taken rather than read, so a segment cannot be shortened twice — and an
                    // empty short form is no short form, not a segment that says nothing.
                    let Some(short) = seg.short.take().filter(|s| !s.is_empty()) else { continue };
                    let before = cost(seg);
                    let full = std::mem::replace(&mut seg.text, short);
                    // `+` before `-`: a short form is shorter, but this must not depend on it.
                    used = used + cost(seg) - before;
                    given_up.push((key.clone(), full));
                }
                // And give back whatever still fits, most important first.
                //
                // Shortening is granular — the meter gives up eight columns to recover one — so
                // stopping at the first arrangement that fits stops one step past the best one
                // whenever it took more than a single segment to get there. Without this the
                // strip has blank columns in the middle of it *and* something abbreviated, which
                // is the one outcome nothing here wanted.
                for (key, full) in given_up.iter().rev() {
                    let Some(seg) = left
                        .iter_mut()
                        .chain(right.iter_mut())
                        .find(|(_, k, _)| k == key)
                        .map(|(_, _, s)| s)
                    else {
                        continue;
                    };
                    // The two forms differ only in their text — the key beside them is the same —
                    // so the difference in what the line costs is the difference in their widths.
                    // `used` already counts the short one, so this cannot go below zero.
                    let want = used + display_width(full) - display_width(&seg.text);
                    if want <= width {
                        seg.text.clone_from(full);
                        used = want;
                    }
                }
            }
            let mut dropped: std::collections::HashSet<String> = Default::default();
            for (_, key) in order.iter() {
                if width == 0 || used <= width {
                    break;
                }
                let seg = left
                    .iter()
                    .chain(right.iter())
                    .find(|(_, k, _)| k == key)
                    .map(|(_, _, s)| s)
                    .expect("came from those lists");
                used -= cost(seg);
                dropped.insert(key.clone());
            }
            left.retain(|(_, k, _)| !dropped.contains(k));
            right.retain(|(_, k, _)| !dropped.contains(k));
        }

        // Marks are built alongside the text so a segment's colour cannot drift from its columns.
        let mut text = String::from(" ");
        let mut marks: Vec<(usize, usize, String)> = Vec::new();
        let push = |text: &mut String, marks: &mut Vec<(usize, usize, String)>, s: &str, hl: &str| {
            let from = text.len();
            text.push_str(s);
            marks.push((from, text.len(), hl.to_string()));
        };

        let segment =
            |text: &mut String, marks: &mut Vec<(usize, usize, String)>, seg: &neosh_proto::StatusSegment| {
                text.push_str("  ");
                push(text, marks, &seg.text, seg.hl.as_deref().unwrap_or("Status.Line"));
                // The key that changes it, right after it. A key is only memorable once it has
                // been seen next to the thing it does; a legend somewhere else is a second place
                // to look.
                if let Some(keys) = seg.keys.as_deref().filter(|k| !k.is_empty()) {
                    text.push(' ');
                    push(text, marks, keys, "Composer.HintKey");
                }
            };

        push(&mut text, &mut marks, mode, "Status.Line");
        for (_, _, seg) in left.iter() {
            segment(&mut text, &mut marks, seg);
        }
        if self.status_segments.is_empty() {
            // Before any plugin has contributed — and under `--clean`, where none will.
            push(&mut text, &mut marks, "  ^C quit", "Status.Line");
        }

        // Right-aligned segments actually go to the right edge. They used to be appended after the
        // left ones and were therefore right-aligned in name only — which is the sort of thing that
        // looks fine until a second plugin uses it and both end up in the middle.
        if !right.is_empty() {
            let mut tail = String::new();
            let mut tail_marks: Vec<(usize, usize, String)> = Vec::new();
            for (_, _, seg) in right.iter() {
                segment(&mut tail, &mut tail_marks, seg);
            }
            let width = self.status_width();
            let used = display_width(&text) + display_width(&tail) + 1;
            if width > used {
                text.push_str(&" ".repeat(width - used));
            }
            let base = text.len();
            text.push_str(&tail);
            marks.extend(tail_marks.into_iter().map(|(a, b, hl)| (base + a, base + b, hl)));
        }
        text.push(' ');

        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::BufSetLines {
            buf: self.vs().status,
            start: 0,
            end: -1,
            lines: vec![text.clone()],
        });
        let _ = self.editor.apply(&plugin, ApiCall::MarkClear {
            ns: self.status_ns,
            buf: self.vs().status,
            start: None,
            end: None,
        });
        for (from, to, hl) in marks {
            let _ = self.editor.apply(&plugin, ApiCall::MarkSet {
                ns: self.status_ns,
                buf: self.vs().status,
                row: 0,
                col: from as u32,
                opts: neosh_proto::ExtmarkOpts {
                    end_col: Some(to as u32),
                    hl_group: Some(hl),
                    line_hl_group: None,
                    virt_text: vec![],
                    virt_text_pos: neosh_proto::VirtTextPos::Eol,
                    on_delete: neosh_proto::OnDelete::Clamp,
                    priority: 0,
                    image: None,
                },
            });
        }
    }

    /// Redraw the chrome around the composer: the rule, the prompt, the placeholder, the shortcuts.
    ///
    /// All of it is virtual text rather than characters in the buffer, which is the only version
    /// that is correct: the composer's contents *are* the message, so a `› ` written into it would
    /// be sent to the model, and a placeholder written into it would be sent the moment someone
    /// pressed Enter without looking. Virtual text is drawn and never read back.
    fn refresh_composer(&mut self) {
        // A terminal pane has no composer — its window *is* the field — and the chrome this draws
        // would be written straight over the shell's own output.
        if self.v().term {
            return;
        }
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::MarkClear {
            ns: self.composer_ns,
            buf: self.v().composer,
            start: None,
            end: None,
        });
        let ascii = self.option_bool("ui.ascii_only");
        let text = self.composer_text();
        // Every path that changes the draft comes through here, which is what makes this the one
        // place to say so. Compared rather than fired blind: this also runs on a resize and on a
        // redraw, and a completion menu that reopened every time the window changed width would be
        // unusable.
        //
        // And not while the field is lent to something else. A search and a key prompt both type
        // into this buffer, and what is in it then is *not* the draft — the `/` that starts a
        // search is a search, and a completion menu that opened on top of it would take the
        // keyboard away from the thing the user had just started.
        let borrowed = self.v().secret.is_some() || self.v().search.is_some();
        if self.v().draft != text && !borrowed {
            self.vm().draft.clone_from(&text);
            self.broadcast(PluginEvent::ComposerChanged { text: text.clone() });
        }
        let lines = self.editor.buffer(self.v().composer).map(|b| b.line_count()).unwrap_or(1).max(1);
        let width = self
            .editor
            .window(self.v().composer_win)
            .and_then(|w| w.viewport.map(|v| v.width as usize))
            .unwrap_or(80);

        // A blank row, then the rule. Both drawn above the first line, so they move with the field
        // rather than being rows the transcript has to remember not to write on.
        //
        // The blank is not decoration. Without it the last line of an answer sits directly on the
        // rule, and the eye reads the two as one block — the end of what was said and the edge of
        // where you say things run together, which is the single worst place in this layout to
        // save a row.
        self.composer_mark(0, VirtTextPos::Above, vec![chunk("", "Composer.Rule")]);
        let rule = if ascii { "-" } else { "\u{2500}" };
        self.composer_mark(0, VirtTextPos::Above, vec![chunk(rule.repeat(width), "Composer.Rule")]);

        // What you typed while it was working, waiting for a gap to be said in — between the rule
        // and the field, which is where it belongs and not where it was.
        //
        // Under the field it sat below the shortcut row, in the strip the eye has learned to read
        // as chrome, one row above the status line: a sentence of yours, filed with the furniture.
        // Above the field it is the last thing between the transcript and what you are typing,
        // which is exactly what it is — the queue is *unsent conversation*, and it reads in order
        // with everything else that has been said.
        //
        // Drawn after the rule so it lands under it: marks at the same position stack in the order
        // they were set.
        // What has been attached but not yet sent, between the rule and the queue. Above the
        // field for the reason the queue is: it is part of the message you are writing, not
        // chrome — and it is the one part of that message you cannot read off the field itself.
        let attached: Vec<String> = self
            .attached
            .get(&self.active_session())
            .map(|v| v.iter().map(|a| a.label()).collect())
            .unwrap_or_default();
        for (i, label) in attached.iter().enumerate() {
            let mut v = vec![
                chunk("  ", "Composer.Hint"),
                // Labelled once, like the queue: a column of the same word is not the part you
                // are reading.
                chunk(if i == 0 { "attached " } else { "         " }, "Status.Pending"),
                chunk(label.clone(), "Composer.Hint"),
            ];
            if i + 1 == attached.len() {
                v.push(chunk("  ", "Composer.Hint"));
                v.push(chunk(if ascii { "Bksp" } else { "\u{232b}" }, "Composer.HintKey"));
                v.push(chunk(" take off", "Composer.Hint"));
            }
            self.composer_mark(0, VirtTextPos::Above, v);
        }

        let queued = self.agent.steering_queue(&self.active_session());
        let take = queued.len().min(3);
        for (i, text) in queued.iter().map(|p| &p.text).take(take).enumerate() {
            let more = queued.len() - take;
            let mut v = vec![
                chunk("  ", "Composer.Hint"),
                // Only the first row is labelled. Three rows each saying "queued" is a column of
                // the same word, and the word is not the part you are checking.
                chunk(if i == 0 { "queued " } else { "       " }, "Status.Pending"),
            ];
            let one = text.lines().next().unwrap_or("").trim();
            let tail = if i + 1 == take && more > 0 { format!("  (+{more} more)") } else { String::new() };
            let key = "^Y";
            // *What happens*, not what you do. `edit` is the operation and "I want that sentence
            // back" is the thought, and the two only look the same to somebody who already knew
            // the key was there.
            let undo = " take back";
            // The label, the gap either side of it, and the words after it, so a long message is
            // clipped rather than pushing the key that undoes it off the edge.
            let room = width
                .saturating_sub(9 + tail.chars().count() + display_width(key) + 2 + undo.len());
            let body: String = one.chars().take(room).collect();
            v.push(chunk(format!("{body}{tail}"), "Composer.Hint"));
            // On the last row, so it is next to the message it would take back rather than
            // attached to the oldest one.
            if i + 1 == take {
                v.push(chunk("  ", "Composer.Hint"));
                v.push(chunk(key, "Composer.HintKey"));
                v.push(chunk(undo, "Composer.Hint"));
            }
            self.composer_mark(0, VirtTextPos::Above, v);
        }

        // The prompt, spliced in at column zero. The caret is pushed past it by the frontend, which
        // is the only place that knows how wide it drew.
        let caret = if ascii { "> " } else { "\u{203a} " };

        // While a key or a search is being typed the composer is borrowed and says so itself, so
        // it gets the rule above it and nothing else: a prompt glyph in front of `/Haiku` would
        // read as part of the search, and shortcuts that no longer work, advertised under a field
        // where every keystroke is captured, are exactly the wrong thing to be reading.
        //
        // The rule stays because it is the layout. Dropping it moves every row of the transcript
        // by one the moment you press `/`, which is the moment you least want the screen to jump.
        if self.v().secret.is_some() || self.v().search.is_some() {
            return;
        }

        self.composer_mark(0, VirtTextPos::Inline, vec![chunk(caret, "Composer.Prompt")]);

        if text.is_empty() {
            // While a turn is running, `\u{23ce}` does something else, and an empty field is the one
            // place you are looking when you are about to press it. "Ask anything" there invites
            // you to do the thing you cannot do and says nothing about the thing you can — which
            // is how somebody presses it, watches the sentence vanish into a queue they did not
            // know existed, and has no idea what just happened to it.
            let words = if self.turns.contains_key(&self.active_session()) {
                "Steer it \u{2014} taken in at the next gap"
            } else {
                "Ask anything, or run a command"
            };
            self.composer_mark(
                0,
                VirtTextPos::Eol,
                vec![chunk(words, "Composer.Placeholder")],
            );
        }

        let row = lines.saturating_sub(1);
        let indent = caret.chars().count();
        let pad = " ".repeat(indent);

        // Reading is the one place a shortcut row earns its line, whatever `ui.hints` says. The
        // keys mean something different here, they are not written down anywhere else, and the
        // composer is not being typed into — so the row is free and the question it answers is
        // live.
        if self.v().reading {
            let mut v = vec![chunk(pad, "Composer.Hint")];
            v.extend(self.reading_hints(width.saturating_sub(indent)));
            self.composer_mark(row, VirtTextPos::Below, v);
            return;
        }

        // The shortcut row goes under the last line you have written — and steps aside once you are
        // writing enough that it would cost you a line to look at.
        if lines <= 2
            && let Some(chunks) = self.hint_row(width.saturating_sub(indent))
        {
            let mut v = vec![chunk(pad, "Composer.Hint")];
            v.extend(chunks);
            self.composer_mark(row, VirtTextPos::Below, v);
        }
    }

    /// What the keys do while reading, as many as fit.
    ///
    /// A pending operator takes the row over entirely: once `y` is down, the only useful thing to
    /// know is what the second key can be, and leaving the general list up would be answering a
    /// question nobody is asking any more.
    fn reading_hints(&self, width: usize) -> Vec<neosh_proto::VirtChunk> {
        // The card under the cursor, if it is one that has something to open.
        let card = self
            .card_at(self.chat_cursor().0)
            .map(|i| &self.v().cards[i])
            .filter(|c| c.settled())
            .map(|c| c.open == cards::Open::Full);
        let tab = self.glyphs().tab;
        let mut pairs: Vec<(&str, &str)> = match self.v().reading_pending {
            Some(Pending::Yank) => vec![
                ("y", "line"),
                ("c", "code block"),
                ("m", "this turn"),
                ("a", "everything"),
                ("i", "inside"),
                ("w $ j", "motion"),
            ],
            Some(Pending::G) => vec![("g", "top"), ("e", "word end back"), ("v", "reselect")],
            Some(Pending::Z) => {
                vec![("z", "centre"), ("t", "top"), ("b", "bottom"), ("a", "card")]
            }
            // Waiting for a literal character, so listing keys would be a lie about what the next
            // press does. The word is the whole message.
            Some(Pending::Find { .. }) => vec![("", "a character")],
            Some(Pending::Object { .. }) => {
                vec![("w", "word"), ("p", "paragraph"), ("\" ' `", "quotes"), ("( [ {", "brackets")]
            }
            None if self.chat_anchored() => vec![
                ("y", "copy"),
                ("hjkl", "extend"),
                match self.v().reading_shape {
                    SelectShape::Line => ("v", "characters"),
                    _ => ("V", "whole lines"),
                },
                ("o", "other end"),
                ("iw", "word"),
                ("esc", "drop"),
            ],
            None => vec![
                ("y", "copy"),
                ("v", "select"),
                ("[ ]", "turn"),
                ("{ }", "block"),
                ("c", "call"),
                ("/", "find"),
                ("i", "write"),
                ("esc", "leave"),
            ],
        };
        // First, because it is about the row you are on and the others are about everywhere.
        if self.v().reading_pending.is_none()
            && !self.chat_anchored()
            && let Some(open) = card
        {
            // `\u{21e5}` no longer *opens* a card the cursor is already in — the cursor did that.
            // What it does is keep it: all of it, and still there when you move off.
            pairs.insert(0, (tab, if open { "fold" } else { "keep open" }));
        }
        let mut out: Vec<neosh_proto::VirtChunk> = Vec::new();
        let mut used = 0usize;
        if self.v().reading_pending.is_some() {
            let head = "waiting…  ";
            used += display_width(head);
            out.push(chunk(head, "Status.Pending"));
        }
        // A count that has been typed and not yet spent. It goes first because it is about the key
        // you are in the middle of pressing, and without it the digit is a press with no effect —
        // which is indistinguishable from a key that does nothing.
        if !self.v().reading_count.is_empty() {
            let head = format!("{}  ", self.v().reading_count);
            used += display_width(&head);
            out.push(chunk(head, "Accent"));
        }
        for (keys, label) in pairs {
            let gap = if out.len() > usize::from(self.v().reading_pending.is_some()) { 2 } else { 0 };
            let cost = gap + display_width(keys) + 1 + display_width(label);
            if used + cost > width {
                break;
            }
            if gap > 0 {
                out.push(chunk("  ", "Composer.Hint"));
            }
            out.push(chunk(keys.to_string(), "Composer.HintKey"));
            out.push(chunk(format!(" {label}"), "Composer.Hint"));
            used += cost;
        }
        out
    }

    /// One virtual-text mark on the composer.
    fn composer_mark(&mut self, row: u32, pos: VirtTextPos, virt_text: Vec<neosh_proto::VirtChunk>) {
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::MarkSet {
            ns: self.composer_ns,
            buf: self.v().composer,
            row,
            col: 0,
            opts: neosh_proto::ExtmarkOpts {
                end_col: None,
                hl_group: None,
                line_hl_group: None,
                virt_text,
                virt_text_pos: pos,
                on_delete: neosh_proto::OnDelete::Clamp,
                priority: 0,
                image: None,
            },
        });
    }

    /// The shortcuts, as many as fit, in the order whoever registered them asked for.
    ///
    /// Truncation drops whole hints from the end rather than cutting one in half, because half a
    /// shortcut is worse than no shortcut: it reads as a key that exists and does something else.
    /// Nothing is ever dropped silently to nothing — a row too narrow for even one hint simply
    /// does not appear, and the full list is still a keypress away.
    fn hint_row(&self, width: usize) -> Option<Vec<neosh_proto::VirtChunk>> {
        if !self.editor.options().bool("ui.hints").unwrap_or(false) {
            return None;
        }
        let mut hints: Vec<(&String, &neosh_proto::Hint)> = self.hints.iter().collect();
        hints.sort_by(|a, b| a.1.priority.cmp(&b.1.priority).then_with(|| a.0.cmp(b.0)));

        let mut out: Vec<neosh_proto::VirtChunk> = Vec::new();
        let mut used = 0usize;
        for (_, h) in hints {
            let gap = if out.is_empty() { 0 } else { 2 };
            let cost = gap + display_width(&h.keys) + 1 + display_width(&h.label);
            if used + cost > width {
                break;
            }
            if gap > 0 {
                out.push(chunk("  ", "Composer.Hint"));
            }
            out.push(chunk(h.keys.clone(), "Composer.HintKey"));
            out.push(chunk(format!(" {}", h.label), "Composer.Hint"));
            used += cost;
        }
        (!out.is_empty()).then_some(out)
    }

    /// What the transcript says before anything has happened.
    ///
    /// Redrawn rather than written once, because the two facts worth putting here — which model
    /// answers, and whether it can — are not known when the first frame goes up: providers register
    /// during startup and a plugin may add one later still. `welcome_rows` is how it knows what it
    /// last wrote, so a redraw replaces its own lines and never a real turn's.
    fn draw_welcome(&mut self) {
        // Anything else in the buffer is a conversation. Leave it alone. "Empty" has to allow for
        // the one blank line a buffer always has, which is not the same as no lines at all.
        let blank = self
            .editor
            .buffer(self.v().chat)
            .map(|b| b.line_count() <= 1 && b.get_lines(0, 1).iter().all(|l| l.is_empty()))
            .unwrap_or(true);
        let n = self.editor.buffer(self.v().chat).map(|b| b.line_count()).unwrap_or(0) as usize;
        if !blank && n != self.v().welcome_rows {
            return;
        }
        let replacing = if blank { n as i64 } else { self.v().welcome_rows as i64 };

        let selection = self.agent.selection();
        let account = selection.as_ref().and_then(|s| {
            let reg = self.agent.providers();
            let inst = reg.instances().find(|i| i.id == s.instance)?;
            Some(neosh_provider::credentials::credentials().source(inst).summary(&inst.auth))
        });
        let model = match &selection {
            Some(s) => match account {
                Some(a) => format!("{}/{}  \u{b7}  {a}", s.instance, s.model),
                None => format!("{}/{}", s.instance, s.model),
            },
            None => "nothing configured \u{2014} run `neosh --list-models`".to_string(),
        };

        // Whether this is a workspace or an empty one. A placeholder is what you land in after
        // closing the last conversation — or after reopening a workspace whose every conversation
        // was archived — and the transcript is the only thing on screen with room to say what to
        // press next, because the panel beside it is, correctly, blank.
        let empty = !self.agent.sessions().any_open();
        let ascii = self.option_bool("ui.ascii_only");
        let width = self.chat_width();
        let mut rows: Vec<String> = vec![String::new()];
        let mut marks: Vec<Mark> = Vec::new();
        let mut say = |rows: &mut Vec<String>, text: String, spans: Vec<(usize, usize, &'static str)>| {
            let at = rows.len() as u32;
            marks.extend(spans.into_iter().map(|(a, b, g)| Mark::at(at, a, b, g)));
            rows.push(text);
        };
        // The word, where there is room for it and a terminal that can draw it. Narrower than
        // the word and it wraps into noise; in ASCII, half its glyphs would be boxes.
        if !ascii && width >= crate::logo::WIDTH + 2 {
            for i in 0..crate::logo::ROWS.len() {
                let (text, spans) = crate::logo::row(i, 2);
                say(&mut rows, text, spans);
            }
            let tag = "  a terminal-first agent workspace".to_string();
            let len = tag.len();
            say(&mut rows, tag, vec![(0, len, "Comment")]);
        } else {
            let tag = "  neosh \u{2014} a terminal-first agent workspace".to_string();
            let len = tag.len();
            say(&mut rows, tag, vec![(2, 2 + "neosh".len(), "Accent"), (2 + "neosh".len(), len, "Comment")]);
        }
        rows.push(String::new());
        // Fitted, because neither of these is a sentence this file gets to keep short. A model id
        // is a catalogue's and an account's, and a directory is the user's — `tilde` takes `$HOME`
        // off the front and there is nothing else to give. Nothing wraps the welcome, so a row
        // longer than the pane becomes a second line at column zero under an indented one, which
        // is exactly what the note under the next row is about; the difference is that that one
        // could be hand-shortened and these cannot.
        //
        // The path is cut from the *front*: the end of a directory is the part that says which one
        // it is, and `…/neosh/work` is an answer where `/Users/you/very/deep/pr…` is not.
        let room = width.saturating_sub(13);
        say(&mut rows, format!("  model      {}", elide(&model, room)), vec![(0, 13, "Comment")]);
        let cwd = tilde(&self.cwd);
        say(&mut rows, format!("  directory  {}", elide_start(&cwd, room)), vec![(0, 13, "Comment")]);
        rows.push(String::new());
        // Said only when it is true: there is no conversation anywhere, so nothing else on screen
        // is going to tell you that typing is how one starts.
        if empty {
            // Short enough to be one row. Nothing here wraps the welcome, so a sentence longer than
            // the chat is a second line starting at column zero, under an indented one — and the
            // row of keys below already says how to add a project, which is the rest of it.
            let line = "  Nothing open. What you type here starts a conversation.".to_string();
            let len = line.len();
            say(&mut rows, line, vec![(0, len, "Comment")]);
            rows.push(String::new());
        }
        // Only when there is something to say. On a working setup the welcome should be the word,
        // a few short lines, and then out of the way.
        if selection.as_ref().is_some_and(|s| !self.selection_is_usable(s)) {
            let warn = "  This model cannot authenticate yet. Press ^P to pick another, or add a \
                        key there."
                .to_string();
            let len = warn.len();
            say(&mut rows, warn, vec![(0, len, "Diagnostic.Warn")]);
            rows.push(String::new());
        }
        // The keys worth knowing before the first question — and the one most people never find,
        // which is that the transcript is a place you can go. The rest of the table is on `^Z`.
        let keys: [(&str, &str); 5] = if empty {
            // A different five, because four of the everyday ones are about a conversation and
            // there is not one. What is worth knowing here is how to get one.
            [
                (if ascii { "Enter" } else { "\u{23ce}" }, "start"),
                ("^O", "add project"),
                ("^T", "projects"),
                ("^F", "archived"),
                ("^Z", "every key"),
            ]
        } else {
            [
                (if ascii { "Enter" } else { "\u{23ce}" }, "send"),
                ("^S", "browse & copy"),
                ("^P", "model"),
                ("^T", "projects"),
                ("^Z", "every key"),
            ]
        };
        let mut line = String::from("  ");
        let mut spans = Vec::new();
        for (i, (key, label)) in keys.iter().enumerate() {
            let sep = if i > 0 { "   " } else { "" };
            // As many as fit, in the order they are worth knowing. A row of hints that wraps is
            // two rows of hints, the second of which starts mid-phrase — and the whole point of
            // this line is to be readable at a glance. `^S` is second because it is the one most
            // people never find; the ones that get dropped first are the ones `^Z` also names.
            let would = sep.len() + key.chars().count() + 1 + label.chars().count();
            if i > 0 && line.chars().count() + would > width {
                break;
            }
            line.push_str(sep);
            let from = line.len();
            line.push_str(key);
            spans.push((from, line.len(), "Key"));
            let from = line.len();
            line.push(' ');
            line.push_str(label);
            spans.push((from, line.len(), "Comment"));
        }
        say(&mut rows, line, spans);
        rows.push(String::new());

        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::MarkClear {
            ns: self.chat_ns,
            buf: self.v().chat,
            start: Some(0),
            end: Some(replacing.max(0) as u32),
        });
        let _ = self.editor.apply(&plugin, ApiCall::BufSetLines {
            buf: self.v().chat,
            start: 0,
            end: replacing,
            lines: rows.clone(),
        });
        self.vm().welcome_rows = rows.len();
        self.draw_marks(&marks, 0);
    }

    /// Push the permission mode out to every driver that runs its own agent loop.
    ///
    /// Called from everywhere the mode can change, which now includes a config reload. It used to
    /// be only the API call, so `permissions.mode` in `config.toml` reached the built-in tools and
    /// no further, and the drivers stayed on whatever they were told at startup.
    fn sync_agent_drivers(&self) {
        let mode = self.agent.permission_mode(&self.active_session());
        for d in &self.agent_drivers {
            d.set_permission_mode(mode);
        }
    }

    /// Set the mode for the conversation on screen, and tell everything that has to be told.
    ///
    /// The conversation holds it; nothing else does. What the agent drivers hold is a *copy* — they
    /// police their own tool calls inside their own process, and there is no way to reach the
    /// permission layer from in there — so they are told, every time, from here. The footer is told
    /// for the ordinary reason: it is how anybody knows.
    fn set_permission_mode(&mut self, mode: neosh_proto::PermissionMode) {
        let here = self.active_session();
        self.set_permission_mode_of(&here, mode);
    }

    fn set_permission_mode_of(
        &mut self,
        session: &neosh_proto::SessionId,
        mode: neosh_proto::PermissionMode,
    ) {
        self.agent.set_permission_mode(session, mode);
        self.sync_agent_drivers();
        self.refresh_status();
        self.persist_sessions();
    }

    /// Serve a terminal rather than be one: `quit` sends the viewer away, and what this workspace
    /// is holding is reported to `live` for `neosh status` to read.
    pub fn serve(
        &mut self,
        live: std::sync::Arc<crate::daemon::Live>,
        control: tokio::sync::mpsc::UnboundedReceiver<crate::daemon::Control>,
    ) {
        self.on_quit = OnQuit::Detach;
        self.live = Some(live);
        self.control_rx = Some(control);
    }

    /// Tell every plugin, and every connection that asked to hear it.
    ///
    /// One method rather than two call sites everywhere, so a new event cannot arrive for plugins
    /// and be invisible to `neosh agent watch` — which is the same argument as the socket carrying
    /// [`ApiCall`] rather than a vocabulary of its own: one surface, two ways in.
    fn broadcast(&self, event: PluginEvent) {
        self.bridge.broadcast(event.clone());
        let Ok(mut subs) = self.subscribers.lock() else { return };
        if subs.is_empty() {
            return;
        }
        // Dropped rather than queued for a subscriber that has gone: the send fails, the sender
        // leaves the list, and nothing about a script walking away can cost this workspace memory.
        subs.retain(|s| s.send(event.clone()).is_ok());
    }

    /// One thing asked over the control socket.
    async fn on_control(&mut self, control: crate::daemon::Control) {
        match control {
            crate::daemon::Control::Subscribe { events } => {
                if let Ok(mut subs) = self.subscribers.lock() {
                    subs.push(events);
                }
            }
            crate::daemon::Control::Call { call, reply } => self.control_call(call, reply).await,
        }
    }

    /// Answer one [`ApiCall`] that arrived over the socket instead of from a plugin.
    ///
    /// The body is [`Host::dispatch`], under the host's own id — the socket is the keyboard, and
    /// the keyboard has never had a manifest to declare capabilities in. What is here rather than
    /// there is the handful of calls whose answer does not arrive now: they are settled through
    /// the script bridge for a plugin, and a caller with no plugin id has to be settled on its own
    /// `oneshot`.
    async fn control_call(
        &mut self,
        call: ApiCall,
        reply: tokio::sync::oneshot::Sender<neosh_proto::ApiResponse>,
    ) {
        // Refused by name rather than half-answered. Each of these finishes when a person does
        // something at a terminal — types a key into a hidden field, has an image on a clipboard —
        // or when a machine on the far side of a network answers, and none of those is a thing a
        // caller with no screen can be waiting for. A clear refusal beats a call that returns
        // successfully having done nothing.
        let unreachable = match &call {
            ApiCall::ProviderSetCredential { .. } => Some(
                "a key is typed into a terminal. Set it in config.toml, in the environment, or \
                 with `provider.key` at a keyboard",
            ),
            ApiCall::ChatAttach { path: None } => {
                Some("there is no clipboard here — name a file: chat.attach with a path")
            }
            ApiCall::SwarmProbe { .. }
            | ApiCall::SwarmCommand { .. }
            | ApiCall::SwarmBrowse { .. }
            // And this one for a second reason on top of the first: it opens a *pane*, which is a
            // rectangle on a screen, and a control connection is deliberately not a view.
            | ApiCall::SwarmShell { .. } => Some(
                "the swarm is answered by the machine at the other end, which this connection has \
                 no way to wait for. Use `agent.command` for a conversation on this machine, and \
                 `path.complete` for a directory on it",
            ),
            _ => None,
        };
        if let Some(why) = unreachable {
            let _ = reply.send(Err(ApiError::Internal { message: why.into() }).into());
            return;
        }
        // Answered when the command's owner answers, which for a handler that opens a picker is
        // when somebody closes it. Never on this loop: waiting here would freeze every terminal
        // attached to this workspace for the length of somebody else's function.
        if let ApiCall::CmdCall { name, args } = call {
            match self.editor.command_owner(&name) {
                None => {
                    let _ = reply
                        .send(Err(ApiError::NotFound { what: format!("command {name}") }).into());
                }
                Some(owner) if owner.0 == BUILTIN => {
                    self.run_builtin(&name, args, None);
                    let _ = reply.send(Ok(ApiOk::Json { value: serde_json::Value::Null }).into());
                }
                Some(owner) => {
                    let bridge = self.bridge.clone();
                    tokio::spawn(async move {
                        let result = bridge.call_command(&owner, &name, args).await;
                        let _ = reply.send(
                            result
                                .map(|value| ApiOk::Json { value })
                                .map_err(|message| ApiError::Internal { message })
                                .into(),
                        );
                    });
                }
            }
            return;
        }
        if Self::is_slow(&call) {
            let svc = self.services(&PluginId::from(BUILTIN));
            tokio::spawn(async move {
                let _ = reply.send(run_slow(svc, call).await.into());
            });
            return;
        }
        let response = self.dispatch(&PluginId::from(BUILTIN), call).await;
        let _ = reply.send(response.into());
    }

    /// Tell whoever is asking what is running here.
    ///
    /// A status query has to be answerable *while* the host is busy, because that is exactly when
    /// somebody asks — so it is a snapshot the accept loop can read rather than a question put to
    /// the run loop, and this is what keeps the snapshot true.
    ///
    /// Called from every path that could have changed the answer, and rate-limited to once a
    /// second rather than reasoned about: a streaming turn flushes a frame every few milliseconds
    /// and each report locks the session store and formats a line per running turn. A second is
    /// finer than any of these numbers are quoted at.
    fn report_live(&mut self) {
        if self.live.is_none() {
            return;
        }
        let fresh = self.live_at.is_some_and(|at| at.elapsed() < Duration::from_secs(1));
        if fresh && self.rounds.len() == self.live_turns {
            return;
        }
        self.live_at = Some(std::time::Instant::now());
        self.live_turns = self.rounds.len();
        self.report_live_now();
    }

    /// The report itself, whatever was asked a moment ago.
    ///
    /// Separate so the paths where the answer has certainly changed — a viewer arriving or
    /// leaving — do not silently do nothing because a frame went past half a second earlier.
    fn report_live_now(&self) {
        let Some(live) = &self.live else { return };
        let store = self.agent.sessions();
        let running: Vec<neosh_proto::RunningTurn> = self
            .rounds
            .iter()
            .filter_map(|(id, round)| {
                let s = store.get(id)?;
                Some(neosh_proto::RunningTurn {
                    session: id.clone(),
                    label: s.label(),
                    cwd: s.cwd.display().to_string(),
                    elapsed_secs: round.started.elapsed().as_secs(),
                })
            })
            .collect();
        let conversations = store.iter().filter(|s| !s.archived).count();
        drop(store);
        live.report(conversations, running);
    }

    /// Take ownership of the agent drivers, so the mode can be mirrored into them later and a
    /// request one of them cannot answer alone can reach a person.
    pub fn adopt_agent_drivers(
        &mut self,
        drivers: Vec<Arc<dyn neosh_provider::drivers::AgentDriver>>,
    ) {
        // Both stand behind a relay, so a prompt that would reach a person here is offered to the
        // machines watching its conversation as well. See [`crate::relay`].
        let asker: Arc<dyn neosh_provider::approval::PermissionAsker> =
            Arc::new(crate::relay::RelayAsker {
                inner: Arc::new(neosh_agent::DriverAsker::new(&self.agent, self.bridge.clone())),
                tx: self.relay_tx.clone(),
            });
        // The other half of "a request one of them cannot answer alone": not whether something may
        // happen, but which of several things you want. Policy has no answer to that one, so it
        // goes somewhere else entirely — see [`neosh_agent::DriverQuestioner`].
        let questioner: Arc<dyn neosh_provider::ask::QuestionAsker> =
            Arc::new(crate::relay::RelayQuestioner {
                inner: Arc::new(neosh_agent::DriverQuestioner::new(&self.agent, self.bridge.clone())),
                tx: self.relay_tx.clone(),
            });
        // Where an agent that speaks unprompted is heard. One per workspace rather than one per
        // driver: what comes back is the conversation it happened in, which is all the host needs
        // to know where to put it.
        let unasked: Arc<dyn neosh_provider::drivers::Unasked> =
            Arc::new(UnaskedSink(self.unasked_tx.clone()));
        let store = self.image_store();
        for d in &drivers {
            d.set_permission_asker(asker.clone());
            d.set_question_asker(questioner.clone());
            d.set_unasked(unasked.clone());
            d.set_image_store(store.clone());
        }
        self.agent_drivers = drivers;
        self.sync_agent_drivers();
    }

    /// The characters this terminal gets to draw the transcript with.
    fn glyphs(&self) -> Glyphs {
        Glyphs::new(self.option_bool("ui.ascii_only"))
    }

    /// Move off a model that cannot authenticate, if there is somewhere sensible to move to.
    ///
    /// The case this exists for: a conversation started months ago against an API-key provider is
    /// reopened on a machine with no key, and every launch greets you with "no key" over a model
    /// you never chose today. A stored selection is a *record of what was used*, not an instruction
    /// — so when it has stopped working, following it is the wrong kind of faithful.
    ///
    /// What `agent.model` says is a different matter and is left alone: that is a person telling
    /// neosh which model to use, and quietly using another one would be worse than the error.
    ///
    /// The replacement keeps the product line where it can — `anthropic/claude-opus-5` becomes the
    /// Opus you have on a plan, not whatever sorts first — because "same model, different way of
    /// paying for it" is a switch nobody needs to think about, and any other switch is one they do.
    fn ensure_usable_selection(&mut self) {
        if self.selection_pinned {
            return;
        }
        let Some(current) = self.agent.selection() else { return };
        if self.selection_is_usable(&current) {
            return;
        }

        let replacement = {
            let reg = self.agent.providers();
            let family = reg
                .instances()
                .find(|i| i.id == current.instance)
                .and_then(|i| i.models.iter().find(|m| m.id == current.model))
                .and_then(|m| m.family.clone());
            let same_line = family.as_ref().and_then(|f| {
                reg.ready_instances().find_map(|i| {
                    let m = i
                        .models
                        .iter()
                        .filter(|m| m.family.as_ref() == Some(f) && !m.legacy)
                        .max_by_key(|m| m.tier.map(|t| t.rank()).unwrap_or(0))?;
                    Some(neosh_proto::ModelSelection {
                        instance: i.id.clone(),
                        model: m.id.clone(),
                        options: catalog::default_options(&m.capabilities),
                    })
                })
            });
            same_line.or_else(|| reg.default_selection())
        };

        let Some(next) = replacement else {
            // Nothing works. Say so once, here, rather than letting the first message find out.
            self.editor_message(
                MessageLevel::Warn,
                format!(
                    "{} cannot authenticate, and nothing else is configured. Press ^P to add a key.",
                    current.instance
                ),
            );
            return;
        };

        let note = format!(
            "{}/{} cannot authenticate \u{2014} using {}/{} instead.",
            current.instance, current.model, next.instance, next.model
        );
        self.select_model(next);
        self.editor_message(MessageLevel::Info, note);
        self.refresh_status();
    }

    /// Set the model this conversation will use, and tell everyone.
    ///
    /// Every path that changes a selection goes through here. There are five of them — a plugin
    /// asking, `agent.model` resolving, a stored model that cannot authenticate being moved off, a
    /// provider registering late, and the first usable one at startup — and only the first is a
    /// deliberate act by somebody who could plausibly redraw their own footer. The other four used
    /// to change the model silently, leaving the strip naming a model that was no longer in use and
    /// a context meter measuring against the wrong window.
    fn select_model(&mut self, selection: neosh_proto::ModelSelection) {
        self.agent.set_selection(selection.clone());
        // Into a turn that is already running, where the driver can say so. Picking a model
        // mid-turn is something you do *because* of how the turn is going, and one that waited for
        // the next turn would arrive after you had already given up on this one.
        let here = self.active_session();
        for d in &self.agent_drivers {
            d.set_model(&here, &selection);
        }
        self.broadcast(PluginEvent::SelectionChanged { selection });
    }

    /// Whether a turn sent with this selection could authenticate.
    fn selection_is_usable(&self, s: &neosh_proto::ModelSelection) -> bool {
        let reg = self.agent.providers();
        reg.instances()
            .find(|i| i.id == s.instance)
            .is_some_and(|i| neosh_provider::credentials::credentials().source(i).is_usable())
    }

    /// The shortcuts the host itself owns. Everything else on that row comes from a plugin.
    fn seed_hints(&mut self) {
        let ascii = self.option_bool("ui.ascii_only");
        let (send, newline) =
            if ascii { ("Enter", "S-Enter") } else { ("\u{23ce}", "\u{21e7}\u{23ce}") };
        self.hints.insert(format!("{BUILTIN}:send"), neosh_proto::Hint {
            keys: send.into(),
            label: "send".into(),
            priority: 0,
        });
        self.hints.insert(format!("{BUILTIN}:newline"), neosh_proto::Hint {
            keys: newline.into(),
            label: "newline".into(),
            priority: 1,
        });
    }

    fn option_bool(&self, name: &str) -> bool {
        self.editor.options().bool(name).unwrap_or(false)
    }

    fn option_usize(&self, name: &str) -> usize {
        self.editor.options().int(name).unwrap_or(0).max(0) as usize
    }

    /// Keys nothing claimed.
    ///
    /// In chat mode this is the composer, which is a real text field rather than a string things
    /// get appended to: the cursor is somewhere, motions move it, and edits happen where it is.
    /// `<Esc>` always interrupts, in every mode, because a key that stops the thing that is running
    /// must not depend on where you happen to be.
    fn handle_unbound_key(&mut self, key: neosh_proto::KeyPress, mode: Mode) {
        if self.v().reading {
            self.reading_key(key);
            return;
        }
        if key.code == KeyCode::Esc {
            if self.cancel_turn_here() == Some(true) {
                self.editor_message(MessageLevel::Info, "interrupted");
            }
            return;
        }
        if mode != Mode::Chat {
            return;
        }

        let m = key.mods;
        match key.code {
            // A newline where a send would be: the composer is multi-line, and pasting a snippet
            // into it should not fire off half a question.
            KeyCode::Enter if m.shift || m.alt => self.compose(TextEdit::Insert { text: "\n".into() }),
            KeyCode::Enter => {
                let text = self.composer_text();
                if text.trim().is_empty() {
                    return;
                }
                self.set_composer("");
                self.start_turn(text);
            }

            // Nothing left to erase, and a chip above the field: the next backspace takes that
            // off. It is how a chip row behaves everywhere else, it needs no modifier anybody's
            // terminal has an opinion about, and "that was the wrong screenshot" is one key away
            // from having pasted it.
            KeyCode::Backspace if self.composer_text().is_empty() && self.has_attachment() => {
                self.drop_attachment()
            }
            KeyCode::Backspace if m.ctrl || m.alt => self.compose(TextEdit::DeleteWordBack),
            KeyCode::Backspace => self.compose(TextEdit::DeleteBack),
            KeyCode::Delete if m.ctrl || m.alt => self.compose(TextEdit::DeleteWordForward),
            KeyCode::Delete => self.compose(TextEdit::DeleteForward),

            KeyCode::Left if m.ctrl || m.alt => self.move_composer(CursorMotion::WordLeft, m.shift),
            KeyCode::Left => self.move_composer(CursorMotion::Left, m.shift),
            KeyCode::Right if m.ctrl || m.alt => {
                self.move_composer(CursorMotion::WordRight, m.shift)
            }
            KeyCode::Right => self.move_composer(CursorMotion::Right, m.shift),

            // Vertical keys do double duty, decided by where the cursor already is. On the first
            // line of a one-line composer — which is nearly always — `↑` is how you look back at
            // what was said, and making you learn a second key for that would be absurd.
            KeyCode::Up if self.composer_cursor().0 == 0 && !m.shift => self.scroll_chat(-1),
            KeyCode::Up => self.move_composer(CursorMotion::Up, m.shift),
            KeyCode::Down if self.composer_at_last_line() && !m.shift => self.scroll_chat(1),
            KeyCode::Down => self.move_composer(CursorMotion::Down, m.shift),

            KeyCode::Home if m.ctrl => self.move_composer(CursorMotion::BufStart, m.shift),
            KeyCode::Home => self.move_composer(CursorMotion::LineStart, m.shift),
            KeyCode::End if m.ctrl => self.move_composer(CursorMotion::BufEnd, m.shift),
            KeyCode::End => self.move_composer(CursorMotion::LineEnd, m.shift),

            // Readline, because this is a terminal and these are the keys terminal fingers have.
            KeyCode::Char { ref c } if m.ctrl => match c.as_str() {
                "a" => self.move_composer(CursorMotion::LineStart, false),
                "u" => self.compose(TextEdit::DeleteToLineStart),
                "w" => self.compose(TextEdit::DeleteWordBack),
                _ => {}
            },
            // A key press carries a grapheme, not a `char`: a dead-key accent and the letter it
            // lands on arrive together, and inserting them separately would put the cursor between
            // a letter and its own diacritic.
            KeyCode::Char { ref c } if !m.alt => {
                self.compose(TextEdit::Insert { text: c.clone() })
            }
            _ => {}
        }
    }

    fn compose(&mut self, edit: TextEdit) {
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::WinEdit {
            win: self.v().composer_win,
            edit,
        });
    }

    fn move_composer(&mut self, motion: CursorMotion, select: bool) {
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::WinMotion {
            win: self.v().composer_win,
            motion,
            select,
        });
    }

    fn composer_cursor(&self) -> (u32, u32) {
        self.editor.window(self.v().composer_win).map(|w| w.cursor).unwrap_or((0, 0))
    }

    fn composer_at_last_line(&self) -> bool {
        let last = self
            .editor
            .buffer(self.v().composer)
            .map(|b| b.line_count().saturating_sub(1))
            .unwrap_or(0);
        self.composer_cursor().0 >= last
    }

    /// What is selected in a window, or nothing.
    fn selection(&mut self, win: WindowId) -> String {
        match self.editor.apply(&PluginId::from(BUILTIN), ApiCall::WinSelection { win }) {
            Ok(ApiOk::Text { text }) => text,
            _ => String::new(),
        }
    }

    /// Copy what is selected, in whichever half of the screen you are looking at.
    ///
    /// Returns whether there was anything to copy, so `<C-c>` can fall through to interrupting when
    /// there is not — which is the behaviour every terminal has and the reason this key can carry
    /// both jobs without ever being ambiguous.
    fn copy_selection(&mut self, cut: bool) -> bool {
        let win = if self.v().reading { self.v().chat_win } else { self.v().composer_win };
        let text = self.selection(win);
        if text.is_empty() {
            return false;
        }
        let lines = text.lines().count();
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::ClipboardWrite { text });
        if cut && !self.v().reading {
            let _ = self.editor.apply(&plugin, ApiCall::WinEdit {
                win,
                edit: TextEdit::DeleteSelection,
            });
        } else {
            let _ = self.editor.apply(&plugin, ApiCall::WinSelect { win, on: false });
        }
        self.editor_message(
            MessageLevel::Info,
            format!("{} {lines} line{}", if cut { "cut" } else { "copied" }, if lines == 1 { "" } else { "s" }),
        );
        true
    }

    // ---- typing in a key -------------------------------------------------

    /// Start collecting a key for `instance`, borrowing the composer to do it.
    ///
    /// Returns the error a caller should be told about rather than opening a prompt that cannot
    /// lead anywhere: a CLI login and a local endpoint have nothing to do with a key, and asking
    /// for one would be asking for something that gets thrown away.
    fn begin_secret(
        &mut self,
        instance: neosh_proto::InstanceId,
        replace: bool,
        waiting: Option<(PluginId, RequestId)>,
    ) -> Result<(), ApiError> {
        if self.v().secret.is_some() {
            return Err(ApiError::Busy { message: "a key is already being entered".into() });
        }
        let (label, accepts, present) = {
            let providers = self.agent.providers();
            let cfg = providers
                .instance(&instance)
                .ok_or_else(|| ApiError::NotFound { what: format!("instance {instance}") })?;
            let store = neosh_provider::credentials::credentials();
            let source = store.source(cfg);
            (
                cfg.display_name.clone(),
                !matches!(cfg.auth, neosh_proto::AuthRef::Inherited | neosh_proto::AuthRef::None),
                source.is_usable(),
            )
        };
        if !accepts {
            return Err(ApiError::InvalidArgument {
                message: format!("{instance} signs in on its own — it has nowhere to put a key"),
            });
        }
        if present && !replace {
            return Err(ApiError::InvalidArgument {
                message: format!("{instance} already has a key — pass `replace` to change it"),
            });
        }

        // Leave the transcript first, so the caret is where the bullets are.
        self.leave_reading();
        let draft = self.composer_text();
        self.vm().secret = Some(SecretPrompt { instance, label, value: String::new(), draft, waiting });
        self.chat_line("");
        self.chat_line("Paste or type the key, then Enter. Esc cancels. Nothing is echoed.");
        self.render_secret();
        self.refresh_status();
        Ok(())
    }

    /// Draw the prompt: a label, and one bullet per grapheme.
    ///
    /// Bullets rather than nothing at all, because a paste that silently did not arrive is the
    /// failure people actually hit. Bullets rather than the value, because this line is a buffer
    /// and a buffer is sent to the frontend.
    fn render_secret(&mut self) {
        let Some(p) = self.v().secret.as_ref() else { return };
        let n = p.value.graphemes(true).count();
        // Capped: a 200-character key would push the label off a narrow composer. The overflow
        // count is what tells you a long paste landed.
        let shown = n.min(32);
        let mut mask = "\u{2022}".repeat(shown);
        if n > shown {
            mask.push_str(&format!(" +{}", n - shown));
        }
        let line = format!("key for {}  {mask}", p.label);
        self.set_composer(&line);
    }

    /// Every key while a key is being typed.
    ///
    /// Deliberately reached *before* the keymaps: while this prompt is up no binding fires, so a
    /// stray `n` in the middle of a pasted key cannot start a conversation.
    fn secret_key(&mut self, key: neosh_proto::KeyPress) {
        let Some(p) = self.vm().secret.as_mut() else { return };
        match key.code {
            KeyCode::Esc => {
                self.finish_secret(false);
                return;
            }
            KeyCode::Enter => {
                self.finish_secret(true);
                return;
            }
            KeyCode::Backspace => {
                // By grapheme, like everything else here that steps through text.
                let keep = p.value.graphemes(true).count().saturating_sub(1);
                p.value = p.value.graphemes(true).take(keep).collect();
            }
            KeyCode::Char { ref c } if key.mods.ctrl => match c.as_str() {
                "u" => p.value.clear(),
                "c" => {
                    self.finish_secret(false);
                    return;
                }
                _ => return,
            },
            KeyCode::Char { ref c } if !key.mods.alt => p.value.push_str(c),
            _ => return,
        }
        self.render_secret();
    }

    /// Pasting is how a key actually arrives.
    ///
    /// Surrounding whitespace is stripped: a key copied out of a web page brings a newline with it,
    /// and a trailing newline in a bearer token is a 401 that looks exactly like a wrong key.
    fn secret_paste(&mut self, text: &str) {
        let Some(p) = self.vm().secret.as_mut() else { return };
        p.value.push_str(text.trim());
        self.render_secret();
    }

    /// Store what was typed, or throw it away, and give the composer back.
    fn finish_secret(&mut self, submit: bool) {
        let Some(mut p) = self.vm().secret.take() else { return };
        let mut stored = false;
        if submit {
            let value = std::mem::take(&mut p.value);
            let trimmed = value.trim();
            if trimmed.is_empty() {
                self.editor_message(MessageLevel::Warn, "nothing entered");
            } else {
                let secret = secrecy::SecretString::from(trimmed);
                let where_it_went =
                    neosh_provider::credentials::credentials().set(&p.instance, secret);
                stored = true;
                let label = p.label.clone();
                match where_it_went {
                    neosh_provider::credentials::Stored::Keychain => self.editor_message(
                        MessageLevel::Info,
                        format!("{label}: key saved to the keychain"),
                    ),
                    // Still usable, just not tomorrow — which is worth saying plainly rather than
                    // letting it be a surprise at the next start.
                    neosh_provider::credentials::Stored::Session => self.editor_message(
                        MessageLevel::Warn,
                        format!(
                            "{label}: key kept for this run only — no keychain available. \
                             Install `secret-tool`, or point `auth` at a command in config.toml."
                        ),
                    ),
                }
            }
        }
        let draft = std::mem::take(&mut p.draft);
        self.set_composer(&draft);
        self.refresh_status();
        if let Some((plugin, id)) = p.waiting.take() {
            let response: ApiResponse = Ok(ApiOk::Bool { value: stored }).into();
            let _ = self.bridge.script().send(ScriptInbound::Plugin {
                plugin,
                msg: PluginInbound::Response { id, response },
            });
        }
        // `p` drops here, which zeroes whatever is left of the value.
    }
    // ---- reading the transcript ------------------------------------------
    //
    // A modal reader over the chat buffer. The keys are vi's, chosen rather than invented: `j` and
    // `y` mean here what they mean everywhere else, and the two things this transcript has that a
    // file does not — turns and code blocks — get keys of their own rather than being approximated
    // with line numbers.

    /// Move into the transcript to read, navigate and select it.
    ///
    /// The composer is where you type; this is where you go to take something out. It is a separate
    /// place rather than a focus change because the keys mean different things there — `j` moves
    /// rather than types — and pretending otherwise is how modeless editors end up with twelve
    /// chords nobody remembers.
    fn enter_reading(&mut self) {
        if self.v().reading {
            return;
        }
        self.vm().reading = true;
        self.vm().reading_pending = None;
        self.vm().reading_shape = SelectShape::Exclusive;
        self.vm().reading_goal = None;
        // And the keymap has to be told, or the sentence above is only true of the keys nobody
        // else wanted. Reading's keys arrive through [`Self::handle_unbound_key`], which is the
        // *last* thing consulted — so `^D` scrolled half a screen only until the git plugin bound
        // it in `chat` mode, after which it opened a diff, and `^B` toggled the sidebar instead of
        // going back a page, and `^E` opened the effort picker instead of moving a line. Half of a
        // documented vim vocabulary, quietly taken by whatever loaded first.
        //
        // `Normal` rather than a `Reading` mode of its own: the keys `chat` carries are the ones
        // that compose a message, and none of them means anything here. What stays bound is the
        // set the host puts in *every* mode — `^C`, `^Q`, `^R`, `^S` — which is exactly the set you
        // want to keep working somewhere you did not intend to be. A plugin that wants a key while
        // reading binds it in `normal`, which needs nothing new.
        self.vm().mode_before_reading = self.editor.mode(self.from.view);
        self.editor.set_mode(self.from.view, Mode::Normal);
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::FocusPush { win: self.v().chat_win });
        let _ = self.editor.apply(&plugin, ApiCall::WinSelect { win: self.v().chat_win, on: false });
        // Start at the newest thing said rather than at line one: you came here to take something
        // out of the answer you are looking at.
        let lines = self.chat_lines();
        let last = lines.len().saturating_sub(1) as u32;
        // A block, because the cursor is now *on* a character rather than between two and every
        // key is a verb. It is also the only thing on screen that says so before you press one.
        let _ = self.editor.apply(&plugin, ApiCall::WinCursorShape {
            win: self.v().chat_win,
            shape: neosh_proto::CursorShape::Block,
        });
        self.reading_jump((last, vim::first_non_blank(lines.last().map(String::as_str).unwrap_or(""))));
        self.refresh_status();
        self.refresh_composer();
    }

    fn leave_reading(&mut self) {
        if !self.v().reading {
            return;
        }
        self.vm().reading = false;
        self.drop_preview();
        self.vm().reading_pending = None;
        self.vm().reading_count.clear();
        self.vm().reading_shape = SelectShape::Exclusive;
        self.vm().reading_goal = None;
        self.editor.set_mode(self.from.view, self.v().mode_before_reading);
        self.clear_matches();
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::WinCursorShape {
            win: self.v().chat_win,
            shape: neosh_proto::CursorShape::Bar,
        });
        let _ = self.editor.apply(&plugin, ApiCall::WinSelect { win: self.v().chat_win, on: false });
        // Named, because giving focus back is about a keyboard: a window names its own view, but
        // *popping* names none, and the local view is not this terminal in a served workspace.
        let here = self.serving();
        let _ = self.editor.apply_in(here, &plugin, ApiCall::FocusPop);
        // Back to following the newest, because that is what the composer is the bottom of.
        //
        // Reading is a place *in* the transcript and the scroll offset is how it gets there —
        // `reading_jump` sets it on every motion. Left behind, coming back out put you in the
        // composer with the window still parked a hundred rows up: the next thing you typed, the
        // spinner, and the whole answer all arrived below the screen, and what you could still
        // see was old text that had not changed. Every other way *into* a conversation resets
        // this for the same reason — see `enter_session`.
        self.scroll_chat(0);
        self.refresh_status();
        self.refresh_composer();
    }

    /// Keys while reading the transcript.
    ///
    /// Vim's, and resolved by [`crate::vim`] rather than by the core's [`CursorMotion`]: those are
    /// a text field's motions, and a mode where the cursor sits *on* a character rather than
    /// between two needs the other set of answers to `h` at column zero, to `$`, and to what a
    /// selection with one end on the cursor contains. Motion extends a running selection without
    /// being told to, which is what makes `v` and then three `j`s three lines.
    fn reading_key(&mut self, key: neosh_proto::KeyPress) {
        if self.v().search.is_some() {
            self.search_key(key);
            return;
        }
        let ch = match &key.code {
            KeyCode::Char { c } => c.as_str(),
            _ => "",
        };

        // `f`, `F`, `t` and `T` take the next keystroke as a *character* rather than as a command:
        // `f5` finds a five and `fj` finds a `j`. Before the count and before every table below,
        // each of which would otherwise claim it first.
        if let Some(Pending::Find { till, back }) = self.v().reading_pending {
            self.vm().reading_pending = None;
            let count = self.take_reading_count().unwrap_or(1);
            if let Some(c) = ch.chars().next() {
                self.vm().reading_find = Some((c, till, back));
                self.reading_motion(vim::Motion::Find { ch: c, till, back }, count);
            }
            self.refresh_composer();
            return;
        }

        // A count, typed before the motion it belongs to. Digits first, because `0` is also a
        // motion: it is the start of the line when nothing is pending and part of the number when
        // something is, which is what it means everywhere else these keys come from.
        if !key.mods.ctrl
            && !key.mods.alt
            && let Some(d) = ch.chars().next().filter(char::is_ascii_digit)
            && !(d == '0' && self.v().reading_count.is_empty())
        {
            // Three digits is already more rows than any transcript has on screen, and it is what
            // stops a leant-on key growing a string without end.
            if self.v().reading_count.len() < 3 {
                self.vm().reading_count.push(d);
            }
            self.refresh_composer();
            return;
        }
        // Whatever was typed, the motion that follows is the only thing that spends it — so take it
        // here and let every arm below run with it already gone. A key that is not a motion
        // therefore ends the count, which is what makes a half-typed one abandonable.
        let typed = self.take_reading_count();
        let count = typed.unwrap_or(1);

        // Chords first — before the pending pair and before the plain motions, because a chord is
        // a different key and not a modified one. Read after them, `^B` was matched by the `b` arm
        // of the motion table and moved a word left, `^Y` opened a pending yank, and `^E` moved a
        // word right: three of the six scroll keys taken by the unmodified letters they happen to
        // share. `<C-d>`/`<C-u>` are half a screen because that is what they are everywhere; a full
        // screen with no overlap loses the line you were reading.
        //
        // A screen is counted in *buffer rows on the screen*, which the frontend reports and the
        // window's height is not: a transcript of paragraphs draws six rows in thirty cells, and
        // half a screen taken as half the height is four screenfuls of scrolling.
        let page = self.reading_page();
        let n = count as i64;
        if key.mods.ctrl {
            match ch {
                "d" => return self.reading_by(n * ((page as i64) + 1) / 2),
                "u" => return self.reading_by(-(n * ((page as i64) + 1) / 2)),
                "f" => return self.reading_by(n * page as i64),
                "b" => return self.reading_by(-(n * page as i64)),
                "e" => return self.reading_scroll(n),
                "y" => return self.reading_scroll(-n),
                // The one Vim key that genuinely is not here. Said rather than swallowed, because
                // a chord that does nothing at all reads as a broken keyboard.
                "v" => {
                    return self.editor_message(
                        MessageLevel::Info,
                        "no blockwise selection here — `v` takes characters and `V` whole lines",
                    );
                }
                _ => {}
            }
        }
        // A chord this mode does not define does *nothing*. Falling through, `^A` was read as `a`
        // and put you back in the composer, `^W` as `w` and moved a word — a key that means
        // something else here quietly meaning the letter it is made of. Alt as well as ctrl, and
        // deliberately not shift: shift *is* a modifier on a motion here, and holding it is how a
        // selection is extended.
        if key.mods.ctrl || key.mods.alt {
            return;
        }

        // A pending first key consumes exactly one more press, whatever it is. Anything else and a
        // mistyped `y` would sit there waiting to eat an unrelated keystroke later.
        if let Some(first) = self.vm().reading_pending.take() {
            self.refresh_composer();
            self.reading_pair(first, ch, typed);
            return;
        }

        // The keys that cannot act on their own.
        //
        // `y` is the only one of them that is two things: an operator when nothing is selected, and
        // "copy this" when something is. `g` and `z` mean the same either way, and gating them
        // alongside it made both dead — `v` then `gg` could not extend to the top of the transcript
        // and `zz` could not recentre a selection you were in the middle of making. `i` and `a` are
        // the other way round: outside a selection they are how you get back to the composer, and
        // only inside one are they the front half of a text object.
        let opener = match ch {
            "g" => Some(Pending::G),
            "z" => Some(Pending::Z),
            "y" if !self.chat_anchored() => Some(Pending::Yank),
            "f" => Some(Pending::Find { till: false, back: false }),
            "F" => Some(Pending::Find { till: false, back: true }),
            "t" => Some(Pending::Find { till: true, back: false }),
            "T" => Some(Pending::Find { till: true, back: true }),
            "i" | "a" if self.chat_anchored() => {
                Some(Pending::Object { around: ch == "a", yank: false })
            }
            _ => None,
        };
        if let Some(p) = opener {
            self.vm().reading_pending = Some(p);
            // Put the count back for the key that finishes the pair. `5gg` is one motion with a
            // number in front of it, and the first `g` is not the thing that spends it.
            if let Some(n) = typed {
                self.vm().reading_count = n.to_string();
            }
            self.refresh_composer();
            return;
        }

        // `G` with a count in front of it is a *row* — `12G` — and it is the one motion here where
        // the number is the destination rather than how many times to go. Before the table, because
        // afterwards it is a repetition like everything else in it.
        if ch == "G"
            && let Some(n) = typed
        {
            return self.reading_motion(vim::Motion::Row(n.saturating_sub(1)), 1);
        }

        use vim::Motion as M;
        let motion = match (&key.code, ch) {
            (KeyCode::Left, _) | (_, "h") => Some(M::Left),
            (KeyCode::Right, _) | (_, "l") | (_, " ") => Some(M::Right),
            (KeyCode::Up, _) | (_, "k") => Some(M::Up),
            (KeyCode::Down, _) | (_, "j") => Some(M::Down),
            (_, "w") => Some(M::WordRight { big: false }),
            (_, "W") => Some(M::WordRight { big: true }),
            (_, "b") => Some(M::WordLeft { big: false }),
            (_, "B") => Some(M::WordLeft { big: true }),
            (_, "e") => Some(M::WordEnd { big: false }),
            (_, "E") => Some(M::WordEnd { big: true }),
            (KeyCode::Home, _) | (_, "0") => Some(M::LineStart),
            (_, "^") => Some(M::FirstNonBlank),
            (KeyCode::End, _) | (_, "$") => Some(M::LineEnd),
            (_, "G") => Some(M::BufEnd),
            (_, "%") => Some(M::MatchPair),
            // `;` repeats the last `f`/`t` and `,` repeats it the other way. Nothing to repeat is
            // not an error — it is a key pressed before the one it belongs to.
            (_, ";") => self.v().reading_find.map(|(c, till, back)| M::Find { ch: c, till, back }),
            (_, ",") => self.v().reading_find.map(|(c, till, back)| M::Find { ch: c, till, back: !back }),
            _ => None,
        };
        if let Some(m) = motion {
            // Shift on an arrow starts a selection the way it does in a text field. Only ever the
            // arrows and the ends: `h` and `H` are different keys, not one key and a modifier.
            if key.mods.shift && !self.chat_anchored() {
                self.begin_select(SelectShape::Inclusive);
            }
            return self.reading_motion(m, count);
        }

        match (&key.code, ch) {
            (KeyCode::PageDown, _) => self.reading_by(n * page as i64),
            (KeyCode::PageUp, _) => self.reading_by(-(n * page as i64)),

            // The two things a transcript has that a file does not.
            (_, "]") => self.reading_to_turn(1),
            (_, "[") => self.reading_to_turn(-1),
            (_, "}") => self.reading_to_block(1),
            (_, "{") => self.reading_to_block(-1),
            // And the third: what the turn actually did. `c` rather than a bracket pair because
            // both of those are spent, and it is free here — nothing in this mode edits, so Vim's
            // `c` has nothing to collide with. The card opens as you land on it.
            (_, "c") => self.reading_to_card(1, count),
            (_, "C") => self.reading_to_card(-1, count),

            // Where the window is, rather than where the text is.
            (_, "H") => self.reading_screen_row(count.saturating_sub(1)),
            (_, "M") => self.reading_screen_row(page / 2),
            (_, "L") => self.reading_screen_row(page.saturating_sub(count.max(1))),

            (_, "v") => self.select_as(SelectShape::Inclusive),
            (_, "V") => self.select_as(SelectShape::Line),
            // Vim's `o`: the anchor becomes the cursor and the cursor the anchor, so the end you
            // are extending is the other one. Without it a selection begun in the wrong direction
            // is a selection to start again.
            (_, "o") if self.chat_anchored() => self.swap_ends(),

            // A folded card opens; an open one folds. `Tab` because it is the key that toggles a
            // fold in every file tree and outliner; `za` for the Vim hands.
            (KeyCode::Tab, _) => self.toggle_card_here(),

            (_, "/") => self.begin_search(false),
            (_, "?") => self.begin_search(true),
            (_, "n") => self.search_step(true),
            (_, "N") => self.search_step(false),
            // The word under the cursor, searched for without typing it.
            (_, "*") => self.search_word(true),
            (_, "#") => self.search_word(false),

            // Yank with a selection running takes the selection, and leaves — you came here to
            // take something, and staying would mean pressing Esc every single time. Without one,
            // `y` has already been held as a pending operator above.
            (_, "y") if self.copy_selection(false) => self.leave_reading(),
            // Anchored, but over nothing at all — which an inclusive selection cannot be, since it
            // always contains the character the cursor is on. Only a `V` on an empty row gets here.
            (_, "y") => {
                self.editor_message(MessageLevel::Warn, "nothing to copy — the selection is empty")
            }
            (_, "Y") => self.yank_line(),

            // Back to typing, at the key every editor uses for it, because the usual reason to
            // stop reading is that you have thought of what to say.
            (KeyCode::Enter, _) | (_, "i") | (_, "a") | (_, "o") => self.leave_reading(),
            // Esc gives up one thing per press, in the order you stopped wanting them: the
            // selection, then the search highlight, then the mode. A running selection is the most
            // local of the three and the one you are most likely to have changed your mind about —
            // half way up a turn, having decided you wanted a different piece of it. Leaving
            // outright there is a keystroke that undoes the wrong amount: the transcript is the
            // thing you were still reading.
            (KeyCode::Esc, _) if self.chat_anchored() => self.drop_selection(),
            (KeyCode::Esc, _) if !self.v().searched.is_empty() => {
                self.vm().searched.clear();
                self.clear_matches();
                self.refresh_composer();
            }
            (KeyCode::Esc, _) | (_, "q") => self.leave_reading(),
            _ => {}
        }
    }

    /// The second key of a pair: `gg`, `ge`, `gv`, `zz`, the `y` operator and a text object.
    ///
    /// `typed` is the count the *first* key put back for this one — `5gg` is one motion with a
    /// number in front of it, and the `g` that opened the pair is not what spends it.
    fn reading_pair(&mut self, first: Pending, ch: &str, typed: Option<u32>) {
        // A key with no character on it — `Esc`, an arrow, `PgDn` — ends the sequence and does
        // nothing else. Quietly: pressing `Esc` because you have changed your mind about a yank
        // should not be answered with a sentence about what `y` takes.
        if ch.is_empty() {
            return;
        }
        let count = typed.unwrap_or(1);
        let page = self.reading_page();
        use vim::Motion as M;
        match (first, ch) {
            (Pending::G, "g") => match typed {
                Some(n) => self.reading_motion(M::Row(n.saturating_sub(1)), 1),
                None => self.reading_motion(M::BufStart, 1),
            },
            (Pending::G, "e") => self.reading_motion(M::WordEndBack { big: false }, count),
            (Pending::G, "E") => self.reading_motion(M::WordEndBack { big: true }, count),
            (Pending::G, "_") => self.reading_motion(M::LastNonBlank, count),
            // The selection you last gave up, back again. What makes `Esc` cheap: dropping one to
            // look at something is otherwise dropping one you have to make again.
            (Pending::G, "v") => self.reselect(),

            // Where the cursor's line sits on screen. `zz` is the one worth having: it is how you
            // read an answer that ends at the bottom edge.
            (Pending::Z, "z" | ".") => self.place_cursor_line(page / 2),
            (Pending::Z, "t") => self.place_cursor_line(0),
            (Pending::Z, "b" | "-") => self.place_cursor_line(page.saturating_sub(1)),
            (Pending::Z, "a") => self.toggle_card_here(),

            (Pending::Yank, "y") => self.yank_line(),
            (Pending::Yank, "c") => self.yank_code(),
            (Pending::Yank, "m" | "t") => self.yank_turn(),
            (Pending::Yank, "p") => self.yank_path(),
            (Pending::Yank, "a") => {
                self.select_all(self.v().chat_win);
                if self.copy_selection(false) {
                    self.leave_reading();
                }
            }
            // `yi"`, `yiw`. Only the `i` half: `ya` is already "copy the whole transcript", which
            // is documented, bound and older than this — so `yaw` is the one shape of this that
            // cannot be had, and `viwy` is how you get it.
            (Pending::Yank, "i") => {
                self.vm().reading_pending = Some(Pending::Object { around: false, yank: true });
                self.refresh_composer();
            }
            // `y` over a motion: `yw`, `y$`, `y5j`, `yG`. Whatever the motion covers, from where
            // the cursor is to where it would have gone.
            (Pending::Yank, _) => match self.yank_motion_for(ch) {
                Some(m) => self.yank_over(m, count),
                None => self.editor_message(
                    MessageLevel::Info,
                    "y then a motion, or one of y c m p a — `?` lists them",
                ),
            },

            (Pending::Object { around, yank }, _) => {
                if let Some(obj) = object_for(ch) {
                    self.apply_object(obj, around, yank);
                }
            }
            // Answered above, before the count and before every table: it takes a literal
            // character, so it can never reach here.
            (Pending::Find { .. }, _) => {}
            _ => {}
        }
    }

    /// The motions `y` will take as its second key.
    ///
    /// Not every motion in the table: `yy` is the line and `yc`, `ym`, `yp` and `ya` are the
    /// transcript's own four, so the letters they use are theirs. Everything here is a key that
    /// means the same after `y` as it does on its own.
    fn yank_motion_for(&self, ch: &str) -> Option<vim::Motion> {
        use vim::Motion as M;
        Some(match ch {
            "w" => M::WordRight { big: false },
            "W" => M::WordRight { big: true },
            "b" => M::WordLeft { big: false },
            "B" => M::WordLeft { big: true },
            "e" => M::WordEnd { big: false },
            "E" => M::WordEnd { big: true },
            "h" => M::Left,
            "l" | " " => M::Right,
            "j" => M::Down,
            "k" => M::Up,
            "0" => M::LineStart,
            "^" => M::FirstNonBlank,
            "$" => M::LineEnd,
            "G" => M::BufEnd,
            "%" => M::MatchPair,
            ";" => {
                let (c, till, back) = self.v().reading_find?;
                M::Find { ch: c, till, back }
            }
            _ => return None,
        })
    }

    /// How many rows of transcript are on screen.
    /// How wide the transcript is, for the one thing that has to fit on a single row.
    ///
    /// Everything else in the transcript is prose and the frontend wraps it. A tool card is not
    /// prose — it is one line whose job is to be scannable, and a card that wraps puts `n .cla…)`
    /// on a row of its own, which is worse than saying less.
    fn chat_width(&self) -> usize {
        self.editor
            .window(self.v().chat_win)
            .and_then(|w| w.viewport.map(|v| v.width as usize))
            .unwrap_or(80)
            .max(24)
    }

    fn chat_height(&self) -> u32 {
        self.editor
            .window(self.v().chat_win)
            .and_then(|w| w.viewport.map(|v| v.height as u32))
            .unwrap_or(20)
            .max(1)
    }

    /// The transcript, as plain rows.
    fn chat_lines(&self) -> Vec<String> {
        self.editor
            .buffer(self.v().chat)
            .map(|b| b.lines().iter().map(|l| l.text.clone()).collect())
            .unwrap_or_default()
    }

    /// Go to a place in the transcript, extending a running selection over what was jumped.
    fn reading_jump(&mut self, at: (u32, u32)) {
        self.vm().reading_goal = None;
        self.place_cursor(at);
    }

    /// The count typed before a motion, if one was, and forget it.
    ///
    /// `None` rather than `1` for "nobody typed one", because the two are different keys: `G` is
    /// the end of the transcript and `1G` is its first row.
    fn take_reading_count(&mut self) -> Option<u32> {
        let typed = std::mem::take(&mut self.vm().reading_count);
        typed.parse::<u32>().ok().filter(|n| *n > 0)
    }

    fn chat_cursor(&self) -> (u32, u32) {
        self.editor.window(self.v().chat_win).map(|w| w.cursor).unwrap_or((0, 0))
    }

    /// Put the cursor somewhere, keeping everything that follows it in step.
    ///
    /// `WinSetCursor` rather than `WinMotion`: the anchor is the selection's other end and it has
    /// to stay exactly where it is, which is what makes a jump take the text it jumped over. The
    /// core repaints the highlight either way — it did not always, and every jump the reader makes
    /// extended a selection that could not be seen until the next `hjkl`.
    fn place_cursor(&mut self, at: (u32, u32)) {
        let at = vim::clamp(&self.chat_lines(), at);
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::WinSetCursor {
            win: self.v().chat_win,
            row: at.0,
            col: at.1,
        });
        // The card the cursor arrived in opens, and the one it left folds. Not while selecting: a
        // selection is two positions with rows between them, and a body appearing in the middle of
        // it would change what `y` copies without anything on screen having been aimed at.
        if !self.chat_anchored() {
            self.refresh_preview();
        }
        self.follow_cursor();
        // The hint row says what the row under the cursor can do, so it moves with it.
        self.refresh_composer();
    }

    /// One motion, `count` times.
    ///
    /// Repeated rather than computed: `5j` is five of what `j` does, including what it does at the
    /// ends and to the column it remembers. A motion that stops moving stops the count with it, so
    /// `50j` at the bottom of the transcript is one press that goes nowhere rather than fifty.
    fn reading_motion(&mut self, m: vim::Motion, count: u32) {
        let lines = self.chat_lines();
        let mut at = self.chat_cursor();
        let mut goal = self.v().reading_goal;
        // `$` is the third motion that leaves a column behind: it remembers "the end of the row",
        // so `$jj` walks down the right-hand edge of the text the way it does in Vim.
        let vertical = matches!(m, vim::Motion::Up | vim::Motion::Down | vim::Motion::LineEnd);
        // The ends of the transcript and an absolute row are destinations, and going to one five
        // times is going to it.
        let once = matches!(m, vim::Motion::BufStart | vim::Motion::BufEnd | vim::Motion::Row(_));
        for _ in 0..if once { 1 } else { count.max(1) } {
            let step = vim::resolve(&lines, at, m, goal);
            at = step.at;
            goal = step.goal;
            if !step.moved {
                break;
            }
        }
        // Cleared by anything horizontal, kept by `j` and `k`. Without it, going down past a short
        // row and back up leaves the cursor at that row's width and it never finds its place again.
        self.vm().reading_goal = vertical.then_some(goal).flatten();
        self.place_cursor(at);
    }

    /// Move by whole rows, keeping the column — what `^D`, `^U` and the page keys do.
    ///
    /// Expressed as that many `j`s rather than as arithmetic on a row number, so paging clamps,
    /// remembers a column and lands on a real position by exactly the same rules one keypress does.
    fn reading_by(&mut self, delta: i64) {
        if delta == 0 {
            return;
        }
        let m = if delta > 0 { vim::Motion::Down } else { vim::Motion::Up };
        self.reading_motion(m, delta.unsigned_abs().min(u32::MAX as u64) as u32);
    }

    /// `^E` and `^Y`: move the *window*, and take the cursor along only if it would fall off.
    ///
    /// The other five scroll keys move the cursor and let the view follow. These two are the pair
    /// that does it the other way round, which is the whole reason to have them.
    fn reading_scroll(&mut self, delta: i64) {
        let last = self.chat_lines().len().saturating_sub(1) as i64;
        let page = self.reading_page() as i64;
        let next = (self.chat_showing() as i64 + delta).clamp(0, last.max(0));
        self.vm().chat_top = Some(next as u32);
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::WinScrollTo {
            win: self.v().chat_win,
            top_line: Some(next as u32),
        });
        let row = self.chat_cursor().0 as i64;
        let want = row.clamp(next, (next + page - 1).min(last).max(next));
        if want != row {
            self.reading_by(want - row);
        } else {
            self.refresh_composer();
        }
    }

    /// `H`, `M` and `L`: a row counted from the top of the *window* rather than of the transcript.
    fn reading_screen_row(&mut self, offset: u32) {
        let lines = self.chat_lines();
        let last = lines.len().saturating_sub(1) as u32;
        let page = self.reading_page();
        let row = self.chat_showing().saturating_add(offset.min(page.saturating_sub(1))).min(last);
        let col = vim::first_non_blank(lines.get(row as usize).map(String::as_str).unwrap_or(""));
        self.vm().reading_goal = None;
        self.place_cursor((row, col));
    }

    /// Buffer rows on the screen, which is what a page is here.
    ///
    /// Reported by the frontend, because it is the only thing that knows: one buffer row of a
    /// wrapped transcript is several rows of screen, and the window's height counts the wrong one.
    /// Falling back to the height is what an unmeasured window gets, and it is never worse than
    /// what this used to do unconditionally.
    fn reading_page(&self) -> u32 {
        self.editor
            .window(self.v().chat_win)
            .and_then(|w| w.viewport)
            .map(|v| v.rows)
            .filter(|r| *r > 0)
            .unwrap_or_else(|| self.chat_height())
            .max(1)
    }

    /// The first buffer row on the screen right now.
    fn chat_showing(&self) -> u32 {
        self.v().chat_top.unwrap_or_else(|| {
            self.editor
                .window(self.v().chat_win)
                .and_then(|w| w.viewport)
                .map(|v| v.top_line)
                .unwrap_or(0)
        })
    }

    /// `*` and `#`: search for the word under the cursor without typing it.
    fn search_word(&mut self, forward: bool) {
        let lines = self.chat_lines();
        let Some(word) = vim::word_under(&lines, self.chat_cursor()) else {
            return self.editor_message(MessageLevel::Info, "no word under the cursor");
        };
        self.vm().searched = word;
        self.vm().searched_back = !forward;
        let query = self.v().searched.clone();
        self.highlight_matches(&query);
        match self.find_from(&query, self.chat_cursor(), forward, false) {
            Some(at) => self.reading_jump(at),
            None => self.editor_message(MessageLevel::Warn, format!("only match for {query:?}")),
        }
    }

    /// Rows where a turn begins: the first line of a question, marked by the bar down its left.
    ///
    /// Read off the buffer rather than remembered alongside it. A remembered list is one more
    /// thing to keep true across a session switch, an answer being rewritten as it settles, and a
    /// transcript rebuilt from stored messages — and it would be wrong in exactly the cases where
    /// somebody is scrolling back through a long conversation looking for something.
    fn turn_rows(&self) -> Vec<u32> {
        let bar = self.glyphs().bar;
        let lines = self.chat_lines();
        let mut out = Vec::new();
        let mut inside = false;
        for (i, line) in lines.iter().enumerate() {
            let is_bar = line.starts_with(bar);
            if is_bar && !inside {
                out.push(i as u32);
            }
            inside = is_bar;
        }
        out
    }

    fn reading_to_turn(&mut self, dir: i32) {
        let here = self.chat_cursor().0;
        let rows = self.turn_rows();
        let next = if dir > 0 {
            rows.into_iter().find(|r| *r > here)
        } else {
            rows.into_iter().rev().find(|r| *r < here)
        };
        match next {
            Some(row) => self.reading_jump((row, 0)),
            None => self.editor_message(
                MessageLevel::Info,
                if dir > 0 { "no later turn" } else { "no earlier turn" },
            ),
        }
    }

    /// The next or previous blank-line boundary.
    ///
    /// Which in this buffer is a paragraph, a list, a table and a code block all at once, because
    /// the markdown renderer separates every block with one — so `}` steps through an answer the
    /// way its author wrote it rather than by a fixed number of rows.
    fn reading_to_block(&mut self, dir: i32) {
        let lines = self.chat_lines();
        let last = lines.len().saturating_sub(1) as i64;
        let blank = |i: i64| lines.get(i as usize).is_none_or(|l| l.trim().is_empty());
        let mut at = self.chat_cursor().0 as i64;
        let step = dir as i64;
        // Off whatever run of blanks we are standing in first, then on to the next one, so holding
        // the key walks block by block instead of stalling in the gap between two.
        while at + step >= 0 && at + step <= last && blank(at + step) == blank(at) {
            at += step;
        }
        at = (at + step).clamp(0, last.max(0));
        while at + step >= 0 && at + step <= last && blank(at) {
            at += step;
        }
        self.reading_jump((at.clamp(0, last.max(0)) as u32, 0));
    }

    /// Scroll so the cursor's line lands `at` rows down the window.
    fn place_cursor_line(&mut self, at: u32) {
        let row = self.chat_cursor().0;
        let top = row.saturating_sub(at);
        self.vm().chat_top = Some(top);
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::WinScrollTo {
            win: self.v().chat_win,
            top_line: Some(top),
        });
    }

    /// `v` and `V`: start a selection of that shape, change a running one into it, or — pressed on
    /// the shape it already is — drop it.
    ///
    /// Vim's, and not out of nostalgia: a second `v` meaning "no longer selecting" is what every
    /// hand expects, and having `V` restart from one line instead would throw away the four you had
    /// just extended over.
    fn select_as(&mut self, shape: SelectShape) {
        match (self.chat_anchored(), self.v().reading_shape == shape) {
            (true, true) => self.drop_selection(),
            // Between the two shapes, keeping both ends: `v` narrows a linewise selection back to
            // where the cursor actually is, and `V` widens the one you have to whole lines.
            (true, false) => self.set_shape(shape),
            (false, _) => self.begin_select(shape),
        }
        self.refresh_composer();
    }

    /// Anchor a selection of this shape where the cursor is.
    fn begin_select(&mut self, shape: SelectShape) {
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::WinSelect { win: self.v().chat_win, on: true });
        self.set_shape(shape);
    }

    fn set_shape(&mut self, shape: SelectShape) {
        self.vm().reading_shape = shape;
        let _ = self
            .editor
            .apply(&PluginId::from(BUILTIN), ApiCall::WinSelectShape { win: self.v().chat_win, shape });
    }

    /// Give up the selection, keeping it for `gv`.
    ///
    /// Kept rather than forgotten because `Esc` is the cheap key here: you drop a selection to look
    /// at something and want it back, and one you have to make again is one you do not drop.
    fn drop_selection(&mut self) {
        if let Some(w) = self.editor.window(self.v().chat_win)
            && let Some(anchor) = w.anchor
        {
            self.vm().reading_last_select = Some((anchor, w.cursor, self.vm().reading_shape));
        }
        self.vm().reading_shape = SelectShape::Exclusive;
        let _ = self
            .editor
            .apply(&PluginId::from(BUILTIN), ApiCall::WinSelect { win: self.v().chat_win, on: false });
        self.refresh_composer();
    }

    /// `gv`: the selection you last gave up, back where it was.
    fn reselect(&mut self) {
        let Some((anchor, cursor, shape)) = self.v().reading_last_select else {
            return self.editor_message(MessageLevel::Info, "nothing selected here yet");
        };
        let lines = self.chat_lines();
        // The transcript may be shorter than it was — a conversation switch, a card that folded.
        // Clamping is what keeps `gv` from naming rows that are now somebody else's.
        self.select_between(vim::clamp(&lines, anchor), vim::clamp(&lines, cursor), shape);
    }

    /// `o`: the anchor becomes the cursor and the cursor the anchor.
    fn swap_ends(&mut self) {
        let Some(w) = self.editor.window(self.v().chat_win) else { return };
        let (Some(anchor), cursor) = (w.anchor, w.cursor) else { return };
        self.select_between(cursor, anchor, self.v().reading_shape);
    }

    /// Put a selection of `shape` between two positions, cursor at the second.
    ///
    /// Three calls, because the anchor is only settable by standing on it — which is the whole of
    /// the public API for a selection and exactly what a plugin doing this would have to write.
    fn select_between(&mut self, anchor: (u32, u32), cursor: (u32, u32), shape: SelectShape) {
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::WinSetCursor {
            win: self.v().chat_win,
            row: anchor.0,
            col: anchor.1,
        });
        let _ = self.editor.apply(&plugin, ApiCall::WinSelect { win: self.v().chat_win, on: true });
        self.set_shape(shape);
        self.vm().reading_goal = None;
        self.place_cursor(cursor);
    }

    /// `iw`, `a"`, `ip`: select what the object names, or copy it.
    fn apply_object(&mut self, obj: vim::Object, around: bool, yank: bool) {
        let lines = self.chat_lines();
        let at = self.chat_cursor();
        let Some((from, to)) = vim::object(&lines, at, obj, around) else {
            return self.editor_message(MessageLevel::Info, "the cursor is not in one of those");
        };
        if yank {
            self.yank_text(from, to, true, "it");
        } else {
            self.select_between(from, to, SelectShape::Inclusive);
        }
    }

    /// `yw`, `y$`, `y5j`: copy what a motion covers.
    fn yank_over(&mut self, m: vim::Motion, count: u32) {
        let lines = self.chat_lines();
        let from = self.chat_cursor();
        let mut at = from;
        let mut goal = None;
        let mut inclusive = false;
        let once = matches!(m, vim::Motion::BufStart | vim::Motion::BufEnd | vim::Motion::Row(_));
        for _ in 0..if once { 1 } else { count.max(1) } {
            let step = vim::resolve(&lines, at, m, goal);
            at = step.at;
            goal = step.goal;
            inclusive = step.inclusive;
            if !step.moved {
                break;
            }
        }
        // Vim's rule, and it is not a detail: a motion that spans rows takes whole ones. `yj` is
        // two lines rather than two ragged halves of them, and `yG` is the rest of the transcript
        // rather than the rest of this row and then all of the last one.
        let linewise = matches!(
            m,
            vim::Motion::Up | vim::Motion::Down | vim::Motion::Row(_) | vim::Motion::BufStart | vim::Motion::BufEnd
        );
        if linewise {
            let (a, b) = (from.0.min(at.0), from.0.max(at.0));
            self.yank_rows(a..b + 1, if a == b { "the line" } else { "the lines" }, false);
        } else {
            self.yank_text(from, at, inclusive, "it");
        }
    }

    /// Copy a range of the transcript, and say how much.
    fn yank_text(&mut self, from: (u32, u32), to: (u32, u32), inclusive: bool, what: &str) {
        let lines = self.chat_lines();
        let (a, mut b) = if from <= to { (from, to) } else { (to, from) };
        if inclusive {
            let text = lines.get(b.0 as usize).map(String::as_str).unwrap_or("");
            b.1 = neosh_core::text::after(text, b.1);
        }
        let text = vim::slice(&lines, a, b);
        if text.is_empty() {
            return self.editor_message(MessageLevel::Warn, "nothing to copy");
        }
        let n = text.lines().count().max(1);
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::ClipboardWrite { text });
        self.editor_message(
            MessageLevel::Info,
            format!("copied {what} — {n} line{}", if n == 1 { "" } else { "s" }),
        );
    }

    /// Copy some rows of the transcript, and say how many.
    fn yank_rows(&mut self, rows: std::ops::Range<u32>, what: &str, strip: bool) {
        let lines = self.chat_lines();
        let taken: Vec<String> = lines
            .iter()
            .skip(rows.start as usize)
            .take(rows.len())
            .map(|l| if strip { l.strip_prefix("  ").unwrap_or(l).to_string() } else { l.clone() })
            .collect();
        let text = taken.join("\n");
        if text.trim().is_empty() {
            self.editor_message(MessageLevel::Warn, format!("nothing to copy — no {what} here"));
            return;
        }
        let n = taken.len();
        let _ = self
            .editor
            .apply(&PluginId::from(BUILTIN), ApiCall::ClipboardWrite { text });
        self.editor_message(
            MessageLevel::Info,
            format!("copied {what} — {n} line{}", if n == 1 { "" } else { "s" }),
        );
    }

    fn yank_line(&mut self) {
        let row = self.chat_cursor().0;
        self.yank_rows(row..row + 1, "the line", false);
    }

    /// Copy the code block the cursor is in, without its indent.
    ///
    /// Found from the marks rather than from the text: a fenced block's rows are the ones the
    /// renderer coloured as code across their whole width, which is a fact it already recorded and
    /// which no amount of looking at the characters could recover — the back-ticks are gone by
    /// the time this buffer exists, because they were punctuation for the parser.
    fn yank_code(&mut self) {
        let Some(rows) = self.code_block_at(self.chat_cursor().0) else {
            self.editor_message(MessageLevel::Warn, "the cursor is not in a code block");
            return;
        };
        self.yank_rows(rows, "the code block", true);
    }

    /// Whether a row is the language line the renderer draws above a fenced block.
    fn is_fence_header_row(buf: &neosh_core::buffer::Buffer, row: u32) -> bool {
        buf.render_range(row, row + 1).first().is_some_and(|r| {
            r.marks.iter().any(|m| m.opts.hl_group.as_deref() == Some("Markdown.Fence"))
        })
    }

    fn code_block_at(&self, row: u32) -> Option<std::ops::Range<u32>> {
        let buf = self.editor.buffer(self.v().chat)?;
        let count = buf.line_count();
        if row >= count {
            return None;
        }
        // A row is code when a mark covers the whole of it with the code group. Sub-ranges are
        // inline back-ticks, which are part of a sentence and not a block to take away.
        let is_code = |i: u32| -> bool {
            buf.render_range(i, i + 1).first().is_some_and(|r| {
                !r.text.is_empty()
                    && r.marks.iter().any(|m| {
                        m.opts.hl_group.as_deref() == Some("Markdown.Code")
                            && m.col == 0
                            && m.opts.end_col == Some(r.text.len() as u32)
                    })
            })
        };
        // Standing on the language header is standing on the block: it is drawn as part of it, it
        // is the first row of it you can see, and it is where the cursor lands coming down from
        // above. Refusing there would be pedantry about a distinction only the parser cares about.
        let row = if is_code(row) {
            row
        } else if Self::is_fence_header_row(buf, row) && row + 1 < count && is_code(row + 1) {
            row + 1
        } else {
            return None;
        };
        let mut from = row;
        while from > 0 && is_code(from - 1) {
            from -= 1;
        }
        let mut to = row + 1;
        while to < count && is_code(to) {
            to += 1;
        }
        Some(from..to)
    }

    /// Copy the conversation's directory — in a worktree, the worktree's.
    ///
    /// The one `y` target that is not a piece of the transcript, here because it is the other thing
    /// you take out of a conversation: the path its shell, its editor and its next message want.
    /// Three keys every terminal sends, which is what lets the chat key be `⌥Y` — an Alt chord,
    /// `¥` on a Mac out of the box, and allowed as a default only because this route exists
    /// beside it. `session.copy.path` is the same verb by name, and the panel's `y`
    /// does it for any row.
    fn yank_path(&mut self) {
        let path = self.root().display().to_string();
        let _ = self
            .editor
            .apply(&PluginId::from(BUILTIN), ApiCall::ClipboardWrite { text: path.clone() });
        self.editor_message(MessageLevel::Info, format!("copied {path}"));
    }

    /// Copy the whole exchange the cursor is in — the question and everything it produced.
    fn yank_turn(&mut self) {
        let here = self.chat_cursor().0;
        let rows = self.turn_rows();
        let from = rows.iter().rev().find(|r| **r <= here).copied().unwrap_or(0);
        let to = rows
            .iter()
            .find(|r| **r > here)
            .copied()
            .unwrap_or_else(|| self.chat_lines().len() as u32);
        self.yank_rows(from..to, "the turn", false);
    }

    // ---- searching -------------------------------------------------------

    /// Start typing a search, borrowing the composer for it.
    fn begin_search(&mut self, back: bool) {
        if self.v().search.is_some() {
            return;
        }
        self.vm().search = Some(SearchPrompt {
            query: String::new(),
            back,
            from: self.chat_cursor(),
            draft: self.composer_text(),
        });
        self.render_search();
        self.refresh_status();
    }

    fn render_search(&mut self) {
        let Some(p) = self.v().search.as_ref() else { return };
        let line = format!("{}{}", if p.back { "?" } else { "/" }, p.query);
        self.set_composer(&line);
    }

    fn search_key(&mut self, key: neosh_proto::KeyPress) {
        let Some(p) = self.vm().search.as_mut() else { return };
        match key.code {
            KeyCode::Esc => return self.finish_search(false),
            KeyCode::Enter => return self.finish_search(true),
            KeyCode::Backspace => {
                if p.query.is_empty() {
                    return self.finish_search(false);
                }
                let keep = p.query.graphemes(true).count().saturating_sub(1);
                p.query = p.query.graphemes(true).take(keep).collect();
            }
            KeyCode::Char { ref c } if key.mods.ctrl => match c.as_str() {
                "u" => p.query.clear(),
                "c" => return self.finish_search(false),
                _ => return,
            },
            KeyCode::Char { ref c } if !key.mods.alt => p.query.push_str(c),
            _ => return,
        }
        self.render_search();
        // Incremental: the hits light up as you type, so a search that was going to find nothing
        // says so before you have finished typing it.
        let (query, back, from) = {
            let p = self.v().search.as_ref().expect("checked above");
            (p.query.clone(), p.back, p.from)
        };
        self.highlight_matches(&query);
        if !query.is_empty() && let Some(at) = self.find_from(&query, from, !back, true) {
            self.reading_jump(at);
        }
    }

    fn finish_search(&mut self, submit: bool) {
        let Some(p) = self.vm().search.take() else { return };
        self.set_composer(&p.draft);
        if submit && !p.query.is_empty() {
            self.vm().searched = p.query.clone();
            self.vm().searched_back = p.back;
            self.highlight_matches(&p.query);
            match self.find_from(&p.query, p.from, !p.back, true) {
                Some(at) => self.reading_jump(at),
                None => self.editor_message(MessageLevel::Warn, format!("no match for {:?}", p.query)),
            }
        } else {
            self.clear_matches();
            self.reading_jump(p.from);
        }
        self.refresh_status();
        self.refresh_composer();
    }

    /// `n` and `N`: on to the next hit for whatever was last searched for.
    fn search_step(&mut self, same_direction: bool) {
        if self.v().searched.is_empty() {
            self.editor_message(MessageLevel::Info, "nothing to search for yet — `/` starts one");
            return;
        }
        let query = self.v().searched.clone();
        // `n` is *onwards*, not forwards: after `?foo` it goes on up the transcript and `N` comes
        // back down it. Fixed at `forward` either way, `?` found one match and then walked away
        // from every other one.
        let forward = same_direction != self.v().searched_back;
        match self.find_from(&query, self.chat_cursor(), forward, false) {
            Some(at) => self.reading_jump(at),
            None => self.editor_message(MessageLevel::Warn, format!("no more matches for {query:?}")),
        }
    }

    /// The next match, wrapping round the end.
    ///
    /// `include_here` is the difference between typing a search — which should land on the match
    /// under the cursor if there is one — and pressing `n`, which must move.
    fn find_from(
        &self,
        query: &str,
        from: (u32, u32),
        forward: bool,
        include_here: bool,
    ) -> Option<(u32, u32)> {
        let lines = self.chat_lines();
        if lines.is_empty() || query.is_empty() {
            return None;
        }
        let needle = query.to_lowercase();
        let n = lines.len();
        // Case-insensitive unless the query has a capital in it, which is `smartcase` and is what
        // everybody has turned on anyway.
        let cased = query.chars().any(char::is_uppercase);
        let hits = |row: usize| -> Vec<u32> {
            let hay = if cased { lines[row].clone() } else { lines[row].to_lowercase() };
            let pat = if cased { query } else { needle.as_str() };
            let mut out = Vec::new();
            let mut at = 0;
            while let Some(i) = hay[at..].find(pat) {
                out.push((at + i) as u32);
                at += i + pat.len().max(1);
            }
            out
        };
        for step in 0..=n {
            let row = if forward {
                (from.0 as usize + step) % n
            } else {
                (from.0 as usize + n - (step % n)) % n
            };
            let cols = hits(row);
            let found = if step == 0 {
                let here = from.1;
                if forward {
                    cols.into_iter().find(|c| if include_here { *c >= here } else { *c > here })
                } else {
                    cols.into_iter().rev().find(|c| if include_here { *c <= here } else { *c < here })
                }
            } else if forward {
                cols.into_iter().next()
            } else {
                cols.into_iter().next_back()
            };
            if let Some(col) = found {
                return Some((row as u32, col));
            }
        }
        None
    }

    /// Colour every hit, so the shape of where a word appears is visible without walking to each.
    fn highlight_matches(&mut self, query: &str) {
        self.clear_matches();
        if query.is_empty() {
            return;
        }
        let lines = self.chat_lines();
        let cased = query.chars().any(char::is_uppercase);
        let needle = query.to_lowercase();
        let plugin = PluginId::from(BUILTIN);
        // Bounded, because a one-letter query against a long transcript is thousands of marks and
        // every one of them travels to the frontend. The cursor still walks to every hit.
        let mut left = 500usize;
        for (row, line) in lines.iter().enumerate() {
            let hay = if cased { line.clone() } else { line.to_lowercase() };
            let pat = if cased { query } else { needle.as_str() };
            let mut at = 0usize;
            while let Some(i) = hay[at..].find(pat) {
                let from = at + i;
                let _ = self.editor.apply(&plugin, ApiCall::MarkSet {
                    ns: self.search_ns,
                    buf: self.v().chat,
                    row: row as u32,
                    col: from as u32,
                    opts: neosh_proto::ExtmarkOpts {
                        end_col: Some((from + pat.len()) as u32),
                        hl_group: Some("Search".into()),
                        line_hl_group: None,
                        virt_text: Vec::new(),
                        virt_text_pos: VirtTextPos::Eol,
                        on_delete: neosh_proto::OnDelete::Invalidate,
                        priority: 50,
                        image: None,
                    },
                });
                left -= 1;
                if left == 0 {
                    return;
                }
                at = from + pat.len().max(1);
            }
        }
    }

    fn clear_matches(&mut self) {
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::MarkClear {
            ns: self.search_ns,
            buf: self.v().chat,
            start: None,
            end: None,
        });
    }

    fn chat_anchored(&self) -> bool {
        self.editor.window(self.v().chat_win).is_some_and(|w| w.anchor.is_some())
    }

    fn select_all(&mut self, win: WindowId) {
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::WinMotion {
            win,
            motion: CursorMotion::BufStart,
            select: false,
        });
        let _ = self.editor.apply(&plugin, ApiCall::WinSelect { win, on: true });
        let _ = self.editor.apply(&plugin, ApiCall::WinMotion {
            win,
            motion: CursorMotion::BufEnd,
            select: true,
        });
        self.follow_cursor();
    }

    /// Keep the cursor on screen while reading. Scrolling is the frontend's, but *what* to show is
    /// ours: the cursor moving out of the viewport with nothing following it is how a reader gets
    /// lost in their own transcript.
    fn follow_cursor(&mut self) {
        let Some(w) = self.editor.window(self.v().chat_win) else { return };
        let row = w.cursor.0;
        let Some(v) = w.viewport else { return };
        // Buffer rows on the screen, which after wrapping is not the window's height — the
        // frontend counts them and reports them back, and taking the height instead is what made
        // this believe a cursor was visible while it was four rows below the bottom edge.
        //
        // A first approximation and not the last word: the frontend bends the scroll it is given
        // so the caret is on screen whatever this says, and reports what it actually drew. What
        // this is for is naming the right *region* — a cursor five hundred rows up is not
        // something the drawing end should be asked to walk to.
        let rows = if v.rows > 0 { v.rows } else { (v.height as u32).max(1) };
        let lines = self.editor.buffer(self.v().chat).map(|b| b.line_count()).unwrap_or(0);
        // `None` means "following the tail", which is a different place from line zero — and the
        // first row of the transcript is a place the reader arrives at, so it has to be sayable.
        // Sent as a bare `0` it was read as "follow" instead, and `^U` up to the top of a long
        // answer put the window back at the bottom of it, cursor off screen, with nothing on the
        // way there to say what had happened.
        let showing = self.v().chat_top.unwrap_or_else(|| lines.saturating_sub(rows));
        let next = if row < showing {
            row
        } else if row >= showing + rows {
            row + 1 - rows
        } else {
            return;
        };
        self.vm().chat_top = Some(next);
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::WinScrollTo {
            win: self.v().chat_win,
            top_line: Some(next),
        });
    }

    /// Redraw everything that is about the conversation you are now in.
    ///
    /// Called after every switch, create and close. The transcript is rebuilt from the session's
    /// messages rather than replayed from the UI events that first produced it: the messages are
    /// the truth, and a switch that reconstructed the buffer from a log of edits would drift the
    /// moment anything else touched it.
    /// Draw the conversation on screen again from its messages, and touch nothing else.
    ///
    /// The buffer half of [`Self::enter_session`], for the times the *replay* has changed its mind
    /// about a conversation you are already in. The live path only ever appends, so anything it has
    /// already drawn stays drawn — which is right for a transcript, and wrong for the one row in it
    /// that is a statement about the conversation's tail rather than about something that happened.
    ///
    /// Deliberately not `enter_session`: that clears the unread mark, leaves reading mode, and
    /// resets the working line and the scroll, all of which are correct on arrival and destructive
    /// in the middle of a conversation you never left.
    fn redraw_transcript(&mut self) {
        let ascii = self.option_bool("ui.ascii_only");
        let limits = self.limits();
        let width = self.chat_width();
        let show_tools = self.option_bool("chat.show_tools");
        let here = self.active_session();
        let running = self.turns.contains_key(&here);
        let offset = self.chat_clock().map(|(offset, _)| offset);
        let (lines, marks, cards, times) = {
            let store = self.agent.sessions();
            let Some(s) = store.get(&here) else { return };
            let (lines, marks, cards, times) =
                transcript(s, ascii, limits, width, show_tools, running, offset);
            (lines, marks, cards, times)
        };
        let plugin = PluginId::from(BUILTIN);
        // Marks first, and all of them: a mark clamps rather than dies when the line under it is
        // replaced, so a rebuild that skipped this would leave every previous mark stacked on the
        // rows that happen to survive.
        let _ = self.editor.apply(&plugin, ApiCall::MarkClear {
            ns: self.chat_ns,
            buf: self.v().chat,
            start: None,
            end: None,
        });
        let _ = self.editor.apply(&plugin, ApiCall::BufSetLines {
            buf: self.v().chat,
            start: 0,
            end: -1,
            lines,
        });
        self.draw_marks(&marks, 0);
        self.draw_turn_times(&times);
        // The rows the cards are on now. The ones recorded before addressed a buffer that has just
        // been replaced, and `⇥` or the next call joining a run would find a card by them.
        self.vm().cards = cards;
    }

    fn enter_session(&mut self) {
        // A shell is not in a conversation, and rebuilding a transcript into its buffer would paint
        // over the screen the child is drawing.
        if self.v().term {
            return;
        }
        // First, and while the transcript being read is still the one on screen. Reading is a place
        // *in* a conversation — a cursor at a row, a selection between two of them, a search
        // highlight — and every one of those addresses text that is about to be replaced by
        // somebody else's. Left running, the mode survives into a conversation it was never opened
        // on, and the rows it names there are whatever happens to be at those numbers.
        self.leave_reading();
        // Arriving is reading. Every way into a conversation comes through here — switching, a new
        // one, the one you land on after closing or archiving, and the one a restored workspace
        // opens on — so this is the one place that has to clear the mark, and a flag left set on
        // the conversation you are sitting in would say "new" about the answer on screen.
        let arrived = {
            let here = self.active_session();
            self.agent.sessions().mark_read(&here)
        };
        if arrived {
            self.persist_sessions();
        }
        // Not closed: the buffer is about to be replaced wholesale, so settling an answer into rows
        // that are on their way out would write into the conversation being left behind.
        self.vm().answer = None;
        self.vm().streaming = None;
        // A row in the transcript being replaced, so it names nothing after this. The rebuild
        // below draws the conversation from its messages, where an unanswered question is simply
        // the last one in the list.
        self.vm().unanswered = None;
        // Both together: the footer is part of the working line, and a row count left behind from
        // the conversation being left would tell the next `chat_end` to insert inside a buffer
        // that no longer has those rows.
        self.vm().working = false;
        self.vm().plan_rows = 0;
        self.vm().chat_top = None;
        let ascii = self.option_bool("ui.ascii_only");
        let limits = self.limits();
        let width = self.chat_width();
        let show_tools = self.option_bool("chat.show_tools");
        let here = self.active_session();
        let running = self.turns.contains_key(&here);
        let offset = self.chat_clock().map(|(offset, _)| offset);
        let Some((lines, marks, cards, times, label)) = ({
            let store = self.agent.sessions();
            store.get(&here).map(|s| {
                let (lines, marks, cards, times) =
                    transcript(s, ascii, limits, width, show_tools, running, offset);
                (lines, marks, cards, times, s.label())
            })
        }) else {
            return;
        };
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::MarkClear {
            ns: self.chat_ns,
            buf: self.v().chat,
            start: None,
            end: None,
        });
        let _ = self.editor.apply(&plugin, ApiCall::BufSetLines {
            buf: self.v().chat,
            start: 0,
            end: -1,
            lines,
        });
        self.draw_marks(&marks, 0);
        self.draw_turn_times(&times);
        // Where the window *is*, and not only what this end believes about it. `chat_top = None`
        // says "following the tail"; the window has to be told, because it is still sitting at
        // whatever row the last conversation was scrolled to — and a scroll offset is kept rather
        // than derived, precisely so that a client attaching later can be told it. Read a long
        // answer with `^S`, go up, come out, switch project: the new conversation was drawn from
        // row two hundred of a transcript that no longer exists, which is to say it was blank.
        //
        // The cursor with it. Nothing draws a caret in the transcript while the composer has focus,
        // but every question the reader asks — which card is under it, which turn, which block — is
        // asked of a row number, and one left over from a longer conversation is off the end of a
        // shorter one.
        let _ = self
            .editor
            .apply(&plugin, ApiCall::WinScrollTo { win: self.v().chat_win, top_line: None });
        let _ = self
            .editor
            .apply(&plugin, ApiCall::WinSetCursor { win: self.v().chat_win, row: 0, col: 0 });
        let _ = self.editor.apply(&plugin, ApiCall::WinSelect { win: self.v().chat_win, on: false });
        let _ = self.editor.apply(&plugin, ApiCall::BufSetName {
            buf: self.v().chat,
            name: format!("[chat] {label}"),
        });
        // Nothing of the previous conversation's welcome survives into this one: the buffer was
        // just replaced wholesale, so what `welcome_rows` remembered is about text that is gone.
        self.vm().welcome_rows = 0;
        // The rows the redraw just put the cards on. Everything recorded before the switch
        // addressed a buffer that no longer exists.
        self.vm().cards = cards;
        self.ensure_usable_selection();
        // The mode belongs to the conversation, and a driver holds one mode for its whole process
        // rather than one per conversation — so arriving somewhere has to say which. Without it, a
        // conversation set to `ask` runs its next tool call under the full access of the one you
        // just left.
        self.sync_agent_drivers();
        self.draw_welcome();
        self.replay_round();
        self.set_composer("");
        self.refresh_status();
        self.broadcast(PluginEvent::SessionChanged {
            session: self.active_session(),
            view: self.from.view,
        });
    }

    /// The settings that say how much of a body to show.
    fn limits(&self) -> cards::Limits {
        cards::Limits {
            diff_lines: self.option_usize("chat.diff_lines"),
            output_lines: self.option_usize("chat.tool_output_lines"),
            preview_lines: self.option_usize("chat.preview_lines"),
        }
    }

    /// The directory paths in the transcript are shown relative to: the conversation's.
    fn root(&self) -> std::path::PathBuf {
        self.session_cwd(&self.active_session())
    }

    /// Draw one tool call's card — the header, with nothing under it yet — and remember where it
    /// went.
    ///
    /// `None` when tool cards are switched off, which is also the answer to "which card do I
    /// finish when it comes back": there is none.
    ///
    /// One function for the live path and the replay, because the two have to agree. They used to
    /// be two pieces of near-identical code, and near-identical is how a card acquires a blank line
    /// in one of them and not the other.
    fn draw_tool_card(&mut self, call: &neosh_proto::ToolCall, _state: ToolState) -> Option<u32> {
        if !self.option_bool("chat.show_tools") {
            return None;
        }
        let leg = Leg {
            id: call.id.clone(),
            name: call.name.clone(),
            input: call.input.clone(),
            result: None,
            started: Some(std::time::Instant::now()),
            took: None,
            exit: None,
        };
        // Onto the card above, when there is one and it is still the last thing in the transcript.
        // *Still the last thing* is the whole condition: a run is calls with nothing between them,
        // and anything at all having been drawn since — a sentence, a body, another kind of call —
        // means the reader has been told something else and the run is over.
        if let Some(i) = self.open_run()
            && self.v().cards[i].takes(&call.input)
        {
            self.vm().cards[i].legs.push(leg);
            let row = self.v().cards[i].row;
            if self.v().cards[i].body > 0 {
                // A stack of commands shows the *last* one's output, and the call that just
                // started has not printed one yet — so its predecessor's rows come off here.
                self.redraw_card(i);
            } else {
                // A run of reads has no body at all, and rewriting the header in place is what
                // keeps a run from costing rows while it happens.
                let header = self.card_header(i);
                self.replace_row(row, &header);
            }
            return Some(row);
        }
        let g = self.glyphs();
        let root = self.root();
        let width = self.chat_width();
        let head = cards::Head {
            name: &leg.name,
            input: &leg.input,
            state: ToolState::Running,
            took: None,
            exit: None,
            output: None,
            result: None,
        };
        let card = cards::header(&g, &head, &root, width);
        // No blank row above it. Air used to go between one action and the next on the argument
        // that it turns the transcript into a list — but a card is already a header at column zero
        // with its body indented under a corner, and a run of actions reads as a list without
        // paying a row for every one of them. Prose still gets its blank, because a paragraph and
        // a command are different kinds of thing; two commands are not.
        let row = self.chat_push(vec![card.text.clone()]);
        self.chat_row(row, &card);
        self.vm().cards.push(Card::new(leg, row));
        Some(row)
    }

    /// The card a new call could join: the last one drawn, if nothing has been drawn since.
    fn open_run(&self) -> Option<usize> {
        let i = self.v().cards.len().checked_sub(1)?;
        let c = &self.v().cards[i];
        (i64::from(c.row + c.body + 1) == self.chat_end()).then_some(i)
    }

    /// Put one row back where it was, marks and all. Nothing below moves.
    fn replace_row(&mut self, row: u32, r: &cards::Row) {
        let plugin = PluginId::from(BUILTIN);
        // Cleared first: a mark clamps rather than dies when the line under it is replaced, and at
        // equal priority the narrower one wins — so a row rewritten every tick ends up painted by
        // the shortest label it ever had.
        let _ = self.editor.apply(&plugin, ApiCall::MarkClear {
            ns: self.chat_ns,
            buf: self.v().chat,
            start: Some(row),
            end: Some(row + 1),
        });
        let _ = self.editor.apply(&plugin, ApiCall::BufSetLines {
            buf: self.v().chat,
            start: row as i64,
            end: row as i64 + 1,
            lines: vec![r.text.clone()],
        });
        self.chat_row(row, r);
    }

    /// The call came back: settle the dot, put the stats on the header and the body under it.
    ///
    /// Every call gets an answer under it, not just the failures. A card that says what ran and
    /// nothing about what came back is a card you have to take on trust, and "it ran" is not the
    /// thing you wanted to know.
    ///
    /// Errors show even with tool cards off — at the end of the transcript, there being no card
    /// to put them under: a tool that failed silently is a debugging session, and the setting is
    /// about noise, not about hiding failures.
    fn finish_card(&mut self, id: &neosh_proto::ToolCallId, result: &neosh_proto::ToolResult) {
        let found = self
            .v()
            .cards
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, c)| c.leg_of(id).map(|k| (i, k)));
        let Some((i, k)) = found else {
            if result.is_error || self.option_bool("chat.show_tools") {
                let g = self.glyphs();
                let limits = self.limits();
                let width = self.chat_width();
                let nothing = serde_json::Value::Null;
                let rows = cards::body(&g, &nothing, result, limits, cards::Open::Folded, width);
                for r in rows {
                    let row = self.chat_push(vec![r.text.clone()]);
                    self.chat_row(row, &r);
                }
            }
            return;
        };
        // Already answered. A replay can meet the same result twice — once in the messages and
        // once in the round's own record — and a second body would say it happened twice.
        if self.v().cards[i].legs[k].result.is_some() {
            return;
        }
        let leg = &mut self.vm().cards[i].legs[k];
        leg.took = leg.started.map(|s| s.elapsed());
        leg.exit = result.is_error.then(|| cards::exit_code(&result.content)).flatten();
        leg.result = Some(result.clone());
        self.redraw_card(i);
    }

    /// How one call is going, which is what a card's mark says. Derived rather than stored: a call
    /// with a result is finished and one without is running, and there is no third source of truth
    /// to fall out of step with.
    fn leg_state(leg: &Leg) -> ToolState {
        match &leg.result {
            Some(r) if r.is_error => ToolState::Failed,
            Some(_) => ToolState::Done,
            None => ToolState::Running,
        }
    }

    /// Every call on card `i`, as the renderer wants them.
    fn card_heads(card: &Card) -> Vec<cards::Head<'_>> {
        card.legs
            .iter()
            .map(|leg| cards::Head {
                name: &leg.name,
                input: &leg.input,
                state: Self::leg_state(leg),
                // A finished call keeps whatever it took; a running one is timed from its start,
                // so the number on screen is the one a clock on the wall would give. Whether
                // either is worth printing is the renderer's decision, in one place, for both.
                took: leg.took.or_else(|| leg.started.map(|s| s.elapsed())),
                exit: leg.exit,
                output: leg.result.as_ref().map(|r| lines_in(&r.content)),
                // Only a stack of commands draws one, and only that reads it back.
                result: leg.result.as_ref(),
            })
            .collect()
    }

    /// Card `i`'s header line, as it should read now.
    fn card_header(&self, i: usize) -> cards::Row {
        let g = self.glyphs();
        let root = self.root();
        let width = self.chat_width();
        let heads = Self::card_heads(&self.v().cards[i]);
        match heads.as_slice() {
            [one] => cards::header(&g, one, &root, width),
            many => cards::group_header(&g, many, &root, width),
        }
    }

    /// Advance the clock on every call that has not come back.
    ///
    /// One row replaced by one row, so nothing below moves. [`Self::redraw_card`] cannot be used
    /// for this: it settles the open answer, because a body appearing shifts every row after it —
    /// and doing that once a second would end the answer a dozen times while it was still
    /// arriving.
    fn tick_cards(&mut self) {
        let live: Vec<usize> = self
            .v()
            .cards
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                c.legs.iter().any(|l| l.result.is_none() && l.started.is_some())
            })
            .map(|(i, _)| i)
            .collect();
        for i in live {
            let card = self.card_header(i);
            let row = self.v().cards[i].row;
            self.replace_row(row, &card);
        }
    }

    /// Draw card `i` again from what it knows: the header in its final state, the body folded or
    /// open as the card says. Rows below move to make room, and the registry moves with them.
    fn redraw_card(&mut self, i: usize) {
        let g = self.glyphs();
        let width = self.chat_width();
        let limits = self.limits();
        let (row, old_body) = (self.v().cards[i].row, self.v().cards[i].body);
        let header = self.card_header(i);
        let root = self.root();
        let card = &self.v().cards[i];
        let body = match card.legs.as_slice() {
            // One call: whatever it came back with, folded or opened.
            [leg] => match &leg.result {
                Some(r) => cards::body(&g, &leg.input, r, limits, card.open, width),
                None => Vec::new(),
            },
            // A run of reads: nothing until it is opened, and then a row per call. What the fold
            // exists to keep out of the transcript is the *contents* of the six files, so opening
            // a run gives back the six names in full and stops there. A run of *commands* keeps
            // the last one's output, because what a command printed is the answer.
            _ => cards::group_body(&g, &Self::card_heads(card), &root, limits, card.open, width),
        };
        // Anything else writing into the transcript ends the answer: its rows are addressed by
        // range, and rows moving underneath it would make "the end of the answer" ambiguous. An
        // open answer is always below every card — it was opened after them — so settling it
        // moves nothing recorded here.
        self.close_answer();
        self.vm().streaming = None;
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::MarkClear {
            ns: self.chat_ns,
            buf: self.v().chat,
            start: Some(row),
            end: Some(row + 1 + old_body),
        });
        let mut lines = vec![header.text.clone()];
        lines.extend(body.iter().map(|r| r.text.clone()));
        let _ = self.editor.apply(&plugin, ApiCall::BufSetLines {
            buf: self.v().chat,
            start: row as i64,
            end: (row + 1 + old_body) as i64,
            lines,
        });
        self.chat_row(row, &header);
        for (k, r) in body.iter().enumerate() {
            self.chat_row(row + 1 + k as u32, r);
        }
        // The one redraw that is an arrival rather than a redraw.
        //
        // Every mark on this row was just cleared and re-set, so the band is a mark the frontend
        // has never seen — which is exactly what it keys the one-shot from. Guarded by `flashed`
        // rather than by "is this the first time we settled", because the two are the same fact and
        // only one of them survives a card being opened and folded again.
        if self.v().cards[i].settled() && !self.v().cards[i].flashed {
            self.vm().cards[i].flashed = true;
            self.chat_band(row, "Agent.ToolLanded");
        }
        let new_body = body.len() as u32;
        self.vm().cards[i].body = new_body;
        let delta = i64::from(new_body) - i64::from(old_body);
        if delta != 0 {
            for c in self.vm().cards.iter_mut().filter(|c| c.row > row) {
                c.row = (i64::from(c.row) + delta).max(0) as u32;
            }
            // And the trailing question moves with them. A call can come back *after* something
            // was steered in — a driver runs them in parallel — and a body opening above the
            // question shifts it down like every other row below the card. Left unshifted, the
            // turn's closing rows would be spliced into the middle of the question they belong
            // above. Not cleared: a tool result landing above it does not answer it.
            if let Some(u) = self.v().unanswered.filter(|u| *u > row) {
                self.vm().unanswered = Some((i64::from(u) + delta).max(0) as u32);
            }
        }
    }

    /// The card whose header or body is on `row`.
    fn card_at(&self, row: u32) -> Option<usize> {
        self.v().cards.iter().position(|c| row >= c.row && row <= c.row + c.body)
    }

    /// Open or fold the card under the cursor, while reading the transcript.
    ///
    /// The cursor goes back to the header either way: it is the one row of the card that is still
    /// where it was, and folding from the middle of a body would otherwise leave the cursor on
    /// whatever moved up into its place.
    fn toggle_card_here(&mut self) {
        let here = self.chat_cursor().0;
        let Some(i) = self.card_at(here) else {
            self.editor_message(MessageLevel::Info, "the cursor is not on a tool call");
            return;
        };
        if !self.v().cards[i].settled() {
            self.editor_message(MessageLevel::Info, "still running \u{2014} nothing to show yet");
            return;
        }
        // Between "all of it, and it stays" and whatever the cursor was already buying. Folding a
        // card the cursor is still in back to nothing would be answering `\u{21e5}` with a card
        // that shows *less* than it did before anybody pressed anything.
        self.vm().cards[i].open = match self.v().cards[i].open {
            cards::Open::Full => cards::Open::Preview,
            _ => cards::Open::Full,
        };
        self.redraw_card(i);
        let row = self.v().cards[i].row;
        self.reading_jump((row, 0));
        self.refresh_composer();
    }

    /// Open the card the cursor is in, and fold the one it has left.
    ///
    /// The rule that a list is a place you move in, on the transcript: the row you are *in* stays
    /// unfolded, and only ever one row — the cursor is what
    /// asks, everywhere else. A turn that ran nine commands is nine rows you walk down, each one
    /// showing what it printed as you arrive and giving the rows back as you leave, and none of it
    /// is a key you have to know about.
    ///
    /// Only when the card actually changes, which is what makes this safe to call on every motion:
    /// moving *within* a card redraws nothing, and a redraw is not free — it settles the answer
    /// that is streaming above it.
    ///
    /// A card pinned open with `\u{21e5}` is left alone in both directions. That is what pinning
    /// is for: you opened it to keep it, and walking off it is not changing your mind.
    fn refresh_preview(&mut self) {
        if !self.v().reading {
            return;
        }
        let (mut row, col) = self.chat_cursor();
        let want = self.card_at(row).filter(|i| self.v().cards[*i].settled());
        let leaving: Vec<usize> = self
            .v()
            .cards
            .iter()
            .enumerate()
            .filter(|(i, c)| c.open == cards::Open::Preview && Some(*i) != want)
            .map(|(i, _)| i)
            .collect();
        for i in leaving {
            let (at, before) = (self.v().cards[i].row, self.v().cards[i].body);
            self.vm().cards[i].open = cards::Open::Folded;
            self.redraw_card(i);
            // Everything below a card that just lost rows moved up, and the cursor is one of the
            // things below it: stepping off the bottom of a nine-row body lands you on the row
            // after it, and that row is now nine higher than where the cursor is pointing.
            if row > at {
                row = row.saturating_sub(before.saturating_sub(self.v().cards[i].body));
            }
        }
        if let Some(i) = want.filter(|i| self.v().cards[*i].open == cards::Open::Folded) {
            self.vm().cards[i].open = cards::Open::Preview;
            self.redraw_card(i);
        }
        if row != self.chat_cursor().0 {
            let at = vim::clamp(&self.chat_lines(), (row, col));
            let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::WinSetCursor {
                win: self.v().chat_win,
                row: at.0,
                col: at.1,
            });
            self.follow_cursor();
        }
    }

    /// Fold whatever the cursor was previewing, without moving it.
    ///
    /// What leaving reading has to do: a preview is the cursor's, and there is no cursor once you
    /// are back in the composer. A card pinned with `\u{21e5}` stays open, because that one is
    /// yours rather than the cursor's.
    fn drop_preview(&mut self) {
        let previewed: Vec<usize> = self
            .v()
            .cards
            .iter()
            .enumerate()
            .filter(|(_, c)| c.open == cards::Open::Preview)
            .map(|(i, _)| i)
            .collect();
        for i in previewed {
            self.vm().cards[i].open = cards::Open::Folded;
            self.redraw_card(i);
        }
    }

    /// `c` and `C`: the next tool call, or the one before it.
    ///
    /// The transcript's own structure, which neither of the other two stepping keys can find.
    /// `[`/`]` is a whole turn — far too coarse to inspect what a turn *did* — and `{`/`}` is a
    /// blank-line block, which since the blank row came off the top of a card means a run
    /// of nine cards is one block. So walking
    /// what happened, one call at a time, was nine presses of `j` per row of body and no way to
    /// skip a long one.
    ///
    /// The cursor lands on the header, which is where a card opens from — and the card opens
    /// itself on arrival, so `c c c` is the whole of a turn's work read one call at a time. A
    /// count works like every other motion here: `3c` is three calls on.
    fn reading_to_card(&mut self, dir: i32, count: u32) {
        let from = self.chat_cursor().0;
        for _ in 0..count.max(1) {
            let here = self.chat_cursor().0;
            // By header row alone, so `C` from half way down a body goes to the top of the card
            // you are in before it goes to the one before it — which is what every other backwards
            // motion here does, and the row you would have to reach anyway to fold it.
            let next = match dir > 0 {
                true => self.v().cards.iter().map(|c| c.row).filter(|r| *r > here).min(),
                false => self.v().cards.iter().map(|c| c.row).filter(|r| *r < here).max(),
            };
            match next {
                Some(row) => self.reading_jump((row, 0)),
                None => break,
            }
        }
        if self.chat_cursor().0 == from && self.v().cards.is_empty() {
            self.editor_message(MessageLevel::Info, "nothing in this conversation ran a tool");
        }
    }

    /// What the turn changed, under its answer.
    ///
    /// From the turn's own tool calls rather than from `git diff`, which would count work done
    /// outside this turn — by you, or by a turn in another conversation on the same checkout.
    fn draw_plan(&mut self, steps: &[neosh_proto::PlanStep], at: Option<u32>) -> u32 {
        if steps.is_empty() || !self.option_bool("chat.show_plan") {
            return 0;
        }
        // Whole. This copy is history — the answer to "what did it decide to do", read back later
        // — and abridging that would leave a transcript with a hole nothing can open.
        let rows = cards::plan(&self.glyphs(), steps, self.chat_width(), None);
        self.place_block(at, &rows)
    }

    /// What the turn changed, under its answer.
    ///
    /// From the turn's own tool calls rather than from `git diff`, which would count work done
    /// outside this turn — by you, or by a turn in another conversation on the same checkout.
    fn draw_summary(&mut self, files: &[cards::FileStat], at: Option<u32>) -> u32 {
        if files.is_empty() {
            return 0;
        }
        let root = self.root();
        let width = self.chat_width();
        let rows = cards::summary(files, &root, width);
        self.place_block(at, &rows)
    }

    /// Say that a question got no reply, when the turn it went into has ended and nothing was
    /// drawn under it.
    ///
    /// `at` is [`Self::unanswered`] as it stands after the turn's closing rows have been placed:
    /// `None` means something *was* said, and there is nothing to explain.
    ///
    /// This is the other half of the bug that put a plan under a message you had just typed.
    /// Moving the plan above it left the question genuinely last — and genuinely bare, which is
    /// the state that reads exactly like a turn still thinking. A message can reach a driver at
    /// the final gap of a turn and be answered by nothing at all: the vendor CLI has already
    /// emitted its result, the round it starts produces no content, and the turn ends. Silence
    /// there is the one outcome the transcript must not render as a question hanging in the air.
    ///
    /// It is a row of the transcript rather than a notification because it is *about* that
    /// question, and a toast that has faded is not an answer to "why is nothing happening".
    fn close_unanswered(&mut self, at: Option<u32>, stop: &neosh_proto::StopReason) {
        use neosh_proto::StopReason as S;
        if at.is_none() {
            return;
        }
        let (text, hl) = match stop {
            // What went wrong, where the question it went wrong on is. A notice says the same
            // thing for a few seconds and then the transcript is back to saying nothing.
            S::Error { message } => (message.clone(), "Diagnostic.Error"),
            S::Refusal { explanation, .. } => (
                explanation.clone().unwrap_or_else(|| "the model declined to answer".into()),
                "Diagnostic.Warn",
            ),
            // You are the one who stopped it, so this is a note rather than a complaint — but it
            // is still worth a row: without one, an interrupted question and a question nobody
            // has got to yet look identical.
            S::Cancelled => ("interrupted".into(), "Agent.Usage"),
            S::MaxTokens => ("the reply hit the output limit".into(), "Diagnostic.Warn"),
            _ => ("no reply \u{2014} the turn ended here".into(), "Diagnostic.Hint"),
        };
        // Through the same placement every other block uses, so it gets one blank line above it
        // and not two — the question already ends with one.
        self.place_block(None, &[cards::Row::plain(text, hl)]);
    }

    /// Put a block of already-rendered rows into the transcript, with their highlights, and answer
    /// with how many rows it took.
    ///
    /// `at` is a row to splice above, or `None` for the end. A row rather than always the end
    /// because a turn's closing rows belong to the answer they close: with a question steered in
    /// after that answer, the end of the buffer is *below* something nobody has answered yet. See
    /// [`Self::unanswered`].
    ///
    /// A blank line above unless there already is one, for the same reason a question gets one: two
    /// blank lines reads as a gap somebody forgot to fill, and none reads as one paragraph.
    fn place_block(&mut self, at: Option<u32>, rows: &[cards::Row]) -> u32 {
        if rows.is_empty() {
            return 0;
        }
        let end = at.map_or_else(|| self.chat_end(), i64::from);
        let gap = end > 0
            && self
                .editor
                .buffer(self.v().chat)
                .map(|b| {
                    b.get_lines((end - 1) as u32, end as u32).iter().any(|l| !l.trim().is_empty())
                })
                .unwrap_or(false);
        let mut lines: Vec<String> = Vec::new();
        if gap {
            lines.push(String::new());
        }
        lines.extend(rows.iter().map(|r| r.text.clone()));
        let n = lines.len() as u32;
        let first = match at {
            None => self.chat_push(lines),
            Some(row) => self.chat_splice(row, lines),
        };
        let start = first + u32::from(gap);
        for (k, r) in rows.iter().enumerate() {
            self.chat_row(start + k as u32, r);
        }
        n
    }

    /// Insert lines into the middle of the transcript, moving everything below down.
    ///
    /// The registry of card rows moves with them, exactly as it does when a card body opens — a
    /// row index that is not maintained is a row index that eventually addresses somebody else's
    /// text.
    fn chat_splice(&mut self, at: u32, lines: Vec<String>) -> u32 {
        self.close_answer();
        let n = lines.len() as u32;
        if n == 0 {
            return at;
        }
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::BufSetLines {
            buf: self.v().chat,
            start: i64::from(at),
            end: i64::from(at),
            lines,
        });
        for c in self.vm().cards.iter_mut().filter(|c| c.row >= at) {
            c.row += n;
        }
        if let Some(u) = self.v().unanswered.filter(|u| *u >= at) {
            self.vm().unanswered = Some(u + n);
        }
        self.vm().streaming = None;
        at
    }

    /// Put back what a turn already said, for a conversation that is still mid-turn.
    ///
    /// The transcript is rebuilt from messages, and what the running turn has produced is not in
    /// them yet. For a model driver that gap is one round; for an agent driver — `claude`, `codex`,
    /// anything speaking ACP — it is the *entire turn*, because such a driver streams its whole
    /// loop down one connection and neosh records none of it until that connection closes. So this
    /// is not a nicety for the last paragraph. Without it, switching away from a running agent turn
    /// and back showed the question and a spinner, and nothing the agent had done in between.
    ///
    /// The working line goes back too, because a turn that is still running should look like one
    /// wherever you are reading it.
    fn replay_round(&mut self) {
        let here = self.active_session();
        if !self.turns.contains_key(&here) {
            return;
        }
        let Some(said) = self.rounds.get_mut(&here).map(|r| std::mem::take(&mut r.said)) else {
            return;
        };
        for item in &said {
            match item {
                Said::Text(text) if text.is_empty() => {}
                // Left open on purpose when it is the last thing: the turn may still be writing
                // this very paragraph, and the next token has to continue it rather than start a
                // second answer underneath.
                Said::Text(text) => self.answer_text(text),
                Said::Tool { call, result } => {
                    // A call the conversation already holds was drawn by `transcript`, which left
                    // it in `cards`. Drawing a second card would say it happened twice — and for a
                    // model driver it always does, because the call lands in the messages the
                    // moment the stream closes and the tool starts running after that.
                    if !self.v().cards.iter().any(|c| c.leg_of(&call.id).is_some()) {
                        self.draw_tool_card(call, ToolState::Running);
                    }
                    // Whether the card came from the messages or was just drawn, a result the
                    // messages do not hold yet goes under it here. One they do hold was drawn with
                    // it, and `finish_card` leaves an answered card alone.
                    if let Some(r) = result {
                        self.finish_card(&call.id, r);
                    }
                }
            }
        }
        if let Some(r) = self.rounds.get_mut(&here) {
            r.said = said;
        }
        self.begin_working();
    }

    /// Append streamed output, starting a new line when the kind of output changes.
    // ---- the transcript --------------------------------------------------

    /// Add lines to the end of the transcript, returning the row the first one landed on.
    ///
    /// "The end" means above the working line when there is one. It says what is happening *now*,
    /// so anything that has already happened belongs above it — a tool card appended underneath
    /// would leave the spinner stranded in the middle of the turn it is reporting on.
    fn chat_push(&mut self, lines: Vec<String>) -> u32 {
        // Anything else writing into the transcript ends the answer: its rows are addressed by
        // range, and something landing after them would make "the end of the answer" ambiguous.
        self.close_answer();
        // Something is being said under the last question, so it is not the end of the transcript
        // any more. `chat_question` sets this again for the question it draws, which is why this
        // clear is safe to make unconditionally here.
        self.vm().unanswered = None;
        let plugin = PluginId::from(BUILTIN);
        let at = self.chat_end();
        let _ = self.editor.apply(&plugin, ApiCall::BufSetLines {
            buf: self.v().chat,
            start: at,
            end: at,
            lines,
        });
        self.vm().streaming = None;
        at.max(0) as u32
    }

    /// Where new content goes: the end, or the line above the working line when there is one.
    fn chat_end(&self) -> i64 {
        let n = self.editor.buffer(self.v().chat).map(|b| b.line_count() as i64).unwrap_or(0);
        if self.v().working { (n - i64::from(self.working_rows())).max(0) } else { n }
    }

    /// How many rows the working line and everything under it take, blank line included.
    fn working_rows(&self) -> u32 {
        if !self.v().working {
            return 0;
        }
        // The gap above, the line itself, and the footer under it.
        u32::from(self.v().plan_rows > 0) + 1 + self.v().plan_rows
    }

    /// Highlight a span of one transcript row.
    fn chat_mark(&mut self, row: u32, from: usize, to: usize, hl: &str) {
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::MarkSet {
            ns: self.chat_ns,
            buf: self.v().chat,
            row,
            col: from as u32,
            opts: neosh_proto::ExtmarkOpts {
                end_col: Some(to as u32),
                hl_group: Some(hl.to_string()),
                line_hl_group: None,
                virt_text: vec![],
                virt_text_pos: neosh_proto::VirtTextPos::Eol,
                on_delete: neosh_proto::OnDelete::Clamp,
                priority: 0,
                image: None,
            },
        });
    }

    // ---- the time in the margin -------------------------------------------

    /// Whether the transcript writes the time in its margin, and on which clock.
    ///
    /// The offset is asked for here rather than per row: rebuilding a long conversation stamps a
    /// margin per turn, and a process spawned for each of them would be a switch you could feel.
    fn chat_clock(&self) -> Option<(i64, bool)> {
        if !self.option_bool("chat.times") {
            return None;
        }
        let twelve = self.editor.options().str("chat.clock") == Some("12h");
        Some((crate::clock::local_offset(), twelve))
    }

    /// Where a mark has ended up, or `None` if it is gone.
    fn mark_row(&mut self, id: neosh_proto::ExtmarkId) -> Option<u32> {
        let call = ApiCall::MarkGet { ns: self.chat_ns, buf: self.v().chat, id };
        let info = self.editor.apply(&PluginId::from(BUILTIN), call);
        match info {
            Ok(ApiOk::MarkInfo { info: Some(info) }) if !info.invalid => Some(info.row),
            _ => None,
        }
    }

    /// Put a time in the right margin of `row`, answering with the mark that carries it.
    ///
    /// Virtual text rather than characters in the buffer, and that is the whole design: a
    /// transcript is an artefact you take pieces out of, so `y`, `ym` and `ya` have to copy what
    /// was said and not what neosh wrote in the margin about it. It is also why the position is
    /// [`VirtTextPos::Right`] — flush against the window's edge, which is a width only the
    /// frontend knows, and which therefore survives a resize that a padded string would not.
    fn set_turn_time(&mut self, row: u32, text: &str) -> Option<neosh_proto::ExtmarkId> {
        let call = ApiCall::MarkSet {
            ns: self.chat_ns,
            buf: self.v().chat,
            row,
            col: 0,
            opts: neosh_proto::ExtmarkOpts {
                end_col: None,
                hl_group: None,
                line_hl_group: None,
                virt_text: vec![chunk(text, "Agent.Time")],
                virt_text_pos: neosh_proto::VirtTextPos::Right,
                on_delete: neosh_proto::OnDelete::Clamp,
                priority: 0,
                image: None,
            },
        };
        match self.editor.apply(&PluginId::from(BUILTIN), call) {
            Ok(ApiOk::Mark { id }) => Some(id),
            _ => None,
        }
    }

    /// Draw the margin for the turn a question at `row` opens, and keep it as the moving one.
    ///
    /// `asked` is when the question entered the conversation, which the caller knows and this does
    /// not: a steered message is drawn when the model is told rather than when it was typed.
    fn start_turn_time(&mut self, row: u32, asked: i64) {
        self.settle_turn_time(asked);
        let Some((offset, twelve)) = self.chat_clock() else { return };
        let before = self.v().last_turn_at;
        let dated = before.is_none_or(|b| {
            crate::clock::day(b, offset) != crate::clock::day(asked, offset)
        });
        let year = crate::clock::year(before.unwrap_or_else(now_secs), offset)
            != crate::clock::year(asked, offset);
        let stamp = crate::clock::Stamp { asked, ended: None, dated, year };
        let text = stamp.label(offset, twelve, None);
        let Some(mark) = self.set_turn_time(row, &text) else { return };
        self.vm().clocked = Some(Clocked { mark, stamp });
        self.vm().last_turn_at = Some(asked);
    }

    /// The turn in the margin has ended, and its margin says how long it took from now on.
    fn end_turn_time(&mut self, ended: i64) {
        if self.v().clocked.is_some_and(|c| c.stamp.ended.is_none()) {
            if let Some(c) = self.vm().clocked.as_mut() {
                c.stamp.ended = Some(ended);
            }
            self.redraw_turn_time();
        }
    }

    /// Fix the moving margin at what it will say forever, because something is being drawn below.
    ///
    /// `by` is when that something happened, and it stands in for an ending the turn never got:
    /// a question steered into a running turn closes off the stretch above it as surely as a
    /// `TurnEnded` would, and a margin left saying only a clock time there would be the one turn
    /// in the transcript that never says how long it took.
    fn settle_turn_time(&mut self, by: i64) {
        let Some(c) = self.v().clocked else { return };
        if c.stamp.ended.is_none() {
            if let Some(c) = self.vm().clocked.as_mut() {
                c.stamp.ended = Some(by.max(c.stamp.asked));
            }
        }
        // Without the *ago*: this row is about to stop being the newest thing in the transcript,
        // and what it says from here on has to stay true with nothing ever redrawing it.
        self.write_turn_time(false);
        self.vm().clocked = None;
    }

    /// Write the moving margin again: with *how long ago* while it is the newest thing in the
    /// transcript, and without it once it is not.
    fn redraw_turn_time(&mut self) {
        self.write_turn_time(true);
    }

    /// The half of the above that says whether *how long ago* is part of it.
    fn write_turn_time(&mut self, with_ago: bool) {
        let Some(c) = self.v().clocked else { return };
        let Some((offset, twelve)) = self.chat_clock() else { return };
        let Some(row) = self.mark_row(c.mark) else {
            self.vm().clocked = None;
            return;
        };
        let text = c.stamp.label(offset, twelve, with_ago.then(now_secs));
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::MarkDel {
            ns: self.chat_ns,
            buf: self.v().chat,
            id: c.mark,
        });
        match self.set_turn_time(row, &text) {
            Some(mark) => {
                if let Some(now) = self.vm().clocked.as_mut() {
                    now.mark = mark;
                }
            }
            None => self.vm().clocked = None,
        }
    }

    /// Whether this view has a margin whose *ago* is going stale.
    ///
    /// What arms the slow tick. A turn that is still running has a working line counting up
    /// already, and a margin with no ending on it says nothing that changes.
    fn ageing(&self) -> bool {
        self.v().clocked.is_some_and(|c| c.stamp.ended.is_some()) && self.chat_clock().is_some()
    }

    /// Draw every turn's margin for a transcript that has just been rebuilt.
    ///
    /// All of them are fixed except the last, which becomes the moving one — so a conversation
    /// switched into says how long ago its newest answer arrived, which is very often the question
    /// somebody switched into it to answer.
    fn draw_turn_times(&mut self, times: &[TurnTime]) {
        self.vm().clocked = None;
        self.vm().last_turn_at = times.last().map(|t| t.stamp.asked);
        let Some((offset, twelve)) = self.chat_clock() else { return };
        let Some((last, earlier)) = times.split_last() else { return };
        for t in earlier {
            let text = t.stamp.label(offset, twelve, None);
            self.set_turn_time(t.row, &text);
        }
        let text = last.stamp.label(offset, twelve, Some(now_secs()));
        if let Some(mark) = self.set_turn_time(last.row, &text) {
            self.vm().clocked = Some(Clocked { mark, stamp: last.stamp });
        }
    }

    /// Draw a rebuilt transcript's marks, `by` rows down from where they were computed.
    fn draw_marks(&mut self, marks: &[Mark], by: u32) {
        for m in marks.iter().map(|m| m.clone().shifted(by)) {
            match (m.span, m.image) {
                (Some((col, _)), Some(image)) => self.chat_image(m.row, col, &image),
                (Some((from, to)), None) => self.chat_mark(m.row, from, to, m.hl),
                (None, _) => self.chat_band(m.row, m.hl),
            }
        }
    }

    /// Say that a transcript row is a picture from `col` on.
    ///
    /// A point mark carrying the file, and nothing about colour: what the row *says* is already
    /// there as text for a terminal that can only show that, and a terminal that can draw the
    /// picture draws it over the text from this column to its own edge — as many screen rows
    /// tall as it decides, which is a question about the pixel size of a cell that nothing here
    /// can answer. `ui.images = false` leaves the mark off, and every terminal sees the name.
    fn chat_image(&mut self, row: u32, col: usize, image: &neosh_proto::ImageFile) {
        if !self.option_bool("ui.images") {
            return;
        }
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::MarkSet {
            ns: self.chat_ns,
            buf: self.v().chat,
            row,
            col: col as u32,
            opts: neosh_proto::ExtmarkOpts {
                image: Some(image.clone()),
                ..Default::default()
            },
        });
    }

    /// Put a band behind a whole transcript row, out to whatever edge it is drawn against.
    ///
    /// A point mark rather than a ranged one: what it covers is the row, and giving it an end
    /// column would be saying the band stops somewhere, which is the bug this exists to fix.
    fn chat_band(&mut self, row: u32, hl: &str) {
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::MarkSet {
            ns: self.chat_ns,
            buf: self.v().chat,
            row,
            col: 0,
            opts: neosh_proto::ExtmarkOpts {
                end_col: None,
                hl_group: None,
                line_hl_group: Some(hl.to_string()),
                virt_text: vec![],
                virt_text_pos: neosh_proto::VirtTextPos::Eol,
                on_delete: neosh_proto::OnDelete::Clamp,
                priority: 0,
                image: None,
            },
        });
    }

    /// Draw one card row at `row`: its spans, and the band behind it.
    fn chat_row(&mut self, row: u32, r: &cards::Row) {
        for (from, to, hl) in &r.spans {
            self.chat_mark(row, *from, *to, hl);
        }
        if let Some(band) = r.band {
            self.chat_band(row, band);
        }
        if let Some((col, image)) = &r.image {
            self.chat_image(row, *col, image);
        }
    }

    /// Put the next piece of an answer into the transcript.
    ///
    /// Markdown is rendered as it arrives rather than after the fact, and the cost of that is a
    /// range replacement of the trailing block per tick instead of an append. Bounded by the size
    /// of one block, not by the size of the answer — see [`crate::markdown`] for why that
    /// distinction is the whole design.
    ///
    /// With `chat.markdown` off this is the old append: text goes in exactly as it came out, which
    /// is what you want when the question is "what did the model actually say".
    fn answer_text(&mut self, text: &str) {
        let plugin = PluginId::from(BUILTIN);
        if !self.option_bool("chat.markdown") {
            // Raw mode appends to the buffer's *last* line, so the working line has to stop being
            // it. It goes back underneath as soon as the chunk has landed.
            self.below_working(|me| {
                if me.v().streaming != Some(Stream::Text) {
                    me.open_answer_row();
                    me.vm().streaming = Some(Stream::Text);
                }
                let buf = me.v().chat;
                let _ =
                    me.editor.apply(&plugin, ApiCall::BufAppendText { buf, text: text.into() });
            });
            return;
        }

        if self.v().answer.is_none() {
            let start = self.open_answer_row();
            self.vm().answer = Some(Answer { md: crate::markdown::Md::new(), start, rows: 1 });
        }
        let Some(mut a) = self.vm().answer.take() else { return };
        let patch = a.md.push(text);
        self.apply_patch(&mut a, patch);
        self.vm().answer = Some(a);
    }

    /// The answer is over: settle its last block and stop treating the tail as provisional.
    fn close_answer(&mut self) {
        let Some(mut a) = self.vm().answer.take() else { return };
        let patch = a.md.finish();
        self.apply_patch(&mut a, patch);
        self.vm().streaming = None;
    }

    /// Do something that appends to the last line of the transcript, with the working line taken
    /// out of the way and put back afterwards.
    ///
    /// `BufAppendText` means "the last line" by definition, so anything using it has to own that
    /// line for the duration. Everything else writes at [`Self::chat_end`] and needs none of this.
    fn below_working(&mut self, f: impl FnOnce(&mut Self)) {
        let had = self.v().working;
        if had {
            self.end_working();
        }
        f(self);
        if had {
            self.begin_working();
        }
    }

    /// Open the row an answer starts on, after a blank line separating it from what came before.
    ///
    /// Above the working line, not at the end of the buffer: the turn is still running, and a
    /// spinner stranded above the answer it is producing says the wrong thing about which of them
    /// is still happening.
    fn open_answer_row(&mut self) -> u32 {
        // An answer is arriving under the question, which is the thing that answers it. Written
        // directly rather than through `chat_push`, so it has to say this for itself.
        self.vm().unanswered = None;
        let n = self.chat_end();
        // A blank line between the last thing and the answer — unless there already is one. A tool
        // card butted straight up against the sentence that follows it reads as one paragraph, and
        // two blank lines reads as a gap somebody forgot to fill.
        let blank_above = n <= 0
            || self
                .editor
                .buffer(self.v().chat)
                .map(|b| b.get_lines((n - 1) as u32, n as u32).iter().all(|l| l.trim().is_empty()))
                .unwrap_or(true);
        let rows = if blank_above { 1 } else { 2 };
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::BufSetLines {
            buf: self.v().chat,
            start: n,
            end: n,
            lines: vec![String::new(); rows],
        });
        (n.max(0) as u32) + (rows as u32 - 1)
    }

    /// Replace the unsettled tail of an answer with its freshly rendered form.
    fn apply_patch(&mut self, a: &mut Answer, patch: crate::markdown::Patch) {
        let at = a.start + patch.from as u32;
        let end = a.start + a.rows;
        let rows = patch.lines.len();
        let plugin = PluginId::from(BUILTIN);
        // Settling a block that was already drawn correctly renders the same rows again. Writing
        // them costs a frame on the wire and changes nothing on screen, which is the definition of
        // a wasted frame — and `finish` does exactly this at the end of every answer.
        let unchanged = self
            .editor
            .buffer(self.v().chat)
            .map(|b| b.get_lines(at, end.max(at)) == patch.lines)
            .unwrap_or(false);
        if unchanged {
            a.rows = patch.from as u32 + rows as u32;
            return;
        }
        // Marks go with the rows they were on. Left behind, a mark from the previous render of the
        // trailing block would clamp onto whatever replaced it — which is how a half-written bold
        // run ends up colouring the wrong words for the rest of the answer.
        let _ = self.editor.apply(&plugin, ApiCall::MarkClear {
            ns: self.chat_ns,
            buf: self.v().chat,
            start: Some(at),
            end: Some(end.max(at)),
        });
        let _ = self.editor.apply(&plugin, ApiCall::BufSetLines {
            buf: self.v().chat,
            start: at as i64,
            end: end.max(at) as i64,
            lines: patch.lines,
        });
        for (row, from, to, hl) in patch.marks {
            self.chat_mark(at + row as u32, from, to, hl);
        }
        a.rows = patch.from as u32 + rows as u32;
    }

    /// Draw a question the way a transcript should: framed, so the eye can find where a turn began.
    ///
    /// A bar down the left rather than a `>` prefix, because a multi-line question wrapped by the
    /// frontend loses a prefix on every line but the first, and the thing you scan for when
    /// scrolling back is *where the turns start*.
    ///
    /// Answers with the first row of the block it drew, which is where the turn's closing rows go
    /// when it turns out nothing was said after it.
    fn chat_question(&mut self, prompt: &Prompt) -> u32 {
        let text = &prompt.text;
        let glyph = self.glyphs().bar;
        let mut rows = Vec::new();
        // A gap above, unless there already is one. Whatever came before was the end of the last
        // answer — often a tool card, which has no trailing blank of its own — and a new question
        // butted straight against it reads as another line of that answer rather than the start of
        // something you said.
        // The line above where this is about to land, which is not the last line of the buffer
        // while a turn is working: the spinner is down there and is not what came before.
        let end = self.chat_end();
        let gap = end > 0
            && self
                .editor
                .buffer(self.v().chat)
                .map(|b| {
                    b.get_lines((end - 1) as u32, end as u32)
                        .iter()
                        .any(|l| !l.trim().is_empty())
                })
                .unwrap_or(false);
        if gap {
            rows.push(String::new());
        }
        // The pictures first, on rows of their own, because that is the order they are in the
        // message and a question is easier to read when what it is pointing at is above it.
        for b in &prompt.images {
            if let ContentBlock::Image { path, media_type } = b {
                rows.push(format!("{glyph} {}", cards::image_row(path, media_type)));
            }
        }
        for line in text.lines() {
            rows.push(format!("{glyph} {line}"));
        }
        if rows.is_empty() || rows.iter().all(|r| r.trim().is_empty()) {
            rows.push(glyph.to_string());
        }
        rows.push(String::new());
        let body = rows.len() as u32 - 1 - u32::from(gap);
        let at = self.chat_push(rows);
        let first = at + u32::from(gap);
        for i in 0..body {
            self.chat_mark(first + i, 0, glyph.len(), "Agent.User");
        }
        // The pictures are the first rows of the question, in the order they were pushed.
        for (i, b) in prompt.images.iter().enumerate() {
            if let ContentBlock::Image { path, media_type } = b {
                let image = neosh_proto::ImageFile { path: path.clone(), media_type: media_type.clone() };
                self.chat_image(first + i as u32, glyph.len() + 1, &image);
            }
        }
        // Nothing has answered it yet, by definition — this is the last thing in the transcript.
        // Whatever is drawn next clears this; if the turn ends and it is still set, the question
        // got no reply. See [`Self::unanswered`].
        self.vm().unanswered = Some(at);
        // And the margin for the turn this question opens. On the blank row above it wherever
        // there is one: an empty line is the one row a right-aligned annotation can never collide
        // with what was said, and the question's own first row is where it goes when this question
        // opens the transcript. Stamped now rather than from the message, because a steered
        // question is drawn when the model is told about it and not when it was typed.
        let row = if gap || at == 0 { at } else { at - 1 };
        self.start_turn_time(row, now_secs());
        at
    }

    /// Put the working line at the end of the transcript, for the conversation on screen.
    ///
    /// Says nothing about starting a turn: the turn's own state is the [`Round`], which exists
    /// whether or not you are looking at it. This is only the line.
    ///
    /// Written directly rather than through [`Self::chat_push`], which settles the answer above
    /// it. The working line is not content — it sits *after* an answer that is still being
    /// written, and closing that answer in order to say it is still going would be a strange thing
    /// to do.
    ///
    /// It goes *first* of the block, above the plan and above what is out. All three are state,
    /// but only one of them changes every tick: what it is doing right now is what you are
    /// watching, and putting it under a checklist means it lands on a different row every time the
    /// checklist grows a step. The rows above it are the transcript, which is settled, so the
    /// first row of the block is the one place in the buffer that stays put.
    fn begin_working(&mut self) {
        if self.v().working {
            return;
        }
        let rows = self.footer();
        let at = self.chat_end();
        // A blank line between what has been said and what is true now, for the same reason a
        // question gets one: butted up against the last tool card, the block reads as part of its
        // output. Only when there is a block — a working line on its own is one row, and one row
        // does not need announcing.
        let gap = !rows.is_empty();
        let mut lines: Vec<String> = Vec::new();
        if gap {
            lines.push(String::new());
        }
        lines.push(String::new());
        lines.extend(rows.iter().map(|r| r.text.clone()));
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::BufSetLines {
            buf: self.v().chat,
            start: at,
            end: at,
            lines,
        });
        self.vm().plan_rows = rows.len() as u32;
        self.vm().working = true;
        let first = at as u32 + u32::from(gap) + 1;
        for (k, r) in rows.iter().enumerate() {
            self.chat_row(first + k as u32, r);
        }
        self.draw_working();
    }

    /// The rows under the working line: the plan, then what is still out.
    ///
    /// A pure function of the round, drawn from scratch every time. There is no incremental path
    /// on purpose — it is a few rows at the very bottom of the buffer, and a diff against the
    /// previous version would be a second place for "where is the footer" to be wrong.
    fn footer(&self) -> Vec<cards::Row> {
        let Some(r) = self.rounds.get(&self.active_session()) else { return Vec::new() };
        let g = self.glyphs();
        let width = self.chat_width();
        let mut rows = if self.option_bool("chat.show_plan") {
            let limit = self.option_usize("chat.plan_rows");
            cards::plan(&g, &r.plan, width, Some(limit))
        } else {
            Vec::new()
        };
        let mut out: Vec<cards::TaskRow> = r
            .tasks
            .iter()
            .filter(|t| t.listed())
            .map(|t| cards::TaskRow {
                title: &t.title,
                role: t.role.as_deref(),
                since: Some(t.since.elapsed()),
                live: t.status == neosh_proto::TaskStatus::Running,
            })
            .collect();
        // And whatever else is running that this turn never started — a shell backgrounded two
        // turns ago is not in any round, and the footer is the one surface that is on screen while
        // you wait. Keyed by task id against what the round already has, since the level and the
        // edges describe the same things and a task in both would otherwise be two rows.
        //
        // No clock on these: the level says what is running, never since when, and a duration
        // counted from the moment this process first heard of it would be a number about neosh
        // rather than about the work. `live: false` for the same reason it is on the block a
        // finished turn leaves behind — nothing is waiting on them.
        //
        // The id is read *first* and on its own line. `active_session` locks the session store, and
        // the store's mutex is not reentrant — written as `sessions().get(&self.active_session())`
        // the receiver takes the lock and then the argument asks for it again, and the host loop
        // stops dead with the panel frozen on whatever it last drew.
        let seen: Vec<&neosh_proto::TaskId> = r.tasks.iter().map(|t| &t.id).collect();
        let here = self.active_session();
        let extra: Vec<(String, Option<String>)> = self
            .agent
            .sessions()
            .get(&here)
            .map(|s| {
                s.background
                    .iter()
                    .filter(|t| !seen.contains(&&t.id))
                    .map(|t| {
                        let title = if t.title.is_empty() {
                            "background task".to_string()
                        } else {
                            t.title.clone()
                        };
                        (title, t.kind.clone())
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.extend(extra.iter().map(|(title, kind)| cards::TaskRow {
            title,
            role: kind.as_deref(),
            since: None,
            live: false,
        }));
        if !out.is_empty() {
            rows.extend(cards::tasks(&g, &out, width));
        }
        rows
    }

    /// What is still out when the turn stops waiting for it, as rows to leave behind.
    fn tasks_left_running(&self, session: &neosh_proto::SessionId) -> Vec<cards::Row> {
        // The level first, and the turn's own bookends only when there is no level to be had.
        //
        // A driver that reports the whole live set is telling us the answer; the turn's list is a
        // reconstruction of it from edges, and the two disagree exactly where reconstructions do —
        // a shell backgrounded three turns ago is in the level and in no round at all. Codex has no
        // level signal, so the edges are all there is there, and dropping them would trade one
        // driver's accuracy for another's silence.
        //
        // Copied out rather than borrowed: `sessions()` is a lock guard, and a `TaskRow` borrows
        // the strings it names.
        let level: Vec<(String, Option<String>)> = self
            .agent
            .sessions()
            .get(session)
            .map(|s| {
                s.background
                    .iter()
                    .map(|t| {
                        let title = if t.title.is_empty() {
                            "background task".to_string()
                        } else {
                            t.title.clone()
                        };
                        (title, t.kind.clone())
                    })
                    .collect()
            })
            .unwrap_or_default();
        let level: Vec<cards::TaskRow> = level
            .iter()
            .map(|(title, kind)| cards::TaskRow {
                title,
                role: kind.as_deref(),
                // No clock. It is still running and this row is not: a frozen number beside
                // something that is still going is worse than no number.
                since: None,
                live: false,
            })
            .collect();
        let left = if level.is_empty() {
            let Some(r) = self.rounds.get(session) else { return Vec::new() };
            r.tasks
                .iter()
                .filter(|t| t.listed())
                .map(|t| cards::TaskRow {
                    title: &t.title,
                    role: t.role.as_deref(),
                    since: None,
                    live: false,
                })
                .collect()
        } else {
            level
        };
        if left.is_empty() {
            return Vec::new();
        }
        let mut rows = cards::still_running(&self.glyphs(), left.len());
        rows.extend(cards::tasks(&self.glyphs(), &left, self.chat_width()));
        rows
    }

    /// Move the clock on anything in the footer that has one.
    ///
    /// Only sub-agents do — a plan step has no duration — so this is skipped entirely when nothing
    /// is out, which is almost always. Unlike [`Self::tick_cards`] the whole footer is laid out
    /// again rather than one row replaced: it is the last few rows of the buffer, and there is
    /// nothing below it to disturb.
    fn tick_footer(&mut self) {
        let ticking = self
            .rounds
            .get(&self.active_session())
            .is_some_and(|r| r.tasks.iter().any(Task::listed));
        if ticking {
            self.redraw_footer();
        }
    }

    /// The plan or the list of what is out has changed, so the footer has to be laid out again.
    ///
    /// Torn down and rebuilt rather than patched, which is what [`Self::below_working`] already
    /// does for anything that writes into the transcript while a turn runs.
    fn redraw_footer(&mut self) {
        if !self.v().working {
            return;
        }
        self.below_working(|_| {});
    }

    /// Redraw the working line in place.
    ///
    /// Cheap enough to call on every token: it is one `set_lines` over a single row, which the
    /// core coalesces with everything else in the same 16 ms frame. The *movement* costs nothing
    /// here at all — the shimmer is the frontend's, running at its own rate over whatever this
    /// last wrote.
    fn draw_working(&mut self) {
        if !self.v().working {
            return;
        }
        let here = self.active_session();
        let Some(w) = self.rounds.get(&here) else { return };
        let (verb, note) = (w.verb, w.note.clone());
        let secs = w.started.elapsed().as_secs();
        let row = self.working_row();

        let ascii = self.option_bool("ui.ascii_only");
        let glyph = Glyphs::new(ascii).work;
        // The same shape every other duration in the workspace is written in — the sidebar's
        // running row, a tool card's tail — because a turn that has been going for two hours read
        // `123m 4s` here and `2h 03m` three columns to the left, about the same turn.
        let elapsed = crate::clock::lasted(secs as i64);

        let label = note.as_deref().unwrap_or(verb);
        let mut text = format!("{glyph} {label}\u{2026}  {elapsed}");
        // What you typed while it was working, and the fact that it has not been said yet.
        match self.agent.steering_count(&here) {
            0 => {}
            1 => text.push_str("  \u{b7}  1 queued"),
            n => text.push_str(&format!("  \u{b7}  {n} queued")),
        }
        text.push_str("  \u{b7}  esc to interrupt");

        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::BufSetLines {
            buf: self.v().chat,
            start: row as i64,
            end: row as i64 + 1,
            lines: vec![text.clone()],
        });
        // Cleared first, and this is not tidiness. A mark clamps rather than dies when the line
        // under it is replaced, so this row accumulated one pair per tick — and where two marks
        // overlap at equal priority the *narrower* one wins, so the tail of a shorter label from
        // three seconds ago went on painting over the label being drawn now. What that looked like
        // was the thing this line exists to prevent: `/compact` sat there with its note in the dead
        // grey of finished text, not moving, exactly like a hang.
        let _ = self.editor.apply(&plugin, ApiCall::MarkClear {
            ns: self.chat_ns,
            buf: self.v().chat,
            start: Some(row),
            end: Some(row + 1),
        });
        // Only the label moves. A clock and a hint sweeping along with it would turn a status line
        // into a light show, and the thing you actually want to read is the number.
        let label_end = glyph.len() + 1 + label.len() + "\u{2026}".len();
        // The sweep, note or no note. `Status.Pending` is for waiting on something outside the
        // program, and a note on this row is never that: it is the turn's own loop saying what it
        // is doing — compacting, running a sub-agent — which is the same "working" the verb means,
        // said more precisely. Swapping the sweep for the slower pulse there took the movement away
        // from the *longest* silence in the program, which is the one that most needs it.
        let hl = "Status.Streaming";
        self.chat_mark(row, 0, label_end, hl);
        self.chat_mark(row, label_end, text.len(), "Agent.Usage");
    }

    /// Take the working line away.
    ///
    /// Removed rather than blanked: it was never content, and a transcript that accumulates one
    /// empty row per turn is a transcript with a growing hole in it.
    fn end_working(&mut self) {
        if !self.v().working {
            return;
        }
        let row = self.working_row();
        let first = row.saturating_sub(u32::from(self.v().plan_rows > 0));
        let last = row + self.v().plan_rows;
        self.vm().working = false;
        self.vm().plan_rows = 0;
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::MarkClear {
            ns: self.chat_ns,
            buf: self.v().chat,
            start: Some(first),
            end: Some(last + 1),
        });
        let _ = self.editor.apply(&plugin, ApiCall::BufSetLines {
            buf: self.v().chat,
            start: first as i64,
            end: last as i64 + 1,
            lines: vec![],
        });
    }

    /// Which row the working line is on, while there is one.
    ///
    /// Counted back from the end rather than remembered, so it is right whatever else wrote to the
    /// buffer: the block is always the last rows of it, and the working line is the first row of
    /// the block that is not the blank above it.
    fn working_row(&self) -> u32 {
        let n = self.editor.buffer(self.v().chat).map(|b| b.line_count()).unwrap_or(0);
        n.saturating_sub(1 + self.v().plan_rows)
    }

    fn stream_into_chat(&mut self, kind: Stream, text: &str) {
        self.below_working(|me| me.stream_into_chat_now(kind, text));
    }

    fn stream_into_chat_now(&mut self, kind: Stream, text: &str) {
        let plugin = PluginId::from(BUILTIN);
        // Reasoning under the question is the turn answering it, even when the answer proper has
        // not started. Another direct writer, so it says so for itself.
        self.vm().unanswered = None;
        if self.v().streaming != Some(kind) {
            self.close_answer();
            // Text is appended to the buffer's *last* line, so the working line has to stop being
            // it before anything streams. This is the one rule that keeps the invariant true.
            self.end_working();
            let n = self.editor.buffer(self.v().chat).map(|b| b.line_count() as i64).unwrap_or(0);
            let _ = self.editor.apply(&plugin, ApiCall::BufSetLines {
                buf: self.v().chat,
                start: n,
                end: n,
                lines: vec![String::new()],
            });
            self.vm().streaming = Some(kind);
        }
        let _ = self.editor.apply(&plugin, ApiCall::BufAppendText {
            buf: self.v().chat,
            text: text.to_string(),
        });
    }

    /// Route one thing a turn did.
    ///
    /// Two jobs, and they are deliberately separate. *Bookkeeping* — what a conversation has said
    /// so far, whether its turn is still running, what to persist — happens for every event
    /// whatever is on screen. *Drawing* happens only when the event came from the conversation you
    /// are looking at. That split is what lets several turns run at once without one of them
    /// writing into another's transcript.
    /// Whatever the agent just said, and then the reader taken along with it if it was at the end.
    ///
    /// A turn that is still going writes rows below wherever you are reading, and reading is a
    /// place *in* the transcript: the scroll offset is pinned by every motion, so an answer
    /// arriving while `^S` is on lands off the bottom of the window. Which made watching a turn a
    /// matter of leaving reading, seeing what had happened, and going back in — for every call.
    ///
    /// **At the end means following, anywhere else means anchored.** The same distinction the
    /// unscrolled window makes and for the same reason: parked on the last row you are watching,
    /// and half way up a diff you are reading, and the second one must not be dragged away from
    /// what it is looking at. Nothing to turn on, and one `G` to start following again.
    ///
    /// Not while selecting or searching — both of those are two positions, and moving one of them
    /// on the agent's behalf changes what the reader is holding.
    fn handle_agent_event(&mut self, ev: AgentEvent) {
        // Which terminals were parked at the end of the transcript before this landed, so each of
        // them can be taken back down to the newest row afterwards. Per view, because one window
        // following the answer while another is half way up a diff is the ordinary case: only the
        // one that was watching gets moved.
        let session = ev.session().clone();
        let mut following = Vec::new();
        for view in self.views_showing(&session) {
            if self.in_view(view, |me| me.reading_at_tail()) {
                following.push(view);
            }
        }
        self.on_agent_event(ev);
        for view in following {
            self.in_view(view, |me| me.reading_follow());
        }
    }

    /// Whether the reader is parked on the last row of the transcript.
    fn reading_at_tail(&self) -> bool {
        self.v().reading
            && self.v().search.is_none()
            && !self.chat_anchored()
            && self.chat_cursor().0 + 1 >= self.chat_len()
    }

    /// How many rows the transcript has, without copying every one of them to find out.
    fn chat_len(&self) -> u32 {
        self.editor.buffer(self.v().chat).map(|b| b.lines().len() as u32).unwrap_or(0)
    }

    /// Take the reader down to the newest row.
    ///
    /// Deliberately not [`Self::place_cursor`]: following is *watching*, and a card that opened
    /// itself under a cursor the agent is dragging is a body nobody aimed at — several of them a
    /// second, each one settling the answer streaming above it. Arriving somewhere by yourself is
    /// what asks a card to open.
    fn reading_follow(&mut self) {
        let lines = self.chat_lines();
        let last = lines.len().saturating_sub(1) as u32;
        if self.chat_cursor().0 == last {
            return;
        }
        let col = vim::first_non_blank(lines.last().map(String::as_str).unwrap_or(""));
        let at = vim::clamp(&lines, (last, col));
        self.vm().reading_goal = None;
        let win = self.v().chat_win;
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::WinSetCursor {
            win,
            row: at.0,
            col: at.1,
        });
        self.follow_cursor();
        self.refresh_composer();
    }

    fn on_agent_event(&mut self, ev: AgentEvent) {
        // Whether *anybody* is looking. It used to be "is this the conversation on screen", which
        // had one answer because there was one screen; now it is a question about every terminal,
        // and the drawing that follows is done once in each of the ones that says yes.
        //
        // Including from another machine: a watcher is somebody reading this conversation, and a
        // turn that ends in front of them is not news to be kept for whoever next looks here.
        let seen = !self.views_showing(ev.session()).is_empty() || self.watched(ev.session());
        match ev {
            AgentEvent::TurnStarted { session, turn } => {
                // Write down that a turn is in flight, and clear whatever the last one left. This
                // has to reach the disk *now* rather than at the next commit: the window it covers
                // starts here, and a note written after the first tool round would miss every turn
                // that was killed before it got that far — which is most of a turn's first minute.
                //
                // Starting a turn is also what ends the previous one's mark. The row is red because
                // the last turn here never finished; it stops being the last turn at this moment,
                // and the transcript keeps the evidence either way.
                let was_cut_off = match self.agent.sessions().get_mut(&session) {
                    Some(sess) => {
                        sess.running_since = Some(now_secs());
                        std::mem::take(&mut sess.interrupted)
                    }
                    None => false,
                };
                self.persist_session(&session);
                // The line saying the last turn was killed is now a lie, and the live path cannot
                // take back something it has drawn — so the replay draws it again without. Done
                // here rather than when the message was pushed because everything up to and
                // including the new question is in `messages` by now and nothing of the answer has
                // been drawn yet, which is the one moment a rebuild costs nothing.
                if was_cut_off {
                    self.each_view_showing(&session, |me| me.redraw_transcript());
                }
                // Out to anybody watching from another machine, before the local broadcast: the
                // two are independent, and a watcher waiting on a plugin round trip would see the
                // turn start late for no reason.
                if self.watched(&session) {
                    self.stream_out(
                        &session,
                        neosh_proto::StreamEvent::TurnStarted { turn: turn.0.clone() },
                    );
                }
                self.broadcast(PluginEvent::TurnStarted { session, turn });
            }
            AgentEvent::Token { session, turn, text } => {
                if let Some(r) = self.rounds.get_mut(&session) {
                    // Whatever it was waiting for, it is not waiting any more — the model is
                    // talking. There is no event for a wait *ending*: a driver that has stopped
                    // waiting simply gets on with the thing it was waiting to do, and this is it.
                    // Cheap to leave in the hot path because it only writes when there is something
                    // to clear, which is almost never.
                    if r.note_is_wait {
                        r.note = None;
                        r.note_is_wait = false;
                    }
                    match r.said.last_mut() {
                        Some(Said::Text(t)) => t.push_str(&text),
                        _ => r.said.push(Said::Text(text.clone())),
                    }
                }
                self.each_view_showing(&session, |me| me.answer_text(&text));
                if self.watched(&session) {
                    self.stream_out(&session, neosh_proto::StreamEvent::Token {
                        turn: turn.0.clone(),
                        text: text.clone(),
                    });
                }
                self.broadcast(PluginEvent::Token { session, turn, text });
            }
            AgentEvent::Thinking { session, turn, text } => {
                // Not accumulated, for the same reason `transcript` does not replay it: reasoning
                // is shown live and reprinting it on a switch would bury the answer under the
                // working out.
                if self.option_bool("chat.show_thinking") {
                    self.each_view_showing(&session, |me| {
                        me.stream_into_chat(Stream::Thinking, &text)
                    });
                }
                if self.watched(&session) {
                    self.stream_out(&session, neosh_proto::StreamEvent::Thinking {
                        turn: turn.0.clone(),
                        text: text.clone(),
                    });
                }
                self.broadcast(PluginEvent::ThinkingToken { session, turn, text });
            }
            AgentEvent::ToolStarted { session, turn, call } => {
                if let Some(r) = self.rounds.get_mut(&session) {
                    r.note = Some(format!("Running {}", call.name));
                    r.note_is_wait = false;
                    r.said.push(Said::Tool { call: call.clone(), result: None });
                }
                self.each_view_showing(&session, |me| {
                    me.draw_tool_card(&call, ToolState::Running);
                    // The tool is what the turn is doing now, so the working line should say so
                    // rather than keep claiming the model is thinking.
                    me.draw_working();
                });
                // The card a watcher on another machine draws — the same card.
                if self.watched(&session) {
                    self.stream_out(&session, neosh_proto::StreamEvent::ToolStarted {
                        turn: turn.0.clone(),
                        call: call.clone(),
                    });
                }
                self.broadcast(PluginEvent::ToolStarted { session, turn, call });
            }
            AgentEvent::ToolFinished { session, turn, call, result } => {
                if let Some(r) = self.rounds.get_mut(&session) {
                    r.note = None;
                    r.note_is_wait = false;
                    // The call this answers, so a replay of the round draws the pair rather than a
                    // dot that is still pulsing over a tool that came back minutes ago.
                    for said in r.said.iter_mut().rev() {
                        if let Said::Tool { call: c, result: slot } = said
                            && c.id == call.id
                        {
                            *slot = Some(result.clone());
                            break;
                        }
                    }
                }
                // Counted whether or not anyone is looking: the summary is drawn when the turn
                // ends, wherever you are by then.
                if !result.is_error
                    && let Some(r) = self.rounds.get_mut(&session)
                {
                    cards::tally(&mut r.changes, &cards::edits_of(&call.input));
                }
                self.each_view_showing(&session, |me| {
                    me.finish_card(&call.id, &result);
                    me.draw_working();
                });
                if self.watched(&session) {
                    self.stream_out(&session, neosh_proto::StreamEvent::ToolFinished {
                        turn: turn.0.clone(),
                        call: call.clone(),
                        result: result.clone(),
                    });
                }
                self.broadcast(PluginEvent::ToolFinished { session, turn, call, result });
            }
            AgentEvent::TurnEnded { session, turn, stop_reason, usage } => {
                self.each_view_showing(&session, |me| {
                    me.close_answer();
                    // Read before the footer goes away with the working line, and drawn again
                    // after: the footer is state, and this is the copy that stays. What the turn
                    // set out to do and how much of it it got through is the one thing a finished
                    // turn leaves behind that its answer usually does not say.
                    let plan =
                        me.rounds.get(&session).map(|r| r.plan.clone()).unwrap_or_default();
                    // Anything the turn is walking away from. A backgrounded sub-agent is still
                    // running when the answer arrives, and the footer it was listed in goes away
                    // with the working line — so without this the one case people actually ask
                    // about, "is something still going?", is the one that leaves no trace.
                    let left = me.tasks_left_running(&session);
                    let changes =
                        me.rounds.get(&session).map(|r| r.changes.clone()).unwrap_or_default();
                    // Where the turn's own closing rows belong. `None` is the end of the buffer,
                    // which is right whenever the last thing there is something the turn said.
                    // When it is a question steered in after that — taken in at the final gap, and
                    // answered by nothing — the end of the buffer is *below* it, and a plan and a
                    // diff summary appended there put two blocks of the previous exchange under a
                    // message you had just typed. Taken rather than read, because these rows are
                    // what stops it being the last thing in the transcript.
                    let mut at = me.vm().unanswered.take();
                    // The working block sits below all of this, so taking it away first moves
                    // none of the rows named here.
                    me.end_working();
                    me.vm().streaming = None;
                    // One after another, each below the last: every block that lands pushes the
                    // question — and so the row the next one goes above — further down.
                    let n = me.draw_plan(&plan, at);
                    at = at.map(|row| row + n);
                    let n = me.place_block(at, &left);
                    at = at.map(|row| row + n);
                    let n = me.draw_summary(&changes, at);
                    at = at.map(|row| row + n);
                    me.close_unanswered(at, &stop_reason);
                    // The margin above this turn's question stops being a clock time and becomes
                    // a clock time and a duration. However the turn ended: an interrupt and an
                    // error both took as long as they took, and a turn that says nothing about
                    // how long it ran is the one you cannot tell from one that never started.
                    me.end_turn_time(now_secs());
                });
                // Queued after the last gap this turn had. It is a question that was asked and not
                // yet answered, so it becomes the next turn rather than being quietly dropped.
                let leftover = self.agent.take_steering(&session);
                self.turns.remove(&session);
                self.rounds.remove(&session);
                // Read out before the fields below are cleared, because a notification about a
                // turn wants to know how long it took and what to call the conversation, and
                // `running_since` is about to be `None`.
                let ended = self.agent.sessions().get(&session).map(|s| {
                    (s.label(), s.running_since.map(|at| now_secs().saturating_sub(at).max(0)))
                });
                if let Some(s) = self.agent.sessions().get_mut(&session) {
                    s.updated_at = now_secs();
                    // The turn ended, so there is nothing for a later process to notice. Cleared
                    // here for *every* ending — finished, errored, interrupted — because the note
                    // means "no one ended this", and all three of those are someone ending it.
                    s.running_since = None;
                    // News, and only where you were not looking. A turn that ended in the
                    // conversation on screen has been seen by definition — including with nothing
                    // attached, because reattaching lands you in that conversation with the answer
                    // already in it. Anywhere else, the row is the only thing that will ever
                    // mention it, so it has to say so until you go there.
                    s.unread = !seen;
                }
                if let Some((label, took)) = ended {
                    self.alert_turn_ended(&session, &stop_reason, &label, took);
                }
                self.persist_sessions();
                if self.watched(&session) {
                    self.stream_out(&session, neosh_proto::StreamEvent::TurnEnded {
                        turn: turn.0.clone(),
                        stop_reason: stop_reason.clone(),
                        usage: usage.clone(),
                    });
                }
                self.broadcast(PluginEvent::TurnEnded {
                    session: session.clone(),
                    turn,
                    stop_reason,
                    usage,
                });
                // The one moment these numbers definitely moved. Asked for rather than done here:
                // the vendor's own accounting lags the end of the stream by a second or two, and a
                // poll at zero reliably returns the figure from *before* the turn — which draws as
                // a turn that cost nothing at all.
                self.quota_due = true;
                // The ending this conversation was waiting for. A turn nobody asked for that began
                // while this one was winding down could not be opened then — see [`Self::unheard`]
                // — and the gap it fell into ends exactly here.
                //
                // Before the leftover below, and that ordering is the whole of it: what the agent
                // has already started saying came first, so it is heard first, and the message you
                // typed in the meantime is steered into that listening turn and asked at the end of
                // it. The other way round, your turn would drain what the agent said and drop it —
                // your message written over the thing it was answering.
                if self.unheard.remove(&session) {
                    self.hear_out(session.clone());
                }
                if !leftover.is_empty() {
                    self.start_turn_in(session, Prompt {
                        text: leftover
                            .iter()
                            .map(|p| p.text.as_str())
                            .collect::<Vec<_>>()
                            .join("\n\n"),
                        images: leftover.into_iter().flat_map(|p| p.images).collect(),
                    });
                }
                // The field says what `\u{23ce}` does, and what it does just changed back.
                self.refresh_composer();
            }
            // What the turn produced is in the conversation now, so the round's copy of it would
            // only go stale — and drawing both on a switch would say everything twice.
            //
            // And it is the moment to write it down. A turn is minutes of work and dozens of tool
            // rounds, and until this the only saves were at either end of it: the conversation on
            // disk held the message you typed and nothing after it, for as long as the turn ran.
            // Anything that took the workspace out without warning — a crash, an OOM kill, a
            // reboot — lost the whole answer and left the transcript ending on your own question.
            // A round boundary is what the agent itself resumes from, so it is the right grain to
            // save at, and one conversation is written rather than every conversation in the
            // workspace: `persist_sessions` several times a minute for the sake of the one that
            // changed is a hundred files rewritten to record one.
            AgentEvent::Committed { session, .. } => {
                if let Some(r) = self.rounds.get_mut(&session) {
                    r.said.clear();
                }
                self.persist_session(&session);
            }
            AgentEvent::Steered { session, prompt, .. } => {
                // Taken in over here, so asked over there too — see `StreamEvent::Asked`.
                if self.watched(&session) {
                    self.stream_out(&session, neosh_proto::StreamEvent::Asked {
                        text: prompt.text.clone(),
                        images: prompt.images.len() as u32,
                        pictures: picture_files(&prompt.images),
                    });
                }
                // A turn opened to hear the agent out has just been handed a question, so it is not
                // a report any more. Left alone, the line went on saying `Reporting back` over the
                // answer to the thing you had typed — which is the same lie as the one that label
                // exists to stop, told the other way round.
                if let Some(r) = self.rounds.get_mut(&session).filter(|r| r.verb == REPORTING) {
                    r.verb = a_verb();
                }
                // Drawn now rather than when it was typed: until the model has been told, a
                // transcript showing the question is a transcript that is lying about what was
                // asked. Off screen it needs no drawing at all — it is in the conversation's
                // messages, which is what a switch back rebuilds from.
                self.each_view_showing(&session, |me| {
                    me.chat_question(&prompt);
                    me.draw_working();
                    me.refresh_composer();
                    // Where it landed is where you should be. It was drawn above the working
                    // line, which is off the bottom of a transcript you had scrolled back in.
                    me.scroll_chat(0);
                });
            }
            AgentEvent::Activity { session, turn, activity } => {
                // A driver's account of its own loop, carried separately from `Token` here for the
                // same reason it is locally: it is not a content block.
                if self.watched(&session) {
                    self.stream_out(&session, neosh_proto::StreamEvent::Activity {
                        turn: turn.0.clone(),
                        activity: activity.clone(),
                    });
                }
                self.handle_activity(session, turn, activity);
            }
            AgentEvent::Notice { session, level, text } => {
                // A provider failing mid-stream is said here and nowhere else, so somebody reading
                // this conversation from another machine is told too — or the turn there simply
                // stops, with the reason in a corner of a screen nobody is looking at.
                if self.watched(&session) {
                    self.stream_out(&session, neosh_proto::StreamEvent::Notice {
                        level,
                        text: text.clone(),
                    });
                }
                self.editor_message(level, text);
            }
        }
    }

    /// Route one thing the driver's own loop said about itself.
    ///
    /// Split from [`Self::handle_agent_event`] for the same reason it exists at all: none of this
    /// is part of the message being produced, and folding it in would mean every arm of that match
    /// having to decide whether it was looking at content or at commentary.
    ///
    /// The same bookkeeping-versus-drawing split applies. A conversation off screen still learns
    /// which sub-agents it has out and what its plan is, because switching to it should show what
    /// is true — not what was true when you last looked.
    fn handle_activity(
        &mut self,
        session: neosh_proto::SessionId,
        turn: neosh_proto::TurnId,
        activity: neosh_proto::Activity,
    ) {
        use neosh_proto::{Activity, TaskStatus};
        // Out to plugins before it is drawn, and whatever it is. A meter, a plan view and a status
        // line all want this and none of them caused it — and it is the only thing that moves
        // *during* a turn, which for an agent driver can be twenty minutes long.
        self.broadcast(PluginEvent::Activity {
            session: session.clone(),
            turn,
            activity: activity.clone(),
        });
        match activity {
            Activity::Plan { steps } => {
                let Some(r) = self.rounds.get_mut(&session) else { return };
                if r.plan == steps {
                    return;
                }
                r.plan = steps;
                self.each_view_showing(&session, |me| {
                    me.redraw_footer();
                });
            }
            // Announced twice on purpose by the driver — once as a snapshot of what is running,
            // once when this turn's own call starts one — so this is written to be idempotent.
            // The second telling knows more (which call it belongs to, which kind of agent it is)
            // and is allowed to fill in what the first could not.
            Activity::TaskStarted { task, title, role, backgrounded, .. } => {
                let Some(r) = self.rounds.get_mut(&session) else { return };
                // Let go of at the moment it started — a detached shell, a sub-agent spawned into
                // the background — is not something this turn is waiting on, and the footer draws
                // it still rather than live from the first frame it appears in.
                let status =
                    if backgrounded { TaskStatus::Backgrounded } else { TaskStatus::Running };
                match r.tasks.iter_mut().find(|t| t.id == task) {
                    Some(t) => {
                        t.title = title;
                        if role.is_some() {
                            t.role = role;
                        }
                        if backgrounded {
                            t.status = status;
                        }
                    }
                    None => r.tasks.push(Task {
                        id: task,
                        title,
                        role,
                        since: std::time::Instant::now(),
                        status,
                    }),
                }
                self.each_view_showing(&session, |me| {
                    me.redraw_footer();
                });
            }
            // What it is up to, in the line that is already on screen for it. Nothing is inserted:
            // a sub-agent that reports every few seconds would otherwise push the answer it is
            // helping to write off the top of the screen.
            Activity::TaskProgress { task, summary, last_tool, .. } => {
                let Some(r) = self.rounds.get_mut(&session) else { return };
                let Some(t) = r.tasks.iter_mut().find(|t| t.id == task) else { return };
                if let Some(text) = summary.or(last_tool) {
                    t.title = text;
                }
                self.each_view_showing(&session, |me| {
                    me.redraw_footer();
                });
            }
            // The whole set, whole, and therefore *replacing* the whole set. Not merged into the
            // turn's own list of sub-agents: that one is edges — what this turn started and is
            // waiting on — and this is a level, said by the driver precisely so that a receiver
            // which only wants "is anything still running" cannot wedge on a bookend it never got.
            // See `Activity::Background`.
            //
            // On the conversation and not the round, which is the entire point: the round is thrown
            // away when the turn ends, and a shell that was put in the background is still running
            // after that.
            Activity::Background { tasks } => {
                if self.take_background(&session, tasks) {
                    // Nothing else is broadcast here: the activity itself already went out to every
                    // plugin above, and that is the signal that means what it means.
                    // `SessionChanged` means the conversation you are *in* changed, and borrowing
                    // it to say "a row's state moved" would have every listener refreshing a footer
                    // about a conversation nobody switched to.
                    self.each_view_showing(&session, |me| {
                        me.redraw_footer();
                    });
                }
            }
            Activity::TaskEnded { task, status, .. } => {
                let Some(r) = self.rounds.get_mut(&session) else { return };
                let Some(t) = r.tasks.iter_mut().find(|t| t.id == task) else { return };
                if t.status == status {
                    return;
                }
                t.status = status;
                self.each_view_showing(&session, |me| {
                    me.redraw_footer();
                });
            }
            // Not answering, and now the line says why.
            //
            // Straight onto the working line rather than into the transcript: it is a state and not
            // an event, it is over in seconds or minutes, and a row about it would still be sitting
            // in the conversation a week later saying nothing anybody wants. The clock beside it is
            // the turn's own and keeps running, which is the half that makes a wait bearable.
            Activity::Waiting { what, retry_in_ms } => {
                let Some(r) = self.rounds.get_mut(&session) else { return };
                r.note_is_wait = true;
                r.note = Some(match retry_in_ms {
                    // Rounded up, because a wait reported as `0s` is a wait that reads as a lie.
                    Some(ms) => format!("{what} \u{b7} in {}s", ms.div_ceil(1000).max(1)),
                    None => what,
                });
                self.each_view_showing(&session, |me| {
                    me.draw_working();
                });
            }
            // The several seconds where the driver answers nothing at all. Without this it is
            // indistinguishable from a hang, which is exactly what `/compact` looked like.
            Activity::Compacting => {
                if let Some(r) = self.rounds.get_mut(&session) {
                    r.note = Some("Compacting the conversation".into());
                    // Ended by `Compacted`, which is a real event, so it is not a self-ending wait.
                    r.note_is_wait = false;
                }
                self.each_view_showing(&session, |me| {
                    me.draw_working();
                });
            }
            Activity::Compacted { before, after } => {
                if let Some(r) = self.rounds.get_mut(&session) {
                    r.note = None;
                    r.note_is_wait = false;
                    // The plan was built from calls the driver can no longer see. Keeping it would
                    // be a checklist for work nothing is going to pick up.
                    r.plan.clear();
                }
                // What the window holds *now*. Compaction is the one moment the number moves
                // without a request being made, and the card said `20.3k → 1.6k` while the meter
                // under it went on reporting the number from before — for the rest of the
                // conversation, because a driver that has answered "how full is it" once turns the
                // estimate in `add_usage` off, and the next answer only arrives with the next
                // request. The driver said `post_tokens`; this is what it is for.
                if let Some(used) = after {
                    // No window with it, and none is wanted: a boundary is a count, and
                    // `set_context` keeps whatever denominator is already known. Reading the old
                    // one back out to hand it straight in again was this doing `set_context`'s job
                    // — and doing it through the session lock, which is the one thing here that
                    // must not be taken twice in a statement.
                    if let Some(s) = self.agent.sessions().get_mut(&session) {
                        s.set_context(used, 0);
                    }
                    self.persist_sessions();
                }
                self.each_view_showing(&session, |me| {
                    let r = cards::compaction(&me.glyphs(), before, after);
                    let row = me.chat_push(vec![r.text.clone()]);
                    me.chat_row(row, &r);
                    me.redraw_footer();
                    me.draw_working();
                });
            }
            Activity::Commands { commands } => {
                self.remember_commands(&session, &commands);
                self.driver_commands.insert(session, commands);
            }
            // What the driver calls this conversation, written down where it will still be after
            // the process that said it has gone. Persisted rather than held: the value is only
            // worth anything on a run that is not this one.
            Activity::Resume { token } => {
                let changed = match self.agent.sessions().get_mut(&session) {
                    Some(s) if s.resume.as_deref() != Some(token.as_str()) => {
                        s.resume = Some(token);
                        true
                    }
                    _ => false,
                };
                if changed {
                    self.persist_sessions();
                }
            }
            // The driver's own answer to "how full is it", which beats anything derived. A prompt
            // size counts one request; this counts the conversation the agent is holding, and it
            // brings the window with it — a vendor CLI's window is whatever that CLI decided, not
            // whatever a catalogue says the model has.
            Activity::Context { used, total } => {
                if let Some(s) = self.agent.sessions().get_mut(&session) {
                    s.set_context(used, total);
                }
                self.persist_sessions();
            }
            // Attributed to the *conversation's* instance, not to whatever the picker is showing.
            // A workspace runs several conversations at once and they need not be on the same
            // provider, so reading the active selection here would file `codex`'s weekly figure
            // under the account you happen to have open.
            Activity::Quota { plan, windows, credits } => {
                // Another machine's plan, reported by its own login: filed here it would land on
                // whichever of this machine's instances shares the id, and the strip would say
                // your allowance moved when somebody else's did.
                if self.mirror_meta(&session).is_some() {
                    return;
                }
                let Some(instance) = self.instance_of(&session) else { return };
                self.take_quota(
                    instance,
                    neosh_proto::QuotaSource::Turn,
                    neosh_provider::quota::Quota { plan, windows, credits },
                );
            }
        }
    }

    /// Which configured instance a conversation is talking to.
    ///
    /// The conversation's own selection first, and the workspace's only as the fallback for one
    /// that has not chosen. Getting this the other way round attributes every driver's report to
    /// whichever row the model picker was last left on.
    fn instance_of(
        &self,
        session: &neosh_proto::SessionId,
    ) -> Option<neosh_proto::InstanceId> {
        let chosen = {
            let store = self.agent.sessions();
            store.get(session).and_then(|s| s.selection.as_ref().map(|sel| sel.instance.clone()))
        };
        chosen.or_else(|| self.agent.selection().map(|sel| sel.instance))
    }

    /// Put a snapshot in the store, and tell everyone if it changed anything.
    ///
    /// One door, whatever the source: a plugin's provider, a mid-turn line and a poll all arrive
    /// here. That is what makes a third-party provider's plan appear in the sidebar strip without
    /// the sidebar having heard of that provider — the strip listens for the event, not the vendor.
    fn take_quota(
        &mut self,
        instance: neosh_proto::InstanceId,
        source: neosh_proto::QuotaSource,
        quota: neosh_provider::quota::Quota,
    ) {
        let Some(snapshot) = self.quota.apply(instance, source, quota, now_secs()) else { return };
        self.broadcast(PluginEvent::Quota { snapshot });
    }

    /// Whether a plugin registered the driver serving this instance.
    ///
    /// The gate on [`ApiCall::QuotaReport`]. Without it any plugin could publish any number for
    /// `claude-cli`, and this strip is a thing people are going to look at before deciding whether
    /// to spend an afternoon on something — a figure it is possible to spoof is a figure that
    /// should not be drawn as fact.
    fn plugin_drivers_own(
        &self,
        plugin: &PluginId,
        instance: &neosh_proto::InstanceId,
    ) -> bool {
        let Some(drivers) = self.plugin_drivers.get(plugin) else { return false };
        self.agent
            .providers()
            .instances()
            .any(|cfg| &cfg.id == instance && drivers.contains(&cfg.driver))
    }

    /// Every instance this machine could actually ask, right now.
    ///
    /// Built from the configured instances rather than from a written list of vendors, so an
    /// instance the user named something else — a second `claude-cli` pointed at another account —
    /// is polled too. Whether it *can* answer is decided in the poll itself, which is where the
    /// filesystem and `PATH` checks belong.
    fn quota_targets(&self) -> Vec<crate::quota::Target> {
        self.agent
            .providers()
            .usable_instances()
            .filter_map(|cfg| {
                let which = neosh_provider::quota::pollable(cfg.driver.0.as_str())?;
                // The program to run is the one the instance says it authenticates with, which is
                // also the one that owns the login being read. A default here would poll the
                // `codex` on `PATH` for an instance pointed at a different build of it.
                let program = match &cfg.auth {
                    neosh_proto::AuthRef::Cli { program, .. } => program.clone(),
                    _ => return None,
                };
                Some(crate::quota::Target { instance: cfg.id.clone(), which, program })
            })
            .collect()
    }

    /// Ask, off the host loop.
    ///
    /// Nothing is awaited here. A poll is a network round trip and a process spawn, and the host
    /// loop is the single writer for the editor — awaiting one inside it freezes typing for as long
    /// as somebody else's server takes to answer.
    fn refresh_quota(&self, only: Option<neosh_proto::InstanceId>) {
        // A poll reads the vendor CLI's own login to ask about the vendor's own account. That is
        // the same trade the CLI drivers already make — neosh is useful the minute it is installed
        // because it reuses a login you have — but it is somebody else's credential store, so it is
        // a setting rather than a decision made on their behalf. Off, the gauges show what the last
        // turn reported and say how long ago that was.
        if self.editor.options().bool("usage.poll") == Some(false) {
            return;
        }
        let mut targets = self.quota_targets();
        if let Some(id) = only {
            targets.retain(|t| t.instance == id);
        }
        if targets.is_empty() {
            return;
        }
        let tx = self.quota_tx.clone();
        tokio::spawn(crate::quota::poll(targets, tx));
    }

    async fn handle_script_out(&mut self, out: ScriptOutbound) {
        match out {
            ScriptOutbound::Loaded { plugin, error } => {
                match error {
                    None => tracing::info!(%plugin, "plugin loaded"),
                    Some(e) => {
                        tracing::error!(%plugin, "failed to activate: {e}");
                        self.editor_message(MessageLevel::Error, format!("{plugin}: {e}"));
                        if let Some(info) = self.plugin_infos.get_mut(&plugin.0) {
                            info.state = "failed".into();
                            info.error = Some(e.clone());
                        }
                        // It threw *during* activation, which means it may already have armed
                        // timers and registered commands, tools and hooks — and is on the
                        // broadcast list, having been put there when its load was asked for.
                        // Nothing else will ever clean those up: reload skips a plugin that is not
                        // loaded, so every reload would stack another copy.
                        self.tear_down_plugin(&plugin);
                    }
                }
                self.advance_startup(&plugin);
                self.woke(&plugin);
            }
            ScriptOutbound::Log { level, message } => match level {
                MessageLevel::Error => tracing::error!("plugin runtime: {message}"),
                MessageLevel::Warn => tracing::warn!("plugin runtime: {message}"),
                MessageLevel::Info => tracing::info!("plugin runtime: {message}"),
            },
            ScriptOutbound::Plugin { plugin, msg } => match msg {
                PluginOutbound::Ready { .. } => {}
                PluginOutbound::Call { id, call, view } => {
                    // Which terminal this call is about, for the whole of answering it. A plugin
                    // acting on a bound namespace has said which; one that has not is put wherever
                    // the call ought to go, which for a command run from a key press is the
                    // terminal it was pressed in. Put back afterwards, because between calls the
                    // host is nobody's.
                    let was = self.from;
                    self.from.view = view.unwrap_or_else(|| self.view_for(&call));
                    self.plugin_call(plugin, id, call).await;
                    self.from = was;
                }
                PluginOutbound::Notify { call, view } => {
                    let was = self.from;
                    self.from.view = view.unwrap_or_else(|| self.view_for(&call));
                    self.plugin_notify(plugin, call).await;
                    self.from = was;
                }
                PluginOutbound::Response { id, response } => match response {
                    PluginResponse::Tool { result } => self.bridge.resolve_tool(&id, result),
                    PluginResponse::Hook { outcome } => self.bridge.resolve_hook(&id, outcome),
                    PluginResponse::Command { value } => self.bridge.resolve_command(&id, value),
                    PluginResponse::ProviderAccepted => self.bridge.resolve_accept(&id),
                    PluginResponse::Error { message } => {
                        tracing::warn!(%plugin, "{message}");
                        self.bridge.fail(&id, message);
                    }
                },
            },
        }
    }

    /// One plugin call, answered. Split out so the terminal it is about can be set for the whole
    /// of it and put back afterwards, however many ways out of it there are.
    async fn plugin_call(&mut self, plugin: PluginId, id: RequestId, call: ApiCall) {
        // In another machine's conversation, what can answer it is that machine's list.
        if self.answer_from_catalogue(&plugin, &id, &call) {
            return;
        }
        // Answered when the prompt closes rather than now: the host has to read the
        // keyboard before it knows whether a key was entered, and the plugin is
        // waiting to hear exactly that.
        // Signing in is to a provider on *this* machine, and the picker in another machine's
        // conversation is listing that machine's providers. Said, rather than asking for a key
        // this machine would then keep for a provider it is not using.
        if let ApiCall::ProviderSetCredential { .. } = &call
            && let Some(m) = self.mirror_meta(&self.active_session())
        {
            let answer = Err(ApiError::Denied {
                reason: format!("sign in on {} — its providers are its own", m.machine),
            });
            self.respond_plugin(plugin, id, answer);
            return;
        }
        if let ApiCall::ProviderSetCredential { instance, replace } = &call {
            let waiting = Some((plugin.clone(), id.clone()));
            if let Err(e) =
                self.begin_secret(instance.clone(), *replace, waiting)
            {
                let response: ApiResponse = Err(e).into();
                let _ = self.bridge.script().send(ScriptInbound::Plugin {
                    plugin,
                    msg: PluginInbound::Response { id, response },
                });
            }
            return;
        }
        // Answered when the owning machine answers, not now. Every ASCP command is
        // replied to exactly once, so this settles — unless the peer has gone, which
        // the swarm reports as a disconnect and which settles it too.
        // Dials an address that may not answer, so it cannot run on the loop.
        // Answered when the picture is here rather than now: what is on the
        // clipboard is sometimes a URL, and going to get one is somebody else's server
        // taking as long as it likes.
        if let ApiCall::ChatAttach { path: None } = &call {
            self.begin_paste(Some((plugin.clone(), id.clone())));
            return;
        }
        // Answered when the command's owner answers. Waiting for that on the loop would freeze
        // typing for as long as the handler takes, and a handler is allowed to open a picker and
        // wait for a person.
        if let ApiCall::CmdCall { name, args } = &call {
            self.begin_command_call(plugin, id, name.clone(), args.clone());
            return;
        }
        if let ApiCall::SwarmProbe { addr } = &call {
            self.begin_swarm_probe(addr.clone(), (plugin.clone(), id.clone()));
            return;
        }
        if let ApiCall::SwarmCommand { node, session, command } = &call {
            self.begin_swarm_command(
                node.clone(),
                session.clone(),
                command.clone(),
                (plugin.clone(), id.clone()),
            );
            return;
        }
        if let ApiCall::SwarmBrowse { node, prefix } = &call {
            self.begin_swarm_browse(node.clone(), prefix.clone(), (plugin.clone(), id.clone()));
            return;
        }
        if self.spawn_slow(&plugin, Some(id.clone()), &call) {
            return;
        }
        let response: ApiResponse = self.dispatch(&plugin, call).await.into();
        let _ = self.bridge.script().send(ScriptInbound::Plugin {
            plugin,
            msg: PluginInbound::Response { id, response },
        });
    }

    /// The same, for a call that wants no answer.
    async fn plugin_notify(&mut self, plugin: PluginId, call: ApiCall) {
        if let ApiCall::ChatAttach { path: None } = &call {
            self.begin_paste(None);
            return;
        }
        if let ApiCall::ProviderSetCredential { instance, replace } = &call {
            if let Err(e) = self.begin_secret(instance.clone(), *replace, None) {
                tracing::warn!(%plugin, "{e}");
            }
            return;
        }
        if self.spawn_slow(&plugin, None, &call) {
            return;
        }
        if let Err(e) = self.dispatch(&plugin, call).await {
            // Fire-and-forget calls have nowhere to return an error, so surface it
            // rather than letting it vanish.
            tracing::warn!(%plugin, "{e}");
        }
    }

    fn handle_input(&mut self, input: InputEvent) -> bool {
        // The prefix panel describes one half-typed chord, so the press after it appeared ends it
        // whatever that press is — including the one that completes the chord, which is about to
        // do the thing the panel was describing. Taken down *before* the key is handled: a hint
        // still on screen over the split it just told you how to make is a panel you would then
        // have to dismiss.
        if self.window_hint.is_some() && matches!(input, InputEvent::Key { .. }) {
            self.drop_window_hint();
        }
        // A shell takes the keyboard whole. Everything a terminal can receive belongs to the
        // program inside it — `^C` interrupts *that*, `^D` ends *its* input — so the workspace's own
        // bindings step aside, and the one way back out is the window prefix.
        //
        // Two exceptions, and both are about not stealing a chord: a sequence already in flight
        // belongs to the keymap that started it, and the key that *starts* the prefix has to reach
        // the keymap or there would be no way to leave a full-screen program.
        if self.v().term {
            let pane = self.v().pane;
            let view = self.view_now();
            match input {
                InputEvent::Key { key }
                    if self.editor.pending_keys(view).is_empty() && !self.starts_prefix(&key) =>
                {
                    let bytes = crate::term::encode(&key);
                    if let Some(t) = self.terminals.get_mut(pane) {
                        t.write(&bytes);
                    }
                    return true;
                }
                InputEvent::Paste { text } => {
                    // Bracketed, so a program that understands it knows this was pasted rather than
                    // typed — which is what stops an editor auto-indenting every line of it.
                    if let Some(t) = self.terminals.get_mut(pane) {
                        t.write(b"\x1b[200~");
                        t.write(text.as_bytes());
                        t.write(b"\x1b[201~");
                    }
                    return true;
                }
                _ => {}
            }
        }
        // A key being typed in takes the keyboard whole, bindings included. Routing it through the
        // keymaps first would mean a key containing `n` — every base64 key does — firing whatever
        // `n` is bound to, halfway through the paste.
        if self.v().secret.is_some() {
            match input {
                InputEvent::Key { key } => self.secret_key(key),
                InputEvent::Paste { text } => self.secret_paste(&text),
                other => return self.handle_input_uncaptured(other),
            }
            return true;
        }
        // The new-tab panel takes the keyboard whole: `j`, `k` and the digits are answers to the
        // question on screen, and every one of them is also bound to something else.
        if self.tab_choice.is_some() {
            match input {
                InputEvent::Key { key } => self.tab_choice_key(key),
                other => return self.handle_input_uncaptured(other),
            }
            return true;
        }
        // Naming a tab takes it the same way, and for the same reason a search does: every letter
        // of the name you are typing is also somebody's binding.
        if self.v().rename.is_some() {
            match input {
                InputEvent::Key { key } => self.rename_key(key),
                InputEvent::Paste { text } => {
                    if let Some(p) = self.vm().rename.as_mut() {
                        // One line. A pasted paragraph is not a tab name, and the newlines in it
                        // would be drawn as a strip several rows tall.
                        p.value.push_str(text.replace('\n', " ").trim());
                    }
                    self.render_rename();
                }
                other => return self.handle_input_uncaptured(other),
            }
            return true;
        }
        // A search being typed takes the keyboard the same way, and for a milder version of the
        // same reason: `n` is a letter in most words and it is also the key for "next match".
        if self.v().search.is_some() {
            match input {
                InputEvent::Key { key } => self.search_key(key),
                InputEvent::Paste { text } => {
                    if let Some(p) = self.vm().search.as_mut() {
                        p.query.push_str(text.trim());
                    }
                    self.render_search();
                    let (query, back, from) = {
                        let p = self.v().search.as_ref().expect("checked above");
                        (p.query.clone(), p.back, p.from)
                    };
                    self.highlight_matches(&query);
                    if let Some(at) = self.find_from(&query, from, !back, true) {
                        self.reading_jump(at);
                    }
                }
                other => return self.handle_input_uncaptured(other),
            }
            return true;
        }
        let handled = self.handle_input_uncaptured(input);
        self.refresh_composer();
        handled
    }

    fn handle_input_uncaptured(&mut self, input: InputEvent) -> bool {
        match input {
            InputEvent::Key { key } => {
                let res = self.editor.feed_key(self.from.view, key);
                self.keys_pending = matches!(res, neosh_core::KeyResolution::Pending);
                // Whose chord it is, so the timeout that eventually gives up on it gives up on the
                // right keyboard. The deadline fires on a timer rather than on a key, by which
                // time `self.from` is nobody's — a half-typed `g` in one terminal would otherwise
                // be replayed as an ordinary `g` into another.
                self.keys_view = self.from.view;
                self.keys_touched = true;
                self.drain_effects();
            }
            InputEvent::Paste { text } => {
                // A panel with the keyboard gets what was pasted into it. Pasted anywhere else, the
                // text went into the composer *behind* the panel — a field you cannot see, from a
                // key you pressed at something else — which is the one thing a modal promises not
                // to do with a keystroke and was doing with a paste. The panel is told rather than
                // typed at: it is a plugin's, and only it knows whether it is a field, a filter, or
                // a sheet that has nothing to do with text.
                let view = self.view_now();
                if let Some(win) = self.editor.focused(view).filter(|w| {
                    *w != self.v().composer_win
                        && self
                            .editor
                            .window(*w)
                            .is_some_and(|w| matches!(w.layout, WindowLayout::Float { .. }))
                }) {
                    self.broadcast(PluginEvent::Event {
                        name: "neosh.paste".to_string(),
                        data: Some(serde_json::json!({ "win": win, "text": text })),
                        from: BUILTIN.to_string(),
                    });
                    return true;
                }
                // At the cursor, replacing whatever is selected — the same edit typing performs,
                // because a paste that always lands at the end is not a paste.
                if self.v().reading {
                    self.leave_reading();
                }
                // Unless it is a picture. Dragging a file onto a terminal window pastes its path,
                // which is the only gesture a terminal has for "this file" — so a paste that is
                // one line, is a path, exists, and turns out to hold an image becomes an
                // attachment instead of a line of text nobody wanted typed out. Everything that
                // is not all four of those is text, because swallowing something somebody meant
                // to type is much worse than making them press a key.
                if let Some(path) = crate::images::pasted_path(&text) {
                    match self.attach(&path) {
                        // Nothing said about it. The chip that just appeared above the field *is*
                        // the report, and a line saying the same words in the same glance is the
                        // kind of noise that teaches people not to read either one.
                        Ok(_) => return true,
                        // It looked like an image and was not one. Fall through: what was pasted
                        // is a path, and a path is perfectly good text.
                        Err(e) => tracing::debug!("not attaching {}: {e}", path.display()),
                    }
                }
                self.compose(TextEdit::Insert { text });
            }
            InputEvent::ViewportChanged { win, width, height, top_line, rows } => {
                let was = self.chat_width();
                let before = self
                    .editor
                    .window(win)
                    .and_then(|w| w.viewport.as_ref())
                    .map(|v| (v.width, v.height));
                self.editor
                    .set_viewport(win, neosh_core::Viewport { width, height, top_line, rows });
                // A size is the one thing only the frontend knows; a plugin that sized a meter to
                // it hears here that it has to size it again.
                if before != Some((width, height)) {
                    self.broadcast(PluginEvent::Event {
                        name: "neosh.viewport".to_string(),
                        data: Some(
                            serde_json::json!({ "win": win, "width": width, "height": height }),
                        ),
                        from: BUILTIN.to_string(),
                    });
                    // The strip is *fitted* to its own width — segments are shortened and then
                    // dropped until the line fits — and this is the only place that width arrives.
                    // Without this the fitting stands from whenever a plugin last wrote a segment,
                    // which at rest is not again: nothing lays the strip out unless something in
                    // it changed, and at rest nothing in it does. So a terminal made wider kept a
                    // strip missing everything that had not fitted the old one, and a terminal
                    // made narrower kept a strip too long for it, with the right-hand half drawn
                    // over the end of the left — until a turn started or a conversation changed and
                    // it silently put itself right, which is the version of this that is hardest to
                    // report.
                    let strip = self.editor.window(win).map(|w| w.buf) == Some(self.vs().status);
                    if strip {
                        self.refresh_status();
                    }
                    // The tab strip is fitted to its own width for the same reason the status line
                    // is — labels are dropped and counted until the row fits — and this is the only
                    // place that width arrives.
                    if self.views.values().any(|v| v.tabline_win == win) {
                        self.refresh_tabline();
                    }
                }
                // Which pane this window is in — *not* whichever pane happens to have the keyboard.
                // A split resizes two panes at once and the frontend reports each window
                // separately, so half of these events are about the pane you are not in. Tested
                // against `self.v()` they were answered by the active pane's state, which meant a
                // scroll position taken from the wrong transcript and a redraw that never happened
                // for the other half of the screen.
                let owner = self.pane_of_window(win);
                if let Some((at, pane)) = owner {
                    self.in_pane(at, pane, |me| {
                        // What the frontend *drew*, which is not always what was asked for: a
                        // window whose scroll had to bend to keep its cursor on screen is showing a
                        // different first row. Taking it back means the next page starts from where
                        // the screen is rather than from where the core last guessed, which with
                        // wrapped rows are different places.
                        if win == me.v().chat_win && me.v().chat_top.is_some() {
                            me.vm().chat_top = Some(top_line);
                        }
                        // This is where the host *learns* a width — `Resize` is the terminal's
                        // outer size and a transcript is some part of it, which since splitting is
                        // a part that changes without the terminal changing at all. So anything
                        // drawn to a width has to be drawn again here, or a pane halved by a split
                        // keeps the whole region's layout until something unrelated redraws it.
                        if win == me.v().chat_win && me.chat_width() != was {
                            me.draw_welcome();
                            // And the block at the bottom of a running turn, for the same reason:
                            // the plan and what is out are laid out against `chat_width`, so after
                            // a resize they are wrapped to a width the window no longer has.
                            me.redraw_footer();
                        }
                        // The composer too, which nothing used to have to redraw: its rule is a run
                        // of `─` as wide as the field, and until a pane could be narrowed on its
                        // own the only thing that changed that width was the terminal itself. A
                        // split halves it, and a rule drawn for the old width wraps onto a second
                        // row — a composer that grows a line every time you split.
                        if win == me.v().composer_win {
                            me.refresh_composer();
                        }
                        // And a shell is told how big it is, which is what sends it `SIGWINCH` —
                        // without it a program redraws to the size the pane used to be.
                        if me.v().term && win == me.v().chat_win {
                            let pane = me.v().pane;
                            if let Some(t) = me.terminals.get_mut(pane) {
                                t.resize(height, width);
                            }
                            let plugin = PluginId::from(BUILTIN);
                            let _ = me.editor.apply_in(at, &plugin, ApiCall::SurfaceClaim {
                                win,
                                rect: neosh_proto::Rect { row: 0, col: 0, width, height },
                            });
                            me.scaffold_terminal(at, pane, height, width);
                            me.draw_terminal(at, pane);
                        }
                    });
                }
            }
            InputEvent::Attached { session, .. } => {
                let arriving = self.from.view;
                // A terminal we have never met gets a screen of its own, in a conversation nobody
                // else is reading. That is the whole of what a second window is for: you opened it
                // because something is running in the first one, so landing you in the same place
                // would be the one thing guaranteed not to help.
                if !self.views.contains_key(&arriving) {
                    self.open_view(arriving, session);
                }
                // A client that has never seen any of this. Whatever was queued for *this view*
                // was queued for whoever was looking before — and nobody may have been — so it is
                // dropped rather than sent to a mirror it makes no sense against, and the whole
                // state is said again in its place. See `Editor::republish`.
                //
                // Only this view's share, and the workspace's: a frame half-built for the terminal
                // next door is a frame that terminal is still waiting for, and throwing all of it
                // away because somebody else opened a window is how a view goes quiet.
                let held: Vec<_> = self
                    .editor
                    .drain_ui()
                    .into_iter()
                    .filter(|(v, _)| v.is_some_and(|v| v != arriving))
                    .collect();
                self.editor.requeue(held);
                self.editor.republish(arriving);
                self.draw_welcome();
                // Every plugin holding a surface has to paint it again: the editor forwards cells
                // and does not keep them, so a claim is all that could be republished.
                self.broadcast(neosh_proto::PluginEvent::ViewAttached { view: arriving });
                // Certainly changed, and not subject to the once-a-second rule: "is anyone
                // watching" is the one field of the report a viewer arriving is entirely about.
                self.report_live_now();
            }
            // A frontend that *is* this process. There is one view, it is this one, and it has
            // been watched since the workspace started — which has to be said, because a view
            // nobody is attached to is deliberately invisible to `view.list` and a plugin would
            // furnish no panel into it.
            InputEvent::Ready { .. } if self.orphan == Some(self.from.view) => {
                self.orphan = None;
                self.broadcast(neosh_proto::PluginEvent::ViewAttached { view: self.from.view });
                self.draw_welcome();
            }
            // A notch, turned into rows here because only the host knows what is under the
            // pointer and how far a step should be. It scrolls and never moves the cursor: a wheel
            // is not a key, and the whole bug it replaces was arrow keys firing bindings.
            InputEvent::Scroll { rows } => {
                let step = self
                    .editor
                    .options()
                    .int("ui.scroll_rows")
                    .unwrap_or(3)
                    .clamp(1, 20) as i32;
                let by = i32::from(rows) * step;
                // A panel over the transcript is what the wheel is pointing at. Without this the
                // notch went to the chat underneath, so a key sheet or a diff too long for the
                // screen scrolled the one thing you could already see and not the one you could
                // not — and the panel was opaque, so nothing on screen moved at all.
                let view = self.view_now();
                if self.editor.focused(view).is_some_and(|w| self.editor.scrollable(w)) {
                    self.scroll_focused(ScrollAmount::Lines { n: by });
                    return true;
                }
                // While reading, the cursor has to stay on screen, so this goes through the same
                // path the reader's own scrolling does rather than moving the window out from
                // under it.
                self.scroll_chat_by(by, false);
            }
            InputEvent::Ready { .. } | InputEvent::Resize { .. } => {
                // A resize changes nothing the core owns; the frontend re-lays-out from the
                // declarative geometry it already has. Redraw so it takes effect.
                let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::WinScrollTo {
                    win: self.v().composer_win,
                    top_line: Some(0),
                });
                // Except the welcome, which is drawn to a width: the word at the top of it has to
                // fit or not be there.
                self.draw_welcome();
            }
            InputEvent::Command { name, args } => {
                // Not yet registered, and something is still loading: held rather than refused.
                // See `Startup::commands`.
                if self.startup_in_flight() && !self.editor.has_command(&name) {
                    self.startup.commands.push((name, args));
                    return true;
                }
                // Through the core's registry, so an unknown name reports "no such command" in the
                // UI exactly as a bad keybinding does.
                self.editor.exec_command(&name, args);
                self.drain_effects();
            }
            // The last terminal on a view has gone. Its screen goes with it; the conversation it
            // was reading, and any turn running in that conversation, do not.
            InputEvent::Detached => self.close_view(self.from.view),
            // Answered in the run loop, which is the only place with a frontend to draw to.
            InputEvent::Repaint => {}
            // Recorded against the view it came from, not folded into one workspace-wide flag: a
            // workspace is away only when *every* terminal open on it is, and that is a question
            // about the set of them. Handled in the run loop, which is where views live.
            InputEvent::Focus { .. } => {}
            InputEvent::Disconnected => return false,
        }
        true
    }

    /// Write down what a conversation is still running, and say whether that is news.
    ///
    /// A **level**: the whole set replaces the whole set, and empty means nothing is running rather
    /// than nothing was said. On the conversation and not the round, which is the entire point —
    /// the round is thrown away when the turn ends, and a shell that was put in the background is
    /// still running after that. See [`neosh_proto::Activity::Background`].
    fn take_background(
        &mut self,
        session: &neosh_proto::SessionId,
        tasks: Vec<neosh_proto::BackgroundTask>,
    ) -> bool {
        // `if let` and not `let ... else`: the temporary `sessions()` hands back does not outlive a
        // `let`-else, and the borrow it yields is what is being written through.
        if let Some(s) = self.agent.sessions().get_mut(session) {
            let changed = s.background != tasks;
            s.background = tasks;
            changed
        } else {
            false
        }
    }

    /// What is running in a conversation that has no turn in it.
    ///
    /// Between turns there is no stream for this to arrive on, and between turns is exactly when it
    /// changes: a detached shell finishes long after the turn that started it was read to its end.
    /// See [`neosh_provider::drivers::Unasked::background`].
    ///
    /// Declined while a turn is running, and that is not belt-and-braces. The driver only speaks
    /// here when nothing is reading, but a turn can attach in the gap between it deciding that and
    /// this being handled — and then the level queued a moment ago would land on top of the one the
    /// turn has already forwarded. A running turn is reading that pipe and reports the set itself,
    /// in order, so there is nothing to lose by leaving it to.
    fn heard_background(
        &mut self,
        session: neosh_proto::SessionId,
        tasks: Vec<neosh_proto::BackgroundTask>,
    ) {
        if self.turns.contains_key(&session) {
            return;
        }
        if !self.take_background(&session, tasks.clone()) {
            return;
        }
        // The same event a turn would have carried, to the same listeners. A plugin drawing a row
        // about this has no way to tell — and no reason to care — whether a turn happened to be
        // running when the set moved.
        self.broadcast(PluginEvent::Activity {
            session: session.clone(),
            turn: neosh_proto::TurnId::default(),
            activity: neosh_proto::Activity::Background { tasks },
        });
        // Every terminal looking at this conversation, not the one that happens to be first: a
        // conversation can be on screen in several at once, and a footer redrawn in one of them is
        // the other ones still saying it is working on something.
        self.each_view_showing(&session, |me| {
            me.redraw_footer();
        });
    }

    /// Open a turn to hold something the agent has already started saying.
    ///
    /// A backgrounded command finishing is a message `claude` enqueues to itself and answers, and
    /// the answer is this conversation's. The drain stopped that turn being read as the reply to
    /// whatever you typed next, by dropping it; this is where it gets somewhere to go instead. The
    /// agent said it would report back when the build landed, and it did — silence on the screen
    /// until you happen to ask is indistinguishable from a hang.
    ///
    /// The turn says nothing: [`AgentDriver::listen_only`] is what tells the driver that, and it is
    /// set here rather than carried on the wire because it is true of exactly the turn about to
    /// start and of nothing else.
    ///
    /// A turn already running is *deferred*, never dropped. See [`Self::unheard`]: the report is
    /// raised by the driver letting go of the pipe, which happens a moment before the ending that
    /// frees this conversation gets here, so "a turn is running" is the ordinary case rather than
    /// the exception — and a report dropped here is the only one that was ever going to be made.
    fn hear_out(&mut self, session: neosh_proto::SessionId) {
        // Gone since the driver said so — closed, or a workspace shutting down. Asked first, so a
        // conversation that is not there does not leave a note nothing will ever spend.
        if !self.agent.sessions().list().iter().any(|s| s.id == session) {
            self.unheard.remove(&session);
            return;
        }
        // A turn is already reading that pipe. Opening one here would put an empty message into a
        // conversation that is mid-answer, so this waits for the ending that is in the way.
        if self.turns.contains_key(&session) {
            self.unheard.insert(session);
            return;
        }
        self.unheard.remove(&session);
        for d in &self.agent_drivers {
            d.listen_only(&session);
        }
        self.start_turn_in(session.clone(), neosh_agent::Prompt::default());
        // Said for what it is. Every other turn on this line was started by something you did, and
        // a spinner wearing "Pondering" over a report the agent volunteered is the workspace
        // claiming a question you never asked — which, arriving in the second between reading an
        // answer and typing the next thing, reads as your own message having started it.
        if let Some(r) = self.rounds.get_mut(&session) {
            r.verb = REPORTING;
        }
        self.each_view_showing(&session, |me| me.draw_working());
    }

    /// Emit one coalesced frame.
    async fn flush(&mut self) -> anyhow::Result<()> {
        // Anything worth drawing is something that might have changed what this workspace is
        // holding — a turn started, a conversation opened, an answer landed.
        self.report_live();
        let mut frame = self.editor.drain_ui();
        if frame.is_empty() {
            return Ok(());
        }
        self.say_what_moved(&frame);
        // The flush is nobody's in particular: it is the end of the frame, and every terminal that
        // got something out of it has to be told to draw.
        frame.push((None, UiEvent::Flush));
        self.frontend.send(frame).await
    }

    /// Tell plugins what the frontend is about to be told, in the words a plugin uses.
    ///
    /// Neovim's autocmds — `WinEnter`, `WinLeave`, `CursorMoved`, `ModeChanged` — as ordinary
    /// events on the ordinary bus, `from: "neosh"`, read off the same batch the screen is drawn
    /// from so the two can never disagree about what happened. A plugin decorating somebody else's
    /// panel needs exactly these: the sidebar's cursor is the sidebar's to publish, but which
    /// window has the keyboard and where its cursor is are facts about the workspace.
    fn say_what_moved(&mut self, frame: &[(Option<neosh_proto::ViewId>, UiEvent)]) {
        let mut said: Vec<(&'static str, serde_json::Value)> = Vec::new();
        for (tag, ev) in frame {
            // Which terminal an event is about. Focus and mode are per view now — two terminals
            // are two places — so the bus says which place moved, and a listener that cares about
            // "the one I am drawn in" filters like it filters on a kind.
            let view = tag.unwrap_or(neosh_proto::ViewId::LOCAL);
            match ev {
                UiEvent::FocusChanged { win } => {
                    let last = self.last_focus.get(&view).copied().flatten();
                    if *win == last {
                        continue;
                    }
                    if let Some(from) = last {
                        said.push(("neosh.win.leave", self.describe_window(from)));
                    }
                    if let Some(to) = *win {
                        said.push(("neosh.win.enter", self.describe_window(to)));
                    }
                    self.last_focus.insert(view, *win);
                }
                UiEvent::WindowOpened { win, .. } => {
                    said.push(("neosh.win.open", self.describe_window(*win)));
                }
                UiEvent::WindowClosed { win } => {
                    said.push(("neosh.win.close", serde_json::json!({ "win": win })));
                    self.last_focus.retain(|_, w| *w != Some(*win));
                    // Its vars go with it, and whoever was watching them is told.
                    let scope = neosh_proto::VarScope::Window { win: *win };
                    for key in self.vars.forget_window(*win) {
                        self.broadcast(PluginEvent::VarChanged {
                            scope: scope.clone(),
                            key,
                            value: None,
                        });
                    }
                }
                UiEvent::CursorMoved { win, row, col } => {
                    said.push((
                        "neosh.cursor",
                        serde_json::json!({ "win": win, "row": row, "col": col }),
                    ));
                }
                _ => {}
            }
        }
        let views: Vec<neosh_proto::ViewId> = self.views.keys().copied().collect();
        for view in views {
            let mode = self.editor.mode(view);
            if self.last_mode.get(&view) != Some(&mode) {
                self.last_mode.insert(view, mode);
                said.push(("neosh.mode", serde_json::json!({ "mode": mode, "view": view })));
            }
        }
        self.last_mode.retain(|v, _| self.views.contains_key(v));
        for (name, data) in said {
            self.wake_on_event(name);
            self.broadcast(PluginEvent::Event {
                name: name.to_string(),
                data: Some(data),
                from: BUILTIN.to_string(),
            });
        }
    }

    // ---- lazy plugins ------------------------------------------------------

    /// Keep a plugin back until one of its triggers fires, registering its commands on its behalf
    /// so a key bound to one of them works from the first press.
    fn hold_plugin(&mut self, id: PluginId, lazy: Lazy) {
        let owner = PluginId::from(LAZY);
        for name in &lazy.manifest.activation.on_command {
            let desc = Some(format!("{} (loads the plugin)", lazy.manifest.description.clone().unwrap_or_default()).trim().to_string());
            if let Err(e) = self.editor.apply(&owner, ApiCall::CmdRegister { name: name.clone(), desc }) {
                tracing::warn!(plugin = %id, command = name, "{e}");
            }
        }
        tracing::info!(plugin = %id, "held until needed");
        if let Some(info) = self.plugin_infos.get_mut(&id.0) {
            info.state = "held".into();
        }
        self.lazy.insert(id, lazy);
    }

    /// Load a held plugin now. Anything it `requires` that is also held is woken first.
    fn wake(&mut self, id: &PluginId) {
        let Some(lazy) = self.lazy.remove(id) else { return };
        let owner = PluginId::from(LAZY);
        for name in &lazy.manifest.activation.on_command {
            // Its own registration is about to arrive, and a name the stub still held would
            // refuse it.
            let _ = self.editor.apply(&owner, ApiCall::CmdUnregister { name: name.clone() });
        }
        for dep in &lazy.manifest.requires {
            let dep = PluginId(dep.clone());
            if self.lazy.contains_key(&dep) {
                self.wake(&dep);
            }
        }
        if lazy.bundled {
            self.editor.mark_bundled(id);
        }
        self.plugin_permissions.insert(id.clone(), lazy.manifest.permissions.clone());
        self.plugin_provides.insert(id.clone(), lazy.manifest.provides.clone());
        tracing::info!(plugin = %id, "waking");
        if let Some(info) = self.plugin_infos.get_mut(&id.0) {
            info.state = "loaded".into();
        }
        self.load_url(id, &lazy.url, lazy.manifest.requires.clone());
    }

    fn wake_on_command(&mut self, name: &str, args: Vec<String>) {
        let who: Option<PluginId> = self
            .lazy
            .iter()
            .find(|(_, l)| l.manifest.activation.on_command.iter().any(|c| c == name))
            .map(|(id, _)| id.clone());
        let Some(id) = who else { return };
        self.waking.entry(id.clone()).or_default().push((name.to_string(), args));
        self.wake(&id);
    }

    fn wake_on_event(&mut self, event: &str) {
        let who: Vec<PluginId> = self
            .lazy
            .iter()
            .filter(|(_, l)| l.manifest.activation.on_event.iter().any(|e| e == event))
            .map(|(id, _)| id.clone())
            .collect();
        for id in who {
            self.wake(&id);
        }
    }

    fn wake_on_kind(&mut self, kind: &str) {
        let who: Vec<PluginId> = self
            .lazy
            .iter()
            .filter(|(_, l)| l.manifest.activation.on_kind.iter().any(|k| k == kind))
            .map(|(id, _)| id.clone())
            .collect();
        for id in who {
            self.wake(&id);
        }
    }

    /// A woken plugin is up: it missed `neosh.ready`, so it is told now, and the press that
    /// woke it is replayed.
    fn woke(&mut self, id: &PluginId) {
        if self.startup_in_flight() {
            return;
        }
        self.bridge.notify(id, PluginEvent::Event {
            name: READY_EVENT.to_string(),
            data: None,
            from: BUILTIN.to_string(),
        });
        for (name, args) in self.waking.remove(id).unwrap_or_default() {
            self.editor.exec_command(&name, args);
            self.drain_effects();
        }
    }

    /// A window as an event describes it: its id, its buffer, and the buffer's kind — the field
    /// a listener filters on, so a plugin interested in the sidebar ignores everything else.
    fn describe_window(&self, win: WindowId) -> serde_json::Value {
        let w = self.editor.window(win);
        let buf = w.map(|w| w.buf);
        let kind = buf.and_then(|b| self.editor.buffer(b)).and_then(|b| b.kind.clone());
        serde_json::json!({ "win": win, "buf": buf, "kind": kind })
    }

    pub async fn run(
        mut self,
        mut script_rx: mpsc::UnboundedReceiver<ScriptOutbound>,
        mut agent_rx: mpsc::UnboundedReceiver<AgentEvent>,
        mut input_rx: mpsc::UnboundedReceiver<(crate::clients::Source, InputEvent)>,
    ) -> anyhow::Result<()> {
        // Send the initial state before anything else, so the frontend has a screen immediately.
        self.flush().await?;

        // A single long sleep, reset on demand. Creating a new one per iteration would restart the
        // window on every mutation and a fast stream would never reach its deadline.
        let idle = tokio::time::sleep(Duration::from_secs(86_400));
        tokio::pin!(idle);
        let mut armed = false;

        // The other deadline: a half-typed binding that never gets its second key. Separate from
        // the redraw window because the two answer different questions and share no timing.
        let keys = tokio::time::sleep(Duration::from_secs(86_400));
        tokio::pin!(keys);
        // The shorter of the two chord timers. `keys` gives up on a half-typed sequence; this one
        // *explains* it, and fires well before it so that hesitating over the prefix teaches you
        // what follows rather than silently timing out. Two timers rather than one with two
        // deadlines, because they mean opposite things: one is "you have stopped typing", and this
        // is "you have paused, so here is the list".
        let hint = tokio::time::sleep(Duration::from_secs(86_400));
        tokio::pin!(hint);
        let mut hinting = false;
        // How often a shell's output reaches the screen. Fast enough that typing feels immediate,
        // slow enough that a build log scrolling past is a few dozen frames a second rather than
        // one per write. Armed only while a terminal exists, so a workspace with none does no work.
        let shells = tokio::time::sleep(Duration::from_millis(16));
        tokio::pin!(shells);
        let mut waiting = false;

        // The elapsed clock on the working line. A third deadline rather than a share of the redraw
        // window, because it answers a different question: the redraw window asks "how long may a
        // burst of edits be batched", this asks "how often does a number that nobody edited change".
        // Armed only while a turn is in flight, so an idle workspace has no timer at all.
        let clock = tokio::time::sleep(Duration::from_secs(86_400));
        tokio::pin!(clock);
        let mut ticking = false;

        // Taken out of the host so the loop can own it. `None` when `[swarm]` names nobody, in
        // which case `recv` on it is never ready and this costs a branch that is never taken.
        let mut swarm_rx = self.swarm_events.take();

        // What a plan has left. A fourth deadline rather than a share of any of the others, for the
        // same reason they are all separate: it answers a different question, on a scale of minutes
        // rather than frames, and the answer is a network round trip. Armed immediately so the
        // first screen has real numbers on it — a gauge that only appears after five minutes of
        // sitting there is one nobody knows exists.
        let mut quota_rx = self.quota_rx.take();
        let mut unasked_rx = self.unasked_rx.take();
        let mut relay_rx = self.relay_rx.take();
        let mut image_rx = self.image_rx.take();
        // `None` unless something is serving, in which case a `recv` on it is never ready — the
        // same shape as the swarm channel above, and for the same reason.
        let mut control_rx = self.control_rx.take();
        let plan = tokio::time::sleep(crate::quota::AFTER_TURN);
        tokio::pin!(plan);
        // How many times in a row a poll has come back with nothing. Doubles the wait each time.
        let mut quota_backoff: u32 = 0;

        // When the gathered notifications go out. A day away when nothing is waiting, which is
        // almost always: the same shape as `clock` above, and for the same reason — a timer that
        // is never ready costs a branch, and one that fires every second costs a wake-up.
        let burst = tokio::time::sleep(Duration::from_secs(86_400));
        tokio::pin!(burst);
        let mut bursting = false;

        // What the other machines are told this one has. A day away when there is no swarm, which
        // is the shape every optional timer above has.
        //
        // A *timer* rather than a call at each of the places a conversation can change, because
        // that list is long and was already wrong: the inventory was published when a peer
        // connected and when a peer drove something here, and never once because somebody sitting
        // at this machine started, renamed, archived or deleted a conversation. Every other
        // computer's picture of this one was therefore a snapshot from whenever it last dialled —
        // a project added an hour ago was invisible over there, with nothing to say why. Nothing
        // goes on the wire unless the answer actually changed, so an idle workspace with peers is
        // one comparison every two seconds and no traffic at all.
        let roster = tokio::time::sleep(if self.swarm_node.is_some() {
            ROSTER_INTERVAL
        } else {
            Duration::from_secs(86_400)
        });
        tokio::pin!(roster);

        loop {
            tokio::select! {
                Some(out) = script_rx.recv() => self.handle_script_out(out).await,
                Some(ev) = async {
                    match swarm_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        // Never ready, rather than ready-with-nothing: the latter would spin.
                        None => std::future::pending().await,
                    }
                } => self.on_swarm_event(ev),
                Some(ev) = agent_rx.recv() => self.handle_agent_event(ev),
                Some(c) = async {
                    match control_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => self.on_control(c).await,
                Some((from, input)) = input_rx.recv() => {
                    // A repaint changes nothing, so it must not go through the input path: the
                    // frontend is asking to draw the same state again while something on it moves.
                    // Answering with an empty batch is what makes an animation unable to
                    // desynchronise from the text it is drawn over.
                    if matches!(input, InputEvent::Repaint) {
                        self.frontend.send(vec![(None, UiEvent::Flush)]).await?;
                        continue;
                    }
                    // Set for the whole of handling, so anything this key reaches — a command, a
                    // keymap, a plugin — can be answered in the terminal it was pressed in.
                    self.from = from;
                    let carry_on = self.handle_input(input);
                    // The fallback for anything that is nobody's key press and names no window.
                    // See `Host::view_for`. Written down *after* handling, because the event that
                    // makes a view exist at all is the one that arrives before it does.
                    if self.views.contains_key(&from.view) {
                        self.last_input = from.view;
                    }
                    self.from = crate::clients::Source::LOCAL;
                    if !carry_on {
                        break;
                    }
                }
                () = &mut idle, if armed => {
                    armed = false;
                    self.flush().await?;
                }
                () = &mut clock, if ticking => {
                    ticking = false;
                    // A clock is nobody's key press, so it has to say which terminal it means —
                    // and it means all of them. Each has its own transcript with its own spinner
                    // row and its own cards, and a tick that only reached whichever view happened
                    // to be in `from` would leave the others frozen mid-turn.
                    for view in self.view_ids() {
                        self.in_view(view, |me| {
                            me.draw_working();
                            me.tick_cards();
                            me.tick_footer();
                            // The one row in the transcript that is not settled: how long ago the
                            // newest turn finished. Every other margin says a clock time and a
                            // duration, both of which are true forever and are written once.
                            me.redraw_turn_time();
                        });
                    }
                    self.report_live();
                }
                () = &mut keys, if waiting => {
                    waiting = false;
                    self.keys_pending = false;
                    if self.editor.flush_pending(self.keys_view) {
                        self.drain_effects();
                    }
                }
                () = &mut shells, if !self.terminals.is_empty() => {
                    // The cells are queued like any other mutation; the loop's own flush at the
                    // bottom sends them. Nothing special about a terminal's frame.
                    self.tick_terminals();
                    shells.as_mut().reset(Instant::now() + Duration::from_millis(16));
                }
                () = &mut hint, if hinting => {
                    hinting = false;
                    // Asked again rather than assumed: the chord may have been completed, given up
                    // on, or replaced by a different one in the time this was armed for.
                    if self.holding_window_prefix() {
                        self.show_window_keys();
                    }
                }
                Some(unheard) = async {
                    match unasked_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        // Never ready, rather than ready-with-nothing: the latter would spin.
                        None => std::future::pending().await,
                    }
                } => match unheard {
                    Unheard::Began(session) => self.hear_out(session),
                    Unheard::Background(session, tasks) => self.heard_background(session, tasks),
                },
                Some(relay) = async {
                    match relay_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => self.on_relay(relay),
                Some(polled) = async {
                    match quota_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        // Never ready, rather than ready-with-nothing: the latter would spin.
                        None => std::future::pending().await,
                    }
                } => {
                    match polled.quota {
                        Some(quota) => {
                            quota_backoff = 0;
                            self.take_quota(
                                polled.instance,
                                neosh_proto::QuotaSource::Poll,
                                quota,
                            );
                        }
                        // Asked and not answered. Slow down, and slow down hard when the vendor was
                        // explicit about it: the endpoint that reports rate limits has one of its
                        // own, and a poller that keeps its cadence through a 429 is a gauge added
                        // to avoid hitting limits that is itself hitting one.
                        None => {
                            quota_backoff = if polled.throttled {
                                (quota_backoff + 1).max(3)
                            } else {
                                quota_backoff + 1
                            };
                            let wait = crate::quota::IDLE_INTERVAL
                                .saturating_mul(1u32 << quota_backoff.min(6))
                                .min(crate::quota::MAX_BACKOFF);
                            plan.as_mut().reset(Instant::now() + wait);
                        }
                    }
                }
                Some(fetched) = async {
                    match image_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        // Never ready, rather than ready-with-nothing: the latter would spin.
                        None => std::future::pending().await,
                    }
                } => {
                    self.finish_paste(
                        fetched.session,
                        fetched.waiting,
                        fetched.result,
                        true,
                    );
                }
                // The gathered burst is due. Not a per-alert timer: three conversations finishing
                // while you are at lunch is one notification that says three, and the only way to
                // know it was three is to wait a moment before speaking.
                () = &mut burst, if bursting => {
                    bursting = false;
                    self.deliver_alerts().await?;
                }
                () = &mut roster, if self.swarm_node.is_some() => {
                    self.publish_inventory_if_changed();
                    self.prune_mirrors();
                    roster.as_mut().reset(Instant::now() + ROSTER_INTERVAL);
                }
                () = &mut plan => {
                    self.refresh_quota(None);
                    // Reset unconditionally, and to the idle interval whichever branch armed it.
                    // A turn ending shortens the *next* wait only; leaving it short afterwards
                    // would poll every five seconds forever after the first turn.
                    self.quota_due = false;
                    plan.as_mut().reset(Instant::now() + crate::quota::IDLE_INTERVAL);
                }
                else => break,
            }

            if self.quitting {
                break;
            }
            // The viewer asked to leave. The workspace does not: this is the whole difference
            // between closing a window and stopping the work in it.
            if let Some(client) = self.detaching.take() {
                self.persist_sessions();
                self.flush().await?;
                self.frontend.detach(client).await?;
                self.report_live_now();
            }
            if std::mem::take(&mut self.keys_touched) {
                // The window prefix does not expire, and every other half-typed sequence does.
                //
                // `timeoutlen` is what keeps a prefix from making a printable character untypeable:
                // with `gd` bound, `g` has to appear in the field eventually or the composer eats
                // it with no way to tell why. Nothing is waiting to be typed behind `<C-w>` — it is
                // a chord, not a character — so the deadline protects nothing here and costs the
                // one thing that matters: the panel appears at 300 ms and the chord died at 500,
                // so reading the list it had just drawn was enough to lose the chord it described,
                // and the verb you then pressed went into the message field. A panel that says
                // "pick one" is a promise to wait for you to pick one.
                //
                // Nothing hangs. A second key either completes the chord or is unmatched, and
                // unmatched replays the whole held sequence as ordinary input on the press itself.
                let prefix = self.holding_window_prefix();
                waiting = self.keys_pending && !prefix;
                if waiting {
                    let ms = self.editor.options().int("timeoutlen").unwrap_or(500).max(0) as u64;
                    keys.as_mut().reset(Instant::now() + Duration::from_millis(ms));
                }
                // A delay rather than drawing at once, so somebody typing `<C-w>v` fluently never
                // sees a flash — and somebody who pauses gets told. Zero means "at once", for
                // anyone who would rather have it that way.
                hinting = self.keys_pending
                    && prefix
                    && self.window_hint.is_none();
                if hinting {
                    let ms = self
                        .editor
                        .options()
                        .int("ui.keys.hint_delay")
                        .unwrap_or(300)
                        .max(0) as u64;
                    hint.as_mut().reset(Instant::now() + Duration::from_millis(ms));
                }
            }
            if !ticking {
                // Two speeds, because they answer two questions. A turn in flight has a clock on
                // its working line that somebody is watching, so it moves every second. A turn
                // that has finished has *how long ago* in its margin, which nobody is watching and
                // which is wrong by a minute at worst — and a workspace nobody is using should not
                // be waking up once a second to say so. With neither, there is no timer at all.
                let (mut fast, mut slow) = (false, false);
                for view in self.view_ids() {
                    self.in_view(view, |me| {
                        fast |= me.v().working;
                        slow |= me.ageing();
                    });
                }
                let every = match (fast, slow) {
                    (true, _) => Some(Duration::from_secs(1)),
                    (false, true) => Some(Duration::from_secs(30)),
                    (false, false) => None,
                };
                if let Some(every) = every {
                    clock.as_mut().reset(Instant::now() + every);
                    ticking = true;
                }
            }
            // A turn is the only thing that moves these numbers by an amount worth redrawing for,
            // so the poll that matters is the one just after one ends.
            if std::mem::take(&mut self.quota_due) {
                plan.as_mut().reset(Instant::now() + crate::quota::AFTER_TURN);
            }
            if self.editor.has_pending_ui() && !armed {
                idle.as_mut().reset(Instant::now() + COALESCE);
                armed = true;
            }
            if let Some(wait) = self.notifier.due_in()
                && !bursting
            {
                burst.as_mut().reset(Instant::now() + wait);
                bursting = true;
            }
        }

        // Plugins first: `deactivate` runs while the host is still here to answer a last call —
        // flushing state, saying goodbye to a server — and nothing can wedge it, because the
        // runtime is dropped a moment later whether or not anybody has finished.
        self.broadcast(PluginEvent::Shutdown);
        for plugin in self.bridge.loaded() {
            let _ = self.bridge.script().send(ScriptInbound::Unload { plugin });
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        // Last thing before the screen goes: whatever was said this session is on disk.
        self.persist_sessions();
        // Anything gathered and not yet spoken. Waiting out the burst window here means never —
        // and a workspace stopping with a question unanswered in it is exactly the case somebody
        // needs telling about.
        self.deliver_alerts_now().await?;
        self.flush().await?;
        self.frontend.send(vec![(None, UiEvent::Shutdown { restarting: self.restarting })]).await?;
        self.frontend.shutdown().await?;
        Ok(())
    }

    // ---- startup --------------------------------------------------------

    /// Kick off startup: declare built-in options, apply config, then run the config scripts.
    ///
    /// Plugin discovery deliberately waits until the config scripts have activated, because
    /// `rtp.add` is how a config says what else should load.
    /// Find the repository we are running in, once.
    ///
    /// Called before startup so the status line and any plugin that asks on activation get a real
    /// answer rather than "no repository" followed by a correction. Running outside a repository is
    /// an ordinary state, not a failure.
    /// Restore saved conversations, and remember where to write them.
    ///
    /// Called before startup so the first thing on screen is the conversation you were in, not an
    /// empty one that gets replaced a moment later.
    pub async fn restore_sessions(&mut self, state_dir: Option<std::path::PathBuf>) {
        let Some(dir) = state_dir else { return };
        self.state_dir = Some(dir.clone());
        self.pstate = crate::pstate::PluginState::new(Some(dir.clone()));
        self.vars = crate::vars::Vars::new(Some(dir.clone()));
        // The sampled percentages, which are the only record anywhere of what a plan looked like an
        // hour ago. Loaded here rather than at construction because this is where the workspace
        // finds out it has a state directory at all — under `--clean` it never does, and the
        // history is then in memory for as long as the process lives and nowhere afterwards.
        self.quota = crate::quota::QuotaStore::new(Some(&dir));

        let (saved, active) = crate::sessions::load(&dir);
        if saved.is_empty() {
            return;
        }
        let restored = saved.len();
        // The session the store was constructed with is empty and unsaved; replacing the ordering
        // below drops it from the list, but it must not linger as a phantom "New conversation"
        // beside the ones the user actually had.
        let placeholder = self.active_session();
        let landed = {
            let mut store = self.agent.sessions();
            let order: Vec<_> = saved.iter().map(|s| s.id.clone()).collect();
            for s in saved {
                store.insert(s);
            }
            // Never an archived one: with every conversation put away, the empty placeholder is
            // the right place to land, and the list is empty because that is what you asked for.
            let target = active.or_else(|| {
                order.iter().find(|id| store.get(id).is_some_and(|s| !s.archived)).cloned()
            });
            let landed = target.filter(|id| store.enter(id, None).is_ok());
            match &landed {
                // Nothing to land in, so what you land in is not a conversation either: the
                // session the store was constructed with becomes the placeholder, which is what
                // makes "everything is archived" look like an empty workspace rather than like a
                // workspace with one conversation in it that you never started.
                None => {
                    if let Some(s) = store.get_mut(&placeholder) {
                        s.ephemeral = true;
                    }
                }
                Some(_) => {}
            }
            store.set_order(order);
            landed
        };
        if let Some(id) = landed {
            self.vm().session = id.clone();
            // The tab is filed with it. This is an arrival that does not come through `arrive_in` —
            // the pane is simply told which conversation it holds — and a first tab left on the
            // placeholder's group is a bar that the first `^T` strands: the tab moves to where you
            // went, and a shell opened before that stays behind on a group nothing will ever show.
            self.file_tab(id);
            let _ = self.agent.sessions().remove(&placeholder);
        }
        // Every restored conversation may live in a different checkout — that is what makes a
        // second project, or a worktree, appear in the sidebar.
        let dirs: Vec<_> = {
            let store = self.agent.sessions();
            store.iter().map(|s| s.cwd.clone()).collect()
        };
        for dir in dirs {
            self.remember_repo(dir).await;
        }
        // And the one that is active decides where "here" is, which is not necessarily where neosh
        // was started: reopening the last conversation you were in should put you back in *its*
        // project, not in whichever directory the shell happened to be sitting in.
        let here = self.session_cwd(&self.active_session());
        if here.is_dir() {
            self.work_in(here).await;
        }
        self.enter_session();
        tracing::info!("restored {restored} conversation(s)");
    }

    /// Persist every conversation. Cheap enough to call after each turn; a no-op under `--clean`.
    /// Where the last-known command list for a conversation's driver and project is filed.
    ///
    /// By instance and directory rather than by conversation, because that is what the answer
    /// actually depends on: `claude` in one repository accepts that repository's project commands
    /// and its MCP prompts, and every conversation in it gets the same list. Keyed by conversation
    /// it would be learned again from scratch for each new one, which is precisely the case this
    /// exists for.
    fn commands_key(&self, session: &neosh_proto::SessionId) -> Option<String> {
        let store = self.agent.sessions();
        let s = store.get(session)?;
        let instance = s
            .selection
            .as_ref()
            .map(|sel| sel.instance.clone())
            .or_else(|| self.agent.selection().map(|sel| sel.instance))?;
        Some(format!("commands:{instance}:{}", s.cwd.display()))
    }

    /// Keep what a driver said it accepts, for the conversations that have not asked it yet.
    fn remember_commands(
        &mut self,
        session: &neosh_proto::SessionId,
        commands: &[neosh_proto::DriverCommand],
    ) {
        // An empty list is a driver that answered nothing, not a driver that accepts nothing.
        // Writing it down would replace a good list with a blank one.
        if commands.is_empty() {
            return;
        }
        let Some(key) = self.commands_key(session) else { return };
        let Ok(value) = serde_json::to_value(commands) else { return };
        if self.pstate.get(BUILTIN, &key) == value {
            return;
        }
        self.pstate.set(BUILTIN, key, value);
    }

    /// What this driver accepted last time it ran here, for a conversation that has not run yet.
    fn remembered_commands(&mut self) -> Vec<neosh_proto::DriverCommand> {
        let here = self.active_session();
        let Some(key) = self.commands_key(&here) else { return Vec::new() };
        serde_json::from_value(self.pstate.get(BUILTIN, &key)).unwrap_or_default()
    }

    /// Write one conversation, now.
    ///
    /// The cheap half of [`Self::persist_sessions`], for the places that fire *inside* a turn.
    /// Ordering is not at stake — `order.json` records which conversations exist and which one you
    /// were in, and neither changes because a tool round finished.
    ///
    /// A [placeholder](neosh_agent::Session::ephemeral) is skipped, the same as in `save_all`: no
    /// caller can reach here with one today, because saying anything is what stops a session being
    /// one and a turn cannot start before that — but the rule is about what may be written down,
    /// not about who happens to be calling.
    fn persist_session(&self, id: &neosh_proto::SessionId) {
        let Some(dir) = &self.state_dir else { return };
        let store = self.agent.sessions();
        let Some(s) = store.get(id).filter(|s| !s.hidden()) else { return };
        if let Err(e) = crate::sessions::save(dir, s) {
            tracing::warn!("could not save conversation {}: {e}", id.0);
        }
    }

    fn persist_sessions(&self) {
        let Some(dir) = &self.state_dir else { return };
        if let Err(e) = crate::sessions::save_all(dir, &self.agent.sessions()) {
            // Reported once, not per turn: a full disk should not turn into a message on every
            // keystroke, and losing history is not a reason to stop working.
            tracing::warn!("could not save conversations: {e}");
        }
    }

    pub async fn discover_repo(&mut self, cwd: &std::path::Path) {
        self.work_in(cwd.to_path_buf()).await;
    }

    /// Look a directory up once and remember the answer, including "not a repository".
    ///
    /// The negative is cached too: a plain directory would otherwise cost a `git rev-parse` on
    /// every status refresh forever.
    /// Point everything that means "here" at a directory.
    ///
    /// Called on every switch and on every new conversation, because *here* is a property of the
    /// conversation. Three things follow it and each was pinned to the launch directory before:
    /// the git repository plugins ask about, the root the agent's file tools resolve against, and
    /// the directory the welcome banner names.
    ///
    /// The vendor CLI drivers are the fourth, and they are told per turn rather than held here —
    /// see [`neosh_proto::TurnRequest::cwd`].
    async fn work_in(&mut self, dir: std::path::PathBuf) {
        self.cwd = dir.clone();
        let mut layer = (*self.agent.permissions()).clone();
        layer.set_workspace(dir.clone());
        self.agent.set_permissions(layer);
        self.remember_repo(dir).await;
    }

    async fn remember_repo(&mut self, dir: std::path::PathBuf) {
        if self.repos.contains_key(&dir) {
            return;
        }
        let found = neosh_vcs::Git::discover(&dir).await;
        if let Some(g) = &found {
            tracing::info!(dir = %dir.display(), root = %g.root().display(), "git repository");
            // Asked once, when the directory is first seen, and remembered. A label is read on
            // every redraw of the panel and is not worth a `rev-parse` each time.
            let (branch, main) = g.identity().await;
            self.projects.insert(
                dir.clone(),
                ProjectFacts {
                    name: project_name(&dir, branch.clone(), main.clone()),
                    repo_root: main.map(|p| p.display().to_string()),
                    branch,
                },
            );
            // Asked here for the same reason and at the same time as the name: once per directory,
            // and the answer does not change while a checkout is open.
            if let Some(key) = g.origin_url().await.and_then(|u| neosh_proto::ProjectKey::from_remote(&u)) {
                self.project_keys.insert(dir.clone(), key);
            }
        }
        self.repos.insert(dir, found);
    }

    /// A branch was renamed: put right everything the host had written down about it.
    ///
    /// [`ProjectFacts`] is asked once, when a directory is first seen, and then read on every
    /// redraw of every panel — which is right, because a label is not worth a subprocess per
    /// frame, and wrong for exactly one moment: the one where somebody moves the branch. The
    /// worktree row in the sidebar *is* `SessionInfo::branch`, so without this the panel says
    /// `spry-reef` under a conversation working on `feature/proxy-rate-limiter` until the
    /// workspace is restarted.
    ///
    /// Matched on the cached branch rather than on `cwd`, because one rename can move several
    /// rows: `cwd` names the checkout the git call ran in, and the branch may be checked out in a
    /// worktree that is a different directory from it. Every directory the host knows that was on
    /// `old` is on `new` now.
    ///
    /// The name is rebuilt through [`project_name`] rather than patched, so `neosh · spry-reef`
    /// becomes `neosh · feature/proxy-rate-limiter` by the one function that decides what a
    /// checkout is called — a second place that formats it is a second place to get it wrong.
    async fn rebranded(&mut self, old: &str, cwd: Option<&str>) {
        let touched: Vec<std::path::PathBuf> = self
            .projects
            .iter()
            .filter(|(dir, facts)| {
                facts.branch.as_deref() == Some(old)
                    // The checkout the call named counts even on a detached head or a stale
                    // cache: it is the one directory we know for certain was involved.
                    || cwd.is_some_and(|c| std::path::Path::new(c) == dir.as_path())
            })
            .map(|(dir, _)| dir.clone())
            .collect();
        for dir in touched {
            // Re-asked rather than patched: `identity()` is the same call the facts were built
            // from, and a rename that failed halfway is then simply not reflected.
            // Cloned rather than borrowed: `Git` is a path and a fact, cheap to copy, and this
            // holds it across an await while the map it came from is written to below.
            let Some(g) = self.repos.get(&dir).and_then(Option::as_ref).cloned() else { continue };
            let (branch, main) = g.identity().await;
            let name = project_name(&dir, branch.clone(), main.clone());
            self.projects.insert(
                dir,
                ProjectFacts { name, repo_root: main.map(|p| p.display().to_string()), branch },
            );
        }
        // Every list that draws a conversation is drawing a name that just changed, and the panel
        // redraws on this. Not the same event as switching — nothing moved — but it is the one
        // signal a thread list already listens to for "the conversations are not what you drew".
        self.broadcast(PluginEvent::SessionChanged {
            session: self.active_session(),
            view: self.from.view,
        });
        self.refresh_status();
    }

    /// Which live conversations are in `dir` or under it.
    ///
    /// "Under it" as well as "in it", because a worktree is a directory somebody works *in*: a
    /// conversation started in `<tree>/crates/neosh-tui` is as much a conversation in that tree as
    /// one started at its root, and a move that re-pointed only the exact matches would leave the
    /// nested ones naming a path that no longer exists.
    ///
    /// A path comparison rather than a string one: `starts_with` on a `Path` is by component, so
    /// `/w/tree-2` is not read as being inside `/w/tree`.
    fn sessions_under(&self, dir: &std::path::Path) -> Vec<neosh_proto::SessionId> {
        self.agent
            .sessions()
            .iter()
            .filter(|s| s.cwd.starts_with(dir))
            .map(|s| s.id.clone())
            .collect()
    }

    /// A worktree's directory moved: put right everything the host had keyed by where it was.
    ///
    /// [`Self::rebranded`] is the same shape one field along — a label the host cached and then
    /// drew on every frame — and this is the harder version of it, because a directory is not a
    /// label. It is the key of the facts every list draws, of the conversations that live there, of
    /// the project vars holding somebody's pin, and of the working directory each vendor CLI was
    /// started in. Four maps and a disk file, and a move that updates three of them is a workspace
    /// pointing at a directory that is not there any more.
    ///
    /// In order, and each for its own reason:
    ///
    /// * **The conversations.** `Session::cwd` is what the agent's tools resolve against and what
    ///   every panel groups by, and this is the only thing in the workspace that rewrites one.
    ///   Persisted immediately: a workspace that stopped between the `git worktree move` and the
    ///   next autosave would come back with every conversation in the tree pointing at nothing.
    /// * **The facts and the repository handle.** Re-asked through [`Self::remember_repo`] rather
    ///   than moved across, for the reason `rebranded` re-asks: `identity()` is the call they were
    ///   built from, so a move that half happened is simply not reflected rather than confidently
    ///   wrong.
    /// * **The project vars.** The pin, the fold, the rank and the name the panel remembered. See
    ///   [`crate::vars::Vars::rename_project`]; the listeners are told, or every cache holding an
    ///   arrangement is now wrong and nothing said so.
    /// * **The vendor CLIs.** One process per conversation, each started in a directory that has
    ///   moved out from under it. Closed rather than migrated — a CLI's cwd is fixed at spawn and
    ///   there is no message in any of these protocols for "you are somewhere else now" — so the
    ///   next turn starts one in the right place and resumes by id. Safe precisely because the call
    ///   refused to run while a turn was in flight.
    ///
    /// And `self.cwd` last, when the host was itself standing in the tree: it is what a new
    /// conversation with no directory of its own inherits.
    async fn relocated(&mut self, from: &std::path::Path, to: &std::path::Path) {
        let moved = self.sessions_under(from);
        {
            let mut store = self.agent.sessions();
            for id in &moved {
                let Some(s) = store.get_mut(id) else { continue };
                // The tail is what makes a nested conversation follow its tree: `<from>/a/b`
                // becomes `<to>/a/b`. The tree's own root has an *empty* tail, and `join("")`
                // appends a separator rather than doing nothing — so the root came out as
                // `<to>/`, which is the same directory under a spelling nothing else uses. Every
                // list keyed by the path then held both: two rows for one worktree, and a second
                // entry in `sidebar.projects` that no amount of moving it again would clear.
                let Ok(tail) = s.cwd.strip_prefix(from) else { continue };
                s.cwd = if tail.as_os_str().is_empty() { to.to_path_buf() } else { to.join(tail) };
            }
        }
        self.persist_sessions();

        self.projects.remove(from);
        self.repos.remove(from);
        self.project_keys.remove(from);
        self.remember_repo(to.to_path_buf()).await;
        // Every directory the move carried, not only the tree's root: a conversation in a
        // subdirectory has facts of its own under a key that has just changed.
        for id in &moved {
            let dir = { self.agent.sessions().get(id).map(|s| s.cwd.clone()) };
            let Some(dir) = dir else { continue };
            self.remember_repo(dir).await;
        }

        let old = from.display().to_string();
        let new = to.display().to_string();
        for (key, value) in self.vars.rename_project(&old, &new) {
            self.broadcast(PluginEvent::VarChanged {
                scope: neosh_proto::VarScope::Project { cwd: new.clone() },
                key,
                value: Some(value),
            });
        }

        for id in &moved {
            for d in &self.agent_drivers {
                d.close_conversation(id);
            }
        }

        let here = self
            .cwd
            .strip_prefix(from)
            .map(|tail| if tail.as_os_str().is_empty() { to.to_path_buf() } else { to.join(tail) })
            .ok();
        if let Some(here) = here {
            self.work_in(here).await;
        }

        // Said rather than left to be worked out: a panel keeping a list of directories cannot tell
        // "the thing at `from` is the thing at `to`" from "one project went and another arrived",
        // and the difference is whether it keeps your place in an order you arranged by hand.
        self.broadcast(PluginEvent::ProjectMoved { from: old, to: new });
        // The same signal `rebranded` ends on, for the same reason: every list drawing a
        // conversation is drawing something that just changed.
        self.broadcast(PluginEvent::SessionChanged {
            session: self.active_session(),
            view: self.from.view,
        });
        self.refresh_status();
    }

    /// Stamp the display name onto a conversation's info.
    ///
    /// Done here rather than in the session store because naming a checkout means having looked at
    /// the repository, which is the host's job — the store holds conversations and knows nothing
    /// about git.
    fn named(&self, mut info: neosh_proto::SessionInfo) -> neosh_proto::SessionInfo {
        let cwd = std::path::Path::new(&info.cwd);
        if let Some(facts) = self.projects.get(cwd) {
            info.project = facts.name.clone();
            info.repo_root = facts.repo_root.clone();
            info.branch = facts.branch.clone();
        }
        // Stamped here for the same reason the name is, and it is the same call the swarm's own
        // inventory makes — so what this workspace calls a project and what it tells other machines
        // it calls one cannot disagree.
        info.project_key = self.project_key(cwd).0;
        info
    }

    pub fn begin_startup(&mut self, boot: Bootstrap) {
        self.seed_hints();
        self.draw_welcome();
        self.refresh_status();
        self.refresh_composer();
        self.startup.reload = Some(boot.reload);
        self.startup.config_file = boot.config_file;
        // Set here and not in `apply_config`, which `config.reload` also runs: a `^R` must not
        // re-ask the question this process was started with.
        self.startup.first_prompt = boot.first_prompt;
        self.startup.base_instances =
            self.agent.providers().instances().cloned().collect();
        // Started once, at boot, and never on reload: the allow-list and the listening socket are
        // decided by config, and re-reading them would mean tearing down live connections whenever
        // somebody pressed `^R`. Changing the swarm is a restart, and saying so is kinder than a
        // reload that half-works.
        let swarm = boot.resolved.swarm.clone();
        self.apply_config(boot.resolved, boot.cli_model, boot.cli_plugin_dirs);
        self.start_swarm(&swarm);
    }

    /// Apply one resolved configuration. Shared by startup and reload — if reload took a different
    /// path, the two would drift and "works on a fresh start" would stop meaning anything.
    fn apply_config(
        &mut self,
        resolved: Resolved,
        cli_model: Option<String>,
        cli_plugin_dirs: Vec<std::path::PathBuf>,
    ) {
        for note in &resolved.notes {
            self.editor_message(note.level, note.text.clone());
        }

        // Re-established on every application, not just the first. A config that rebinds `<C-r>`
        // replaces the default binding; without re-registering, the reload that config triggers
        // would leave the key bound to nothing once the old config's keymaps were removed — and
        // the only way to reload is the key that just stopped working.
        self.declare_builtin_options();
        self.register_builtin_commands();

        // Providers and permissions are part of the configuration too. Applying only options and
        // scripts is what made `config.reload` report success while silently ignoring both.
        self.apply_providers(resolved.providers.clone());
        self.apply_permissions(&resolved.permissions);

        // `[agent]` in config.toml is spelled as the options it sets, so there is exactly one path
        // from "a value the user wrote" to "a value that took effect".
        let mut values = Vec::new();
        if let Some(m) = resolved.model {
            values.push(("agent.model".to_string(), OptionValue::Str(m)));
        }
        if let Some(p) = resolved.system_prompt {
            values.push(("agent.system_prompt".to_string(), OptionValue::Str(p)));
        }
        values.extend(resolved.options);
        for (name, value) in values {
            self.set_option_or_defer(name, value);
        }

        self.startup.cli_plugin_dirs = cli_plugin_dirs;
        self.startup.no_builtin_plugins = resolved.no_builtin_plugins;
        self.startup.plugin_dirs = resolved.plugin_dirs;
        self.startup.plugin_dirs.extend(self.startup.cli_plugin_dirs.clone());
        self.startup.cli_model = cli_model;
        self.startup.scripts = resolved.init_scripts.into();
        self.startup.discovered = false;

        self.next_config_script();
    }

    /// neosh's own commands.
    ///
    /// Registered through `ApiCall::CmdRegister` under the plugin id `neosh`, so they show up in
    /// `cmd.list()`, can be rebound with `keymap.set`, and can be invoked by a plugin — exactly
    /// like anyone else's. The default bindings are ordinary bindings; setting the same key in
    /// your config replaces them.
    fn register_builtin_commands(&mut self) {
        let plugin = PluginId::from(BUILTIN);
        let commands = [
            ("config.reload", "Re-read config.toml and init.ts, and reload every plugin"),
            ("quit", "Close this terminal. Whatever is running keeps running"),
            ("stop", "Stop the workspace, and everything running in it"),
            ("interrupt", "Stop what is running, or leave"),
            ("chat.scroll_up", "Scroll the transcript up a screen"),
            ("chat.scroll_down", "Scroll the transcript down a screen"),
            ("chat.scroll_end", "Jump to the end of the transcript"),
            (
                "chat.read",
                "Read the transcript: hjkl move, [ ] turns, { } blocks, / finds, yc takes the code, yp the path",
            ),
            ("chat.toggle_card", "Open or fold the tool card under the cursor, while reading"),
            (
                "chat.queue.edit",
                "Take the last thing you queued back into the composer, to change it or drop it",
            ),
            ("chat.image.paste", "Attach the image on the clipboard"),
            ("chat.image.drop", "Take the last attached image back off"),
            ("chat.image.clear", "Take every attached image off"),
            ("edit.copy", "Copy what is selected"),
            ("edit.cut", "Cut what is selected"),
            ("edit.select_all", "Select everything here"),
            ("edit.newline", "Break the line instead of sending"),
            // Built in rather than left to the model plugin: with `--clean`, or with plugins
            // disabled, this is the only way to authenticate, and "install a plugin first" is a
            // poor answer to "it says I have no API key".
            (
                "provider.key",
                "Enter an API key — for the named instance, or the one this model belongs to",
            ),
            // Windows, panes and tabs. Every one of these is an ordinary named command before it
            // is a key, which is what makes the prefix rebindable, `^K` able to run any of them by
            // name, and a plugin able to build a different window manager out of the same verbs.
            ("window.split.right", "Split this pane, and put the new one on the right"),
            ("window.split.left", "Split this pane, and put the new one on the left"),
            ("window.split.down", "Split this pane, and put the new one below"),
            ("window.split.up", "Split this pane, and put the new one above"),
            ("window.split.chat", "Split to the right, on a new conversation"),
            ("window.close", "Close this pane"),
            ("window.only", "Close every pane but this one"),
            ("window.left", "Go to the pane on the left"),
            ("window.right", "Go to the pane on the right"),
            ("window.up", "Go to the pane above"),
            ("window.down", "Go to the pane below"),
            ("window.next", "Go to the next pane, or the next tab when there is one pane"),
            ("window.wider", "Widen this pane"),
            ("window.narrower", "Narrow this pane"),
            ("window.taller", "Make this pane taller"),
            ("window.shorter", "Make this pane shorter"),
            ("window.equalize", "Give every pane an equal share"),
            ("window.move.left", "Move this pane to the far left"),
            ("window.move.down", "Move this pane to the bottom"),
            ("window.move.up", "Move this pane to the top"),
            ("window.move.right", "Move this pane to the far right"),
            ("window.exchange", "Swap this pane with the next one, keeping their places"),
            ("tab.open", "Open a tab, choosing what goes in it"),
            ("tab.new", "Open a tab showing what you are reading"),
            ("tab.new.chat", "Open a tab on a new conversation in this directory"),
            ("tab.close", "Close this tab and everything in it"),
            // Registered as well as bound. A command that is bound to a key and owned by nobody
            // resolves to "no such command" on every press — which is what `<C-w>1` did, silently
            // enough that the key simply looked dead.
            ("tab.select", "Go to the tab with this number, counting from the left"),
            ("tab.next", "Go to the next tab"),
            ("tab.prev", "Go to the previous tab"),
            ("tab.rename", "Name this tab"),
            ("tab.move.left", "Move this tab left along the bar"),
            ("tab.move.right", "Move this tab right along the bar"),
            ("window.split.term", "Split this pane, and run a shell in the new one"),
            ("tab.new.term", "Open a tab with a shell in it"),
            ("window.keys", "Show what the window prefix can do"),
            // Moving in a panel that does not fit. Named commands before they are keys, like
            // everything else here — so `^K` runs them, `init.ts` moves them, and a plugin whose
            // float sets `scroll` gets all eight without writing any of them.
            ("scroll.down", "Scroll this panel down a line"),
            ("scroll.up", "Scroll this panel up a line"),
            ("scroll.half_down", "Scroll this panel down half a screen"),
            ("scroll.half_up", "Scroll this panel up half a screen"),
            ("scroll.page_down", "Scroll this panel down a screen"),
            ("scroll.page_up", "Scroll this panel up a screen"),
            ("scroll.top", "To the top of this panel"),
            ("scroll.bottom", "To the bottom of this panel"),
        ];
        for (name, desc) in commands {
            let _ = self.editor.apply(&plugin, ApiCall::CmdRegister {
                name: name.to_string(),
                desc: Some(desc.to_string()),
            });
        }
        // Two lists, and the split is the point. **A mode's keys are the mode's keys**: while you
        // are reading the transcript, `^X` cutting a composer you cannot see and `^Y` pulling a
        // queued message into it are not things you asked for — they are the previous mode still
        // holding the keyboard. Everything that acts on the composer or scrolls the transcript
        // *without* the cursor is therefore bound in `Chat` alone, and reading answers those keys
        // itself, in the way this mode means them.
        //
        // What is left is the escape hatches, and only those. A config that fails to load can put
        // you somewhere unexpected, which is exactly when you need to reload, quit, interrupt, or
        // get into the transcript — so those four are bound everywhere and nothing else is.
        for (lhs, command, desc) in [
            ("<C-r>", "config.reload", "Reload configuration"),
            ("<C-q>", "quit", "Close neosh"),
            // Bound because it is the first thing anyone tries. In raw mode the terminal does
            // not turn this into SIGINT, so without a binding it does nothing at all — and a
            // program you cannot get out of is not one people give a second chance.
            ("<C-c>", "interrupt", "Cancel, clear, or quit"),
            // Reading the transcript is a separate place, and this is the door to it.
            ("<C-s>", "chat.read", "Read, select and copy the transcript"),
        ] {
            for mode in [Mode::Normal, Mode::Insert, Mode::Visual, Mode::Chat] {
                let _ = self.editor.apply(&plugin, ApiCall::KeymapSet {
                    mode,
                    lhs: lhs.to_string(),
                    command: command.to_string(),
                    scope: None,
                    desc: Some(desc.to_string()),
                });
            }
        }
        for (lhs, command, desc) in [
            // Scrolling that leaves the cursor behind, which is what you want when the composer
            // has it. Reading binds the same two keys to a screen of *motion*, because there the
            // cursor is the thing you are moving.
            ("<PageUp>", "chat.scroll_up", "Scroll up"),
            ("<PageDown>", "chat.scroll_down", "Scroll down"),
            ("<C-End>", "chat.scroll_end", "Back to the newest message"),
            ("<C-x>", "edit.cut", "Cut the selection"),
            ("<C-a>", "edit.select_all", "Select everything"),
            // `^Y`, because it is readline's: `^U` and `^W` kill, `^Y` brings back what was
            // killed, and those two are already the composer's. Sending while a turn runs is a
            // kill of the same kind — the sentence leaves the field — so the key that undoes it is
            // the one a terminal's fingers already know.
            //
            // It used to be `⇧↑`, which needs an arrow, and a keyboard without arrows could not
            // reach it at all. It also took `⇧↑` away from the composer, where it is how you select
            // upward through a pasted snippet — the field handles it and never saw it.
            ("<C-y>", "chat.queue.edit", "Take the last queued message back"),
            // The key a terminal cannot deliver an image with, bound to the only thing that can.
            // Bracketed paste is a *text* protocol — `⌘V` on a screenshot arrives as nothing at
            // all — so pasting a picture has to be a key that goes and asks the clipboard, and
            // `^V` is the one every hand is already on. It stays free for that: nothing else in
            // chat wants it, and the composer has never had a use for it.
            ("<C-v>", "chat.image.paste", "Attach the image on the clipboard"),
            // Taking one back off is `⌫` on an empty composer, handled below rather than bound
            // here. It was `⌥V`, which is not a key: a Mac terminal turns Option-v into `√`
            // unless its owner has been into the settings, and a keyboard whose Alt is a layer
            // toggle cannot send it at all. `chat.image.drop` is still a command, so `^K` runs it
            // and `init.ts` can put it back on a chord of your own.
        ] {
            let _ = self.editor.apply(&plugin, ApiCall::KeymapSet {
                mode: Mode::Chat,
                lhs: lhs.to_string(),
                command: command.to_string(),
                scope: None,
                desc: Some(desc.to_string()),
            });
        }
        self.bind_scroll_keys();
        self.bind_window_keys();
    }

    /// The reader's motions, on any panel that said it might not fit.
    ///
    /// Bound at `BufKind { name: "neosh.scroll" }`, which is a scope the core pushes into the chain
    /// for a focused float whose [`FloatConfig::scroll`](neosh_proto::FloatConfig::scroll) is set —
    /// so one set of defaults serves the key sheet, the git panels, the usage panel and a third
    /// party's float alike, and none of them has to spell out eight bindings it would then own.
    ///
    /// These are Vim's, and the same ones the transcript reader answers, because a panel of rows you
    /// are moving through is the thing the reader already is. What they are *not* is global: they
    /// resolve below the panel's own kind, so a panel that wants `j` keeps it, and they do not exist
    /// at all while the keyboard is in the composer, where `j` is a letter.
    ///
    /// Every key here is one every terminal sends. The arrows and the paging keys are bound as well
    /// and are never the only way — which is the rule for arrows everywhere in this workspace.
    fn bind_scroll_keys(&mut self) {
        let plugin = PluginId::from(BUILTIN);
        let scope = Some(neosh_proto::KeymapScope::BufKind {
            name: neosh_core::editor::SCROLL_KIND.to_string(),
        });
        for (lhs, command, desc) in [
            ("j", "scroll.down", "Down a line"),
            ("<Down>", "scroll.down", "Down a line"),
            ("k", "scroll.up", "Up a line"),
            ("<Up>", "scroll.up", "Up a line"),
            ("<C-d>", "scroll.half_down", "Down half a screen"),
            ("<C-u>", "scroll.half_up", "Up half a screen"),
            ("<C-f>", "scroll.page_down", "Down a screen"),
            ("<PageDown>", "scroll.page_down", "Down a screen"),
            ("<C-b>", "scroll.page_up", "Up a screen"),
            ("<PageUp>", "scroll.page_up", "Up a screen"),
            ("gg", "scroll.top", "To the top"),
            ("<Home>", "scroll.top", "To the top"),
            ("G", "scroll.bottom", "To the bottom"),
            ("<End>", "scroll.bottom", "To the bottom"),
        ] {
            for mode in [Mode::Normal, Mode::Insert, Mode::Visual, Mode::Chat] {
                let _ = self.editor.apply(&plugin, ApiCall::KeymapSet {
                    mode,
                    lhs: lhs.to_string(),
                    command: command.to_string(),
                    scope: scope.clone(),
                    desc: Some(desc.to_string()),
                });
            }
        }
    }

    /// Move the focused panel's scroll, if it is one that scrolls.
    ///
    /// Nothing happens when the keyboard is not in a scrollable float, which is not a case these
    /// keys can normally reach — the scope they are bound at is only in the chain while one has
    /// focus — but `^K` can run them by name from anywhere, and a command that reports a failure
    /// nobody caused is worse than one that does nothing.
    fn scroll_focused(&mut self, amount: ScrollAmount) {
        let view = self.view_now();
        let Some(win) = self.editor.focused(view) else { return };
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::WinScroll { win, amount });
    }

    /// The window prefix, out of `ui.keys.window_prefix`.
    ///
    /// Read on every use rather than cached, for the reason `ui.modal_escape_keys` is: it is a
    /// setting, `^R` reloads settings, and a cached copy of the key that reaches every other key is
    /// the one that cannot be corrected without restarting.
    fn window_prefix(&self) -> String {
        self.editor
            .options()
            .get("ui.keys.window_prefix")
            .and_then(|v| v.as_str().map(str::to_string))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "<C-w>".to_string())
    }

    /// Bind everything under the window prefix.
    ///
    /// A prefix layer, which the rest of this workspace deliberately does not have — and the
    /// argument against one is on the record: *"inventing a prefix layer for one verb costs every
    /// user a concept to save one of them a keystroke."* This is not one verb. It is twenty-four,
    /// on a keyboard where every Ctrl-letter is already spoken for, and where the alternative is
    /// that most of them ship with no key at all. Vim and tmux both landed here from the same
    /// place.
    ///
    /// What makes it affordable is that it is *self-teaching*: holding the prefix draws the list —
    /// see [`Self::window_hint`] — so nobody has to have been told, which was the real cost of a
    /// prefix and not the keystroke. The prefix itself is on the tab bar, permanently.
    ///
    /// Rebound rather than added to on reload, so moving the prefix in `init.ts` does not leave the
    /// old one working as well: two prefixes, one of which you did not choose, is worse than either.
    fn bind_window_keys(&mut self) {
        let plugin = PluginId::from(BUILTIN);
        let prefix = self.window_prefix();
        for (suffix, command) in WINDOW_KEYS {
            for mode in [Mode::Normal, Mode::Insert, Mode::Visual, Mode::Chat] {
                let _ = self.editor.apply(&plugin, ApiCall::KeymapSet {
                    mode,
                    lhs: format!("{prefix}{suffix}"),
                    command: command.to_string(),
                    scope: None,
                    desc: None,
                });
            }
        }
        // A tab by number. Nine of them, which is as many as the bar can usefully print and as
        // many as a single keypress can name.
        for n in 1..=9u32 {
            for mode in [Mode::Normal, Mode::Insert, Mode::Visual, Mode::Chat] {
                let _ = self.editor.apply(&plugin, ApiCall::KeymapSet {
                    mode,
                    lhs: format!("{prefix}{n}"),
                    command: "tab.select".to_string(),
                    scope: None,
                    desc: None,
                });
            }
        }
    }

    /// Rebuild the instance list from the base plus whatever configuration names now.
    fn apply_providers(&mut self, from_config: Vec<neosh_proto::InstanceConfig>) {
        let mut reg = self.agent.providers_mut();
        reg.clear_instances();
        for i in self.startup.base_instances.clone() {
            reg.add_instance(i);
        }
        for i in from_config {
            reg.add_instance(i);
        }
    }

    /// Rebuild the permission layer.
    ///
    /// The workspace root comes from the layer being replaced rather than being re-derived: it is
    /// fixed for the life of the process, and a reload must not be able to move it.
    fn apply_permissions(&mut self, cfg: &crate::config::PermissionsConfig) {
        let workspace = self.agent.permissions().workspace().to_path_buf();
        let mut layer = neosh_agent::PermissionLayer::new(workspace).with_mode(cfg.mode);
        for c in &cfg.allow_commands {
            layer = layer.allow_command(c);
        }
        for h in &cfg.allow_hosts {
            layer = layer.allow_host(h);
        }
        for r in &cfg.allow_roots {
            layer = layer.allow_root(r);
        }
        self.agent.set_permissions(layer);
        // A mode that only reached the built-in tools would leave an agent driver enforcing the
        // one it was told at startup, under a footer saying otherwise.
        self.sync_agent_drivers();
    }

    /// Everything that has to happen when a plugin stops being loaded, whether it is being
    /// reloaded or its `activate` threw partway through registering things.
    /// Forget the held plugins, for a reload that is about to discover them again.
    fn drop_held_plugins(&mut self) {
        self.lazy.clear();
        self.waking.clear();
        self.editor.remove_plugin(&PluginId::from(LAZY));
    }

    fn tear_down_plugin(&mut self, id: &PluginId) {
        let _ = self.bridge.script().send(ScriptInbound::Unload { plugin: id.clone() });
        self.editor.remove_plugin(id);
        self.agent.remove_plugin(id);
        self.bridge.forget(id);
        // Dropped with the plugin, so a reload that removes `vcs_write` from a manifest actually
        // removes the capability rather than leaving the previous grant behind.
        self.plugin_permissions.remove(id);
        self.plugin_provides.remove(id);
        // A segment outliving its plugin would leave a stale model name in the footer after a
        // reload removed the plugin that put it there.
        let prefix = format!("{id}:");
        self.status_segments.retain(|k, _| !k.starts_with(&prefix));
        // Same for its shortcuts: a row advertising `^P model` after the model plugin was unloaded
        // is a key that does nothing, which is worse than a row with one fewer entry on it.
        self.hints.retain(|k, _| !k.starts_with(&prefix));
        if let Some(drivers) = self.plugin_drivers.remove(id) {
            let mut reg = self.agent.providers_mut();
            for d in drivers {
                reg.remove_driver(&d);
            }
        }
    }

    /// `<C-c>`: stop the most local thing that is happening, and only leave if nothing is.
    ///
    /// The cascade matters more than any individual step. A key that sometimes quits and sometimes
    /// cancels is only tolerable if "quit" is always the *last* resort and always announced first.
    fn interrupt(&mut self) {
        const ARM_WINDOW: Duration = Duration::from_secs(3);

        // With something selected, `^C` copies — the behaviour of every terminal, and unambiguous
        // because the two cases cannot both be true. Without this the key that means "copy"
        // everywhere else in your day means "throw away what you were writing" here.
        if self.copy_selection(false) {
            self.vsm().quit_armed = None;
            return;
        }
        if self.v().reading {
            self.leave_reading();
            self.vsm().quit_armed = None;
            return;
        }
        if let Some(stopped) = self.cancel_turn_here() {
            self.vsm().quit_armed = None;
            if stopped {
                self.editor_message(MessageLevel::Info, "interrupted");
            }
            return;
        }
        if !self.composer_text().is_empty() {
            self.set_composer("");
            self.vsm().quit_armed = None;
            return;
        }
        let armed = self.vs().quit_armed.is_some_and(|at| at.elapsed() < ARM_WINDOW);
        if armed {
            self.leave();
            return;
        }
        self.vsm().quit_armed = Some(std::time::Instant::now());
        self.editor_message(MessageLevel::Info, match self.on_quit {
            OnQuit::Stop => "press ^C again to quit, or ^Q",
            OnQuit::Detach => "press ^C again to close this terminal, or ^Q",
        });
    }

    /// What `quit` does here — from the key, from the command, from the last rung of `^C`.
    ///
    /// One door, because there is only one door. A workspace is a process with turns running in it
    /// and possibly other terminals looking at it, so the key that ends *this* terminal must not be
    /// able to end that. `^C^C` set `quitting` directly and took the workspace with it: every
    /// conversation, in every project, on a key whose first four meanings — copy, leave reading,
    /// interrupt, clear the draft — are all local. The rule that `^Q` must never stop a
    /// workspace was never really about `^Q`; it is about every way out there is.
    fn leave(&mut self) {
        match self.on_quit {
            OnQuit::Stop => self.quitting = true,
            OnQuit::Detach => self.detaching = Some(self.from.client),
        }
    }

    /// Page the transcript. `0` returns to following the tail.
    ///
    /// Following the newest is `None` rather than a row, which is why jumping to the end is a reset
    /// rather than a seek: the renderer already shows the last screenful when nothing has scrolled.
    /// Row zero is `Some(0)` and means the top — paging up to it used to ask for "follow" and land
    /// back at the end, one page short of the beginning every time.
    /// Scroll the transcript a screen at a time. `0` goes back to following the newest.
    fn scroll_chat(&mut self, direction: i32) {
        let page = self.chat_page();
        match direction {
            0 => self.scroll_chat_by(0, true),
            d if d < 0 => self.scroll_chat_by(-(page as i32), false),
            _ => self.scroll_chat_by(page as i32, false),
        }
    }

    /// How many buffer rows fit in the transcript window right now.
    fn chat_page(&self) -> u32 {
        self.editor
            .window(self.v().chat_win)
            .and_then(|w| w.viewport)
            .map(|v| v.height as u32)
            .unwrap_or(10)
            .max(1)
    }

    /// Scroll by `rows`, or back to following when `to_end`.
    ///
    /// Rows rather than screens because a wheel notch is not a page: the terminal sends one event
    /// per notch and a page each was the whole of "my eyes cannot follow it". The end test still
    /// uses the page — scrolling *down* past the last screenful means following again, whatever
    /// the step was that got there.
    fn scroll_chat_by(&mut self, rows: i32, to_end: bool) {
        let plugin = PluginId::from(BUILTIN);
        let lines = self.editor.buffer(self.v().chat).map(|b| b.line_count() as u32).unwrap_or(0);
        let win = self.v().chat_win;
        let page = self
            .editor
            .window(win)
            .and_then(|w| w.viewport)
            .map(|v| v.height as u32)
            .unwrap_or(10)
            .max(1);
        let current = self.v().chat_top;

        let top = if to_end || rows == 0 {
            None
        } else if rows < 0 {
            // From "following", scrolling up starts from where the tail actually begins.
            let from = current.unwrap_or_else(|| lines.saturating_sub(page));
            Some(from.saturating_sub(rows.unsigned_abs()))
        } else {
            match current {
                // Already at the end, and there is nowhere past it to scroll into.
                None => None,
                Some(c) => {
                    let next = c.saturating_add(rows as u32);
                    // Past the end means back to following, so a user cannot scroll into blank
                    // space.
                    if next + page >= lines { None } else { Some(next) }
                }
            }
        };
        self.vm().chat_top = top;
        let _ = self.editor.apply(&plugin, ApiCall::WinScrollTo { win, top_line: top });
    }

    fn run_builtin(
        &mut self,
        name: &str,
        args: Vec<String>,
        key: Option<&neosh_proto::KeyContext>,
    ) {
        match name {
            "config.reload" => self.reload_config(),
            "quit" => self.leave(),
            "stop" => self.quitting = true,
            "interrupt" => self.interrupt(),
            "chat.scroll_up" => self.scroll_chat(-1),
            "chat.scroll_down" => self.scroll_chat(1),
            "chat.scroll_end" => self.scroll_chat(0),
            // A toggle, because it is the door and a door opens both ways. Pressed while already
            // reading it used to do nothing at all, which for the one key bound in *every* mode is
            // the wrong answer — that set exists so there is always something that works.
            "chat.read" => {
                if self.v().reading {
                    self.leave_reading();
                } else {
                    self.enter_reading();
                }
            }
            // Sending while a turn runs *queues*, which is the right default and is also the one
            // send you cannot take back by pressing anything. Enter is a decision made in half a
            // second about a sentence you were still writing; this is the other half of it.
            //
            // Back into the composer rather than simply dropped, because "I queued the wrong thing"
            // and "I queued the right thing badly" are the same keystroke away from each other, and
            // a key that deleted your sentence to fix a typo in it would be a worse trade than
            // having no key at all. Whatever is already in the composer keeps its place after it.
            "chat.queue.edit" => {
                let here = self.active_session();
                match self.agent.unqueue_last(&here) {
                    None => self.editor_message(MessageLevel::Info, "nothing queued"),
                    Some(prompt) => {
                        let draft = self.composer_text();
                        let joined = match draft.trim().is_empty() {
                            true => prompt.text,
                            false => format!("{}\n{draft}", prompt.text),
                        };
                        // What was attached to it comes back too, on the row it came off. A
                        // sentence you can edit and a picture you cannot put back would be half
                        // an undo.
                        if !prompt.images.is_empty() {
                            let store = self.image_store();
                            let row = self.attached.entry(here).or_default();
                            for b in &prompt.images {
                                if let ContentBlock::Image { path, .. } = b
                                    && let Ok(a) =
                                        crate::images::from_path(&store, std::path::Path::new(path))
                                {
                                    row.push(a);
                                }
                            }
                        }
                        self.set_composer(&joined);
                        self.draw_working();
                        self.refresh_composer();
                    }
                }
            }
            // Nothing on the clipboard is the common case rather than a failure, so it is said the
            // same way "nothing queued" is: a line, not an error.
            // Only the failure is worth a line: a chip appearing above the composer says the
            // other thing better than a sentence could. "Nothing on the clipboard" has nothing to
            // show for itself, and is the answer people actually need explaining.
            "chat.image.paste" => self.begin_paste(None),
            "chat.image.drop" => self.drop_attachment(),
            "chat.image.clear" => {
                let here = self.active_session();
                let n = self.attached.remove(&here).unwrap_or_default().len();
                self.editor_message(
                    MessageLevel::Info,
                    match n {
                        0 => "nothing attached".to_string(),
                        1 => "took off 1 image".to_string(),
                        n => format!("took off {n} images"),
                    },
                );
                self.refresh_composer();
            }
            "chat.toggle_card" => {
                if self.v().reading {
                    self.toggle_card_here();
                } else {
                    self.editor_message(MessageLevel::Info, "^S first: cards open while reading");
                }
            }
            "edit.copy" => {
                if !self.copy_selection(false) {
                    self.editor_message(MessageLevel::Warn, "nothing selected");
                }
            }
            "edit.cut" => {
                if !self.copy_selection(true) {
                    self.editor_message(MessageLevel::Warn, "nothing selected");
                }
            }
            "edit.select_all" => {
                let win = if self.v().reading { self.v().chat_win } else { self.v().composer_win };
                self.select_all(win);
            }
            "edit.newline" => self.compose(TextEdit::Insert { text: "\n".into() }),
            "provider.key" => {
                // Named, or else the instance serving the model you are on — the one you would be
                // asked about, and the only one this could mean without a picker to choose from.
                let target = args
                    .first()
                    .map(|a| neosh_proto::InstanceId::from(a.as_str()))
                    .or_else(|| self.agent.selection().map(|s| s.instance));
                match target {
                    Some(instance) => {
                        if let Err(e) = self.begin_secret(instance, true, None) {
                            self.editor_message(MessageLevel::Warn, e.to_string());
                        }
                    }
                    None => self.editor_message(MessageLevel::Warn, "no model is selected"),
                }
            }

            // ---- windows, panes and tabs ---------------------------------
            "window.split.right" => self.split_pane(Direction::Right, false),
            "window.split.left" => self.split_pane(Direction::Left, false),
            "window.split.down" => self.split_pane(Direction::Down, false),
            "window.split.up" => self.split_pane(Direction::Up, false),
            "window.split.chat" => self.split_pane(Direction::Right, true),
            "window.close" => self.close_pane(),
            "window.only" => self.only_pane(),
            "window.left" => self.go_pane(Direction::Left),
            "window.right" => self.go_pane(Direction::Right),
            "window.up" => self.go_pane(Direction::Up),
            "window.down" => self.go_pane(Direction::Down),
            "window.next" => self.next_pane(),
            "window.wider" => self.resize_pane(Direction::Right, RESIZE_STEP),
            "window.narrower" => self.resize_pane(Direction::Right, -RESIZE_STEP),
            "window.taller" => self.resize_pane(Direction::Down, RESIZE_STEP),
            "window.shorter" => self.resize_pane(Direction::Down, -RESIZE_STEP),
            "window.equalize" => {
                let view = self.view_now();
                self.editor.pane_equalize(view);
            }
            "window.move.left" => self.move_pane(Direction::Left),
            "window.move.down" => self.move_pane(Direction::Down),
            "window.move.up" => self.move_pane(Direction::Up),
            "window.move.right" => self.move_pane(Direction::Right),
            "window.exchange" => self.exchange_pane(),
            "window.keys" => self.show_window_keys(),

            // ---- scrolling a panel that does not fit ---------------------
            // Aimed at whatever has focus, which for these keys is always the float they resolved
            // in: the scope they are bound at exists only while one is focused.
            "scroll.down" => self.scroll_focused(ScrollAmount::Lines { n: 1 }),
            "scroll.up" => self.scroll_focused(ScrollAmount::Lines { n: -1 }),
            "scroll.half_down" => self.scroll_focused(ScrollAmount::Half { n: 1 }),
            "scroll.half_up" => self.scroll_focused(ScrollAmount::Half { n: -1 }),
            "scroll.page_down" => self.scroll_focused(ScrollAmount::Page { n: 1 }),
            "scroll.page_up" => self.scroll_focused(ScrollAmount::Page { n: -1 }),
            "scroll.top" => self.scroll_focused(ScrollAmount::Top),
            "scroll.bottom" => self.scroll_focused(ScrollAmount::Bottom),

            "tab.open" => self.begin_tab_choice(),
            "tab.new" => self.new_tab(),
            "tab.new.chat" => {
                let cwd = self.session_cwd(&self.v().session.clone());
                self.new_tab();
                self.start_conversation_here(cwd);
            }
            "tab.new.term" => {
                let view = self.view_now();
                self.editor.tab_new(view, None, true);
                // Marked *before* the sync that furnishes it: `sync_panes` is what turns a pane the
                // tree has into a pane with something in it, and it has nothing else to go on.
                // A directory, when a caller named one. `<C-w>T` names none and gets the
                // conversation's, which is what it has always done; a key pressed on a row in the
                // project panel names the row's, because that is what the key was about.
                let cwd = args
                    .first()
                    .filter(|a| !a.is_empty())
                    .map(std::path::PathBuf::from)
                    .filter(|p| p.is_dir());
                self.pending_term =
                    self.editor.active_pane_of(view).map(|p| (p, ShellAt::Here { cwd }));
                self.sync_panes();
                self.refresh_status();
            }
            "window.split.term" => {
                let view = self.view_now();
                let from = self.editor.active_pane(view);
                if let Some(new) = self.editor.pane_split(view, from, Direction::Right) {
                    self.pending_term = Some((new, ShellAt::Here { cwd: None }));
                    self.sync_panes();
                    self.editor.pane_focus(view, new);
                    self.sync_panes();
                    self.refresh_status();
                }
            }
            "tab.close" => self.close_tab(),
            "tab.next" => self.step_tab(1),
            "tab.prev" => self.step_tab(-1),
            "tab.rename" => self.begin_tab_rename(),
            "tab.move.left" => self.shift_tab(-1),
            "tab.move.right" => self.shift_tab(1),
            "tab.select" => {
                // The digit is in the key rather than in an argument: `<C-w>3` is one binding per
                // number and no way to carry a parameter through a keymap. Run from `^K` with no
                // key at all, an argument is how it is said instead.
                let n = key
                    .and_then(|k| match &k.key.code {
                        KeyCode::Char { c } => c.chars().next(),
                        _ => None,
                    })
                    .and_then(|c| c.to_digit(10))
                    .or_else(|| args.first().and_then(|a| a.parse::<u32>().ok()));
                if let Some(n) = n.filter(|n| *n >= 1) {
                    let view = self.view_now();
                    // Counted on the bar, which is what the bar numbered: a conversation's tabs are
                    // `1`…`n` on screen, and indexing the whole list would take `<C-w>2` to whatever
                    // tab of whatever other conversation happens to sit second in memory.
                    let tabs = self.editor.visible_tabs_of(view);
                    match tabs.get(n as usize - 1) {
                        Some(t) => {
                            let id = t.id;
                            self.editor.tab_select(view, id);
                        }
                        // Silently, because `<C-w>4` with three tabs open is a miscount rather than
                        // a mistake, and a warning per press is noise on a key you are holding.
                        None => {}
                    }
                }
            }
            other => tracing::warn!("no built-in command {other}"),
        }
    }

    // ---- windows, panes and tabs ---------------------------------------------
    //
    // Each of these is a thin wrapper over the editor's public API — the same calls a plugin makes
    // — plus the two things a *key* wants that a plugin call should not assume: focus following the
    // split, and a message when there is nowhere to go. `sync_panes` furnishes whatever appears, so
    // none of these knows how to build a transcript.

    /// Divide this pane, and go into the new one.
    ///
    /// Focus follows, which the API call deliberately does not do: a plugin splitting to show you
    /// something and a person pressing a key to go and work there are different intentions, and
    /// only the second one wants the keyboard to move.
    fn split_pane(&mut self, dir: Direction, fresh: bool) {
        let view = self.view_now();
        let from = self.editor.active_pane(view);
        let Some(new) = self.editor.pane_split(view, from, dir) else {
            self.editor_message(MessageLevel::Warn, "this pane cannot be split");
            return;
        };
        // Before focusing: the new pane inherits the session of whichever pane is active when it is
        // furnished, and that has to still be the one it was split from.
        self.sync_panes();
        self.editor.pane_focus(view, new);
        self.sync_panes();
        if fresh {
            // A split you asked to put something *else* in — the one-key version of splitting and
            // then reaching for `^N`. Same directory, no dialog: `^N` is the key that asks where,
            // and this is not a second place to be asked it.
            let cwd = self.session_cwd(&self.v().session.clone());
            self.start_conversation_here(cwd);
            return;
        }
        self.refresh_status();
        self.refresh_composer();
    }

    fn close_pane(&mut self) {
        let view = self.view_now();
        let pane = self.editor.active_pane(view);
        if !self.editor.pane_close(view, pane) {
            self.editor_message(
                MessageLevel::Warn,
                "this is the last pane — ^Q closes the terminal, and the turns keep running",
            );
            return;
        }
        self.sync_panes();
        self.refresh_status();
        self.refresh_composer();
    }

    /// Close every pane in this tab except the one you are in.
    fn only_pane(&mut self) {
        let view = self.view_now();
        let keep = self.editor.active_pane(view);
        let Some(tab) = self.editor.active_tab_of(view) else { return };
        for pane in self.editor.panes_in_tab(view, tab) {
            if pane != keep {
                self.editor.pane_close(view, pane);
            }
        }
        self.sync_panes();
        self.refresh_status();
        self.refresh_composer();
    }

    fn go_pane(&mut self, dir: Direction) {
        let view = self.view_now();
        if self.editor.pane_focus_dir(view, dir).is_none() {
            // Nothing said out loud. Running out of panes in a direction is the ordinary outcome of
            // holding a key down, and a message per press would be a warning for something that has
            // not gone wrong. The cursor simply does not move, which is what Vim does too.
            return;
        }
        self.sync_panes();
        self.refresh_status();
        self.refresh_composer();
    }

    /// The next pane, or the next tab when this one has only the pane you are in.
    ///
    /// One key that always goes *somewhere*. `<C-w>w` on a single-pane tab doing nothing at all is
    /// the most common way a prefix layer teaches somebody it is broken.
    fn next_pane(&mut self) {
        let view = self.view_now();
        let Some(tab) = self.editor.active_tab_of(view) else { return };
        let panes = self.editor.panes_in_tab(view, tab);
        if panes.len() <= 1 {
            self.step_tab(1);
            return;
        }
        let at = self.editor.active_pane(view);
        let next = panes.iter().position(|p| *p == at).map(|i| (i + 1) % panes.len()).unwrap_or(0);
        let to = panes[next];
        self.editor.pane_focus(view, to);
        self.sync_panes();
        self.refresh_status();
        self.refresh_composer();
    }

    fn resize_pane(&mut self, dir: Direction, delta: i16) {
        let view = self.view_now();
        let pane = self.editor.active_pane(view);
        // Growing rightwards at the right-hand edge has no boundary to move, and the thing a person
        // means by "wider" there is the boundary on the *other* side. Tried in order rather than
        // reported as a failure: `<C-w>L` doing nothing in the last pane of a split is the key
        // working exactly half the time.
        if !self.editor.pane_resize(view, pane, dir, delta) {
            let back = match dir {
                Direction::Right => Direction::Left,
                Direction::Left => Direction::Right,
                Direction::Down => Direction::Up,
                Direction::Up => Direction::Down,
            };
            self.editor.pane_resize(view, pane, back, delta);
        }
    }

    /// Open a tab on the conversation you are already in.
    ///
    /// **A tab is a place, not a conversation.** It inherits, exactly as a split does and for the
    /// same reason: `:tabnew` in every editor gives you another view of your work, and what you do
    /// next decides what goes in it. Starting a conversation here instead put a row in the project
    /// panel every time somebody opened a tab — a list of half-used "New conversation" entries in
    /// the one panel whose job is the conversations you are actually working in, and none of them
    /// asked for.
    ///
    /// Getting a *new* conversation in it is `^N`, which is the key that asks where — a worktree,
    /// elsewhere, another machine. A tab is not a second place to be asked that, and it is not a
    /// way to be handed a conversation you did not ask for either.
    /// Send this pane to an edge of the tab, and go with it.
    ///
    /// The keyboard follows, because you moved the thing you were in — a move that left you behind
    /// would be a move you then have to chase.
    fn move_pane(&mut self, dir: Direction) {
        let view = self.view_now();
        let pane = self.editor.active_pane(view);
        if self.editor.pane_move_edge(view, pane, dir) {
            self.editor.pane_focus(view, pane);
            self.sync_panes();
            self.refresh_status();
        }
    }

    /// Swap this pane with the next one along, keeping the shape.
    ///
    /// The weights belong to the slots, so this is how you put the pane you care about in the wide
    /// half without resizing anything. Wraps, because two panes are the common case and "next" with
    /// no wrap would do nothing in one of them.
    fn exchange_pane(&mut self) {
        let view = self.view_now();
        let Some(tab) = self.editor.active_tab_of(view) else { return };
        let panes = self.editor.panes_in_tab(view, tab);
        if panes.len() < 2 {
            return;
        }
        let at = self.editor.active_pane(view);
        let Some(i) = panes.iter().position(|p| *p == at) else { return };
        let with = panes[(i + 1) % panes.len()];
        self.editor.pane_swap(view, at, with);
        // Focus follows the *pane*, which after a swap is in the other slot — so the keyboard stays
        // with the conversation you were reading rather than with the rectangle it used to be in.
        self.editor.pane_focus(view, at);
        self.sync_panes();
        self.refresh_status();
    }

    fn new_tab(&mut self) {
        let view = self.view_now();
        self.editor.tab_new(view, None, true);
        self.sync_panes();
        self.refresh_status();
        self.refresh_composer();
    }

    /// Start a conversation in this directory and go to it, in whichever pane is active.
    ///
    /// Made here rather than through `ApiCall::SessionNew`, which is `async` for one reason — it
    /// remembers the directory as a project — and the directory is one the workspace is already in,
    /// since it came off a conversation that is on screen. Everything else that call does is these
    /// four lines, and they are the four that matter: a conversation inherits the model you are
    /// using and the system prompt you set, because starting it on whatever the catalogue lists
    /// first would silently change what you are talking to.
    fn start_conversation_here(&mut self, cwd: std::path::PathBuf) {
        let mut session = neosh_agent::Session::new(cwd);
        session.created_at = now_secs();
        session.updated_at = session.created_at;
        session.selection = self.agent.selection();
        session.system = self.agent.session().system.clone();
        let id = session.id.clone();
        self.agent.sessions().insert(session);
        if self.arrive_in(&id).is_ok() {
            // `arrive_in` says *which* conversation this pane is in; this is what draws it. Without
            // it the pane keeps the rows of whatever it was showing — a transcript from another
            // conversation under a footer that has already moved on, which reads as the new
            // conversation having inherited somebody else's messages.
            self.enter_session();
        }
        self.refresh_status();
        self.refresh_composer();
    }

    fn close_tab(&mut self) {
        let view = self.view_now();
        let Some(tab) = self.editor.active_tab_of(view) else { return };
        if !self.editor.tab_close(view, tab) {
            // Of *this conversation*, which is the honest sentence now that a bar is one
            // conversation's: there may well be tabs open elsewhere, and closing this one would
            // have landed you in them — which is a key about where you are looking deciding what
            // you are working on. `^T` goes to another conversation and `^Q` closes the terminal.
            self.editor_message(
                MessageLevel::Warn,
                "this conversation's last tab — ^T goes elsewhere, ^Q closes the terminal",
            );
            return;
        }
        self.sync_panes();
        self.refresh_status();
        self.refresh_composer();
    }

    fn step_tab(&mut self, delta: i32) {
        let view = self.view_now();
        self.editor.tab_step(view, delta);
        self.sync_panes();
        self.refresh_status();
        self.refresh_composer();
    }

    /// Show what the window prefix can do.
    ///
    /// The thing that makes a prefix layer affordable at all. Vim's `<C-w>` is undiscoverable —
    /// you either know it or you never find out — and a workspace whose rule is *"a default binding
    /// is a key every terminal sends"* cannot then hide two dozen verbs behind a letter nobody has
    /// been told about. So the letter tells you.
    ///
    /// Built from the *live* keymap rather than from [`WINDOW_KEYS`], so it lists the keys somebody
    /// actually has: a rebinding in `init.ts` shows the new letter, and a plugin that adds a verb
    /// under the prefix appears here without anyone writing glue for it. A legend is a promise
    /// about a keyboard.
    ///
    /// Not modal, not focusable: it is a hint over whatever you were doing, and the next key goes
    /// to the chord it is describing rather than to the panel. Which is also why it closes itself
    /// on the next press — see [`Self::drop_window_hint`].
    fn show_window_keys(&mut self) {
        if self.window_hint.is_some() {
            return;
        }
        let prefix = self.window_prefix();
        let view = self.view_now();

        // The live registry, narrowed to what starts with the prefix. Sorted by the order
        // `WINDOW_KEYS` lists them in — grouped by intent rather than alphabetically, because
        // somebody reading this has a goal and not a letter — with anything a plugin added after.
        let mut rows: Vec<(String, String, String)> = Vec::new();
        for entry in self.editor.keymaps_for(self.editor.mode(view)) {
            let Some(suffix) = entry.lhs.strip_prefix(&prefix) else { continue };
            if suffix.is_empty() {
                continue;
            }
            let rank = WINDOW_KEYS
                .iter()
                .position(|(k, _)| *k == suffix)
                .unwrap_or(WINDOW_KEYS.len());
            // The panel says which key to *press*, so it prints the character rather than the
            // notation you would write in `init.ts`. `^Z` still lists the notation, which is what
            // you would copy.
            let shown = suffix.replace("<lt>", "<");
            let desc = entry
                .desc
                .clone()
                .or_else(|| self.editor.command_desc(&entry.command))
                .unwrap_or_else(|| entry.command.clone());
            rows.push((format!("{rank:04}{suffix}"), shown, desc));
        }
        rows.sort();
        // The nine numbered tabs are one idea, not nine rows: listing `1`…`9` separately would be
        // a third of the panel spent on a thing the tab bar already numbers for you.
        rows.retain(|(_, k, _)| k.parse::<u32>().is_err());
        if self.editor.tab_count_of(view) > 1 {
            rows.push((String::new(), "1-9".into(), "Go to that tab".into()));
        }
        if rows.is_empty() {
            return;
        }

        let key_col = rows.iter().map(|(_, k, _)| display_width(k)).max().unwrap_or(1);
        let lines: Vec<String> = rows
            .iter()
            .map(|(_, k, d)| format!("  {k:<key_col$}  {d}"))
            .collect();
        let width = lines.iter().map(|l| display_width(l)).max().unwrap_or(20) + 2;

        let plugin = PluginId::from(BUILTIN);
        let buf = self.editor.create_buffer_of_kind("[window keys]", KIND_WINDOW_HINT);
        let _ = self.editor.apply_in(view, &plugin, ApiCall::BufSetLines {
            buf,
            start: 0,
            end: -1,
            lines,
        });
        for (row, (_, k, _)) in rows.iter().enumerate() {
            let _ = self.editor.apply_in(view, &plugin, ApiCall::MarkSet {
                ns: self.tabline_ns,
                buf,
                row: row as u32,
                col: 2,
                opts: neosh_proto::ExtmarkOpts {
                    end_col: Some(2 + k.len() as u32),
                    hl_group: Some("Tabline.Key".into()),
                    line_hl_group: None,
                    virt_text: vec![],
                    virt_text_pos: VirtTextPos::Eol,
                    on_delete: neosh_proto::OnDelete::Clamp,
                    priority: 0,
                    image: None,
                },
            });
        }

        let res = self.editor.apply_in(view, &plugin, ApiCall::FloatOpen {
            buf,
            config: neosh_proto::FloatConfig {
                // Against the message field, which is where your eyes already are. Centred it would
                // cover the answer you are deciding how to split.
                anchor: neosh_proto::Anchor::Dock { dock: Dock::Bottom },
                border: neosh_proto::BorderStyle::Rounded,
                border_hl: Some("Tabline.Key".into()),
                title: Some(format!(" {prefix} ")),
                footer: None,
                width: neosh_proto::Extent::Max { n: width as u16 },
                // Tall enough for every verb there is, so the list is not silently short by three
                // rows — the ones that would go are the tab verbs at the end, which is exactly the
                // half somebody who has just found the prefix does not know about. The frontend
                // clamps to the room there is, so a short terminal still gets as much as fits.
                height: neosh_proto::Extent::Max { n: 32 },
                // Neither focusable nor modal: the key you press next belongs to the chord this is
                // describing. A panel that took the keyboard would be a hint you have to dismiss
                // before you can use what it is hinting at.
                focusable: false,
                modal: false,
                // Which is also why it does not scroll: the scroll keys resolve against whatever
                // has focus, and nothing that cannot take focus can be what they mean. On a
                // terminal too short for the whole list it is clipped, and the bar down its right
                // edge is what says so — `^Z` is where the rest of it is written down.
                scroll: false,
                close_on_blur: false,
                z: 300,
                offset: Default::default(),
            },
            view: Some(view),
        });
        if let Ok(ApiOk::Win { win }) = res {
            self.window_hint = Some((win, buf));
        }
    }

    /// Ask what a new tab should hold.
    ///
    /// The tab is not made until you have answered. Making it first and filling it afterwards would
    /// mean `Esc` had to *undo* one — and a tab that flickers into the bar and out again on a key
    /// you cancelled is worse than the question taking a moment.
    fn begin_tab_choice(&mut self) {
        if self.tab_choice.is_some() {
            return;
        }
        let view = self.view_now();
        let plugin = PluginId::from(BUILTIN);
        let buf = self.editor.create_buffer_of_kind("[new tab]", KIND_TAB_CHOICE);
        let names = TAB_CHOICES.iter().map(|(n, _)| display_width(n)).max().unwrap_or(0);
        let width = TAB_CHOICES
            .iter()
            .map(|(_, d)| names + display_width(d) + 6)
            .max()
            .unwrap_or(40);
        let res = self.editor.apply_in(view, &plugin, ApiCall::FloatOpen {
            buf,
            config: neosh_proto::FloatConfig {
                anchor: neosh_proto::Anchor::Dock { dock: Dock::Bottom },
                border: neosh_proto::BorderStyle::Rounded,
                border_hl: Some("Tabline.Key".into()),
                title: Some(" new tab ".into()),
                footer: Some(" 1-3 pick   jk move   esc cancel ".into()),
                width: neosh_proto::Extent::Max { n: width as u16 },
                height: neosh_proto::Extent::Max { n: TAB_CHOICES.len() as u16 },
                // Focusable and modal: this is a question you are in the middle of answering, so
                // nothing else may take the keyboard out from under it — including the keys that
                // would open a second panel behind this one.
                focusable: true,
                modal: true,
                // Three rows, and `j`/`k` move between them: this is a list with a cursor, which is
                // the case `scroll` is deliberately not for.
                scroll: false,
                close_on_blur: false,
                z: 300,
                offset: Default::default(),
            },
            view: Some(view),
        });
        let Ok(ApiOk::Win { win }) = res else { return };
        self.tab_choice = Some(TabChooser { win, buf, at: 0 });
        self.render_tab_choice();
    }

    fn render_tab_choice(&mut self) {
        let Some(c) = self.tab_choice.as_ref() else { return };
        let (buf, at) = (c.buf, c.at);
        let view = self.view_now();
        let plugin = PluginId::from(BUILTIN);
        let names = TAB_CHOICES.iter().map(|(n, _)| display_width(n)).max().unwrap_or(0);
        let lines: Vec<String> = TAB_CHOICES
            .iter()
            .enumerate()
            // Numbered, because a list you can also answer with one key is a list you do not have
            // to walk. The same numbering the tab bar uses, for the same reason.
            //
            // The second column is aligned, which is the whole of why this is not one `format!`:
            // three rows whose descriptions each start somewhere different read as three unrelated
            // sentences rather than as a table you can scan down.
            .map(|(n, (name, desc))| format!(" {} {:<pad$}  {}", n + 1, name, desc, pad = names))
            .collect();
        let _ = self.editor.apply_in(view, &plugin, ApiCall::BufSetLines {
            buf,
            start: 0,
            end: -1,
            lines: lines.clone(),
        });
        let _ = self.editor.apply_in(view, &plugin, ApiCall::MarkClear {
            ns: self.tabline_ns,
            buf,
            start: None,
            end: None,
        });
        for (n, line) in lines.iter().enumerate() {
            let group = if n == at { "Tabline.Active" } else { "Tabline.Tab" };
            let _ = self.editor.apply_in(view, &plugin, ApiCall::MarkSet {
                ns: self.tabline_ns,
                buf,
                row: n as u32,
                col: 0,
                opts: neosh_proto::ExtmarkOpts {
                    end_col: Some(line.len() as u32),
                    hl_group: Some(group.into()),
                    line_hl_group: (n == at).then(|| "CursorLine".to_string()),
                    virt_text: vec![],
                    virt_text_pos: VirtTextPos::Eol,
                    on_delete: neosh_proto::OnDelete::Clamp,
                    priority: 0,
                    image: None,
                },
            });
        }
    }

    /// A key while the panel is up. Answers, moves, or gives up.
    fn tab_choice_key(&mut self, key: neosh_proto::KeyPress) {
        let Some(c) = self.tab_choice.as_ref() else { return };
        let at = c.at;
        let last = TAB_CHOICES.len() - 1;
        match &key.code {
            KeyCode::Esc => return self.end_tab_choice(None),
            KeyCode::Enter => return self.end_tab_choice(Some(at)),
            KeyCode::Down => self.move_tab_choice(at.saturating_add(1).min(last)),
            KeyCode::Up => self.move_tab_choice(at.saturating_sub(1)),
            KeyCode::Char { c } if key.mods.ctrl => match c.as_str() {
                "n" => self.move_tab_choice(at.saturating_add(1).min(last)),
                "p" => self.move_tab_choice(at.saturating_sub(1)),
                "c" => return self.end_tab_choice(None),
                _ => {}
            },
            KeyCode::Char { c } => match c.as_str() {
                "j" => self.move_tab_choice(at.saturating_add(1).min(last)),
                "k" => self.move_tab_choice(at.saturating_sub(1)),
                "q" => return self.end_tab_choice(None),
                d => {
                    if let Some(n) = d.parse::<usize>().ok().filter(|n| *n >= 1 && *n <= last + 1) {
                        return self.end_tab_choice(Some(n - 1));
                    }
                }
            },
            _ => {}
        }
    }

    fn move_tab_choice(&mut self, to: usize) {
        if let Some(c) = self.tab_choice.as_mut() {
            c.at = to;
        }
        self.render_tab_choice();
    }

    /// Take the panel down, and make the tab it was asking about.
    fn end_tab_choice(&mut self, pick: Option<usize>) {
        let Some(c) = self.tab_choice.take() else { return };
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::WinClose { win: c.win });
        let _ = self.editor.apply(&plugin, ApiCall::BufDelete { buf: c.buf });
        // Closed *before* the tab is made, so the panel is not on screen over the thing it just
        // opened — and so the modal is off the focus stack before anything takes the keyboard.
        // Positional, and deliberately not a lookup by label: the order is a product decision made
        // in `TAB_CHOICES`, and reading it back out of a string would make renaming a row a way to
        // silently change what it does.
        match pick {
            Some(0) => {
                let cwd = self.session_cwd(&self.v().session.clone());
                self.new_tab();
                self.start_conversation_here(cwd);
            }
            Some(1) => self.run_builtin("tab.new.term", Vec::new(), None),
            Some(2) => self.new_tab(),
            _ => {}
        }
    }

    /// Take the prefix panel down.
    ///
    /// Called from the key path on the press *after* it appeared, whatever that press was — the
    /// panel describes one half-typed chord, and the chord is over either way.
    fn drop_window_hint(&mut self) {
        let Some((win, buf)) = self.window_hint.take() else { return };
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::WinClose { win });
        let _ = self.editor.apply(&plugin, ApiCall::BufDelete { buf });
    }

    /// Whether this key is the first press of the window prefix.
    ///
    /// The one key a terminal pane does not swallow. Parsed from the setting rather than compared
    /// against `<C-w>`, so moving the prefix in `init.ts` moves the way out of a shell too — a
    /// rebinding that left you unable to leave a full-screen program would be the worst possible
    /// thing to get wrong here.
    fn starts_prefix(&self, key: &neosh_proto::KeyPress) -> bool {
        neosh_core::keymap::parse_keys(&self.window_prefix())
            .ok()
            .and_then(|seq| seq.first().cloned())
            .is_some_and(|first| first == *key)
    }

    /// Whether the keys held so far are the window prefix and nothing more.
    ///
    /// What the hint timer asks before drawing. Any *other* half-typed chord — a `g` in the
    /// transcript, a plugin's own prefix — is not this panel's business.
    fn holding_window_prefix(&self) -> bool {
        let view = self.view_now();
        let held = self.editor.pending_keys(view);
        !held.is_empty() && neosh_core::keymap::format_keys(&held) == self.window_prefix()
    }

    /// Start naming this tab, borrowing the composer to type in.
    ///
    /// The same borrow the search prompt makes, and given back the same way. A field of its own
    /// would be a second text input to get right — cursor keys, `^U`, `^W`, paste, width — for one
    /// verb, and the composer is already the thing your hands are on.
    fn begin_tab_rename(&mut self) {
        if self.v().rename.is_some() || self.v().search.is_some() || self.v().secret.is_some() {
            return;
        }
        let view = self.view_now();
        let Some(tab) = self.editor.active_tab_of(view) else { return };
        // Prefilled with what it is called now, including the derived name — so `<C-w>r` then a
        // word is an edit, and `^U` then a word is a replacement. Starting empty makes every rename
        // a retype.
        let current = self
            .editor
            .tabs_of(view)
            .iter()
            .find(|t| t.id == tab)
            .map(|t| self.tab_title(view, t))
            .unwrap_or_default();
        self.vm().rename = Some(RenamePrompt {
            tab,
            value: current,
            draft: self.composer_text(),
        });
        self.render_rename();
        self.refresh_status();
    }

    fn render_rename(&mut self) {
        let Some(p) = self.v().rename.as_ref() else { return };
        let line = format!("name this tab: {}", p.value);
        self.set_composer(&line);
    }

    fn rename_key(&mut self, key: neosh_proto::KeyPress) {
        let Some(p) = self.vm().rename.as_mut() else { return };
        match key.code {
            KeyCode::Esc => return self.finish_rename(false),
            KeyCode::Enter => return self.finish_rename(true),
            KeyCode::Backspace => {
                let keep = p.value.graphemes(true).count().saturating_sub(1);
                p.value = p.value.graphemes(true).take(keep).collect();
            }
            KeyCode::Char { ref c } if key.mods.ctrl => match c.as_str() {
                "u" => p.value.clear(),
                "w" => {
                    let kept = p.value.trim_end();
                    let cut = kept.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
                    p.value.truncate(cut);
                }
                "c" => return self.finish_rename(false),
                _ => return,
            },
            KeyCode::Char { ref c } if !key.mods.alt => p.value.push_str(c),
            _ => return,
        }
        self.render_rename();
    }

    fn finish_rename(&mut self, submit: bool) {
        let Some(p) = self.vm().rename.take() else { return };
        self.set_composer(&p.draft);
        if submit {
            let view = self.view_now();
            // An empty name is not a name: it gives the tab back to being called after what is in
            // it, which is the only way back to a derived name once one has been set.
            let title = Some(p.value.trim().to_string()).filter(|s| !s.is_empty());
            self.editor.tab_rename(view, p.tab, title);
        }
        self.refresh_status();
        self.refresh_composer();
    }

    fn shift_tab(&mut self, delta: i32) {
        let view = self.view_now();
        let Some(tab) = self.editor.active_tab_of(view) else { return };
        // Positions on the bar, which is what a drag is in: `to` is a place among this
        // conversation's tabs, and one place along has to be one place along on screen.
        let tabs = self.editor.visible_tabs_of(view);
        let Some(at) = tabs.iter().position(|t| t.id == tab) else { return };
        let to = (at as i32 + delta).clamp(0, tabs.len().saturating_sub(1) as i32) as u32;
        self.editor.tab_move(view, tab, to);
    }

    /// Tear down everything configuration produced, then build it again from the files on disk.
    ///
    /// Deliberately whole-world rather than incremental: "re-read my configuration" is a semantic
    /// people can predict, and diffing two configurations to apply the delta is a source of bugs
    /// that only appear in the states nobody tested.
    fn reload_config(&mut self) {
        if self.startup.active.is_some() || self.startup.loading > 0 {
            if !self.startup.reload_pending {
                self.startup.reload_pending = true;
                self.editor_message(MessageLevel::Info, "still loading; reloading when it settles");
            }
            return;
        }
        let Some(reload) = self.startup.reload.as_ref() else {
            self.editor_message(MessageLevel::Error, "this session cannot reload configuration");
            return;
        };

        // Read the files *first*. A config that no longer parses should leave the running one
        // alone rather than tearing it down and having nothing to put back.
        let resolved = match reload() {
            Ok(r) => r,
            Err(e) => {
                self.editor_message(MessageLevel::Error, format!("reload failed, keeping the current config: {e}"));
                return;
            }
        };

        for id in self.bridge.loaded() {
            self.tear_down_plugin(&id);
        }
        self.drop_held_plugins();
        // A reload may have added, removed or repointed a provider instance, so what was discovered
        // from the old set is no longer an answer to anything.
        self.model_cache.lock().expect("model cache poisoned").clear();

        // Reset the settings the old configuration changed, so deleting a line from `init.ts`
        // actually removes its effect. Sourcing a config that only ever *adds* is the thing that
        // makes people restart instead of reloading.
        let modified: Vec<String> = self
            .editor
            .options()
            .all()
            .into_iter()
            .filter(|o| o.modified && o.owner == BUILTIN)
            .map(|o| o.name)
            .collect();
        for name in modified {
            let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::OptReset { name });
        }
        self.drain_effects();

        self.startup.generation += 1;
        self.startup.deferred.clear();
        let model = self.startup.cli_model.clone();
        let dirs = self.startup.cli_plugin_dirs.clone();
        self.apply_config(resolved, model, dirs);
        self.editor_message(MessageLevel::Info, "configuration reloaded");
    }

    /// The options neosh itself owns.
    ///
    /// Declared through `ApiCall::OptDeclare` under the plugin id `neosh`, exactly as a plugin
    /// declares its own. If this ever needed a private path, the option API would be wrong.
    fn declare_builtin_options(&mut self) {
        let specs = [
            OptionSpec {
                name: "agent.model".into(),
                ty: OptionType::Str,
                default: OptionValue::Str(String::new()),
                description: Some(
                    "Model to use, as `instance/model`. Empty picks the first one that works."
                        .into(),
                ),
            },
            OptionSpec {
                name: "agent.system_prompt".into(),
                ty: OptionType::Str,
                default: OptionValue::Str(String::new()),
                description: Some("Replaces the built-in system prompt when set.".into()),
            },
            OptionSpec {
                name: "agent.append_prompt".into(),
                ty: OptionType::Str,
                default: OptionValue::Str(String::new()),
                description: Some(
                    "Said at the end of every message you send — \"run the tests before you say \
                     you are done\", \"answer briefly\". Added to what is sent, never to the \
                     transcript, so what you typed is what stays on screen. Empty says nothing."
                        .into(),
                ),
            },
            OptionSpec {
                name: "chat.markdown".into(),
                ty: OptionType::Bool,
                default: OptionValue::Bool(true),
                description: Some(
                    "Render answers as markdown: headings, lists, code blocks, and inline \
                     emphasis with its markers removed. Off shows exactly what the model sent."
                        .into(),
                ),
            },
            OptionSpec {
                name: "chat.show_thinking".into(),
                ty: OptionType::Bool,
                default: OptionValue::Bool(false),
                description: Some("Stream the model's reasoning into the chat buffer.".into()),
            },
            OptionSpec {
                name: "chat.show_tools".into(),
                ty: OptionType::Bool,
                default: OptionValue::Bool(true),
                description: Some("Show a line in chat when a tool runs. Errors always show.".into()),
            },
            OptionSpec {
                name: "chat.times".into(),
                ty: OptionType::Bool,
                default: OptionValue::Bool(true),
                description: Some(
                    "Write the time in the transcript's right margin: when each turn was asked, \
                     how long it took, and how long ago the newest one finished. Virtual text, so \
                     it is never part of what `y` copies."
                        .into(),
                ),
            },
            OptionSpec {
                name: "chat.clock".into(),
                ty: OptionType::Enum { values: vec!["24h".into(), "12h".into()] },
                default: OptionValue::Str("24h".into()),
                description: Some(
                    "Which clock the times in the transcript are written on.".into(),
                ),
            },
            OptionSpec {
                name: "chat.show_plan".into(),
                ty: OptionType::Bool,
                default: OptionValue::Bool(true),
                description: Some(
                    "Show the agent's own checklist above the working line while it works, and \
                     leave the final one under its answer. Only drivers that keep a plan have one."
                        .into(),
                ),
            },
            OptionSpec {
                name: "chat.plan_rows".into(),
                ty: OptionType::Int { min: Some(0), max: Some(50) },
                default: OptionValue::Int(5),
                description: Some(
                    "How many rows the checklist may take above the working line. Past it, what is \
                     finished and what is not started yet each become a count and the step in hand \
                     stays. `0` shows every step. The copy left under the answer is always whole."
                        .into(),
                ),
            },
            OptionSpec {
                name: "chat.tool_output_lines".into(),
                ty: OptionType::Int { min: Some(0), max: Some(200) },
                default: OptionValue::Int(3),
                description: Some(
                    "How much of what a tool came back with to show under it. `0` shows only how \
                     many lines there were. An error always shows at least its first line."
                        .into(),
                ),
            },
            OptionSpec {
                name: "chat.preview_lines".into(),
                ty: OptionType::Int { min: Some(0), max: Some(500) },
                default: OptionValue::Int(16),
                description: Some(
                    "How much of a tool card the cursor is in shows while reading the transcript \
                     with `^S`. The card opens as you arrive and folds as you leave, so this is a \
                     budget per card rather than per transcript. Never less than the folded card \
                     showed. Tab opens one all the way and keeps it open."
                        .into(),
                ),
            },
            OptionSpec {
                name: "chat.diff_lines".into(),
                ty: OptionType::Int { min: Some(0), max: Some(500) },
                default: OptionValue::Int(12),
                description: Some(
                    "How many rows of an edit's diff to show under its card before it folds. `0` \
                     leaves only the +N -N on the header. Any card opens with Tab while reading."
                        .into(),
                ),
            },
            OptionSpec {
                name: "mapleader".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("\\".into()),
                description: Some(
                    "What `<leader>` means in a key binding. Read when the binding is made, as in \
                     Neovim — set it in `init.ts` before you use it."
                        .into(),
                ),
            },
            OptionSpec {
                name: "ui.scroll_rows".into(),
                ty: OptionType::Int { min: Some(1), max: Some(20) },
                default: OptionValue::Int(3),
                description: Some(
                    "Buffer rows the transcript moves for one notch of the mouse wheel. Three is \
                     what a terminal sends per notch when it has to fake the wheel with arrow \
                     keys, so it is the distance a hand already expects."
                        .into(),
                ),
            },
            OptionSpec {
                name: "timeoutlen".into(),
                ty: OptionType::Int { min: Some(0), max: Some(10_000) },
                default: OptionValue::Int(500),
                description: Some(
                    "Milliseconds to wait for the rest of a multi-key binding before giving up and \
                     treating what was typed as ordinary input. Without this, binding a sequence \
                     whose first key is a printable character makes that character untypeable."
                        .into(),
                ),
            },
            // Widget keys. Space-separated lists in Neovim notation, so more than one key can mean
            // the same thing — `<C-n>` and `<Down>` both move down, and adding a third is a
            // setting rather than a fork. Declared by the host rather than by whichever plugin
            // happens to open a picker first, because two plugins declaring the same option name
            // is an error and every plugin uses this widget library.
            //
            // **The chord comes first, and the arrow second.** The first key in the list is the
            // one a widget prints on its hint row, and a legend is a promise about a keyboard:
            // every terminal sends `^N`, while an arrow is a key some keyboards only have on a
            // layer and some remote sessions mangle. Both work — this is the order they are
            // *offered* in, not the set.
            OptionSpec {
                name: "ui.keys.next".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<C-n> <Down> <C-j>".into()),
                description: Some("Move down in a picker or completion list.".into()),
            },
            OptionSpec {
                name: "ui.keys.prev".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<C-p> <Up> <C-k>".into()),
                description: Some("Move up in a picker or completion list.".into()),
            },
            OptionSpec {
                name: "ui.keys.page_down".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<PageDown>".into()),
                description: Some(
                    "Move down a screenful in a picker. Deliberately not `<C-d>` by default: a \
                     widget claims its keys while it is open, and taking a chord you use elsewhere \
                     for something you do rarely is a bad trade you only notice later."
                        .into(),
                ),
            },
            OptionSpec {
                name: "ui.keys.page_up".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<PageUp>".into()),
                description: Some("Move up a screenful in a picker.".into()),
            },
            OptionSpec {
                name: "ui.keys.first".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<C-Home>".into()),
                description: Some("Jump to the first row of a picker.".into()),
            },
            OptionSpec {
                name: "ui.keys.last".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<C-End>".into()),
                description: Some("Jump to the last row of a picker.".into()),
            },
            OptionSpec {
                name: "ui.keys.accept".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<CR>".into()),
                description: Some("Take the highlighted row, or what you typed.".into()),
            },
            OptionSpec {
                name: "ui.keys.dismiss".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<Esc> <C-c>".into()),
                description: Some("Close a picker or prompt without choosing.".into()),
            },
            OptionSpec {
                name: "ui.keys.complete".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<Tab> <Right>".into()),
                description: Some(
                    "Take the highlighted completion into the field without accepting it — how you \
                     descend a directory a segment at a time."
                        .into(),
                ),
            },
            OptionSpec {
                name: "ui.keys.clear".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<C-u>".into()),
                description: Some(
                    "Empty the filter or field. `<C-u>` because that is what it does in every \
                     other text field a terminal has ever shown you."
                        .into(),
                ),
            },
            OptionSpec {
                name: "ui.keys.window_prefix".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<C-w>".into()),
                description: Some(
                    "The key everything about panes and tabs lives under — splitting, moving \
                     between them, resizing, and the tab verbs. Vim's, because the fingers that \
                     would reach for a prefix already know it, and because every Ctrl-with-a-letter \
                     in this workspace is spoken for. Hold it to see what follows."
                        .into(),
                ),
            },
            OptionSpec {
                name: "ui.keys.hint_delay".into(),
                ty: OptionType::Int { min: Some(0), max: Some(5_000) },
                default: OptionValue::Int(300),
                description: Some(
                    "How long to hold the window prefix before it lists what it can do, in \
                     milliseconds. Long enough that typing a chord fluently never flashes a panel; \
                     short enough that hesitating is answered. `0` shows it at once."
                        .into(),
                ),
            },
            OptionSpec {
                name: "ui.keys.delete_word".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<C-BS>".into()),
                description: Some(
                    "Delete the word behind the cursor — a path segment, in a path field. Was \
                     `<C-w>`, which is now the window prefix: one key cannot both delete a word \
                     and wait to see whether a window verb follows it, because waiting is what \
                     makes every word-delete feel late. `ui.keys.window_prefix` moves the prefix \
                     instead, if `<C-w>` is the one you want back."
                        .into(),
                ),
            },
            OptionSpec {
                name: "ui.keys.pane_next".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<Tab> <Right>".into()),
                description: Some(
                    "Move to the next pane of a two-pane widget — from the provider rail to the \
                     models beside it. Shares `<Tab>` with `complete`, which is safe because no \
                     widget has both: a rail has nothing to complete and a path field has no rail."
                        .into(),
                ),
            },
            OptionSpec {
                name: "ui.keys.pane_prev".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<S-Tab> <Left>".into()),
                description: Some("Move back a pane in a two-pane widget.".into()),
            },
            // Where worktrees go. Host-owned rather than the git plugin's, for two reasons: it is
            // a place neosh writes on your disk, which is the host's business to answer for, and
            // the default has to be an absolute path — a plugin has no way to find your home
            // directory, and a `~` that only some readers expand is worse than no `~` at all.
            OptionSpec {
                name: "worktree.root".into(),
                ty: OptionType::Str,
                default: OptionValue::Str(default_worktree_root()),
                description: Some(
                    "Where `git.worktree.new` puts a new worktree, as `<root>/<repo>/<branch>`. \
                     A leading `~` is expanded. A relative path is inside the repository itself — \
                     `.worktrees` puts them at `<repo>/.worktrees/<branch>`, kept out of `git \
                     status` for you. Empty means beside the repository instead, in \
                     `<repo>-worktrees/`."
                        .into(),
                ),
            },
            // Where clones go. Host-owned for the two reasons `worktree.root` is, and separate
            // from it for a third: `worktree.root` puts trees at `<root>/<repo>/<branch>`, so a
            // clone into `<worktree.root>/<repo>` would land exactly on the directory that
            // repository's worktrees live *inside*. One setting for both would make cloning a
            // repository you already have worktrees of an error about a non-empty directory.
            OptionSpec {
                name: "clone.root".into(),
                ty: OptionType::Str,
                default: OptionValue::Str(default_clone_root()),
                description: Some(
                    "Where `^O` clones a repository, as `<root>/<owner>/<repo>`. A leading `~` is \
                     expanded. Nested by owner so two projects called `api` from different \
                     accounts do not collide. This is the first destination offered; others you \
                     have cloned into are remembered and offered beside it."
                        .into(),
                ),
            },
            // Display settings every widget reads. Declared here rather than by whichever plugin
            // happens to load first: two plugins declaring one name is an error, and a plugin that
            // is switched off must not take the setting away from everything else — which is
            // exactly what happened when the sidebar owned these and `plugins.disabled` was used.
            // Off by default. It read as a good idea and was not: nearly every key on it is
            // already in the sidebar's own footer, two rows below, and the ones that are not are
            // on the help key. What it actually did was take a row off the transcript and press
            // the composer against the status strip.
            OptionSpec {
                name: "ui.hints".into(),
                ty: OptionType::Bool,
                default: OptionValue::Bool(false),
                description: Some(
                    "Show a shortcut row under the composer. Off by default: the sidebar's footer \
                     carries the same keys and `^Z` carries all of them, so the row mostly cost \
                     the transcript a line."
                        .into(),
                ),
            },
            // The way out of a panel that has taken the keyboard. A modal float stops global
            // bindings resolving — which is what makes `^N` over an open picker do nothing instead
            // of opening a conversation behind it — so these are named to survive that. A list
            // rather than a constant because they are keys, and every key here is the user's; empty
            // it and the only way out of a panel is the way the panel gives you.
            OptionSpec {
                name: "ui.modal_escape_keys".into(),
                ty: OptionType::List,
                default: OptionValue::List(vec!["<C-q>".into(), "<C-r>".into()]),
                description: Some(
                    "Keys that still reach the workspace while a modal panel is open. Quit and \
                     reload by default, so a panel that fails to bind a way out is never a \
                     terminal you have to kill."
                        .into(),
                ),
            },
            OptionSpec {
                name: "ui.ascii_only".into(),
                ty: OptionType::Bool,
                default: OptionValue::Bool(false),
                description: Some(
                    "Use ASCII for glyphs, spinners and provider marks, for terminals without a \
                     decent font."
                        .into(),
                ),
            },
            OptionSpec {
                name: "ui.images".into(),
                ty: OptionType::Bool,
                default: OptionValue::Bool(true),
                description: Some(
                    "Draw pictures in the transcript — an image you attached, or one the agent \
                     read — on a terminal that can (kitty, Ghostty, WezTerm, Konsole; tmux with \
                     allow-passthrough). Elsewhere a picture is its name, `[png · shot]`, and \
                     this setting changes nothing."
                        .into(),
                ),
            },
            OptionSpec {
                name: "ui.motion".into(),
                ty: OptionType::Bool,
                default: OptionValue::Bool(true),
                description: Some(
                    "Animate spinners and status pulses. Measured at ~1% of one core and under \
                     1 KiB/s, so this is on even over SSH; turn it off if you prefer a still \
                     screen."
                        .into(),
                ),
            },
            OptionSpec {
                name: "ui.nerd_font".into(),
                ty: OptionType::Bool,
                default: OptionValue::Bool(false),
                description: Some(
                    "Use Nerd Font glyphs where there is one — provider logos, chiefly. Off by \
                     default and not detected: a terminal cannot be asked what its font contains, \
                     and guessing wrong draws a row of boxes, which reads as a broken program \
                     rather than as a missing font."
                        .into(),
                ),
            },
            // Notifications. Declared here rather than by a plugin for the same reason the display
            // settings above are: everything in the workspace reads them, and a setting that
            // disappears when `plugins.disabled` names its owner is not a setting.
            OptionSpec {
                name: "notify.enabled".into(),
                ty: OptionType::Bool,
                default: OptionValue::Bool(true),
                description: Some(
                    "Tell you about things you did not ask for and cannot see. Off leaves the \
                     sidebar's own marks, which are what remember it either way."
                        .into(),
                ),
            },
            OptionSpec {
                name: "notify.when".into(),
                ty: OptionType::List,
                default: OptionValue::List(
                    crate::notify::AlertReason::all().iter().map(|r| r.key().into()).collect(),
                ),
                description: Some(
                    "What is worth interrupting you for: turn.done, question, permission, \
                     failure, quota, plugin. A list rather than six switches, because it is one \
                     decision asked six times."
                        .into(),
                ),
            },
            OptionSpec {
                name: "notify.away".into(),
                ty: OptionType::Enum {
                    values: vec!["focus".into(), "offscreen".into(), "never".into()],
                },
                default: OptionValue::Str("focus".into()),
                description: Some(
                    "When you count as not looking. `focus` is when no terminal has it — the \
                     honest answer; `offscreen` is any conversation that is not the one on screen, \
                     for a tiled layout you keep neosh visible in and do not watch."
                        .into(),
                ),
            },
            OptionSpec {
                name: "notify.desktop".into(),
                ty: OptionType::Enum {
                    values: vec!["when-detached".into(), "always".into(), "never".into()],
                },
                default: OptionValue::Str("when-detached".into()),
                description: Some(
                    "When to use the machine's own notifications rather than the terminal's. \
                     Detached only, by default: with a terminal attached the escape reaches the \
                     machine you are sitting at, and this one reaches the machine neosh is on — \
                     which over SSH is not the same computer."
                        .into(),
                ),
            },
            OptionSpec {
                name: "notify.idle_after".into(),
                ty: OptionType::Int { min: Some(5), max: Some(3600) },
                default: OptionValue::Int(60),
                description: Some(
                    "Seconds without a keystroke that count as away, on a terminal that cannot \
                     report whether it has focus. Ignored by one that can."
                        .into(),
                ),
            },
            OptionSpec {
                name: "notify.min_turn".into(),
                ty: OptionType::Int { min: Some(0), max: Some(3600) },
                default: OptionValue::Int(10),
                description: Some(
                    "The shortest turn worth telling you finished. A three-second turn ended while \
                     you were still looking at the key that started it."
                        .into(),
                ),
            },
            OptionSpec {
                name: "notify.osc".into(),
                ty: OptionType::Enum {
                    values: vec!["auto".into(), "9".into(), "777".into(), "99".into(), "off".into()],
                },
                default: OptionValue::Str("auto".into()),
                description: Some(
                    "Which escape your terminal raises a notification with. Guessed from the \
                     environment, which is sometimes wrong because a multiplexer lies about $TERM. \
                     Only reaches a terminal this process owns — set NEOSH_NOTIFY_OSC where the \
                     terminal is when you are attached over a socket."
                        .into(),
                ),
            },
            OptionSpec {
                name: "notify.bell".into(),
                ty: OptionType::Bool,
                default: OptionValue::Bool(false),
                description: Some(
                    "Ring the terminal bell as well. Off by default — a bell says something \
                     happened and never which thing — but many terminals turn one into a mark on \
                     the tab, which is the whole point."
                        .into(),
                ),
            },
            OptionSpec {
                name: "ui.confirm_destructive".into(),
                ty: OptionType::Bool,
                default: OptionValue::Bool(true),
                description: Some(
                    "Ask before anything that cannot be undone — deleting a conversation, removing \
                     a worktree. Turn it off once the keys are in your fingers; nothing that only \
                     changes what is on screen asks in the first place."
                        .into(),
                ),
            },
            OptionSpec {
                name: "ui.theme".into(),
                ty: OptionType::Enum { values: vec!["dark".into(), "light".into()] },
                default: OptionValue::Str("dark".into()),
                description: Some(
                    "Colour theme. Not auto-detected: no terminal reliably reports whether its \
                     background is light or dark, and guessing is wrong half the time."
                        .into(),
                ),
            },
            OptionSpec {
                name: "plugins.disabled".into(),
                ty: OptionType::List,
                default: OptionValue::List(Vec::new()),
                description: Some(
                    "Names of bundled plugins not to load, e.g. [\"sidebar\"]. Yours is not a \
                     bundled plugin; remove it from the plugin directory instead."
                        .into(),
                ),
            },
            OptionSpec {
                name: "gen.model".into(),
                ty: OptionType::Str,
                default: OptionValue::Str(String::new()),
                description: Some(
                    "Model for one-shot generation — branch names, commit messages, titles — as                      `instance/model`. Empty uses the conversation's model. Point it at something                      cheap; naming a branch does not need a frontier model."
                        .into(),
                ),
            },
        ];
        let plugin = PluginId::from(BUILTIN);
        for spec in specs {
            let name = spec.name.clone();
            if let Err(e) = self.editor.apply(&plugin, ApiCall::OptDeclare { spec }) {
                // Only reachable if two built-ins share a name, which is a bug here, not a user
                // error — but it must not be silent.
                tracing::error!("built-in option {name} could not be declared: {e:?}");
            }
        }
    }

    /// Set an option now, or hold it until whoever declares it has loaded.
    fn set_option_or_defer(&mut self, name: String, value: OptionValue) {
        if self.editor.options().get(&name).is_none() {
            self.startup.deferred.push((name, value));
            return;
        }
        let r = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::OptSet {
            name: name.clone(),
            value,
        });
        match r {
            Ok(_) => self.drain_effects(),
            Err(e) => self.editor_message(MessageLevel::Error, describe_api_error(&name, &e)),
        }
    }

    /// Retry held option values now that another plugin has declared its own.
    fn retry_deferred_options(&mut self) {
        let pending = std::mem::take(&mut self.startup.deferred);
        for (name, value) in pending {
            self.set_option_or_defer(name, value);
        }
    }

    /// Load the next config script, or move on to plugins when there are none left.
    fn next_config_script(&mut self) {
        match self.startup.scripts.pop_front() {
            Some((id, path)) => {
                self.startup.active = Some(id.clone());
                // The user's own configuration: the last word on every key, command and colour,
                // even though it is the first thing to load.
                self.editor.mark_user(&id);
                if !self.load_script(&id, &path) {
                    self.startup.active = None;
                    self.next_config_script();
                }
            }
            None => self.discover_plugins(),
        }
    }

    /// One `Loaded` arrived. Advance whichever phase it belongs to.
    fn advance_startup(&mut self, plugin: &PluginId) {
        if self.startup.active.as_ref() == Some(plugin) {
            self.startup.active = None;
            // A config script may have declared options of its own.
            self.retry_deferred_options();
            self.next_config_script();
            return;
        }
        if self.startup.loading > 0 {
            self.startup.loading -= 1;
            self.retry_deferred_options();
            if self.startup.loading == 0 {
                self.retry_pending_model();
                self.report_unknown_options();
                self.run_held_commands();
                // Before a queued reload, which tears all of this down and builds it again — and
                // says so a second time when *it* settles.
                self.announce_ready();
                self.run_pending_reload();
            }
        }
    }

    /// Whether anything is still being loaded — a config script or a plugin.
    fn startup_in_flight(&self) -> bool {
        self.startup.active.is_some() || self.startup.loading > 0 || !self.startup.discovered
    }

    /// Run the commands that arrived before anything had registered them.
    ///
    /// In the order they were asked for, which is the only order that can be right: two of them
    /// are a person pressing two keys.
    fn run_held_commands(&mut self) {
        for (name, args) in std::mem::take(&mut self.startup.commands) {
            self.editor.exec_command(&name, args);
            self.drain_effects();
        }
    }

    /// Say that the workspace is up.
    ///
    /// A plugin's own `activate` returning is not this: the other plugins are still loading
    /// alongside it, so a key `^E` is bound to is a key nothing has bound yet, a contribution
    /// point another plugin fills is still empty, and an `agent.model` naming a provider nobody
    /// has registered is still held. Anything that depends on the *rest* of the workspace waits
    /// for `neosh.ready` instead of guessing.
    ///
    /// An ordinary event on the ordinary bus rather than a new wire type — a plugin listens for it
    /// with the same `neosh.event.on` it uses for anything else, and `from` is `neosh` because the
    /// host is what said it. It is announced again after a reload, which is the same fact about the
    /// same workspace being true a second time.
    fn announce_ready(&mut self) {
        self.report_unread_points();
        self.broadcast(PluginEvent::Event {
            name: READY_EVENT.to_string(),
            data: None,
            from: BUILTIN.to_string(),
        });
        // Taken rather than read: this fires again when a late plugin settles, and asking the same
        // question twice would be two turns for one command line.
        if let Some(text) = self.startup.first_prompt.take() {
            self.start_turn(text);
        }
    }

    /// Apply a held `agent.model` the moment something can serve it.
    ///
    /// Not a replacement for [`Self::retry_pending_model`], which is where a spec that nothing
    /// *ever* serves is finally reported and where the fallback is chosen. This one only ever
    /// succeeds: it is called on every provider registration, and a spec that still resolves to
    /// nothing is left held for the end of startup exactly as before.
    fn apply_pending_model_if_servable(&mut self) {
        let Some(spec) = self.startup.pending_model.clone() else { return };
        // The read guard is released at the end of this statement, before `apply_option` takes
        // `&mut self`.
        let servable = Config::parse_model(&spec).is_some_and(|(instance, model)| {
            self.agent
                .providers()
                .resolve(&neosh_proto::ModelSelection {
                    instance: instance.into(),
                    model: neosh_proto::ModelId::from(model),
                    options: Vec::new(),
                })
                .is_some()
        });
        if servable {
            self.apply_option("agent.model", &OptionValue::Str(spec));
        }
    }

    /// Re-apply an `agent.model` that named an instance a plugin had not registered yet, then say
    /// so if there is still nothing to talk to.
    fn retry_pending_model(&mut self) {
        if let Some(spec) = self.startup.pending_model.take() {
            self.apply_option("agent.model", &OptionValue::Str(spec));
        }
        if self.agent.selection().is_none() {
            // A plugin may have registered the first usable instance a moment ago. The read guard
            // is released before anything takes `&mut self`.
            let fallback = self.agent.providers().default_selection();
            match fallback {
                Some(s) => {
                    self.select_model(s);
                    self.refresh_status();
                }
                None => self.editor_message(
                    MessageLevel::Warn,
                    "no usable model. Set `agent.model`, or run `neosh --list-models`.",
                ),
            }
        }
        // Last, because it can only judge a selection once every driver that might serve it has
        // registered — a plugin provider arriving late is exactly the case that would otherwise be
        // mistaken for "this model does not work".
        self.ensure_usable_selection();
        self.draw_welcome();
        self.refresh_composer();
    }

    /// Run a reload that arrived while startup was still in flight.
    fn run_pending_reload(&mut self) {
        if std::mem::take(&mut self.startup.reload_pending) {
            self.reload_config();
        }
    }

    /// Anything still deferred once every plugin has loaded names an option that does not exist.
    fn report_unknown_options(&mut self) {
        let leftover = std::mem::take(&mut self.startup.deferred);
        for (name, value) in leftover {
            self.editor_message(
                MessageLevel::Warn,
                format!(
                    "option {name:?} = {} is not declared by anything. Is the plugin that \
                     provides it installed?",
                    value.display()
                ),
            );
        }
    }

    /// Config has had its say; apply the flags that outrank it and load plugins.
    fn discover_plugins(&mut self) {
        self.startup.discovered = true;

        // Last, so `--model` beats every file. Nothing is more annoying than a flag that lost.
        if let Some(m) = self.startup.cli_model.take() {
            self.set_option_or_defer("agent.model".into(), OptionValue::Str(m));
        }
        if self.agent.selection().is_none() {
            // The read guard is released before anything takes `&mut self`.
            let first = self.agent.providers().default_selection();
            if let Some(s) = first {
                self.select_model(s);
            }
        }
        // The "no usable model" warning waits until plugins have loaded: a session whose only
        // provider comes from a plugin has nothing to select at this point, and warning here would
        // be a false alarm the user cannot act on.

        // Bundled plugins load first, so a user plugin registering the same command or key wins by
        // loading after. Disabling one is `plugins.disabled = ["sidebar"]` rather than deleting a
        // file the user does not own — and it applies to every plugin, not only the bundled ones,
        // because "remove it from the directory" is not an answer for a plugin `neosh plugin add`
        // put there.
        let disabled: Vec<String> = self
            .editor
            .options()
            .get("plugins.disabled")
            .and_then(|v| match v {
                OptionValue::List(items) => Some(items.clone()),
                _ => None,
            })
            .unwrap_or_default();

        // Everything that could load, in the default order, before anything does: a plugin's
        // `requires` is about another plugin that may sit in a later directory.
        struct Found {
            id: PluginId,
            url: String,
            manifest: neosh_proto::PluginManifest,
            bundled: bool,
        }
        let mut found: Vec<Found> = Vec::new();
        let builtins =
            if self.startup.no_builtin_plugins { Vec::new() } else { neosh_script::builtin_plugins() };
        for builtin in builtins {
            if disabled.iter().any(|d| d == &builtin.name) {
                tracing::info!(plugin = %builtin.name, "bundled plugin disabled by config");
                continue;
            }
            match toml::from_str::<neosh_proto::PluginManifest>(&builtin.manifest) {
                Ok(manifest) => found.push(Found {
                    id: PluginId(builtin.name.clone()),
                    url: builtin.url.clone(),
                    manifest,
                    bundled: true,
                }),
                Err(e) => {
                    // A bundled manifest that does not parse is a build mistake. Skip it rather
                    // than loading code whose declared capabilities are unknown.
                    tracing::error!(plugin = %builtin.name, "bundled manifest is invalid: {e}");
                }
            }
        }

        let dirs = std::mem::take(&mut self.startup.plugin_dirs);
        let mut seen = std::collections::HashSet::new();
        for dir in dirs {
            if !seen.insert(dir.clone()) {
                continue;
            }
            let (plugins, errors) = neosh_script::discover(&dir);
            for e in errors {
                tracing::error!("plugin discovery: {e}");
                self.editor_message(MessageLevel::Error, e.to_string());
            }
            for plugin in plugins {
                if disabled.iter().any(|d| d == &plugin.id.0) {
                    tracing::info!(plugin = %plugin.id, "plugin disabled by config");
                    continue;
                }
                if found.iter().any(|f| f.id == plugin.id) {
                    // Two directories, one name: the second would share the first's registry
                    // bucket and be torn down with it. The first found wins, and the second is
                    // said out loud rather than silently merged.
                    let msg = format!(
                        "plugin {} in {} is already loaded from elsewhere; skipping it",
                        plugin.id,
                        plugin.dir.display()
                    );
                    tracing::warn!("{msg}");
                    self.editor_message(MessageLevel::Warn, msg);
                    continue;
                }
                let Some(url) = self.script_url(&plugin.entry) else { continue };
                found.push(Found { id: plugin.id, url, manifest: plugin.manifest, bundled: false });
            }
        }

        let candidates: Vec<neosh_script::Candidate> = found
            .iter()
            .map(|f| neosh_script::Candidate {
                name: f.id.0.clone(),
                requires: f.manifest.requires.clone(),
                after: f.manifest.after.clone(),
            })
            .collect();
        let (order, refused) = neosh_script::order(&candidates);
        self.plugin_infos.clear();
        for f in &found {
            self.plugin_infos.insert(f.id.0.clone(), neosh_proto::PluginInfo {
                name: f.id.0.clone(),
                manifest: f.manifest.clone(),
                bundled: f.bundled,
                state: "loaded".into(),
                error: None,
            });
        }
        for r in refused {
            tracing::error!("{r}");
            self.editor_message(MessageLevel::Error, r.to_string());
            let name = match &r {
                neosh_script::Refused::MissingRequirement { plugin, .. }
                | neosh_script::Refused::Cycle { plugin } => plugin.clone(),
            };
            if let Some(info) = self.plugin_infos.get_mut(&name) {
                info.state = "failed".into();
                info.error = Some(r.to_string());
            }
        }
        // A plugin something eager requires is eager too: holding it would hold the other.
        let eager_needs: std::collections::HashSet<String> = found
            .iter()
            .filter(|f| f.manifest.activation.is_empty())
            .flat_map(|f| f.manifest.requires.iter().cloned())
            .collect();
        for i in order {
            let f = &found[i];
            let id = f.id.clone();
            if !f.manifest.activation.is_empty() && !eager_needs.contains(&id.0) {
                self.hold_plugin(id, Lazy { url: f.url.clone(), manifest: f.manifest.clone(), bundled: f.bundled });
                continue;
            }
            if f.bundled {
                // Its keymaps are defaults from here on: they will not take a key something else
                // has already bound, which is the only way `init.ts` running first can also mean
                // it wins.
                self.editor.mark_bundled(&id);
            }
            self.plugin_permissions.insert(id.clone(), f.manifest.permissions.clone());
            self.plugin_provides.insert(id.clone(), f.manifest.provides.clone());
            let requires = f.manifest.requires.clone();
            let url = f.url.clone();
            if self.load_url(&id, &url, requires) {
                self.startup.loading += 1;
            }
        }

        if self.startup.loading == 0 {
            self.retry_pending_model();
            self.report_unknown_options();
            self.run_held_commands();
            self.announce_ready();
            // Nothing to wait for — a reload queued during config activation runs now.
            self.run_pending_reload();
        }
        self.refresh_status();
    }

    /// Ask the runtime to import and activate one entry module.
    ///
    /// A failure here is reported and skipped: one broken plugin — or one broken config script —
    /// must not stop the editor from starting.
    ///
    /// The plugin joins the broadcast list *here*, when its load is asked for, rather than when
    /// its `Loaded` comes back. Those are not the same moment: `activate` runs — and registers
    /// listeners — before the host is told it finished, and several plugins activate in the gap.
    /// Anything broadcast in that window used to be delivered to whoever happened to have been
    /// confirmed already, which made the answer depend on how many plugins were installed and in
    /// what order they finished. It cost a held `[options]` value its `option_changed`, and the
    /// only reason it had never been seen is that the plugin declaring one was usually last.
    fn load_script(&mut self, id: &PluginId, entry: &std::path::Path) -> bool {
        let Some(url) = self.script_url(entry) else { return false };
        self.load_url(id, &url, Vec::new())
    }

    /// The URL an entry file loads from, with this generation's cache-busting query on it.
    ///
    /// On a reload, the generation makes this a URL deno_core has not cached, so the file is read
    /// again. The loader strips it before touching the filesystem and propagates it to relative
    /// imports.
    fn script_url(&mut self, entry: &std::path::Path) -> Option<String> {
        let Ok(mut url) = url::Url::from_file_path(entry) else {
            self.editor_message(
                MessageLevel::Error,
                format!("{}: not an absolute path", entry.display()),
            );
            return None;
        };
        if self.startup.generation > 0 {
            url.set_query(Some(&format!("v={}", self.startup.generation)));
        }
        Some(url.into())
    }

    /// Load an entry module by URL, whether it lives on disk or inside the binary.
    fn load_url(&mut self, id: &PluginId, url: &str, requires: Vec<String>) -> bool {
        let url = if self.startup.generation > 0 && !url.contains("?v=") {
            format!("{url}?v={}", self.startup.generation)
        } else {
            url.to_string()
        };
        match self.bridge.script().send(ScriptInbound::Load {
            plugin: id.clone(),
            url,
            config: serde_json::Value::Object(Default::default()),
            version: neosh_proto::PROTOCOL_VERSION,
            requires,
        }) {
            Ok(()) => {
                // Reachable from here, not from its `Loaded`. See [`Self::load_script`].
                self.bridge.register_loaded(id.clone());
                true
            }
            Err(e) => {
                tracing::error!(plugin = %id, "{e}");
                false
            }
        }
    }

}

/// One group of a contributed theme, in the shape `hl.define` takes: `{ link: "Name" }` or a
/// highlight spec (`{ fg, bg, bold, … }`), with the wire form (`{ kind: "link", to }`) accepted too.
fn parse_highlight(v: &serde_json::Value) -> Option<neosh_proto::HighlightDef> {
    if let Some(to) = v.get("link").and_then(|l| l.as_str()) {
        return Some(neosh_proto::HighlightDef::Link { to: to.to_string() });
    }
    if v.get("kind").is_some()
        && let Ok(def) = serde_json::from_value::<neosh_proto::HighlightDef>(v.clone())
    {
        return Some(def);
    }
    serde_json::from_value::<neosh_proto::HighlightSpec>(v.clone())
        .ok()
        .map(|spec| neosh_proto::HighlightDef::Spec { spec })
}

/// The closest of `candidates` to `name`, when it is close enough to be a typo.
fn nearest<'a>(name: &str, candidates: &'a [String]) -> Option<&'a str> {
    candidates
        .iter()
        .map(|c| (levenshtein(name, c), c.as_str()))
        .filter(|(d, _)| *d <= 3)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Turn a refused option into something a user can act on.
fn describe_api_error(name: &str, e: &ApiError) -> String {
    match e {
        ApiError::NotFound { what } | ApiError::InvalidArgument { message: what } => what.clone(),
        other => format!("option {name}: {other:?}"),
    }
}

/// Register the built-in provider drivers and instances.
///
/// A free function rather than a method: listing models must not require a frontend, and a
/// frontend must not be required to know about providers.
/// Register every driver neosh ships, and hand back the ACP ones.
///
/// They come back because they need telling when the permission mode changes: an ACP agent asks
/// its *client* for permission, and the driver has to answer synchronously with no way to reach
/// the permission layer from inside a spawned turn. Mirroring the mode into the driver is the
/// narrowest thing that works — see [`neosh_provider::drivers::acp`] for why that is a stated
/// limitation rather than a design.
pub fn install_builtin_providers(
    agent: &Agent,
) -> Vec<Arc<dyn neosh_provider::drivers::AgentDriver>> {
    let mut out: Vec<Arc<dyn neosh_provider::drivers::AgentDriver>> = Vec::new();
    {
        let mut reg = agent.providers_mut();
        reg.register_driver(Arc::new(neosh_provider::drivers::AnthropicProvider));
        reg.register_driver(Arc::new(neosh_provider::drivers::OpenAiCompatProvider));
        reg.register_driver(Arc::new(neosh_provider::drivers::GoogleProvider));
        // Only when the CLI is actually there. It needs no key, so it is first in the catalogue
        // and would otherwise be what a fresh install defaults to — and every turn would fail with
        // "no such file", on the one path that is supposed to work with no setup at all. The
        // instance stays configured either way, so a settings list can say the driver is missing
        // rather than the provider simply not existing.
        let claude = Arc::new(neosh_provider::drivers::ClaudeCliProvider::default());
        if claude.available() {
            reg.register_driver(claude.clone());
            out.push(claude);
        }
        let codex = Arc::new(neosh_provider::drivers::CodexCliProvider::default());
        if codex.available() {
            reg.register_driver(codex.clone());
            out.push(codex);
        }
        // Same bargain as the two above: its own login, so no key to paste, and only offered when
        // the CLI is actually installed.
        let antigravity = Arc::new(neosh_provider::drivers::AntigravityProvider::default());
        if antigravity.available() {
            reg.register_driver(antigravity.clone());
            out.push(antigravity);
        }
        // One driver, three programs: they all speak the Agent Client Protocol. Registered only
        // where the program exists, for the same reason as the other two — a provider whose every
        // turn fails with "no such file" is worse than one that is not offered.
        for (kind, program, args) in [
            ("cursor-cli", "cursor-agent", &["--acp"][..]),
            ("grok-cli", "grok", &["--acp"][..]),
            ("gemini-cli", "gemini", &["--experimental-acp"][..]),
        ] {
            let acp = Arc::new(neosh_provider::drivers::AcpProvider::new(kind, program, args));
            if acp.available() {
                reg.register_driver(acp.clone());
                out.push(acp);
            }
        }
        for i in catalog::builtin_instances() {
            reg.add_instance(i);
        }
    }
    out
}

/// The workspace, as somewhere a driver can say its agent started talking on its own.
///
/// The model another machine says a conversation of its is using, as a selection here.
///
/// An older machine names the model and not the provider serving it; the provider is filled in
/// from that machine's catalogue when it arrives, and until then the footer still says the model.
fn remote_selection(agent: &neosh_proto::AgentSummary) -> Option<neosh_proto::ModelSelection> {
    Some(neosh_proto::ModelSelection {
        instance: neosh_proto::InstanceId(agent.instance.clone().unwrap_or_default()),
        model: neosh_proto::ModelId(agent.model.clone()?),
        options: agent.options.clone(),
    })
}

/// A model-picker call, answered from another machine's catalogue.
fn catalogue_answer(
    catalogue: &neosh_proto::ModelCatalogue,
    call: &ApiCall,
) -> Result<ApiOk, ApiError> {
    match call {
        ApiCall::AgentListModels { instance, .. } => Ok(ApiOk::Models {
            models: catalogue
                .models
                .iter()
                .filter(|e| instance.as_ref().is_none_or(|i| &e.instance == i))
                .cloned()
                .collect(),
        }),
        ApiCall::ProviderCredentials => {
            Ok(ApiOk::Credentials { credentials: catalogue.credentials.clone() })
        }
        _ => Err(ApiError::Internal { message: "not a catalogue question".into() }),
    }
}

/// A prompt as it goes out on a conversation's stream.
fn offer_event(id: &str, offer: &crate::relay::Offer) -> neosh_proto::StreamEvent {
    match offer.clone() {
        crate::relay::Offer::Question(questions) => {
            neosh_proto::StreamEvent::Question { id: id.to_string(), questions }
        }
        crate::relay::Offer::Permission { title, capability, options } => {
            neosh_proto::StreamEvent::Permission { id: id.to_string(), title, capability, options }
        }
    }
}

/// A channel rather than a call into the host: this arrives from a reader task on a driver's own
/// thread, and the host is a single writer that owns everything it would have to touch.
#[derive(Debug)]
struct UnaskedSink(tokio::sync::mpsc::UnboundedSender<Unheard>);

impl neosh_provider::drivers::Unasked for UnaskedSink {
    fn began(&self, conversation: &neosh_proto::SessionId) {
        // A closed channel is a workspace that has stopped. Nothing to report it to, and nothing
        // to be done about it.
        let _ = self.0.send(Unheard::Began(conversation.clone()));
    }

    fn background(
        &self,
        conversation: &neosh_proto::SessionId,
        tasks: Vec<neosh_proto::BackgroundTask>,
    ) {
        let _ = self.0.send(Unheard::Background(conversation.clone(), tasks));
    }
}

/// What a driver says about a conversation when no turn is running in it.
///
/// Both arrive from a reader task on the driver's own thread, and the host is a single writer that
/// owns everything either of them would have to touch — so they come down a channel rather than
/// through a call. See [`neosh_provider::drivers::Unasked`].
#[derive(Debug)]
enum Unheard {
    /// The agent has started saying something nobody asked for, and needs a turn to say it in.
    Began(neosh_proto::SessionId),
    /// Everything still running there that no turn is waiting for, whole.
    Background(neosh_proto::SessionId, Vec<neosh_proto::BackgroundTask>),
}

/// Route one slow call to its service.
///
/// Free function rather than a method: it runs on a spawned task with no access to `Host`, which
/// is exactly the property that makes it safe to run while the host loop keeps drawing.
async fn run_slow(svc: Services, call: ApiCall) -> ApiResult {
    match call {
        ApiCall::PathComplete { prefix } => svc.path_complete(prefix).await,
        ApiCall::UpdateCheck { force, local } => {
            Ok(ApiOk::Update { update: svc.updater.check(force, local).await })
        }
        ApiCall::UpdateApply => {
            // Progress goes on the bus rather than back to the caller, because the caller is not
            // necessarily the thing drawing — `neosh agent run update` asks and the sidebar's
            // plugin is what shows it — and because a call that answers once at the end is a
            // progress row spinning on nothing for the length of a `brew upgrade`.
            let bus = svc.clone();
            let progress: crate::update::Progress = std::sync::Arc::new(move |p| {
                bus.emit(
                    crate::update::UPDATE_EVENT,
                    serde_json::to_value(p).unwrap_or(serde_json::Value::Null),
                );
            });
            Ok(ApiOk::UpdateApplied { outcome: svc.updater.apply(progress).await })
        }
        ApiCall::GitStatus { cwd } => svc.git_status(cwd).await,
        ApiCall::GitBranches { include_remote, cwd } => {
            svc.git_branches(include_remote, cwd).await
        }
        ApiCall::GitWorktrees { cwd } => svc.git_worktrees(cwd).await,
        ApiCall::GitClone { url, path } => svc.git_clone(url, path).await,
        ApiCall::GitLog { limit } => svc.git_log(limit).await,
        ApiCall::GitDiff { target, stat } => svc.git_diff(target, stat).await,
        ApiCall::GitDefaultBranch => svc.git_default_branch().await,
        ApiCall::GitCreateBranch { name, from } => svc.git_create_branch(name, from).await,
        ApiCall::GitCheckout { rev } => svc.git_checkout(rev).await,
        ApiCall::GitStage { paths } => svc.git_stage(paths).await,
        ApiCall::GitUnstage { paths } => svc.git_unstage(paths).await,
        ApiCall::GitCommit { message } => svc.git_commit(message).await,
        ApiCall::GitFetch { cwd } => svc.git_fetch(cwd).await,
        ApiCall::ForgePulls { cwd } => svc.forge_pulls(cwd).await,
        ApiCall::GitPull { cwd, rebase } => svc.git_pull(cwd, rebase).await,
        ApiCall::GitAddWorktree { path, branch, create, cwd } => {
            svc.git_add_worktree(path, branch, create, cwd).await
        }
        ApiCall::GitRemoveWorktree { path, force, cwd } => {
            svc.git_remove_worktree(path, force, cwd).await
        }
        ApiCall::GenComplete { prompt, system, json, field, selection } => {
            svc.gen_complete(prompt, system, json, field, selection).await
        }
        // Off the host loop for the same reason as git: discovery is a network round trip per
        // configured provider, and doing it inline freezes typing for as long as it takes.
        ApiCall::AgentListModels { instance, refresh } => svc.list_models(instance, refresh).await,
        ApiCall::PermissionCheck { capability } => svc.permission_check(capability).await,
        ApiCall::AskUser { questions } => svc.ask_user(questions).await,
        ApiCall::UsageHistory { since, until, resolution, time_zone } => {
            svc.usage_history(since, until, resolution, time_zone).await
        }
        // Off the loop for the same reason git is, and for a reason of its own: the number of
        // files it reads is exactly the number of conversations somebody has never thrown away.
        ApiCall::SessionsStored => svc.sessions_stored().await,
        other => Err(ApiError::Internal {
            message: format!("{other:?} is not a background call"),
        }),
    }
}

/// Seconds since the epoch, or 0 if the clock is before it.
///
/// Zero rather than a panic: a wrong timestamp sorts a list oddly, and refusing to create a session
/// because the system clock is unset would be a much worse trade.
/// How many lines a tool wrote, as the card counts them.
///
/// Trailing blank lines are not lines anybody read. A `Read` whose output ends with a newline —
/// which almost every file does — would otherwise be reported as one line longer than it is, and a
/// card that is out by one about something this easy to check is a card nothing else on it is
/// believed.
fn lines_in(content: &str) -> usize {
    let body = content.trim_end();
    if body.is_empty() { 0 } else { body.lines().count() }
}

/// A refusal, in words a person can act on.
fn refusal_text(r: &neosh_proto::Refusal) -> String {
    use neosh_proto::Refusal as R;
    match r {
        R::NoSuchAgent => "that conversation is not there any more".into(),
        R::NotPermitted { what } => what.clone(),
        R::Busy { message } | R::Failed { message } => message.clone(),
    }
}

/// What to call this machine when nobody has said.
///
/// The hostname, which is what a person would answer if asked which computer they were at — and it
/// is asked *of the operating system* rather than of the environment, which is the whole point of
/// this function existing in this shape.
///
/// `HOSTNAME` is not an exported variable on any platform worth relying on. bash sets it and does
/// not export it, zsh does not set it at all (it has `HOST`), and the workspace is a daemon that
/// gets a thinner environment than any of them. So the environment answered "no" on every Mac and
/// most Linux boxes, every node fell through to the literal fallback, and every machine in the
/// swarm was called `neosh` — in a panel whose entire job is telling machines apart.
///
/// The environment is still consulted *first*, because a container that sets `HOSTNAME` to
/// something meaningful is deliberately overriding a kernel hostname that is a random hex string.
fn machine_name() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .or_else(|| hostname::get().ok().map(|h| h.to_string_lossy().into_owned()))
        // `mini.local`, `build.tail1234.ts.net` — the first label is the name, and the rest is the
        // network's business. A row in a list has one line for this.
        .map(|h| h.split('.').next().unwrap_or(&h).to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "neosh".to_string())
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Render a conversation as chat-buffer lines.
///
/// Deliberately the *same* shape the live stream produces, so switching away and back does not
/// change what a turn looks like.
fn transcript(
    session: &neosh_agent::Session,
    ascii: bool,
    limits: cards::Limits,
    width: usize,
    show_tools: bool,
    running: bool,
    offset: Option<i64>,
) -> (Vec<String>, Vec<Mark>, Vec<Card>, Vec<TurnTime>) {
    use neosh_proto::{ContentBlock, Role};

    let g = Glyphs::new(ascii);
    let root = session.cwd.as_path();
    // Which calls have an answer, whether it was a failure, what it exited with, and how much it
    // came back with. Gathered first because a result always comes *after* the call it answers, and
    // a dot that only turns green on the second pass over the buffer is a dot that flickers on
    // every switch. The other three are here for the same reason: they all belong on the header,
    // which is written before the result carrying them has been reached.
    type Answer = (bool, Option<i64>, usize);
    let answered: std::collections::HashMap<&neosh_proto::ToolCallId, Answer> = session
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, is_error, content, .. } => Some((
                tool_use_id,
                (
                    *is_error,
                    is_error.then(|| cards::exit_code(content)).flatten(),
                    lines_in(content),
                ),
            )),
            _ => None,
        })
        .collect();
    // When each turn was asked and when the last thing in it landed.
    //
    // There is no turn record in a conversation's messages and there does not need to be: a
    // question is what a turn is *for*, so a turn starts where somebody said something and ends
    // with the last thing that arrived before the next question. Tool results are user-role
    // messages and open nothing — what makes a message a question is a block somebody typed.
    let opens = |m: &neosh_proto::Message| {
        m.role == Role::User
            && m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { .. } | ContentBlock::Image { .. }))
    };
    let mut spans: Vec<(Option<i64>, Option<i64>)> = Vec::new();
    for m in &session.messages {
        if opens(m) {
            spans.push((m.at, None));
        } else if let (Some(last), Some(at)) = (spans.last_mut(), m.at) {
            last.1 = Some(at);
        }
    }
    // A turn that is still going has not taken any length of time yet. Its working line is
    // counting up a few rows below, which is the honest place for a number that is still moving.
    if running {
        if let Some(last) = spans.last_mut() {
            last.1 = None;
        }
    }
    let mut times: Vec<TurnTime> = Vec::new();
    let mut turn = 0usize;
    // The turn before the one being drawn, for deciding whether a margin has to lead with a date.
    let mut before: Option<i64> = None;
    let mut lines: Vec<String> = Vec::new();
    let mut marks: Vec<Mark> = Vec::new();
    let mut cards: Vec<Card> = Vec::new();
    // What the turn being read has changed, for the summary that closes it.
    let mut changes: Vec<cards::FileStat> = Vec::new();

    // Rows go in at `at` — under a card's header, usually — and everything below moves down, the
    // marks and the cards with it. The same shape the live path has, for the same reason: an agent
    // runs calls in parallel, and a result appended at the end lands under the wrong card.
    fn insert(
        lines: &mut Vec<String>,
        marks: &mut Vec<Mark>,
        cards: &mut [Card],
        at: u32,
        rows: Vec<cards::Row>,
    ) {
        let n = rows.len() as u32;
        if n == 0 {
            return;
        }
        for m in marks.iter_mut().filter(|m| m.row >= at) {
            m.row += n;
        }
        for c in cards.iter_mut().filter(|c| c.row >= at) {
            c.row += n;
        }
        for (k, r) in rows.into_iter().enumerate() {
            let row = at + k as u32;
            marks.extend(Mark::of(row, &r));
            lines.insert(row as usize, r.text);
        }
    }
    // What is under card `i`, replaced. A card acquires and loses a body while the conversation is
    // being read — a stack of commands shows the last one's output, so every command that joins it
    // takes the previous one's rows back off the screen — and the rows below it, the marks and the
    // other cards all move with it.
    #[allow(clippy::items_after_statements)]
    fn set_body(
        lines: &mut Vec<String>,
        marks: &mut Vec<Mark>,
        cards: &mut Vec<Card>,
        i: usize,
        rows: Vec<cards::Row>,
    ) {
        let at = cards[i].row + 1;
        let old = cards[i].body;
        if old > 0 {
            lines.drain(at as usize..(at + old) as usize);
            marks.retain(|m| m.row < at || m.row >= at + old);
            for m in marks.iter_mut().filter(|m| m.row >= at + old) {
                m.row -= old;
            }
            for c in cards.iter_mut().filter(|c| c.row >= at + old) {
                c.row -= old;
            }
            cards[i].body = 0;
        }
        let n = rows.len() as u32;
        insert(lines, marks, cards, at, rows);
        cards[i].body = n;
    }
    // A card's header, from what the conversation says about every call on it.
    //
    // The results are read out of `answered` rather than off the legs: a result block always comes
    // *after* the call it answers, and a header that only settled on the second pass would show
    // every finished call as running for one frame of every switch.
    fn header_of(
        g: &Glyphs,
        card: &Card,
        answered: &std::collections::HashMap<&neosh_proto::ToolCallId, (bool, Option<i64>, usize)>,
        root: &std::path::Path,
        width: usize,
    ) -> cards::Row {
        let heads = heads_of(card, answered);
        match heads.as_slice() {
            [one] => cards::header(g, one, root, width),
            many => cards::group_header(g, many, root, width),
        }
    }
    // Every call on a card, as the renderer wants them. The live path's `card_heads`, for a card
    // built out of the conversation rather than watched happening.
    #[allow(clippy::items_after_statements)]
    fn heads_of<'a>(
        card: &'a Card,
        answered: &std::collections::HashMap<&neosh_proto::ToolCallId, (bool, Option<i64>, usize)>,
    ) -> Vec<cards::Head<'a>> {
        card
            .legs
            .iter()
            .map(|leg| {
                let (state, exit, output) = match answered.get(&leg.id) {
                    Some((true, exit, n)) => (ToolState::Failed, *exit, Some(*n)),
                    Some((false, _, n)) => (ToolState::Done, None, Some(*n)),
                    None => (ToolState::Running, None, None),
                };
                cards::Head {
                    name: &leg.name,
                    input: &leg.input,
                    state,
                    took: None,
                    exit,
                    output,
                    result: leg.result.as_ref(),
                }
            })
            .collect()
    }
    // What goes under a card, folded, from what the conversation says: one call's answer, or a
    // stack's. The live path's [`Host::redraw_card`] arm, asked the same way — the two have to
    // produce the same rows, or switching away and back re-draws the conversation.
    #[allow(clippy::items_after_statements)]
    fn body_of(
        g: &Glyphs,
        card: &Card,
        answered: &std::collections::HashMap<&neosh_proto::ToolCallId, (bool, Option<i64>, usize)>,
        root: &std::path::Path,
        limits: cards::Limits,
        width: usize,
    ) -> Vec<cards::Row> {
        match card.legs.as_slice() {
            [leg] => match &leg.result {
                Some(r) => cards::body(g, &leg.input, r, limits, card.open, width),
                None => Vec::new(),
            },
            _ => cards::group_body(g, &heads_of(card, answered), root, limits, card.open, width),
        }
    }
    // A gap, unless there already is one: what came before was often a tool card, which has no
    // trailing blank of its own, and the next thing butted against it reads as part of it.
    #[allow(clippy::items_after_statements)]
    fn gap(lines: &mut Vec<String>) {
        if lines.last().is_some_and(|l| !l.trim().is_empty()) {
            lines.push(String::new());
        }
    }
    let summarise = |lines: &mut Vec<String>, marks: &mut Vec<Mark>, changes: &mut Vec<cards::FileStat>| {
        let rows = cards::summary(changes, root, width);
        changes.clear();
        if rows.is_empty() {
            return;
        }
        gap(lines);
        for r in rows {
            let row = lines.len() as u32;
            marks.extend(Mark::of(row, &r));
            lines.push(r.text);
        }
    };

    // Whether the row just written was an attached image, which is the one case where the text
    // under it must *not* get a blank line of its own: the picture and the sentence are one
    // question, and a gap between them reads as two.
    let mut after_image = false;
    // Which message the margin above has already been drawn for, so a question made of a picture
    // and a sentence — two blocks, one question — is one turn rather than two.
    let mut timed: Option<usize> = None;
    // The margin for the turn a question opens, on the blank row above it wherever there is one:
    // an empty line always has room for something flush right, and a question that opens the
    // transcript has no row above to use.
    let open_turn = |lines: &mut Vec<String>,
                         times: &mut Vec<TurnTime>,
                         timed: &mut Option<usize>,
                         before: &mut Option<i64>,
                         turn: &mut usize,
                         mi: usize| {
        if *timed == Some(mi) {
            return;
        }
        *timed = Some(mi);
        let here = *turn;
        *turn += 1;
        let Some(offset) = offset else { return };
        let Some((Some(asked), ended)) = spans.get(here).copied() else { return };
        // A margin goes on the blank line above its question, and the first turn of a rebuilt
        // conversation has none — `gap` has nothing to separate it from. One is made rather than
        // the margin falling onto the question's own row: a right-aligned annotation there is
        // fine until the question is longer than the window, and a first turn that reads
        // differently from every turn under it is one more thing to learn.
        if lines.is_empty() {
            lines.push(String::new());
        }
        let row = if lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.len() as u32 - 1
        } else {
            lines.len() as u32
        };
        times.push(TurnTime {
            row,
            stamp: crate::clock::Stamp {
                asked,
                ended,
                dated: before.is_none_or(|b| {
                    crate::clock::day(b, offset) != crate::clock::day(asked, offset)
                }),
                // Against *now* when there is nothing above to differ from: a conversation from
                // this year needs no year on it, and one from two years ago is exactly where
                // saying so matters. The comparison is re-made every time the transcript is
                // rebuilt, which is the only moment this row is written, so it cannot go stale
                // where it is read.
                year: crate::clock::year(before.unwrap_or_else(now_secs), offset)
                    != crate::clock::year(asked, offset),
            },
        });
        *before = Some(asked);
    };
    for (mi, message) in session.messages.iter().enumerate() {
        for block in &message.content {
            let was_image = std::mem::take(&mut after_image);
            match block {
                ContentBlock::Image { path, media_type } if message.role == Role::User => {
                    // Images come first in a user message, so this is where the turn boundary is.
                    if !was_image {
                        summarise(&mut lines, &mut marks, &mut changes);
                        gap(&mut lines);
                        open_turn(&mut lines, &mut times, &mut timed, &mut before, &mut turn, mi);
                    }
                    let row = lines.len() as u32;
                    marks.push(Mark::at(row, 0, g.bar.len(), "Agent.User"));
                    marks.push(Mark::picture(row, g.bar.len() + 1, path, media_type));
                    lines.push(format!("{} {}", g.bar, cards::image_row(path, media_type)));
                    after_image = true;
                }
                ContentBlock::Text { text } if message.role == Role::User => {
                    // A question starts a turn, so the one before it is over.
                    if !was_image {
                        summarise(&mut lines, &mut marks, &mut changes);
                        gap(&mut lines);
                    }
                    open_turn(&mut lines, &mut times, &mut timed, &mut before, &mut turn, mi);
                    for line in text.lines() {
                        marks.push(Mark::at(lines.len() as u32, 0, g.bar.len(), "Agent.User"));
                        lines.push(format!("{} {line}", g.bar));
                    }
                    lines.push(String::new());
                }
                ContentBlock::Text { text } => {
                    // Through the same renderer the live stream uses, so switching away and back
                    // does not change what an answer looks like — and behind the same blank row,
                    // which `open_answer_row` puts there live and this used to leave out. Nothing
                    // showed while a card had no air of its own; the moment one did, a sentence
                    // after a card was the only thing on the screen still butted against it.
                    gap(&mut lines);
                    let at = lines.len() as u32;
                    let (rendered, styled) = crate::markdown::render(text);
                    marks.extend(
                        styled.into_iter().map(|(r, a, b, g)| Mark::at(at + r as u32, a, b, g)),
                    );
                    lines.extend(rendered);
                    lines.push(String::new());
                }
                // Gated on the same setting the live path uses. Drawn unconditionally, switching
                // conversations with tool cards off made them all appear.
                ContentBlock::ToolUse { id, name, input } if show_tools => {
                    // The mark says how it went, which the conversation already records: a call
                    // with a result somewhere after it is finished, and one without is still
                    // running. Nothing here has to remember anything.
                    let exit = match answered.get(id) {
                        Some((true, exit, _)) => *exit,
                        _ => None,
                    };
                    let leg = Leg {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                        // The result is filled in when its block is reached, which is what puts a
                        // body under the right header. The *header* cannot wait for that — it is
                        // written now, from `answered`, or a restored transcript would show every
                        // call as running until the second pass.
                        result: None,
                        // Nothing here watched the call happen, so nothing here can say how long it
                        // took. A restored transcript is a record, not a replay.
                        started: None,
                        took: None,
                        exit,
                    };
                    // Onto the run above when there is one and nothing has been drawn since — the
                    // same rule the live path uses, because the two have to produce the same rows.
                    let open = cards
                        .last()
                        .filter(|c| c.row + c.body + 1 == lines.len() as u32)
                        .is_some_and(|c| c.takes(input));
                    let at = if open {
                        let i = cards.len() - 1;
                        cards[i].legs.push(leg);
                        // A card that was one command with its output under it is now a stack of
                        // them, and a stack shows the *last* one's output — which this call has
                        // not produced yet. So its predecessor's rows come off the screen here,
                        // exactly as the live path takes them off when it redraws the card.
                        let rows = body_of(&g, &cards[i], &answered, root, limits, width);
                        set_body(&mut lines, &mut marks, &mut cards, i, rows);
                        cards[i].row
                    } else {
                        // No gap, exactly as the live path draws it: the two have to produce the
                        // same rows, or switching away and back re-spaces the conversation.
                        let at = lines.len() as u32;
                        cards.push(Card::new(leg, at));
                        lines.push(String::new());
                        at
                    };
                    let i = cards.len() - 1;
                    let row = header_of(&g, &cards[i], &answered, root, width);
                    marks.retain(|m| m.row != at);
                    marks.extend(Mark::of(at, &row));
                    lines[at as usize] = row.text;
                }
                ContentBlock::ToolUse { .. } => {}
                // Errors show even with tool cards off, exactly as they do live: the setting is
                // about noise, not about hiding failures.
                ContentBlock::ToolResult { tool_use_id, is_error, content, images }
                    if *is_error || show_tools =>
                {
                    let result = neosh_proto::ToolResult {
                        content: content.clone(),
                        is_error: *is_error,
                        images: images.clone(),
                    };
                    let found = cards
                        .iter()
                        .enumerate()
                        .rev()
                        .find_map(|(i, c)| c.leg_of(tool_use_id).map(|k| (i, k)));
                    match found {
                        // Under its own header, however many other calls came between.
                        Some((i, k)) => {
                            if !*is_error {
                                cards::tally(&mut changes, &cards::edits_of(&cards[i].legs[k].input));
                            }
                            // The answer goes on the leg first, because what a card shows is a
                            // question about the whole card: a run of reads shows none of them, a
                            // stack of commands shows the last one's, and one call shows its own.
                            cards[i].legs[k].result = Some(result);
                            let rows = body_of(&g, &cards[i], &answered, root, limits, width);
                            set_body(&mut lines, &mut marks, &mut cards, i, rows);
                        }
                        // No card to go under — cards are off and this is an error — so at the
                        // end, which is where the live path puts it too.
                        None => {
                            let rows = cards::body(
                                &g,
                                &serde_json::Value::Null,
                                &result,
                                limits,
                                cards::Open::Folded,
                                width,
                            );
                            for r in rows {
                                let row = lines.len() as u32;
                                marks.extend(Mark::of(row, &r));
                                lines.push(r.text);
                            }
                        }
                    }
                }
                ContentBlock::ToolResult { .. } => {}
                // An image on an *assistant* message, which nothing produces today. Silently
                // dropped rather than drawn: a row saying "[png]" under an answer nobody attached
                // one to would be the transcript inventing something.
                ContentBlock::Image { .. } => {}
                // Reasoning is not replayed: it is shown live when `chat.show_thinking` is on, and
                // reprinting it on every switch would bury the answer under the working out.
                ContentBlock::Thinking { .. } => {}
            }
        }
    }
    // The last turn is over too — unless it is still going, in which case its summary is drawn
    // when it ends, by whoever is looking then.
    if !running {
        summarise(&mut lines, &mut marks, &mut changes);
    }
    // Where it stopped, when nothing stopped it.
    //
    // The sidebar can say *which* conversation was cut off; only the transcript can say where. What
    // the agent had finished is above this line and is real; what it was in the middle of is not
    // anywhere, and without a line saying so the conversation simply ends — on a half-written
    // sentence, or on a tool call with no result, which reads exactly like an agent that gave up.
    //
    // Not drawn for a turn that is running: `running` is a turn in flight *now*, in this process,
    // and this mark is about one that was in flight in a process that is gone.
    if session.interrupted && !running {
        gap(&mut lines);
        let row = lines.len() as u32;
        let r = cards::Row::plain(
            format!("{} the workspace stopped while this turn was running", g.fail),
            "Diagnostic.Error",
        );
        marks.extend(Mark::of(row, &r));
        lines.push(r.text);
    }
    (lines, marks, cards, times)
}

/// One highlight in a rebuilt transcript, before it becomes an extmark.
///
/// `span` is `None` for a band — a group behind the whole row rather than behind a range of bytes
/// on it. The two travel in one list because they are written together and have to arrive together:
/// a diff row whose band was applied on a later pass would be green a frame after it was green.
#[derive(Clone)]
struct Mark {
    row: u32,
    span: Option<(usize, usize)>,
    hl: &'static str,
    /// The row is a picture from `span.0` on, and this is it. See [`Host::chat_image`].
    image: Option<neosh_proto::ImageFile>,
}

impl Mark {
    fn at(row: u32, from: usize, to: usize, hl: &'static str) -> Self {
        Self { row, span: Some((from, to)), hl, image: None }
    }

    fn picture(row: u32, col: usize, path: &str, media_type: &str) -> Self {
        Self {
            row,
            span: Some((col, col)),
            hl: "",
            image: Some(neosh_proto::ImageFile { path: path.into(), media_type: media_type.into() }),
        }
    }

    /// Every mark a card row carries, ready to be shifted onto whatever row it lands on.
    fn of(row: u32, r: &cards::Row) -> Vec<Self> {
        r.spans
            .iter()
            .map(|(a, b, h)| Self::at(row, *a, *b, h))
            .chain(r.band.map(|h| Self { row, span: None, hl: h, image: None }))
            .chain(r.image.as_ref().map(|(col, i)| Self::picture(row, *col, &i.path, &i.media_type)))
            .collect()
    }

    fn shifted(self, by: u32) -> Self {
        Self { row: self.row + by, ..self }
    }
}


/// Where one turn's margin goes, and what it is made of.
///
/// Produced by [`transcript`] while it rebuilds a conversation, because the turn boundaries are
/// something it already has to find and nothing else does. The row is where the margin is drawn:
/// the blank line above the question when there is one, which is a line with nothing on it and
/// therefore always has room, and the question's own first row when the turn opens the transcript.
struct TurnTime {
    row: u32,
    stamp: crate::clock::Stamp,
}

/// A virtual-text chunk, which is three words of ceremony often enough to be worth a name.
fn chunk(text: impl Into<String>, hl: &str) -> neosh_proto::VirtChunk {
    neosh_proto::VirtChunk { text: text.into(), hl_group: Some(hl.to_string()) }
}

/// Screen columns a string occupies.
///
/// The host almost never needs this — columns are byte offsets and width is the frontend's job —
/// but the shortcut row is laid out here, and a row measured in `char`s wraps the moment a hint
/// uses `⇧` or `⏎`.
fn display_width(s: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    s.width()
}

/// `s`, or as much of it as fits in `room` columns with an `…` saying so.
///
/// By grapheme cluster and by display width, not by `char`: a byte slice can land mid-character and
/// a `char` count is wrong about the first emoji it meets. Room for nothing but the ellipsis gives
/// the ellipsis, and a `room` of zero gives an empty string — a caller with no space left is not
/// helped by one column of something.
fn elide(s: &str, room: usize) -> String {
    if display_width(s) <= room {
        return s.to_string();
    }
    let Some(budget) = room.checked_sub(1) else { return String::new() };
    let mut out = String::new();
    let mut used = 0usize;
    for g in s.graphemes(true) {
        let w = display_width(g);
        if used + w > budget {
            break;
        }
        out.push_str(g);
        used += w;
    }
    out.push('\u{2026}');
    out
}

/// `s`, or as much of its *end* as fits in `room` columns with an `…` where the front went.
///
/// [`elide`]'s mirror, and the right one for a path: what a directory is called is the last
/// segment or two, so cutting the front of `/Users/you/src/neosh/work` leaves `…/neosh/work`
/// while cutting the back leaves `/Users/you/src/n…`, which names nothing.
fn elide_start(s: &str, room: usize) -> String {
    if display_width(s) <= room {
        return s.to_string();
    }
    let Some(budget) = room.checked_sub(1) else { return String::new() };
    let mut tail: Vec<&str> = Vec::new();
    let mut used = 0usize;
    for g in s.graphemes(true).rev() {
        let w = display_width(g);
        if used + w > budget {
            break;
        }
        tail.push(g);
        used += w;
    }
    let mut out = String::from("\u{2026}");
    out.extend(tail.into_iter().rev());
    out
}

/// What the host learned about a directory the first time it looked: the display name, and the
/// checkout facts the name was made from. Kept whole because [`SessionInfo`] carries both — the
/// name for reading, the root for grouping — and deriving one from the other means parsing a
/// display format back apart.
///
/// [`SessionInfo`]: neosh_proto::SessionInfo
struct ProjectFacts {
    name: String,
    /// The main checkout's path; `None` outside a repository.
    repo_root: Option<String>,
    /// The branch at that directory when it was first seen; `None` detached.
    branch: Option<String>,
}

/// What to call a checkout.
///
/// The last segment of the path is what everything used to use, and it is wrong for exactly the
/// case a project list exists to make sense of: a worktree is named by whoever created it, so a
/// panel reading `wt-fe3c0d93` says nothing about which repository or branch that is — and
/// somebody looking at it reasonably wonders what an unrelated tool's directory is doing in their
/// workspace.
///
/// So: a checkout is named after the repository it belongs to, and a *linked* worktree adds its
/// branch. `neosh` for the original, `neosh · fix/thing` for a worktree of it. Anything that is not
/// a repository keeps its directory name, which is all there is to go on.
fn project_name(
    dir: &std::path::Path,
    branch: Option<String>,
    main: Option<std::path::PathBuf>,
) -> String {
    let leaf = |p: &std::path::Path| {
        p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
    };
    let Some(main) = main else { return leaf(dir) };
    let repo = leaf(&main);
    // The original checkout is just the repository. Adding its branch would put a word that
    // changes on every `git switch` next to a name that never does.
    if same_dir(dir, &main) {
        return repo;
    }
    match branch {
        Some(b) => format!("{repo} \u{b7} {b}"),
        // A detached head has no name of its own; the directory somebody chose is the next best
        // thing, and better than showing a bare hash.
        None => format!("{repo} \u{b7} {}", leaf(dir)),
    }
}

/// The one row an attached image gets in a transcript.
///
/// What the API says about an attachment.
/// A URL as much of itself as fits in a line about it: the host, and the last thing in the path.
fn short_url(url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url) else { return url.to_string() };
    let host = parsed.host_str().unwrap_or("").to_string();
    match parsed.path_segments().and_then(|s| s.filter(|p| !p.is_empty()).next_back()) {
        Some(last) if last.len() <= 40 => format!("{host}/{last}"),
        _ => host,
    }
}

fn info_of(a: &crate::images::Attachment) -> neosh_proto::AttachmentInfo {
    neosh_proto::AttachmentInfo {
        path: a.path.display().to_string(),
        media_type: a.media_type.clone(),
        width: a.width,
        height: a.height,
        bytes: a.bytes.min(u32::MAX as usize) as u32,
    }
}

/// Whether two paths are the same directory, following symlinks where they resolve.
fn same_dir(a: &std::path::Path, b: &std::path::Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The other direction: `~/x` back into an absolute path.
///
/// Returns nothing when there is no home directory to substitute, which leaves the path exactly as
/// written — a literal directory called `~` is a strange thing to have, but inventing a path is
/// worse than honouring one.
fn expand_tilde(raw: &str) -> Option<String> {
    let rest = raw.strip_prefix('~')?;
    if !(rest.is_empty() || rest.starts_with('/')) {
        // `~other/x` is another user's home, which is not ours to resolve.
        return None;
    }
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    Some(format!("{}{rest}", home.to_string_lossy()))
}

/// Where worktrees go unless told otherwise.
///
/// `~/.nsh`, expanded at declare time so the default is already absolute — an option whose default
/// value is a string only some readers understand is a trap for the first plugin that reads it
/// without thinking.
fn default_worktree_root() -> String {
    expand_tilde("~/.nsh").unwrap_or_else(|| ".nsh".to_string())
}

/// Where clones go unless told otherwise.
///
/// Under `~/.nsh` like worktrees, and in a directory of its own beneath it: `worktree.root` spends
/// `<root>/<repo>/` on one repository's *branches*, so a clone written to `<root>/<repo>` would be
/// asked to create the very directory those branches live in.
fn default_clone_root() -> String {
    expand_tilde("~/.nsh/repos").unwrap_or_else(|| ".nsh/repos".to_string())
}

/// A path with the home directory written the way people say it.
///
/// Cosmetic, and worth it: the welcome block is four lines and one of them should not be an
/// absolute path wide enough to wrap on its own.
fn tilde(path: &std::path::Path) -> String {
    let text = path.display().to_string();
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => {
            let home = home.to_string_lossy().to_string();
            match text.strip_prefix(&home) {
                Some(rest) if rest.is_empty() => "~".to_string(),
                Some(rest) if rest.starts_with('/') => format!("~{rest}"),
                _ => text,
            }
        }
        _ => text,
    }
}
