//! The workspace outliving the terminal, driven as two real processes over a real socket.
//!
//! There is no way to fake this one. The thing being asserted is that a *process* survives another
//! process going away, so both of them have to exist: `neosh --serve` on one side, a socket client
//! standing in for the terminal on the other. What the client sends and receives here is exactly
//! what `crate::client` sends and receives — the same envelope, the same `InputEvent`s — with a
//! `Mirror` in place of a screen, because a screen is the one part of a terminal a test cannot
//! usefully have.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use neosh_proto::{ClientMessage, DetachReason, InputEvent, ServerMessage, UiEvent};

mod support;

/// How long any "wait until" in here is allowed to take.
///
/// Generous, because a cold workspace boots deno and discovers plugins before it binds, and CI is
/// slower than a laptop by more than the margin a tight bound would leave.
const PATIENCE: Duration = Duration::from_secs(30);

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// A sandbox with its own config, state and — the point — its own socket.
///
/// The socket path is derived from the config directory, so a test gets a workspace of its own
/// without any further arrangement. That is not incidental: if it were keyed on the user, running
/// the suite would attach to the developer's real workspace and stop it.
struct Sandbox {
    root: PathBuf,
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Stop the workspace before removing the directory it is writing into. It is a background
        // process by construction: nothing else will.
        let _ = self.neosh(&["stop"]).output();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let root = support::sandbox_root("ws", name);
        for d in ["config", "state", "work", "run"] {
            std::fs::create_dir_all(root.join(d)).expect("sandbox dirs");
        }
        Self { root }
    }

    fn neosh(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_neosh"));
        c.arg("--config-dir")
            .arg(self.root.join("config"))
            .arg("--cwd")
            .arg(self.root.join("work"))
            .args(args)
            .env("NEOSH_STATE_DIR", self.root.join("state"))
            .env("NEOSH_DATA_DIR", self.root.join("state"))
            .env("NEOSH_CACHE_DIR", self.root.join("state"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"));
        c
    }

    /// Where this sandbox's workspace listens.
    ///
    /// Recomputed here rather than asked of the binary: a test that derived the path by calling
    /// the code under test would pass just as happily if both were wrong about it.
    fn socket(&self) -> PathBuf {
        let key = fnv(self.root.join("config").as_os_str().as_encoded_bytes());
        self.root.join("run/neosh").join(format!("{key}.sock"))
    }

    fn serve(&self, script: &str) -> Workspace {
        self.serve_paced(script, 0)
    }

    /// Start a workspace and wait for it to answer.
    ///
    /// `pace_ms` slows the mock provider down, for the tests that need a turn to still be running
    /// while something happens to the terminal watching it.
    fn serve_paced(&self, script: &str, pace_ms: u64) -> Workspace {
        let child = self
            .neosh(&["--serve", "--mock-script"])
            .arg(fixture(script))
            .args(["--model", "mock/mock"])
            .env("NEOSH_MOCK_DELAY_MS", pace_ms.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn workspace");
        let socket = self.socket();
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            if UnixStream::connect(&socket).is_ok() {
                return Workspace { child, socket };
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the workspace never came up on {}", socket.display());
    }
}

/// FNV-1a, matching `paths::key_for`. Duplicated on purpose: a test that computed the path by
/// calling the code under test would pass just as happily if both were wrong.
fn fnv(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

struct Workspace {
    child: Child,
    socket: PathBuf,
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Workspace {
    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// End it the way a crash does: no shutdown, no last flush, no chance to save anything.
    ///
    /// `Child::kill` is already `SIGKILL`, so this is that plus waiting for the socket to be
    /// unusable — the process is gone before the next workspace tries to bind over it.
    fn kill_hard(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// A terminal, minus the terminal.
///
/// Dropping one has to look to the workspace exactly like a window being closed, which means the
/// socket must actually close. It does not close by itself: the reader lives on its own thread
/// with a cloned descriptor, and that clone keeps the connection open long after the struct is
/// gone — so a test that "closed the terminal" would in fact still be attached, and would then
/// assert the wrong thing about a workspace that was behaving correctly.
struct Client {
    write: UnixStream,
    lines: std::sync::mpsc::Receiver<String>,
    mirror: neosh_tui::Mirror,
    ended: Option<DetachReason>,
    /// Why an attach was turned away, when it was.
    refusal: Option<String>,
    stopped: bool,
    /// How big this terminal says it is. A real frontend resolves layout from the declarative
    /// geometry it is sent and reports what it arrived at; this reports the whole thing, which is
    /// all the host needs to know to draw a row that has to fit.
    size: (u16, u16),
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.write.shutdown(std::net::Shutdown::Both);
    }
}

impl Client {
    fn attach(socket: &Path, protocol: u32) -> Self {
        Self::attach_sized(socket, protocol, 100, 34)
    }

    fn attach_sized(socket: &Path, protocol: u32, width: u16, height: u16) -> Self {
        let stream = UnixStream::connect(socket).expect("connect");
        let read = stream.try_clone().expect("clone");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(read).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut c = Self {
            write: stream,
            lines: rx,
            mirror: neosh_tui::Mirror::new(),
            ended: None,
            refusal: None,
            stopped: false,
            size: (width, height),
        };
        c.send(ClientMessage::Attach {
            protocol_version: protocol,
            width,
            height,
            // The build this test binary is, which is not the workspace's — the workspace is the
            // real `neosh` under test. Left as the default so the two compare equal and no test
            // here has to care: an unknown stamp is not a difference. See `BuildId::same_as`.
            build: Default::default(),
            // Wherever the workspace would have put us. Asking for a conversation by name is what
            // `neosh "a prompt"` does, and no test here is that.
            session: None,
        });
        c
    }

    /// Say goodbye the way `client::run` does when the terminal it was in was closed.
    fn leave(&mut self) {
        self.send(ClientMessage::Detach);
    }

    /// Report resolved geometry, the way a frontend that has just laid itself out does.
    ///
    /// Without this the host never learns any size at all and falls back to eighty columns, so a
    /// test about width would pass or fail for reasons that have nothing to do with the terminals
    /// in it.
    fn report_viewports(&mut self) {
        let (width, height) = self.size;
        let wins: Vec<_> = self.mirror.windows.keys().copied().collect();
        for win in wins {
            self.send(ClientMessage::Input {
                event: InputEvent::ViewportChanged {
                    win,
                    width,
                    height,
                    top_line: 0,
                    rows: height as u32,
                },
            });
        }
    }

    fn send(&mut self, msg: ClientMessage) {
        let mut line = serde_json::to_vec(&msg).expect("encode");
        line.push(b'\n');
        let _ = self.write.write_all(&line);
        let _ = self.write.flush();
    }

    fn key(&mut self, text: &str) {
        for ch in text.chars() {
            self.send(ClientMessage::Input {
                event: InputEvent::Key {
                    key: neosh_proto::KeyPress {
                        code: neosh_proto::KeyCode::Char { c: ch.to_string() },
                        mods: neosh_proto::KeyMods::default(),
                    },
                },
            });
        }
    }

    /// A chord, which is a different key from the letter it is made of.
    fn ctrl(&mut self, c: char) {
        self.send(ClientMessage::Input {
            event: InputEvent::Key {
                key: neosh_proto::KeyPress {
                    code: neosh_proto::KeyCode::Char { c: c.to_string() },
                    mods: neosh_proto::KeyMods { ctrl: true, ..Default::default() },
                },
            },
        });
    }

    fn enter(&mut self) {
        self.send(ClientMessage::Input {
            event: InputEvent::Key {
                key: neosh_proto::KeyPress {
                    code: neosh_proto::KeyCode::Enter,
                    mods: neosh_proto::KeyMods::default(),
                },
            },
        });
    }

    /// Fold in whatever has arrived. Returns false once the connection is over.
    fn pump_once(&mut self) -> bool {
        while let Ok(line) = self.lines.try_recv() {
            match serde_json::from_str::<ServerMessage>(&line) {
                Ok(ServerMessage::Events { batch }) => {
                    for ev in batch {
                        if matches!(ev, UiEvent::Shutdown { .. }) {
                            self.stopped = true;
                        }
                        self.mirror.apply(ev);
                    }
                }
                Ok(ServerMessage::Detached { reason }) => self.ended = Some(reason),
                Ok(ServerMessage::Refused { reason, .. }) => {
                    self.ended = Some(DetachReason::Stopping);
                    self.refusal = Some(reason);
                }
                _ => {}
            }
        }
        self.ended.is_none() && !self.stopped
    }

    /// Pump until `f` is happy, or give up.
    fn until(&mut self, what: &str, f: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            self.pump_once();
            if f(self) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for {what}:\n{:#?}", self.text());
    }

    /// Every line of every buffer, which is what "is it on screen" means here.
    fn text(&self) -> Vec<String> {
        self.mirror
            .buffers
            .values()
            .flat_map(|b| b.lines.iter().map(|l| l.text.clone()))
            .collect()
    }

    fn shows(&self, needle: &str) -> bool {
        self.text().iter().any(|l| l.contains(needle))
    }

    /// Whether a buffer whose name contains `name` is actually *on screen* here.
    ///
    /// Not the same question as [`Self::shows`], which reads buffer contents: a panel that has been
    /// hidden still has its buffer, and a buffer with no window is a buffer nothing draws.
    fn windowed(&self, name: &str) -> bool {
        self.mirror.windows.values().any(|w| {
            self.mirror.buffers.get(&w.buf).is_some_and(|b| b.name.contains(name))
        })
    }

    /// Wait until the workspace is actually ready to be typed at.
    ///
    /// Not the welcome screen, which is drawn before the plugins are up — the sidebar's heading,
    /// which only exists once a plugin has activated and drawn. A key sent before that lands
    /// nowhere, and the test that sent it fails with "no turn ran" a long way from the reason.
    fn ready(&mut self) {
        self.until("the workspace to finish starting", |c| c.shows("PROJECTS"));
    }

    /// Type a question and send it, and wait until it is a question rather than a draft.
    ///
    /// The wait is on the *transcript*, not on "is this text anywhere": what you have typed is in
    /// the composer from the first keystroke, so a check across every buffer is satisfied before
    /// `Enter` has been read, and the test races on from there.
    fn ask(&mut self, question: &str) {
        self.key(question);
        self.enter();
        self.until("the question to land in the transcript", |c| {
            c.transcript().iter().any(|l| l.contains(question))
        });
    }

    /// The chat buffer, which is the one named for it.
    fn transcript(&self) -> Vec<String> {
        self.mirror
            .buffers
            .values()
            .filter(|b| b.name.contains("chat"))
            .flat_map(|b| b.lines.iter().map(|l| l.text.clone()))
            .collect()
    }
}

fn status(sb: &Sandbox) -> String {
    let out = sb.neosh(&["status"]).output().expect("neosh status");
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn a_turn_keeps_running_when_the_terminal_that_started_it_goes_away() {
    // The whole point. A terminal is a window onto the work; closing a window is not a decision
    // about the work.
    let sb = Sandbox::new("survives");
    let ws = sb.serve_paced("hello_turn.jsonl", 900);

    let mut c = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    c.ready();
    c.ask("keep at it");

    // The terminal is gone — not asked to leave, *gone*, the way a closed window or a dropped ssh
    // connection goes.
    drop(c);

    let text = status(&sb);
    assert!(text.contains("running"), "the workspace is still there: {text}");
    assert!(text.contains("keep at it"), "and the turn is still in it: {text}");
    let mut ws = ws;
    assert!(ws.alive());
}

#[test]
fn reattaching_shows_what_happened_while_nobody_was_looking() {
    let sb = Sandbox::new("reattach");
    let ws = sb.serve("hello_turn.jsonl");

    let mut first = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    first.ready();
    first.ask("ask me something");
    drop(first);

    // Long enough for the turn to have finished with nobody attached at all.
    std::thread::sleep(Duration::from_millis(600));

    let mut second = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    second.until("the answer the turn produced", |c| c.shows("All done."));
    assert!(second.shows("ask me something"), "and the question: {:?}", second.text());
}

#[test]
fn quitting_a_terminal_leaves_the_workspace_running_and_stop_does_not() {
    // Two words for two different things. `quit` used to mean both because there was only one
    // place to be.
    let sb = Sandbox::new("quitstop");
    let mut ws = sb.serve("hello_turn.jsonl");

    let mut c = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    c.ready();
    c.send(ClientMessage::Input {
        event: InputEvent::Command { name: "quit".into(), args: Vec::new() },
    });
    c.until("to be sent away", |c| c.ended == Some(DetachReason::Asked));
    assert!(ws.alive(), "quit closed the terminal, not the workspace");
    assert!(status(&sb).contains("running"));

    let out = sb.neosh(&["stop"]).output().expect("stop");
    assert!(String::from_utf8_lossy(&out.stdout).contains("stopped"));
    let deadline = Instant::now() + PATIENCE;
    while ws.alive() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!ws.alive(), "stop stopped it");
    assert!(status(&sb).contains("no workspace"));
}

#[test]
fn a_second_terminal_joins_rather_than_taking_the_workspace_over() {
    // It used to take over and tell the first why. The reason to open a second window is that a
    // turn is running in the first, and taking it over is the exact moment you stop being able to
    // watch that turn.
    let sb = Sandbox::new("twoviews");
    let ws = sb.serve("hello_turn.jsonl");

    let mut first = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    first.ready();

    let mut second = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    second.ready();

    first.pump_once();
    assert!(first.ended.is_none(), "the first terminal was not sent away");
    assert!(second.ended.is_none(), "and the second one is here too");
}

#[test]
fn what_happens_in_one_terminal_happens_in_the_other() {
    // A conversation open in two terminals is one conversation: a question asked here is a
    // question on screen there, live and without asking for it. What is *not* shared is where you
    // are in it — two transcripts, two scroll positions, two drafts — but the rows say the same
    // thing, because the rows are what the agent produced.
    let sb = Sandbox::new("mirrored");
    let ws = sb.serve("hello_turn.jsonl");

    let mut first = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    first.ready();
    let mut second = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    second.ready();

    first.ask("what did you do");

    second.until("the other terminal to show the question", |c| {
        c.transcript().iter().any(|l| l.contains("what did you do"))
    });
    second.until("and the answer it produced", |c| c.shows("All done."));
    first.until("in the terminal that asked, too", |c| c.shows("All done."));
}

#[test]
fn a_second_terminal_lands_somewhere_the_first_one_is_not() {
    // The reason to open a second window is that the first one is busy. Landing in the same
    // conversation would make it a copy of the thing you were trying to get away from, so it takes
    // the most recent conversation nobody is reading.
    let sb = Sandbox::new("elsewhere");
    let ws = sb.serve("two_conversations.jsonl");

    let mut first = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    first.ready();
    first.ask("about the first thing");

    // A second conversation, which the first terminal is now the one reading.
    first.send(ClientMessage::Input {
        event: InputEvent::Command { name: "session.new".into(), args: Vec::new() },
    });
    first.until("the new conversation to be empty", |c| {
        !c.transcript().iter().any(|l| l.contains("about the first thing"))
    });
    first.ask("about the second thing");

    let mut second = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    second.ready();
    second.until("the other conversation, not this one", |c| {
        c.transcript().iter().any(|l| l.contains("about the first thing"))
    });

    // And the first terminal did not move to make room.
    first.pump_once();
    assert!(
        first.transcript().iter().any(|l| l.contains("about the second thing")),
        "the first terminal was moved out of the conversation it was in\n{:#?}",
        first.transcript()
    );
    assert!(
        !second.transcript().iter().any(|l| l.contains("about the second thing")),
        "the second terminal is reading the first one's conversation\n{:#?}",
        second.transcript()
    );
}

#[test]
fn typing_in_one_terminal_stays_in_that_terminal() {
    // Two terminals in two conversations are two drafts. It is the same rule as the transcript:
    // what the agent produced is the workspace's, and what you are in the middle of writing is
    // yours.
    let sb = Sandbox::new("twodrafts");
    let ws = sb.serve("two_conversations.jsonl");

    let mut first = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    first.ready();
    first.key("half a sentence");
    first.until("the draft to appear here", |c| c.shows("half a sentence"));

    let mut second = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    second.ready();
    second.pump_once();
    assert!(
        !second.shows("half a sentence"),
        "somebody else's draft turned up in this terminal\n{:#?}",
        second.text()
    );
}

#[test]
fn a_turn_running_in_one_terminal_is_visible_from_the_other() {
    // The whole point of the second window: you opened it because something is running in the
    // first. So what is *happening* has to reach it — the panel lists every conversation in the
    // workspace, whoever is reading them — while what you are reading does not.
    let sb = Sandbox::new("watching");
    let ws = sb.serve_paced("two_answers.jsonl", 700);

    let mut first = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    first.ready();
    first.ask("the quiet one");

    // A second conversation, and a turn running in it. The first terminal is now the one reading
    // that, which leaves the quiet conversation for whoever attaches next.
    first.send(ClientMessage::Input {
        event: InputEvent::Command { name: "session.new".into(), args: Vec::new() },
    });
    first.until("the new conversation to be empty", |c| {
        !c.transcript().iter().any(|l| l.contains("the quiet one"))
    });
    first.ask("go and do the thing");

    let mut second = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    second.ready();
    second.until("the quiet conversation here", |c| {
        c.transcript().iter().any(|l| l.contains("the quiet one"))
    });

    // The working conversation is in this terminal's panel, and not in its transcript.
    second.until("the working conversation listed in the panel", |c| {
        c.shows("go and do the thing")
    });
    assert!(
        !second.transcript().iter().any(|l| l.contains("go and do the thing")),
        "the second terminal is reading the first one's conversation\n{:#?}",
        second.transcript()
    );
    // And the turn finishes where it started.
    first.until("the answer, in the terminal that asked", |c| c.shows("ANSWER-LATER"));
}

#[test]
fn each_terminal_has_a_panel_of_its_own() {
    // A dock exists once per terminal or it exists in one of them. `^B` hides the column in front
    // of you and says nothing about anybody else's.
    let sb = Sandbox::new("twopanels");
    let ws = sb.serve("two_conversations.jsonl");

    let mut first = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    first.ready();
    let mut second = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    second.ready();

    assert!(first.windowed("sidebar"), "no panel to hide");
    first.ctrl('b');
    first.until("the panel to go here", |c| !c.windowed("sidebar"));

    second.pump_once();
    assert!(
        second.windowed("sidebar"),
        "hiding one terminal's panel hid the other's\n{:#?}",
        second.text()
    );
}

#[test]
fn closing_one_terminal_leaves_the_others_alone() {
    // `^Q` closes *this* window. The workspace carries on, and so does every other view of it —
    // which was the one thing one-viewer-at-a-time could never be asked.
    let sb = Sandbox::new("closeone");
    let ws = sb.serve("hello_turn.jsonl");

    let mut first = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    first.ready();
    let mut second = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    second.ready();

    first.leave();
    first.until("the terminal that asked to leave to go", |c| c.ended.is_some());

    // The survivor is not merely still connected — it is still being drawn to.
    second.ask("still here");
    second.until("the workspace to answer the one that stayed", |c| c.shows("All done."));

    let text = status(&sb);
    assert!(text.contains("attached"), "somebody is still looking: {text}");
}

#[test]
fn the_last_terminal_leaving_makes_the_workspace_idle_not_gone() {
    let sb = Sandbox::new("allidle");
    let ws = sb.serve("hello_turn.jsonl");

    let mut first = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    first.ready();
    let mut second = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    second.ready();

    first.leave();
    first.until("the first to go", |c| c.ended.is_some());
    // Still attached: one terminal leaving is not "nobody is looking".
    assert!(status(&sb).contains("attached"));

    second.leave();
    second.until("the second to go", |c| c.ended.is_some());

    let text = status(&sb);
    assert!(!text.contains("no workspace"), "it is still running: {text}");
    let mut ws = ws;
    assert!(ws.alive(), "an empty workspace is idle, not over");
}

#[test]
fn each_terminal_fits_a_card_to_its_own_width() {
    // A tool card is one row, so it is fitted to a width rather than wrapped — and it used to be
    // fitted to the *smallest* attached terminal's, because every view shared one transcript
    // buffer and content measured for the wide one wrapped in the narrow one. A transcript belongs
    // to a view now, so there is nothing left to share: the wide window gets the whole path and the
    // narrow one gets it elided, and each is right about the terminal it is in.
    let sb = Sandbox::new("smallest");
    let ws = sb.serve("wide_card.jsonl");

    let mut wide = Client::attach_sized(&ws.socket, neosh_proto::PROTOCOL_VERSION, 160, 40);
    wide.ready();
    wide.report_viewports();

    let mut narrow = Client::attach_sized(&ws.socket, neosh_proto::PROTOCOL_VERSION, 60, 20);
    narrow.ready();
    narrow.report_viewports();

    narrow.until("the welcome to be drawn for the narrow terminal", |c| {
        c.transcript().iter().all(|l| l.chars().count() <= 60)
    });
    wide.ask("how wide");
    wide.until("the answer", |c| c.shows("All done."));

    // Both terminals are in the one conversation there is, so both show the card — each fitted to
    // itself, and neither wrapping.
    narrow.until("the answer next door", |c| c.shows("All done."));
    let narrow_rows = narrow.transcript();
    let widest = narrow_rows.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    assert!(
        widest <= 60,
        "a row of {widest} columns does not fit the narrow terminal: {narrow_rows:#?}"
    );

    let wide_rows = wide.transcript();
    let longest = wide_rows.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    assert!(
        longest > 60,
        "the wide terminal was still clamped to the narrow one's width: {wide_rows:#?}"
    );
}

#[test]
fn a_client_from_a_different_neosh_is_refused_in_a_sentence() {
    // The failure this exists for: neosh is upgraded while a workspace from the old one is still
    // running. Rendering a partial UI would be a worse answer than saying so.
    let sb = Sandbox::new("protocol");
    let ws = sb.serve("hello_turn.jsonl");

    let mut c = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION + 1);
    c.until("a refusal", |c| c.refusal.is_some());
    let reason = c.refusal.clone().expect("a reason");
    assert!(reason.contains("neosh stop"), "and it says what to do about it: {reason}");
}

#[test]
fn a_second_workspace_will_not_start_on_a_socket_that_answers() {
    let sb = Sandbox::new("onlyone");
    let _ws = sb.serve("hello_turn.jsonl");

    let out = sb
        .neosh(&["--serve", "--model", "mock/mock", "--mock-script"])
        .arg(fixture("hello_turn.jsonl"))
        .output()
        .expect("second workspace");
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("already running"), "{text}");
    assert!(text.contains("neosh stop"), "and says how to deal with it: {text}");
}

#[test]
fn a_socket_left_behind_by_a_workspace_that_died_is_not_a_workspace() {
    // A socket file outlives the process that made it, so a crash leaves one. Treating its
    // presence as "something is running" would mean one hard kill locks the directory forever.
    let sb = Sandbox::new("stale");
    let socket = sb.socket();
    std::fs::create_dir_all(socket.parent().expect("dir")).expect("mkdir");
    std::fs::write(&socket, b"not a socket").expect("leftover");

    let mut ws = sb.serve("hello_turn.jsonl");
    assert!(ws.alive(), "it bound over the leftover");
    let mut c = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    c.ready();
}

#[test]
fn status_answers_while_a_turn_is_running_which_is_when_it_is_asked() {
    let sb = Sandbox::new("statusbusy");
    let ws = sb.serve_paced("hello_turn.jsonl", 900);
    let mut c = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    c.ready();
    c.ask("think about it");

    let deadline = Instant::now() + PATIENCE;
    loop {
        let text = status(&sb);
        if text.contains("think about it") {
            assert!(text.contains("a terminal is attached"), "{text}");
            break;
        }
        assert!(Instant::now() < deadline, "status never reported the turn: {text}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `^C` twice is a way out of a *terminal*, and a workspace is not one.
///
/// `^Q` has always known that. `^C`'s cascade — copy, leave reading, interrupt the turn, clear the
/// draft, and only then quit — ended by setting `quitting` directly, so the last rung of the key
/// people press when they want out of something took the whole workspace with it: every
/// conversation, in every project, and every turn running in any of them.
#[test]
fn pressing_ctrl_c_twice_closes_the_terminal_and_not_the_workspace() {
    let sb = Sandbox::new("ctrlc");
    let mut ws = sb.serve("hello_turn.jsonl");

    let mut c = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    c.ready();
    // Nothing selected, nothing being read, no turn, no draft: the cascade is empty, so the first
    // press arms and the second one is the way out.
    c.ctrl('c');
    c.ctrl('c');
    c.until("to be sent away", |c| c.ended == Some(DetachReason::Asked));

    assert!(ws.alive(), "^C^C closed the terminal, not the workspace");
    assert!(status(&sb).contains("running"), "{}", status(&sb));

    // And it is still a workspace you can come back to, which is the part that matters.
    let mut again = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    again.ready();
}

/// What a turn has already done is on disk while it is still doing the rest.
///
/// A turn is minutes of work and a dozen tool rounds, and the only saves used to be at either end
/// of it — so for the whole time it ran, the conversation on disk held the message you typed and
/// nothing after it. Anything that took the workspace out without warning lost the entire answer
/// and left the transcript ending on your own question.
#[test]
fn what_a_turn_has_done_is_on_disk_before_the_turn_ends() {
    let sb = Sandbox::new("midturn");
    let mut ws = sb.serve_paced("hello_turn.jsonl", 700);

    let mut c = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    c.ready();
    c.ask("read it for me");

    // The first round's tool call, written down while the second round is still going. Waited for
    // rather than slept on, and the turn is asserted to be still in flight when it arrives — a
    // save that only happened because the turn had quietly finished would prove nothing.
    let deadline = Instant::now() + PATIENCE;
    loop {
        let saved = sessions_on_disk(&sb);
        if saved.contains("hello_greet") {
            assert!(
                status(&sb).contains("read it for me"),
                "the turn was already over, so this says nothing about saving during one"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the turn's first round never reached the disk:\n{saved}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // And the real test of it: take the workspace out the way a crash would, with no chance to
    // flush anything, and see what a new one finds.
    ws.kill_hard();
    drop(c);
    let ws2 = sb.serve_paced("hello_turn.jsonl", 0);
    let mut c2 = Client::attach(&ws2.socket, neosh_proto::PROTOCOL_VERSION);
    c2.ready();
    c2.until("the killed turn's work to come back", |c| {
        c.transcript().iter().any(|l| l.contains("read it for me"))
    });
    let back = c2.transcript().join("\n");
    assert!(back.contains("hello_greet"), "the tool round the turn had finished is gone:\n{back}");
}

/// Every session file the sandbox has written, as one string to look for things in.
fn sessions_on_disk(sb: &Sandbox) -> String {
    let dir = sb.root.join("state/sessions");
    let Ok(entries) = std::fs::read_dir(&dir) else { return String::new() };
    entries
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A turn killed with the workspace says so, in the panel and in the transcript.
///
/// The one thing a conversation cannot notice about itself. Someone shuts the computer down mid
/// turn; the process does not get to write down that it died, so the conversation comes back
/// ending on a half-finished piece of work with nothing anywhere saying that is what it is.
#[test]
fn a_turn_killed_with_the_workspace_is_marked_when_it_comes_back() {
    let sb = Sandbox::new("cutoff");
    let mut ws = sb.serve_paced("hello_turn.jsonl", 700);

    let mut c = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    c.ready();
    c.ask("do the long thing");
    // Wait until the turn is genuinely in flight before pulling the plug, so what is being tested
    // is a turn that was killed rather than one that had never started.
    let deadline = Instant::now() + PATIENCE;
    while !status(&sb).contains("do the long thing") {
        assert!(Instant::now() < deadline, "the turn never started");
        std::thread::sleep(Duration::from_millis(50));
    }

    // No shutdown, no last flush: the machine went away.
    ws.kill_hard();
    drop(c);

    let ws2 = sb.serve_paced("hello_turn.jsonl", 0);
    let mut c2 = Client::attach(&ws2.socket, neosh_proto::PROTOCOL_VERSION);
    c2.ready();
    c2.until("the cut-off turn to be accounted for", |c| {
        c.transcript().iter().any(|l| l.contains("the workspace stopped while this turn was running"))
    });

    // And it survives a second start. The note the mark is derived from is cleared by the load that
    // reads it, so a mark that was not itself written down would last exactly one restart — and the
    // restart after that would quietly say everything was fine.
    drop(c2);
    let out = sb.neosh(&["stop"]).output().expect("stop");
    assert!(String::from_utf8_lossy(&out.stdout).contains("stopped"));
    let ws3 = sb.serve_paced("hello_turn.jsonl", 0);
    let mut c3 = Client::attach(&ws3.socket, neosh_proto::PROTOCOL_VERSION);
    c3.ready();
    c3.until("the mark to survive a second restart", |c| {
        c.transcript().iter().any(|l| l.contains("the workspace stopped while this turn was running"))
    });
}

/// And it goes away when a new turn starts there, because then it is not the last turn any more.
#[test]
fn asking_again_clears_the_mark_a_killed_turn_left() {
    let sb = Sandbox::new("cutoffclear");
    let mut ws = sb.serve_paced("hello_turn.jsonl", 700);
    let mut c = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    c.ready();
    c.ask("the one that dies");
    let deadline = Instant::now() + PATIENCE;
    while !status(&sb).contains("the one that dies") {
        assert!(Instant::now() < deadline, "the turn never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    ws.kill_hard();
    drop(c);

    let ws2 = sb.serve_paced("hello_turn.jsonl", 0);
    let mut c2 = Client::attach(&ws2.socket, neosh_proto::PROTOCOL_VERSION);
    c2.ready();
    c2.until("the mark", |c| {
        c.transcript().iter().any(|l| l.contains("the workspace stopped while this turn was running"))
    });
    c2.ask("and the one that does not");
    c2.until("the new turn to finish", |c| {
        c.transcript().iter().any(|l| l.contains("All done."))
    });
    let after = c2.transcript().join("\n");
    assert!(
        !after.contains("the workspace stopped while this turn was running"),
        "a finished turn left the previous turn's mark behind:\n{after}"
    );
}

/// The whole sequence from the report: interrupt mid-answer, close the terminal, open it again,
/// ask something else. What comes back must answer the new question and not finish the old one.
#[test]
fn what_an_interrupted_turn_was_saying_does_not_arrive_under_the_next_question() {
    let sb = Sandbox::new("crossed");
    let ws = sb.serve_paced("two_answers.jsonl", 900);

    let mut first = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    first.ready();
    first.ask("question one");
    first.until("the first answer to start arriving", |c| c.shows("ANSWER"));
    // `^C` on a running turn interrupts it, which is the first rung of that key and not the last.
    first.ctrl('c');
    first.until("the turn to stop", |c| !c.shows("esc to interrupt"));
    first.leave();
    first.until("the first terminal to go", |c| c.ended.is_some());

    let mut second = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    second.ready();
    second.until("the conversation to come back", |c| c.shows("question one"));
    second.ask("question two");
    second.until("an answer to it", |c| c.shows("ANSWER-LATER"));
    second.until("the turn to be over", |c| !c.shows("esc to interrupt"));

    // Long enough for anything the interrupted turn was still writing to have been written.
    std::thread::sleep(Duration::from_millis(2000));
    second.pump_once();
    let t = second.transcript();
    let at = t.iter().position(|l| l.contains("question two")).expect("the second question");
    assert!(
        !t.iter().skip(at).any(|l| l.contains("ANSWER-ONE")),
        "the interrupted turn finished itself under the new question: {t:#?}"
    );
}

#[test]
fn reattaching_while_a_turn_runs_shows_it_still_running() {
    // "I want to see the running ones when I open it again." A terminal that goes away mid-answer
    // and comes back has to find the turn where it left it — the question, what has been said so
    // far, and a working line that is still working — rather than a transcript that ends on a
    // question nothing appears to be answering.
    let sb = Sandbox::new("stillrunning");
    let ws = sb.serve_paced("two_answers.jsonl", 900);

    let mut first = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    first.ready();
    first.ask("keep at it");
    first.until("the first answer to start arriving", |c| c.shows("ANSWER"));
    first.leave();
    first.until("the first terminal to go", |c| c.ended.is_some());

    let mut second = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    second.ready();
    second.until("the question to be back", |c| c.shows("keep at it"));
    second.until("what it had already said", |c| c.shows("ANSWER-"));
    second.until("and a turn that still looks like one", |c| c.shows("esc to interrupt"));
    second.until("which then finishes here", |c| !c.shows("esc to interrupt"));
}

/// A sandbox whose `claude` is a stand-in that logs every time it is started.
///
/// The real failure is a *process* count, so nothing short of a real program on `PATH` can see it:
/// a mock provider is one object and cannot be spawned twice. Every line it says names the process
/// that said it, which is what tells one CLI answering twice from two CLIs answering once each.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
L="$NEOSH_FAKE_LOG"
case "$1" in
  --version) echo "2.2.0 (Claude Code)"; exit 0 ;;
esac
echo "`date +%s.%N` $$ $*" >> "$L/spawns"
n=0
while IFS= read -r line; do
  case "$line" in
    *interrupt*)
      : > "$L/stop.$$"
      id=`printf '%s' "$line" | sed 's/.*"request_id":"\([^"]*\)".*/\1/'`
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id"
      continue ;;
    *control_request*)
      id=`printf '%s' "$line" | sed 's/.*"request_id":"\([^"]*\)".*/\1/'`
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"totalTokens":10,"maxTokens":100}}}\n' "$id"
      continue ;;
  esac
  n=`expr $n + 1`
  rm -f "$L/stop.$$"
  printf '{"type":"system","subtype":"init","session_id":"s%s","slash_commands":[]}\n' "$$"
  m=$n
  p=$$
  (
    i=1
    while [ $i -le 8 ]; do
      if [ -f "$L/stop.$p" ]; then break; fi
      printf '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"<p%s.q%s.%s>"}}}\n' "$p" "$m" "$i"
      sleep 1
      i=`expr $i + 1`
    done
    printf '{"type":"result","subtype":"success","is_error":false,"session_id":"s%s","result":""}\n' "$p"
  ) &
done
"#;

/// Install the stand-in and start a workspace pointed at it. Returns the workspace and its log.
fn serve_with_fake_claude(sb: &Sandbox) -> (Workspace, PathBuf) {
    let bin = sb.root.join("bin");
    let log = sb.root.join("log");
    std::fs::create_dir_all(&bin).expect("bin");
    std::fs::create_dir_all(&log).expect("log");
    let script = bin.join("claude");
    // Written by a child, never through a descriptor this process holds open: a writable fd on a
    // program a sibling test is about to `execve` is how `ETXTBSY` happens.
    let wrote = Command::new("/bin/sh")
        .args(["-c", r#"printf '%s' "$1" > "$2" && chmod 755 "$2""#, "neosh-test-write"])
        .arg(FAKE_CLAUDE)
        .arg(&script)
        .status()
        .expect("a shell to write the stand-in");
    assert!(wrote.success(), "writing the stand-in failed");

    let child = sb
        .neosh(&["--serve", "--model", "claude-cli/claude-opus-5"])
        .env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()))
        .env("NEOSH_FAKE_LOG", &log)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn workspace");
    let socket = sb.socket();
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline && UnixStream::connect(&socket).is_err() {
        std::thread::sleep(Duration::from_millis(20));
    }
    (Workspace { child, socket }, log)
}

/// Every CLI the stand-in was asked to be, as (pid, argv).
fn spawns(log: &Path) -> Vec<(String, String)> {
    std::fs::read_to_string(log.join("spawns"))
        .map(|s| {
            s.lines()
                .filter_map(|l| {
                    let mut parts = l.splitn(3, ' ');
                    let _when = parts.next()?;
                    Some((parts.next()?.to_string(), parts.next().unwrap_or("").to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Which of them are still running. The whole symptom is a process count, so this is the assertion.
fn still_alive(log: &Path) -> Vec<String> {
    spawns(log)
        .into_iter()
        .map(|(pid, _)| pid)
        .filter(|pid| {
            Command::new("kill")
                .args(["-0", pid])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        })
        .collect()
}

/// Which process said the last thing in the transcript. Every line the stand-in emits names it.
fn answering_pid(t: &[String], after: &str) -> Option<String> {
    let at = t.iter().position(|l| l.contains(after))?;
    let line = t.iter().skip(at + 1).find(|l| l.contains("<p"))?;
    let start = line.find("<p")? + 2;
    let end = line[start..].find('.')? + start;
    Some(line[start..end].to_string())
}

/// One conversation is one agent, and interrupting it does not start another.
///
/// The report this comes from: `^C` mid-answer, quit, reopen, ask again — and two `claude`
/// processes running under one workspace with one conversation in it. Both halves are here because
/// they were two separate faults that looked like one: a one-shot generation (the thread's title)
/// took a slot in the map keyed by conversation and held a whole agent session open forever, and
/// an interrupt ended by killing the CLI it had already stopped politely.
#[test]
fn one_conversation_runs_one_agent_across_an_interrupt() {
    let sb = Sandbox::new("oneagent");
    let (ws, log) = serve_with_fake_claude(&sb);

    let mut c = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    c.ready();
    c.ask("question one");
    c.until("the first answer to start arriving", |c| c.shows(".q1.1>"));
    let first = answering_pid(&c.transcript(), "question one").expect("somebody answered");

    c.ctrl('c');
    std::thread::sleep(Duration::from_millis(2000));
    c.leave();
    c.until("the terminal to go", |c| c.ended.is_some());

    let mut back = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    back.ready();
    back.ask("question two");
    back.until("an answer to the second question", |c| {
        answering_pid(&c.transcript(), "question two").is_some()
    });
    let second = answering_pid(&back.transcript(), "question two").expect("somebody answered");
    assert_eq!(second, first, "the interrupt threw the conversation's agent away and started another");

    // Everything a one-shot borrowed is given back when it is finished with, so what is left is
    // the conversation's own agent and nothing else. Waited for rather than sampled: a one-shot is
    // a turn like any other and takes as long as the model does.
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline && still_alive(&log).len() > 1 {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        still_alive(&log),
        vec![first],
        "one conversation, one agent — spawned {:?}",
        spawns(&log)
    );
}

// ---------------------------------------------------------------------------
// The control surface — a caller with no screen
// ---------------------------------------------------------------------------

/// A script, driven over the same socket a terminal attaches to.
///
/// Deliberately not `crate::control::Control`: this is an integration test of a *protocol*, and a
/// client that shared its implementation with the thing under test would pass just as happily if
/// both were wrong about the wire. So it speaks `ClientMessage` and reads `ServerMessage` by hand,
/// exactly as the terminal client above does.
struct Script {
    write: UnixStream,
    lines: std::sync::mpsc::Receiver<String>,
    held: std::collections::VecDeque<neosh_proto::PluginEvent>,
    next: u64,
}

impl Drop for Script {
    fn drop(&mut self) {
        let _ = self.write.shutdown(std::net::Shutdown::Both);
    }
}

impl Script {
    fn open(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket).expect("connect");
        let read = stream.try_clone().expect("clone");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(read).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self { write: stream, lines: rx, held: Default::default(), next: 0 }
    }

    fn send(&mut self, msg: ClientMessage) {
        let mut line = serde_json::to_vec(&msg).expect("encode");
        line.push(b'\n');
        self.write.write_all(&line).expect("write");
        self.write.flush().expect("flush");
    }

    fn subscribe(&mut self) {
        self.send(ClientMessage::Subscribe);
    }

    /// One call, and the answer to it. Panics on an API error, which in these tests is always a
    /// failure of the thing being tested rather than something to assert about.
    fn call(&mut self, call: neosh_proto::ApiCall) -> neosh_proto::ApiOk {
        match self.try_call(call) {
            neosh_proto::ApiResponse::Ok { value } => value,
            neosh_proto::ApiResponse::Err { error } => panic!("the workspace refused: {error}"),
        }
    }

    fn try_call(&mut self, call: neosh_proto::ApiCall) -> neosh_proto::ApiResponse {
        self.next += 1;
        let id = self.next.to_string();
        self.send(ClientMessage::Call { id: id.clone(), call });
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            let Ok(line) = self.lines.recv_timeout(Duration::from_millis(200)) else { continue };
            match serde_json::from_str::<ServerMessage>(&line) {
                Ok(ServerMessage::Called { id: got, response }) if got == id => return response,
                Ok(ServerMessage::Event { event }) => self.held.push_back(event),
                _ => {}
            }
        }
        panic!("no answer to {id}");
    }

    /// Read events until `f` is happy with one.
    fn until_event(
        &mut self,
        what: &str,
        f: impl Fn(&neosh_proto::PluginEvent) -> bool,
    ) -> neosh_proto::PluginEvent {
        let deadline = Instant::now() + PATIENCE;
        loop {
            while let Some(e) = self.held.pop_front() {
                if f(&e) {
                    return e;
                }
            }
            if Instant::now() > deadline {
                panic!("timed out waiting for {what}");
            }
            let Ok(line) = self.lines.recv_timeout(Duration::from_millis(200)) else { continue };
            if let Ok(ServerMessage::Event { event }) = serde_json::from_str::<ServerMessage>(&line)
            {
                self.held.push_back(event);
            }
        }
    }
}

fn sessions(s: &mut Script) -> Vec<neosh_proto::SessionInfo> {
    match s.call(neosh_proto::ApiCall::SessionList { include_archived: true }) {
        neosh_proto::ApiOk::Sessions { sessions } => sessions,
        other => panic!("expected a conversation list, got {other:?}"),
    }
}

/// A script starts a conversation, runs a turn in it, and reads back what was said — with nobody
/// at a terminal at any point.
///
/// This is the whole of `neosh agent`, minus the argument parsing: start, send, hear it end, read
/// it. It runs against a workspace that has never had a client attached, because that is how a
/// fan-out actually runs — the person who launched it has gone.
#[test]
fn a_script_can_run_a_turn_with_nobody_watching() {
    let sb = Sandbox::new("control");
    let ws = sb.serve("hello_turn.jsonl");

    let mut s = Script::open(&ws.socket);
    // Subscribed *before* the turn is started. The other order is a race a short turn wins, and
    // the mock provider is about as short as a turn gets.
    s.subscribe();

    let made = match s.call(neosh_proto::ApiCall::SessionNew {
        cwd: Some(sb.root.join("work").display().to_string()),
        title: Some("from a script".into()),
        activate: false,
    }) {
        neosh_proto::ApiOk::Session { session } => session,
        other => panic!("expected the conversation it made, got {other:?}"),
    };
    assert_eq!(made.title.as_deref(), Some("from a script"));

    s.call(neosh_proto::ApiCall::AgentCommand {
        session: Some(made.id.clone()),
        command: neosh_proto::AgentCommand::Send { text: "say something".into(), images: Vec::new() },
    });

    let want = made.id.clone();
    s.until_event("the turn to end", move |e| {
        matches!(e, neosh_proto::PluginEvent::TurnEnded { session, .. } if *session == want)
    });

    let messages =
        match s.call(neosh_proto::ApiCall::SessionMessages { session: Some(made.id.clone()) }) {
            neosh_proto::ApiOk::Messages { messages } => messages,
            other => panic!("expected a transcript, got {other:?}"),
        };
    let said: String = messages
        .iter()
        .filter(|m| m.role == neosh_proto::Role::Assistant)
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            neosh_proto::ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(said.contains("All done."), "the script never saw the answer: {said:?}");
}

/// A control connection is not a terminal.
///
/// The thing this protects: "is anybody watching" decides whether a notification leaves the
/// terminal and whether the workspace counts as idle. A script polling every ten seconds must not
/// make a workspace believe somebody is sitting at it — and must not take an actual terminal's
/// place, which is what attaching does.
#[test]
fn a_script_is_not_somebody_watching() {
    let sb = Sandbox::new("controlnotaview");
    let ws = sb.serve("hello_turn.jsonl");

    let mut c = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    c.ready();

    let mut s = Script::open(&ws.socket);
    // Enough calls that a bug counting connections would have counted these.
    for _ in 0..3 {
        sessions(&mut s);
    }
    c.pump_once();
    assert!(c.ended.is_none(), "a script took the terminal's place: {:?}", c.ended);

    let out = sb.neosh(&["status"]).output().expect("status");
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        said.contains("a terminal is attached"),
        "the terminal should still be the one watching: {said}"
    );

    drop(c);
    // The terminal has gone and only the script is left, so nobody is watching — even though a
    // connection is open and answering.
    let deadline = Instant::now() + PATIENCE;
    let mut said = String::new();
    while Instant::now() < deadline {
        let out = sb.neosh(&["status"]).output().expect("status");
        said = String::from_utf8_lossy(&out.stdout).into_owned();
        if said.contains("nobody watching") {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(said.contains("nobody watching"), "a script counted as a viewer: {said}");
    // And it is still answering, which is the other half: not being a terminal is not the same as
    // having been disconnected.
    assert!(!sessions(&mut s).is_empty());
}

/// Deleting a conversation over the socket deletes it, and a call that cannot be answered says so
/// rather than reporting success.
#[test]
fn a_script_can_throw_a_conversation_away_and_is_refused_what_needs_a_person() {
    let sb = Sandbox::new("controldelete");
    let ws = sb.serve("hello_turn.jsonl");
    let mut s = Script::open(&ws.socket);

    let made = match s.call(neosh_proto::ApiCall::SessionNew {
        cwd: Some(sb.root.join("work").display().to_string()),
        title: Some("temporary".into()),
        activate: false,
    }) {
        neosh_proto::ApiOk::Session { session } => session,
        other => panic!("expected a conversation, got {other:?}"),
    };
    assert!(sessions(&mut s).iter().any(|x| x.id == made.id));

    s.call(neosh_proto::ApiCall::SessionClose { session: made.id.clone() });
    assert!(
        !sessions(&mut s).iter().any(|x| x.id == made.id),
        "the conversation is still here after being closed"
    );

    // A key is typed into a terminal, and there is no terminal on this connection. The refusal
    // says what to do instead — a call that returned `ok` having done nothing is the failure mode
    // worth a test of its own.
    let answer = s.try_call(neosh_proto::ApiCall::ProviderSetCredential {
        instance: neosh_proto::InstanceId("mock".into()),
        replace: false,
    });
    match answer {
        neosh_proto::ApiResponse::Err { error } => {
            let said = error.to_string();
            assert!(said.contains("config.toml"), "the refusal says no fix: {said}");
        }
        neosh_proto::ApiResponse::Ok { value } => {
            panic!("a key prompt with nobody to type into it answered {value:?}")
        }
    }
}

/// `neosh "a prompt"` lands the terminal in the conversation it just started.
///
/// The case this is really about is a workspace that has been sitting with nobody attached: it
/// keeps the last screen for whoever comes back, so a terminal that trusted "the newest
/// conversation is where an arriving terminal lands" would be handed the old one instead, with its
/// own turn running out of sight. See `Host::open_view`.
#[test]
fn attaching_into_a_named_conversation_beats_the_screen_that_was_left_behind() {
    let sb = Sandbox::new("controlopen");
    let ws = sb.serve("hello_turn.jsonl");

    // Somebody was here, in a conversation of their own, and left. The workspace keeps their
    // screen.
    let mut first = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    first.ready();
    first.leave();
    first.until("the first terminal to go", |c| c.ended.is_some());
    drop(first);

    // A script starts a conversation with a name nothing else would have.
    let mut s = Script::open(&ws.socket);
    let made = match s.call(neosh_proto::ApiCall::SessionNew {
        cwd: Some(sb.root.join("work").display().to_string()),
        title: Some("zzz-the-one-we-asked-for".into()),
        activate: false,
    }) {
        neosh_proto::ApiOk::Session { session } => session,
        other => panic!("expected a conversation, got {other:?}"),
    };

    // And the terminal asks for it by name, exactly as `neosh "a prompt"` does.
    let stream = UnixStream::connect(&ws.socket).expect("connect");
    let read = stream.try_clone().expect("clone");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(read).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut asked = Client {
        write: stream,
        lines: rx,
        mirror: neosh_tui::Mirror::new(),
        ended: None,
        refusal: None,
        stopped: false,
        size: (100, 34),
    };
    asked.send(ClientMessage::Attach {
        protocol_version: neosh_proto::PROTOCOL_VERSION,
        width: 100,
        height: 34,
        build: Default::default(),
        session: Some(made.id.clone()),
    });
    asked.ready();

    // Asked of the workspace rather than read off the screen: which conversation a terminal is in
    // is a fact the host holds, and a title only reaches the screen if some panel happens to draw
    // it.
    let here = sessions(&mut s)
        .into_iter()
        .find(|x| x.on_screen)
        .expect("some conversation is on a screen");
    assert_eq!(
        here.id, made.id,
        "the terminal landed in {:?} rather than the conversation it asked for",
        here.title
    );
}
