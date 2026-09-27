//! The bundled plugins, driven through the real binary.
//!
//! These exist to keep one claim honest: the sidebar and the switchers are *plugins*, written
//! against the same API a third party has. If any of them ever needed a private path into the host,
//! it would show up here as a call these tests cannot make.
//!
//! Everything is asserted on rendered text and emitted UI events — what a user would actually see.
//!
//! **The mock plugins here announce themselves from `neosh.ready`, never from the end of their own
//! `activate`**, and every `wait_for("… ready")` below is relying on that. A plugin's own
//! activation is the wrong moment to type at: the ten bundled plugins are still loading alongside
//! it, so `^E`, `^L` and `^T` are keys nothing has bound yet, `session.new` is a name nothing
//! answers to, and a configured `agent.model` naming this very plugin may still be held. A test
//! that pressed a key there was racing ten module loads — which is a race an idle laptop wins and
//! a busy CI runner loses, and losing it is what turned twenty of these red at once.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use serde_json::Value;

mod support;

const BUDGET: Duration = Duration::from_secs(30);

struct Sandbox {
    root: PathBuf,
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Sandbox {
    /// A directory of this test's own.
    ///
    /// **The name has to be unique across the file.** The pid keeps two `cargo test` runs apart and
    /// does nothing at all about two *tests in one run*: they share a process, so the same name is
    /// the same directory, and the suite runs several at once — so one test wipes the other's
    /// checkout on the way in and its `Drop` deletes it again on the way out. It shows up as a
    /// failure in whichever of the pair happened to be slower, which moves from run to run, which
    /// is exactly what a flaky test looks like from the outside.
    fn new(name: &str) -> Self {
        let root = support::sandbox_root("bp", name);
        // `claude` and `codex` are the vendors' own homes, empty unless a test fills one. See
        // `Sandbox::write_usage_history`.
        for d in ["config", "state", "work", "claude/projects", "codex/sessions"] {
            std::fs::create_dir_all(root.join(d)).expect("sandbox dirs");
        }
        Self { root }
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    fn write_config(&self, contents: &str) {
        std::fs::write(self.root.join("config/config.toml"), contents).expect("write config");
    }

    /// Give `work/` an origin, so it has an identity other machines can agree with.
    ///
    /// A `ProjectKey` is the normalised origin URL, and without one a checkout falls back to
    /// `dir:<name>` — which is a guess, and here it would be a guess that happened to work, since
    /// both sides would be guessing from a directory the test named. Setting a remote makes the
    /// merge test assert the thing that actually happens in a workspace.
    fn git_remote(&self, url: &str) {
        let out = Command::new("git")
            .current_dir(self.work())
            .args(["remote", "add", "origin", url])
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git remote add: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// Make `work/` a git repository, so the git-dependent plugins have something to report.
    fn git_init(&self) {
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .current_dir(self.work())
                .args(args)
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        run(&["init", "--initial-branch=trunk"]);
        run(&["config", "user.email", "test@neosh.invalid"]);
        run(&["config", "user.name", "neosh test"]);
        run(&["config", "commit.gpgsign", "false"]);
        std::fs::write(self.work().join("README.md"), "hello\n").expect("write");
        run(&["add", "."]);
        run(&["commit", "-m", "initial"]);
    }

    /// One answer in a `claude` transcript, `hours_ago` old.
    ///
    /// The last-30-days block is read from `~/.claude/projects` and `~/.codex/sessions`, which are
    /// files another program wrote — so a test that asserts on it has to bring its own. Without
    /// this the assertion is about whatever the developer happened to have spent that month, which
    /// is a test that passes on a laptop and says "nothing in this span" on a fresh runner.
    fn write_usage_history(&self, model: &str, hours_ago: i64) {
        let at = now_secs() - hours_ago * 3_600;
        let dir = self.root.join("claude/projects/-tmp-neosh");
        std::fs::create_dir_all(&dir).expect("transcript dir");
        let line = serde_json::json!({
            "type": "assistant",
            "timestamp": iso_utc(at),
            "requestId": "req-1",
            "message": {
                "id": "msg-1",
                "model": model,
                "usage": {
                    "input_tokens": 12_000,
                    "output_tokens": 3_000,
                    "cache_read_input_tokens": 40_000,
                    "cache_creation_input_tokens": 1_000,
                },
            },
        });
        std::fs::write(dir.join("session.jsonl"), format!("{line}\n")).expect("transcript");
    }

    fn start(&self) -> Session {
        let child = Command::new(env!("CARGO_BIN_EXE_neosh"))
            .args(["--ui-protocol", "stdio"])
            .arg("--config-dir")
            .arg(self.root.join("config"))
            .arg("--cwd")
            .arg(self.work())
            .args(["--mock-script", &fixture().display().to_string()])
            .args(["--model", "mock/mock"])
            .env("NEOSH_STATE_DIR", self.root.join("state"))
            // `$SHELL` and nothing cleverer is right in the product and useless in a test: it is a
            // person's own shell with their own configuration in it, and the first run of a shell
            // test against a real login shell was answered by an `oh-my-zsh` update prompt that ate
            // a keystroke. A test that depends on whose machine it runs on is a test that will fail
            // for somebody else. `Term::spawn_shell` exists for this reason one level down; a pty
            // opened for a *peer* has no such seam, so the environment is where it is said.
            .env("SHELL", "/bin/sh")
            // The history block reads the *vendors'* transcripts rather than ours — see
            // `crate::usage`. Pointed at this sandbox, because the alternative is a hundred and
            // forty-seven neoshes scanning a real month of somebody's work and asserting on it.
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude"))
            .env("CODEX_HOME", self.root.join("codex"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("neosh should start");
        Session::wrap(child)
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// A unix second as `YYYY-MM-DDTHH:MM:SSZ`, which is what a transcript timestamp looks like.
///
/// Hinnant's `civil_from_days`, the companion to the `days_from_civil` in `crate::usage` that
/// reads it back. Written out rather than pulled in: one date format in one direction is not worth
/// a dependency, and the two halves failing together is easier to see when they are both here.
fn iso_utc(secs: i64) -> String {
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let (h, mi, sec) = (rest / 3_600, (rest % 3_600) / 60, rest % 60);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{sec:02}Z")
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hello_turn.jsonl")
}

fn have_git() -> bool {
    Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success())
}

struct Session {
    child: Child,
    lines: Receiver<String>,
    events: Vec<Value>,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Session {
    fn wrap(mut child: Child) -> Self {
        let out = BufReader::new(child.stdout.take().expect("stdout piped"));
        let (tx, lines) = channel();
        // On a thread, so the deadline is real: a blocking read ignores it and hangs the suite.
        std::thread::spawn(move || {
            for line in out.lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut s = Self { child, lines, events: Vec::new() };
        s.send(r#"{"type":"ready","width":100,"height":30}"#);
        s
    }

    fn send(&mut self, line: &str) {
        let stdin = self.child.stdin.as_mut().expect("stdin piped");
        writeln!(stdin, "{line}").expect("write");
        stdin.flush().expect("flush");
    }

    fn key(&mut self, c: &str) {
        self.send(&format!(
            r#"{{"type":"key","key":{{"code":{{"kind":"char","c":"{c}"}},"mods":{{}}}}}}"#
        ));
    }

    /// `^N`, and wait until the conversation it starts is actually in the list.
    ///
    /// Worth a helper: starting one is now several round trips — it asks git whether there are
    /// worktrees to offer before deciding whether to ask you anything — so a test that pressed the
    /// key and immediately moved the cursor was moving it over a list about to gain a row.
    fn new_conversation(&mut self) {
        let before = self.sidebar_rows();
        self.ctrl("n");
        assert!(
            self.pump(move |s| s.sidebar_rows() > before),
            "a conversation was started\n{:?}",
            self.sidebar_now()
        );
    }

    /// How many conversation rows the panel is showing, however they are named.
    ///
    /// Counted by the age in the right-hand column rather than by the indent. A row the panel has
    /// unfolded is several lines, and a line of somebody's title is indented exactly as far as the
    /// row it belongs to — so the text alone cannot tell a second line of a title from an idle
    /// conversation. The age can: it is virtual text on the row, and a continuation has none.

    fn sidebar_rows(&self) -> usize {
        let text = self.sidebar_now();
        let virt = self.sidebar_virt_now();
        text.iter()
            .zip(virt.iter())
            .filter(|(l, v)| l.starts_with(CONVERSATION_INDENT) && !v.trim().is_empty())
            .count()
    }

    fn ctrl(&mut self, c: &str) {
        self.send(&format!(
            r#"{{"type":"key","key":{{"code":{{"kind":"char","c":"{c}"}},"mods":{{"ctrl":true}}}}}}"#
        ));
    }

    fn special(&mut self, kind: &str) {
        self.send(&format!(r#"{{"type":"key","key":{{"code":{{"kind":"{kind}"}},"mods":{{}}}}}}"#));
    }

    fn pump(&mut self, pred: impl Fn(&Session) -> bool) -> bool {
        let deadline = Instant::now() + BUDGET;
        loop {
            if pred(self) {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&line) {
                        self.events.push(v);
                    }
                }
                Err(RecvTimeoutError::Disconnected | RecvTimeoutError::Timeout) => {
                    return pred(self);
                }
            }
        }
    }

    fn wait_for(&mut self, needle: &str) {
        let want = needle.to_string();
        if !self.pump(move |s| s.saw(&want)) {
            panic!("timed out waiting for {needle:?}\n{}", self.transcript());
        }
    }

    fn drain_for(&mut self, how_long: Duration) {
        let deadline = Instant::now() + how_long;
        while let Ok(line) =
            self.lines.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                self.events.push(v);
            }
            if Instant::now() >= deadline {
                break;
            }
        }
    }

    fn texts(&self) -> Vec<String> {
        self.events
            .iter()
            .flat_map(|e| match e["type"].as_str() {
                Some("buffer_lines") => e["lines"]
                    .as_array()
                    .map(|a| {
                        a.iter().filter_map(|l| l["text"].as_str().map(str::to_string)).collect()
                    })
                    .unwrap_or_default(),
                Some("message") => {
                    e["text"].as_str().map(|t| vec![t.to_string()]).unwrap_or_default()
                }
                _ => Vec::new(),
            })
            .collect()
    }

    fn saw(&self, needle: &str) -> bool {
        self.texts().iter().any(|l| l.contains(needle))
    }

    /// Buffers opened so far, by name. Used to tell "the sidebar exists" from "it is on screen".
    fn buffer_named(&self, name: &str) -> Option<u64> {
        self.events.iter().find_map(|e| {
            (e["type"] == "buffer_opened" && e["name"] == name).then(|| e["buf"].as_u64())?
        })
    }

    fn open_windows(&self) -> Vec<u64> {
        let mut open = Vec::new();
        for e in &self.events {
            match e["type"].as_str() {
                Some("window_opened") => {
                    if let Some(w) = e["win"].as_u64() {
                        open.push(w);
                    }
                }
                Some("window_closed") => {
                    if let Some(w) = e["win"].as_u64() {
                        open.retain(|x| *x != w);
                    }
                }
                _ => {}
            }
        }
        open
    }

    /// Every buffer that has ever been opened under `name`, newest last.
    ///
    /// A panel that is opened, closed and opened again is two buffers with one name, so a helper
    /// that took the first would be watching the one from last time forever.
    fn buffers_named(&self, name: &str) -> Vec<u64> {
        self.events
            .iter()
            .filter(|e| e["type"] == "buffer_opened" && e["name"] == name)
            .filter_map(|e| e["buf"].as_u64())
            .collect()
    }

    /// Wait until something on screen is showing a buffer called `name`.
    ///
    /// Not `wait_for` on its text: that scans everything the screen has *ever* said, so the second
    /// time a panel is opened it returns immediately — before the panel is there — and the keys
    /// meant for it go to the composer instead.
    fn wait_open(&mut self, name: &str) {
        let there = |s: &Session| s.buffers_named(name).iter().any(|b| !s.windows_for(*b).is_empty());
        assert!(self.pump(there), "{name} never opened\n{}", self.transcript());
    }

    /// Wait until nothing on screen is showing the buffer called `name`.
    ///
    /// The panels are plugin-drawn, so closing one is a round trip: the key resolves against the
    /// window that is still open, the plugin is told, and the window goes away a moment later. A
    /// test that types the next key immediately is typing it *into the panel* — which no person is
    /// fast enough to do, and every test is.
    fn wait_closed(&mut self, name: &str) {
        let gone = |s: &Session| {
            let bufs = s.buffers_named(name);
            !bufs.is_empty() && bufs.iter().all(|b| s.windows_for(*b).is_empty())
        };
        assert!(self.pump(gone), "{name} never closed\n{}", self.transcript());
    }

    /// Windows currently showing `buf`.
    fn windows_for(&self, buf: u64) -> Vec<u64> {
        let open = self.open_windows();
        self.events
            .iter()
            .filter(|e| e["type"] == "window_opened" && e["buf"].as_u64() == Some(buf))
            .filter_map(|e| e["win"].as_u64())
            .filter(|w| open.contains(w))
            .collect()
    }

    /// Every event this frontend has been sent, verbatim.
    ///
    /// The only way to assert a *negative* about the wire: that something the host knows never
    /// appears in what it sends. Rendered text is not enough — a value could be in a field nothing
    /// draws and still be in the JSON a frontend logs.
    fn wire(&self) -> String {
        self.events.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("\n")
    }

    fn transcript(&self) -> String {
        let mut s = String::from("--- text seen ---\n");
        for l in self.texts() {
            s.push_str(&format!("  {l}\n"));
        }
        s
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

#[test]
fn the_bundled_plugins_load_without_a_config_or_a_plugin_directory() {
    // They ship inside the binary, so a first run with an empty config directory still has them.
    let sb = Sandbox::new("load");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    assert!(s.saw("PROJECTS"), "the sidebar rendered\n{}", s.transcript());
}

#[test]
fn a_bundled_plugin_can_be_switched_off_from_config() {
    let sb = Sandbox::new("disable");
    sb.write_config("[options]\n\"plugins.disabled\" = [\"sidebar\"]\n");
    let mut s = sb.start();
    // Wait for something the *other* bundled plugin does, so this is not just a race with startup.
    s.drain_for(Duration::from_secs(3));
    assert!(
        s.buffer_named("[sidebar]").is_none(),
        "the sidebar should not have been created\n{}",
        s.transcript()
    );
    // And the rest still loaded: disabling one plugin must not disable them all.
    s.ctrl("p");
    s.wait_for("Model");
}

// ---------------------------------------------------------------------------
// Sidebar
// ---------------------------------------------------------------------------

#[test]
fn the_branch_you_are_on_is_on_screen() {
    // In the footer rather than the panel: it is one line, it is true of the whole workspace, and
    // the footer is the part you can still see with the sidebar hidden.
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("branch");
    sb.git_init();
    let mut s = sb.start();
    s.wait_for("trunk");
}

#[test]
fn the_sidebar_toggles_and_the_second_toggle_puts_it_back() {
    let sb = Sandbox::new("toggle");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    let buf = s.buffer_named("[sidebar]").expect("sidebar buffer");
    assert_eq!(s.windows_for(buf).len(), 1, "open at startup");

    s.ctrl("b");
    assert!(s.pump(|s| s.windows_for(buf).is_empty()), "closes\n{}", s.transcript());

    s.ctrl("b");
    assert!(s.pump(|s| !s.windows_for(buf).is_empty()), "reopens\n{}", s.transcript());
}

/// Reloading the configuration must leave one sidebar, not two.
///
/// `^R` unloads the plugin and loads it again. The reloaded copy asks for a panel per terminal —
/// `view.onOpen` fires for the ones already here — but nothing took the old copy's column off the
/// screen: the panel's `dispose` deliberately does not close a window, because its only caller was
/// a terminal that had already gone and taken its windows with it, and on this path nothing called
/// `dispose` at all. So the workspace got a second sidebar beside the first, and the first was
/// drawn by code that no longer existed and answered no keys.
///
/// Counted across *every* `[sidebar]` buffer rather than the first one, because the whole symptom
/// is a second buffer with a second window on it — a test that looked at one buffer would have
/// been perfectly happy.
#[test]
fn reloading_the_config_leaves_one_sidebar_rather_than_two() {
    let sb = Sandbox::new("reload-dup");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    let before: usize =
        s.buffers_named("[sidebar]").iter().map(|b| s.windows_for(*b).len()).sum();
    assert_eq!(before, 1, "one to start with\n{}", s.transcript());

    s.send(&command("config.reload"));
    // The panel has to come back — a reload that closed the old column and never opened a new one
    // would pass a naive "not two" assertion while leaving somebody with no sidebar at all.
    assert!(
        s.pump(|s| {
            let open: usize =
                s.buffers_named("[sidebar]").iter().map(|b| s.windows_for(*b).len()).sum();
            open == 1 && s.buffers_named("[sidebar]").len() > 1
        }),
        "one sidebar after the reload, on a freshly built panel\n{}",
        s.transcript()
    );

    // And it settles there: the old window is closed rather than merely drawn over, so nothing
    // arrives a moment later to make it two again.
    s.drain_for(Duration::from_secs(2));
    let after: usize = s.buffers_named("[sidebar]").iter().map(|b| s.windows_for(*b).len()).sum();
    assert_eq!(after, 1, "still one\n{}", s.transcript());
}

#[test]
fn the_sidebar_stays_shut_when_config_says_so() {
    let sb = Sandbox::new("closed");
    sb.write_config("[options]\n\"sidebar.open\" = false\n");
    let mut s = sb.start();
    // The plugin still loads and still creates its buffer — it just does not open a window.
    assert!(s.pump(|s| s.buffer_named("[sidebar]").is_some()), "plugin loaded\n{}", s.transcript());
    let buf = s.buffer_named("[sidebar]").expect("buffer");
    s.drain_for(Duration::from_secs(2));
    assert!(s.windows_for(buf).is_empty(), "no window\n{}", s.transcript());
}

#[test]
fn outside_a_repository_the_sidebar_still_lists_the_project() {
    // `work/` is deliberately not a git repository here. Working outside version control is an
    // ordinary state, and a panel that renders an exception instead of your conversations would be
    // worse than one that renders less.
    let sb = Sandbox::new("norepo");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.drain_for(Duration::from_secs(1));
    let panel = s.sidebar_now();
    assert!(
        panel.iter().any(|l| l.contains("New conversation")),
        "the conversation is listed even with no repository\n{panel:?}"
    );
}

// ---------------------------------------------------------------------------
// Model and effort switchers
// ---------------------------------------------------------------------------

#[test]
fn the_model_picker_opens_and_lists_what_is_reachable() {
    let sb = Sandbox::new("modelpick");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("p");
    s.wait_for("Model");
    let rows = s.picker_now();
    // Two panes: providers on the left under the heading for what they cost, their models on the
    // right. The heading is the whole point — a plan and a key are different things to spend, and
    // one flat list could not say which a row was.
    assert!(rows.iter().any(|l| l.contains("LOCAL")), "grouped by account\n{rows:?}");
    assert!(
        rows.iter().any(|l| l.contains("Mock") && l.contains('\u{2502}')),
        "a rail entry and a model row on the same line\n{rows:?}"
    );
    // The model id is still there: it is unique per instance and not globally, so a picker that
    // dropped it would have to guess which endpoint a row means.
    assert!(rows.iter().any(|l| l.contains("mock")), "{rows:?}");
}

/// A second provider with two models, so the rail has somewhere to move to and the pane has
/// somewhere to move *within* — which is how "did the focus actually change panes" is answerable.
const OTHER: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("other", [{
    id: "other", driver: "other", display_name: "Other",
    models: [
      { id: "other-1", display_name: "Other One" },
      { id: "other-2", display_name: "Other Two" },
    ],
  }], async (_req, emit) => { emit({ type: "message_stop" }); });
  neosh.event.on("neosh.ready", () => neosh.notify("other ready"));
}
"#;

fn install_other(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/other");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"other\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), OTHER).expect("plugin");
}

/// Moving into the rail and down it changes which provider's models are on the right.
///
/// Regression, and a nasty one: `<Tab>` is deliberately shared between `complete` and `pane_next`
/// on the reasoning that no widget has both. But the key was resolved against every action in a
/// fixed order regardless of which widget was asking, so `<Tab>` in the model picker came back as
/// `complete` — an action a rail picker has no case for — and did nothing at all. The second pane
/// was on screen, named in the shortcut row, and unreachable.
#[test]
fn the_rail_is_reachable_and_choosing_a_provider_lists_its_models() {
    let sb = Sandbox::new("railmove");
    install_other(&sb);
    let mut s = sb.start();
    s.wait_for("other ready");
    s.wait_for("PROJECTS");
    s.ctrl("p");
    s.wait_for("Model");

    // The shortcut row says how to get there, so it had better be true.
    assert!(
        s.picker_now().iter().any(|l| l.contains("providers")),
        "the picker says the rail is reachable\n{:?}",
        s.picker_now()
    );

    // Back a pane, into the rail; down it, onto the other provider; then *forward* a pane with
    // `<Tab>` and down again. If `<Tab>` reached the pane, that last press moved the model cursor.
    // If it did not, it moved the rail instead — off the end, leaving the cursor on the first
    // model of a provider whose list nobody asked to change.
    s.special("back_tab");
    s.special("down");
    assert!(
        s.pump(|s| s.picker_now().iter().any(|l| l.contains("Other One"))),
        "moving down the rail listed the other provider\n{:?}",
        s.picker_now()
    );
    s.special("tab");
    s.special("down");
    assert!(
        s.pump(|s| s
            .picker_now()
            .iter()
            .any(|l| l.contains("Other Two") && l.contains('\u{276f}'))),
        "and the cursor is on the second model, so <Tab> reached the pane\n{:?}",
        s.picker_now()
    );
}

/// A model you named yourself can be taken back out.
///
/// `n` existed and `d` did not, which meant a mistyped id was a row you had to read past forever —
/// and the most likely thing to be in a hand-typed list of model ids is a typo.
#[test]
fn a_model_you_added_yourself_can_be_removed_again() {
    let sb = Sandbox::new("customdel");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("p");
    s.wait_for("Model");

    // `^A`, then the id, then the name it shows as.
    s.ctrl("a");
    assert!(s.pump(|s| !s.prompt_now().is_empty()), "the id prompt opened");
    s.type_text("acme-1");
    s.enter();
    assert!(
        s.pump(|s| s.prompt_now().iter().any(|l| l.contains("Show it as"))),
        "then it asks what to call it\n{:?}",
        s.prompt_now()
    );
    s.type_text("!");
    s.enter();
    assert!(
        s.pump(|s| s.picker_now().iter().any(|l| l.contains("acme-1") && l.contains("yours"))),
        "and it is in the list, marked as yours\n{:?}",
        s.picker_now()
    );

    // Onto it — it sorts above the provider's own models — and out. Asked about first: you named
    // this one yourself, so nothing will offer it back.
    s.special("up");
    s.ctrl("d");
    assert!(
        s.pump(|s| s.saw("Remove acme-1")),
        "it asks before taking it away\n{}",
        s.transcript()
    );
    s.key("y");
    assert!(
        s.pump(|s| !s.picker_now().iter().any(|l| l.contains("acme-1"))),
        "and out again\n{:?}",
        s.picker_now()
    );
    assert!(
        s.saw("removed"),
        "and it says so"
    );
}

/// The filter can contain any letter, including the ones the picker's own actions used to take.
///
/// Regression: sign-in, refresh, add and remove were on bare `s`, `r`, `n` and `d`, checked before
/// the filter ever saw the key. So typing "sonnet" opened a sign-in prompt on the `s`, and there
/// was no way to search a long model list for anything whose name contained one of four very
/// common letters.
#[test]
fn typing_a_model_name_filters_rather_than_firing_a_command() {
    let sb = Sandbox::new("filterletters");
    install_other(&sb);
    let mut s = sb.start();
    s.wait_for("other ready");
    s.wait_for("PROJECTS");
    s.ctrl("p");
    s.wait_for("Model");
    // Onto the provider with two models to tell apart.
    s.special("back_tab");
    s.special("down");
    assert!(s.pump(|s| s.picker_now().iter().any(|l| l.contains("Other Two"))));
    s.special("tab");

    // Every letter here is one that used to be a command.
    s.type_text("nd");
    assert!(
        s.pump(|s| {
            let rows = s.picker_now();
            rows.iter().any(|l| l.contains("> nd"))
        }),
        "the letters went into the filter\n{:?}",
        s.picker_now()
    );
    assert!(
        s.prompt_now().is_empty() && !s.saw("asking again"),
        "and fired nothing: {:?}",
        s.prompt_now()
    );
}

/// `d` on a model the provider serves is not a deletion. It is not ours to delete, and saying
/// which of the two a row is beats doing nothing at all.
#[test]
fn d_on_a_model_the_provider_serves_explains_rather_than_deleting() {
    let sb = Sandbox::new("customdel2");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("p");
    s.wait_for("Model");
    let before = s.picker_now();

    s.ctrl("d");
    assert!(
        s.pump(|s| s.saw("nothing of yours to remove")),
        "it says which kind of row this is"
    );
    assert_eq!(s.picker_now(), before, "and nothing moved");
}

/// A picker says which keys it answers to.
///
/// Every one of them is a list you arrived at by pressing something, and none of them used to say
/// what to press next — which makes a modal you have to guess your way out of.
#[test]
fn a_picker_says_how_to_use_it() {
    let sb = Sandbox::new("pickerhints");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("k");
    // On the border, not in the list. A strip written as the last row of the buffer is a row of
    // height every picker pays for on every screen, and on a short one it is the row that does not
    // fit — which left the one panel you have to be told how to leave as the one with no legend.
    assert!(
        s.pump(|s| s.footer_of("[Go to]").is_some_and(|f| f.contains("choose"))),
        "the key strip is there\n{:?}",
        s.footer_of("[Go to]")
    );
    let strip = s.footer_of("[Go to]").expect("found above");
    assert!(strip.contains("close"), "and how to get out again: {strip:?}");
}

/// Written from the bindings, not from a string, so rebinding one does not make the strip a lie.
#[test]
fn the_key_strip_says_the_keys_that_are_actually_bound() {
    let sb = Sandbox::new("pickerhintsbound");
    sb.write_config("[options]\n\"ui.keys.accept\" = \"<C-y>\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("PROJECTS");
    s.ctrl("k");
    assert!(
        // `^Y`, capitalised: nobody presses shift to send it, and the capital is how a terminal
        // has spelled a chord since curses. Read off the border, which is where a legend lives now.
        s.pump(|s| s.footer_of("[Go to]").is_some_and(|f| f.contains("^Y choose"))),
        "the strip follows the setting\n{:?}",
        s.footer_of("[Go to]")
    );
}

#[test]
fn a_picker_owns_its_navigation_keys_while_it_is_open() {
    // `<C-n>` is bound globally to "new conversation". A raw capture only receives keys that no
    // binding claimed, so without window-scoped bindings, pressing it inside a picker started a
    // new conversation and took the picker off screen — while you were in the middle of typing a
    // filter. This is the fix, and the reason the fix has to be scoped rather than global.
    let sb = Sandbox::new("pickerkeys");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    let before = s.sidebar_now().iter().filter(|l| l.contains("New conversation")).count();
    let windows = s.open_windows().len();
    s.ctrl("p");
    s.wait_for("Model");
    s.ctrl("n");
    s.drain_for(Duration::from_secs(1));
    assert_eq!(
        s.sidebar_now().iter().filter(|l| l.contains("New conversation")).count(),
        before,
        "no conversation was created\n{:?}",
        s.sidebar_now()
    );

    // And the binding comes back the moment the picker is gone — the scope is the window, not the
    // session, so nothing is left holding a key it no longer needs. Waiting for the window to
    // actually close first: closing is a round trip to the plugin thread, and a key sent before it
    // lands is still the picker's.
    s.special("esc");
    assert!(s.pump(|s| s.open_windows().len() == windows), "the picker closed");
    s.ctrl("n");
    assert!(
        s.pump(|s| {
            s.sidebar_now().iter().filter(|l| l.contains("New conversation")).count() > before
        }),
        "the global binding is back\n{:?}",
        s.sidebar_now()
    );
}

#[test]
fn the_navigation_keys_can_be_rebound() {
    let sb = Sandbox::new("rebind");
    sb.write_config("[options]\n\"ui.keys.next\" = \"<C-f>\"\n");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.ctrl("p");
    s.wait_for("Model");
    // `<C-f>` now moves. There is one model in the mock catalogue, so what this proves is that the
    // key reaches the picker at all rather than falling through to the composer.
    s.ctrl("f");
    s.drain_for(Duration::from_secs(1));
    assert!(
        !s.composer_now().iter().any(|l| l.contains('\u{6}')),
        "the rebound key was consumed by the picker\n{:?}",
        s.composer_now()
    );
    s.special("esc");
}

#[test]
fn typing_filters_the_picker_and_escape_leaves_the_model_alone() {
    let sb = Sandbox::new("filter");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("p");
    s.wait_for("Mock");

    // A query that cannot match anything must say so rather than silently showing everything.
    for c in ["z", "z", "z"] {
        s.key(c);
    }
    s.wait_for("no matches");

    s.special("esc");
    s.drain_for(Duration::from_secs(1));
    assert!(!s.saw("model: "), "dismissing must not change the selection\n{}", s.transcript());
}

#[test]
fn choosing_a_model_reports_the_new_selection() {
    let sb = Sandbox::new("choose");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("p");
    s.wait_for("Mock");
    s.special("enter");
    s.wait_for("model: mock/Mock");
}

#[test]
fn a_model_with_no_options_says_so_instead_of_opening_an_empty_panel() {
    // The mock driver exposes no `ProviderOptionDescriptor`s, so the switcher has nothing to offer.
    // Opening an empty panel and making the user press Escape would be worse than a message — and
    // the message names the model, because the answer to "why not" is almost always "not that one".
    let sb = Sandbox::new("effort");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("e");
    s.wait_for("has nothing to set");
}

// ---------------------------------------------------------------------------
// Git
// ---------------------------------------------------------------------------

#[test]
fn git_status_renders_the_working_tree() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("gitstatus");
    sb.git_init();
    std::fs::write(sb.work().join("new.txt"), "x\n").expect("write");
    std::fs::write(sb.work().join("README.md"), "changed\n").expect("write");

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("g");
    // The two-column `XY` prefix git itself prints: staged state, then working tree. `?` for
    // untracked and `M` for modified — git's own letters. `U` here would claim a conflict.
    s.wait_for(" ?  new.txt");
    assert!(s.saw(" M  README.md"), "modified file listed\n{}", s.transcript());
}

#[test]
fn outside_a_repository_git_status_warns_rather_than_throwing() {
    let sb = Sandbox::new("gitnorepo");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("g");
    s.wait_for("no git repository");
}

impl Sandbox {
    /// Give `work/` an `origin` that is three commits ahead of it.
    ///
    /// A bare repository on disk rather than a network remote: `git fetch` against a path is the
    /// same code path, the same refs and the same `behind` count, and it is the only version of
    /// this that a test can run without a server.
    ///
    /// The commits are pushed from a *second* clone, so `work/` genuinely does not have them —
    /// which is the whole point. Committing them in `work` and resetting would leave the objects
    /// present and the fetch with nothing to bring.
    fn git_behind_by(&self, commits: usize) {
        let run = |dir: &Path, args: &[&str]| {
            let out = Command::new("git").current_dir(dir).args(args).output().expect("git runs");
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        let origin = self.root.join("origin.git");
        let other = self.root.join("other");
        run(&self.work(), &["init", "--bare", "--initial-branch=trunk", &origin.to_string_lossy()]);
        run(&self.work(), &["remote", "add", "origin", &origin.to_string_lossy()]);
        run(&self.work(), &["push", "--set-upstream", "origin", "trunk"]);
        run(&self.root, &["clone", &origin.to_string_lossy(), &other.to_string_lossy()]);
        run(&other, &["config", "user.email", "test@neosh.invalid"]);
        run(&other, &["config", "user.name", "neosh test"]);
        run(&other, &["config", "commit.gpgsign", "false"]);
        for i in 0..commits {
            std::fs::write(other.join("README.md"), format!("upstream {i}\n")).expect("write");
            run(&other, &["commit", "-am", &format!("upstream {i}")]);
        }
        run(&other, &["push", "origin", "trunk"]);
    }
}

/// How far behind is on the project's own row, and there is no block above it.
///
/// The claim is the same one the block used to make — that a panel about a repository has to have
/// *asked* a remote, because `behind` out of `git status` is a comparison with the remote-tracking
/// ref on this disk — but it is now made where the repository already is. A `GIT` heading with the
/// branch and a verb under it was a fourth and fifth row about one of the rows below it, on a
/// column whose whole job is your conversations.
#[test]
fn how_far_behind_is_on_the_project_row_and_there_is_no_git_block() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("gitsidebar");
    sb.git_init();
    sb.git_behind_by(3);

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    // The count, on the row the repository is already on. `↓3` rather than `pull 3 commits`,
    // because a row is a glance and a sentence is a thing you read.
    assert!(
        s.pump(|s| s.projects_now().iter().any(|l| l.contains("↓3"))),
        "the project row carries what the fetch found\n{:?}",
        s.sidebar_now()
    );
    // And nothing above the list is about git any more.
    let rows = s.sidebar_now();
    let head = rows.iter().position(|l| l.contains("PROJECTS")).unwrap_or(0);
    assert!(
        !rows.iter().any(|l| l.contains("GIT")),
        "no git heading anywhere\n{rows:?}"
    );
    assert!(
        !rows[..head].iter().any(|l| l.contains("trunk") || l.contains("pull 3 commits")),
        "and nothing about the branch above the projects\n{rows:?}"
    );
}

/// `p` on the project row pulls, and says what git said.
///
/// The verb moved onto the key when the block went. That is not a smaller thing than it was: the
/// key used to run `git pull` and report whatever came back, so the *choice* a diverged branch
/// needs — rebase or merge, which do different things to your own commits — lived only on a row
/// that no longer exists. `pullRepository` is the whole verb now, and `p` is one of its three doors.
#[test]
fn p_on_a_project_row_pulls_and_reports_what_happened() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("gitpullrow");
    sb.git_init();
    sb.git_behind_by(2);

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    assert!(
        s.pump(|s| s.projects_now().iter().any(|l| l.contains("↓2"))),
        "two waiting, on the row\n{:?}",
        s.sidebar_now()
    );
    s.enter_panel();
    // The first row of the list is the project itself — no block above it to step past any more.
    s.key("g");
    s.key("g");
    s.key("p");
    // What git said, as a notification. It used to be a row *in* the block, held for six seconds;
    // with the block gone the answer to a key you just pressed is a reply, which is exactly what
    // the notification vocabulary calls this and does not stack.
    assert!(
        s.pump(|s| s.saw("Fast-forward")),
        "it says what git said\n{}",
        s.transcript()
    );
    let head = Command::new("git")
        .current_dir(sb.work())
        .args(["log", "--oneline", "-1", "--format=%s"])
        .output()
        .expect("git runs");
    assert_eq!(
        String::from_utf8_lossy(&head.stdout).trim(),
        "upstream 1",
        "and the commits actually arrived"
    );
    // And the row follows, with no key pressed. `↓2` after a pull that brought both commits in
    // was a row that stayed wrong until something unrelated redrew it — moving the cursor,
    // switching conversations, the next turn ending — and what it was wrong about is the one
    // number the key was pressed for. The verb tells the badge itself, so this is immediate rather
    // than whenever the next redraw happens to come round.
    assert!(
        s.pump(|s| !s.projects_now().iter().any(|l| l.contains("↓2"))),
        "the row stops saying behind the moment the pull is done\n{:?}",
        s.sidebar_now()
    );
    // And nothing is left spinning on it: the busy mark leaves with the operation.
    assert!(
        !s.projects_now().iter().any(|l| l.contains('•') || l.contains('⠋')),
        "no busy mark once the pull is over\n{:?}",
        s.sidebar_now()
    );
}

// ---------------------------------------------------------------------------
// Generated branch names — the whole point of `neosh.gen`
// ---------------------------------------------------------------------------

impl Sandbox {
    /// Start with a specific recorded turn, for the generation tests.
    fn start_with(&self, fixture: &Path) -> Session {
        self.spawn(fixture, Some("mock/mock"))
    }

    /// Start without `--model`, so `agent.model` from config actually decides.
    ///
    /// `--model` is applied last on purpose — a flag that lost to a config file is an hour of
    /// someone's life — which means a test about config-chosen models must not pass it.
    fn start_letting_config_choose(&self) -> Session {
        self.spawn(&fixture(), None)
    }

    fn spawn(&self, script: &Path, model: Option<&str>) -> Session {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_neosh"));
        cmd.args(["--ui-protocol", "stdio"])
            .arg("--config-dir")
            .arg(self.root.join("config"))
            .arg("--cwd")
            .arg(self.work())
            .args(["--mock-script", &script.display().to_string()])
            .env("NEOSH_STATE_DIR", self.root.join("state"))
            // `$SHELL` and nothing cleverer is right in the product and useless in a test: it is a
            // person's own shell with their own configuration in it, and the first run of a shell
            // test against a real login shell was answered by an `oh-my-zsh` update prompt that ate
            // a keystroke. A test that depends on whose machine it runs on is a test that will fail
            // for somebody else. `Term::spawn_shell` exists for this reason one level down; a pty
            // opened for a *peer* has no such seam, so the environment is where it is said.
            .env("SHELL", "/bin/sh")
            // The history block reads the *vendors'* transcripts rather than ours — see
            // `crate::usage`. Pointed at this sandbox, because the alternative is a hundred and
            // forty-seven neoshes scanning a real month of somebody's work and asserting on it.
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude"))
            .env("CODEX_HOME", self.root.join("codex"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(m) = model {
            cmd.args(["--model", m]);
        }
        Session::wrap(cmd.spawn().expect("neosh should start"))
    }
}

fn branch_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/branch_name.jsonl")
}

impl Session {
    /// Type a string one key at a time, as a user would.
    fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            self.key(&c.to_string());
        }
    }
}

#[test]
fn a_branch_name_is_generated_slugged_and_prefixed_from_config() {
    if !have_git() {
        return;
    }
    // The fixture answers with `"Fix the Login Redirect!"` wrapped in a ```json fence and a
    // preamble — which is what models actually do. Three separate things have to work for this to
    // pass: the host finding JSON inside prose, the plugin slugging it into a legal refname, and
    // the configured prefix surviving that slugging.
    let sb = Sandbox::new("branchgen");
    sb.git_init();
    sb.write_config("[options]\n\"git.branch.prefix\" = \"feature/\"\n");

    let mut s = sb.start_with(&branch_fixture());
    s.wait_for("PROJECTS");

    s.send(&command("git.branch.new"));
    s.wait_for("What are you about to work on?");
    s.type_text("the login page bounces");
    s.special("enter");

    // The proposed name is shown for approval before the branch exists.
    s.wait_for("feature/fix-the-login-redirect");
}

/// A prefix the user configured and a type the model chose are the same decision, made twice.
///
/// The built-in prompt asks for `fix/`, `feature/`, `chore/` and the rest, so a
/// `git.branch.prefix` bolted on unconditionally reads `feature/fix/the-login-redirect` — a
/// namespace nobody meant, under a type that is now wrong. The prefix is for a name that arrived
/// without one, which is what it does here and what it still does in the test above.
#[test]
fn a_configured_prefix_does_not_stack_on_a_type_the_model_chose() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("branchtyped");
    sb.git_init();
    sb.write_config("[options]\n\"git.branch.prefix\" = \"feature/\"\n");

    let mut s = sb.start_with(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/branch_name_typed.jsonl"),
    );
    s.wait_for("PROJECTS");

    s.send(&command("git.branch.new"));
    s.wait_for("What are you about to work on?");
    s.type_text("the login page bounces");
    s.special("enter");

    s.wait_for("fix/the-login-redirect");
    assert!(
        !s.saw("feature/fix/the-login-redirect"),
        "and the configured prefix was not stacked on top of it\n{}",
        s.transcript()
    );
}

/// `git.branch.model` is a model, and an unset one is the fallback rather than a guess.
///
/// The option is `instance/model`, parsed in the plugin and handed to `gen.complete` as a
/// selection — so a malformed or absent one has to mean "you decide" all the way down to the host,
/// which then tries `gen.model` and finally the conversation's own. A plugin that filled in a
/// default here would be a third place the precedence is written, and the first to disagree.
#[test]
fn the_model_that_names_a_branch_is_a_setting() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("branchmodel");
    sb.git_init();
    sb.write_config("[options]\n\"git.branch.model\" = \"mock/mock\"\n");

    let mut s = sb.start_with(&branch_fixture());
    s.wait_for("PROJECTS");

    s.send(&command("git.branch.new"));
    s.wait_for("What are you about to work on?");
    s.type_text("the login page bounces");
    s.special("enter");

    // Named at all is the assertion: a selection the host could not resolve fails the call, and
    // the plugin says so instead of proposing a name.
    s.wait_for("fix-the-login-redirect");
}

/// A scratch worktree is named by the first thing asked in it, and the panel says so.
///
/// The whole shape of this: `git.worktree.new.inside` gives you `brisk-otter` because naming a
/// branch before the work is a decision made at the worst possible moment — and the first message
/// *is* that decision, arriving on its own. Three things have to happen and the third is the one
/// that was missing: git renames the branch, the host notices that the label it wrote down when it
/// first saw the directory is now wrong, and the sidebar row — which is that label — redraws.
#[test]
fn a_scratch_worktree_is_named_by_the_first_message_and_the_sidebar_follows() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("branchauto");
    sb.git_init();

    let mut s = sb.start_with(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/branch_name_typed.jsonl"),
    );
    s.wait_for("PROJECTS");

    s.send(&command("git.worktree.new.inside"));
    let under = sb.work().join(".worktrees");
    assert!(
        s.pump(|_| std::fs::read_dir(&under).is_ok_and(|d| d.count() > 0)),
        "the worktree exists, on a name nobody chose"
    );
    let tree = std::fs::read_dir(&under)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .next()
        .expect("one worktree");

    // Whatever it landed on. Named for us, which is exactly what makes it replaceable — two words
    // so it can be read out, and a tag so the twentieth one is not a name already on disk.
    let scratch = head_of(&tree);
    assert_eq!(
        scratch.split('-').count(),
        3,
        "two words and a tag, not {scratch:?} — see `scratchName`"
    );

    // Waited for rather than assumed: creating the tree also starts a conversation in it and
    // switches to it, and a test that types before that lands is typing into the conversation in
    // the *main* checkout — where there is no scratch branch and nothing to rename.
    let row_says = |s: &Session, want: &str| s.sidebar_now().iter().any(|l| l.contains(want));
    let named_scratch = scratch.clone();
    assert!(
        s.pump(move |s| row_says(s, &named_scratch)),
        "the worktree is a row in the panel, under the name nobody chose\n{:?}",
        s.sidebar_now()
    );

    // The first thing asked in it. The turn and the naming call both read the same fixture, so
    // which of them the mock answers first does not decide the test.
    s.type_text("the login page bounces");
    s.special("enter");

    assert!(
        s.pump(|_| head_of(&tree) == "fix/the-login-redirect"),
        "git renamed the branch — it is on {:?}\n{}",
        head_of(&tree),
        s.transcript()
    );
    // And the row followed. Without the host putting right the label it wrote down the first time
    // it saw this directory, the panel says the scratch name until the workspace restarts.
    //
    // Matched on the start of the name, because the row is a branch in a narrow column and the
    // panel elides the end of it — `⎇ fix/the-login-redire…`. Asserting the whole string would be
    // asserting the sidebar's width.
    assert!(
        s.pump(move |s| row_says(s, "⎇ fix/the-login")),
        "the sidebar row followed it\n{:?}",
        s.sidebar_now()
    );
    // And it is gone from the *whole* column, not merely replaced on the row above. The block at
    // the top names the branch of the conversation you are in, which is this worktree — so a rename
    // that only reached the project row would leave the panel saying two different things about one
    // checkout. Pumped rather than read once: the row is put right by the host inside the rename
    // call and the block by a plugin re-reading `git status` afterwards, so they settle a round trip
    // apart and which one you catch first is a matter of scheduling.
    let gone = scratch.clone();
    assert!(
        s.pump(move |s| !s.sidebar_now().iter().any(|l| l.contains(&gone))),
        "and the scratch name is not still anywhere in the column\n{:?}",
        s.sidebar_now()
    );
}

/// A directory in the way does not cost a worktree its branch.
///
/// The case this exists for is the one the scratch names created: a tree is made at
/// `.worktrees/brisk-otter` and the first message renames its *branch* to `fix/the-login`, leaving
/// the directory where it is on purpose — moving it would invalidate the conversation's `cwd`. So
/// `git branch` no longer mentions `brisk-otter` while the directory sits there forever, and the
/// next tree to land on that name asked git for something git refused: `already exists`, on a name
/// nothing on screen had said was taken.
///
/// Squatted by hand here rather than by renaming a generated branch, because which two words the
/// generator picks is not a thing a test may depend on. The branch is what you asked for; where its
/// checkout sits is the plugin's business, so the counter goes on the directory.
#[test]
fn a_directory_in_the_way_moves_the_worktree_not_the_branch() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("worktreesquat");
    sb.git_init();
    // Relative, so the trees land inside the sandbox's own repository rather than beside it.
    sb.write_config("[options]\n\"worktree.root\" = \".worktrees\"\n");

    // Non-empty: `git worktree add` is happy to move into an empty directory, and a real leftover
    // is a whole checkout.
    let squat = sb.work().join(".worktrees/feat-thing");
    std::fs::create_dir_all(&squat).expect("mkdir");
    std::fs::write(squat.join("leftover"), "from a tree that has been renamed\n").expect("write");

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.send(&command_with("git.worktree.new", "feat/thing"));

    let moved = sb.work().join(".worktrees/feat-thing-2");
    assert!(
        s.pump(|_| moved.join(".git").exists()),
        "the worktree landed beside the squatter, at {}",
        moved.display()
    );
    assert_eq!(head_of(&moved), "feat/thing", "on the branch that was asked for");
    assert!(squat.join("leftover").exists(), "and nothing touched what was already there");
}

/// And named when the model answers with the name and nothing else.
///
/// The prompt asks for `{"branch": …}` and gets the object most of the time. The rest of the time
/// — two runs in twenty-two, measured on a real workspace's own history — it gets
/// `fix/the-login-redirect` on its own: the name it asked for, without the envelope. That was read
/// as a failed request and swallowed by a `log.info` nothing prints, so the worktree kept the name
/// nobody chose, for good, with the name it should have had sitting in the reply.
///
/// Deliberately end to end rather than a unit test of the parse: what broke was a *shape* crossing
/// four layers — driver, host, the API's `gen.field`, and the plugin that acts on it — and every
/// one of them could have thrown the answer away on its own.
#[test]
fn a_branch_name_without_its_json_envelope_still_names_the_branch() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("branchbare");
    sb.git_init();

    let mut s = sb.start_with(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/branch_name_bare.jsonl"),
    );
    s.wait_for("PROJECTS");

    s.send(&command("git.worktree.new.inside"));
    let under = sb.work().join(".worktrees");
    assert!(
        s.pump(|_| std::fs::read_dir(&under).is_ok_and(|d| d.count() > 0)),
        "the worktree exists, on a name nobody chose"
    );
    let tree = std::fs::read_dir(&under)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .next()
        .expect("one worktree");

    let scratch = head_of(&tree);
    assert!(scratch.contains('-'), "a two-word scratch name, not {scratch:?}");

    // Waited for, for the reason the test above says: typing before the new conversation lands
    // types into the one in the main checkout, where there is no scratch branch to rename.
    let named_scratch = scratch.clone();
    assert!(
        s.pump(move |s| s.sidebar_now().iter().any(|l| l.contains(&named_scratch))),
        "the worktree is a row in the panel\n{:?}",
        s.sidebar_now()
    );

    s.type_text("the login page bounces");
    s.special("enter");

    assert!(
        s.pump(|_| head_of(&tree) == "fix/the-login-redirect"),
        "a bare answer is the answer — the branch is on {:?}\n{}",
        head_of(&tree),
        s.transcript()
    );
}

/// Moving a worktree takes the conversations in it with it.
///
/// The git call is the easy half and is covered in `neosh-vcs`. What this is about is the half a
/// `git worktree move` on its own cannot do: a worktree's path is the key of the facts every list
/// draws, of the conversations living there, and of the project vars holding somebody's pin — so a
/// move that only moved the directory would leave the panel pointing at nothing and the agent
/// running somewhere that is not there. `Host::relocated` is what this asserts, one field at a
/// time.
#[test]
fn moving_a_worktree_takes_its_conversations_and_its_arrangement_with_it() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("wtmove");
    sb.git_init();
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    // A tree inside the repository, with a conversation in it — which is what `new.inside` makes:
    // it creates the worktree and lands you in it.
    s.send(&command("git.worktree.new.inside"));
    let under = sb.work().join(".worktrees");
    assert!(
        s.pump(|_| std::fs::read_dir(&under).is_ok_and(|d| d.count() > 0)),
        "the worktree exists"
    );
    // Resolved, because the workspace's copy of this path came from git and git follows symlinks:
    // on macOS the sandbox is under `/tmp`, which is `/private/tmp`, and a worktree named by the
    // unresolved spelling is a worktree the panel has never heard of. The product compares paths
    // as strings — as removing one does — so it is the test's job to name it the way git does.
    let from = std::fs::read_dir(&under)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .next()
        .and_then(|p| std::fs::canonicalize(p).ok())
        .expect("one worktree");
    let branch = head_of(&from);
    // Waited for, not assumed: creating the tree also starts a conversation in it, and asserting
    // before that lands is asserting about the main checkout.
    let leaf = from.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        s.pump({
            let leaf = leaf.clone();
            move |s| s.sidebar_now().iter().any(|l| l.contains(leaf.trim_end_matches('/')))
                || s.sidebar_now().iter().any(|l| l.contains(&branch))
        }),
        "the tree is a row in the panel\n{:?}",
        s.sidebar_now()
    );

    // Out of the repository, to a sibling of it. Both paths given, so this is the scripted form
    // and no picker is involved — the picker is a separate test, and what is under test here is
    // what happens *after* a destination has been chosen.
    let to = std::fs::canonicalize(&sb.root).unwrap_or_else(|_| sb.root.clone()).join("moved-tree");
    s.send(&format!(
        r#"{{"type":"command","name":"git.worktree.move","args":["{}","{}"]}}"#,
        from.display(),
        to.display(),
    ));

    assert!(
        s.pump(|_| to.join(".git").exists() && !from.exists()),
        "the checkout moved and left nothing behind\n{:?}",
        s.texts()
            .iter()
            .filter(|t| t.contains("worktree") || t.contains("moved") || t.contains("git"))
            .collect::<Vec<_>>()
    );

    // The conversation followed. This is the field nothing else in the workspace rewrites, and
    // without it the agent's tools would resolve against a directory that no longer exists.
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("moved-tree"))
            || s.sidebar_now().iter().any(|l| l.contains(&head_of(&to)))),
        "the panel is drawing the tree where it is now\n{:?}",
        s.sidebar_now()
    );

    // And the old path is not still a row beside it. A move that reads as "one project went and
    // another arrived" is the bug `PluginEvent::ProjectMoved` exists to prevent: the list would
    // keep both, and the cursor would land on a directory that is not there.
    let stale = from.display().to_string();
    assert!(
        !s.sidebar_now().iter().any(|l| l.contains(&stale)),
        "and the directory it left is not still listed\n{:?}",
        s.sidebar_now()
    );

    // The project vars came across — this is the pin, the fold and the rank. Read off disk rather
    // than through the panel, because what is being asserted is that the *store* re-keyed rather
    // than that one panel happened to redraw.
    let vars = std::fs::read_to_string(sb.root.join("state/vars.json")).unwrap_or_default();
    assert!(
        !vars.contains(&format!("project:{}", from.display())),
        "nothing is still filed under where it was\n{vars}"
    );

    // **Once.** Not "the new path is present and the old one is not" — that passed while the
    // destination was on the list twice, because a conversation at the tree's own root has an
    // empty tail and `to.join("")` appends a separator: `/…/moved-tree` and `/…/moved-tree/` are
    // one directory under two spellings, and every list keyed by the path kept both. A duplicate
    // is invisible to an assertion about membership and obvious in the panel, which is exactly the
    // class of bug this file exists to hold on to.
    let listed = |v: &str| {
        let want = to.display().to_string();
        serde_json::from_str::<serde_json::Value>(v)
            .ok()
            .and_then(|j| j["global"]["sidebar.projects"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter(|p| p.as_str().is_some_and(|p| p.trim_end_matches('/') == want))
            .count()
    };
    assert_eq!(listed(&vars), 1, "the panel's list names it exactly once\n{vars}");
}

/// `m` on a worktree row offers the same places `^N` offers when it makes one.
///
/// The point of the feature, and the thing that made it worth a picker rather than a path prompt:
/// somebody who has learnt that "In this project" means `.worktrees/` when they create a tree has
/// learnt what it means when they move one. Every row names its resolved path, and the row for
/// where the tree already is says so rather than being quietly dropped — a menu that hides the
/// answer you were looking for reads as the option not existing.
#[test]
fn moving_a_worktree_offers_the_places_a_new_one_would_go() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("wtmovewhere");
    sb.git_init();
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.send(&command("git.worktree.new.inside"));
    let under = sb.work().join(".worktrees");
    assert!(
        s.pump(|_| std::fs::read_dir(&under).is_ok_and(|d| d.count() > 0)),
        "the worktree exists"
    );
    // Resolved for the reason the move test resolves it: git's spelling of this path is the one
    // the workspace is holding.
    let tree = std::fs::read_dir(&under)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .next()
        .and_then(|p| std::fs::canonicalize(p).ok())
        .expect("one worktree");

    // The tree named, so the first picker is skipped and the one under test is the destination.
    s.send(&command_with("git.worktree.move", &tree.display().to_string()));
    // The picker names its buffer after its title, which here is the branch being moved.
    let named = |s: &Session| {
        s.events.iter().find_map(|e| {
            let n = e["name"].as_str()?;
            (e["type"] == "buffer_opened" && n.starts_with("[Move ")).then(|| n.to_string())
        })
    };
    // Waited on the *rows*, not on the buffer: a picker opens its buffer before it has anything in
    // it, and reading at that moment is reading an empty float that is about to be correct.
    let rows_now =
        |s: &Session| named(s).map(|n| s.picker_named(&n)).unwrap_or_default();
    assert!(s.pump(|s| !rows_now(s).is_empty()), "the destination picker opened with rows in it");
    let rows = rows_now(&s);

    assert!(
        rows.iter().any(|l| l.contains("In this project")),
        "the place it can be kept in the repository\n{rows:?}"
    );
    assert!(
        rows.iter().any(|l| l.contains("Beside the repository")),
        "and the place beside it\n{rows:?}"
    );
    assert!(
        rows.iter().any(|l| l.contains("Somewhere else")),
        "and the path field, for one nobody listed\n{rows:?}"
    );
    // Where it is now is a row, and says so. Hiding it would leave a menu of two places for a
    // repository that has three.
    assert!(
        rows.iter().any(|l| l.contains("where it is now")),
        "the row for where it already is says so\n{rows:?}"
    );
}

/// Which branch a checkout is on, asked of git rather than of anything under test.
fn head_of(dir: &Path) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .expect("git runs");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn the_generated_name_is_editable_before_the_branch_is_created() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("branchedit");
    sb.git_init();
    let mut s = sb.start_with(&branch_fixture());
    s.wait_for("PROJECTS");

    s.send(&command("git.branch.new"));
    s.wait_for("What are you about to work on?");
    s.type_text("x");
    s.special("enter");
    s.wait_for("fix-the-login-redirect");

    // Clear the field and type our own, then accept.
    s.ctrl("u");
    s.type_text("my-own-name");
    s.special("enter");
    // The footer, which is where the branch lives. It used to be a toast saying the same thing,
    // and a message restating what is already on screen is exactly what the notice rework took away — so
    // the branch segment is now refreshed on the spot instead, and this waits on that.
    s.wait_for("my-own-name  mock");

    let branch = Command::new("git")
        .current_dir(sb.work())
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .expect("git runs");
    assert_eq!(
        String::from_utf8_lossy(&branch.stdout).trim(),
        "my-own-name",
        "the branch git actually created"
    );
}

#[test]
fn dismissing_the_description_creates_nothing() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("branchcancel");
    sb.git_init();
    let mut s = sb.start_with(&branch_fixture());
    s.wait_for("PROJECTS");

    s.send(&command("git.branch.new"));
    s.wait_for("What are you about to work on?");
    s.special("esc");
    s.drain_for(Duration::from_secs(2));

    let branches = Command::new("git")
        .current_dir(sb.work())
        .args(["for-each-ref", "--format=%(refname:short)", "refs/heads"])
        .output()
        .expect("git runs");
    let names = String::from_utf8_lossy(&branches.stdout);
    assert_eq!(names.trim(), "trunk", "no branch was created");
}

/// Invoke a command the way a menu or a palette entry would.
///
/// Not a synthesised keystroke: a command with no default binding has no key to synthesise, and one
/// that does would break the moment a user rebound it.
fn command(name: &str) -> String {
    format!(r#"{{"type":"command","name":"{name}"}}"#)
}

fn command_with(name: &str, arg: &str) -> String {
    format!(r#"{{"type":"command","name":"{name}","args":["{arg}"]}}"#)
}

#[test]
fn a_plugin_that_did_not_declare_vcs_write_is_refused() {
    if !have_git() {
        return;
    }
    // Enforcement is per plugin, from the manifest — not the agent's permission mode. This proves
    // the check is real: identical code, one word missing from `plugin.toml`.
    let sb = Sandbox::new("nodeclare");
    sb.git_init();
    let dir = sb.root.join("config/plugins/meddler");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"meddler\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\n",
    )
    .expect("write manifest");
    std::fs::write(
        dir.join("main.ts"),
        r#"
import type { PluginContext } from "@neosh/api";
export async function activate({ neosh }: PluginContext) {
  // Reading is fine and needs nothing declared.
  const status = await neosh.git.status();
  neosh.notify(`read ok: ${status.repo.branch}`);
  try {
    await neosh.git.createBranch("sneaky");
    neosh.notify("WROTE ANYWAY");
  } catch (e) {
    neosh.notify(`refused: ${e}`);
  }
}
"#,
    )
    .expect("write plugin");

    let mut s = sb.start();
    s.wait_for("read ok: trunk");
    s.wait_for("refused:");
    assert!(s.saw("vcs_write"), "the refusal names what to declare\n{}", s.transcript());
    assert!(!s.saw("WROTE ANYWAY"), "the write must not have happened\n{}", s.transcript());

    let branches = Command::new("git")
        .current_dir(sb.work())
        .args(["for-each-ref", "--format=%(refname:short)", "refs/heads"])
        .output()
        .expect("git runs");
    assert_eq!(String::from_utf8_lossy(&branches.stdout).trim(), "trunk");
}

#[test]
fn clean_drops_user_config_but_keeps_the_bundled_plugins() {
    // `--clean` answers "is this neosh, or is it my setup". The sidebar and the switchers *are*
    // neosh — starting with no interface at all would answer a question nobody asked. To suspect
    // one of them, `plugins.disabled` names it.
    let sb = Sandbox::new("clean");
    let child = Command::new(env!("CARGO_BIN_EXE_neosh"))
        .args(["--ui-protocol", "stdio", "--clean"])
        .arg("--cwd")
        .arg(sb.work())
        .args(["--mock-script", &fixture().display().to_string()])
        .args(["--model", "mock/mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("neosh starts");
    let mut s = Session::wrap(child);

    s.wait_for("--clean");
    s.wait_for("PROJECTS");
    assert!(s.buffer_named("[sidebar]").is_some(), "the bundled UI is still there");
}

/// A provider plugin whose model exposes a knob the host has never heard of.
///
/// The claim under test is the strong form: a driver invents an option, and the bundled switcher
/// renders and applies it with **no change to the switcher**. That is the whole reason
/// `ProviderOptionDescriptor` exists rather than a hard-coded reasoning-effort enum.
const INVENTED_KNOB: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("invented", [{
    id: "lab",
    driver: "invented",
    display_name: "Lab",
    models: [{
      id: "brainy",
      display_name: "Brainy",
      capabilities: {
        tools: false, vision: false, streaming: true, thinking: true, prompt_caching: false,
        option_descriptors: [{
          type: "select",
          id: "vibe",
          label: "Vibe",
          options: [
            { id: "chill", label: "Chill", is_default: true },
            { id: "intense", label: "Intense", description: "more of it", is_default: false },
          ],
        }],
      },
    }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "brainy", usage: {} });
    emit({ type: "message_stop" });
  });

  await neosh.cmd.register("lab.report", async () => {
    const sel = await neosh.agent.selection();
    neosh.notify(`selection: ${sel?.instance}/${sel?.model} ${JSON.stringify(sel?.options ?? [])}`);
  });
  neosh.event.on("neosh.ready", () => neosh.notify("lab ready"));
}
"#;

/// A driver whose top level is a *word* rather than a parameter.
///
/// The shape a vendor harness has: a ladder that can be sent, plus a level you buy by saying it. The
/// handler reads back both halves of what arrived, which is the only way to tell the difference
/// between "the word was injected" and "the word was sent as the effort, and the endpoint would have
/// rejected it".
const SPOKEN_KNOB: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("spoken", [{
    id: "lab",
    driver: "spoken",
    display_name: "Lab",
    models: [{
      id: "wordy",
      display_name: "Wordy",
      capabilities: {
        tools: false, vision: false, streaming: true, thinking: true, prompt_caching: false,
        option_descriptors: [{
          type: "select",
          id: "effort",
          label: "Effort",
          options: [
            { id: "low", label: "Low", is_default: false },
            { id: "high", label: "High", is_default: true },
            { id: "abracadabra", label: "Abracadabra", is_default: false },
          ],
          prompt_injected_values: ["abracadabra"],
        }],
      },
    }],
  }], async (req, emit) => {
    const last = req.messages[req.messages.length - 1];
    const text = last?.content.map((b) => (b.type === "text" ? b.text : "")).join("") ?? "";
    const effort = req.selection.options?.find((o) => o.id === "effort")?.value ?? "none";
    emit({ type: "message_start", model: "wordy", usage: {} });
    emit({ type: "block_start", index: 0, block: { kind: "text" } });
    emit({ type: "text_delta", index: 0, text: `got[${text.replace(/\n/g, "|")}] effort[${effort}]` });
    emit({ type: "block_stop", index: 0 });
    emit({ type: "message_delta", stop_reason: { kind: "end_turn" }, usage: {} });
    emit({ type: "message_stop" });
  });
  neosh.event.on("neosh.ready", () => neosh.notify("spoken ready"));
}
"#;

/// A driver with a full ladder in it: three rungs, plus one superseded model.
const LADDER: &str = r#"
import type { PluginContext } from "@neosh/api";

const model = (id: string, name: string, tier: string, legacy: boolean) => ({
  id,
  display_name: name,
  family: "rung",
  tier,
  legacy,
  tagline: `the ${tier} one`,
  capabilities: {
    tools: false, vision: false, streaming: true, thinking: false, prompt_caching: false,
    option_descriptors: [],
  },
});

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("ladder", [{
    id: "lab",
    driver: "ladder",
    display_name: "Lab",
    models: [
      model("big", "Big", "frontier", false),
      model("mid", "Mid", "balanced", false),
      model("small", "Small", "fast", false),
      model("old", "Old", "balanced", true),
    ],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "mid", usage: {} });
    emit({ type: "message_stop" });
  });

  await neosh.cmd.register("lab.report", async () => {
    const sel = await neosh.agent.selection();
    neosh.notify(`selection: ${sel?.instance}/${sel?.model}`);
  });
  neosh.event.on("neosh.ready", () => neosh.notify("ladder ready"));
}
"#;

fn install_ladder(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/lab");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"lab\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("write manifest");
    std::fs::write(dir.join("main.ts"), LADDER).expect("write plugin");
}

#[test]
fn one_step_down_is_a_rung_not_a_list() {
    // The whole reason models carry a tier. "Give me something cheaper" should be a keystroke, and
    // it should stay on the provider you are already paying for.
    let sb = Sandbox::new("ladder");
    install_ladder(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"lab/big\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("ladder ready");

    s.send(&command("model.downgrade"));
    s.wait_for("model: lab/Mid");
    s.send(&command("model.downgrade"));
    s.wait_for("model: lab/Small");

    // No wrapping. A downgrade that teleports you to the most expensive thing in the catalogue is
    // a keystroke you learn not to press.
    s.send(&command("model.downgrade"));
    s.wait_for("already the quickest one here");
    s.send(&command("lab.report"));
    s.wait_for("selection: lab/small");

    s.send(&command("model.upgrade"));
    s.wait_for("model: lab/Mid");
}

#[test]
fn a_line_resolves_to_the_one_that_has_not_been_superseded() {
    // "Use Opus" is a statement about a product line, not about a pinned id that goes stale. The
    // superseded entry sits on the same rung as the current one, so picking wrongly is possible
    // and would be silent.
    let sb = Sandbox::new("line");
    install_ladder(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"lab/small\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("ladder ready");

    s.send(&command_with("model.line", "rung"));
    s.wait_for("model: lab/Big");
    s.send(&command_with("model.line", "nonesuch"));
    s.wait_for(r#"nothing reachable is in the "nonesuch" line"#);
}

#[test]
fn superseded_models_are_folded_behind_a_count() {
    // Reachable and out of the way. You go looking for last year's model to reproduce something,
    // which is exactly when hiding it outright would be worst.
    let sb = Sandbox::new("legacy");
    install_ladder(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"lab/big\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("ladder ready");

    s.ctrl("p");
    s.wait_for("Model");
    let rows = s.picker_now();
    assert!(rows.iter().any(|l| l.contains("Big")), "the current models are listed\n{rows:?}");
    assert!(
        rows.iter().any(|l| l.contains("1 superseded")),
        "and the old one is behind a count\n{rows:?}"
    );
    assert!(
        !rows.iter().any(|l| l.contains("Old")),
        "which means it is not in the way\n{rows:?}"
    );
}

#[test]
fn the_rail_says_what_a_provider_costs_before_you_pick_from_it() {
    // A plan you already pay for and a key billed per token are different decisions. The old flat
    // list could not say which a row was, which is why "it says I have no API key" was reasonable.
    let sb = Sandbox::new("accounts");
    sb.write_config(NEEDS_A_KEY);
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("p");
    s.wait_for("Model");
    let rows = s.picker_now();
    assert!(rows.iter().any(|l| l.contains("API KEYS")), "grouped by account\n{rows:?}");
    assert!(rows.iter().any(|l| l.contains("LOCAL")), "grouped by account\n{rows:?}");
    assert!(rows.iter().any(|l| l.contains("Acme")), "the configured instance is on the rail\n{rows:?}");
}

#[test]
fn model_speed_is_visible_and_switches_between_standard_and_fast() {
    let sb = Sandbox::new("model-speed");
    let dir = sb.root.join("config/plugins/lab");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"lab\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("manifest");
    let plugin = INVENTED_KNOB
        .replace("vibe", "service_tier")
        .replace("Vibe", "Speed")
        .replace("chill", "default")
        .replace("Chill", "Standard")
        .replace("intense", "priority")
        .replace("Intense", "Fast")
        .replace(
            r#"neosh.event.on("neosh.ready", () => neosh.notify("lab ready"));"#,
            r#"neosh.event.on("neosh.ready", async () => {
              // A stored conversation may not have the newly discovered speed option yet.
              await neosh.agent.setSelection({ instance: "lab", model: "brainy", options: [] });
              neosh.notify("lab ready");
            });"#,
        );
    std::fs::write(dir.join("main.ts"), plugin).expect("plugin");
    sb.write_config("[options]\n\"agent.model\" = \"lab/brainy\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("lab ready");
    assert!(
        s.pump(|s| s.status_now().join("").contains("Standard")),
        "default speed in footer: {:?}",
        s.status_now()
    );
    s.ctrl("e");
    s.wait_open("[model options]");
    s.wait_for("Speed");
    s.special("right");
    s.special("enter");
    s.wait_closed("[model options]");
    assert!(
        s.pump(|s| s.status_now().join("").contains("Fast")),
        "fast speed in footer: {:?}",
        s.status_now()
    );
    s.send(&command("lab.report"));
    s.wait_for(r#"selection: lab/brainy [{"id":"service_tier","value":"priority"}]"#);
    s.ctrl("e");
    s.wait_open("[model options]");
    s.special("left");
    s.special("enter");
    s.wait_closed("[model options]");
    assert!(
        s.pump(|s| s.status_now().join("").contains("Standard")),
        "standard speed in footer: {:?}",
        s.status_now()
    );
    s.send(&command("lab.report"));
    s.wait_for(r#"selection: lab/brainy [{"id":"service_tier","value":"default"}]"#);
}

#[test]
fn model_context_is_visible_and_switches_between_default_and_expanded() {
    let sb = Sandbox::new("model-context");
    let dir = sb.root.join("config/plugins/lab");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"lab\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("manifest");
    let plugin = INVENTED_KNOB
        .replace("vibe", "context")
        .replace("Vibe", "Context")
        .replace("chill", "default")
        .replace("Chill", "Default context")
        .replace("intense", "1000000")
        .replace("Intense", "1M")
        .replace(
            r#"neosh.event.on("neosh.ready", () => neosh.notify("lab ready"));"#,
            r#"neosh.event.on("neosh.ready", async () => {
              // A stored conversation may not have the newly discovered context option yet.
              await neosh.agent.setSelection({ instance: "lab", model: "brainy", options: [] });
              neosh.notify("lab ready");
            });"#,
        );
    std::fs::write(dir.join("main.ts"), plugin).expect("plugin");
    sb.write_config("[options]\n\"agent.model\" = \"lab/brainy\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("lab ready");
    assert!(
        s.pump(|s| s.status_now().join("").contains("Default context")),
        "default context in footer: {:?}",
        s.status_now()
    );
    s.ctrl("e");
    s.wait_open("[model options]");
    s.wait_for("Context");
    s.special("right");
    s.special("enter");
    s.wait_closed("[model options]");
    assert!(
        s.pump(|s| s.status_now().join("").contains("1M")),
        "expanded context in footer: {:?}",
        s.status_now()
    );
    s.send(&command("lab.report"));
    s.wait_for(r#"selection: lab/brainy [{"id":"context","value":"1000000"}]"#);
    s.ctrl("e");
    s.wait_open("[model options]");
    s.special("left");
    s.special("enter");
    s.wait_closed("[model options]");
    assert!(
        s.pump(|s| s.status_now().join("").contains("Default context")),
        "default context in footer: {:?}",
        s.status_now()
    );
    s.send(&command("lab.report"));
    s.wait_for(r#"selection: lab/brainy [{"id":"context","value":"default"}]"#);
}

#[test]
fn a_driver_can_invent_an_option_and_the_switcher_renders_it() {
    let sb = Sandbox::new("invented");
    let dir = sb.root.join("config/plugins/lab");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"lab\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("write manifest");
    std::fs::write(dir.join("main.ts"), INVENTED_KNOB).expect("write plugin");
    sb.write_config("[options]\n\"agent.model\" = \"lab/brainy\"\n");

    let mut s = sb.start_letting_config_choose();
    s.wait_for("lab ready");

    // The switcher has never heard of "vibe" and does not need to. Every value is on screen at
    // once — that is what the panel is for — so both of them are visible before anything is chosen.
    s.ctrl("e");
    s.wait_open("[model options]");
    s.wait_for("Vibe");
    assert!(s.saw("Chill"), "choices rendered\n{}", s.transcript());
    assert!(s.saw("Intense"), "choices rendered\n{}", s.transcript());

    // Along the row, then out. Right rather than down: the values of one knob run across, and the
    // knobs run down — which is the difference between this and the two nested lists it replaced.
    s.special("right");
    s.special("enter");
    s.wait_closed("[model options]");

    // And it is really on the selection the next turn would use. Asserted here rather than from a
    // notification, because there is no longer one to assert: the panel applies as you move, so the
    // *state* is the acknowledgement.
    s.send(&command("lab.report"));
    s.wait_for(r#"selection: lab/brainy [{"id":"vibe","value":"intense"}]"#);
}

#[test]
fn a_level_that_is_a_word_is_said_rather_than_sent() {
    // The half of `prompt_injected_values` that never existed. The field has long been on the wire
    // and nothing acted on it, so choosing such a level put it where a parameter goes —
    // `--effort abracadabra`, which is not a level, and the turn fails.
    let sb = Sandbox::new("spoken");
    let dir = sb.root.join("config/plugins/lab");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"lab\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("write manifest");
    std::fs::write(dir.join("main.ts"), SPOKEN_KNOB).expect("write plugin");
    sb.write_config("[options]\n\"agent.model\" = \"lab/wordy\"\n");

    let mut s = sb.start_letting_config_choose();
    s.wait_for("spoken ready");
    s.ctrl("e");
    s.wait_open("[model options]");
    // Past the top of the sendable ladder: low, high, and then the word.
    s.special("right");
    s.special("right");
    s.special("enter");
    s.wait_closed("[model options]");

    for c in ["h", "e", "l", "l", "o"] {
        s.key(c);
    }
    s.special("enter");

    // Both halves at once. The word arrived at the top of the message, and what went in the
    // parameter is the ladder's own default rather than the word — so a driver that would have
    // rejected it never sees it, and a driver whose default is lower does not quietly get its way.
    s.wait_for("got[abracadabra||hello] effort[high]");
}

#[test]
fn nothing_else_reaches_the_keyboard_while_the_option_sheet_is_open() {
    // Every global key used to fall straight through it. `^N` opened a new conversation *behind*
    // the panel, which stayed on screen; `^T` opened the project panel underneath and moved focus
    // into it. Shadowing the keys the panel wants could never have fixed this — a panel can name
    // the keys it wants and never the ones it merely does not want to happen.
    let sb = Sandbox::new("modalsheet");
    let dir = sb.root.join("config/plugins/lab");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"lab\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("write manifest");
    std::fs::write(dir.join("main.ts"), INVENTED_KNOB).expect("write plugin");
    sb.write_config("[options]\n\"agent.model\" = \"lab/brainy\"\n");

    let mut s = sb.start_letting_config_choose();
    s.wait_for("lab ready");
    s.ctrl("e");
    s.wait_open("[model options]");

    s.ctrl("n");
    s.ctrl("t");
    // A letter, too: a modal that let characters through would be typing into the composer behind
    // the float, where you cannot see what you are writing.
    s.key("z");
    // Nothing to wait *for* — the assertion is that none of those three did anything — so this is
    // the one case that has to spend real time before it can say so.
    s.drain_for(Duration::from_millis(1200));

    assert!(
        s.buffers_named("[New conversation]").is_empty(),
        "`^N` opened a conversation behind the panel\n{}",
        s.transcript()
    );
    let sheet = s.buffers_named("[model options]");
    assert!(
        sheet.iter().any(|b| !s.windows_for(*b).is_empty()),
        "the panel that ate the keys is not even on screen\n{}",
        s.transcript()
    );

    // And the key that opened it closes it, which is the obligation modality creates: the global
    // `^E` no longer arrives to toggle it off.
    s.ctrl("e");
    s.wait_closed("[model options]");
}

#[test]
fn a_knob_row_says_it_is_a_control_before_you_press_anything() {
    // A ladder with one rung lit reads identically whether or not `h`/`l` do anything to it — which
    // is how a live control came to look like a summary, and why `↵` on it (which means "done")
    // felt like a key that does nothing at all.
    let sb = Sandbox::new("railsheet");
    let dir = sb.root.join("config/plugins/lab");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"lab\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("write manifest");
    std::fs::write(dir.join("main.ts"), INVENTED_KNOB).expect("write plugin");
    sb.write_config("[options]\n\"agent.model\" = \"lab/brainy\"\n");

    let mut s = sb.start_letting_config_choose();
    s.wait_for("lab ready");
    s.ctrl("e");
    s.wait_open("[model options]");
    s.wait_for("Vibe");

    assert!(
        s.texts().iter().any(|l| l.contains("‹") && l.contains("›") && l.contains("Chill")),
        "the row is drawn without the rail that says it is one\n{}",
        s.transcript()
    );
    // And the key that changes something is named before the key that leaves.
    // Letters, not arrows: the arrows are bound too, and the row is a promise about a keyboard.
    assert!(
        s.pump(|s| s.footer_of("[model options]").is_some_and(|f| f.starts_with(" h l change"))),
        "the hints lead with the key that does the thing\n{:?}",
        s.footer_of("[model options]")
    );
}

#[test]
fn what_you_typed_is_what_the_transcript_keeps() {
    // The word belongs to the *request*, not to the conversation. Written into the stored message it
    // would be replayed on every later turn — so turning the level off would not turn it off — and
    // it would be sitting in the transcript above an answer, as something you never said.
    let sb = Sandbox::new("spokenkeep");
    let dir = sb.root.join("config/plugins/lab");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"lab\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("write manifest");
    std::fs::write(dir.join("main.ts"), SPOKEN_KNOB).expect("write plugin");
    sb.write_config("[options]\n\"agent.model\" = \"lab/wordy\"\n");

    let mut s = sb.start_letting_config_choose();
    s.wait_for("spoken ready");
    s.ctrl("e");
    s.wait_open("[model options]");
    s.special("right");
    s.special("right");
    s.special("enter");
    s.wait_closed("[model options]");
    for c in ["h", "i"] {
        s.key(c);
    }
    s.special("enter");
    s.wait_for("got[abracadabra||hi]");

    // Back down the ladder, and ask again. If the first turn's word had been kept, this one would
    // carry it too.
    s.ctrl("e");
    s.wait_open("[model options]");
    s.special("left");
    s.special("left");
    s.special("enter");
    s.wait_closed("[model options]");
    for c in ["y", "o"] {
        s.key(c);
    }
    s.special("enter");
    s.wait_for("got[yo] effort[low]");
}

#[test]
fn switching_models_keeps_an_option_the_new_one_also_has() {
    // Switching between two thinking models should not silently reset your effort level; switching
    // to one without the knob must drop it rather than send a value the driver would reject.
    let sb = Sandbox::new("carryover");
    let dir = sb.root.join("config/plugins/lab");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"lab\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("write manifest");
    std::fs::write(
        dir.join("main.ts"),
        &INVENTED_KNOB.replace(
            "models: [{",
            "models: [{ id: \"plain\", display_name: \"Plain\", capabilities: { tools: false, \
             vision: false, streaming: true, thinking: false, prompt_caching: false, \
             option_descriptors: [] } }, {",
        ),
    )
    .expect("write plugin");
    sb.write_config("[options]\n\"agent.model\" = \"lab/brainy\"\n");

    let mut s = sb.start_letting_config_choose();
    s.wait_for("lab ready");
    s.ctrl("e");
    s.wait_open("[model options]");
    s.wait_for("Vibe");
    s.special("right");
    s.special("enter");
    s.wait_closed("[model options]");

    // Move to "Plain", which has no options at all.
    s.ctrl("p");
    s.wait_for("Plain");
    for c in ["P", "l", "a", "i", "n"] {
        s.key(c);
    }
    s.special("enter");
    s.wait_for("model: lab/Plain");

    s.send(&command("lab.report"));
    s.wait_for("selection: lab/plain []");
}

#[test]
fn a_model_answer_git_would_reject_is_made_into_a_legal_refname() {
    if !have_git() {
        return;
    }
    // `git check-ref-format` refuses every part of this: leading dashes, `//`, a `.lock` suffix,
    // spaces, and trailing punctuation. The slug runs in the plugin rather than the host so the
    // name shown for approval is exactly the name that gets created.
    let sb = Sandbox::new("nastyname");
    sb.git_init();
    let mut s = sb.start_with(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/branch_name_nasty.jsonl"),
    );
    s.wait_for("PROJECTS");

    s.send(&command("git.branch.new"));
    s.wait_for("What are you about to work on?");
    s.type_text("x");
    s.special("enter");
    s.wait_for("fix/the-login");
    s.special("enter");
    // The footer rather than a toast.
    s.wait_for("fix/the-login  mock");

    // The real proof: git accepted it.
    let branch = Command::new("git")
        .current_dir(sb.work())
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .expect("git runs");
    assert_eq!(String::from_utf8_lossy(&branch.stdout).trim(), "fix/the-login");
}

// ---------------------------------------------------------------------------
// Getting out
// ---------------------------------------------------------------------------

#[test]
fn ctrl_c_closes_a_picker_rather_than_arming_a_quit() {
    // `<C-c>` is bound globally to `interrupt`. While a picker is on screen it obviously means
    // "close this", so the picker binds it window-scoped, which outranks the global binding.
    let sb = Sandbox::new("ctrlc-picker");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("p");
    s.wait_for("Model");

    let picker_buf = s.buffer_named("[Model]").expect("picker buffer");
    assert_eq!(s.windows_for(picker_buf).len(), 1);

    s.ctrl("c");
    assert!(
        s.pump(|s| s.windows_for(picker_buf).is_empty()),
        "the picker should close\n{}",
        s.transcript()
    );
    assert!(
        !s.saw("press ^C again"),
        "closing a picker must not also arm a quit\n{}",
        s.transcript()
    );
}

// ---------------------------------------------------------------------------
// The thread list, driven from the keyboard
// ---------------------------------------------------------------------------

impl Session {
    fn down(&mut self) {
        self.send(r#"{"type":"key","key":{"code":{"kind":"down"},"mods":{}}}"#);
    }

    fn enter(&mut self) {
        self.send(r#"{"type":"key","key":{"code":{"kind":"enter"},"mods":{}}}"#);
    }

    /// Focus the panel and wait until it has the keyboard.
    ///
    /// `<C-t>` reaches the plugin thread asynchronously, so a key sent straight afterwards races it
    /// and lands in the composer instead. The hint strip switching to the in-panel verbs is the
    /// signal that focus and the key capture are both in place.
    fn enter_panel(&mut self) {
        self.ctrl("t");
        assert!(
            self.pump(|s| s.sidebar_now().iter().any(|l| l.contains("? keys"))),
            "the panel took the keyboard\n{:?}",
            self.sidebar_now()
        );
    }

    /// Virtual text attached to the sidebar's lines.
    ///
    /// Right-aligned columns — ages, counts, elapsed times — are extmarks rather than buffer text,
    /// because only the frontend knows how wide the pane is or how many columns a character takes.
    fn sidebar_virt(&self) -> Vec<String> {
        let Some(buf) = self.buffer_named("[sidebar]") else { return Vec::new() };
        self.events
            .iter()
            .filter(|e| e["type"] == "buffer_lines" && e["buf"].as_u64() == Some(buf))
            .filter_map(|e| e["lines"].as_array())
            .flatten()
            .filter_map(|l| l["marks"].as_array())
            .flatten()
            .filter_map(|m| m["virt_text"].as_array())
            .flatten()
            .filter_map(|c| c["text"].as_str().map(str::to_string))
            .collect()
    }

    /// The sidebar buffer's current contents, folded the way a frontend folds splices.
    /// The lines of the model picker, whatever pane they belong to.
    fn picker_now(&self) -> Vec<String> {
        self.picker_named("[Model]")
    }

    /// The newest buffer with a name, for widgets that open one per use.
    ///
    /// A prompt asks two questions by opening two buffers both called `[prompt]`, so matching the
    /// first one means reading the answer to a question that has already been answered.
    fn latest_named(&self, name: &str) -> Option<u64> {
        self.events.iter().rev().find_map(|e| {
            (e["type"] == "buffer_opened" && e["name"] == name).then(|| e["buf"].as_u64())?
        })
    }

    fn prompt_now(&self) -> Vec<String> {
        match self.latest_named("[prompt]") {
            Some(b) => self.lines_of(b),
            None => Vec::new(),
        }
    }

    /// The contents of a picker window, by the title it was opened with.
    fn picker_named(&self, name: &str) -> Vec<String> {
        match self.buffer_named(name) {
            Some(buf) => self.lines_of(buf),
            None => Vec::new(),
        }
    }

    /// The highlight groups on each row of the transcript.
    fn chat_groups(&self) -> Vec<Vec<String>> {
        let name = self.events.iter().find_map(|e| {
            let n = e["name"].as_str()?;
            (e["type"] == "buffer_opened" && n.starts_with("[chat]")).then(|| n.to_string())
        });
        match name.and_then(|n| self.buffer_named(&n)) {
            Some(b) => self.groups_of(b),
            None => Vec::new(),
        }
    }

    fn sidebar_now(&self) -> Vec<String> {
        match self.buffer_named("[sidebar]") {
            Some(b) => self.lines_of(b),
            None => Vec::new(),
        }
    }

    /// The sidebar from the `PROJECTS` heading down.
    ///
    /// Which is what a question about the project list actually means, and is not the same as the
    /// whole column. The git block sits *above* `PROJECTS` and its second row is the **current
    /// branch** — so a worktree test asking whether `sideline` appears anywhere in the sidebar
    /// finds the branch row, two lines from the top, rather than the tree row. It then finds it
    /// *above* the repository, and concludes the tree is not nested, for a reason with nothing to
    /// do with nesting.
    ///
    /// That failure is timing-shaped without being a timing bug: the git block polls, so on a fast
    /// machine the assertion often runs before the branch row exists and the test passes. It went
    /// red on CI under load, which is the same test being right about the same code — the sidebar
    /// had simply had time to finish drawing.
    fn projects_now(&self) -> Vec<String> {
        let rows = self.sidebar_now();
        match rows.iter().position(|l| l.contains("PROJECTS")) {
            Some(i) => rows[i..].to_vec(),
            None => rows,
        }
    }

    /// Replay every edit to one buffer, so what comes back is what is on screen now rather than
    /// everything that was ever drawn there.
    /// The highlight groups on each row of a buffer, folded the same way its text is.
    ///
    /// Needed because a mark is not text: inserting a row above a marked one moves the mark with
    /// it, and an off-by-one there is invisible in the transcript's contents and glaring on screen.
    fn groups_of(&self, buf: u64) -> Vec<Vec<String>> {
        let mut rows: Vec<Vec<String>> = Vec::new();
        for e in &self.events {
            if e["type"] != "buffer_lines" || e["buf"].as_u64() != Some(buf) {
                continue;
            }
            let start = e["start"].as_i64().unwrap_or(0);
            let old_end = e["old_end"].as_i64().unwrap_or(start);
            let new: Vec<Vec<String>> = e["lines"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|l| {
                            l["marks"]
                                .as_array()
                                .map(|ms| {
                                    // Both, because both are highlight groups on the row and a
                                    // caller asking what colours a row has does not care which
                                    // vocabulary put them there. A cursor line is a *band* — it
                                    // sits under every ranged group rather than winning against
                                    // them, which is what lets a spinner on the row you are
                                    // standing on go on spinning — and reading only `hl_group`
                                    // made the selected row look unhighlighted to this helper.
                                    ms.iter()
                                        .flat_map(|m| {
                                            [
                                                m["hl_group"].as_str(),
                                                m["line_hl_group"].as_str(),
                                            ]
                                        })
                                        .flatten()
                                        .map(str::to_string)
                                        .collect()
                                })
                                .unwrap_or_default()
                        })
                        .collect()
                })
                .unwrap_or_default();
            let s = start.clamp(0, rows.len() as i64) as usize;
            let en = if old_end < 0 { rows.len() } else { (old_end as usize).clamp(s, rows.len()) };
            rows.splice(s..en, new);
        }
        rows
    }

    fn lines_of(&self, buf: u64) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        for e in &self.events {
            if e["type"] != "buffer_lines" || e["buf"].as_u64() != Some(buf) {
                continue;
            }
            let start = e["start"].as_i64().unwrap_or(0);
            let old_end = e["old_end"].as_i64().unwrap_or(start);
            let new: Vec<String> = e["lines"]
                .as_array()
                .map(|a| a.iter().filter_map(|l| l["text"].as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let s = start.clamp(0, lines.len() as i64) as usize;
            let en =
                if old_end < 0 { lines.len() } else { (old_end as usize).clamp(s, lines.len()) };
            lines.splice(s..en, new);
        }
        lines
    }
}

#[test]
fn the_sidebar_lists_conversations_under_their_project() {
    let sb = Sandbox::new("threads");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    for c in ["a", "b", "c"] {
        s.key(c);
    }
    s.enter();
    s.wait_for("abc");

    // The conversation appears in the list, labelled by what was said in it.
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("abc"))),
        "the conversation is listed\n{:?}",
        s.sidebar_now()
    );
    // And under a project heading, not loose.
    let panel = s.sidebar_now();
    // Trimmed: rows carry a left margin, and this test is about ordering rather than indentation.
    let projects = panel.iter().position(|l| l.trim() == "PROJECTS").expect("heading");
    let thread = panel.iter().position(|l| l.contains("abc")).expect("row");
    assert!(thread > projects, "the row sits under the heading\n{panel:?}");
}

#[test]
fn a_new_conversation_appears_in_the_list_and_becomes_the_active_one() {
    let sb = Sandbox::new("newthread");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    for c in ["o", "n", "e"] {
        s.key(c);
    }
    s.enter();
    s.wait_for("one");

    s.ctrl("n");
    assert!(
        s.pump(|s| {
            let p = s.sidebar_now();
            p.iter().filter(|l| l.contains("New conversation")).count() == 1
                && p.iter().any(|l| l.contains("one"))
        }),
        "both conversations listed\n{:?}",
        s.sidebar_now()
    );
    // The new one is the active one: exactly one row carries the active marker.
    let active = s.sidebar_now().iter().filter(|l| l.trim_start().starts_with("▸ ")).count();
    assert_eq!(active, 1, "one active row\n{:?}", s.sidebar_now());
}

#[test]
fn moving_and_selecting_in_the_thread_list_switches_conversation() {
    let sb = Sandbox::new("select");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    for c in ["f", "i", "r", "s", "t"] {
        s.key(c);
    }
    s.enter();
    s.wait_for("first");

    s.new_conversation(); // a second conversation, now active
    assert!(s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("New conversation"))));

    // Into the list, down to the other row, select it.
    s.enter_panel();
    s.down();
    s.enter();

    // The chat buffer comes back with the first conversation's text.
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("first"))),
        "switched back to the first conversation\n{:?}",
        s.chat_now()
    );
}

// ---------------------------------------------------------------------------
// Favourites, order and folding
// ---------------------------------------------------------------------------

/// Install a plugin that stalls the agent inside `turn.pre`.
///
/// The mock provider answers instantly, which makes "a turn is running" impossible to observe from
/// the outside. A blocking hook that never resolves holds the turn open for as long as the test
/// needs, without any sleeping or racing.
fn install_staller(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/staller");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"staller\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"hooks_blocking\"]\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("main.ts"),
        r#"
import type { PluginContext } from "@neosh/api";
export async function activate({ neosh }: PluginContext) {
  await neosh.hook.register("turn_pre", () => new Promise(() => {}), {
    blocking: true,
    timeoutMs: 25000,
  });
  neosh.event.on("neosh.ready", () => neosh.notify("staller ready"));
}
"#,
    )
    .expect("plugin");
}

#[test]
fn a_running_turn_says_how_long_it_has_been_running() {
    // "Working" with no clock is indistinguishable from wedged, which is the moment you reach for
    // `^C` and throw away the answer.
    let sb = Sandbox::new("elapsed");
    install_staller(&sb);
    let mut s = sb.start();
    s.wait_for("staller ready");
    s.wait_for("PROJECTS");

    for c in ["h", "i"] {
        s.key(c);
    }
    s.enter();

    assert!(
        s.pump(|s| {
            s.sidebar_now().iter().any(|l| l.contains("hi"))
                && s.sidebar_virt().iter().any(|t| t.trim_end().ends_with('s'))
        }),
        "a duration appears beside the working conversation\n{:?}\n{:?}",
        s.sidebar_now(),
        s.sidebar_virt()
    );
}

/// A star on a row, meaning the project is a favourite.
///
/// U+2605 rather than a heart, and not because it looks better: `♥` is a codepoint Unicode calls
/// an emoji, so a terminal with a colour-emoji font draws it from there and hands back an outline.
const STAR: char = '\u{2605}';

/// What a conversation's row is indented by, and therefore what tells one from a project's.
///
/// A project sits one column in and a conversation three, which is the same two-column step a
/// worktree takes from the repository it is a tree of.
const CONVERSATION_INDENT: &str = "   ";

/// Project rows carrying the favourite mark.
///
/// Scoped to rows naming a project, because the hint strip also prints a star — that is the point
/// of the hint, and matching it would make these tests pass with the feature ripped out.
fn favourited(panel: &[String], project: &str) -> bool {
    panel.iter().any(|l| l.contains(STAR) && l.contains(project))
}

#[test]
fn favouriting_marks_a_project_and_it_is_still_marked_next_time() {
    let sb = Sandbox::new("fav");
    {
        let mut s = sb.start();
        s.wait_for("PROJECTS");
        s.enter_panel();
        s.key("f");
        assert!(
            s.pump(|s| favourited(&s.sidebar_now(), "work")),
            "the project is marked\n{:?}",
            s.sidebar_now()
        );
        // One list, not two: there is no second heading to go and look under.
        assert_eq!(
            s.sidebar_now().iter().filter(|l| l.trim() == "PROJECTS").count(),
            1,
            "favourites stay in the same list\n{:?}",
            s.sidebar_now()
        );
    }

    // A favourite you have to re-mark every morning is not a favourite.
    let mut s = sb.start();
    assert!(
        s.pump(|s| favourited(&s.sidebar_now(), "work")),
        "still marked after a restart\n{:?}",
        s.sidebar_now()
    );
}

#[test]
fn a_favourite_sorts_above_a_project_that_was_used_more_recently() {
    let sb = Sandbox::new("favtop");
    let other = sb.root.join("elsewhere");
    std::fs::create_dir_all(&other).expect("mkdir");

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.send(&serde_json::json!({
        "type": "command",
        "name": "project.open",
        "args": [other.display().to_string()],
    })
    .to_string());
    assert!(s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("elsewhere"))));

    // `work` is now the older project. Favourite it and it goes above the newer one anyway.
    s.enter_panel();
    s.down();
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("↵ fold"))),
        "the cursor is on a project\n{:?}",
        s.sidebar_now()
    );
    s.key("f");
    assert!(
        s.pump(|s| {
            let p = s.sidebar_now();
            let w = p.iter().position(|l| l.contains(STAR) && l.contains("work"));
            let e = p.iter().position(|l| l.contains("elsewhere"));
            matches!((w, e), (Some(w), Some(e)) if w < e)
        }),
        "the favourite leads\n{:?}",
        s.sidebar_now()
    );
}

#[test]
fn unfavouriting_takes_the_mark_away() {
    let sb = Sandbox::new("unfav");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.enter_panel();
    s.key("f");
    assert!(s.pump(|s| favourited(&s.sidebar_now(), "work")));
    s.key("f");
    assert!(
        s.pump(|s| !favourited(&s.sidebar_now(), "work")),
        "the mark is gone\n{:?}",
        s.sidebar_now()
    );
}

#[test]
fn adding_a_project_is_a_row_you_can_see_rather_than_a_key_you_have_to_know() {
    let sb = Sandbox::new("addrow");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("+ Add project"))),
        "the panel offers it\n{:?}",
        s.sidebar_now()
    );

    // And it does something: with no other worktree to offer, it asks for a path.
    s.enter_panel();
    for _ in 0..8 {
        s.down();
        if s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("add project"))) {
            break;
        }
    }
    s.enter();
    s.wait_for("Add project");
}

// ---------------------------------------------------------------------------
// Worktrees
// ---------------------------------------------------------------------------

/// `^N` in a repository asks where the conversation goes, because there is a real answer that is
/// not always "here".
///
/// Outside a repository it does not ask — the question would have one answer — so this also pins
/// down that `session.new` stays a single key everywhere the choice would be theatre.
#[test]
fn a_new_conversation_in_a_repository_offers_a_worktree() {
    let sb = Sandbox::new("newwhere");
    sb.git_init();
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("n");
    // Waiting on the picker's own buffer, not on the words in it: "New conversation" is also what
    // an unnamed conversation is called in the panel, so a text match passes before it opens.
    assert!(
        s.pump(|s| !s.picker_named("[New conversation]").is_empty()),
        "the picker opened\n{:?}",
        s.sidebar_now()
    );
    let rows = s.picker_named("[New conversation]");
    assert!(rows.iter().any(|l| l.contains("Here")), "the default\n{rows:?}");
    assert!(rows.iter().any(|l| l.contains("new worktree")), "and the other one\n{rows:?}");
    // The in-project variant is a visible row, not a setting you have to know about: a choice
    // that only exists after editing config.toml is a choice nobody discovers.
    assert!(
        rows.iter().any(|l| l.contains("in this project")),
        "and the one that stays in the repository\n{rows:?}"
    );
}

/// Somewhere else sits above the worktrees, and says the field completes paths.
///
/// The list of trees is unbounded and the verb under it was therefore at a distance that grew with
/// how long you had used the program: "I can't go down in the sidebar for it" is the same complaint
/// one panel along. It is also the row that tells you the filter line *is* a path field, and a
/// sentence about how to type is no use once you have scrolled past it looking for somewhere to
/// type.
#[test]
fn somewhere_else_is_above_the_worktrees_rather_than_under_them() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("newwhereorder");
    sb.git_init();
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("n");
    assert!(s.pump(|s| !s.picker_named("[New conversation]").is_empty()), "the picker opened");

    let rows = s.picker_named("[New conversation]");
    let elsewhere = rows.iter().position(|l| l.contains("Another directory"));
    let trees = rows.iter().position(|l| l.contains("worktree  ·  "));
    assert!(elsewhere.is_some(), "somewhere else is offered\n{rows:?}");
    // Only meaningful when there is a tree to be above; without one the row's position is not in
    // question and asserting on it would be asserting on nothing.
    if let (Some(e), Some(t)) = (elsewhere, trees) {
        assert!(e < t, "somewhere else comes first\n{rows:?}");
    }
    assert!(
        rows.iter().any(|l| l.contains("type a path")),
        "and it says the field takes one\n{rows:?}"
    );
}

/// `^O` lands in the path field, rather than in a menu with the path field at the bottom of it.
///
/// This used to be the current repository's worktrees with `Type a path…` under them — a list of
/// the one place you were already in, which you had to read to the end of every time to reach the
/// thing you almost always wanted. The row is first now, and it is a fallback rather than the
/// route: typing a path is what the field does from the first keystroke.
#[test]
fn add_project_puts_the_path_first() {
    let sb = Sandbox::new("addfirst");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.send(&command("project.open"));
    // The float, not the sidebar's `+ Add project` row — which is on screen from the start and
    // would let this assert before anything opened.
    assert!(s.pump(|s| !s.float_named("[Add project").is_empty()), "the field opened");

    let rows = s.float_named("[Add project");
    // The title, then the filter line, then the rows — so the first row is the third line.
    let first = rows.iter().skip(2).find(|l| !l.trim().is_empty()).cloned().unwrap_or_default();
    assert!(first.contains("Type a path"), "the path is the first row\n{rows:?}");
}

/// `^O` offers no worktrees at all — not the ones on the list, and not the ones off it.
///
/// Moving the path row to the top left the trees underneath it, and every one of them was wrong in
/// one of two ways. A tree already on the panel is an offer to add what is added — the repository
/// you are standing in was the second row — and a tree that is *not* on the panel is a scratch
/// branch you took off it with `X`, handed straight back. The only filter was a live conversation's
/// `cwd`, which is neither, so what got through was finished work drawn as its own full path with
/// the basename and branch repeated after it, truncated mid-branch in a float too narrow for both.
///
/// Two worktrees, one with a conversation in it and one without, because the old filter told those
/// two apart and nothing else did: if either is a row, the list is back.
#[test]
fn add_project_offers_no_worktrees() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("addnotrees");
    sb.git_init();
    let at = sb.root.join("sprout");
    let out = Command::new("git")
        .current_dir(sb.work())
        .args(["worktree", "add", "-b", "sprout", &at.display().to_string()])
        .output()
        .expect("git runs");
    assert!(out.status.success(), "worktree add: {}", String::from_utf8_lossy(&out.stderr));

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    // One of them gets a conversation, so the two trees differ in exactly the way the old filter
    // read — and the assertion below is that neither is offered regardless.
    s.send(&command_with("project.open", &at.display().to_string()));
    assert!(s.pump(|s| s.sidebar_rows() >= 2), "it is a project now\n{:?}", s.sidebar_now());

    s.send(&command("project.open"));
    assert!(s.pump(|s| !s.float_named("[Add project").is_empty()), "the field opened");

    let rows = s.float_named("[Add project");
    assert!(
        rows.iter().any(|l| l.contains("Type a path")),
        "the path row is still there\n{rows:?}"
    );
    // The branch that has a conversation, and the checkout that does not. Neither is a row: `^O`
    // means somewhere you do not work yet, and this repository is not that.
    assert!(!rows.iter().any(|l| l.contains("sprout")), "the tree is not offered\n{rows:?}");
    assert!(
        !rows.iter().any(|l| l.contains(&sb.work().display().to_string())),
        "and neither is the repository you are in\n{rows:?}"
    );
}

/// The trees it offers under "an existing one" are the ones on the panel's list, not every
/// checkout `git worktree list` can see.
///
/// A scratch branch you finished with in March is still a worktree on disk long after `X` took it
/// off the panel, so this picker was handing back every project you had ever removed — twenty rows
/// of finished work above the three you are in — and choosing one put it straight into the sidebar
/// again. The list is the workspace's answer to *which places do I work in*, and this
/// question asks exactly that.
///
/// Two worktrees, alike in every way git can see and differing only in whether anything was ever
/// started in one of them, which is what makes this a statement about the list rather than about
/// worktrees.
#[test]
fn a_worktree_that_is_not_on_the_list_is_not_offered() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("newwherelist");
    sb.git_init();
    let add = |branch: &str| {
        let at = sb.root.join(branch);
        let out = Command::new("git")
            .current_dir(sb.work())
            .args(["worktree", "add", "-b", branch, &at.display().to_string()])
            .output()
            .expect("git runs");
        assert!(out.status.success(), "worktree add: {}", String::from_utf8_lossy(&out.stderr));
        at
    };
    let onlist = add("onlist");
    add("offlist");

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    // A conversation started in a directory is how it joins the list. The other tree gets nothing,
    // which is the state `X` leaves behind: on the disk, in git, on nobody's list.
    s.send(&command_with("project.open", &onlist.display().to_string()));
    assert!(s.pump(|s| s.sidebar_rows() >= 2), "it is a project now\n{:?}", s.sidebar_now());
    // Asked from the checkout rather than from inside the new tree — the tree you are standing in
    // is never one of the answers, so asking there could not tell the two rules apart.
    s.send(&command_with("project.open", &sb.work().display().to_string()));
    assert!(s.pump(|s| s.sidebar_rows() >= 3), "and we are back\n{:?}", s.sidebar_now());

    s.ctrl("n");
    assert!(
        s.pump(|s| s.picker_named("[New conversation]").iter().any(|l| l.contains("onlist"))),
        "the tree on the list is a row\n{:?}",
        s.picker_named("[New conversation]")
    );
    let rows = s.picker_named("[New conversation]");
    assert!(rows.iter().any(|l| l.contains("Here")), "the question is still asked\n{rows:?}");
    assert!(
        !rows.iter().any(|l| l.contains("offlist")),
        "and the one nobody kept is not\n{rows:?}"
    );
}

/// The picker's in-project row works with nothing configured: the tree lands in `.worktrees/`
/// and `.gitignore` keeps it out of `git status` — no `worktree.root` edit required.
///
/// The ignore line goes in the *tracked* file, not `.git/info/exclude`, because the exclude file
/// is per clone: a colleague pulling the layout would rediscover the untracked-directory noise
/// this exists to prevent. And it names the worktrees' directory, not the tree — one line that
/// covers every branch after it, in a file everyone shares.
#[test]
fn a_worktree_inside_the_project_needs_no_configuration() {
    let sb = Sandbox::new("wtinsidecmd");
    sb.git_init();
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.send(&command("git.worktree.new.inside"));

    let under = sb.work().join(".worktrees");
    assert!(
        s.pump(|_| std::fs::read_dir(&under).is_ok_and(|d| d.count() > 0)),
        "a worktree appeared under {} with nothing asked and nothing configured",
        under.display()
    );
    let gitignore = sb.work().join(".gitignore");
    assert!(
        s.pump(|_| std::fs::read_to_string(&gitignore)
            .is_ok_and(|t| t.lines().any(|l| l.trim() == "/.worktrees/"))),
        "and .gitignore keeps the whole directory out of git status, for every clone\n{:?}",
        std::fs::read_to_string(&gitignore)
    );
}

/// `git.worktree.remove <path>` takes the checkout off the disk without a picker — the sidebar's
/// `d` is a row that already points at one, and a picker opened from it asks you to point twice.
///
/// The removal runs from the main checkout regardless of where the active conversation is,
/// because `git worktree remove` refuses to saw off the branch it is sitting on.
#[test]
fn a_worktree_is_removed_by_its_path_without_a_picker() {
    let sb = Sandbox::new("wtremove");
    sb.git_init();
    sb.write_config("[options]\n\"ui.confirm_destructive\" = false\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("PROJECTS");

    s.send(&command("git.worktree.new.inside"));
    // Waited for by the notification the verb ends with, not by the directory appearing. Those are
    // the two halves of it: `git worktree add` puts the checkout on disk, and the conversation is
    // created in it afterwards. The refusal asserted below is about the second half — so a test
    // that moved as soon as the directory existed was asking to remove a tree nothing was standing
    // in yet, which is a removal that succeeds.
    assert!(s.pump(|s| s.saw("branched ")), "the tree was made and entered\n{}", s.transcript());
    let under = sb.work().join(".worktrees");
    assert!(
        s.pump(|_| std::fs::read_dir(&under).is_ok_and(|d| d.count() > 0)),
        "the worktree exists to begin with"
    );
    // Canonicalised, because the worktree list reports real paths and `$TMPDIR` on macOS is a
    // symlink — `/var/folders/…` names a directory git will only ever call `/private/var/…`.
    let made = std::fs::read_dir(&under)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .next()
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
        .expect("one worktree");

    // Landing in the new tree is the point of creating one — which means removing it has to be
    // refused right now: the conversation is standing in it.
    s.send(&command_with("git.worktree.remove", &made.display().to_string()));
    assert!(
        s.pump(|s| s.saw("switch to another conversation")),
        "removing the tree you are in is refused with a reason"
    );

    // From the main checkout it goes.
    s.send(&command_with("project.open", &sb.work().display().to_string()));
    s.send(&command_with("git.worktree.remove", &made.display().to_string()));
    assert!(s.pump(|_| !made.exists()), "the checkout is gone from the disk");
}

/// Deleting the last conversation in a worktree takes the checkout with it.
///
/// A worktree is made for the conversations in it — that is what `^N`'s second row does — so
/// the last of them going is the directory's reason for being on the disk going. The dialog is
/// off here so the command answers at once; what it would have said is the sidebar's business
/// and the checkout going is the host's.
#[test]
fn deleting_the_last_conversation_in_a_worktree_removes_the_checkout() {
    let sb = Sandbox::new("wtdelete");
    sb.git_init();
    sb.write_config("[options]\n\"ui.confirm_destructive\" = false\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("PROJECTS");

    s.send(&command("git.worktree.new.inside"));
    assert!(s.pump(|s| s.saw("branched ")), "the tree was made and entered\n{}", s.transcript());
    let under = sb.work().join(".worktrees");
    let made = std::fs::read_dir(&under)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .next()
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
        .expect("one worktree");
    // Something uncommitted in it, which is what makes `git worktree remove` refuse unless the
    // dialog has already said the changes go — and it has.
    std::fs::write(made.join("draft.txt"), "half done\n").expect("write");

    // The conversation in the tree, found by where it is rather than by being current: closing
    // the one you are in lands you somewhere else first, and that is the host's to arrange.
    assert!(
        s.pump(|_| session_in(&sb, &made).is_some()),
        "a conversation in the worktree was saved"
    );
    let in_tree = session_in(&sb, &made).expect("found above");
    // Out of the tree, so the removal is not of the checkout the workspace is standing in.
    s.send(&command_with("project.open", &sb.work().display().to_string()));
    assert!(s.pump(|s| s.sidebar_rows() >= 2), "back in the repository\n{:?}", s.sidebar_now());

    s.send(&command_with("session.close", &in_tree));
    assert!(
        s.pump(|_| !made.exists()),
        "the checkout went with its last conversation\n{}",
        s.transcript()
    );
    assert!(
        s.pump(|s| !s.projects_now().iter().any(|l| l.contains("nothing here yet"))),
        "and its row went too\n{:?}",
        s.sidebar_now()
    );
}

/// The id of a saved conversation whose directory is `dir`, read off the state directory.
///
/// Compared canonicalised on both sides: the worktree was found through `$TMPDIR`, which on macOS
/// is a symlink, and the conversation was created at whatever spelling git handed back.
fn session_in(sb: &Sandbox, dir: &Path) -> Option<String> {
    let want = std::fs::canonicalize(dir).ok()?;
    let files = std::fs::read_dir(sb.root.join("state/sessions")).ok()?;
    files.filter_map(|e| e.ok()).find_map(|e| {
        let text = std::fs::read_to_string(e.path()).ok()?;
        let v: Value = serde_json::from_str(&text).ok()?;
        let cwd = std::fs::canonicalize(v["cwd"].as_str()?).ok()?;
        (cwd == want).then(|| v["id"].as_str().map(str::to_string))?
    })
}

/// `git.pull` brings the remote's commits into the conversation's checkout, and what git said
/// about it is the answer rather than silence.
#[test]
fn pulling_brings_the_remote_changes_into_the_checkout() {
    let sb = Sandbox::new("gitpull");
    sb.git_init();
    let git = |dir: &std::path::Path, args: &[&str]| {
        let out = Command::new("git").current_dir(dir).args(args).output().expect("git runs");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    // A bare "origin" beside the checkout, and a second clone playing the colleague who pushed.
    git(&sb.root, &["clone", "--bare", "--quiet", "work", "origin.git"]);
    git(&sb.work(), &["remote", "add", "origin", "../origin.git"]);
    git(&sb.work(), &["fetch", "--quiet", "origin"]);
    git(&sb.work(), &["branch", "--set-upstream-to=origin/trunk", "trunk"]);
    git(&sb.root, &["clone", "--quiet", "origin.git", "peer"]);
    let peer = sb.root.join("peer");
    git(&peer, &["config", "user.email", "peer@neosh.invalid"]);
    git(&peer, &["config", "user.name", "peer"]);
    git(&peer, &["config", "commit.gpgsign", "false"]);
    std::fs::write(peer.join("news.txt"), "from the remote\n").expect("write");
    git(&peer, &["add", "."]);
    git(&peer, &["commit", "--quiet", "-m", "news"]);
    git(&peer, &["push", "--quiet", "origin", "trunk"]);

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.send(&command("git.pull"));
    assert!(
        s.pump(|_| sb.work().join("news.txt").is_file()),
        "the remote's commit arrived in the working tree\n{}",
        s.transcript()
    );
}

#[test]
fn a_new_conversation_outside_a_repository_does_not_ask() {
    let sb = Sandbox::new("newnoask");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    let before = s.sidebar_now().iter().filter(|l| l.contains("New conversation")).count();
    s.ctrl("n");
    assert!(
        s.pump(|s| {
            s.sidebar_now().iter().filter(|l| l.contains("New conversation")).count() > before
        }),
        "it just started one\n{:?}",
        s.sidebar_now()
    );
    assert!(
        s.picker_named("[New conversation]").is_empty(),
        "without asking a question with one answer"
    );
}

/// A worktree goes under `worktree.root`, as `<root>/<repo>/<branch>`, and you land in it.
///
/// The landing is the point. Creating a directory you then have to go and find is not a feature,
/// it is a chore with extra steps — and `^N` offering it only makes sense if choosing it puts you
/// where you asked to be.
#[test]
fn a_worktree_is_created_under_the_configured_root_and_you_land_in_it() {
    let sb = Sandbox::new("wtnew");
    sb.git_init();
    let root = sb.root.join("trees");
    sb.write_config(&format!(
        "[options]\n\"worktree.root\" = \"{}\"\n",
        root.display()
    ));
    let mut s = sb.start_letting_config_choose();
    s.wait_for("PROJECTS");

    s.send(&command("git.worktree.new"));
    s.wait_for("Branch for the new worktree");
    s.type_text("feat/thing");
    s.enter();

    // The layout says both halves: the repository, then the branch with its slash flattened,
    // because a directory is a name and not a path.
    let want = format!("{}/work/feat-thing", root.display());
    assert!(
        s.pump(|_| std::path::Path::new(&want).is_dir()),
        "the worktree is on disk at {want}"
    );
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("feat-thing"))),
        "and the conversation is in it\n{:?}",
        s.chat_now()
    );
}

/// A worktree is a row *inside* the repository it belongs to, named by its branch.
///
/// It used to be a top-level project named `work · sideline` — correct in that a worktree is a
/// place conversations live, and wrong as a list: four scratch trees of one repository read as
/// four unrelated projects, and the name carried the relationship precisely because the structure
/// did not. Nested, the repository's name is said once by the row above, and what is left to say
/// about the tree is the branch.
#[test]
fn a_worktree_nests_under_the_repository_it_belongs_to() {
    let sb = Sandbox::new("wtname");
    sb.git_init();
    let root = sb.root.join("trees");
    sb.write_config(&format!("[options]\n\"worktree.root\" = \"{}\"\n", root.display()));
    let mut s = sb.start_letting_config_choose();
    s.wait_for("PROJECTS");
    // The original checkout is just the repository — adding its branch would put a word that
    // changes on every `git switch` beside a name that never does.
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("work") && !l.contains('\u{b7}'))),
        "the main checkout is named plainly\n{:?}",
        s.sidebar_now()
    );

    s.send(&command_with("git.worktree.new", "sideline"));
    assert!(
        s.pump(|s| {
            // The project list alone: the git block above it prints the branch, which is the same
            // word and is not a project row. See `projects_now`.
            let rows = s.projects_now();
            let repo = rows.iter().position(|l| l.contains("work") && !l.contains("sideline"));
            let tree = rows.iter().position(|l| l.contains("sideline"));
            match (repo, tree) {
                // Below its repository, indented past the repository's own arrow, and the branch
                // alone — `work · sideline` is the flat list this replaced.
                (Some(r), Some(t)) => {
                    t > r
                        && rows[t].starts_with(CONVERSATION_INDENT)
                        && !rows[t].contains('\u{b7}')
                }
                _ => false,
            }
        }),
        "the worktree is a nested row named by its branch\n{:?}",
        s.sidebar_now()
    );
}

/// The project list is asked for by name, never taken as "the whole sidebar".
///
/// This replaced a test that waited for the git block to print the branch above `PROJECTS` and
/// then checked the tree was still nested below it. That block is gone — the stats it drew are on
/// the rows now — so the specific trap it guarded cannot happen from that direction any more.
///
/// The helper stays and so does a test of it, because the hazard is structural rather than about
/// git: `projects_now()` means "the rows that are projects", anything contributing a section above
/// the list can put any word there, and a worktree is *named by its branch* — so a future block
/// naming a branch would resurrect exactly this bug in a test that reads the whole column.
#[test]
fn the_project_list_is_the_rows_below_the_projects_heading() {
    let sb = Sandbox::new("wtscope");
    sb.git_init();
    let root = sb.root.join("trees");
    sb.write_config(&format!("[options]\n\"worktree.root\" = \"{}\"\n", root.display()));
    let mut s = sb.start_letting_config_choose();
    s.wait_for("PROJECTS");
    s.send(&command_with("git.worktree.new", "sideline"));

    assert!(
        s.pump(|s| {
            let rows = s.projects_now();
            let repo = rows.iter().position(|l| l.contains("work") && !l.contains("sideline"));
            let tree = rows.iter().position(|l| l.contains("sideline"));
            matches!((repo, tree), (Some(r), Some(t)) if t > r)
        }),
        "the tree is below its repository in the project list\n{:?}",
        s.sidebar_now()
    );
    // And the helper really is a slice rather than the whole column: everything it returns is at or
    // below the heading, so a section above the list can never be mistaken for a project row.
    let all = s.sidebar_now();
    let rows = s.projects_now();
    assert!(rows.len() <= all.len(), "the slice is no longer than the column");
    assert!(
        rows.first().is_none_or(|l| l.contains("PROJECTS")),
        "and it starts at the heading\n{rows:?}"
    );
}

/// And it goes when you have deleted everything you were doing in there.
///
/// A worktree is made for the conversations in it — that is what `^N`'s second row does — so the
/// last of them going is the checkout's reason for being on the disk going with it: the directory
/// is removed, the branch stays, and the row goes rather than sitting under the repository saying
/// `nothing here yet` about a place that is not there. The repository it was a tree of is untouched.
/// This used to keep the emptied tree as a project of its own, which was a directory of a full
/// checkout kept for a row nobody would press.
#[test]
fn an_emptied_worktree_goes_from_the_disk_and_the_list() {
    let sb = Sandbox::new("wtempty");
    sb.git_init();
    let root = sb.root.join("trees");
    sb.write_config(&format!(
        "[options]\n\"worktree.root\" = \"{}\"\n\"ui.confirm_destructive\" = false\n",
        root.display()
    ));
    let mut s = sb.start_letting_config_choose();
    s.wait_for("PROJECTS");

    s.send(&command_with("git.worktree.new", "sideline"));
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("sideline"))),
        "the worktree is there to begin with\n{:?}",
        s.sidebar_now()
    );
    let made = root.join("work").join("sideline");
    assert!(s.pump(|_| made.is_dir()), "and on the disk at {}", made.display());

    // The conversation the worktree was made for is the active one, so this empties the tree.
    s.send(&command("session.close"));
    assert!(
        s.pump(|_| !made.exists()),
        "the checkout went with its last conversation\n{}",
        s.transcript()
    );
    assert!(
        s.pump(|s| !s.sidebar_now().iter().any(|l| l.contains("sideline"))),
        "and its row went too\n{:?}",
        s.sidebar_now()
    );
    assert!(
        s.sidebar_now().iter().any(|l| l.contains("work")),
        "the repository it was a tree of is still there\n{:?}",
        s.sidebar_now()
    );
    let branches = Command::new("git")
        .current_dir(sb.work())
        .args(["branch", "--list", "sideline"])
        .output()
        .expect("git runs");
    assert!(
        String::from_utf8_lossy(&branches.stdout).contains("sideline"),
        "and the branch stays: the checkout was the space, the branch is the work"
    );
}

/// No questions at all. A branch named before the work is a decision made at the worst possible
/// moment, and needing to make it is what stops people reaching for a clean tree in the first
/// place.
///
/// The generated name has to be a *name* — two words you can say out loud and find again in a list
/// of eight of them — which is why this asserts the shape rather than merely that a directory
/// appeared: `wt-3` and a timestamp would both pass "it exists".
#[test]
fn a_worktree_can_be_had_without_answering_anything() {
    let sb = Sandbox::new("wtauto");
    sb.git_init();
    let root = sb.root.join("trees");
    sb.write_config(&format!("[options]\n\"worktree.root\" = \"{}\"\n", root.display()));
    let mut s = sb.start_letting_config_choose();
    s.wait_for("PROJECTS");

    s.send(&command("git.worktree.new.auto"));

    let under = root.join("work");
    assert!(
        s.pump(|_| std::fs::read_dir(&under).is_ok_and(|d| d.count() > 0)),
        "a worktree appeared under {} with nothing asked",
        under.display()
    );
    assert!(s.prompt_now().is_empty(), "and no field was opened\n{:?}", s.prompt_now());

    let made = std::fs::read_dir(&under)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .next()
        .expect("one worktree");
    // Words rather than an identifier, and then a tag: the words are what you read out of the
    // panel, the tag is what stops the twentieth tree landing on a name already on disk.
    let parts: Vec<&str> = made.split('-').collect();
    assert_eq!(parts.len(), 3, "two words and a tag: {made}");
    assert!(
        parts[..2].iter().all(|w| w.len() > 2 && w.chars().all(|c| c.is_ascii_lowercase())),
        "it is words rather than an identifier: {made}"
    );
    assert!(
        parts[2].len() == 4 && parts[2].chars().all(|c| c.is_ascii_alphanumeric()),
        "and a four-character tag: {made}"
    );
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains(&made))),
        "and the conversation landed in it\n{:?}",
        s.chat_now()
    );
}

/// `n` in the panel asks the same question `^N` does.
///
/// They used to differ: `^N` opened the picker and `n` created one outright. One letter and a
/// modifier apart, doing visibly different things, which makes "which one did I want" a thing you
/// can only answer by having already been surprised once.
#[test]
fn the_panel_key_for_a_new_conversation_asks_what_the_chat_key_asks() {
    let sb = Sandbox::new("panelnew");
    sb.git_init();
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.enter_panel();
    s.key("n");
    assert!(
        s.pump(|s| !s.picker_named("[New conversation]").is_empty()),
        "the same picker\n{:?}",
        s.sidebar_now()
    );
    let rows = s.picker_named("[New conversation]");
    assert!(rows.iter().any(|l| l.contains("Here")), "with Here first\n{rows:?}");
}

/// One question. The location is a setting; asking again is asking somebody to repeat themselves.
#[test]
fn making_a_worktree_asks_only_for_the_branch() {
    let sb = Sandbox::new("wtoneq");
    sb.git_init();
    let root = sb.root.join("trees");
    sb.write_config(&format!("[options]\n\"worktree.root\" = \"{}\"\n", root.display()));
    let mut s = sb.start_letting_config_choose();
    s.wait_for("PROJECTS");

    s.send(&command("git.worktree.new"));
    s.wait_for("Branch for the new worktree");
    s.type_text("quick");
    s.enter();

    let want = format!("{}/work/quick", root.display());
    assert!(
        s.pump(|_| std::path::Path::new(&want).is_dir()),
        "it went straight to {want} without a second prompt"
    );
    assert!(
        s.prompt_now().is_empty() || !s.prompt_now().iter().any(|l| l.contains("Create worktree at")),
        "nothing asked where\n{:?}",
        s.prompt_now()
    );
}

/// An empty root restores the old sibling layout, for anyone who wants their trees beside the
/// thing they are trees of.
#[test]
fn an_empty_worktree_root_puts_them_beside_the_repository() {
    let sb = Sandbox::new("wtsibling");
    sb.git_init();
    sb.write_config("[options]\n\"worktree.root\" = \"\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("PROJECTS");

    s.send(&command("git.worktree.new"));
    s.wait_for("Branch for the new worktree");
    s.type_text("side");
    s.enter();
    let want = format!("{}/work-worktrees/side", sb.root.display());
    assert!(
        s.pump(|_| std::path::Path::new(&want).is_dir()),
        "the worktree went beside the repository, at {want}"
    );
}

/// A relative root is inside the repository itself — `<repo>/.worktrees/<branch>`, with no
/// `<repo>` level, because inside the repository nothing else's worktrees can collide with yours.
///
/// The ignore entry is the half that makes the layout livable: git happily creates a worktree
/// inside its own repository and then reports the entire checkout as one untracked directory, in
/// every status, forever. It goes in the repository's `.gitignore` — tracked, so every clone gets
/// it — and names the worktrees' directory once rather than growing a line per branch.
#[test]
fn a_relative_worktree_root_lives_inside_the_repository_and_stays_out_of_status() {
    let sb = Sandbox::new("wtinside");
    sb.git_init();
    sb.write_config("[options]\n\"worktree.root\" = \".worktrees\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("PROJECTS");

    s.send(&command("git.worktree.new"));
    s.wait_for("Branch for the new worktree");
    s.type_text("inside");
    s.enter();

    let repo = sb.root.join("work");
    let want = repo.join(".worktrees/inside");
    assert!(
        s.pump(|_| want.is_dir()),
        "the worktree is inside the repository, at {}",
        want.display()
    );
    let gitignore = repo.join(".gitignore");
    assert!(
        s.pump(|_| std::fs::read_to_string(&gitignore)
            .is_ok_and(|t| t.lines().any(|l| l.trim() == "/.worktrees/"))),
        "and .gitignore keeps it out of git status\n{:?}",
        std::fs::read_to_string(&gitignore)
    );
}

#[test]
fn folding_a_project_hides_its_conversations_and_keeps_the_count() {
    let sb = Sandbox::new("fold");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    for c in ["t", "a", "l", "k"] {
        s.key(c);
    }
    s.enter();
    s.wait_for("talk");

    s.enter_panel();
    // From the conversation the panel lands on, up onto the project it belongs to.
    s.special("up");
    s.key(" ");
    assert!(
        s.pump(|s| !s.sidebar_now().iter().any(|l| l.contains("talk"))),
        "the conversation is folded away\n{:?}",
        s.sidebar_now()
    );

    s.key(" ");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("talk"))),
        "and unfolds again\n{:?}",
        s.sidebar_now()
    );
}

#[test]
fn a_second_project_can_be_opened_and_dragged_above_the_first() {
    let sb = Sandbox::new("reorder");
    let other = sb.root.join("elsewhere");
    std::fs::create_dir_all(&other).expect("mkdir");

    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.send(&serde_json::json!({
        "type": "command",
        "name": "project.open",
        "args": [other.display().to_string()],
    })
    .to_string());
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("elsewhere"))),
        "the second project appears\n{:?}",
        s.sidebar_now()
    );

    // Most recently used first, so the new one leads.
    let before = s.sidebar_now();
    let elsewhere = before.iter().position(|l| l.contains("elsewhere")).expect("row");
    let work = before.iter().position(|l| l.contains("work")).expect("row");
    assert!(elsewhere < work, "the newest project leads to begin with\n{before:?}");

    // Onto `work` — one row down from the conversation the panel lands on — then move it up past
    // `elsewhere`. Each step waits: keys reach the plugin thread asynchronously, so a burst races
    // itself and half of it lands in the composer.
    s.enter_panel();
    s.down();
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("↵ fold"))),
        "the cursor is on a project\n{:?}",
        s.sidebar_now()
    );
    s.key("K");
    assert!(
        s.pump(|s| {
            let p = s.sidebar_now();
            let e = p.iter().position(|l| l.contains("elsewhere"));
            let w = p.iter().position(|l| l.contains(" work"));
            matches!((w, e), (Some(w), Some(e)) if w < e)
        }),
        "the project moved above the one that was newer\n{:?}",
        s.sidebar_now()
    );
}

/// A project is a place you work, not a property of the work in it.
///
/// The list used to be derived from where the conversations were, so emptying a project deleted it:
/// you cleared out a month of threads and the directory you had been working in all month went with
/// them, leaving nothing to start the next one from.
#[test]
fn a_project_outlives_the_conversations_in_it() {
    let sb = Sandbox::new("emptyproject");
    sb.write_config("[options]\n\"ui.confirm_destructive\" = false\n");
    let other = sb.root.join("elsewhere");
    std::fs::create_dir_all(&other).expect("mkdir");

    {
        let mut s = sb.start();
        s.wait_for("PROJECTS");
        s.send(&command_with("project.open", &other.display().to_string()));
        assert!(
            s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("elsewhere"))),
            "the second project appears\n{:?}",
            s.sidebar_now()
        );

        // The conversation `project.open` started is the active one, so this is the last thing in
        // that directory going away.
        s.send(&command("session.close"));
        assert!(
            s.pump(|s| {
                let p = s.sidebar_now();
                p.iter().any(|l| l.contains("elsewhere"))
                    && p.iter().any(|l| l.contains("nothing here yet"))
            }),
            "the project is still a row, with nothing in it\n{:?}",
            s.sidebar_now()
        );
    }

    // And still there next time: the list is written down rather than worked out again from
    // whatever happens to be in it.
    let mut s = sb.start();
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("elsewhere"))),
        "the project survived a restart\n{:?}",
        s.sidebar_now()
    );
}

/// Which means there has to be a way off the list, and it is the key that already means "for good".
#[test]
fn a_project_is_removed_by_the_key_on_its_heading() {
    let sb = Sandbox::new("removeproject");
    sb.write_config("[options]\n\"ui.confirm_destructive\" = false\n");
    let other = sb.root.join("elsewhere");
    std::fs::create_dir_all(&other).expect("mkdir");

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.send(&command_with("project.open", &other.display().to_string()));
    assert!(s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("elsewhere"))));
    s.send(&command("session.close"));
    assert!(s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("nothing here yet"))));

    // The cursor lands on the conversation you are in — the one left in `work` — and the row under
    // it is the empty project's heading.
    s.enter_panel();
    s.down();
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("↵ fold"))),
        "the cursor is on a project, and the strip says so\n{:?}",
        s.sidebar_now()
    );

    s.key("X");
    assert!(
        s.pump(|s| !s.sidebar_now().iter().any(|l| l.contains("elsewhere"))),
        "the project is off the list\n{:?}",
        s.sidebar_now()
    );
    // And the one it was sitting next to is untouched.
    assert!(
        s.sidebar_now().iter().any(|l| l.contains("work")),
        "the other project is still here\n{:?}",
        s.sidebar_now()
    );
}

#[test]
fn the_key_hints_follow_what_the_cursor_is_on() {
    // A terminal UI that hides its verbs is a terminal UI you use three of.
    let sb = Sandbox::new("hints");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    // The way in is on the heading of the thing it opens, and the foot keeps the doors out.
    assert!(
        s.pump(|s| {
            s.sidebar_virt_now().iter().any(|v| v.trim() == "^T")
                && s.sidebar_now().iter().any(|l| l.contains("^F archive"))
        }),
        "unfocused, the heading says how to get in and the foot says the main keys\n{:?}\n{:?}",
        s.sidebar_now(),
        s.sidebar_virt_now()
    );

    s.enter_panel();
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("↵ open"))),
        "on a conversation, the strip says what ↵ does here\n{:?}",
        s.sidebar_now()
    );
    assert!(
        s.pump(|s| s.sidebar_virt_now().iter().any(|v| v.trim() == "esc")),
        "and the heading now says the way out\n{:?}",
        s.sidebar_virt_now()
    );

    s.special("up");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("↵ fold"))),
        "on a project, ↵ is a fold\n{:?}",
        s.sidebar_now()
    );
}

/// The key card: pause on a row and every key for it is beside the panel, level with the row.
///
/// Built from the registry, so it is checked the way the sheet is — by the verbs it names — and it
/// follows the cursor: what it said on the conversation is not what it says on the project.
#[test]
fn the_key_card_lists_the_row_under_the_cursor_and_follows_it() {
    let sb = Sandbox::new("keycard");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.enter_panel();
    assert!(
        s.pump(|s| s.key_card().iter().any(|l| l.contains("rename"))),
        "on a conversation, the card says the conversation's keys\n{:?}",
        s.key_card()
    );
    let card = s.key_card();
    assert!(
        card.iter().any(|l| l.contains("archive")) && card.iter().any(|l| l.contains("delete")),
        "all of them\n{card:?}"
    );
    assert!(
        !card.iter().any(|l| l.contains("fold")),
        "and not a project's\n{card:?}"
    );

    s.special("up");
    assert!(
        s.pump(|s| s.key_card().iter().any(|l| l.contains("fold or unfold"))),
        "on a project, the card follows\n{:?}",
        s.key_card()
    );
    assert!(
        !s.key_card().iter().any(|l| l.contains("rename")),
        "and stops saying the conversation's\n{:?}",
        s.key_card()
    );

    // Leaving takes the card with the keyboard: a card about a row nobody is on is a card that
    // covers the transcript for no reason.
    s.special("esc");
    assert!(
        s.pump(|s| s.window_closed_for("[sidebar keys]")),
        "the card closed with the panel\n{:?}",
        s.key_card()
    );
}

/// A pinned project with a git badge is one row under the cursor, not two.
///
/// The badge is appended to the row after the star and the unread mark, and it used to be appended
/// to the row's *unclipped* text as well — which has neither — so the two stopped being prefixes
/// of each other, and the list, finding nothing to trust, said the whole row again underneath:
/// name, badge and all, on the one row you had just moved onto.
#[test]
fn a_pinned_project_with_a_badge_is_not_said_twice_under_the_cursor() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("badgeonce");
    sb.git_init();
    std::fs::write(sb.work().join("scratch.txt"), "untracked\n").expect("write");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    assert!(
        s.pump(|s| s.projects_now().iter().any(|l| l.contains("?1"))),
        "the git plugin has marked the row\n{:?}",
        s.sidebar_now()
    );

    s.enter_panel();
    s.special("up");
    assert!(
        s.pump(|s| s.sidebar_cursor().is_some_and(|r| r.contains("work"))),
        "the cursor is on the project\n{:?}",
        s.sidebar_now()
    );
    s.key("f");
    assert!(
        s.pump(|s| s.projects_now().iter().any(|l| l.contains('★'))),
        "and it is pinned\n{:?}",
        s.sidebar_now()
    );
    s.drain_for(Duration::from_millis(600));
    let rows = s.projects_now();
    let badged: Vec<&String> = rows.iter().filter(|l| l.contains("?1")).collect();
    assert_eq!(badged.len(), 1, "the badge is on one row and nowhere else\n{rows:?}");
    let named: Vec<&String> = rows.iter().filter(|l| l.contains("work")).collect();
    assert_eq!(named.len(), 1, "and so is the name\n{rows:?}");
}

#[test]
fn the_key_card_can_be_switched_off() {
    let sb = Sandbox::new("nokeycard");
    sb.write_config("[options]\n\"sidebar.legend\" = false\n");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.enter_panel();
    s.drain_for(Duration::from_secs(1));
    assert!(
        s.buffer_named("[sidebar keys]").is_none(),
        "no card\n{:?}",
        s.key_card()
    );
}

impl Session {
    /// What the key card says right now, or nothing while there is none.
    fn key_card(&self) -> Vec<String> {
        match self.buffer_named("[sidebar keys]") {
            Some(b) => self.lines_of(b),
            None => Vec::new(),
        }
    }

    /// Whether every window that ever showed the named buffer has since closed.
    fn window_closed_for(&self, name: &str) -> bool {
        let Some(buf) = self.buffer_named(name) else { return false };
        let opened: Vec<u64> = self
            .events
            .iter()
            .filter(|e| e["type"] == "window_opened" && e["buf"].as_u64() == Some(buf))
            .filter_map(|e| e["win"].as_u64())
            .collect();
        !opened.is_empty()
            && opened.iter().all(|w| {
                self.events.iter().any(|e| e["type"] == "window_closed" && e["win"].as_u64() == Some(*w))
            })
    }
}

#[test]
fn the_hint_strip_can_be_switched_off() {
    let sb = Sandbox::new("nohints");
    sb.write_config("[options]\n\"sidebar.hints\" = false\n");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.drain_for(Duration::from_secs(1));
    assert!(
        !s.sidebar_now().iter().any(|l| l.contains("^F archive")),
        "no hint strip\n{:?}",
        s.sidebar_now()
    );
}

/// The float a widget opened, folded the way a frontend folds splices.
impl Session {
    fn float_named(&self, prefix: &str) -> Vec<String> {
        let name = self.events.iter().find_map(|e| {
            let n = e["name"].as_str()?;
            (e["type"] == "buffer_opened" && n.starts_with(prefix)).then(|| n.to_string())
        });
        let Some(name) = name else { return Vec::new() };
        let Some(buf) = self.buffer_named(&name) else { return Vec::new() };
        let mut lines: Vec<String> = Vec::new();
        for e in &self.events {
            if e["type"] != "buffer_lines" || e["buf"].as_u64() != Some(buf) {
                continue;
            }
            let start = e["start"].as_i64().unwrap_or(0);
            let old_end = e["old_end"].as_i64().unwrap_or(start);
            let new: Vec<String> = e["lines"]
                .as_array()
                .map(|a| a.iter().filter_map(|l| l["text"].as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let s = start.clamp(0, lines.len() as i64) as usize;
            let en =
                if old_end < 0 { lines.len() } else { (old_end as usize).clamp(s, lines.len()) };
            lines.splice(s..en, new);
        }
        lines
    }
}

impl Session {
    /// Where the terminal caret was last put in a window.
    fn caret_in(&self, win: u64) -> Option<(u64, u64)> {
        self.events
            .iter()
            .filter(|e| e["type"] == "cursor_moved" && e["win"].as_u64() == Some(win))
            .filter_map(|e| Some((e["row"].as_u64()?, e["col"].as_u64()?)))
            .next_back()
    }

    fn window_showing(&self, buffer_prefix: &str) -> Option<u64> {
        let name = self.events.iter().find_map(|e| {
            let n = e["name"].as_str()?;
            (e["type"] == "buffer_opened" && n.starts_with(buffer_prefix)).then(|| n.to_string())
        })?;
        let buf = self.buffer_named(&name)?;
        self.windows_for(buf).first().copied()
    }
}

/// Start a conversation called `keep`, then archive it from the panel.
///
/// Shared by both archive tests: getting there is six keystrokes of setup and neither test is about
/// the setup.
fn archive_one(s: &mut Session) {
    for c in ["k", "e", "e", "p"] {
        s.key(c);
    }
    s.enter();
    s.wait_for("keep");
    s.new_conversation(); // a second conversation, so the first is not the only one left

    s.enter_panel();
    assert!(s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("keep"))));
    s.down();
    assert!(
        s.pump(|s| s.sidebar_cursor().is_some_and(|r| r.contains("keep"))),
        "the cursor is on the conversation\n{:?}",
        s.sidebar_now()
    );
    s.key("x");
}

#[test]
fn archiving_takes_a_conversation_out_of_the_panel_altogether() {
    // The everyday verb, and it asks nothing, because there is nothing to lose. What it leaves
    // behind is *nothing at all* in this column: not a section of dim rows, and not a row saying
    // how many either. The archive is a popup on `^F`, and the panel is the list you work in.
    let sb = Sandbox::new("archive");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    let panel_only = s.open_windows().len();
    archive_one(&mut s);
    assert_eq!(s.open_windows().len(), panel_only, "and nothing asked");
    assert!(
        s.pump(|s| !s.sidebar_now().iter().any(|l| l.contains("Archived"))),
        "nothing about the archive is in the column\n{:?}",
        s.sidebar_now()
    );

    // Not folded away, not dimmed at the foot: gone. Every row of the panel is drained first, so
    // this is about what the panel says *now* rather than what it ever said.
    assert!(
        s.pump(|s| !s.sidebar_now().iter().any(|l| l.contains("keep"))),
        "and it left the panel entirely\n{:?}",
        s.sidebar_now()
    );
    // Including with everything unfolded — there is no state of this panel that shows it.
    s.key("j");
    s.key(" ");
    s.drain_for(Duration::from_millis(400));
    assert!(
        !s.sidebar_now().iter().any(|l| l.contains("keep")),
        "no fold brings it back\n{:?}",
        s.sidebar_now()
    );
}

#[test]
fn the_archive_is_a_place_you_go_and_come_back_from() {
    // `a` in the panel — and `^F` from anywhere — opens what you have put away, as a panel you move
    // in. `u` on a row puts it back without switching to it, because tidying and going somewhere
    // are different things.
    //
    // `a` arrives as an `archive.action` contribution rather than as a key the sidebar draws for
    // itself: the archive is another plugin's panel, and the door to it is that plugin's to put
    // there. Which is also what this asserts by pressing it.
    let sb = Sandbox::new("browse");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    archive_one(&mut s);
    assert!(s.pump(|s| !s.sidebar_now().iter().any(|l| l.contains("keep"))), "it left the list");

    s.key("a");
    assert!(
        s.pump(|s| s.picker_named("[archive]").iter().any(|l| l.contains("keep"))),
        "the archive opened with it in\n{:?}",
        s.picker_named("[archive]")
    );
    // It says how many, and what the keys are — a panel with no verbs on it is a modal you have to
    // guess your way out of.
    assert!(
        s.pump(|s| s.picker_named("[archive]").iter().any(|l| l.contains("1 archived"))),
        "the header counts what is in it\n{:?}",
        s.picker_named("[archive]")
    );
    assert!(
        s.pump(|s| s.picker_named("[archive]").iter().any(|l| l.contains("^X empty"))),
        "and the way to empty it is on screen\n{:?}",
        s.picker_named("[archive]")
    );

    s.key("u");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("keep"))),
        "`u` put it back in the list\n{:?}",
        s.sidebar_now()
    );
}

#[test]
fn the_count_in_the_panel_is_something_you_ask_for() {
    // Off by default: the column is your conversations, and a permanent line about the ones you are
    // finished with is the space this panel can least afford. `archive.sidebar = true` is for
    // somebody who wants the number on screen, and it is the *only* way that row appears.
    let sb = Sandbox::new("archiverow");
    sb.write_config("[options]\n\"archive.sidebar\" = true\n");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    archive_one(&mut s);
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("Archived"))),
        "asked for, so the row is there\n{:?}",
        s.sidebar_now()
    );
    // The count and the key it opens with ride along as virtual text, so they are on the wire
    // rather than in the buffer — a row with a number and no way in is a fact you cannot act on.
    assert!(s.wire().contains("^F "), "with the key that opens it");
}

#[test]
fn emptying_the_archive_asks_once_and_then_empties_it() {
    // The verb the archive did not have: one key for everything in it, rather than a delete and a
    // confirmation per conversation. It is irreversible, so it asks — and the question says how
    // many and how much is in them, which is the part that makes it worth stopping for.
    let sb = Sandbox::new("empty");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    archive_one(&mut s);
    let panel_only = s.open_windows().len();
    s.key("a");
    assert!(s.pump(|s| s.picker_named("[archive]").iter().any(|l| l.contains("keep"))));
    let with_archive = s.open_windows().len();

    s.ctrl("x");
    assert!(s.pump(|s| s.open_windows().len() > with_archive), "it asked first");
    assert!(
        s.pump(|s| s.saw("Empty the archive")),
        "and said what it was about to do\n{}",
        s.transcript()
    );
    assert!(
        s.pump(|s| s.saw("no undo")),
        "including that it cannot be taken back\n{}",
        s.transcript()
    );

    // The cursor starts on the answer that changes nothing, so the destructive one is chosen.
    s.special("up");
    s.enter();
    assert!(
        s.pump(|s| !s.sidebar_now().iter().any(|l| l.contains("keep"))),
        "what was in it did not come back to the list\n{:?}",
        s.sidebar_now()
    );
    // Nothing left behind it, so the panel takes itself off screen — waited for rather than
    // assumed, because the key below *toggles* and pressing it into a panel that is still closing
    // would shut it instead of reopening it.
    assert!(
        s.pump(|s| s.open_windows().len() == panel_only),
        "the dialog and the panel both went"
    );
    // And it really is empty. Asked by going back rather than by reading the buffer the panel left
    // behind: a scratch buffer keeps whatever was last drawn into it, so what it says after its
    // window has gone is what it said before the delete.
    s.key("a");
    assert!(
        s.pump(|s| s.saw("nothing is archived")),
        "the archive is empty\n{}",
        s.transcript()
    );
}

#[test]
fn a_verb_in_the_archive_is_about_everything_ticked() {
    // Marks are what make this a panel rather than a picker: `<Space>` ticks a row, and every verb
    // then means all of them. The strip says which, because a key whose arity you cannot see is one
    // you press twice to find out.
    let sb = Sandbox::new("marks");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    archive_one(&mut s);
    s.key("a");
    assert!(s.pump(|s| s.picker_named("[archive]").iter().any(|l| l.contains("keep"))));
    assert!(
        s.pump(|s| s.picker_named("[archive]").iter().any(|l| l.contains("(this one)"))),
        "with nothing ticked, a verb is about the row under the cursor\n{:?}",
        s.picker_named("[archive]")
    );

    s.key(" ");
    assert!(
        s.pump(|s| s.picker_named("[archive]").iter().any(|l| l.contains("1 marked"))),
        "ticking one says so, in the header and on the keys\n{:?}",
        s.picker_named("[archive]")
    );
    assert!(
        s.pump(|s| s.picker_named("[archive]").iter().any(|l| l.contains("(1 marked)"))),
        "and the verbs are now about it\n{:?}",
        s.picker_named("[archive]")
    );

    // `<Esc>` drops the marks before it drops the panel: one thing per press, so a mistyped key
    // does not cost a selection and the window it was made in at the same time.
    s.special("esc");
    assert!(
        s.pump(|s| s.picker_named("[archive]").iter().any(|l| l.contains("(this one)"))),
        "the marks went\n{:?}",
        s.picker_named("[archive]")
    );
    assert!(
        s.pump(|s| s.picker_named("[archive]").iter().any(|l| l.contains("keep"))),
        "and the panel did not\n{:?}",
        s.picker_named("[archive]")
    );
}

#[test]
fn deleting_a_conversation_with_something_in_it_asks_first() {
    // Deleting removes the file from disk. There is no undo, so this is the one verb in the panel
    // that stops and asks — and it is shifted, so it is not the one your fingers reach for.
    let sb = Sandbox::new("confirmdel");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    for c in ["k", "e", "e", "p"] {
        s.key(c);
    }
    s.enter();
    s.wait_for("keep");
    s.new_conversation(); // a second conversation, so deleting the first is even allowed

    s.enter_panel();
    assert!(s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("keep"))));
    s.down();
    assert!(
        s.pump(|s| s.sidebar_cursor().is_some_and(|r| r.contains("keep"))),
        "the cursor is on the conversation\n{:?}",
        s.sidebar_now()
    );
    let panel_only = s.open_windows().len();
    s.key("X");
    assert!(s.pump(|s| s.open_windows().len() > panel_only), "the dialog opened");

    // What is at stake, in the dialog, rather than a bare "are you sure?" you learn to clear.
    assert!(
        s.pump(|s| s.saw("messages, in")),
        "it says what there is to lose\n{}",
        s.transcript()
    );
    // And the answer that cannot be undone is not drawn like the one beside it.
    let dialog = s.buffer_named("[confirm]").expect("the dialog buffer");
    assert!(
        s.groups_of(dialog).iter().any(|row| row.iter().any(|g| g == "Diagnostic.Error")),
        "the destructive answer is coloured as one\n{:?}",
        s.groups_of(dialog)
    );
    // The row under the cursor is banded rather than repainted, which is what leaves the colour
    // above intact. A ranged group across the same bytes would take it.
    assert!(
        s.wire().contains(r#""line_hl_group":"Picker.Selected""#),
        "the cursor row is a band under the text, not a group over it"
    );

    // The cursor starts on the answer that changes nothing: `<CR>` is reflex by the second time you
    // have seen a dialog, and a reflex must not delete anything.
    s.enter();
    assert!(s.pump(|s| s.open_windows().len() == panel_only), "and it closed again");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("keep"))),
        "accepting the default kept it\n{:?}",
        s.sidebar_now()
    );

    // And saying yes really does delete it, so the dialog is a question rather than a speed bump.
    s.key("X");
    assert!(s.pump(|s| s.open_windows().len() > panel_only), "asked a second time");
    s.special("up");
    s.enter();
    assert!(
        s.pump(|s| !s.sidebar_now().iter().any(|l| l.contains("keep"))),
        "choosing Delete removed it\n{:?}",
        s.sidebar_now()
    );
}

#[test]
fn deleting_asks_even_when_there_is_nothing_in_it_to_lose() {
    // It used to go straight through for an empty conversation, on the grounds that a dialog for
    // something with nothing to lose is friction. What that actually bought was a key whose
    // behaviour depended on state you cannot see from the row it is pointed at — so the one time it
    // did stop and ask was the one time your fingers were already through it.
    let sb = Sandbox::new("nodialog");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("n");
    assert!(s.pump(|s| {
        s.sidebar_now().iter().filter(|l| l.contains("New conversation")).count() == 2
    }));

    s.enter_panel();
    let panel_only = s.open_windows().len();
    s.key("X");
    assert!(s.pump(|s| s.open_windows().len() > panel_only), "it asked anyway");
    assert!(
        s.pump(|s| s.saw("Nothing has been said in it yet")),
        "and said what there was to lose\n{}",
        s.transcript()
    );

    // `y` answers it outright, which is the point of a dialog with two answers and no filter.
    s.key("y");
    assert!(
        s.pump(|s| {
            s.sidebar_now().iter().filter(|l| l.contains("New conversation")).count() == 1
        }),
        "answering yes deleted it\n{:?}",
        s.sidebar_now()
    );
}

#[test]
fn confirmation_can_be_switched_off() {
    let sb = Sandbox::new("noconfirm");
    sb.write_config("[options]\n\"ui.confirm_destructive\" = false\n");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    for c in ["g", "o", "n", "e"] {
        s.key(c);
    }
    s.enter();
    s.wait_for("gone");
    s.new_conversation();

    s.enter_panel();
    assert!(s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("gone"))));
    s.down();
    assert!(s.pump(|s| s.sidebar_cursor().is_some_and(|r| r.contains("gone"))));
    s.key("X");
    assert!(
        s.pump(|s| !s.sidebar_now().iter().any(|l| l.contains("gone"))),
        "deleted without asking\n{:?}",
        s.sidebar_now()
    );
}

#[test]
fn the_caret_follows_what_you_type_into_a_picker() {
    // Left at the origin the caret sits on the title, and the field reads as inert: you type, the
    // text appears, and the cursor is somewhere else entirely — so it looks like nothing happened.
    let sb = Sandbox::new("caret");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.ctrl("p");
    s.wait_for("Model");
    let win = s.window_showing("[Model]").expect("the picker window");
    for c in ["m", "o"] {
        s.key(c);
    }
    assert!(
        s.pump(|s| s.caret_in(win).is_some_and(|(row, col)| row == 1 && col == 4)),
        "the caret is at the end of `> mo`, got {:?}",
        s.caret_in(win)
    );
    s.special("esc");
}

#[test]
fn typing_a_path_offers_the_directories_that_match_it() {
    // A path field with no completion is a path field you type wrong. The list under it is the
    // directories matching what you have typed, refreshed on every keystroke.
    let sb = Sandbox::new("pathpick");
    for name in ["alpha", "beta"] {
        std::fs::create_dir_all(sb.work().join(name)).expect("mkdir");
    }
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.send(&command("project.open"));
    // The float, not the sidebar's `+ Add project` row — which is on screen from the start and
    // would let the test type into the composer instead.
    assert!(s.pump(|s| !s.float_named("[Add project").is_empty()), "the field opened");

    for c in sb.work().display().to_string().chars() {
        s.key(&c.to_string());
    }
    s.key("/");

    assert!(
        s.pump(|s| {
            let f = s.float_named("[Add project");
            f.iter().any(|l| l.ends_with("alpha/")) && f.iter().any(|l| l.ends_with("beta/"))
        }),
        "both directories are offered\n{:?}",
        s.float_named("[Add project")
    );
}

#[test]
fn a_directory_the_system_refuses_says_so_rather_than_reading_as_empty() {
    use std::os::unix::fs::PermissionsExt;

    // Nothing there and not allowed to look are the same empty list, and they are not the same
    // answer. macOS is where this bites hardest — the privacy layer grants folder access to the
    // *terminal*, so `~/Documents` full of projects reads as empty until somebody ticks a box, and
    // a field drawing "no directory matches" over that sends people off to check a path that was
    // right all along. TCC is not something a test can arrange; mode bits reach the same code by
    // the same route, and which of the two sentences comes out is `access.rs`'s own unit tests.
    let sb = Sandbox::new("pathdenied");
    let shut = sb.work().join("shut");
    std::fs::create_dir_all(&shut).expect("mkdir");
    std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o000)).expect("chmod");

    // As root the mode bits are advisory, so there would be nothing here to refuse.
    if std::fs::read_dir(&shut).is_ok() {
        let _ = std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o755));
        return;
    }

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.send(&command("project.open"));
    assert!(s.pump(|s| !s.float_named("[Add project").is_empty()), "the field opened");

    for c in shut.display().to_string().chars() {
        s.key(&c.to_string());
    }
    s.key("/");

    let said = s.pump(|s| s.saw("no permission to read"));
    // Put the bits back before asserting: a failure must not leave the sandbox holding a directory
    // its own cleanup cannot remove.
    let _ = std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o755));
    assert!(said, "the refusal is on screen rather than an empty list");
}

#[test]
fn tab_takes_the_highlighted_completion_into_the_field() {
    let sb = Sandbox::new("pathtab");
    std::fs::create_dir_all(sb.work().join("alpha")).expect("mkdir");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.send(&command("project.open"));
    // The float, not the sidebar's `+ Add project` row — which is on screen from the start and
    // would let the test type into the composer instead.
    assert!(s.pump(|s| !s.float_named("[Add project").is_empty()), "the field opened");
    for c in sb.work().display().to_string().chars() {
        s.key(&c.to_string());
    }
    s.key("/");
    assert!(s.pump(|s| s.float_named("[Add project").iter().any(|l| l.ends_with("alpha/"))));

    s.special("tab");
    assert!(
        s.pump(|s| {
            s.float_named("[Add project").iter().any(|l| l.starts_with("> ") && l.ends_with("alpha/"))
        }),
        "the field now holds the completed path\n{:?}",
        s.float_named("[Add project")
    );
}

#[test]
fn a_path_you_typed_in_full_is_accepted_even_though_it_was_never_offered() {
    // A completion list that refuses a path it did not think of is a list that gets in the way.
    // The last segment is typed but not completed, so no row matches it.
    let sb = Sandbox::new("pathfree");
    let target = sb.work().join("chosen");
    std::fs::create_dir_all(&target).expect("mkdir");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.send(&command("project.open"));
    // The float, not the sidebar's `+ Add project` row — which is on screen from the start and
    // would let the test type into the composer instead.
    assert!(s.pump(|s| !s.float_named("[Add project").is_empty()), "the field opened");
    for c in target.display().to_string().chars() {
        s.key(&c.to_string());
    }
    s.enter();

    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("chosen"))),
        "the project was added\n{:?}",
        s.sidebar_now()
    );
}

/// The key sheet is the longest panel in the workspace, so it is one you move around in.
///
/// It used to close on *any* key, which on a screen shorter than the list meant the only thing you
/// could do with the part you could not see was dismiss it. Now the scroll keys resolve at
/// `neosh.scroll` and never reach its capture, and what closes it is named: `<Esc>`, `q`, `<CR>`,
/// `^C`, and `^Z` again.
#[test]
fn the_key_list_scrolls_and_closes_on_a_key_that_means_close() {
    let sb = Sandbox::new("keylist");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.enter_panel();
    s.key("?");
    // The panel's section is named after its buffer kind and is generated from the live registry —
    // it used to be a hand-written list in the palette, which is exactly the drift this replaced.
    // It comes first because `?` was pressed *in* the panel.
    s.wait_for("neosh.sidebar");
    let keys = s.buffer_named("[keys]").expect("key list buffer");
    assert!(
        s.pump(|s| {
            let lines = s.lines_of(keys);
            let first = lines.iter().position(|l| l.contains("neosh.sidebar"));
            let chat = lines.iter().position(|l| l.trim() == "CHAT");
            matches!((first, chat), (Some(f), Some(c)) if f < c)
        }),
        "the panel you are standing in is listed before the global bindings\n{:?}",
        s.lines_of(keys)
    );
    assert!(
        s.lines_of(keys).iter().any(|l| l.contains("Archive this conversation")),
        "and its verbs are there, described by the command that owns them\n{:?}",
        s.lines_of(keys)
    );
    // The text is written before the float is opened, so seeing the text is not seeing the window.
    assert!(s.pump(|s| s.windows_for(keys).len() == 1), "it is on screen\n{}", s.transcript());

    // A scroll is counted in the rows the frontend says it drew, and a stdio frontend says nothing
    // unless a test does — so a window nobody has laid out is one this deliberately leaves alone.
    s.viewport("[keys]", 64, 10);

    // A letter that is not a way out leaves it exactly where it was. `j` scrolls it — the binding
    // is at `neosh.scroll`, which is nearer than the capture — and `z` does nothing at all, which
    // is the whole point: a stray keystroke must not throw away where you had scrolled to.
    let win = s.windows_for(keys)[0];
    s.key("j");
    assert!(
        s.pump(|s| s.events.iter().any(|e| e["type"] == "scroll_to"
            && e["win"].as_u64() == Some(win)
            && e["top_line"].as_u64() == Some(1))),
        "`j` scrolls the sheet\n{}",
        s.transcript()
    );
    s.key("z");
    assert!(!s.windows_for(keys).is_empty(), "a stray letter does not close it\n{}", s.transcript());

    s.key("q");
    assert!(s.pump(|s| s.windows_for(keys).is_empty()), "`q` closes it\n{}", s.transcript());
}

/// The key list is reachable from the composer, and by a chord rather than by `F1`. Apple's top
/// row is brightness until somebody finds the setting, so the key whose job is teaching every
/// other key was the one key a new Mac could not press.
#[test]
fn the_key_list_opens_on_a_chord_from_the_composer() {
    let sb = Sandbox::new("keylistchord");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.ctrl("z");
    s.wait_for("CHAT");
    let keys = s.buffer_named("[keys]").expect("key list buffer");
    assert!(
        s.pump(|s| s.lines_of(keys).iter().any(|l| l.contains("Command palette"))),
        "the live registry, not a written-down list\n{:?}",
        s.lines_of(keys)
    );
    assert!(s.pump(|s| s.windows_for(keys).len() == 1), "it is on screen\n{}", s.transcript());
}

#[test]
fn escape_leaves_the_thread_list_and_typing_goes_back_to_the_composer() {
    let sb = Sandbox::new("leave");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.enter_panel();
    s.special("esc");
    // Leaving is a round trip to the plugin thread as well; the hint strip going back to its
    // out-of-panel form is the signal that the keyboard has been handed back.
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("^F archive"))),
        "the panel gave the keyboard back\n{:?}",
        s.sidebar_now()
    );
    // A key that would move the list instead lands in the composer.
    s.key("z");
    assert!(
        s.pump(|s| s.composer_now().iter().any(|l| l.contains('z'))),
        "typing resumed in the composer\n{:?}",
        s.composer_now()
    );
}

impl Session {
    /// The composer buffer's current contents.
    fn composer_now(&self) -> Vec<String> {
        let buf = match self.buffer_named("[composer]") {
            Some(b) => b,
            None => return Vec::new(),
        };
        let mut lines: Vec<String> = Vec::new();
        for e in &self.events {
            if e["type"] != "buffer_lines" || e["buf"].as_u64() != Some(buf) {
                continue;
            }
            let start = e["start"].as_i64().unwrap_or(0);
            let old_end = e["old_end"].as_i64().unwrap_or(start);
            let new: Vec<String> = e["lines"]
                .as_array()
                .map(|a| a.iter().filter_map(|l| l["text"].as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let s = start.clamp(0, lines.len() as i64) as usize;
            let en =
                if old_end < 0 { lines.len() } else { (old_end as usize).clamp(s, lines.len()) };
            lines.splice(s..en, new);
        }
        lines
    }

    fn chat_now(&self) -> Vec<String> {
        let buf = self.events.iter().find_map(|e| {
            (e["type"] == "buffer_opened"
                && e["name"].as_str().is_some_and(|n| n.starts_with("[chat]")))
            .then(|| e["buf"].as_u64())?
        });
        let Some(buf) = buf else { return Vec::new() };
        let mut lines: Vec<String> = Vec::new();
        for e in &self.events {
            if e["type"] != "buffer_lines" || e["buf"].as_u64() != Some(buf) {
                continue;
            }
            let start = e["start"].as_i64().unwrap_or(0);
            let old_end = e["old_end"].as_i64().unwrap_or(start);
            let new: Vec<String> = e["lines"]
                .as_array()
                .map(|a| a.iter().filter_map(|l| l["text"].as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let s = start.clamp(0, lines.len() as i64) as usize;
            let en =
                if old_end < 0 { lines.len() } else { (old_end as usize).clamp(s, lines.len()) };
            lines.splice(s..en, new);
        }
        lines
    }
}

#[test]
fn motion_can_be_turned_off_without_turning_off_the_sidebar() {
    let sb = Sandbox::new("nomotion");
    sb.write_config("[options]\n\"ui.motion\" = false\n\"ui.ascii_only\" = true\n");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.drain_for(Duration::from_secs(2));
    // No braille anywhere: with ascii_only the glyph set is plain, and with motion off nothing
    // cycles at all.
    let panel = s.sidebar_now().join("\n");
    assert!(
        !panel.contains('⠋') && !panel.contains('⠙'),
        "no spinner frames with motion off\n{panel}"
    );
    assert!(
        !panel.contains('\u{25be}') && !panel.contains('\u{25b8}'),
        "ascii_only means the fold arrows are ascii too\n{panel}"
    );
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

impl Session {
    /// Highlight groups the host has announced, by name.
    fn highlight_groups(&self) -> Vec<String> {
        self.events
            .iter()
            .filter(|e| e["type"] == "highlight_defined")
            .filter_map(|e| e["name"].as_str().map(str::to_string))
            .collect()
    }

    /// The most recent definition of a group.
    fn highlight(&self, name: &str) -> Option<&Value> {
        self.events
            .iter()
            .rev()
            .find(|e| e["type"] == "highlight_defined" && e["name"] == name)
            .map(|e| &e["def"])
    }
}

#[test]
fn every_group_a_widget_links_to_is_announced() {
    // The bug this guards: `Picker.Selected` used to link to `CursorLine`, which nothing defined,
    // so the chain dead-ended and the selected row in every picker rendered plain. A dead link does
    // not error — it just quietly has no colour.
    let sb = Sandbox::new("palette");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    let groups = s.highlight_groups();
    for required in [
        "Normal",
        "Comment",
        "Title",
        "CursorLine",
        "Search",
        "Picker.Selected",
        "Picker.Match",
        "Sidebar.Heading",
        "Status.Working",
        "Meter.Fill",
        "Git.Added",
        "Diff.Add",
        "Agent.Thinking",
    ] {
        assert!(groups.iter().any(|g| g == required), "{required} was never defined; have {groups:?}");
    }
}

#[test]
fn the_selected_row_of_a_picker_has_a_visible_highlight() {
    let sb = Sandbox::new("selection");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("p");
    s.wait_for("Mock");

    // The mark is emitted with the text, so a row carrying `Picker.Selected` proves both that the
    // widget applied it and that the group exists to be applied.
    let marked = s.events.iter().any(|e| {
        e["type"] == "buffer_lines"
            && e["lines"].as_array().is_some_and(|lines| {
                lines.iter().any(|l| {
                    l["marks"].as_array().is_some_and(|marks| {
                        marks.iter().any(|m| m["hl_group"] == "Picker.Selected")
                    })
                })
            })
    });
    assert!(marked, "the cursor row is highlighted\n{}", s.transcript());
    assert!(
        s.highlight("Picker.Selected").is_some(),
        "and the group resolves rather than dead-ending"
    );
}

#[test]
fn switching_theme_re_announces_the_groups() {
    // Highlights are pushed, not pulled: a frontend holds a copy and cannot ask for a new one, so a
    // theme switch that did not re-emit would change nothing on screen.
    let sb = Sandbox::new("theme");
    sb.write_config("[options]\n\"ui.theme\" = \"light\"\n");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    let dark = Sandbox::new("theme-dark");
    let mut d = dark.start();
    d.wait_for("PROJECTS");

    assert_ne!(
        s.highlight("Comment"),
        d.highlight("Comment"),
        "the light theme is not the dark one under another name"
    );
}

#[test]
fn an_unknown_theme_is_rejected_by_the_option_registry() {
    let sb = Sandbox::new("badtheme");
    sb.write_config("[options]\n\"ui.theme\" = \"solarized\"\n");
    let mut s = sb.start();
    // Typed options make this an error naming the problem, not a silent fallback.
    s.wait_for("ui.theme");
    assert!(s.saw("solarized"), "the message names the value\n{}", s.transcript());
}

// ---------------------------------------------------------------------------
// Diffs
// ---------------------------------------------------------------------------

#[test]
fn the_diff_view_shows_a_files_patch() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("diff");
    sb.git_init();
    std::fs::write(sb.work().join("README.md"), "hello\nsecond line\n").expect("write");

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("d");
    s.wait_for("Changed files");
    s.enter();
    s.wait_for("+second line");
    assert!(s.saw("── unstaged ──"), "labelled by which half it came from\n{}", s.transcript());
}

#[test]
fn an_untracked_file_says_so_rather_than_showing_an_empty_diff() {
    if !have_git() {
        return;
    }
    // A blank pane would look like a bug. There is genuinely nothing to diff against.
    let sb = Sandbox::new("untracked");
    sb.git_init();
    std::fs::write(sb.work().join("brand-new.txt"), "x\n").expect("write");

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("d");
    // The picker's title, not the filename: the sidebar already lists the file, so waiting on that
    // would send Enter into the composer before anything opened.
    s.wait_for("Changed files");
    s.enter();
    s.wait_for("is untracked");
}

// ---------------------------------------------------------------------------
// Entering an API key
// ---------------------------------------------------------------------------

/// The literal a test types in. Distinctive so a search for it means something.
const FAKE_KEY: &str = "sk-neosh-test-DO-NOT-LOG-8f3a";

/// A configured instance that wants a key.
///
/// The mock provider needs none, which is exactly right for every other test here and no use at
/// all for this one. The variable is never set, so this instance starts out with nothing.
const NEEDS_A_KEY: &str = r#"
[[providers]]
id = "acme"
driver = "openai-compat"
display_name = "Acme"
base_url = "https://acme.invalid/v1"
auth = { kind = "env", var = "NEOSH_TEST_ACME_KEY_NEVER_SET" }
"#;

#[test]
fn a_key_typed_in_never_reaches_the_frontend() {
    // The load-bearing test for the whole credential path. The host reads these keystrokes itself
    // precisely so the value never enters a buffer — because a buffer is drawn, and drawing it
    // would put the key in a `UiEvent`, across a process boundary, into whatever the frontend logs.
    let sb = Sandbox::new("secretwire");
    sb.write_config(NEEDS_A_KEY);
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.send(&command_with("provider.key", "acme"));
    s.wait_for("Paste or type the key");

    for c in FAKE_KEY.chars() {
        s.key(&c.to_string());
    }
    assert!(
        s.pump(|s| s.saw("\u{2022}\u{2022}\u{2022}")),
        "the field shows bullets, so a paste that did not arrive is visible\n{}",
        s.transcript()
    );
    s.enter();
    // Something happened — either it was stored, or the keychain was unavailable and it is held for
    // the run. Both are fine here; what is not fine is the value being anywhere in the JSON.
    assert!(s.pump(|s| s.saw("key saved") || s.saw("kept for this run")), "{}", s.transcript());

    assert!(
        !s.wire().contains(FAKE_KEY),
        "the key reached the frontend, which is the one thing this design exists to prevent"
    );
    assert!(!s.wire().contains("sk-neosh-test"), "not even a prefix of it");
}

#[test]
fn the_key_prompt_gives_the_composer_back_with_the_draft_still_in_it() {
    // It borrows the composer. Borrowing is only acceptable if it gives it back — losing an unsent
    // question because you stopped to paste a key would be its own reason never to stop.
    let sb = Sandbox::new("secretdraft");
    sb.write_config(NEEDS_A_KEY);
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    for c in ["h", "a", "l", "f"] {
        s.key(c);
    }
    assert!(s.pump(|s| s.saw("half")), "{}", s.transcript());

    s.send(&command_with("provider.key", "acme"));
    s.wait_for("Paste or type the key");
    for c in ["a", "b", "c"] {
        s.key(c);
    }
    s.send(r#"{"type":"key","key":{"code":{"kind":"esc"},"mods":{}}}"#);

    assert!(
        s.pump(|s| s.texts().iter().rev().take(4).any(|l| l == "half")),
        "the draft came back\n{}",
        s.transcript()
    );
    assert!(!s.wire().contains("abc"), "and what was typed into the prompt went nowhere");
}

#[test]
fn while_a_key_is_being_typed_no_binding_fires() {
    // Every base64 key contains an `n`, and `<C-n>` is a new conversation. Routing these keys
    // through the keymaps would mean pasting a key sometimes started a conversation halfway
    // through — which is also how half a key ends up stored.
    let sb = Sandbox::new("secretkeys");
    sb.write_config(NEEDS_A_KEY);
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    let before = s.sidebar_now().iter().filter(|l| l.contains("New conversation")).count();

    s.send(&command_with("provider.key", "acme"));
    s.wait_for("Paste or type the key");
    s.ctrl("n");
    s.ctrl("t");
    s.key("n");
    assert!(s.pump(|s| s.saw("\u{2022}")), "the `n` went into the key\n{}", s.transcript());

    s.send(r#"{"type":"key","key":{"code":{"kind":"esc"},"mods":{}}}"#);
    s.drain_for(Duration::from_millis(200));
    assert_eq!(
        s.sidebar_now().iter().filter(|l| l.contains("New conversation")).count(),
        before,
        "no conversation was created\n{:?}",
        s.sidebar_now()
    );
}

// ---------------------------------------------------------------------------
// The transcript
// ---------------------------------------------------------------------------

/// A driver that starts a turn and never finishes it, so the working line is reliably on screen.
const STALLS: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("stall", [{
    id: "stall", driver: "stall", display_name: "Stall",
    models: [{ id: "stall", display_name: "Stall" }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "stall", usage: {} });
    // Nothing else, ever. The turn stays in flight.
  });
  neosh.event.on("neosh.ready", () => neosh.notify("stall ready"));
}
"#;

/// A provider that reports it is waiting and then never finishes — the shape of a vendor CLI sitting
/// in retry backoff after a 529.
///
/// The reason this needs a test at all is that the failure mode is *nothing on screen*: several
/// minutes with no tokens, no card and no card body, which is indistinguishable from a wedged
/// process and is what sends people to `^C` on a turn that was about to come back.
const WAITER: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("waiter", [{
    id: "waiter", driver: "waiter", display_name: "Waiter",
    models: [{ id: "waiter", display_name: "Waiter" }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "waiter", usage: {} });
    emit({ type: "activity", activity: {
      kind: "waiting",
      what: "Retrying after 529 · attempt 2 of 5",
      retry_in_ms: 4200,
    } });
    // And then nothing. The wait is the whole turn.
  });
  neosh.event.on("neosh.ready", () => neosh.notify("waiter ready"));
}
"#;

fn install_waiter(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/waiter");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"waiter\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), WAITER).expect("plugin");
}

#[test]
fn a_turn_that_is_waiting_says_what_it_is_waiting_for() {
    // "Working… 4m" and "Retrying after 529 · attempt 2 of 5 · in 5s … 4m" are the same turn. Only
    // one of them is a reason to keep waiting rather than to interrupt.
    let sb = Sandbox::new("waiting");
    install_waiter(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"waiter/waiter\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("waiter ready");

    s.type_text("go");
    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("Retrying after 529")
            && l.contains("attempt 2 of 5")
            && l.contains("esc to interrupt"))),
        "the working line says why nothing is happening\n{:?}",
        s.chat_now()
    );
    // Rounded up rather than truncated: a wait reported as `in 0s` reads as a lie.
    assert!(
        s.chat_now().iter().any(|l| l.contains("in 5s")),
        "and how long it expects to be\n{:?}",
        s.chat_now()
    );
}

/// A provider that backgrounds something, answers, and ends — the shape of `run_in_background`.
const LEAVER: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("leaver", [{
    id: "leaver", driver: "leaver", display_name: "Leaver",
    models: [{ id: "leaver", display_name: "Leaver" }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "leaver", usage: {} });
    emit({ type: "activity", activity: { kind: "background", tasks: [
      { id: "bg1", title: "npm run build", kind: "local_bash" },
    ] } });
    emit({ type: "block_start", index: 0, block: { kind: "text" } });
    emit({ type: "text_delta", index: 0, text: "Started the build." });
    emit({ type: "block_stop", index: 0 });
    emit({ type: "message_delta", stop_reason: { kind: "end_turn" }, usage: {} });
    emit({ type: "message_stop" });
  });
  neosh.event.on("neosh.ready", () => neosh.notify("leaver ready"));
}
"#;

fn install_leaver(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/leaver");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"leaver\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), LEAVER).expect("plugin");
}

#[test]
fn a_finished_turn_says_what_it_walked_away_from() {
    // The turn ends and the footer listing what was out goes away with the working line. Without
    // this the one case people actually ask about — "it says it is done, is it?" — is the one that
    // leaves no trace at all.
    let sb = Sandbox::new("leftrunning");
    install_leaver(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"leaver/leaver\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("leaver ready");

    s.type_text("build it");
    s.enter();
    s.wait_for("Started the build.");
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("Still running in the background"))),
        "the answer does not get to be the last word\n{:?}",
        s.chat_now()
    );
    assert!(
        s.chat_now().iter().any(|l| l.contains("npm run build") && l.contains("local_bash")),
        "and it says what, in the driver's own words for the kind of thing it is\n{:?}",
        s.chat_now()
    );
    // The conversation is what carries this, not the turn — so the row in the panel knows too, and
    // knows it after the turn that started it is over.
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("\u{25cb} build it"))),
        "the panel marks a conversation that is still doing something\n{:?}",
        s.sidebar_now()
    );
}

/// A provider that says something and then never finishes — the shape of a real turn that has
/// written a sentence and gone off to run tools.
const HALFWAY: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("half", [{
    id: "half", driver: "half", display_name: "Half",
    models: [{ id: "half", display_name: "Half" }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "half", usage: {} });
    emit({ type: "block_start", index: 0, block: { kind: "text" } });
    emit({ type: "text_delta", index: 0, text: "Here is what I found so far." });
    // And then nothing, ever. The turn stays in flight with an answer already on screen.
  });
  neosh.event.on("neosh.ready", () => neosh.notify("half ready"));
}
"#;

/// A tool that succeeds with several lines, and a model that calls it once.
///
/// The built-in fixture's tools all fail, and a failure is the one case that ignores the limit —
/// so proving the limit does anything needs a call that works.
const CHATTY: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.tool.register(
    { name: "count", description: "Count", inputSchema: { type: "object" } },
    async () => ({ content: "one\ntwo\nthree\nfour\nfive" }),
  );
  let asked = false;
  await neosh.provider.register("chatty", [{
    id: "chatty", driver: "chatty", display_name: "Chatty",
    models: [{ id: "chatty", display_name: "Chatty" }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "chatty", usage: {} });
    if (!asked) {
      asked = true;
      emit({ type: "block_start", index: 0, block: { kind: "tool_use", id: "c1", name: "count" } });
      emit({ type: "tool_input_delta", index: 0, partial_json: "{}" });
      emit({ type: "block_stop", index: 0 });
      emit({ type: "message_delta", stop_reason: { kind: "tool_use" }, usage: {} });
      emit({ type: "message_stop" });
      return;
    }
    emit({ type: "block_start", index: 0, block: { kind: "text" } });
    emit({ type: "text_delta", index: 0, text: "Counted." });
    emit({ type: "block_stop", index: 0 });
    emit({ type: "message_delta", stop_reason: { kind: "end_turn" }, usage: {} });
    emit({ type: "message_stop" });
  });
  neosh.event.on("neosh.ready", () => neosh.notify("chatty ready"));
}
"#;

/// A tool whose call is an edit — `old_string`/`new_string` at a path — and a model that makes one.
///
/// What the card shows for an edit is read off those arguments, so this is the shape that proves
/// the diff, the `+N -N`, and the summary at the end of the turn.
const EDITOR: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.tool.register(
    { name: "edit", description: "Edit", inputSchema: { type: "object" } },
    async () => ({ content: "edited" }),
  );
  let asked = false;
  await neosh.provider.register("editor", [{
    id: "editor", driver: "editor", display_name: "Editor",
    models: [{ id: "editor", display_name: "Editor" }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "editor", usage: {} });
    if (!asked) {
      asked = true;
      const input = {
        file_path: "/work/src/main.rs",
        old_string: "a\nb\nc\nd\ne\nf\ng\nh",
        new_string: "a\nb\nc\nd\nE\nf\ng\nh\ni",
      };
      emit({ type: "block_start", index: 0, block: { kind: "tool_use", id: "e1", name: "edit" } });
      emit({ type: "tool_input_delta", index: 0, partial_json: JSON.stringify(input) });
      emit({ type: "block_stop", index: 0 });
      emit({ type: "message_delta", stop_reason: { kind: "tool_use" }, usage: {} });
      emit({ type: "message_stop" });
      return;
    }
    emit({ type: "block_start", index: 0, block: { kind: "text" } });
    emit({ type: "text_delta", index: 0, text: "Edited." });
    emit({ type: "block_stop", index: 0 });
    emit({ type: "message_delta", stop_reason: { kind: "end_turn" }, usage: {} });
    emit({ type: "message_stop" });
  });
  neosh.event.on("neosh.ready", () => neosh.notify("editor ready"));
}
"#;

const LOOKER: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  const lines = (n: number) => Array.from({ length: n }, (_, i) => `line ${i + 1}`).join("\n");
  await neosh.tool.register(
    { name: "look", description: "Read", inputSchema: { type: "object" } },
    async () => ({ content: lines(9) }),
  );
  await neosh.tool.register(
    { name: "hunt", description: "Grep", inputSchema: { type: "object" } },
    async () => ({ content: lines(4) }),
  );
  await neosh.tool.register(
    { name: "edit", description: "Edit", inputSchema: { type: "object" } },
    async () => ({ content: "edited" }),
  );
  let turn = 0;
  await neosh.provider.register("looker", [{
    id: "looker", driver: "looker", display_name: "Looker",
    models: [{ id: "looker", display_name: "Looker" }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "looker", usage: {} });
    const call = (i: number, name: string, input: unknown) => {
      emit({ type: "block_start", index: i, block: { kind: "tool_use", id: `t${turn}_${i}`, name } });
      emit({ type: "tool_input_delta", index: i, partial_json: JSON.stringify(input) });
      emit({ type: "block_stop", index: i });
    };
    turn += 1;
    if (turn === 1) {
      call(0, "look", { file_path: "/work/src/one.rs" });
      call(1, "look", { file_path: "/work/src/two.rs" });
      call(2, "hunt", { pattern: "fn main", path: "/work/src" });
      call(3, "edit", { file_path: "/work/src/one.rs", old_string: "a", new_string: "b" });
      emit({ type: "message_delta", stop_reason: { kind: "tool_use" }, usage: {} });
      emit({ type: "message_stop" });
      return;
    }
    emit({ type: "block_start", index: 0, block: { kind: "text" } });
    emit({ type: "text_delta", index: 0, text: "Looked." });
    emit({ type: "block_stop", index: 0 });
    emit({ type: "message_delta", stop_reason: { kind: "end_turn" }, usage: {} });
    emit({ type: "message_stop" });
  });
  neosh.event.on("neosh.ready", () => neosh.notify("looker ready"));
}
"#;

fn install_looker(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/looker");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"looker\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\", \"tools\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), LOOKER).expect("plugin");
}

fn install_editor(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/editor");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"editor\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\", \"tools\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), EDITOR).expect("plugin");
}

fn install_chatty(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/chatty");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"chatty\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\", \"tools\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), CHATTY).expect("plugin");
}

fn install_halfway(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/half");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"half\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), HALFWAY).expect("plugin");
}

fn install_stall(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/stall");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"stall\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), STALLS).expect("plugin");
}

#[test]
fn a_question_is_framed_so_you_can_find_where_a_turn_began() {
    // A bar down the left rather than a `>` prefix: the frontend wraps long lines, a prefix
    // survives only on the first of them, and what you scan for when scrolling back is exactly
    // where the turns start.
    let sb = Sandbox::new("framing");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    for c in ["h", "i"] {
        s.key(c);
    }
    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l == "\u{258c} hi")),
        "the question is framed\n{:?}",
        s.chat_now()
    );
}

#[test]
fn a_tool_call_says_what_it_is_about_not_only_its_name() {
    // The tool's name tells you nothing you did not already know. `Read  .env` is the line you
    // actually wanted, and both halves of it cost one lookup in the input the tool was given.
    let sb = Sandbox::new("toolrow");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.key("h");
    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("Read") && l.contains(".env"))),
        "the subject is beside what was done\n{:?}",
        s.chat_now()
    );
}

#[test]
fn a_turn_in_flight_says_so_and_says_for_how_long() {
    // The gap between pressing Enter and the first token is the longest silence in the program,
    // and a still screen and a wedged process look identical. The clock is the difference.
    let sb = Sandbox::new("working");
    install_stall(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"stall/stall\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("stall ready");

    for c in ["h", "a", "n", "g"] {
        s.key(c);
    }
    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains('\u{2026}') && l.contains("esc to interrupt"))),
        "the working line is up\n{:?}",
        s.chat_now()
    );
    // And it is the last line: it says what is happening *now*, so anything already reported
    // belongs above it.
    let rows = s.chat_now();
    let last = rows.iter().rposition(|l| !l.trim().is_empty()).unwrap_or(0);
    assert!(rows[last].contains("esc to interrupt"), "pinned to the bottom\n{rows:?}");
}

#[test]
fn the_working_line_is_gone_the_moment_there_is_an_answer() {
    // Leaving it above the reply would say a turn is in flight while you are reading its result.
    let sb = Sandbox::new("workingends");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.key("h");
    s.enter();
    s.wait_for("All done.");
    s.drain_for(Duration::from_millis(300));
    assert!(
        !s.chat_now().iter().any(|l| l.contains("esc to interrupt")),
        "no working line survives the turn\n{:?}",
        s.chat_now()
    );
}


// ---------------------------------------------------------------------------
// The composer: what it says, and what the keys under it do
// ---------------------------------------------------------------------------

impl Session {
    /// Everything drawn *around* the composer that is not in the composer: the prompt, the
    /// placeholder, the rule, the shortcut row. All of it is virtual text, so none of it can be
    /// read back out of the buffer — which is the point, since the buffer's contents get sent.
    fn composer_chrome(&self) -> Vec<String> {
        let buf = self
            .events
            .iter()
            .find_map(|e| {
                (e["type"] == "buffer_opened" && e["name"] == "[composer]")
                    .then(|| e["buf"].as_u64())?
            })
            .unwrap_or(u64::MAX);
        let mut latest: Vec<String> = Vec::new();
        for e in &self.events {
            if e["type"] != "buffer_lines" || e["buf"].as_u64() != Some(buf) {
                continue;
            }
            let mut found = Vec::new();
            for line in e["lines"].as_array().into_iter().flatten() {
                for m in line["marks"].as_array().into_iter().flatten() {
                    let text: String = m["virt_text"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|c| c["text"].as_str())
                        .collect();
                    if !text.trim().is_empty() {
                        found.push(text);
                    }
                }
            }
            if !found.is_empty() {
                latest = found;
            }
        }
        latest
    }
}

#[test]
fn the_shortcut_row_is_off_because_the_sidebar_already_says_all_of_it() {
    // It read as a good idea and was not. `^T`, `^N` and `^K` are in the sidebar's own footer two
    // rows below, `^Z` is on the row it points at, and what the duplication actually bought was
    // one fewer line of transcript and a composer pressed against the status strip.
    let sb = Sandbox::new("nohintstrip");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("^K palette"))),
        "the sidebar carries the keys\n{:?}",
        s.sidebar_now()
    );
    assert!(
        !s.composer_chrome().join(" ").contains("palette"),
        "and the composer does not say them again\n{:?}",
        s.composer_chrome()
    );
}

#[test]
fn the_shortcut_row_comes_back_if_you_ask_for_it() {
    let sb = Sandbox::new("hintstrip");
    sb.write_config("[options]\n\"ui.hints\" = true\n");
    let mut s = sb.start_letting_config_choose();
    // Waiting on a plugin's entry, not the host's: the host seeds `⏎ send` before any plugin has
    // loaded, so asserting on that alone would pass before the row was finished.
    assert!(
        s.pump(|s| s.composer_chrome().iter().any(|t| t.contains("^Z keys"))),
        "the way to the keys that did not fit\n{:?}",
        s.composer_chrome()
    );
    let chrome = s.composer_chrome().join(" ");
    assert!(chrome.contains("send"), "and the one key everybody needs: {chrome}");
}

/// A provider whose model says how big its window is, and which reports having filled a third of
/// it. Enough for the meter to be a real number rather than a shape.
const WIDE: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("wide", [{
    id: "wide", driver: "wide", display_name: "Wide",
    models: [{ id: "wide-1", display_name: "Wide One", context_window: 300000 }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "wide-1", usage: { input_tokens: 100000 } });
    emit({ type: "text_delta", index: 0, text: "ok" });
    emit({
      type: "message_delta",
      stop_reason: { kind: "end_turn" },
      usage: { input_tokens: 100000, output_tokens: 20 },
    });
    emit({ type: "message_stop" });
  });
  neosh.event.on("neosh.ready", () => neosh.notify("wide ready"));
}
"#;

/// A driver that compacts without ever having said how big its window is.
///
/// Which is most of them, and `claude` whenever the control question goes unanswered: a boundary
/// carries `post_tokens` and no denominator. The turn then ends carrying the usage of the request
/// that did the compacting — the whole pre-compaction conversation — and that is what used to land
/// on the meter.
const AMNESIAC: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("amnesiac", [{
    id: "amnesiac", driver: "amnesiac", display_name: "Amnesiac",
    models: [{ id: "amnesiac", display_name: "Amnesiac", context_window: 300000 }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "amnesiac", usage: {} });
    emit({ type: "activity", activity: { kind: "compacted", before: 180000, after: 12000 } });
    emit({ type: "block_start", index: 0, block: { kind: "text" } });
    emit({ type: "text_delta", index: 0, text: "Carrying on." });
    emit({ type: "block_stop", index: 0 });
    emit({
      type: "message_delta",
      stop_reason: { kind: "end_turn" },
      usage: { input_tokens: 180000, output_tokens: 20 },
    });
    emit({ type: "message_stop" });
  }, { agentLoop: true });
  neosh.event.on("neosh.ready", () => neosh.notify("amnesiac ready"));
}
"#;

fn install_amnesiac(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/amnesiac");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"amnesiac\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), AMNESIAC).expect("plugin");
}

/// A count from a driver turns the guess off, even when it comes without a window.
///
/// "A driver has answered how full it is" and "the window is known" were one field, and a
/// compaction boundary is the case that tells them apart: it is the first without being the
/// second. Read as *nobody has told us*, it turned `add_usage`'s estimate back on — so the turn
/// ended, the prompt that did the compacting was counted, and the meter went back to the number
/// the compaction had just removed. See ADR 0050, which is the same failure through another door.
#[test]
fn a_compaction_without_a_window_still_stops_the_meter_being_guessed() {
    let sb = Sandbox::new("ctxamnesia");
    install_amnesiac(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"amnesiac/amnesiac\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("amnesiac ready");
    s.type_text("go");
    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("Carrying on."))),
        "the turn finished\n{:?}",
        s.chat_now()
    );
    // Past the turn's end, which is where the estimate used to be applied. The failure is not that
    // the wrong number is drawn once; it is that it settles there.
    s.drain_for(Duration::from_millis(2000));
    let line = s.status_now().join("");
    assert!(
        line.contains("4% of 300k") && !line.contains("60%"),
        "and the meter keeps what the compaction left, not the prompt that did it\n{line:?}"
    );
}

/// A driver that fills its context and then compacts it, reporting both the way `claude` does.
///
/// `Activity::Context` is the driver answering "how full is it"; `Activity::Compacted` carries the
/// count either side of a compaction. The second one is the whole subject of the test: it is the
/// only moment the number moves without a request being made.
const COMPACTOR: &str = r#"
import type { PluginContext } from "@neosh/api";
const nap = (ms: number) => new Promise((r) => setTimeout(r, ms));

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("compactor", [{
    id: "compactor", driver: "compactor", display_name: "Compactor",
    models: [{ id: "compactor", display_name: "Compactor" }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "compactor", usage: {} });
    emit({ type: "activity", activity: { kind: "context", used: 180000, total: 200000 } });
    await nap(400);
    emit({ type: "activity", activity: { kind: "compacted", before: 180000, after: 12000 } });
    emit({ type: "block_start", index: 0, block: { kind: "text" } });
    emit({ type: "text_delta", index: 0, text: "Carrying on." });
    emit({ type: "block_stop", index: 0 });
    emit({ type: "message_delta", stop_reason: { kind: "end_turn" }, usage: {} });
    emit({ type: "message_stop" });
  }, { agentLoop: true });
  neosh.event.on("neosh.ready", () => neosh.notify("compactor ready"));
}
"#;

fn install_compactor(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/compactor");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"compactor\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), COMPACTOR).expect("plugin");
}

fn install_wide(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/wide");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"wide\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), WIDE).expect("plugin");
}

/// How full the window is, drawn as a bar and said as a percentage — and drawn *before* anything
/// has been spent.
///
/// It used to appear only once a turn had gone through, so the one moment you could not see how
/// much room a model has was before deciding what to do with it. A 200k window and a 1M window are
/// different tools and the difference matters most at the start.
#[test]
fn the_context_meter_is_there_before_the_first_turn() {
    let sb = Sandbox::new("ctxmeter");
    install_wide(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"wide/wide-1\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("wide ready");

    assert!(
        s.pump(|s| s.status_now().iter().any(|l| l.contains("of 300k"))),
        "the window is on the strip before a single token has been spent\n{:?}",
        s.status_now()
    );
    assert!(
        s.status_now().iter().any(|l| l.contains("0% of 300k")),
        "and it reads empty rather than being absent\n{:?}",
        s.status_now()
    );
    assert!(
        s.status_now().iter().any(|l| l.contains('\u{2591}')),
        "as a bar, not only as a number\n{:?}",
        s.status_now()
    );

    s.type_text("go");
    s.enter();
    assert!(
        s.pump(|s| s.status_now().iter().any(|l| l.contains("33% of 300k"))),
        "and it fills\n{:?}",
        s.status_now()
    );
    assert!(
        s.status_now().iter().any(|l| l.contains('\u{2588}')),
        "with filled cells\n{:?}",
        s.status_now()
    );
}

/// The same driver, holding the turn open, so the strip can be read while one is actually running.
///
/// The bug this exists for only happens mid-turn: a running turn puts a segment of its own on the
/// right-hand end, and a strip that fitted at rest is several columns too long the moment there is
/// something to watch.
const SLOW_WIDE: &str = r#"
import type { PluginContext } from "@neosh/api";
const nap = (ms: number) => new Promise((r) => setTimeout(r, ms));

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("wide", [{
    id: "wide", driver: "wide", display_name: "Wide",
    models: [{ id: "wide-1", display_name: "Wide One", context_window: 300000 }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "wide-1", usage: { input_tokens: 100000 } });
    emit({ type: "text_delta", index: 0, text: "ok" });
    await nap(12000);
    emit({
      type: "message_delta",
      stop_reason: { kind: "end_turn" },
      usage: { input_tokens: 100000, output_tokens: 20 },
    });
    emit({ type: "message_stop" });
  });
  neosh.event.on("neosh.ready", () => neosh.notify("slow wide ready"));
}
"#;

fn install_slow_wide(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/wide");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"wide\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), SLOW_WIDE).expect("plugin");
}

/// The meter says less rather than nothing, and it is the last thing on the strip to go.
///
/// Two bugs, one line. A segment used to cost its full width or nothing, so the widest thing in
/// the strip was the first thing to disappear from a narrow one — and the widest thing is the
/// context meter, which is also the only number here that says whether the conversation is about
/// to stop working. It sat on priority 20, tied with the git branch and separated from it only by
/// the fact that `context` sorts after `branch`, which made it the second segment out of the line.
/// And a running turn adds a segment, so a terminal wide enough at rest lost the meter for exactly
/// the length of every answer — the one time anybody is watching it.
///
/// Now it carries a short form of itself: the denominator comes off before the segment does, and
/// `██░░░░░░ 33%` is as true as `██░░░░░░ 33% of 300k`.
#[test]
fn a_narrow_strip_shortens_the_context_meter_rather_than_dropping_it() {
    let sb = Sandbox::new("ctxnarrow");
    install_slow_wide(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"wide/wide-1\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("slow wide ready");

    // A stdio frontend has no geometry, so the strip is unfitted until a width is said out loud —
    // which is the whole subject here.
    s.viewport("[status]", 50, 1);

    s.type_text("go");
    s.enter();
    assert!(
        s.pump(|s| s.status_now().iter().any(|l| l.contains("33%"))),
        "the meter is on a 50-column strip with a turn in flight\n{:?}",
        s.status_now()
    );
    assert!(
        !s.status_now().iter().any(|l| l.contains("of 300k")),
        "shortened rather than dropped — the denominator is what it gave up\n{:?}",
        s.status_now()
    );
    assert!(
        s.status_now().iter().any(|l| l.contains('\u{2588}') || l.contains('\u{2591}')),
        "and it is still a bar, not only a number\n{:?}",
        s.status_now()
    );

    // And the strip is fitted *here*, on the width arriving, rather than whenever a plugin next
    // happens to rewrite a segment. Widening used to leave the old, narrower line in place.
    s.viewport("[status]", 200, 1);
    assert!(
        s.pump(|s| s.status_now().iter().any(|l| l.contains("33% of 300k"))),
        "and a terminal made wider says the whole thing again\n{:?}",
        s.status_now()
    );
}

/// A plugin that writes down every draft it is told about.
///
/// Which is the whole of what a completion menu does before it decides whether to open — so this is
/// the contract, with nothing in the way of reading it.
const WATCHER: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  const buf = await neosh.buf.create({ name: "[drafts]", scratch: true });
  const seen: string[] = [];
  neosh.agent.onComposerChange(({ text }) => {
    seen.push(`draft<${text}>`);
    void neosh.buf.setLines(buf, 0, -1, seen);
  });
  neosh.event.on("neosh.ready", () => neosh.notify("watcher ready"));
}
"#;

// ---------------------------------------------------------------------------
// The slash menu
// ---------------------------------------------------------------------------

/// Put a lone `/` in the composer and wait for the menu it opens.
///
/// Retried, because plugins load asynchronously and nothing in the protocol announces "every
/// builtin is up" — a `/` typed a millisecond too early is a `/` in the composer and no menu.
/// Backspaced between attempts so the field is left holding exactly one slash whichever attempt
/// won, which is what the tests below then type into.
fn open_slash_menu(s: &mut Session) {
    for _ in 0..20 {
        s.type_text("/");
        s.drain_for(Duration::from_millis(150));
        if s.buffer_named("[Run]").is_some() {
            // The menu can open from a slash this loop had already given up on — the backspace
            // below races the very keystroke that opens it — which leaves the field holding *two*
            // slashes and the menu up. The tests after this then type into that, and the failure is
            // `//compact` in a test about what the composer says.
            //
            // So trim, rather than trusting the count. Bounded and drain-only: `pump` waits the
            // full 30s `BUDGET` when its predicate is false, and a predicate that can stay false —
            // which this one can, because once the menu is up a typed `/` goes to its filter and
            // never reaches the composer — turns twenty retries into ten minutes and then fails
            // anyway. That is exactly what the first attempt at this did to CI.
            for _ in 0..4 {
                if s.composer_now().iter().any(|l| l == "/") {
                    return;
                }
                s.special("backspace");
                s.drain_for(Duration::from_millis(50));
            }
            return;
        }
        s.special("backspace");
        s.drain_for(Duration::from_millis(50));
    }
    panic!("the slash menu never opened\n{}", s.transcript());
}

/// Typing `/compact` and pressing enter must send `/compact`.
///
/// It did not. The menu opened on the `/`, kept the keyboard, fuzzy-matched `compact` over every
/// command's *description* — six letters that occur in that order in a great deal of English — and
/// `↵` ran whichever unrelated command that put under the cursor. Meanwhile `/compact` itself was
/// not in the list, because a driver only reports its commands after a turn has run and this
/// conversation had not run one. So the one thing the user typed was the one thing that could not
/// happen, and what happened instead was silent.
#[test]
fn a_command_the_menu_has_never_heard_of_is_sent_to_the_agent_as_typed() {
    let sb = Sandbox::new("slashunknown");
    let mut s = sb.start();

    open_slash_menu(&mut s);

    s.type_text("compact");
    // The composer is the field: what was typed is in it, whatever the menu is doing.
    assert!(
        s.pump(|s| s.composer_now().iter().any(|l| l == "/compact")),
        "and the composer says what was typed\n{:?}",
        s.composer_now()
    );

    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("/compact"))),
        "and enter sends it\n{:?}",
        s.chat_now()
    );
    // The failure mode this replaces: something else ran instead. `chat.read` was the usual
    // winner, and it leaves the mode line saying so.
    assert!(
        !s.status_now().iter().any(|l| l.contains("reading")),
        "rather than running whatever happened to match\n{:?}",
        s.status_now()
    );
}

/// A command the *driver* named is one row of the same list, and choosing it sends it.
///
/// The two vocabularies differ in exactly one place — what accepting does. A neosh command runs and
/// the draft is cleared, because it was never a message. A driver command *is* a message and, if it
/// is finished being typed, it goes: asking for a second confirmation of a row you picked out of a
/// list is a keystroke that answers nothing.
#[test]
fn a_command_the_driver_named_is_offered_and_sent_as_a_message() {
    let sb = Sandbox::new("slashdriver");
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/driver_commands.jsonl");
    let mut s = sb.spawn(&script, Some("mock/mock"));

    // Nothing knows the driver's commands until a turn has run — the handshake is what says.
    s.type_text("hi");
    s.enter();
    assert!(s.pump(|s| s.chat_now().iter().any(|l| l.contains("Hello."))), "a turn ran");

    open_slash_menu(&mut s);
    s.type_text("comp");
    assert!(
        s.pump(|s| s.lines_of(s.buffer_named("[Run]").unwrap_or(u64::MAX))
            .iter()
            .any(|l| l.contains("Summarise the conversation so far"))),
        "the driver's own command, in the driver's own words\n{:?}",
        s.buffer_named("[Run]").map(|b| s.lines_of(b))
    );

    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("/compact"))),
        "and choosing it asks for it\n{:?}",
        s.chat_now()
    );
}

/// Escape leaves what was typed where it was being typed.
#[test]
fn dismissing_the_slash_menu_keeps_the_draft() {
    let sb = Sandbox::new("slashescape");
    let mut s = sb.start();
    open_slash_menu(&mut s);
    s.type_text("git");
    s.special("esc");
    assert!(
        s.pump(|s| s.composer_now().iter().any(|l| l == "/git")),
        "still there, mid-word\n{:?}",
        s.composer_now()
    );
}

/// A search borrows the composer, and what is in it then is not a draft.
///
/// `/` in the transcript types into the same field the composer uses, which is deliberate — a float
/// over the thing you are searching would hide the answer. The cost is that anything watching the
/// draft sees `/`, then `/n`, then `/ne`. For the slash-command menu, which opens on exactly a lone
/// `/`, that means opening a picker on top of the search the moment it starts and taking the
/// keyboard off it.
///
/// So the event is about the *draft*, and a borrowed field has none.
fn drafts(s: &Session) -> Vec<String> {
    s.buffer_named("[drafts]").map(|b| s.lines_of(b)).unwrap_or_default()
}

#[test]
fn a_search_is_not_broadcast_as_a_draft() {
    let sb = Sandbox::new("searchdraft");
    let dir = sb.root.join("config/plugins/watcher");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"watcher\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), WATCHER).expect("plugin");

    let mut s = sb.start();
    s.wait_for("watcher ready");

    // An ordinary draft is broadcast, or the event would be useless.
    s.type_text("hi");
    assert!(
        s.pump(|s| drafts(s).iter().any(|l| l.contains("draft<hi>"))),
        "typing in the composer is a draft\n{:?}",
        drafts(&s)
    );

    // Now the search. Every one of these keystrokes lands in the same buffer.
    s.ctrl("s");
    s.type_text("/neosh");
    assert!(
        s.pump(|s| s.composer_now().iter().any(|l| l.contains("/neosh"))),
        "the search is being typed\n{:?}",
        s.composer_now()
    );

    // Back to the composer, and one more character. Waiting for *that* to be recorded is what makes
    // the negative below mean anything: events reach a plugin in order, so a draft recorded after
    // the search proves every keystroke of the search was already handled — or dropped.
    s.special("esc");
    s.special("esc");
    s.type_text("z");
    assert!(
        s.pump(|s| drafts(s).iter().any(|l| l.contains("draft<hiz>"))),
        "the composer is the draft again\n{:?}",
        drafts(&s)
    );

    let seen = drafts(&s);
    assert!(
        !seen.iter().any(|l| l.contains("draft</")),
        "and not one keystroke of the search was announced as a draft\n{seen:?}"
    );
}

/// A driver that reports its own context occupancy while it works, the way a vendor CLI does.
///
/// The catalogue says this model has a 300k window. The driver says the window is 128k and that it
/// has 64k in it — which is the situation that matters, because an agent driver's window is
/// whatever *it* decided and it is the only thing that knows what it is still holding.
const REPORTER: &str = r#"
import type { PluginContext } from "@neosh/api";
const nap = (ms: number) => new Promise((r) => setTimeout(r, ms));

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("reporter", [{
    id: "reporter", driver: "reporter", display_name: "Reporter",
    models: [{ id: "reporter-1", display_name: "Reporter One", context_window: 300000 }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "reporter-1", usage: {} });
    // Mid-turn, before a single token of answer and long before the turn ends.
    emit({ type: "activity", activity: { kind: "context", used: 64000, total: 128000 } });
    await nap(900);
    emit({ type: "block_start", index: 0, block: { kind: "text" } });
    emit({ type: "text_delta", index: 0, text: "ok" });
    emit({ type: "block_stop", index: 0 });
    emit({ type: "message_delta", stop_reason: { kind: "end_turn" }, usage: {} });
    emit({ type: "message_stop" });
  }, { agentLoop: true });
  neosh.event.on("neosh.ready", () => neosh.notify("reporter ready"));
}
"#;

fn install_reporter(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/reporter");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"reporter\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), REPORTER).expect("plugin");
}

/// The meter believes the driver, and moves while the turn is still running.
///
/// Two faults, one symptom. The number came from the usage of the last *request*, and the window
/// came from neosh's catalogue — so for an agent driver, which holds the conversation on its own
/// side and prunes and compacts it without telling anybody, both halves were guesses. And it was
/// only ever recomputed when a turn *ended*, which for an agent driver is minutes: a fresh
/// conversation read `0%` and stayed there for the whole of its first turn, which is exactly when
/// somebody looks at it.
#[test]
fn the_context_meter_believes_the_driver_and_moves_while_it_works() {
    let sb = Sandbox::new("ctxreport");
    install_reporter(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"reporter/reporter-1\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("reporter ready");

    // Before the turn: nothing has been reported, so the catalogue is the best guess there is.
    assert!(
        s.pump(|s| s.status_now().iter().any(|l| l.contains("0% of 300k"))),
        "the catalogue is the fallback, not a lie\n{:?}",
        s.status_now()
    );

    s.type_text("go");
    s.enter();
    // 64k of 128k. Both halves are the driver's: the catalogue would have said 21% of 300k.
    assert!(
        s.pump(|s| s.status_now().iter().any(|l| l.contains("50% of 128k"))),
        "the driver's own numerator and its own denominator\n{:?}",
        s.status_now()
    );
    assert!(
        !s.status_now().iter().any(|l| l.contains("of 300k")),
        "and not the catalogue's window once it has been told\n{:?}",
        s.status_now()
    );
    // The driver sent that before it wrote a word of the answer, so seeing it here means the meter
    // did not wait for the turn to be over.
    assert!(
        !s.saw("ok"),
        "and it arrived before the answer did, not after the turn\n{}",
        s.transcript()
    );
}

/// One gauge, not two. The sidebar drew a context meter and so did the usage plugin; for as long
/// as no model in the catalogue reported a window, only one of them ever appeared. The moment one
/// did, the footer had two bars in it that disagreed about what "used" meant.
#[test]
fn there_is_exactly_one_context_meter() {
    let sb = Sandbox::new("onemeter");
    install_wide(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"wide/wide-1\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("wide ready");
    s.type_text("go");
    s.enter();
    assert!(s.pump(|s| s.status_now().iter().any(|l| l.contains("33% of 300k"))));

    let line = s.status_now().join(" ");
    let bars = line.match_indices('\u{2588}').count();
    assert!(bars > 0 && bars <= 8, "one bar of at most eight cells, not two: {line:?}");
}

/// Compaction moves the meter, because compaction is what moved the context.
///
/// The card said `180k → 12k` and the meter under it went on reporting 180k — for the rest of the
/// conversation, because a driver that has once answered "how full is it" turns off the estimate
/// derived from usage, and the next answer only arrives with the next request. The driver said
/// `after`; that is the number.
#[test]
fn compacting_updates_the_context_meter() {
    let sb = Sandbox::new("compactctx");
    install_compactor(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"compactor/compactor\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("compactor ready");
    s.type_text("go");
    s.enter();

    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("Compacted the conversation"))),
        "it compacted, and said so\n{:?}",
        s.chat_now()
    );
    // The card carries both numbers, so the one the meter has to agree with is on screen beside
    // it. 12k of the 200k window the driver reported is 6%; 180k is 90%, which is what the meter
    // went on saying.
    assert!(
        s.chat_now().iter().any(|l| l.contains("180.0k") && l.contains("12.0k")),
        "with the count either side of it\n{:?}",
        s.chat_now()
    );
    assert!(
        s.pump(|s| {
            let line = s.status_now().join("");
            line.contains("6% of 200k") && !line.contains("90%")
        }),
        "and the meter says what the window holds now\n{:?}",
        s.status_now()
    );
}

/// A strip too narrow for everything drops whole segments, least important first, rather than
/// writing the right half over the end of the left one.
///
/// Shortening comes first and dropping is what is left — see
/// [`a_narrow_strip_shortens_the_context_meter_rather_than_dropping_it`] — so at forty columns the
/// meter has already given up its denominator and it is the model name that goes. What must never
/// happen either way is the thing this test is named for: a right-hand segment written over the end
/// of a left-hand one, which reads as a corrupted line rather than as a full one.
#[test]
fn a_narrow_status_strip_drops_segments_rather_than_overlapping_them() {
    let sb = Sandbox::new("narrowstatus");
    install_wide(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"wide/wide-1\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("wide ready");
    assert!(s.pump(|s| s.status_now().iter().any(|l| l.contains("of 300k"))));

    s.viewport("[status]", 40, 1);
    assert!(
        s.pump(|s| {
            let line = s.status_now().join("");
            !line.contains("of 300k") && line.contains("chat")
        }),
        "the denominator went and the mode stayed\n{:?}",
        s.status_now()
    );
    let line = s.status_now().join("");
    assert!(
        line.chars().count() <= 41,
        "and the line still fits the window: {} chars of 40\n{line:?}",
        line.chars().count()
    );
    // Something was dropped — forty columns is not enough for what is left even shortened — and
    // what went is the model name rather than the meter.
    assert!(
        line.contains('%') && !line.contains("^P"),
        "and what went is the model name, not the number the meter exists for\n{line:?}"
    );
}

#[test]
fn a_key_shown_beside_what_it_changes_is_not_repeated_on_the_shortcut_row() {
    // The rule the footer exists for. Saying it twice costs a row and teaches nothing the first
    // place did not — and the first place is better, because the thing being changed is right
    // there being read.
    let sb = Sandbox::new("nodupekeys");
    let mut s = sb.start();
    assert!(
        s.pump(|s| s.status_now().iter().any(|l| l.contains("^P"))),
        "the model carries its key\n{:?}",
        s.status_now()
    );
    assert!(
        !s.composer_chrome().join(" ").contains("^P"),
        "and the shortcut row does not say it again\n{:?}",
        s.composer_chrome()
    );
}

#[test]
fn an_empty_composer_says_what_to_do_with_it() {
    let sb = Sandbox::new("placeholder");
    let mut s = sb.start();
    assert!(
        s.pump(|s| s.composer_chrome().iter().any(|t| t.contains("Ask anything"))),
        "a prompt for the one field there is\n{:?}",
        s.composer_chrome()
    );
    // And it goes away once there is something to send, or it would look like part of the message.
    s.type_text("hi");
    assert!(
        s.pump(|s| !s.composer_chrome().iter().any(|t| t.contains("Ask anything"))),
        "the placeholder steps aside\n{:?}",
        s.composer_chrome()
    );
}

#[test]
fn the_welcome_says_which_account_answers() {
    // The whole complaint the account split exists to answer: a fresh screen that says "no API
    // key" tells you nothing about the plan you are actually on. This says which it is, before you
    // have typed anything.
    let sb = Sandbox::new("welcome");
    let mut s = sb.start();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("model      mock/mock"))),
        "the welcome names the model\n{:?}",
        s.chat_now()
    );
}

#[test]
fn the_welcome_does_not_land_on_top_of_a_conversation() {
    // Regression: the greeting was appended at startup unconditionally, so restoring a session
    // printed "type a message and press Enter" *underneath* the last thing that was said.
    let sb = Sandbox::new("welcomeconv");
    let mut s = sb.start();
    s.pump(|s| s.chat_now().iter().any(|l| l.contains("neosh")));
    s.type_text("a question");
    s.enter();
    assert!(s.pump(|s| s.chat_now().iter().any(|l| l.contains("a question"))), "it was asked");
    // It may stay at the top — that is where a header belongs. What it must never do is appear
    // *below* the exchange, which is what happened when it was appended at startup regardless of
    // what had already been restored into the buffer.
    let chat = s.chat_now();
    let welcome = chat.iter().position(|l| l.contains("agent workspace"));
    let question = chat.iter().position(|l| l.contains("a question")).expect("the question is there");
    if let Some(w) = welcome {
        assert!(w < question, "the welcome stays above the conversation\n{chat:?}");
    }
}


// ---------------------------------------------------------------------------
// A model that has stopped working
// ---------------------------------------------------------------------------

impl Sandbox {
    /// Put a conversation on disk that remembers a model needing an API key nobody has.
    ///
    /// Written as a file rather than produced by driving the UI, because the situation being tested
    /// is specifically "this was chosen a long time ago, on a machine that had the key".
    fn remember_a_conversation_using(&self, instance: &str, model: &str) {
        let dir = self.root.join("state/sessions");
        std::fs::create_dir_all(&dir).expect("session dir");
        let cwd = self.work().display().to_string();
        std::fs::write(
            dir.join("aaaaaaaa-1111-2222-3333-444444444444.json"),
            format!(
                r#"{{"id":"aaaaaaaa-1111-2222-3333-444444444444","cwd":"{cwd}",
                    "created_at":1,"updated_at":2,"messages":[],
                    "selection":{{"instance":"{instance}","model":"{model}","options":[]}},
                    "archived":false}}"#
            ),
        )
        .expect("write session");
    }
}

#[test]
fn a_remembered_model_that_cannot_authenticate_gives_way_to_one_that_can() {
    // The complaint this exists for: a conversation started against an API-key provider, reopened
    // on a machine with no key, greeting you with "no key" over a model you did not choose today.
    // A stored selection is a record of what was used, not an instruction.
    let sb = Sandbox::new("staleselection");
    sb.remember_a_conversation_using("anthropic", "claude-opus-5");
    let mut s = sb.start_letting_config_choose();
    assert!(
        s.pump(|s| s.texts().iter().any(|t| t.contains("instead"))),
        "it said what it swapped and what for\n{}",
        s.transcript()
    );
    assert!(
        s.texts().iter().any(|t| t.contains("anthropic/claude-opus-5 cannot authenticate")),
        "and named the one that stopped working\n{}",
        s.transcript()
    );
}

#[test]
fn a_model_you_asked_for_is_not_swapped_out_from_under_you() {
    // The other half of the rule. `agent.model` is somebody saying which model to use; answering
    // that with a different one would be worse than the error they get when they send.
    let sb = Sandbox::new("pinnedselection");
    let dir = sb.root.join("config/plugins/locked");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"locked\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("write manifest");
    std::fs::write(dir.join("main.ts"), WANTS_A_KEY).expect("write plugin");
    sb.write_config("[options]\n\"agent.model\" = \"vault/sealed\"\n");

    let mut s = sb.start_letting_config_choose();
    s.wait_for("vault ready");
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("vault/sealed"))),
        "the welcome still names the model that was asked for\n{:?}",
        s.chat_now()
    );
    // The welcome still warns that it cannot authenticate — that is the honest thing to say. What
    // must not happen is a substitution, which is the "instead" this looks for.
    assert!(
        !s.texts().iter().any(|t| t.contains("instead")),
        "the choice stood\n{}",
        s.transcript()
    );
}

/// A driver whose instance wants a key from a variable that is never set.
///
/// Registered by a plugin rather than written in `config.toml`, because the point is an instance
/// that *resolves* — there is a driver behind it — and still cannot authenticate. A configured
/// instance with no driver loaded fails a different way, one step earlier.
const WANTS_A_KEY: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("vault", [{
    id: "vault",
    driver: "vault",
    display_name: "Vault",
    auth: { kind: "env", var: "NEOSH_TEST_VAULT_KEY_NEVER_SET" },
    models: [{
      id: "sealed",
      display_name: "Sealed",
      capabilities: {
        tools: false, vision: false, streaming: true, thinking: false, prompt_caching: false,
        option_descriptors: [],
      },
    }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "sealed", usage: {} });
    emit({ type: "message_stop" });
  });
  neosh.event.on("neosh.ready", () => neosh.notify("vault ready"));
}
"#;


/// A two-pane picker with a slow provider between two quick ones.
///
/// The shape of the bug this reproduces: holding a movement key starts one lookup per row it passes
/// through, and they finish in whatever order the network allows.
const SLOW_RAIL: &str = r#"
import type { PluginContext } from "@neosh/api";
import { railPicker } from "@neosh/api/ui";

const after = (neosh: any, ms: number) =>
  new Promise<void>((done) => neosh.timer.after(ms, () => done()));

export async function activate({ neosh }: PluginContext) {
  await neosh.cmd.register("slow.pick", async () => {
    await railPicker<string, string>(neosh, {
      title: "Slow",
      rail: [
        { label: "First", value: "first" },
        { label: "Dawdler", value: "dawdler" },
        { label: "Last", value: "last" },
      ],
      async items(which) {
        if (which === "dawdler") {
          await after(neosh, 600);
          return [];
        }
        return [{ label: `${which} arrived`, value: which }];
      },
    });
  });
  neosh.event.on("neosh.ready", () => neosh.notify("slow ready"));
}
"#;

#[test]
fn a_slow_provider_cannot_clobber_the_one_you_moved_on_to() {
    let sb = Sandbox::new("railrace");
    let dir = sb.root.join("config/plugins/slowrail");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"slowrail\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\n",
    )
    .expect("write manifest");
    std::fs::write(dir.join("main.ts"), SLOW_RAIL).expect("write plugin");

    let mut s = sb.start();
    s.wait_for("slow ready");
    s.send(&command("slow.pick"));
    s.wait_for("first arrived");

    // Two moves in a row, without waiting: straight through the slow one and out the other side,
    // which is what holding a movement key does.
    s.special("left");
    s.special("down");
    s.special("down");
    assert!(
        s.pump(|s| s.picker_named("[Slow]").iter().any(|l| l.contains("last arrived"))),
        "the provider you ended on decides what is listed\n{:?}",
        s.picker_named("[Slow]")
    );

    // And it stays that way once the one you passed through finally replies. Pumping keeps events
    // flowing while that happens, which is what gives the late answer its chance to land.
    let until = Instant::now() + Duration::from_millis(1200);
    while Instant::now() < until {
        s.pump(|_| false);
    }
    assert!(
        s.picker_named("[Slow]").iter().any(|l| l.contains("last arrived")),
        "a late answer to a question you have moved on from is dropped\n{:?}",
        s.picker_named("[Slow]")
    );
}


// ---------------------------------------------------------------------------
// Markdown in the transcript
// ---------------------------------------------------------------------------

fn markdown_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/markdown_turn.jsonl")
}

#[test]
fn an_answer_is_rendered_as_markdown_while_it_streams() {
    // The fixture arrives three characters at a time, so what this checks is the incremental path:
    // headings, lists and fences all have to survive being cut in half by a token boundary.
    let sb = Sandbox::new("mdstream");
    let mut s = sb.spawn(&markdown_fixture(), Some("mock/mock"));
    s.wait_for("PROJECTS");
    s.type_text("tell me");
    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l == "That is all.")),
        "the answer finished\n{:?}",
        s.chat_now()
    );

    let chat = s.chat_now();
    let has = |needle: &str| chat.iter().any(|l| l == needle);
    assert!(has("What I found"), "the heading lost its hashes\n{chat:?}");
    assert!(has("  \u{2022} the rule is drawn above the composer"), "bullets and bold\n{chat:?}");
    assert!(has("  \u{2022} ui.hints turns the row off"), "inline code\n{chat:?}");
    assert!(has("  \u{2022} links like the docs keep their words"), "link labels\n{chat:?}");
    assert!(has("  rust"), "the fence says what language it is\n{chat:?}");
    assert!(has("  fn main() {"), "and its contents are indented\n{chat:?}");
    assert!(
        has("      // ## not a heading"),
        "nothing inside a fence is markup\n{chat:?}"
    );
    assert!(
        !chat.iter().any(|l| l.contains("```") || l.contains("**")),
        "no markers survive into the transcript\n{chat:?}"
    );
}

#[test]
fn a_rendered_answer_looks_the_same_after_switching_away_and_back() {
    // Two renderers is how a reopened conversation stops looking like the one you were just in.
    let sb = Sandbox::new("mdreplay");
    // Not asked before closing: these use `session.close` as a way to *leave* a conversation and
    // come back, and the confirmation is a different test's subject.
    sb.write_config("[options]\n\"ui.confirm_destructive\" = false\n");
    let mut s = sb.spawn(&markdown_fixture(), Some("mock/mock"));
    s.wait_for("PROJECTS");
    s.type_text("tell me");
    s.enter();
    // Not just "the last words arrived" — the working line is still down there until the turn
    // actually ends, and it is not part of the answer being compared.
    assert!(
        s.pump(|s| {
            let rows = s.chat_now();
            rows.iter().any(|l| l == "That is all.")
                && !rows.iter().any(|l| l.contains("esc to interrupt"))
        }),
        "the turn finished"
    );
    let live: Vec<String> = s.chat_now().into_iter().filter(|l| !l.trim().is_empty()).collect();

    s.send(&command("session.new"));
    assert!(s.pump(|s| !s.chat_now().iter().any(|l| l == "That is all.")), "somewhere else now");
    // Closing the empty one lands back in the conversation it was opened from.
    s.send(&command("session.close"));
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l == "That is all.")),
        "and back\n{:?}",
        s.chat_now()
    );
    let replayed: Vec<String> = s.chat_now().into_iter().filter(|l| !l.trim().is_empty()).collect();
    assert!(
        replayed.ends_with(&live[live.len().saturating_sub(6)..]),
        "the replay ends the way the live stream did\nlive: {live:?}\nback: {replayed:?}"
    );
}

#[test]
fn markdown_can_be_turned_off_for_when_you_want_what_was_actually_sent() {
    let sb = Sandbox::new("mdoff");
    sb.write_config("[options]\n\"chat.markdown\" = false\n");
    let mut s = sb.spawn(&markdown_fixture(), Some("mock/mock"));
    s.wait_for("PROJECTS");
    s.type_text("tell me");
    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("## What I found"))),
        "the hashes are still there\n{:?}",
        s.chat_now()
    );
}


// ---------------------------------------------------------------------------
// Steering: saying something to a turn that is already running
// ---------------------------------------------------------------------------

#[test]
fn typing_while_it_works_is_steering_rather_than_an_error() {
    // It used to say "a turn is already running" and throw the sentence away. That is the one
    // moment in the program where you know exactly what you want to say and cannot say it.
    let sb = Sandbox::new("steer");
    install_stall(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"stall/stall\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("stall ready");

    s.type_text("first");
    s.enter();
    assert!(s.pump(|s| s.chat_now().iter().any(|l| l.contains("first"))), "the turn started");

    s.type_text("actually, do it differently");
    s.enter();
    assert!(
        s.pump(|s| s.composer_chrome().iter().any(|t| t.contains("queued"))),
        "it is held, and says so\n{:?}",
        s.composer_chrome()
    );
    assert!(
        s.composer_chrome().iter().any(|t| t.contains("do it differently")),
        "and says what is waiting\n{:?}",
        s.composer_chrome()
    );
    assert!(
        !s.texts().iter().any(|t| t.contains("already running")),
        "and does not refuse it\n{}",
        s.transcript()
    );
    // Not in the transcript yet: the model has not been told, and a transcript showing a question
    // nobody has been asked is a transcript that is lying.
    assert!(
        !s.chat_now().iter().any(|l| l.contains("do it differently")),
        "not shown as asked until it has been\n{:?}",
        s.chat_now()
    );
}

#[test]
fn a_message_queued_past_the_last_gap_becomes_the_next_turn() {
    // Steering is taken into the running turn at the next gap. If the turn ends before there is
    // one — interrupted, or simply finished — the question was still asked, and dropping it
    // silently would be the worst possible reading of "queued".
    let sb = Sandbox::new("steerleftover");
    install_stall(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"stall/stall\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("stall ready");

    s.type_text("first");
    s.enter();
    assert!(s.pump(|s| s.chat_now().iter().any(|l| l.contains("esc to interrupt"))), "working");
    s.type_text("second");
    s.enter();
    assert!(s.pump(|s| s.composer_chrome().iter().any(|t| t.contains("queued"))), "held");

    s.special("esc");
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("second"))),
        "it was asked once there was room to ask it\n{:?}",
        s.chat_now()
    );
}

#[test]
fn the_working_line_says_how_much_is_waiting() {
    let sb = Sandbox::new("steercount");
    install_stall(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"stall/stall\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("stall ready");
    s.type_text("go");
    s.enter();
    assert!(s.pump(|s| s.chat_now().iter().any(|l| l.contains("esc to interrupt"))), "working");

    s.type_text("one");
    s.enter();
    s.type_text("two");
    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("2 queued"))),
        "the line that says a turn is in flight also says what is behind it\n{:?}",
        s.chat_now()
    );
}

/// A driver that answers with the message it was actually handed.
///
/// The point of view that matters here. Everything else in this file can see what was drawn, and
/// what was drawn is not what was asked — the transcript writes a queued message down the moment
/// it enters the conversation, whether or not anything ever went out with it. Only the driver
/// knows, so the driver says.
///
/// It naps first, so there is a window in which to queue something while it is out.
const PARROT: &str = r#"
import type { PluginContext } from "@neosh/api";
const nap = (ms: number) => new Promise((r) => setTimeout(r, ms));

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("parrot", [{
    id: "parrot", driver: "parrot", display_name: "Parrot",
    models: [{ id: "parrot", display_name: "Parrot" }],
  }], async (req, emit) => {
    await nap(900);
    const last = [...req.messages].reverse().find((m) => m.role === "user");
    const said = (last?.content ?? [])
      .map((b: { type: string; text?: string }) => (b.type === "text" ? b.text ?? "" : ""))
      .join(" ")
      .split("\n")
      .map((l: string) => l.trim())
      .filter(Boolean)
      .join(" + ");
    emit({ type: "message_start", model: "parrot", usage: {} });
    emit({ type: "block_start", index: 0, block: { kind: "text" } });
    emit({ type: "text_delta", index: 0, text: "asked: " + said });
    emit({ type: "block_stop", index: 0 });
    emit({ type: "message_delta", stop_reason: { kind: "end_turn" }, usage: {} });
    emit({ type: "message_stop" });
  });
  neosh.event.on("neosh.ready", () => neosh.notify("parrot ready"));
}
"#;

fn install_parrot(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/parrot");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"parrot\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), PARROT).expect("plugin");
}

/// Two things queued into the same gap are two things that were asked.
///
/// They used to become two user *messages*, and a driver that keeps the conversation on its own
/// side is handed the newest one and nothing else — so the older question was drawn in the
/// transcript as asked and then never put to anybody. From the outside that is indistinguishable
/// from an agent that read your message and ignored it, which is exactly what it looked like.
///
/// Asserted on what the driver says it was handed, because the transcript cannot tell the
/// difference: it writes a queued message down when it enters the conversation, whether or not
/// anything went out with it.
#[test]
fn everything_queued_into_one_gap_is_asked_and_not_just_the_last_of_it() {
    let sb = Sandbox::new("steerboth");
    install_parrot(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"parrot/parrot\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("parrot ready");

    s.type_text("go");
    s.enter();
    assert!(s.pump(|s| s.chat_now().iter().any(|l| l.contains("esc to interrupt"))), "working");

    s.type_text("also the tests");
    s.enter();
    s.type_text("and the docs");
    s.enter();
    assert!(s.pump(|s| s.chat_now().iter().any(|l| l.contains("2 queued"))), "both held");

    assert!(
        s.pump(|s| s
            .chat_now()
            .iter()
            .any(|l| l.contains("asked: also the tests + and the docs"))),
        "both reached the driver, in the order they were typed\n{:?}",
        s.chat_now()
    );
}

/// The queue is unsent conversation, so it reads with the conversation and not with the furniture.
///
/// Under the field it sat below the shortcut row and one line above the status bar, in the strip
/// the eye has been trained to skip. Above it, it is the last thing between what has been said and
/// what you are saying — which is what it is.
#[test]
fn what_is_queued_sits_above_the_composer_and_says_how_to_take_it_back() {
    let sb = Sandbox::new("steerplace");
    install_stall(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"stall/stall\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("stall ready");
    s.type_text("go");
    s.enter();
    assert!(s.pump(|s| s.chat_now().iter().any(|l| l.contains("esc to interrupt"))), "working");

    s.type_text("and check the tests");
    s.enter();
    assert!(
        s.pump(|s| s.composer_chrome().iter().any(|t| t.contains("and check the tests"))),
        "held, and shown\n{:?}",
        s.composer_chrome()
    );
    assert!(
        s.composer_chrome().iter().any(|t| t.contains("take back")),
        "with the key that undoes it, saying what it does rather than naming the operation\n{:?}",
        s.composer_chrome()
    );
    // And the field itself says what `\u{23ce}` does now. "Ask anything" while a turn is running
    // invites the one thing you cannot do and says nothing about the thing you can, which is how a
    // sentence disappears into a queue nobody knew was there.
    assert!(
        s.composer_chrome().iter().any(|t| t.contains("Steer it")),
        "and the empty field says what sending does now\n{:?}",
        s.composer_chrome()
    );
    assert!(
        !s.composer_now().iter().any(|l| l.contains("and check the tests")),
        "and it is chrome, not something that would be sent twice: {:?}",
        s.composer_now()
    );

    // Enter is a decision made in half a second about a sentence you were still writing. This is
    // the other half of it: the message comes back to be changed rather than being lost to a typo.
    s.send(&command("chat.queue.edit"));
    assert!(
        s.pump(|s| s.composer_now().iter().any(|l| l.contains("and check the tests"))),
        "back where it can be edited\n{:?}",
        s.composer_now()
    );
    assert!(
        s.pump(|s| !s.composer_chrome().iter().any(|t| t.contains("and check the tests"))),
        "and out of the queue, not in both places\n{:?}",
        s.composer_chrome()
    );
}

/// A question butted against whatever came before it reads as another line of that answer.
///
/// The case this exists for is a tool card, which has no trailing blank of its own: the previous
/// turn ends with `└ 3 files`, and the next thing you said starts on the very next row.
// ---------------------------------------------------------------------------
// A conversation happens where its project is
// ---------------------------------------------------------------------------

/// A provider that answers with the directory it was told the turn happens in.
///
/// Which is the only way to check this from outside: the thing that was wrong was what a *driver*
/// received, and a driver is the far side of the provider boundary.
const WHERE: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("where", [{
    id: "where", driver: "where", display_name: "Where",
    models: [{ id: "where-1", display_name: "Where One" }],
  }], async (req, emit) => {
    emit({ type: "text_delta", index: 0, text: `cwd=${req.cwd}` });
    emit({ type: "message_stop" });
  });
  // Driven through the same calls a real plugin uses, so the test exercises the public path
  // rather than a back door the product does not have.
  await neosh.cmd.register("where.at", async (args) => {
    await neosh.session.create({ cwd: args[0], activate: true });
  }, { desc: "open a conversation in a directory" });
  await neosh.cmd.register("where.goto", async (args) => {
    const all = await neosh.session.list();
    const found = all.find((s) => s.cwd === args[0]);
    if (found) await neosh.session.switch(found.id);
    else neosh.notify(`no conversation in ${args[0]}`, "warn");
  }, { desc: "switch to the conversation in a directory" });
  neosh.event.on("neosh.ready", () => neosh.notify("where ready"));
}
"#;

fn install_where(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/where");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"where\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), WHERE).expect("plugin");
}

/// A turn happens in the conversation's project, not in whatever directory neosh was started from.
///
/// This was the whole bug. A conversation carried a `cwd` and the sidebar grouped by it, but the
/// thing that actually does the work — a vendor CLI, which reads files and runs commands relative
/// to where it was spawned — was never told. So every project's conversations ran in the same
/// directory, and the grouping was decoration.
#[test]
fn a_turn_runs_in_the_project_the_conversation_belongs_to() {
    let sb = Sandbox::new("turncwd");
    install_where(&sb);
    let elsewhere = sb.root.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).expect("mkdir");
    sb.write_config("[options]\n\"agent.model\" = \"where/where-1\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("where ready");
    s.wait_for("PROJECTS");

    // A conversation in a second directory, which is what adding a project amounts to.
    s.send(&command_with("where.at", &elsewhere.display().to_string()));
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("elsewhere"))),
        "the banner names the project this conversation is in\n{:?}",
        s.chat_now()
    );

    s.type_text("go");
    s.enter();
    let want = format!("cwd={}", elsewhere.display());
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains(&want))),
        "and the driver was told to work there\n{:?}",
        s.chat_now()
    );
}

/// Switching back moves everything back with it.
#[test]
fn switching_conversations_moves_the_working_directory_too() {
    let sb = Sandbox::new("switchcwd");
    install_where(&sb);
    let elsewhere = sb.root.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).expect("mkdir");
    sb.write_config("[options]\n\"agent.model\" = \"where/where-1\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("where ready");
    s.wait_for("PROJECTS");
    s.send(&command_with("where.at", &elsewhere.display().to_string()));
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("elsewhere"))),
        "in the second project\n{:?}",
        s.chat_now()
    );

    // Back to the first, and a turn there must run in the original workspace again.
    let home = format!("directory  {}", sb.work().display());
    s.send(&command_with("where.goto", &sb.work().display().to_string()));
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains(&home))),
        "back in the first\n{:?}",
        s.chat_now()
    );
    s.type_text("go");
    s.enter();
    let want = format!("cwd={}", sb.work().display());
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains(&want))),
        "back in the first project\n{:?}",
        s.chat_now()
    );
}

#[test]
fn a_question_gets_a_gap_above_it_and_keeps_its_bar() {
    let sb = Sandbox::new("questiongap");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.type_text("go");
    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("Read  .env"))),
        "the turn ran\n{:?}",
        s.chat_now()
    );
    s.type_text("again");
    s.enter();

    assert!(
        s.pump(|s| {
            let rows = s.chat_now();
            rows.iter().position(|l| l.contains("again")).is_some_and(|at| {
                at > 0 && rows[at - 1].trim().is_empty()
            })
        }),
        "there is a blank row above it\n{:?}",
        s.chat_now()
    );
    // And the bar is *coloured* on the question rather than on the blank row above it. The gap
    // shifts every row after it, which is exactly the sort of off-by-one that leaves the text
    // looking perfectly correct and the colour one row out.
    let rows = s.chat_now();
    let at = rows.iter().position(|l| l.contains("again")).expect("found above");
    let groups = s.chat_groups();
    assert!(
        groups.get(at).is_some_and(|g| g.iter().any(|h| h == "Agent.User")),
        "the bar is coloured on the question, not on the gap: {:?}",
        &groups[at.saturating_sub(1)..(at + 1).min(groups.len())]
    );
}

#[test]
fn a_tool_calls_dot_says_whether_it_is_still_going() {
    // The only thing in the transcript that says whether something is still happening. Colour
    // rather than shape, because a glyph that changes to mean "finished" makes the column
    // impossible to scan — and a dot that never stops pulsing is a tool that looks hung.
    let sb = Sandbox::new("tooldot");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.key("h");
    s.enter();
    assert!(
        s.pump(|s| {
            let rows = s.chat_now();
            let Some(at) = rows.iter().position(|l| l.contains("Read  .env")) else {
                return false;
            };
            // Failed, because `.env` is not there — which is the point: the dot reports how it
            // went, not merely that it is over.
            s.chat_groups().get(at).is_some_and(|g| g.iter().any(|h| h == "Agent.ToolFailed"))
        }),
        "the dot beside a finished call says how it went\n{:?}\n{:?}",
        s.chat_now(),
        s.chat_groups()
    );
}

#[test]
fn a_turn_that_has_already_said_something_still_says_it_is_working() {
    // The bug this fixes: the first token took the working line away and nothing put it back, so
    // a turn that had written a sentence and then went off to run tools looked finished in the
    // chat while the footer and the sidebar both said it was still going.
    let sb = Sandbox::new("stillworking");
    install_halfway(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"half/half\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("half ready");

    s.type_text("go");
    s.enter();

    assert!(
        s.pump(|s| {
            let rows = s.chat_now();
            rows.iter().any(|l| l.contains("what I found so far"))
                && rows.iter().any(|l| l.contains("esc to interrupt"))
        }),
        "the answer is on screen and so is the fact that it is not finished\n{:?}",
        s.chat_now()
    );
    // And in that order: a spinner above the sentence it is producing says the wrong thing about
    // which of them is still happening.
    let rows = s.chat_now();
    let said = rows.iter().position(|l| l.contains("what I found so far")).expect("the answer");
    let spinner = rows.iter().position(|l| l.contains("esc to interrupt")).expect("the line");
    assert!(spinner > said, "the working line is under the answer\n{rows:?}");
}

#[test]
fn how_much_of_a_tool_result_to_show_is_a_setting() {
    // Three lines is a default, not a law. `0` is the other end of it: how much came back, and
    // nothing else.
    let sb = Sandbox::new("toollines");
    install_chatty(&sb);
    sb.write_config(
        "[options]\n\"agent.model\" = \"chatty/chatty\"\n\"chat.tool_output_lines\" = 2\n",
    );
    let mut s = sb.start_letting_config_choose();
    s.wait_for("chatty ready");
    s.type_text("go");
    s.enter();

    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("Counted."))),
        "the turn finished\n{:?}",
        s.chat_now()
    );
    let rows = s.chat_now();
    assert!(rows.iter().any(|l| l.contains("one")), "one line of head is there\n{rows:?}");
    // Folded from the *middle*: what a command decided is at the end of what it printed, and a
    // card that keeps the first two lines of `cargo test` keeps `Compiling` and `Finished`.
    assert!(rows.iter().any(|l| l.contains("five")), "and the line it ended on\n{rows:?}");
    assert!(
        !rows.iter().any(|l| l.contains("three")),
        "and the setting stopped it there\n{rows:?}"
    );
    assert!(
        rows.iter().any(|l| l.contains("+3 lines")),
        "saying how much was left out, because a result that is silently cut is a result you \
         would believe\n{rows:?}"
    );
}

#[test]
fn a_tool_call_says_what_came_back_not_only_that_it_ran() {
    // A card that names the tool and nothing else is a card you have to take on trust, and "it
    // ran" is not the thing you wanted to know.
    let sb = Sandbox::new("toolresult");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.type_text("go");
    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("Read  .env"))),
        "the call is shown with its subject\n{:?}",
        s.chat_now()
    );
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains('\u{2514}')
            && l.contains("could not read"))),
        "and what it came back with, under it and joined to it\n{:?}",
        s.chat_now()
    );
}

#[test]
fn an_edit_card_shows_the_change_and_how_much_of_it() {
    // `edit(main.rs)` and "edited" underneath says that something happened. The diff says what,
    // the `+2 -1` says how much, and the row at the end of the turn adds it up — which is the
    // difference between a transcript you can review and one you have to take on trust.
    let sb = Sandbox::new("editcard");
    install_editor(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"editor/editor\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("editor ready");
    s.type_text("go");
    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("changed 1 file"))),
        "the turn finished and was summed up\n{:?}",
        s.chat_now()
    );
    let rows = s.chat_now();
    let header = rows
        .iter()
        .position(|l| l.starts_with("  Edited  ") && l.contains("main.rs"))
        .expect("the card's header");
    assert!(
        rows[header].ends_with("+2 -1"),
        "the stats are on the header, and a call this fast is not timed\n{rows:?}"
    );
    let del = rows.iter().position(|l| l.ends_with("- e")).expect("the removed line");
    let add = rows.iter().position(|l| l.ends_with("+ E")).expect("the added line");
    assert!(header < del && del < add, "the diff is under its header, in order\n{rows:?}");
    assert!(rows.iter().any(|l| l.ends_with("+ i")), "{rows:?}");
    assert!(
        !rows.iter().any(|l| l.trim() == "a" || l.trim() == "b"),
        "not every unchanged line: two either side of a change is context, eight is a file\n{rows:?}"
    );
    assert!(
        !rows.iter().any(|l| l.contains("more lines")),
        "every change is on screen, so there is no 'more'\n{rows:?}"
    );
    let total = rows.iter().position(|l| l.contains("changed 1 file  +2 -1")).expect("the total");
    assert!(rows[total + 1].contains("main.rs") && rows[total + 1].ends_with("+2 -1"), "{rows:?}");
    let answer = rows.iter().position(|l| l.contains("Edited.")).expect("the answer");
    assert!(answer < total, "the summary closes the turn, under the answer\n{rows:?}");
}

#[test]
fn calls_that_only_looked_at_things_share_one_row_and_open_into_several() {
    // A turn that reads three files used to be three cards, each with a mark down the left and a
    // slice of the file under it. What you wanted was the *list of what it looked at*, which is
    // exactly what those bodies were holding apart. So the run is one row — and the edit that
    // follows is not part of it, because a diff is an answer and not a name.
    let sb = Sandbox::new("looks");
    install_looker(&sb);
    // Not asked before closing: these use `session.close` as a way to *leave* a conversation and
    // come back, and the confirmation is a different test's subject.
    sb.write_config(
        "[options]\n\"agent.model\" = \"looker/looker\"\n\"ui.confirm_destructive\" = false\n",
    );
    let mut s = sb.start_letting_config_choose();
    s.wait_for("looker ready");
    s.type_text("go");
    s.enter();
    assert!(
        s.pump(|s| {
            let rows = s.chat_now();
            rows.iter().any(|l| l.contains("Looked."))
                && !rows.iter().any(|l| l.contains("esc to interrupt"))
        }),
        "the turn finished\n{:?}",
        s.chat_now()
    );
    let rows = s.chat_now();
    let run = rows
        .iter()
        .position(|l| l.starts_with("  Explored  "))
        .unwrap_or_else(|| panic!("the run is one row\n{rows:?}"));
    assert_eq!(
        rows[run], "  Explored  one.rs, two.rs, fn main",
        "named, shortest first-glance form\n{rows:?}"
    );
    assert!(
        !rows.iter().any(|l| l.contains("line 1")),
        "and nothing any of them came back with\n{rows:?}"
    );
    assert!(
        !rows.iter().any(|l| l.starts_with('\u{23fa}')),
        "no card carries the old dot\n{rows:?}"
    );
    // The edit is its own card, on the very next row: a card is a header at column zero with its
    // body indented under a corner, which is what says where one action ends and the next begins —
    // a blank row between every two of them was a row spent saying it again.
    assert!(rows[run + 1].starts_with("  Edited  "), "directly under the run\n{rows:?}");
    assert!(rows.iter().any(|l| l.ends_with("- a")), "the edit kept its diff\n{rows:?}");

    // Opened, one row per call, with the whole subject and how much came back.
    s.ctrl("s");
    s.key("g");
    s.key("g");
    for _ in 0..run {
        s.key("j");
    }
    s.special("tab");
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("Read  /work/src/one.rs"))),
        "open, every call is named in full\n{:?}",
        s.chat_now()
    );
    let open = s.chat_now();
    assert!(open.iter().any(|l| l.contains("Read  /work/src/two.rs  9 lines")), "{open:?}");
    assert!(open.iter().any(|l| l.contains("Searched  fn main  4 lines")), "{open:?}");
    assert_eq!(open[run], rows[run], "the header did not move or change\n{open:?}");

    // And the same rows come back after switching away and back, which is the whole reason the
    // live path and the replay go through one renderer.
    s.special("esc");
    let live: Vec<String> = s.chat_now().into_iter().filter(|l| !l.trim().is_empty()).collect();
    s.send(&command("session.new"));
    assert!(s.pump(|s| !s.chat_now().iter().any(|l| l.contains("Looked."))), "somewhere else");
    s.send(&command("session.close"));
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("Looked."))),
        "and back\n{:?}",
        s.chat_now()
    );
    let back = s.chat_now();
    assert!(
        back.iter().any(|l| l == "  Explored  one.rs, two.rs, fn main"),
        "the replay folds the run exactly as the live stream did\nlive: {live:?}\nback: {back:?}"
    );
    assert!(
        !back.iter().any(|l| l.contains("line 1")),
        "and does not unfold it\n{back:?}"
    );
}

/// A provider that runs three commands in one turn, and a `run` tool with canned output for each.
///
/// The shell neosh does not have: what matters to a card is that the call was handed a `command`,
/// which is how a card decides what a call is in the first place — off the arguments, never off a
/// list of tool names.
const RUNNER: &str = r#"
import type { PluginContext } from "@neosh/api";

const SAID: Record<string, string> = {
  "git add -A": "",
  "git commit -m 'stack the cards'": "[main 6759f10] stack the cards\n 3 files changed",
  "git push origin main": "To github.com:neoswarm/neosh\n   6759f10..c280938  main -> main",
};

export async function activate({ neosh }: PluginContext) {
  await neosh.tool.register(
    { name: "run", description: "Run", inputSchema: { type: "object" } },
    async (input) => ({ content: SAID[(input as { command: string }).command] ?? "ran" }),
  );
  let turn = 0;
  await neosh.provider.register("runner", [{
    id: "runner", driver: "runner", display_name: "Runner",
    models: [{ id: "runner", display_name: "Runner" }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "runner", usage: {} });
    turn += 1;
    if (turn === 1) {
      Object.keys(SAID).forEach((command, i) => {
        emit({ type: "block_start", index: i, block: { kind: "tool_use", id: `r${i}`, name: "run" } });
        emit({ type: "tool_input_delta", index: i, partial_json: JSON.stringify({ command }) });
        emit({ type: "block_stop", index: i });
      });
      emit({ type: "message_delta", stop_reason: { kind: "tool_use" }, usage: {} });
      emit({ type: "message_stop" });
      return;
    }
    emit({ type: "block_start", index: 0, block: { kind: "text" } });
    emit({ type: "text_delta", index: 0, text: "Pushed." });
    emit({ type: "block_stop", index: 0 });
    emit({ type: "message_delta", stop_reason: { kind: "end_turn" }, usage: {} });
    emit({ type: "message_stop" });
  });
  neosh.event.on("neosh.ready", () => neosh.notify("runner ready"));
}
"#;

fn install_runner(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/runner");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"runner\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\", \"tools\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), RUNNER).expect("plugin");
}

/// A stretch of a turn spent running things is one row, and the answer is the last one's.
#[test]
fn a_run_of_commands_is_one_row_that_keeps_the_last_answer() {
    let sb = Sandbox::new("stack");
    install_runner(&sb);
    sb.write_config(
        "[options]\n\"agent.model\" = \"runner/runner\"\n\"ui.confirm_destructive\" = false\n",
    );
    let mut s = sb.start_letting_config_choose();
    // The splash names the active model, and the model the config asked for only becomes active
    // once the plugin that provides it has finished loading. "runner ready" is the plugin saying
    // it registered; this is the host saying it took.
    s.wait_for("runner/runner");
    s.type_text("go");
    s.enter();
    assert!(
        s.pump(|s| {
            let rows = s.chat_now();
            rows.iter().any(|l| l.contains("Pushed."))
                && !rows.iter().any(|l| l.contains("esc to interrupt"))
        }),
        "the turn finished\n{:?}",
        s.chat_now()
    );
    let rows = s.chat_now();
    let run = rows
        .iter()
        .position(|l| l.starts_with("  Ran  "))
        .unwrap_or_else(|| panic!("the stack is one row\n{rows:?}"));
    assert_eq!(
        rows[run], "  Ran  git add, git commit, git push origin",
        "each command named by what it is rather than how it was spelled\n{rows:?}"
    );
    // The last one's output, because that is the one the other two were getting to.
    assert!(
        rows.iter().any(|l| l.contains("6759f10..c280938")),
        "the answer is under it\n{rows:?}"
    );
    assert!(
        !rows.iter().any(|l| l.contains("3 files changed")),
        "and the ones before it folded into their names\n{rows:?}"
    );

    // Opened, every command in full with the whole of what it printed.
    s.ctrl("s");
    s.key("g");
    s.key("g");
    for _ in 0..run {
        s.key("j");
    }
    s.special("tab");
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("3 files changed"))),
        "open, nothing the fold hid is out of reach\n{:?}",
        s.chat_now()
    );
    let open = s.chat_now();
    assert!(
        open.iter().any(|l| l.contains("Ran  git commit -m 'stack the cards'")),
        "and every command is there in full\n{open:?}"
    );
    assert_eq!(open[run], rows[run], "the header did not move or change\n{open:?}");

    // And the replay folds it exactly as the live stream did, which is the whole reason the two
    // go through one renderer.
    s.special("tab");
    s.special("esc");
    s.send(&command("session.new"));
    assert!(s.pump(|s| !s.chat_now().iter().any(|l| l.contains("Pushed."))), "somewhere else");
    s.send(&command("session.close"));
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("Pushed."))),
        "and back\n{:?}",
        s.chat_now()
    );
    let back = s.chat_now();
    assert!(
        back.iter().any(|l| l == "  Ran  git add, git commit, git push origin"),
        "one row again\n{back:?}"
    );
    assert!(
        back.iter().any(|l| l.contains("6759f10..c280938"))
            && !back.iter().any(|l| l.contains("3 files changed")),
        "with the last answer and no other\n{back:?}"
    );
}

/// The three states a card can be in, and the two things that move it between them.
///
/// Folded is what a card costs when nobody is looking at it. The card the *cursor* is in shows
/// more — bounded, because nine hundred rows appearing under `j` is what the fold exists to
/// prevent — and `\u{21e5}` is all of it. The trailer says which key at every step where something
/// is still being held back.
#[test]
fn a_card_opens_as_the_cursor_arrives_and_folds_as_it_leaves() {
    let sb = Sandbox::new("cardfold");
    install_editor(&sb);
    sb.write_config(
        "[options]\n\"agent.model\" = \"editor/editor\"\n\"chat.diff_lines\" = 2\n\
         \"chat.preview_lines\" = 4\n",
    );
    let mut s = sb.start_letting_config_choose();
    s.wait_for("editor ready");
    s.type_text("go");
    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("changed 1 file"))),
        "the turn finished\n{:?}",
        s.chat_now()
    );
    let rows = s.chat_now();
    let trailer = rows.iter().find(|l| l.contains(" lines (")).expect("a trailer");
    assert!(trailer.contains("+6 lines"), "{trailer}");
    assert!(trailer.contains("^S \u{21e5} to expand"), "it says how: {trailer}");
    assert!(!rows.iter().any(|l| l.ends_with("+ i")), "the end of the diff is folded away\n{rows:?}");
    let header = rows.iter().position(|l| l.starts_with("  Edited  ")).expect("the header");

    // Into the transcript, to the top, and down onto the card. Arriving is what opens it — no key.
    s.ctrl("s");
    s.key("g");
    s.key("g");
    for _ in 0..header {
        s.key("j");
    }
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("+4 lines"))),
        "the card the cursor is in is worth more rows, and still bounded\n{:?}",
        s.chat_now()
    );
    assert!(
        !s.chat_now().iter().any(|l| l.ends_with("+ i")),
        "a preview is a budget, not an opening\n{:?}",
        s.chat_now()
    );

    s.special("tab");
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.ends_with("+ i"))),
        "open, the whole diff is there\n{:?}",
        s.chat_now()
    );
    let rows = s.chat_now();
    assert!(!rows.iter().any(|l| l.contains(" lines (")), "and nothing is held back\n{rows:?}");
    assert!(
        rows.iter().position(|l| l.contains("changed 1 file")).unwrap() > header,
        "what was below the card is still below it\n{rows:?}"
    );

    // `\u{21e5}` again gives back what the cursor was already buying — never less than that, which
    // would be a key that answers "show me more" with a smaller card.
    s.special("tab");
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("+4 lines"))),
        "back to the preview\n{:?}",
        s.chat_now()
    );

    // And off the card entirely, which is the only thing that folds it.
    s.key("k");
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("+6 lines"))),
        "the rows go back when the cursor does\n{:?}",
        s.chat_now()
    );
}

#[test]
fn the_welcome_says_how_to_read_the_transcript() {
    // `^S` existed for a long time before anything on screen mentioned it. The welcome is the one
    // place everybody looks once.
    let sb = Sandbox::new("welcomekeys");
    let mut s = sb.start();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("^S browse & copy"))),
        "the key row\n{:?}",
        s.chat_now()
    );
    let rows = s.chat_now();
    // The frontend in these tests is 100 columns wide, which is room for the word.
    assert!(
        rows.iter().any(|l| l.contains("\u{2588}\u{2588}\u{2588}\u{2557}")),
        "and the word above it\n{rows:?}"
    );
}

#[test]
fn the_footer_says_what_the_agent_may_do_and_which_key_changes_it() {
    let sb = Sandbox::new("permmode");
    let mut s = sb.start();
    // Full access out of the box — see `PermissionsConfig::default` — which is precisely why the
    // row has to be there in every mode rather than only the surprising ones.
    assert!(
        s.pump(|s| s.status_now().iter().any(|l| l.contains("full access \u{21e7}\u{21e5}"))),
        "the mode, and the key beside it\n{:?}",
        s.status_now()
    );
    s.send(&command("permission.cycle"));
    assert!(
        s.pump(|s| s.status_now().iter().any(|l| l.contains("deny"))),
        "and it changes\n{:?}",
        s.status_now()
    );
}

/// The mode belongs to the conversation, not to the terminal it is read in.
///
/// The failure this is here to catch is silent and in the dangerous direction: a mode kept in one
/// shared place means switching away from a conversation you had put in `deny` and into any other
/// one leaves that other one in `deny` too — and, worse, coming back the other way carries full
/// access into a conversation somebody had deliberately locked down.
#[test]
fn a_permission_mode_belongs_to_the_conversation_it_was_chosen_in() {
    let sb = Sandbox::new("permsession");
    // Not asked before closing: these use `session.close` as a way to *leave* a conversation and
    // come back, and the confirmation is a different test's subject.
    sb.write_config("[options]\n\"ui.confirm_destructive\" = false\n");
    let mut s = sb.start_letting_config_choose();
    assert!(
        s.pump(|s| s.status_now().iter().any(|l| l.contains("full access"))),
        "the configured default to start from\n{:?}",
        s.status_now()
    );

    s.send(&command("permission.cycle")); // full access -> deny
    assert!(
        s.pump(|s| s.status_now().iter().any(|l| l.contains("deny"))),
        "this one is set\n{:?}",
        s.status_now()
    );

    // A conversation that has never been given a mode takes the configured one, not whatever the
    // conversation you came from happened to be left in.
    s.send(&command("session.new"));
    assert!(
        s.pump(|s| s.status_now().iter().any(|l| l.contains("full access"))),
        "a new one is its own\n{:?}",
        s.status_now()
    );

    s.send(&command("session.close"));
    assert!(
        s.pump(|s| s.status_now().iter().any(|l| l.contains("deny"))),
        "and going back is going back to it\n{:?}",
        s.status_now()
    );
}

impl Session {
    /// The window showing a named buffer, if the frontend was told about one.
    fn window_of(&self, name: &str) -> Option<u64> {
        let buf = self.buffer_named(name)?;
        self.events.iter().find_map(|e| {
            (e["type"] == "window_opened" && e["buf"].as_u64() == Some(buf))
                .then(|| e["win"].as_u64())?
        })
    }

    /// The key strip on a float's bottom border, as the window it belongs to was announced.
    ///
    /// A legend is *not* a row of the buffer any more, and the reason is the reason it is worth
    /// asserting: written as content it was both the row that scrolled away and the first row a
    /// short terminal clipped, and the row in question is the one that says how to get out. It
    /// travels on `FloatConfig`, so this reads it where the promise is actually made.
    fn footer_of(&self, name: &str) -> Option<String> {
        let buf = self.buffer_named(name)?;
        let open = self.open_windows();
        self.events
            .iter()
            .filter(|e| e["type"] == "window_opened" && e["buf"].as_u64() == Some(buf))
            .filter(|e| e["win"].as_u64().is_some_and(|w| open.contains(&w)))
            .find_map(|e| e["layout"]["config"]["footer"].as_str().map(str::to_string))
    }

    /// Tell the core how big a window really is.
    ///
    /// A stdio frontend has no geometry of its own, so nothing reports this unless a test does —
    /// which means anything that depends on real width is untested until it is said out loud here.
    fn viewport(&mut self, name: &str, width: u16, height: u16) {
        let Some(win) = self.window_of(name) else { return };
        self.send(&format!(
            r#"{{"type":"viewport_changed","win":{win},"width":{width},"height":{height},"top_line":0}}"#
        ));
    }

    fn status_now(&self) -> Vec<String> {
        match self.buffer_named("[status]") {
            Some(b) => self.lines_of(b),
            None => Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Extending a bundled panel from outside it
// ---------------------------------------------------------------------------

/// A plugin that has never heard of the sidebar's source, adding to the sidebar.
///
/// Everything it does is on the public surface: a contribution point for rows, a contribution point
/// for a verb, and a key bound at the *buffer kind* rather than at a window it cannot name. If any
/// of that ever needs a private path into the panel, these tests stop compiling against the API a
/// third party has.
const EXTENDER: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh, subscriptions }: PluginContext) {
  subscriptions.push(await neosh.cmd.register("acme.touched", (args) => {
    // The row under the cursor arrives as arguments, which is the whole reason the panel binds the
    // key rather than the contributor doing it.
    neosh.notify(`acme touched ${args.join(" ")}`);
  }, { desc: "Touch the row" }));

  subscriptions.push(await neosh.ext.contribute("sidebar.section", "todo", {
    title: "ACME",
    at: "below",
    rows: [{ text: "Ship the thing", command: "acme.touched", args: ["from-row"] }],
  }));

  subscriptions.push(await neosh.ext.contribute("sidebar.action", "touch", {
    key: "t",
    label: "touch",
    command: "acme.touched",
    on: "any",
  }));

  neosh.event.on("neosh.ready", () => neosh.notify("acme ready"));
}
"#;

fn install_extender(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/acme");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"acme\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), EXTENDER).expect("plugin");
}

/// Rows somebody else contributed are in the panel, and pressing `↵` on one runs their command.
#[test]
fn a_third_party_can_put_rows_in_the_sidebar() {
    let sb = Sandbox::new("extendrows");
    install_extender(&sb);
    let mut s = sb.start();
    s.wait_for("acme ready");
    s.wait_for("PROJECTS");

    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("Ship the thing"))),
        "the contributed row is drawn\n{:?}",
        s.sidebar_now()
    );

    // Into the panel, then down to their row and press it. Walked rather than jumped: the cursor
    // wraps, so a fixed number of presses would depend on how many rows the panel happens to have.
    s.ctrl("t");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("↵ open"))),
        "in the panel\n{:?}",
        s.sidebar_now()
    );
    let mut landed = false;
    for _ in 0..12 {
        if s.sidebar_cursor().is_some_and(|r| r.contains("Ship the thing")) {
            landed = true;
            break;
        }
        s.key("j");
        s.drain_for(Duration::from_millis(120));
    }
    assert!(landed, "the cursor can be put on a contributed row\n{:?}", s.sidebar_now());

    s.special("enter");
    assert!(
        s.pump(|s| s.saw("acme touched from-row")),
        "the contributed command ran with the arguments it asked for\n{:?}",
        s.sidebar_now()
    );
}

/// A verb contributed by somebody else is a real binding on the panel: it fires on its key, and the
/// row under the cursor is handed to it.
///
/// The key is bound at `buf_kind` scope, which is the part that could not be done before — the
/// panel's window id is private to the panel and changes every time it is toggled, so a third party
/// had no name to bind against.
#[test]
fn a_third_party_verb_fires_on_its_key_with_the_row_it_was_pointed_at() {
    let sb = Sandbox::new("extendverb");
    install_extender(&sb);
    let mut s = sb.start();
    s.wait_for("acme ready");
    s.wait_for("PROJECTS");

    s.ctrl("t");
    // The panel lands on the conversation you are in, so the row under the cursor is a session.
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("↵ open"))),
        "the cursor is on a conversation\n{:?}",
        s.sidebar_now()
    );

    s.key("t");
    assert!(
        s.pump(|s| s.saw("acme touched session")),
        "the contributed key fired, and was told which row\n{}",
        s.transcript()
    );
}

/// The panel's own keys are ordinary bindings, so the registry knows them — and so anybody can
/// replace one.
///
/// This is the claim the whole rewrite rests on. Before it, the panel resolved keys in a private
/// switch statement: `^Z` could not list them, `^K` could not run them, and rebinding `x` meant
/// forking the plugin.
#[test]
fn the_panels_keys_are_in_the_registry_and_a_user_can_rebind_them() {
    let sb = Sandbox::new("panelkeys");
    std::fs::write(
        sb.root.join("config/init.ts"),
        r#"import type { PluginContext } from "@neosh/api";
export async function activate({ neosh }: PluginContext) {
  await neosh.cmd.register("mine.shout", () => neosh.notify("mine ran"), { desc: "Shout" });
  // Exactly what a third party writes to take a key inside somebody else's panel.
  await neosh.keymap.set("chat", "x", "mine.shout", {
    scope: { kind: "buf_kind", name: "neosh.sidebar" },
    desc: "Shout instead of archiving",
  });
  neosh.notify("init done");
}
"#,
    )
    .expect("init.ts");

    let mut s = sb.start();
    s.wait_for("init done");
    s.wait_for("PROJECTS");

    s.ctrl("t");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("↵ open"))),
        "in the panel, on a conversation\n{:?}",
        s.sidebar_now()
    );

    // `x` archives a conversation by default. Bound by the user, it does not.
    let before = s.sidebar_rows();
    s.key("x");
    assert!(
        s.pump(|s| s.saw("mine ran")),
        "the user's binding won\n{}",
        s.transcript()
    );
    assert_eq!(
        s.sidebar_rows(),
        before,
        "and the default it replaced did not also happen\n{:?}",
        s.sidebar_now()
    );
}

/// A count moves that many rows you can land on, and the panel says one is being typed.
///
/// The keys are Vim's because this is a cursor over a column in a terminal, and a list of thirty
/// conversations reached one `j` at a time is a list nobody moves around in. Counted in rows the
/// cursor can *land* on rather than lines, so the headings and rules it already skips do not
/// silently eat two of the five.
#[test]
fn a_count_before_a_motion_moves_that_many_rows() {
    let sb = Sandbox::new("counts");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    // Three conversations, so there is somewhere to count to. Named by what was typed into them,
    // which is what a conversation with no title is called in a list.
    let named = |s: &mut Session, name: &str| {
        s.type_text(name);
        s.enter();
        // Waited for in the *panel*, not in the transcript: the next thing this test does is start
        // another conversation, and a row still called "New conversation" because the rename has
        // not arrived yet is a row that makes counting them meaningless.
        let want = name.to_string();
        assert!(
            s.pump(move |s| s.sidebar_now().iter().any(|l| l.contains(&want))),
            "the panel says what this conversation is about"
        );
    };
    named(&mut s, "alpha");
    s.new_conversation();
    named(&mut s, "beta");
    s.new_conversation();
    named(&mut s, "gamma");

    // The panel lands on the conversation you are in, and the list is newest first — so counting
    // down from `gamma` two rows is `alpha`, with `beta` in between.
    s.enter_panel();
    assert!(
        s.pump(|s| s.sidebar_cursor().is_some_and(|l| l.contains("gamma"))),
        "the cursor starts where you are\n{:?}",
        s.sidebar_now()
    );

    // Half of it: a digit on its own moves nothing and says so, because a first press with no
    // visible effect is indistinguishable from a key that does nothing.
    s.key("2");
    assert!(
        s.pump(|s| s.sidebar_virt_now().iter().any(|v| v.trim() == "2")),
        "the count being typed is on screen\n{:?}",
        s.sidebar_virt_now()
    );
    assert!(
        s.sidebar_cursor().is_some_and(|l| l.contains("gamma")),
        "and nothing has moved yet\n{:?}",
        s.sidebar_now()
    );

    s.key("j");
    assert!(
        s.pump(|s| s.sidebar_cursor().is_some_and(|l| l.contains("alpha"))),
        "two rows down, not two lines\n{:?}",
        s.sidebar_now()
    );
    assert!(
        !s.sidebar_virt_now().iter().any(|v| v.trim() == "2"),
        "and the count is spent\n{:?}",
        s.sidebar_virt_now()
    );

    // `gg` is the top of the list, which is the project heading — the first row anything can land
    // on. `G` is the far end.
    s.key("g");
    s.key("g");
    assert!(
        s.pump(|s| s.sidebar_cursor().is_some_and(|l| l.contains("work"))),
        "gg is the first row you can land on\n{:?}",
        s.sidebar_now()
    );
    s.key("2");
    s.key("G");
    assert!(
        s.pump(|s| s.sidebar_cursor().is_some_and(|l| l.contains("gamma"))),
        "2G is the second row, not the end\n{:?}",
        s.sidebar_now()
    );
}

/// The column is as wide as you want it, and resizing it does not take the keyboard away.
///
/// Reopening the dock was how this used to be done, and it gives the window a new id: the panel
/// resized from inside itself threw the cursor back to the composer on every press. The width is
/// the `sidebar.width` setting either way, so a key and a line of `config.toml` say one number.
#[test]
fn the_panel_is_resized_in_place_and_keeps_the_cursor() {
    let sb = Sandbox::new("width");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.enter_panel();

    let rule = |s: &Session| {
        s.sidebar_now().iter().filter(|l| l.starts_with('\u{2500}')).map(|l| l.chars().count()).max()
    };
    let before = rule(&s).expect("the panel has a rule under its heading");

    s.key("3");
    s.key(">");
    assert!(
        s.pump(move |s| rule(s).is_some_and(|w| w > before)),
        "the column got wider\n{:?}",
        s.sidebar_now()
    );
    // Still in the panel: the hint strip is the panel's own, not the composer's.
    assert!(
        s.sidebar_now().iter().any(|l| l.contains("? keys")),
        "and the panel still has the keyboard\n{:?}",
        s.sidebar_now()
    );

    s.key("=");
    assert!(
        s.pump(move |s| rule(s) == Some(before)),
        "and back to the default\n{:?}",
        s.sidebar_now()
    );
}

impl Session {
    /// The right-hand column of every panel row, as it is *now*.
    ///
    /// [`Session::sidebar_virt`] is every one ever drawn, which answers a different question: a
    /// count that was on screen and has since been spent is in that list for the rest of the
    /// session, so "it is gone" is not a thing it can be asked.
    fn sidebar_virt_now(&self) -> Vec<String> {
        let Some(buf) = self.buffer_named("[sidebar]") else { return Vec::new() };
        let mut rows: Vec<String> = Vec::new();
        for e in &self.events {
            if e["type"] != "buffer_lines" || e["buf"].as_u64() != Some(buf) {
                continue;
            }
            let start = e["start"].as_i64().unwrap_or(0);
            let old_end = e["old_end"].as_i64().unwrap_or(start);
            let new: Vec<String> = e["lines"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|l| {
                            l["marks"]
                                .as_array()
                                .map(|ms| {
                                    ms.iter()
                                        .filter_map(|m| m["virt_text"].as_array())
                                        .flatten()
                                        .filter_map(|c| c["text"].as_str())
                                        .collect::<Vec<_>>()
                                        .join("")
                                })
                                .unwrap_or_default()
                        })
                        .collect()
                })
                .unwrap_or_default();
            let s = start.clamp(0, rows.len() as i64) as usize;
            let en = if old_end < 0 { rows.len() } else { (old_end as usize).clamp(s, rows.len()) };
            rows.splice(s..en, new);
        }
        rows
    }

    /// The marks on the panel row the cursor is on, as `(group, is_band)`.
    ///
    /// Both kinds, and which kind each is, because that distinction is the whole question here: a
    /// band sits under every ranged group on the row and a ranged group wins against them, and the
    /// marks are otherwise identical.
    fn sidebar_cursor_marks(&self) -> Vec<(String, bool)> {
        let Some(buf) = self.buffer_named("[sidebar]") else { return Vec::new() };
        let mut rows: Vec<Vec<(String, bool)>> = Vec::new();
        for e in &self.events {
            if e["type"] != "buffer_lines" || e["buf"].as_u64() != Some(buf) {
                continue;
            }
            let start = e["start"].as_i64().unwrap_or(0);
            let old_end = e["old_end"].as_i64().unwrap_or(start);
            let new: Vec<Vec<(String, bool)>> = e["lines"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|l| {
                            l["marks"]
                                .as_array()
                                .map(|ms| {
                                    ms.iter()
                                        .flat_map(|m| {
                                            [
                                                m["hl_group"].as_str().map(|g| (g.to_string(), false)),
                                                m["line_hl_group"]
                                                    .as_str()
                                                    .map(|g| (g.to_string(), true)),
                                            ]
                                        })
                                        .flatten()
                                        .collect()
                                })
                                .unwrap_or_default()
                        })
                        .collect()
                })
                .unwrap_or_default();
            let st = start.clamp(0, rows.len() as i64) as usize;
            let en = if old_end < 0 { rows.len() } else { (old_end as usize).clamp(st, rows.len()) };
            rows.splice(st..en, new);
        }
        rows.into_iter()
            .find(|row| row.iter().any(|(g, _)| g == "Sidebar.Selected"))
            .unwrap_or_default()
    }

    /// The panel row the cursor is on, read from the highlight rather than from a count.
    ///
    /// A count would be a test that passes for the wrong reason the moment the panel gains a row.
    fn sidebar_cursor(&self) -> Option<String> {
        let buf = self.buffer_named("[sidebar]")?;
        let text = self.lines_of(buf);
        let groups = self.groups_of(buf);
        groups
            .iter()
            .position(|row| row.iter().any(|g| g == "Sidebar.Selected"))
            .and_then(|i| text.get(i).cloned())
    }
}

/* -------------------------------------------------------------------------- */
/* What the plan has left                                                     */
/* -------------------------------------------------------------------------- */

/// A provider written as a plugin, reporting its own vendor's allowance two ways.
///
/// Both paths at once because they are the two halves of the rule: the mid-turn `Activity::Quota`
/// carries one window and must *patch*, and `quota.report` is complete and must *replace*. A
/// fixture that only exercised one of them would pass with the other wired backwards.
const PLANNER: &str = r#"
import type { PluginContext } from "@neosh/api";
const nap = (ms: number) => new Promise((r) => setTimeout(r, ms));

const win = (id: string, label: string, pct: number, active = false) => ({
  id, label, used_percent: pct, severity: pct >= 90 ? "critical" : "normal",
  active, resets_at: null, window_minutes: null, scope: null,
});

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("planner", [{
    id: "planner", driver: "planner", display_name: "Planner",
    // How it is drawn, declared by the plugin exactly as a bundled provider declares it. A group
    // name, never a colour: the theme owns what `Brand.Anthropic` looks like.
    brand: { mark: "\u2733", ascii: "A", hl: "Brand.Anthropic" },
    models: [{ id: "planner-1", display_name: "Planner One", context_window: 300000 }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "planner-1", usage: {} });
    // One window, mid-turn, before a word of the answer. Everything else must survive it.
    emit({ type: "activity", activity: {
      kind: "quota", windows: [win("weekly", "Weekly", 93, true)],
    } });
    await nap(600);
    emit({ type: "block_start", index: 0, block: { kind: "text" } });
    emit({ type: "text_delta", index: 0, text: "ok" });
    emit({ type: "block_stop", index: 0 });
    emit({ type: "message_delta", stop_reason: { kind: "end_turn" }, usage: {} });
    emit({ type: "message_stop" });
  }, { agentLoop: true });

  // The complete picture, published the way a plugin provider does it.
  await neosh.cmd.register("planner.publish", async () => {
    await neosh.quota.report({
      instance: "planner",
      plan: "flightplan",
      windows: [win("session", "Session", 12, true), win("weekly", "Weekly", 40)],
      credits: null,
      observed_at: Math.floor(Date.now() / 1000),
      source: "plugin",
    });
    neosh.notify("published");
  });

  neosh.event.on("neosh.ready", () => neosh.notify("planner ready"));
}
"#;

fn install_planner(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/planner");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"planner\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), PLANNER).expect("plugin");
}

/// A provider written as a plugin gets its plan drawn like anybody's.
///
/// The whole test of the extensibility claim for this feature: nothing in the sidebar has heard of
/// `planner`, and the strip draws its windows anyway — because it listens for the event rather than
/// knowing which vendors exist.
#[test]
fn a_plugins_own_provider_appears_in_the_plan_strip() {
    let sb = Sandbox::new("planstrip");
    install_planner(&sb);
    sb.write_config(
        "[options]\n\"agent.model\" = \"planner/planner-1\"\n\"usage.sidebar.style\" = \"full\"\n",
    );
    let mut s = sb.start_letting_config_choose();
    s.wait_for("planner ready");

    s.send(&command("planner.publish"));
    s.wait_for("published");

    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("PLAN"))),
        "the strip is a section like any other\n{:?}",
        s.sidebar_now()
    );
    let strip = s.sidebar_now().join("\n");
    assert!(strip.contains("Session"), "both windows, not just the worst:\n{strip}");
    assert!(strip.contains("Weekly"), "both windows, not just the worst:\n{strip}");
    assert!(strip.contains("12%"), "the vendor's own figures:\n{strip}");
    assert!(strip.contains("40%"), "the vendor's own figures:\n{strip}");

    // And it is on disk, because a percentage is only ever reported: a workspace that forgot the
    // last one opens with empty gauges and stays that way until something reports again — which on
    // a machine that is offline, signed out, or being told to ask less often is indefinitely.
    let saved = sb.root.join("state/quota.json");
    assert!(saved.is_file(), "the snapshot is kept, not only forwarded");
    let body = std::fs::read_to_string(&saved).expect("readable");
    assert!(body.contains("flightplan"), "with the plan it belongs to:\n{body}");
    assert!(
        std::fs::read_to_string(sb.root.join("state/quota.jsonl"))
            .is_ok_and(|h| h.contains("session")),
        "and the samples beside it, so a gauge can be a line"
    );
}

/// The plan is one row until you ask for more of it.
///
/// Three bars, a name and a sentence is five rows of a column whose job is your conversations —
/// and four of them answer a question you did not ask. The default is the limit that would refuse
/// the next request, plus anything else already critical; `<Tab>` on the row steps up to every
/// window and then to the whole block, and `^L` is all of it whichever is showing.
#[test]
fn the_plan_strip_is_one_row_until_you_ask_for_more() {
    let sb = Sandbox::new("plandense");
    install_planner(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"planner/planner-1\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("planner ready");
    s.send(&command("planner.publish"));
    s.wait_for("published");

    // The row is the account wearing the bar of the window the provider marked active — the one
    // that would refuse the next request. Its own label waits behind `⇥`: `Session` on a strip
    // with no account row above it says whose session to nobody. And `weekly` at 40% is not
    // news, so it is not a row.
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("Planner"))),
        "the account, wearing the limit that binds\n{:?}",
        s.sidebar_now()
    );
    let strip = s.sidebar_now().join("\n");
    assert!(!strip.contains("Session"), "the window's own label waits behind ⇥:\n{strip}");
    assert!(!strip.contains("Weekly"), "and a limit at 40% is not news:\n{strip}");
    assert!(!strip.contains("% left ·"), "nor is the sentence that explains it:\n{strip}");

    // Onto a plan row and `<Tab>`: every window, contributed as a verb on the rows it belongs to
    // rather than bound over every conversation in the panel.
    s.enter_panel();
    s.key("G");
    // On the key card beside the panel, which is where a row's verbs are said now — and only on
    // the plan row, which is what `custom:plan` means.
    assert!(
        s.pump(|s| s.key_card().iter().any(|l| l.contains("plan detail"))),
        "the key is advertised on the rows it applies to\n{:?}",
        s.key_card()
    );
    s.special("tab");
    assert!(
        s.pump(|s| s.sidebar_now().join("\n").contains("Weekly")),
        "every window, one step up\n{:?}",
        s.sidebar_now()
    );
    s.special("tab");
    assert!(
        s.pump(|s| s.sidebar_now().join("\n").contains("88% left")),
        "and the account and the sentence at the top of the ladder\n{:?}",
        s.sidebar_now()
    );
    s.special("tab");
    assert!(
        s.pump(|s| !s.sidebar_now().join("\n").contains("Weekly")),
        "and round to one row again\n{:?}",
        s.sidebar_now()
    );
}

/// The provider's mark keeps the provider's colour, on a row graded by something else.
///
/// The bar is coloured by how close the limit is, which is what a bar is for — and the one thing on
/// the row that says *whose* allowance it is has a colour of its own. Two accounts are then two
/// rows you tell apart without reading a word. It needs a span rather than a row highlight, so
/// `sidebar.section` rows carry spans: a contributed row that can only be one colour is a row a
/// plugin cannot put a mark on.
#[test]
fn the_plan_row_keeps_the_providers_own_colour() {
    let sb = Sandbox::new("planbrand");
    install_planner(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"planner/planner-1\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("planner ready");
    s.send(&command("planner.publish"));
    s.wait_for("published");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("Planner"))),
        "the strip is drawn\n{:?}",
        s.sidebar_now()
    );

    let buf = s.buffer_named("[sidebar]").expect("the panel buffer");
    let at = s
        .sidebar_now()
        .iter()
        .position(|l| l.contains("Planner"))
        .expect("the row with the binding limit on it");
    let groups = s.groups_of(buf);
    let on_row = groups.get(at).cloned().unwrap_or_default();
    assert!(
        on_row.iter().any(|g| g == "Brand.Anthropic"),
        "the mark carries the provider's group\n{on_row:?}"
    );
    assert!(
        on_row.iter().any(|g| g != "Brand.Anthropic"),
        "and the rest of the row is graded by how close the limit is\n{on_row:?}"
    );
}

/// Whose allowance it is, said on the strip.
///
/// Three bars and no name answers "how much is left" without ever saying *of what* — and on a
/// machine with a Claude plan and a Codex one it was two identical-looking stacks with nothing to
/// tell them apart. The account row carries the provider's display name and its plan, and the
/// provider here is one a plugin registered, so nothing in the sidebar or the usage plugin has
/// heard of it: the name is read off the credential list, like the model picker reads it.
#[test]
fn the_plan_strip_says_which_account_it_is_about() {
    let sb = Sandbox::new("planwho");
    install_planner(&sb);
    sb.write_config(
        "[options]\n\"agent.model\" = \"planner/planner-1\"\n\"usage.sidebar.style\" = \"full\"\n",
    );
    let mut s = sb.start_letting_config_choose();
    s.wait_for("planner ready");

    s.send(&command("planner.publish"));
    s.wait_for("published");

    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("Planner"))),
        "the account is named, not left as an instance id\n{:?}",
        s.sidebar_now()
    );

    // What the vendor calls the plan, and the way into the panel. Both are right-aligned virtual
    // text rather than buffer content, because only the frontend knows how wide the column is.
    let virt = s.sidebar_virt().join("|");
    assert!(virt.contains("flightplan"), "the plan's own name sits on the account row:\n{virt}");
    assert!(
        virt.contains("^L"),
        "and the heading says how to see the rest of it:\n{virt}"
    );

    // The one row here that is a sentence. Every other row is a measurement whose direction you
    // have to already know: a bar that fills as an allowance is *spent* reads as "how much is left"
    // to about half of everyone. `session` is at 12% and is the window the provider marked active,
    // so the strip says 88 — the number in the direction people ask the question.
    let strip = s.sidebar_now().join("\n");
    assert!(
        strip.contains("88% left"),
        "the binding window, in words and the right way round:\n{strip}"
    );
}

/// A mid-turn report patches; it does not replace.
///
/// `claude`'s `rate_limit_event` carries exactly one window and says nothing at all about the
/// others. Treated as a snapshot it deletes every other row — so the session limit vanishes from
/// the strip the first time the weekly one moves, which is precisely when somebody is watching it.
#[test]
fn a_mid_turn_report_does_not_erase_the_other_windows() {
    let sb = Sandbox::new("planpatch");
    install_planner(&sb);
    sb.write_config(
        "[options]\n\"agent.model\" = \"planner/planner-1\"\n\"usage.sidebar.style\" = \"full\"\n",
    );
    let mut s = sb.start_letting_config_choose();
    s.wait_for("planner ready");

    s.send(&command("planner.publish"));
    s.wait_for("published");
    assert!(s.pump(|s| s.sidebar_now().join("\n").contains("40%")), "the published weekly");

    // The turn reports the weekly window alone, at a different figure.
    s.type_text("go");
    s.enter();
    assert!(
        s.pump(|s| s.sidebar_now().join("\n").contains("93%")),
        "the driver's mid-turn figure lands\n{:?}",
        s.sidebar_now()
    );

    let strip = s.sidebar_now().join("\n");
    assert!(
        strip.contains("Session") && strip.contains("12%"),
        "and the window it said nothing about is still there:\n{strip}"
    );
    assert!(!strip.contains("40%"), "with the stale weekly figure replaced:\n{strip}");
}

/// The panel opens on its own key, with both halves in it.
///
/// Two blocks and never one axis: an opaque percentage and a token count do not convert into each
/// other, and the panel that drew them together would be inventing an exchange rate.
#[test]
fn the_usage_panel_opens_with_the_plan_above_the_history() {
    let sb = Sandbox::new("planpanel");
    install_planner(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"planner/planner-1\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("planner ready");
    s.send(&command("planner.publish"));
    s.wait_for("published");

    s.ctrl("l");
    assert!(
        s.pump(|s| s.buffer_named("[usage]").is_some()),
        "^L opens a buffer of its own\n{}",
        s.transcript()
    );
    let buf = s.buffer_named("[usage]").expect("the panel buffer");
    // Pumped on the *plan name*, not on the heading. The heading is drawn immediately, with
    // "nothing has reported an allowance yet" under it, while the transcript scan runs — so a test
    // that stopped at the heading would be reading the panel mid-load and asserting on the
    // placeholder.
    assert!(
        s.pump(|s| s.lines_of(buf).iter().any(|l| l.contains("flightplan"))),
        "the plan's own name, where the vendor gives one\n{:?}",
        s.lines_of(buf)
    );
    let text = s.lines_of(buf).join("\n");
    assert!(text.contains("YOUR PLAN"), "the gauges are the first thing in it:\n{text}");
    assert!(text.contains("Session") && text.contains("Weekly"), "every window:\n{text}");
    assert!(
        text.contains("THE LAST"),
        "and the history under it, as a separate block:\n{text}"
    );
}

/// The panel's keys are the panel's keys, bound to named commands.
///
/// `t` and `c` are ordinary letters. Bound at buffer-kind scope they only fire here — and because
/// they point at named commands rather than at a `switch` inside a handler, `^Z` lists them and an
/// `init.ts` can move them.
#[test]
fn the_usage_panel_swaps_metric_on_its_own_key() {
    let sb = Sandbox::new("planmetric");
    install_planner(&sb);
    // A month with exactly one answer in it, so the breakdown below has a row to draw. Read off a
    // transcript this test wrote, never off the developer's own.
    sb.write_usage_history("claude-opus-4-8", 2);
    sb.write_config("[options]\n\"agent.model\" = \"planner/planner-1\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("planner ready");

    s.ctrl("l");
    assert!(s.pump(|s| s.buffer_named("[usage]").is_some()), "the panel opens");
    let buf = s.buffer_named("[usage]").expect("the panel buffer");
    // Asserted on the breakdown's own figures, not on the heading: the metric is drawn there as
    // right-aligned virtual text, which is a mark rather than buffer text and so is not something
    // this harness can read back. The `$` is what a reader is actually looking at.
    assert!(
        s.pump(|s| s.lines_of(buf).iter().any(|l| l.contains("calls") && l.contains('$'))),
        "it opens on cost\n{:?}",
        s.lines_of(buf)
    );

    s.key("t");
    assert!(
        s.pump(|s| s.lines_of(buf).iter().any(|l| l.contains("calls") && !l.contains('$'))),
        "`t` is tokens\n{:?}",
        s.lines_of(buf)
    );
    s.key("c");
    assert!(
        s.pump(|s| s.lines_of(buf).iter().any(|l| l.contains("calls") && l.contains('$'))),
        "`c` is back to cost\n{:?}",
        s.lines_of(buf)
    );
}

/// A plugin may only publish for a driver it registered.
///
/// The strip is a thing people look at before deciding whether to spend an afternoon on something.
/// A figure any plugin could put under `claude-cli` is not one to draw as fact.
#[test]
fn a_plugin_cannot_publish_a_quota_for_somebody_elses_provider() {
    const LIAR: &str = r#"
import type { PluginContext } from "@neosh/api";
export async function activate({ neosh }: PluginContext) {
  await neosh.cmd.register("liar.try", async () => {
    try {
      await neosh.quota.report({
        instance: "claude-cli", plan: "free", windows: [], credits: null,
        observed_at: 1, source: "plugin",
      });
      neosh.notify("liar accepted");
    } catch (e) {
      neosh.notify(`liar refused: ${e}`);
    }
  });
  neosh.event.on("neosh.ready", () => neosh.notify("liar ready"));
}
"#;
    let sb = Sandbox::new("planliar");
    let dir = sb.root.join("config/plugins/liar");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"liar\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), LIAR).expect("plugin");

    let mut s = sb.start();
    s.wait_for("liar ready");
    s.send(&command("liar.try"));
    assert!(s.pump(|s| s.saw("liar refused")), "refused, by name\n{}", s.transcript());
    assert!(!s.saw("liar accepted"), "and not accepted\n{}", s.transcript());
}

#[test]
fn a_conversation_waiting_on_an_answer_is_marked_in_the_panel() {
    // A conversation that has asked you something looks *exactly* like one that is thinking: the
    // turn is still in flight, blocked on the hook, so the row is a spinner and the only way to
    // find it is to open conversations until one of them asks. The footer can only say how many —
    // it is one line and it is shared — so the panel is the only place that can say which.
    //
    // The two halves are both under test: the `questions` plugin writing `question.asking`, and
    // this panel reading it. They are separate plugins and neither imports the other, which is the
    // point — a question panel of your own writes the same var and this row still lights up.
    const ASKER: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.cmd.register("t.ask", async () => {
    const answers = await neosh.ask([{
      question: "Tabs or spaces?",
      header: "Indentation",
      multi_select: false,
      options: [
        { label: "Tabs", description: "tab characters" },
        { label: "Spaces", description: "space characters" },
      ],
    }]);
    neosh.notify(`answered: ${answers === null ? "nothing" : JSON.stringify(answers)}`);
  });
  // To the other conversation if there is one, to a new one if there is not.
  await neosh.cmd.register("t.switch", async () => {
    const list = await neosh.session.list();
    const other = list.find((x) => !x.is_active);
    if (other) await neosh.session.switch(other.id);
    else await neosh.session.create({ activate: true });
  });
  neosh.event.on("neosh.ready", () => neosh.notify("asker ready"));
}
"#;
    let sb = Sandbox::new("asking-row");
    let dir = sb.root.join("config/plugins/asker");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"asker\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), ASKER).expect("plugin");

    let mut s = sb.start();
    s.wait_for("asker ready");
    s.wait_for("PROJECTS");

    s.send(&command("t.ask"));
    s.wait_for("Tabs or spaces?");
    // Away, so the question is one you cannot see. This is the whole case: on screen it is a panel
    // over the composer and nothing needs saying twice.
    s.send(&command("t.switch"));
    s.wait_for("conversation is asking");

    let marked = |s: &Session| {
        s.sidebar_now().iter().filter(|l| l.trim_start().starts_with("? ")).count()
    };
    assert!(
        s.pump(|s| marked(s) == 1),
        "one row says a question is waiting on it\n{:?}",
        s.sidebar_now()
    );
    // And in the colour the palette keeps for waiting on something outside the program — the same
    // one the footer's `Question.Waiting` links to. A glyph nobody can pick out of a column of
    // thirty is a glyph that is not there.
    let buf = s.buffer_named("[sidebar]").expect("sidebar buffer");
    assert!(
        s.groups_of(buf).iter().flatten().any(|g| g == "Status.Pending"),
        "the row is coloured, not only marked\n{:?}",
        s.groups_of(buf)
    );

    // Answering is what takes it off. Nothing to dismiss, and nothing left behind on a row that is
    // not waiting on anybody any more.
    //
    // Counted rather than waited for: the question has already been on screen once, so anything
    // scanning everything ever drawn is satisfied before the panel has come back — and the key
    // would land in the composer of the conversation being switched to.
    let drawn = |s: &Session| s.texts().iter().filter(|l| l.contains("Tabs or spaces?")).count();
    let quiet = drawn(&s);
    s.send(&command("t.switch"));
    assert!(
        s.pump(move |x| drawn(x) > quiet + 1),
        "the question is in front of you again\n{}",
        s.transcript()
    );
    s.key("2");
    s.wait_for(r#""answers":["Spaces"]"#);
    assert!(
        s.pump(|s| marked(s) == 0),
        "and the mark goes with the answer\n{:?}",
        s.sidebar_now()
    );
}

/// The row you are in says all of its title; every other row waits for the cursor.
///
/// A conversation's title is the only thing telling two of them in one project apart, and a
/// generated title is a sentence rather than a word — so a 34-column panel cuts it at about the
/// point where it was going to say which of them this is. The conversation you are *in* is not a
/// row you are considering, it is where you are, so it stays open with nothing on it. Everywhere
/// else the cursor is what asks, and leaving folds the row back — which is what keeps the column
/// something you read down.
#[test]
fn the_row_you_are_in_says_all_of_a_title_the_column_had_to_cut() {
    let sb = Sandbox::new("unfold");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    let title = "a conversation whose title is longer than the panel it has to fit inside";
    for c in title.chars() {
        s.key(&c.to_string());
    }
    s.enter();
    s.wait_for("longer than the panel");

    // Cut, first of all — otherwise this test would pass on a panel that never had the problem.
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains('…'))),
        "the title does not fit the column\n{:?}",
        s.sidebar_now()
    );
    // And open, with the cursor nowhere near it and the panel not even focused: this is the
    // conversation on screen, and abbreviating the one row you are certain about is the one place
    // the cursor rule is wrong.
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("has to fit inside"))),
        "the conversation you are in says its whole name\n{:?}",
        s.sidebar_now()
    );

    // Somewhere else. The row that was open is now an ordinary one, and goes back to a line.
    s.new_conversation();
    assert!(
        s.pump(|s| !s.sidebar_now().iter().any(|l| l.contains("has to fit inside"))),
        "a row nobody is in or on stays one row\n{:?}",
        s.sidebar_now()
    );

    // Onto it with the cursor, and it says the rest of itself again. The panel keeps the cursor on
    // the conversation you are in, and the list is newest first — so the older one is the row
    // below where the cursor lands.
    s.enter_panel();
    s.key("j");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("has to fit inside"))),
        "the row under the cursor says the rest of itself\n{:?}",
        s.sidebar_now()
    );

    // Off it again, and it folds away.
    s.key("k");
    assert!(
        s.pump(|s| !s.sidebar_now().iter().any(|l| l.contains("has to fit inside"))),
        "and folds back when the cursor leaves\n{:?}",
        s.sidebar_now()
    );
}

/// A plugin can drive a conversation it is not looking at.
///
/// The vocabulary for doing something *to* a conversation — steer it, interrupt it, re-model it,
/// start one — existed only over the swarm: `swarm.command` names a node and a session, so a
/// plugin could fan work out over every other machine and, on this one, had `agent.send`, which
/// takes no session and acts on whatever is on screen. Watching was already per-conversation, so
/// the local API could see every conversation in the workspace and drive exactly one of them.
///
/// That is the shape of an orchestrator, so it is asserted as one: two conversations are given
/// different work by id, and the screen never moves. `sessions.switch` before each message is the
/// thing this replaced — it works, and it drags the transcript out from under whoever is reading.
#[test]
fn a_plugin_can_run_a_conversation_it_is_not_looking_at() {
    const FANOUT: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.cmd.register("lab.fanout", async () => {
    try {
    const here = (await neosh.session.current()).id;
    // Started by name, and the answer is the one that was made — not whatever is on screen.
    const made = await neosh.agent.command({ command: "new_session", title: "over there" });
    if (!made) { neosh.notify("no session came back"); return; }
    if (made === here) { neosh.notify("it answered with the conversation on screen"); return; }
    await neosh.agent.command({ command: "send", text: "work for the second" }, made);
    await neosh.agent.command({ command: "send", text: "work for the first" }, here);
    neosh.notify("fanned out");
    } catch (e) { neosh.notify("fanout failed: " + e); }
  });
  neosh.event.on("neosh.ready", () => neosh.notify("lab ready"));
}
"#;
    let sb = Sandbox::new("fanout");
    let dir = sb.root.join("config/plugins/lab");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"lab\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), FANOUT).expect("plugin");
    install_parrot(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"parrot/parrot\"\n");

    let mut s = sb.start_letting_config_choose();
    s.wait_for("parrot ready");
    s.wait_for("lab ready");

    // The conversation on screen, so there is something to have stayed on.
    s.type_text("first");
    s.enter();
    assert!(s.pump(|s| s.saw("asked: first")), "the conversation on screen answers\n{}", s.transcript());

    s.send(&command("lab.fanout"));
    s.wait_for("fanned out");

    // Both turns ran. The second conversation is not on screen, so its answer is asserted through
    // the driver rather than the transcript — which is the point: nothing about it was drawn.
    assert!(
        s.pump(|s| s.saw("asked: work for the first")),
        "the conversation on screen was steered by id\n{}",
        s.transcript()
    );
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("over there"))),
        "the conversation that was started is in the list\n{:?}",
        s.sidebar_now()
    );
    // And the screen never moved: what is in the chat buffer is still the first conversation's.
    assert!(
        s.chat_now().iter().any(|l| l.contains("first")),
        "the transcript on screen is still the one that was there\n{}",
        s.transcript()
    );
    assert!(
        !s.chat_now().iter().any(|l| l.contains("work for the second")),
        "the other conversation's work was drawn into this one's transcript\n{}",
        s.transcript()
    );
}


/// A plugin decides which model answers.
///
/// Every other hook is about a turn already under way — what the prompt says, which tool may run,
/// what it cost. None of them could answer the question an orchestrator exists to answer: *who
/// should do this one*. Routing policy could only be written in Rust, in a program whose whole
/// claim is that a third party could have written the feature.
///
/// Asserted through the driver, because that is the only witness that cannot be faked by the UI:
/// the turn is sent to an instance the conversation was never pointed at.
#[test]
fn a_plugin_can_choose_which_model_answers_a_turn() {
    const ROUTER: &str = r#"
import type { PluginContext, HookPayload } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  const say = (who: string) => async (_req: unknown, emit: (e: unknown) => void) => {
    emit({ type: "message_start", model: who, usage: {} });
    emit({ type: "block_start", index: 0, block: { kind: "text" } });
    emit({ type: "text_delta", index: 0, text: "answered by " + who });
    emit({ type: "block_stop", index: 0 });
    emit({ type: "message_delta", stop_reason: { kind: "end_turn" }, usage: {} });
    emit({ type: "message_stop" });
  };
  await neosh.provider.register("small", [{
    id: "small", driver: "small", display_name: "Small",
    models: [{ id: "small", display_name: "Small" }],
  }], say("small"));
  await neosh.provider.register("big", [{
    id: "big", driver: "big", display_name: "Big",
    models: [{ id: "big", display_name: "Big" }],
  }], say("big"));

  await neosh.hook.register("turn_route", (p: HookPayload) => {
    if (p.hook !== "turn_route") return { action: "continue" };
    if (p.text.includes("too expensive")) {
      return { action: "veto", reason: "over the budget for today" };
    }
    if (!p.text.includes("urgent")) return { action: "continue" };
    // Re-point this one turn. The conversation was never switched to it.
    return {
      action: "modify",
      payload: { ...p, selection: { instance: "big", model: "big", options: [] } },
    };
  }, { blocking: true });

  neosh.event.on("neosh.ready", () => neosh.notify("router ready"));
}
"#;
    let sb = Sandbox::new("router");
    let dir = sb.root.join("config/plugins/lab");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"lab\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\", \"hooks_blocking\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), ROUTER).expect("plugin");
    sb.write_config("[options]\n\"agent.model\" = \"small/small\"\n");

    let mut s = sb.start_letting_config_choose();
    s.wait_for("router ready");

    // Ordinary work goes where the conversation points.
    s.type_text("something ordinary");
    s.enter();
    assert!(s.pump(|s| s.saw("answered by small")), "the unrouted turn\n{}", s.transcript());

    // And the router sends this one somewhere else, without the conversation being re-pointed by
    // hand and without anything on screen having been switched.
    s.type_text("this one is urgent");
    s.enter();
    assert!(s.pump(|s| s.saw("answered by big")), "the routed turn\n{}", s.transcript());

    // And a router that says no says why, *in the transcript*, under the question it refused.
    // A veto used to end the turn as `Cancelled` — the value `<Esc>` produces — so a question a
    // plugin had blocked was indistinguishable from one the person had interrupted, and the reason
    // the plugin gave lived for a few seconds in a toast and then nowhere at all.
    s.type_text("this one is too expensive");
    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("over the budget for today"))),
        "the reason is in the transcript\n{}",
        s.transcript()
    );
}

/// The rest of the declared permissions are enforced too.
///
/// `vcs_write` was the only one ever checked, which made the other seven documentation shaped like
/// a boundary: `neosh plugins` printed a manifest's permissions, and a plugin that declared none of
/// them could still register a tool the model calls, take a blocking hook and refuse every turn, or
/// stand in as a model provider. A workspace that runs other people's code — and, on the swarm,
/// takes commands from other machines — cannot have a permission list that is only a description.
///
/// One plugin, three attempts, nothing declared. Reading and drawing stay free, because they are no
/// more privileged than what the person sitting there can already do.
#[test]
fn a_plugin_that_declared_nothing_cannot_register_a_tool_a_provider_or_a_blocking_hook() {
    let sb = Sandbox::new("undeclared");
    let dir = sb.root.join("config/plugins/greedy");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"greedy\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("main.ts"),
        r#"
import type { PluginContext } from "@neosh/api";
export async function activate({ neosh }: PluginContext) {
  const tried = async (what: string, f: () => Promise<unknown>) => {
    try { await f(); neosh.notify(`ALLOWED ${what}`); }
    catch (e) { neosh.notify(`refused ${what}: ${e}`); }
  };
  await tried("tool", () =>
    neosh.tool.register({ name: "t", description: "d", inputSchema: {} }, async () => ({
      content: "x", is_error: false,
    })));
  await tried("provider", () =>
    neosh.provider.register("x", [{
      id: "x", driver: "x", display_name: "X", models: [{ id: "x", display_name: "X" }],
    }], async () => {}));
  await tried("blocking hook", () =>
    neosh.hook.register("turn_pre", () => ({ action: "continue" }), { blocking: true }));
  // An observer cannot refuse anything, which is exactly what `hooks_blocking` distinguishes — so
  // this one needs nothing declared and must still work.
  await tried("observing hook", () =>
    neosh.hook.register("turn_post", () => ({ action: "continue" })));
  neosh.notify("greedy done");
}
"#,
    )
    .expect("plugin");

    let mut s = sb.start();
    s.wait_for("greedy done");
    assert!(!s.saw("ALLOWED tool"), "a tool was registered undeclared\n{}", s.transcript());
    assert!(!s.saw("ALLOWED provider"), "a provider was registered undeclared\n{}", s.transcript());
    assert!(
        !s.saw("ALLOWED blocking hook"),
        "a blocking hook was taken undeclared\n{}",
        s.transcript()
    );
    assert!(
        s.saw("ALLOWED observing hook"),
        "an observer needs nothing declared and was refused anyway\n{}",
        s.transcript()
    );
    // And each refusal says the word to put in the manifest, so the fix is one line rather than a
    // capability name and a guess about where it goes.
    for want in ["tools", "providers", "hooks_blocking"] {
        assert!(s.saw(want), "the refusal names {want}\n{}", s.transcript());
    }
}

// ---------------------------------------------------------------------------
// Plugins on top of plugins
// ---------------------------------------------------------------------------

/// A plugin that builds on another: `requires` it, imports it by name, and asks it a question
/// through `cmd.call`. Everything a decorator of somebody else's panel needs, end to end through
/// the real host.
const ON_TOP: &str = r#"
import type { PluginContext } from "@neosh/api";
import * as sidebar from "plugin:sidebar";

export async function activate({ neosh }: PluginContext) {
  await neosh.cmd.register("ontop.answer", (args) => ({ echoed: args, from: "ontop" }));
  neosh.event.on("neosh.ready", async () => {
    const imported = typeof sidebar.activate === "function" ? "imports sidebar" : "no sidebar";
    const self = await neosh.cmd.call<{ echoed: string[]; from: string }>("ontop.answer", ["x"]);
    const host = await neosh.cmd.call("sidebar.refresh");
    let missing = "";
    try { await neosh.cmd.call("nothing.here"); } catch (e) { missing = e instanceof Error ? e.message : String(e); }
    neosh.notify(`${imported}; self=${self.from}:${self.echoed.join(",")}; host=${host === null}; missing=${missing.includes("nothing.here")}`);
  });
}
"#;

fn install_plugin(sb: &Sandbox, name: &str, manifest_extra: &str, source: &str) {
    let dir = sb.root.join("config/plugins").join(name);
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        format!("name = \"{name}\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\n{manifest_extra}"),
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), source).expect("plugin");
}

#[test]
fn a_plugin_can_require_import_and_ask_another_plugin() {
    let sb = Sandbox::new("ontop");
    install_plugin(&sb, "ontop", "requires = [\"sidebar\"]\n", ON_TOP);
    let mut s = sb.start();
    s.wait_for("imports sidebar; self=ontop:x; host=true; missing=true");
}

/// Requiring something that is not there is said at startup, with the name, and the rest of the
/// workspace loads anyway.
#[test]
fn a_missing_requirement_is_reported_and_does_not_stop_the_rest() {
    let sb = Sandbox::new("ontop-missing");
    install_plugin(&sb, "ontop", "requires = [\"no-such-plugin\"]\n", ON_TOP);
    let mut s = sb.start();
    s.wait_for("requires \"no-such-plugin\"");
    s.wait_for("PROJECTS");
}

/// `plugins.disabled` applies to a plugin you installed, not only to the bundled ones.
#[test]
fn a_third_party_plugin_can_be_switched_off_from_config() {
    let sb = Sandbox::new("disable-third");
    install_plugin(&sb, "ontop", "requires = [\"sidebar\"]\n", ON_TOP);
    sb.write_config("[options]\n\"plugins.disabled\" = [\"ontop\"]\n");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.drain_for(Duration::from_secs(2));
    assert!(!s.saw("imports sidebar"), "the plugin should not have run\n{}", s.transcript());
}

/// A plugin puts a mark on a row the sidebar drew — the git plugin's stat strip on the project
/// row — without the sidebar knowing git exists. The badge sits after the name, in the row.
///
/// Two changes of two different kinds, because the strip counts them separately: a tracked file
/// somebody edited is `~1` and a file git has never seen is `?1`. A single count could not tell
/// them apart, which is the whole reason the badge is a run of parts rather than one.
#[test]
fn a_decoration_lands_on_the_project_row() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("decorate");
    sb.git_init();
    std::fs::write(sb.work().join("new.txt"), "x\n").expect("write");
    std::fs::write(sb.work().join("README.md"), "changed\n").expect("write");
    let mut s = sb.start();
    s.wait_for("work ~1 ?1");
}

/// The badge gives way before the name does.
///
/// A project name is what the row *is*; the strip is news about it. So in a column too narrow for
/// both, the git plugin's `short` form — the worst single stat — is what gets drawn, and the name
/// is left whole. Before this the badge took its columns first and the row read
/// `neosh-websit… ↓3 ↑12 ~5 ?7`: four facts nobody asked for, bought with the one thing the panel
/// exists to show.
///
/// At the narrowest the panel goes, which is where the rule has to hold if it holds anywhere.
#[test]
fn a_badge_gives_up_its_columns_before_the_name_is_clipped() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("badge-yields");
    sb.write_config("[options]\n\"sidebar.width\" = 16\n");
    sb.git_init();
    for i in 0..4 {
        std::fs::write(sb.work().join(format!("u{i}.txt")), "x\n").expect("write");
    }
    std::fs::write(sb.work().join("README.md"), "changed\n").expect("write");
    let mut s = sb.start();
    // One tracked file edited and four git has never seen: `~1 ?4` in full, `~1` short. The row
    // keeps its name and the head of the strip.
    s.wait_for("work ~1");
    s.drain_for(Duration::from_secs(2));
    // The footer says the whole of it — it has the room — so this has to name the row rather than
    // the count, or it asserts against the status bar instead of the panel.
    assert!(!s.saw("work ~1 ?4"), "the row should not carry the full strip\n{}", s.transcript());
}

/// A third-party decoration: a badge on a conversation and a section anchored by name between
/// two of the panel's own blocks — neither of which the panel had to be told about.
const DECORATOR: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  neosh.event.on("neosh.ready", async () => {
    const here = await neosh.session.current();
    await neosh.ext.contribute("sidebar.decoration", "mine", {
      target: { session: here.id },
      badge: { text: "2 PRs", hl: "Accent" },
    });
    await neosh.ext.contribute("sidebar.section", "between", {
      title: "BETWEEN",
      before: "add",
      rows: [{ text: "anchored here" }],
    });
    const cursor = await neosh.cmd.call<{ kind: string } | null>("sidebar.cursor");
    neosh.notify(`cursor=${cursor === null ? "none" : cursor.kind}`);
  });
}
"#;

#[test]
fn a_third_party_decorates_a_conversation_and_anchors_a_section_by_name() {
    let sb = Sandbox::new("decorate-third");
    install_plugin(&sb, "deco", "after = [\"sidebar\"]\n", DECORATOR);
    let mut s = sb.start();
    s.wait_for("2 PRs");
    s.wait_for("anchored here");
    s.wait_for("cursor=");
    // The section sits between the project list and the `+ Add project` row, where `before:
    // "add"` put it — not at the foot, where `at: "below"` would have.
    let texts = s.texts();
    let between = texts.iter().rposition(|t| t.contains("anchored here")).expect("drawn");
    let add = texts.iter().rposition(|t| t.contains("+ Add project")).expect("drawn");
    let projects = texts.iter().rposition(|t| t.contains("PROJECTS")).expect("drawn");
    assert!(projects < between && between < add, "order: projects={projects} between={between} add={add}");
}

/// A theme is a plugin: contributed on `ui.theme`, listed beside `dark` and `light`, chosen with
/// the option, and gone — with the workspace back on a built-in — when the plugin is.
const THEME: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.ext.contribute("ui.theme", "gruvbox", {
    base: "dark",
    groups: {
      Comment: { fg: { kind: "rgb", r: 1, g: 2, b: 3 } },
      "Gruv.Extra": { link: "Normal" },
    },
  });
  neosh.event.on("neosh.ready", async () => {
    const entry = await neosh.opt.entry("ui.theme");
    const values = entry?.type.type === "enum" ? entry.type.values.join(",") : "?";
    neosh.notify(`themes: ${values}`);
    await neosh.opt.set("ui.theme", "gruvbox");
    const comment = await neosh.hl.get("Comment");
    const extra = await neosh.hl.get("Gruv.Extra");
    neosh.notify(`comment=${JSON.stringify(comment.resolved?.fg)} extra=${extra.resolved !== null}`);
    // A plugin's own definition still wins over the theme's, and leaves when reset.
    await neosh.hl.define("Comment", { link: "Accent" });
    const mine = await neosh.hl.get("Comment");
    await neosh.hl.reset("Comment");
    const back = await neosh.hl.get("Comment");
    neosh.notify(`mine=${mine.def?.kind === "link"} back=${JSON.stringify(back.resolved?.fg)}`);
  });
}
"#;

#[test]
fn a_theme_is_a_plugin() {
    let sb = Sandbox::new("themepoint");
    install_plugin(&sb, "gruvbox", "", THEME);
    let mut s = sb.start();
    s.wait_for("themes: dark,light,gruvbox");
    s.wait_for("comment={\"kind\":\"rgb\",\"r\":1,\"g\":2,\"b\":3} extra=true");
    s.wait_for("mine=true back={\"kind\":\"rgb\",\"r\":1,\"g\":2,\"b\":3}");
}

/// Recolouring one panel's window: a remap on the sidebar's *kind* reaches the window it is drawn
/// in, and the frontend is told the whole map.
const RESTYLE: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  neosh.event.on("neosh.ready", async () => {
    await neosh.hl.define("Acme.Panel", { bg: { kind: "rgb", r: 9, g: 9, b: 9 } });
    // By *kind*, deliberately before the sidebar has opened a window in any terminal: the remap
    // has to reach windows opened later, or a restyling plugin would have to race every attach.
    await neosh.win.setHighlights({ kind: "neosh.sidebar" }, { Normal: "Acme.Panel" });
    neosh.notify("restyled");
  });
}
"#;

#[test]
fn a_kind_remap_reaches_the_sidebars_window() {
    let sb = Sandbox::new("restyle");
    install_plugin(&sb, "restyle", "", RESTYLE);
    let mut s = sb.start();
    s.wait_for("restyled");
    // The sidebar's window may open after the remap was set — a view attaching is what opens a
    // panel now — and the point of a *kind* remap is that it reaches those too.
    assert!(
        s.pump(|s| {
            let Some(sidebar) = s.buffer_named("[sidebar]") else { return false };
            let Some(win) = s
                .events
                .iter()
                .find(|e| e["type"] == "window_opened" && e["buf"].as_u64() == Some(sidebar))
                .and_then(|e| e["win"].as_u64())
            else {
                return false;
            };
            s.events.iter().any(|e| {
                e["type"] == "window_highlights"
                    && e["win"].as_u64() == Some(win)
                    && e["map"]["Normal"] == "Acme.Panel"
            })
        }),
        "the sidebar's window was told its remap\n{}",
        s.transcript()
    );
}

/// The workspace's own events — windows gaining and losing the keyboard, the mode changing —
/// arrive on the bus a plugin already listens to, with the buffer kind to filter on; a window
/// var lives in memory and goes with its window, and whoever was watching is told.
const AUTOCMD: &str = r#"
import type { PluginContext, WindowEvent } from "@neosh/api";

export async function activate({ neosh, subscriptions }: PluginContext) {
  subscriptions.push(neosh.event.on("neosh.win.enter", (e) => {
    const w = e.data as WindowEvent;
    neosh.notify(`entered ${w.kind}`);
  }, { kind: "neosh.sidebar" }));
  subscriptions.push(neosh.event.on("neosh.win.leave", (e) => {
    const w = e.data as WindowEvent;
    neosh.notify(`left ${w.kind}`);
  }));
  subscriptions.push(neosh.event.on("neosh.mode", (e) => {
    neosh.notify(`mode ${JSON.stringify(e.data)}`);
  }));
  subscriptions.push(neosh.vars.onChange((e) => {
    if (e.scope.scope === "window") neosh.notify(`winvar ${e.key}=${JSON.stringify(e.value)}`);
  }));
  await neosh.cmd.register("autocmd.winvar", async () => {
    const [w] = await neosh.win.ofKind("neosh.sidebar");
    if (!w) return;
    await neosh.vars.set({ scope: "window", win: w.win }, "acme.note", "hello");
    const back = await neosh.vars.get({ scope: "window", win: w.win }, "acme.note");
    neosh.notify(`winvar read ${back}`);
  });
  await neosh.keymap.set("chat", "<C-y>", "autocmd.winvar");
}
"#;

#[test]
fn the_workspaces_own_events_are_on_the_bus() {
    let sb = Sandbox::new("autocmd");
    install_plugin(&sb, "autocmd", "", AUTOCMD);
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("y");
    s.wait_for("winvar acme.note=\"hello\"");
    s.wait_for("winvar read hello");
    s.ctrl("t");
    s.wait_for("entered neosh.sidebar");
    s.send(r#"{"type":"key","key":{"code":{"kind":"esc"},"mods":{}}}"#);
    s.wait_for("left neosh.sidebar");
    s.ctrl("b");
    // Closing the sidebar's window drops its vars, and says so.
    s.wait_for("winvar acme.note=undefined");
}

/// A contribution to a point nobody reads is said at startup, with the nearest real point.
const TYPO: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.ext.contribute("sidebar.sektion", "mine", { title: "X", rows: [] });
  neosh.event.on("neosh.ready", async () => {
    const points = await neosh.ext.points();
    const section = points.find((p) => p.point === "sidebar.section");
    neosh.notify(`readers of sidebar.section: ${section?.readers.join(",")}`);
  });
}
"#;

#[test]
fn a_contribution_to_a_point_nothing_reads_is_reported() {
    let sb = Sandbox::new("typo-point");
    install_plugin(&sb, "typo", "", TYPO);
    let mut s = sb.start();
    s.wait_for("typo contributes to \"sidebar.sektion\", which no plugin declares it reads — did you mean \"sidebar.section\"?");
    s.wait_for("readers of sidebar.section: sidebar");
}

/// A plugin with `[activation]` is not loaded at startup. Its command exists from the first press
/// — registered on its behalf — and the press that wakes it is replayed once it is up, with
/// `neosh.ready` delivered to it even though the workspace was ready long before.
const LAZY_PLUGIN: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  neosh.notify("lazy activated");
  await neosh.cmd.register("lazy.hello", (args) => neosh.notify(`lazy hello ${args.join(" ")}`));
  neosh.event.on("neosh.ready", () => neosh.notify("lazy saw ready"));
}
"#;

#[test]
fn a_lazy_plugin_loads_on_its_command_and_the_press_is_replayed() {
    let sb = Sandbox::new("lazy");
    install_plugin(
        &sb,
        "lazy",
        "[activation]\non_command = [\"lazy.hello\"]\n",
        LAZY_PLUGIN,
    );
    std::fs::write(
        sb.root.join("config/init.ts"),
        r#"import type { PluginContext } from "@neosh/api";
export async function activate({ neosh }: PluginContext) {
  await neosh.keymap.set("chat", "<C-y>", "lazy.hello");
}
"#,
    )
    .expect("init.ts");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.drain_for(Duration::from_secs(1));
    assert!(!s.saw("lazy activated"), "held at startup\n{}", s.transcript());
    s.ctrl("y");
    s.wait_for("lazy activated");
    s.wait_for("lazy saw ready");
    s.wait_for("lazy hello");
}

/// `on_event` and `on_kind` wake a plugin too — here, a buffer kind another plugin creates.
#[test]
fn a_lazy_plugin_loads_when_its_kind_appears() {
    let sb = Sandbox::new("lazy-kind");
    install_plugin(
        &sb,
        "lazy",
        "[activation]\non_kind = [\"acme.thing\"]\n",
        LAZY_PLUGIN,
    );
    install_plugin(
        &sb,
        "maker",
        "",
        r#"import type { PluginContext } from "@neosh/api";
export async function activate({ neosh }: PluginContext) {
  await neosh.cmd.register("maker.make", async () => { await neosh.buf.create({ kind: "acme.thing" }); });
  await neosh.keymap.set("chat", "<C-y>", "maker.make");
}
"#,
    );
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.drain_for(Duration::from_millis(500));
    assert!(!s.saw("lazy activated"), "held at startup\n{}", s.transcript());
    s.ctrl("y");
    s.wait_for("lazy activated");
}

/// A third-party panel on `ListPanel` gets the whole surface for a kind and a `rows` function:
/// named verbs at `buf_kind` scope, contribution points, decorations, a published cursor. A
/// second plugin that has never heard of it decorates a row and puts a key on the panel.
const TASKS_PANEL: &str = r#"
import type { PluginContext } from "@neosh/api";
import { ListPanel } from "@neosh/api/ui";

export async function activate({ neosh, subscriptions }: PluginContext) {
  const panel = await ListPanel.create<{ id: string; title: string }>(neosh, {
    kind: "acme.tasks",
    dock: "right",
    size: () => 30,
    rows: () => [
      { text: " TASKS", hl: "Sidebar.Heading", inert: true },
      { text: " ▸ write the docs", value: { id: "t1", title: "write the docs" } },
      { text: " ▸ ship it", value: { id: "t2", title: "ship it" } },
    ],
    key: (v) => ({ task: v.id }),
    kindOf: () => "task",
    onOpen: (v) => neosh.notify(`opened ${v.title}`),
  });
  subscriptions.push({ dispose: () => panel.dispose() });
  subscriptions.push(neosh.event.on("acme.tasks.cursor", (e) => {
    const v = e.data as { id: string } | null;
    neosh.notify(`tasks cursor ${v?.id ?? "none"}`);
  }));
  await neosh.keymap.set("chat", "<C-y>", "acme.tasks.focus");
}
"#;

const TASKS_DECORATOR: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.ext.contribute("acme.tasks.decoration", "due", {
    target: { task: "t2" },
    badge: { text: "due today", hl: "Status.Unread" },
  });
  await neosh.cmd.register("deco.done", (args) => neosh.notify(`done ${args.join(" ")}`));
  await neosh.ext.contribute("acme.tasks.action", "done", {
    key: "x", label: "mark done", command: "deco.done", on: "task",
  });
}
"#;

#[test]
fn a_third_party_panel_on_list_panel_is_a_surface() {
    let sb = Sandbox::new("listpanel");
    install_plugin(&sb, "tasks", "[provides]\nkinds = [\"acme.tasks\"]\npoints = [\"acme.tasks.section\", \"acme.tasks.action\", \"acme.tasks.decoration\"]\n", TASKS_PANEL);
    install_plugin(&sb, "deco", "after = [\"tasks\"]\n", TASKS_DECORATOR);
    let mut s = sb.start();
    s.wait_for("ship it due today");
    s.ctrl("y");
    s.wait_for("tasks cursor t1");
    s.key("j");
    s.wait_for("tasks cursor t2");
    s.key("x");
    s.wait_for("done task t2");
    s.send(r#"{"type":"key","key":{"code":{"kind":"enter"},"mods":{}}}"#);
    s.wait_for("opened ship it");
}

/// The plugins panel: every plugin and what became of it, then what one registered.
#[test]
fn the_plugins_panel_lists_what_loaded_and_what_each_registered() {
    let sb = Sandbox::new("plugins-panel");
    install_plugin(&sb, "lazy", "[activation]\non_command = [\"lazy.hello\"]\n", LAZY_PLUGIN);
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("k");
    assert!(s.pump(|s| !s.picker_named("[Go to]").is_empty()), "the palette opened");
    s.type_text("plugins.list");
    s.wait_for("plugins.list");
    s.send(r#"{"type":"key","key":{"code":{"kind":"enter"},"mods":{}}}"#);
    assert!(s.pump(|s| !s.picker_named("[Plugins]").is_empty()), "the plugins panel opened");
    s.wait_for("held");
    s.type_text("sidebar");
    s.send(r#"{"type":"key","key":{"code":{"kind":"enter"},"mods":{}}}"#);
    s.wait_for("points read");
    s.wait_for("sidebar.decoration");
    s.wait_for("neosh.sidebar");
}

// ---------------------------------------------------------------------------
// Panes and tabs
// ---------------------------------------------------------------------------
//
// The layout arithmetic is unit-tested in `neosh_core::panes` and the rectangles in
// `neosh-tui`'s `rendering`. What is left for a whole workspace to answer is the part neither of
// those can see: that splitting produces a *second of everything a conversation needs* — its own
// transcript, its own composer, its own draft — and that closing one takes all of it away again.

impl Session {
    /// Press the window prefix and then one key, which is what `<C-w>v` is.
    fn window_key(&mut self, k: &str) {
        self.ctrl("w");
        self.key(k);
    }

    /// Every buffer whose name starts with `[chat]`, which is one per pane.
    fn chat_buffers(&self) -> Vec<u64> {
        let mut out: Vec<u64> = self
            .events
            .iter()
            .filter(|e| {
                e["type"] == "buffer_opened"
                    && e["name"].as_str().is_some_and(|n| n.starts_with("[chat]"))
            })
            .filter_map(|e| e["buf"].as_u64())
            .collect();
        // A buffer that has since been closed is not a pane. Splitting and unsplitting all
        // afternoon would otherwise look like a workspace with thirty panes in it.
        let closed: Vec<u64> = self
            .events
            .iter()
            .filter(|e| e["type"] == "buffer_closed")
            .filter_map(|e| e["buf"].as_u64())
            .collect();
        out.retain(|b| !closed.contains(b));
        out.dedup();
        out
    }

    fn tabline_now(&self) -> String {
        match self.buffer_named("[tabs]") {
            Some(b) => self.lines_of(b).join(""),
            None => String::new(),
        }
    }
}

/// Splitting gives you a second conversation surface, not a second view of one buffer.
#[test]
fn a_split_is_a_second_transcript_and_a_second_field() {
    let sb = Sandbox::new("split");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    assert_eq!(s.chat_buffers().len(), 1, "one pane to start with");

    s.window_key("v");
    assert!(
        s.pump(|s| s.chat_buffers().len() == 2),
        "splitting made a second transcript\n{:?}",
        s.chat_buffers()
    );
    assert!(
        s.pump(|s| s.tabline_now().contains("2 panes")),
        "and the bar says so: {:?}",
        s.tabline_now()
    );
}

/// The whole point of a per-pane composer: two drafts at once, neither of them the other's.
#[test]
fn each_pane_holds_its_own_draft() {
    let sb = Sandbox::new("drafts");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.type_text("left");
    s.window_key("v");
    assert!(s.pump(|s| s.chat_buffers().len() == 2), "split");
    s.type_text("right");

    // Two composer buffers, one holding each. Read as a set rather than in order, because which
    // one is "first" is an implementation detail of how the ids were handed out.
    assert!(
        s.pump(|s| {
            let drafts: Vec<String> = s
                .events
                .iter()
                .filter(|e| e["type"] == "buffer_opened")
                .filter(|e| e["name"] == "[composer]")
                .filter_map(|e| e["buf"].as_u64())
                .map(|b| s.lines_of(b).join(""))
                .collect();
            drafts.iter().any(|d| d.contains("left")) && drafts.iter().any(|d| d.contains("right"))
        }),
        "each field kept what was typed into it"
    );
}

/// And closing one takes its transcript with it, rather than leaving a buffer nothing can reach.
#[test]
fn closing_a_pane_takes_its_transcript_away() {
    let sb = Sandbox::new("unsplit");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.window_key("v");
    assert!(s.pump(|s| s.chat_buffers().len() == 2), "split");

    s.window_key("q");
    assert!(
        s.pump(|s| s.chat_buffers().len() == 1),
        "closing gave the transcript back\n{:?}",
        s.chat_buffers()
    );
    assert!(
        s.pump(|s| !s.tabline_now().contains("panes")),
        "and the bar is a single tab again: {:?}",
        s.tabline_now()
    );
}

/// The last pane is refused rather than leaving a terminal with nothing to draw in.
#[test]
fn the_last_pane_cannot_be_closed() {
    let sb = Sandbox::new("lastpane");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.window_key("q");
    assert!(
        s.pump(|s| s.chat_buffers().len() == 1),
        "still exactly one pane\n{:?}",
        s.chat_buffers()
    );
}

/// A tab is a place, not a conversation.
///
/// Opening one used to start a conversation, which put a row in the project panel every time —
/// a list of half-used "New conversation" entries nobody asked for, in the one panel whose job is
/// the conversations you are actually working in. It inherits now, exactly as a split does; `^N` is
/// still the key that starts one and still the key that asks where.
#[test]
fn a_new_tab_does_not_start_a_conversation() {
    let sb = Sandbox::new("newtab");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    let before = s.sidebar_rows();

    // Through the chooser, taking the option that shows what you are already reading — the one
    // that must not add a row to the project panel.
    s.window_key("t");
    assert!(s.pump(|s| s.buffer_named("[new tab]").is_some()), "the panel is up");
    s.key("3");
    assert!(
        s.pump(|s| {
            let t = s.tabline_now();
            t.contains(" 1 ") && t.contains(" 2 ")
        }),
        "the bar has two tabs: {:?}",
        s.tabline_now()
    );
    // Given a moment for a conversation to appear, if one were going to.
    s.drain_for(Duration::from_millis(400));
    assert_eq!(
        s.sidebar_rows(),
        before,
        "and the project panel gained nothing\n{:?}",
        s.sidebar_now()
    );
}

/// The bar is always there, and always says how to reach the rest — which is the whole of why a
/// prefix layer is affordable here at all.
#[test]
fn the_tab_bar_advertises_the_prefix_from_the_first_frame() {
    let sb = Sandbox::new("tabbar");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    assert!(
        s.pump(|s| s.tabline_now().contains("windows")),
        "the bar names the window prefix before anything has been done\n{:?}",
        s.tabline_now()
    );
}

/// Holding the prefix lists what follows it, built from the live keymap.
#[test]
fn holding_the_prefix_lists_what_it_can_do() {
    let sb = Sandbox::new("whichkey");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("w");
    assert!(
        s.pump(|s| {
            match s.buffer_named("[window keys]") {
                Some(b) => {
                    let rows = s.lines_of(b).join("\n");
                    rows.contains("Split this pane") && rows.contains("Open a tab")
                }
                None => false,
            }
        }),
        "the panel lists the verbs behind the prefix"
    );
}

/// The layout is announced as soon as it exists, not only when something changes it.
///
/// A window docked in a pane the frontend has not heard of gets no rectangle and is not drawn —
/// which is right, and is why the *first* tab has to announce itself. Left to the first mutation, a
/// terminal that opens and is simply used had a transcript and a composer that existed, held the
/// right text, and were never once on screen.
#[test]
fn the_pane_layout_is_announced_before_any_key_is_pressed() {
    let sb = Sandbox::new("panes-init");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    assert!(
        s.pump(|s| s.events.iter().any(|e| e["type"] == "panes_changed")),
        "the frontend was told how the main region is divided"
    );

    // And every window in it names a pane the layout contains, or it is a window nothing can draw.
    let panes: Vec<u64> = s
        .events
        .iter()
        .filter(|e| e["type"] == "panes_changed")
        .filter_map(|e| e["tabs"].as_array())
        .flatten()
        .filter_map(|t| t["root"]["pane"].as_u64())
        .collect();
    let docked: Vec<u64> = s
        .events
        .iter()
        .filter(|e| e["type"] == "window_opened")
        .filter_map(|e| e["layout"]["pane"].as_u64())
        .collect();
    assert!(!docked.is_empty(), "the transcript and the composer are docked in a pane");
    for p in &docked {
        assert!(panes.contains(p), "window docked in pane {p}, which no tab contains: {panes:?}");
    }
}

/// The window prefix waits for you.
///
/// `timeoutlen` is what stops a prefix from making a printable character untypeable — with `gd`
/// bound, `g` has to reach the field eventually. Nothing is waiting to be typed behind `^W`, so the
/// deadline protected nothing there and cost the thing that matters: the panel listing the verbs
/// appeared at 300 ms, the chord expired at 500, and reading the list it had just drawn was enough
/// to lose the chord it described — the verb you then pressed went into the message field.
#[test]
fn the_window_prefix_does_not_expire_while_you_read_the_panel() {
    let sb = Sandbox::new("prefix-wait");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.ctrl("w");
    // Comfortably past `timeoutlen`, which is 500 ms by default.
    std::thread::sleep(Duration::from_millis(1200));
    s.key("v");

    assert!(
        s.pump(|s| s.chat_buffers().len() == 2),
        "the split still happened after a pause\n{:?}",
        s.chat_buffers()
    );
}

/// And a key the prefix does not claim ends it, rather than leaving the keyboard held forever.
#[test]
fn a_key_the_prefix_does_not_claim_gives_the_keyboard_back() {
    let sb = Sandbox::new("prefix-escape");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.ctrl("w");
    std::thread::sleep(Duration::from_millis(1200));
    s.key("z");

    assert!(
        s.pump(|s| {
            match s.buffer_named("[composer]") {
                Some(b) => s.lines_of(b).join("").contains('z'),
                None => false,
            }
        }),
        "the unclaimed key was replayed into the field rather than swallowed"
    );
    assert_eq!(s.chat_buffers().len(), 1, "and nothing was split");
}

/// A prefix in flight must not disable the rest of the keyboard.
///
/// `<C-w><C-p>` is bound to nothing, so the sequence is abandoned — and abandoning it used to dump
/// both keys into the *unbound* path, which skips the keymaps entirely. So while the window prefix
/// was waiting, every other binding on the keyboard silently stopped working: `^P` did not open the
/// model picker, `^V` did not attach the image on the clipboard, and the only sign was that the key
/// you pressed did nothing.
///
/// Each key of an abandoned sequence is re-resolved on its own now. `^P` is bound by itself, so
/// that is what it means.
#[test]
fn a_binding_still_fires_after_an_abandoned_prefix() {
    let sb = Sandbox::new("prefix-passthru");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.ctrl("w");
    std::thread::sleep(Duration::from_millis(1200));
    s.ctrl("p");

    s.wait_for("Model");
    assert_eq!(s.chat_buffers().len(), 1, "and the prefix itself did nothing");
}

/// A plugin that reports, and sets, the model the workspace thinks it is talking to.
///
/// `agent.selection()` is what `^P` reads and `^E` builds its option sheet from, and it resolves
/// through the *store's current conversation* — which is the thing that had to start following the
/// pane with the keyboard. Asking it is asking exactly the question those two ask.
const PROBE: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("probe", [{
    id: "probe", driver: "probe", display_name: "Probe",
    models: [{ id: "one", display_name: "One" }, { id: "two", display_name: "Two" }],
  }], async (_req, emit) => { emit({ type: "message_stop" }); });

  await neosh.cmd.register("probe.pick", async (args) => {
    await neosh.agent.setSelection({ instance: "probe", model: args[0] });
  }, { desc: "choose a model" });

  await neosh.cmd.register("probe.model", async () => {
    const s = await neosh.agent.selection();
    neosh.notify("model=" + (s ? s.model : "none"));
  }, { desc: "say which model is selected" });

  neosh.event.on("neosh.ready", () => neosh.notify("probe ready"));
}
"#;

fn install_probe(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/probe");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"probe\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), PROBE).expect("plugin");
}

/// The pane with the keyboard is the conversation the model belongs to.
///
/// Which model answers, how hard it thinks, what the context meter measures and what the permission
/// key sets are all facts about a *conversation*, and they resolve through the one conversation the
/// store calls current. Nothing told it the keyboard had moved between panes — so with two chats
/// side by side, `^P` showed the model of whichever pane you last started a conversation in, `^E`
/// opened that one's options, and the footer named its model while you typed into the other.
#[test]
fn the_focused_pane_is_the_conversation_the_model_belongs_to() {
    let sb = Sandbox::new("pane-model");
    install_probe(&sb);
    let mut s = sb.start();
    s.wait_for("probe ready");
    s.wait_for("PROJECTS");

    /// Ask, and wait for an answer that says `want`.
    ///
    /// Retried rather than asked once: `setSelection` crosses the plugin boundary, so the first
    /// answer can be the value from before it landed. Every retry is a fresh question, so a stale
    /// notification cannot satisfy this by sitting in the log.
    fn model_is(s: &mut Session, want: &str) -> bool {
        for _ in 0..40 {
            let before = s.texts().iter().filter(|t| t.starts_with("model=")).count();
            s.send(&command("probe.model"));
            let target = before;
            s.pump(move |s| s.texts().iter().filter(|t| t.starts_with("model=")).count() > target);
            if s.texts().iter().rev().find(|t| t.starts_with("model=")).is_some_and(|t| t == want) {
                return true;
            }
        }
        false
    }

    s.send(&command_with("probe.pick", "one"));
    assert!(model_is(&mut s, "model=one"), "the left pane is on model one");

    // A split onto a conversation of its own, given a different model.
    s.window_key("c");
    assert!(s.pump(|s| s.chat_buffers().len() == 2), "split onto a new conversation");
    s.send(&command_with("probe.pick", "two"));
    assert!(model_is(&mut s, "model=two"), "the right pane is on model two");

    // And back. This is the assertion the bug failed: the keyboard moved and nothing told the
    // store, so `^P`, `^E` and the footer all kept answering about the pane you had left.
    s.window_key("h");
    assert!(
        model_is(&mut s, "model=one"),
        "moving back to the left pane brought its model back, not the other pane's"
    );
}

/// A shell in a tab: spawned, drawn, and named after what is running in it.
///
/// The unit tests in `crate::term` prove the pty and the parser; this is the wiring — that a pane
/// can *be* a terminal, that its window gets a surface, and that keys reach the child rather than
/// the composer.
#[test]
fn a_tab_can_run_a_shell() {
    let sb = Sandbox::new("shell-tab");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.window_key("T");
    assert!(
        s.pump(|s| s.buffer_named("[terminal]").is_some()),
        "a terminal pane was furnished"
    );
    // A surface is what a terminal draws through — cells rather than lines of text.
    assert!(
        s.pump(|s| s.events.iter().any(|e| e["type"] == "surface_claimed")),
        "and it claimed a surface to paint on"
    );
    assert!(
        s.pump(|s| s.events.iter().any(|e| e["type"] == "surface_cells")),
        "and the shell drew something"
    );
    // Named for the command, not for a conversation it is not in.
    assert!(
        s.pump(|s| {
            let t = s.tabline_now();
            t.contains("sh") || t.contains("bash") || t.contains("zsh")
        }),
        "the tab is named after the shell: {:?}",
        s.tabline_now()
    );
}

/// `<C-w>t` asks what the tab is for, rather than deciding.
#[test]
fn a_new_tab_asks_what_goes_in_it() {
    let sb = Sandbox::new("tab-choice");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.window_key("t");
    assert!(
        s.pump(|s| match s.buffer_named("[new tab]") {
            Some(b) => {
                let rows = s.lines_of(b).join("\n");
                rows.contains("New conversation") && rows.contains("Terminal")
            }
            None => false,
        }),
        "the panel offers what a tab can be"
    );
}

/// And the tab is not made until you have answered — a tab that flickers into the bar and out
/// again on a key you cancelled is worse than the question taking a moment.
#[test]
fn cancelling_the_chooser_makes_no_tab() {
    let sb = Sandbox::new("tab-cancel");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.window_key("t");
    assert!(s.pump(|s| s.buffer_named("[new tab]").is_some()), "the panel is up");

    s.special("esc");
    s.drain_for(Duration::from_millis(500));
    let bar = s.tabline_now();
    assert!(!bar.contains(" 2 "), "no second tab was made: {bar:?}");
}

/// Choosing the shell gives you one.
#[test]
fn choosing_a_terminal_starts_a_shell() {
    let sb = Sandbox::new("tab-choice-term");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.window_key("t");
    assert!(s.pump(|s| s.buffer_named("[new tab]").is_some()), "the panel is up");

    s.key("2");
    assert!(
        s.pump(|s| s.buffer_named("[terminal]").is_some()),
        "a terminal pane was furnished"
    );
    assert!(
        s.pump(|s| s.events.iter().any(|e| e["type"] == "surface_cells")),
        "and the shell drew something"
    );
}

/// A tab by number. The binding existed and the command did not, so every `<C-w>1` resolved to
/// "no such command" — silently enough that the key just looked dead.
#[test]
fn a_tab_can_be_reached_by_its_number() {
    let sb = Sandbox::new("tab-number");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    // Two tabs, landing on the second.
    s.window_key("t");
    assert!(s.pump(|s| s.buffer_named("[new tab]").is_some()), "the panel is up");
    s.key("3");
    assert!(s.pump(|s| s.tabline_now().contains(" 2 ")), "there are two tabs");

    s.window_key("1");
    s.drain_for(Duration::from_millis(400));
    assert!(
        !s.texts().iter().any(|t| t.contains("no such command")),
        "the key is bound to something that exists"
    );
}

/// The bar is fitted to its width, and the one tab it must never drop is the one you are in.
///
/// Filled left to right and cut at the end, four tabs with the keyboard in the fourth read
/// `1 new  2 new` — the row whose whole job is saying which conversations are open spending itself
/// on the ones you are not in. Nothing else on screen answers "which tab is this", so the answer
/// has to survive the fitting rather than be the first thing it discards. The tabs are all called
/// `new` here, which is exactly the case that makes this matter: with no titles to tell them apart,
/// the number on the bar is the only answer there is.
#[test]
fn the_bar_always_names_the_tab_you_are_in() {
    let sb = Sandbox::new("tabline-keeps-active");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    // Four tabs, landing in the fourth. `3` is this conversation again, which is the cheap one —
    // what is being tested is the bar, not what the tabs hold.
    for n in 2..=4 {
        s.window_key("t");
        assert!(s.pump(|s| s.buffer_named("[new tab]").is_some()), "the panel is up");
        s.key("3");
        assert!(
            s.pump(|s| s.tabline_now().contains(&format!(" {n} "))),
            "there are {n} tabs: {:?}",
            s.tabline_now()
        );
    }

    // Narrow enough that not all four can be on it. A stdio frontend reports no geometry of its
    // own, so until this is said the bar has infinite room and nothing is ever dropped.
    s.viewport("[tabs]", 30, 1);
    assert!(
        s.pump(|s| s.tabline_now().contains('+')),
        "the bar had to leave something out: {:?}",
        s.tabline_now()
    );
    assert!(
        s.pump(|s| s.tabline_now().contains(" 4 ")),
        "and what it left out is not the tab the keyboard is in: {:?}",
        s.tabline_now()
    );

    // And it is a window that moves with you, not a rule about the end of the list.
    s.window_key("1");
    assert!(
        s.pump(|s| s.tabline_now().contains(" 1 ")),
        "the bar followed the keyboard back: {:?}",
        s.tabline_now()
    );
}

/// Every key the prefix panel advertises is a key that is actually bound.
///
/// A legend is a promise about a keyboard, and the panel is built from the live registry — so the
/// way to break that promise is for a binding to fail to *register*, which is silent. `<C-w><` did:
/// a bare `<` opens a special-key name in the notation, so the binding was rejected as an
/// unterminated `<` and the key did nothing while `>` beside it worked. The panel listing it is the
/// proof it registered, because the panel has no other source.
#[test]
fn every_window_key_the_panel_lists_is_bound() {
    let sb = Sandbox::new("window-keys-real");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.ctrl("w");
    assert!(
        s.pump(|s| match s.buffer_named("[window keys]") {
            Some(b) => {
                let rows = s.lines_of(b).join("\n");
                rows.contains("Narrow this pane") && rows.contains("Widen this pane")
            }
            None => false,
        }),
        "both resize keys parsed, registered, and reached the legend"
    );
}

/// The way out of a full-screen program is drawn over it.
#[test]
fn the_prefix_panel_opens_over_a_terminal() {
    let sb = Sandbox::new("prefix-over-term");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.window_key("T");
    assert!(s.pump(|s| s.buffer_named("[terminal]").is_some()), "a shell is running");
    assert!(s.pump(|s| s.events.iter().any(|e| e["type"] == "surface_cells")), "and drawing");

    s.ctrl("w");
    assert!(
        s.pump(|s| match s.buffer_named("[window keys]") {
            Some(b) => s.lines_of(b).join("\n").contains("Close this pane"),
            None => false,
        }),
        "the panel opened while a shell had the pane"
    );
}

impl Session {
    /// The panes of the active tab, in tree order, as the core last said them.
    fn pane_order(&self) -> Vec<u64> {
        fn leaves(node: &serde_json::Value, out: &mut Vec<u64>) {
            match node["kind"].as_str() {
                Some("leaf") => {
                    if let Some(p) = node["pane"].as_u64() {
                        out.push(p);
                    }
                }
                Some("split") => {
                    for c in node["children"].as_array().into_iter().flatten() {
                        leaves(&c["node"], out);
                    }
                }
                _ => {}
            }
        }
        let last = self.events.iter().rev().find(|e| e["type"] == "panes_changed");
        let Some(ev) = last else { return Vec::new() };
        let active = &ev["active"];
        let mut out = Vec::new();
        for tab in ev["tabs"].as_array().into_iter().flatten() {
            if &tab["id"] == active {
                leaves(&tab["root"], &mut out);
            }
        }
        out
    }
}

/// You can move *between* panes and you can move *a* pane.
///
/// The second was missing: `PaneSwap` existed in the API with no key on it, and Vim's keys for
/// moving a window were taken by a resize alias that `<`/`>` had made redundant. A layout that came
/// out the wrong way round could only be closed and rebuilt.
#[test]
fn a_pane_can_be_sent_to_an_edge() {
    let sb = Sandbox::new("pane-move");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.window_key("v");
    assert!(s.pump(|s| s.pane_order().len() == 2), "two panes");
    s.window_key("s");
    assert!(s.pump(|s| s.pane_order().len() == 3), "three panes");
    let before = s.pane_order();
    let moving = *before.last().expect("the pane with the keyboard is the newest");

    s.window_key("H");
    assert!(
        s.pump(|s| s.pane_order().first() == Some(&moving)),
        "the pane went to the far left: {:?} -> {:?}",
        before,
        s.pane_order()
    );
    assert_eq!(s.pane_order().len(), 3, "and nothing was lost on the way");
}

/// Exchange swaps two panes' places while the shape stays put — which is how you get the pane you
/// care about into the wide half without resizing anything.
#[test]
fn two_panes_can_be_exchanged() {
    let sb = Sandbox::new("pane-swap");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.window_key("v");
    assert!(s.pump(|s| s.pane_order().len() == 2), "two panes");
    let before = s.pane_order();

    s.window_key("x");
    assert!(
        s.pump(|s| {
            let now = s.pane_order();
            now.len() == 2 && now[0] == before[1] && now[1] == before[0]
        }),
        "the two swapped places: {:?} -> {:?}",
        before,
        s.pane_order()
    );
}

// ---------------------------------------------------------------------------
// Cloning — the other way a project arrives
// ---------------------------------------------------------------------------

/// An address in the `^O` field becomes an offer to clone, rather than a directory that is not there.
///
/// The shorthand is the case worth pinning: `owner/repo` is not a path and, since `^O` stopped
/// having a menu behind it, it is not a filter either — so it matched nothing at all and the field
/// sat there saying `no directory matches` at somebody who had pasted a perfectly good repository.
/// The row names the **expanded** URL, because a shorthand that does not show what it expanded to
/// is asking you to trust a guess about which host it means.
#[test]
fn an_address_in_the_add_project_field_offers_to_clone_it() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("cloneoffer");
    sb.git_init();
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.send(&command("project.open"));
    assert!(s.pump(|s| !s.float_named("[Add project").is_empty()), "the field opened");
    s.type_text("neoswarm/neosh");

    assert!(
        s.pump(|s| s.float_named("[Add project").iter().any(|l| l.contains("Clone neoswarm/neosh"))),
        "the address is a row\n{:?}",
        s.float_named("[Add project")
    );
    let rows = s.float_named("[Add project");
    assert!(
        rows.iter().any(|l| l.contains("https://github.com/neoswarm/neosh")),
        "and it says what the shorthand expanded to\n{rows:?}"
    );
}

/// A path is still a path. The shorthand must not eat the thing this field is mainly for.
#[test]
fn a_path_in_the_add_project_field_is_not_read_as_a_repository() {
    let sb = Sandbox::new("clonenotpath");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.send(&command("project.open"));
    assert!(s.pump(|s| !s.float_named("[Add project").is_empty()), "the field opened");
    // Two segments, exactly the shape the shorthand takes — and a path, because of the `~`.
    s.type_text("~/");

    assert!(
        s.pump(|s| s.float_named("[Add project").iter().any(|l| l.contains('/'))),
        "it completed directories\n{:?}",
        s.float_named("[Add project")
    );
    let rows = s.float_named("[Add project");
    assert!(!rows.iter().any(|l| l.contains("Clone ")), "and offered no clone\n{rows:?}");
}

/// `git.clone` fetches a real repository, into a directory whose parents do not exist yet.
///
/// The whole path end to end: the command the sidebar calls, the `ApiCall` behind it, the `git`
/// process, and the directory afterwards. `file://` rather than a bare path on purpose — a plain
/// local clone hardlinks and reports nothing, so the progress this feature exists to draw would
/// never be exercised.
#[test]
fn cloning_puts_a_repository_where_it_was_asked_to() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("clonereal");
    sb.git_init();
    // `clone.root` inside the sandbox, and not for tidiness: the default is `~/.nsh/repos`, so a
    // test that pressed `↵` on the first row without setting this would clone into the home
    // directory of whoever ran it.
    let root = sb.root.join("myrepos");
    sb.write_config(&format!("[options]\n\"clone.root\" = \"{}\"\n", root.display()));
    // The source at a path whose last two segments are the owner and the repository, because that
    // is what the destination is built from and `<sandbox>/work` would name them after the test.
    let src = clone_source(&sb, "acme", "thing");
    let url = format!("file://{}", src.display());

    let mut s = sb.start_letting_config_choose();
    s.wait_for("PROJECTS");
    // One argument. The owner, repository and host are read back off the URL rather than passed,
    // so this is the same call a script or the palette makes.
    s.send(&format!(r#"{{"type":"command","name":"git.clone","args":["{url}"]}}"#));
    assert!(s.pump(|s| !s.float_named("[Clone acme/thing").is_empty()), "it asked where");
    s.enter();

    // `<root>/acme/thing` — two levels of parent that have never existed. `git clone` makes the
    // leaf and not the path to it, which is why the vcs layer creates them.
    let at = root.join("acme").join("thing");
    assert!(
        s.pump(|_| at.join("README.md").is_file()),
        "the working tree arrived at {}",
        at.display()
    );
    assert!(at.join(".git").exists(), "and it is a repository");
    // Cloning is how a project joins the list. The verb opens what it fetched — otherwise it does
    // the work and withholds the reason for it, and you go and find the thing you just asked for.
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("thing"))),
        "and the project is on the panel\n{:?}",
        s.sidebar_now()
    );
}

/// A destination argument skips the asking, which is what makes this callable with no screen.
///
/// `neosh agent run git.clone <url>` opens a picker, and a picker is a thing only a person at a
/// terminal can answer — so a caller without one names the destination and nothing is asked. A call
/// that opens a dialog nobody can reach is a call that never returns.
#[test]
fn cloning_to_a_named_destination_asks_nothing() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("clonedest");
    sb.git_init();
    let src = clone_source(&sb, "acme", "thing");
    let at = sb.root.join("somewhere").join("particular");

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.send(&format!(
        r#"{{"type":"command","name":"git.clone","args":["file://{}","{}"]}}"#,
        src.display(),
        at.display()
    ));

    assert!(
        s.pump(|_| at.join("README.md").is_file()),
        "it cloned straight to {}",
        at.display()
    );
    assert!(
        s.float_named("[Clone ").is_empty(),
        "and asked nothing\n{:?}",
        s.float_named("[Clone ")
    );
}

/// A repository to clone from, at `<sandbox>/<owner>/<repo>`.
///
/// Cloned from the sandbox's own checkout rather than built from scratch, so it has the `README.md`
/// the assertions look for and exactly one commit.
fn clone_source(sb: &Sandbox, owner: &str, repo: &str) -> PathBuf {
    let at = sb.root.join(owner).join(repo);
    std::fs::create_dir_all(at.parent().expect("parent")).expect("mkdir");
    let out = Command::new("git")
        .args(["clone", "--quiet", "--", &sb.work().display().to_string(), &at.display().to_string()])
        .output()
        .expect("git runs");
    assert!(out.status.success(), "clone source: {}", String::from_utf8_lossy(&out.stderr));
    at
}

/// The destination picker offers `clone.root` first, and says which row that is.
///
/// Always asking is the point — a clone writes a whole history onto a disk and where that happens
/// is a thing people answer differently. The default being *first* is what keeps that from costing
/// anything: `↵` takes it.
#[test]
fn the_clone_destination_offers_the_configured_root_first() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("cloneroot");
    sb.git_init();
    let root = sb.root.join("myrepos");
    sb.write_config(&format!("[options]\n\"clone.root\" = \"{}\"\n", root.display()));
    let src = clone_source(&sb, "acme", "thing");
    let url = format!("file://{}", src.display());

    let mut s = sb.start_letting_config_choose();
    s.wait_for("PROJECTS");
    s.send(&format!(r#"{{"type":"command","name":"git.clone","args":["{url}"]}}"#));

    assert!(s.pump(|s| !s.float_named("[Clone acme/thing").is_empty()), "it asked where");
    let rows = s.float_named("[Clone acme/thing");
    // Nested by owner: one root collects repositories from everywhere, and `api` is the name of a
    // great many of them.
    assert!(
        rows.iter().any(|l| l.contains("myrepos") && l.contains("acme") && l.contains("thing")),
        "the configured root is a row, nested by owner\n{rows:?}"
    );
    assert!(
        rows.iter().any(|l| l.contains("clone.root")),
        "and it says which setting put it there\n{rows:?}"
    );
}

/* -------------------------------------------------------------------------- */
/* Another computer's work, in this computer's tree                           */
/* -------------------------------------------------------------------------- */

/// Which way round a shell may be opened, for [`Peer::beside_with`].
#[derive(Clone, Copy, Default)]
struct Shells {
    /// Whether the *peer* opens shells — what `t` in this workspace's panel needs.
    theirs: bool,
    /// Whether the workspace *under test* opens shells for the peer. Off by default, here as in a
    /// real config, which is the state the refusal test asserts.
    ours: bool,
}

/// A peer, played by a bare swarm node in this process.
///
/// The alternative is a second whole neosh — a second process, a second config directory and a
/// second screen to read — for a machine whose entire contribution to this test is an inventory and
/// a list of projects. What is under test is what *this* workspace does with those two things.
///
/// The runtime is held because the node's tasks live on it: dropped, the peer stops mid-handshake
/// and the workspace under test correctly reports a machine that went away.
struct Peer {
    _rt: tokio::runtime::Runtime,
    handle: neosh_swarm::SwarmHandle,
    events: tokio::sync::mpsc::UnboundedReceiver<neosh_swarm::SwarmEvent>,
    /// Whoever dialled us, learnt from the handshake — the address every answer goes back to.
    asker: Option<neosh_proto::NodeId>,
    /// What this machine says it can run, whenever the workspace asks. Answered from inside
    /// [`Peer::wait_event`], because the workspace asks the moment a conversation opens.
    catalogue: neosh_proto::ModelCatalogue,
}

impl Peer {
    /// Stand one up, authorised by and authorising the workspace in `sb`.
    ///
    /// The workspace's identity is minted *here*, before it starts, which is the whole trick:
    /// pairing is mutual and each side has to name the other's key, so somebody has to know both
    /// before either is running. `Identity::load_or_create` writes it where the workspace will look.
    fn beside(sb: &Sandbox, name: &str) -> Self {
        Self::beside_with(sb, name, Shells::default())
    }

    /// The same, saying which of the two machines will open a shell for the other.
    ///
    /// Two flags rather than one because they are two different decisions and this branch is about
    /// both: `theirs` is what a peer advertises, which is what `t` in the panel needs, and `ours` is
    /// `accepts_shells` in the config of the workspace under test — the direction where a bug means
    /// somebody else's prompt on your machine.
    fn beside_with(sb: &Sandbox, name: &str, shells: Shells) -> Self {
        // Bind to learn a port and let go again. A fixed one cannot run twice at once, and this
        // suite runs several tests at a time.
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe");
        let addr = probe.local_addr().expect("addr");
        drop(probe);

        let state = sb.root.join("state");
        std::fs::create_dir_all(&state).expect("state dir");
        let mine = neosh_swarm::Identity::load_or_create(&neosh_swarm::identity::key_path(&state))
            .expect("this workspace's key");
        let theirs = neosh_swarm::Identity::ephemeral();

        sb.write_config(&format!(
            "[swarm]\nenabled = true\nlisten = \"\"\nheartbeat_secs = 1\n\
             accepts_shells = {}\n\n\
             [[swarm.peers]]\naddr = \"{addr}\"\nid = \"{}\"\nname = \"{name}\"\n",
            shells.ours,
            theirs.id()
        ));

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("a runtime for the peer");
        let guard = rt.enter();
        let allowed = neosh_swarm::Allowed::load(None);
        allowed.add(mine.id().clone(), neosh_swarm::AllowedPeer {
            name: "the workspace under test".into(),
            alias: None,
            addr: None,
            source: neosh_swarm::Source::Paired,
        });
        let cfg = neosh_swarm::SwarmConfig {
            name: name.into(),
            listen: Some(addr),
            accepts_commands: true,
            accepts_shells: shells.theirs,
            heartbeat: Duration::from_millis(200),
            ..Default::default()
        };
        let (handle, events) =
            neosh_swarm::node::spawn(std::sync::Arc::new(theirs), allowed, cfg, "0.4.6".into());
        drop(guard);
        Self { _rt: rt, handle, events, asker: None, catalogue: Default::default() }
    }

    /// Wait for the workspace to ask for a shell, and hand it one.
    ///
    /// Standing in for a pty and a login shell, which is the right amount of machinery: what is
    /// under test is whether this workspace turns an answer into a pane you can see, not whether
    /// `openpty` works. So the "shell" is one line of output, sent the moment the handle is minted.
    fn answer_shell(&mut self, say: &str) -> String {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            assert!(std::time::Instant::now() < deadline, "no shell was ever asked for");
            let Some(event) = self.events.blocking_recv() else { panic!("the peer stopped") };
            match event {
                neosh_swarm::SwarmEvent::PeerUp { node, .. } => self.asker = Some(node.id),
                neosh_swarm::SwarmEvent::PtyOpen { node, id, .. } => {
                    self.handle.send(neosh_swarm::SwarmRequest::PtyOpened {
                        node: node.clone(),
                        id,
                        result: Ok("t1".into()),
                    });
                    self.handle.send(neosh_swarm::SwarmRequest::PtyData {
                        node,
                        pty: "t1".into(),
                        data: say.as_bytes().to_vec(),
                    });
                    return "t1".into();
                }
                _ => {}
            }
        }
    }

    /// Say what this machine has and what is open in it.
    fn publish(
        &self,
        projects: Vec<neosh_proto::RemoteProject>,
        agents: Vec<neosh_proto::AgentSummary>,
    ) {
        self.handle.send(neosh_swarm::SwarmRequest::Projects { projects });
        self.handle.send(neosh_swarm::SwarmRequest::Publish { full: true, agents, gone: Vec::new() });
    }
}

/// One conversation on the peer, in one of its directories.
fn there(session: &str, label: &str, key: &str, cwd: &str, root: &str, branch: Option<&str>) -> neosh_proto::AgentSummary {
    neosh_proto::AgentSummary {
        session: neosh_proto::SessionId(session.into()),
        project: neosh_proto::ProjectKey(key.into()),
        project_name: match branch {
            Some(b) => format!("work · {b}"),
            None => "work".into(),
        },
        cwd: cwd.into(),
        repo_root: Some(root.into()),
        branch: branch.map(str::to_string),
        label: label.into(),
        state: neosh_proto::AgentState::Idle,
        message_count: 1,
        model: None,
        instance: None,
        mode: None,
        options: Vec::new(),
        context_tokens: 0,
        context_window: None,
        turn_started_at: None,
        updated_at: now_secs(),
        usage: Default::default(),
        unread: false,
        interrupted: false,
        background: 0,
        decorations: Vec::new(),
    }
}

/// One of the peer's checkouts, whether or not anything is open in it.
fn theirs(key: &str, name: &str, cwd: &str, root: &str, branch: Option<&str>) -> neosh_proto::RemoteProject {
    neosh_proto::RemoteProject {
        key: neosh_proto::ProjectKey(key.into()),
        name: name.into(),
        cwd: cwd.into(),
        repo_root: Some(root.into()),
        branch: branch.map(str::to_string),
        head: None,
        active: false,
        sessions: 0,
        running: 0,
        decorations: Vec::new(),
    }
}

/// A repository you both have is **one row**, and the other machine's work is inside it.
///
/// The bug this pins down drew it as two: local conversations were grouped by directory and matched
/// against remote ones on a key the local side never had — inferred from the remote agent's `cwd`,
/// so the two only met when both machines happened to have the repository at the same absolute
/// path. They never do. The repository you were sitting in appeared once as yours and once again
/// below as a project you had never heard of, named after whichever remote conversation sorted
/// first — which for a worktree is `work · fix/thing`, a display string being used as a key.
#[test]
fn a_repository_on_two_computers_is_one_row_with_both_lots_of_work_in_it() {
    const KEY: &str = "git:github.com/neoswarm/work";
    let sb = Sandbox::new("swarm-one-tree");
    sb.git_init();
    sb.git_remote("https://github.com/neoswarm/work.git");
    let peer = Peer::beside(&sb, "linux-box");
    // Deliberately nothing like the local path: that difference is the whole point.
    peer.publish(
        vec![theirs(KEY, "work", "/srv/checkouts/work", "/srv/checkouts/work", None)],
        vec![
            there("r1", "reading the parser", KEY, "/srv/checkouts/work", "/srv/checkouts/work", None),
            there("r2", "chasing the flake", KEY, "/srv/checkouts/work/.wt/fix", "/srv/checkouts/work", Some("fix/the-thing")),
        ],
    );

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("reading the parser"))),
        "the other machine's conversation arrives at all:\n{:?}",
        s.sidebar_now()
    );

    let rows = s.sidebar_now();
    let named = |needle: &str| rows.iter().position(|l| l.contains(needle));

    // One project row, not two. `work · fix/thing` was the second one, and it is the display string
    // the old grouping mistook for an identity.
    assert!(
        !rows.iter().any(|l| l.contains("work · ") || l.contains("work \u{b7} ")),
        "a worktree is never a project named after a branch:\n{rows:?}"
    );
    let heads = rows.iter().filter(|l| l.contains("work") && !l.contains("\u{2387}")).count();
    assert_eq!(heads, 1, "one row for one repository:\n{rows:?}");

    // The other machine is a block of its own under the repository: its row, then its main
    // checkout's conversation, then its worktree and the conversation in that — never folded in
    // with ours, whatever it is on.
    let repo = named("work").expect("the repository row");
    let block = named("@lb").expect("the machine's block row");
    let mine = named("reading the").expect("their main-checkout conversation");
    let tree = named("fix/the-thing").expect("their worktree, as a checkout row");
    let inside = named("chasing the").expect("the conversation in their worktree");
    assert!(
        repo < block && block < mine && mine < tree && tree < inside,
        "nested, in order:\n{rows:?}"
    );

    // The block row says which machine, once; everything under it is indented under it, with no
    // line drawn down the side.
    let indent = |row: usize| rows[row].len() - rows[row].trim_start().len();
    for row in [mine, tree, inside] {
        assert!(!rows[row].contains('@'), "said once, on the block:\n{rows:?}");
        assert!(!rows[row].contains('\u{2502}'), "no rail down the side:\n{rows:?}");
        assert!(indent(row) > indent(block), "row {row} hangs off the block:\n{rows:?}");
    }
}

/// A checkout on a machine that has not allowed this one is not a row.
///
/// Its project list arrived in the handshake — the half of pairing that did happen — so this is not
/// about what may be shown. `waiting` is the far end explicitly saying no, and a one-sided pairing
/// filling your column with somebody else's repositories is a list growing on its own.
#[test]
fn a_machine_that_has_not_allowed_this_one_contributes_no_rows() {
    const KEY: &str = "git:github.com/neoswarm/other";
    let sb = Sandbox::new("swarm-unapproved");
    sb.git_init();
    sb.git_remote("https://github.com/neoswarm/work.git");
    // Authorised *by* us and not authorising us back, which is what leaves the link `waiting`.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe");
    let addr = probe.local_addr().expect("addr");
    drop(probe);
    let state = sb.root.join("state");
    std::fs::create_dir_all(&state).expect("state dir");
    let _mine = neosh_swarm::Identity::load_or_create(&neosh_swarm::identity::key_path(&state))
        .expect("this workspace's key");
    let theirs_id = neosh_swarm::Identity::ephemeral();
    sb.write_config(&format!(
        "[swarm]\nenabled = true\nlisten = \"\"\nheartbeat_secs = 1\n\n\
         [[swarm.peers]]\naddr = \"{addr}\"\nid = \"{}\"\nname = \"stranger\"\n",
        theirs_id.id()
    ));
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
    let guard = rt.enter();
    // An empty allow-list: it does not know us, so it hangs up after proving who it is.
    let cfg = neosh_swarm::SwarmConfig {
        name: "stranger".into(),
        listen: Some(addr),
        accepts_commands: true,
        heartbeat: Duration::from_millis(200),
        ..Default::default()
    };
    let (handle, _events) = neosh_swarm::node::spawn(
        std::sync::Arc::new(theirs_id),
        neosh_swarm::Allowed::load(None),
        cfg,
        "0.4.6".into(),
    );
    handle.send(neosh_swarm::SwarmRequest::Projects {
        projects: vec![theirs(KEY, "other", "/srv/other", "/srv/other", None)],
    });
    drop(guard);

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    // Long enough for a dial, a handshake and the refusal that ends it — the panel redraws on the
    // swarm change either way, so what is asserted is that nothing arrived rather than that it has
    // not arrived yet.
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("work"))),
        "our own project is drawn:\n{:?}",
        s.sidebar_now()
    );
    let rows = s.sidebar_now();
    assert!(
        !rows.iter().any(|l| l.contains("other")),
        "nothing from a machine that has not allowed this one:\n{rows:?}"
    );
    drop(handle);
    drop(rt);
}

impl Session {
    /// Put the panel's cursor on the first row containing `needle`, and say whether it got there.
    ///
    /// By moving rather than by counting, because a count is a test that passes for the wrong
    /// reason the moment the panel gains a row — and this panel gains rows for a living.
    fn sidebar_seek(&mut self, needle: &str) -> bool {
        for _ in 0..40 {
            if self.sidebar_cursor().is_some_and(|r| r.contains(needle)) {
                return true;
            }
            self.down();
            self.drain_for(Duration::from_millis(60));
        }
        false
    }

    /// Everything painted onto a raw-cell surface so far, row by row.
    ///
    /// A terminal pane draws in cells rather than lines — `Term::cells` emits every cell every time
    /// — so the buffer under it says nothing and this is the only place its contents are.
    fn surface_text(&self) -> Vec<String> {
        let mut rows: std::collections::BTreeMap<u64, std::collections::BTreeMap<u64, String>> =
            Default::default();
        for e in &self.events {
            if e["type"] != "surface_cells" {
                continue;
            }
            for c in e["cells"].as_array().into_iter().flatten() {
                let (Some(r), Some(col)) = (c["row"].as_u64(), c["col"].as_u64()) else { continue };
                let g = c["grapheme"].as_str().unwrap_or(" ").to_string();
                rows.entry(r).or_default().insert(col, g);
            }
        }
        rows.values().map(|cols| cols.values().cloned().collect::<String>()).collect()
    }
}

/// `t` on a row about another computer opens a terminal **on that computer**.
///
/// The half of a remote shell somebody actually presses: a key on a row becomes a pane with a
/// prompt in it. What is on the far end is a real pty in a real workspace and one line of output
/// here, which is the right amount of machinery — under test is whether this workspace turns an
/// answer into a pane you can see, not whether `openpty` works.
#[test]
fn t_on_another_computers_row_opens_a_terminal_over_there() {
    const KEY: &str = "git:github.com/neoswarm/faraway";
    let sb = Sandbox::new("swarm-shell");
    sb.git_init();
    sb.git_remote("https://github.com/neoswarm/work.git");
    let mut peer = Peer::beside_with(&sb, "linux-box", Shells { theirs: true, ..Default::default() });
    // A repository this machine has no clone of, so it is a row of its own rather than nested
    // inside `work` — which is also the row somebody is most likely to want a shell on.
    peer.publish(vec![theirs(KEY, "faraway", "/srv/faraway", "/srv/faraway", None)], Vec::new());

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("faraway"))),
        "the other machine's project arrives:\n{:?}",
        s.sidebar_now()
    );

    s.enter_panel();
    assert!(s.sidebar_seek("faraway"), "the cursor reaches it:\n{:?}", s.sidebar_now());
    s.key("t");

    // The pane exists from the press, not from the answer: a key that does nothing visible until
    // the network replies is indistinguishable from a key that is not bound.
    assert!(s.pump(|s| s.buffer_named("[terminal]").is_some()), "a terminal pane was furnished");
    assert!(
        s.pump(|s| s.surface_text().iter().any(|r| r.contains("opening a shell on linux-box"))),
        "and says what it is waiting for:\n{:?}",
        s.surface_text()
    );

    let pty = peer.answer_shell("hello from over there\r\n");
    assert_eq!(pty, "t1");
    assert!(
        s.pump(|s| s.surface_text().iter().any(|r| r.contains("hello from over there"))),
        "what the far end printed is drawn here:\n{:?}",
        s.surface_text()
    );
    // Named for where it is, which is the fact that tells this tab from the four beside it.
    assert!(
        s.pump(|s| s.tabline_now().contains("linux-box")),
        "the tab says which machine: {:?}",
        s.tabline_now()
    );
}

/// A machine that has not been told to open shells refuses, and says which setting to change.
///
/// Refused here rather than over there, because both halves of the answer are things this end can
/// know: the capability arrived in the handshake, and a round trip that cannot succeed should not
/// be spent finding that out. What matters is that it is *said* — a pane that stays black is the
/// one outcome somebody cannot act on.
#[test]
fn a_machine_that_does_not_open_shells_says_so_and_says_which_setting() {
    const KEY: &str = "git:github.com/neoswarm/faraway";
    let sb = Sandbox::new("swarm-noshell");
    sb.git_init();
    sb.git_remote("https://github.com/neoswarm/work.git");
    let peer = Peer::beside_with(&sb, "linux-box", Shells::default());
    peer.publish(vec![theirs(KEY, "faraway", "/srv/faraway", "/srv/faraway", None)], Vec::new());

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    assert!(s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("faraway"))));
    s.enter_panel();
    assert!(s.sidebar_seek("faraway"), "the cursor reaches it:\n{:?}", s.sidebar_now());
    s.key("t");

    assert!(
        s.pump(|s| s.texts().iter().any(|t| t.contains("accepts_shells"))),
        "the refusal names the setting to change:\n{}",
        s.transcript()
    );
    // And nothing is left on the bar. A tab you cannot type into is worse than no tab: the keys go
    // somewhere, and nothing on screen ever says why the prompt never came.
    assert!(
        s.pump(|s| !s.tabline_now().contains("linux-box")),
        "no tab is left behind: {:?}",
        s.tabline_now()
    );
}

impl Peer {
    /// Ask the workspace under test for a shell, and wait for what it says.
    ///
    /// The other direction, and the one where a bug is a stranger's prompt on your machine. The
    /// connection is symmetric once established, so this goes back down the link the workspace
    /// dialled — which is also the shape a real peer would use.
    fn ask_for_shell(&mut self, cwd: &str) -> Result<String, String> {
        let node = self.wait_up();
        self.handle.send(neosh_swarm::SwarmRequest::PtyOpen {
            node,
            id: "ask".into(),
            cwd: Some(cwd.into()),
            cols: 80,
            rows: 24,
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            assert!(std::time::Instant::now() < deadline, "the workspace never answered");
            let Some(event) = self.events.blocking_recv() else { panic!("the peer stopped") };
            if let neosh_swarm::SwarmEvent::PtyOpened { result, .. } = event {
                return result.map_err(|r| format!("{r:?}"));
            }
        }
    }

    /// Wait until the workspace has told this machine what project it is in.
    ///
    /// The one signal from outside that its key has *settled*. A workspace answers `dir:<name>`
    /// for the first moments of its life — a guess from the directory, until it has read the
    /// repository's origin — and everything that compares projects across machines has to tolerate
    /// that. A test pressing a key in that window is testing the fallback rather than the rule, and
    /// it is a window narrow enough to catch one run in three, which is the worst width there is.
    ///
    /// Nothing on screen says it, because on one machine nothing depends on it. The inventory does:
    /// it is this workspace saying what it has, to a peer, in the vocabulary the question is asked
    /// in.
    fn wait_for_key(&mut self, key: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "the workspace never settled on {key} for its own project"
            );
            let Some(event) = self.events.blocking_recv() else { panic!("the peer stopped") };
            match event {
                neosh_swarm::SwarmEvent::PeerUp { node, .. } => self.asker = Some(node.id),
                neosh_swarm::SwarmEvent::Inventory { agents, .. }
                    if agents.iter().any(|a| a.project.0 == key) =>
                {
                    return;
                }
                _ => {}
            }
        }
    }

    /// Say goodbye and stop, the way a workspace shutting down does.
    fn hang_up(&self) {
        self.handle.send(neosh_swarm::SwarmRequest::Shutdown);
    }

    /// Type at a shell this peer opened on the workspace under test.
    fn type_into(&mut self, pty: &str, text: &str) {
        let node = self.wait_up();
        self.handle.send(neosh_swarm::SwarmRequest::PtyData {
            node,
            pty: pty.into(),
            data: text.as_bytes().to_vec(),
        });
    }

    /// The id of the workspace under test, once the link is up.
    fn wait_up(&mut self) -> neosh_proto::NodeId {
        if let Some(id) = &self.asker {
            return id.clone();
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            assert!(std::time::Instant::now() < deadline, "the workspace never connected");
            let Some(event) = self.events.blocking_recv() else { panic!("the peer stopped") };
            if let neosh_swarm::SwarmEvent::PeerUp { node, .. } = event {
                self.asker = Some(node.id.clone());
                return node.id;
            }
        }
    }
}

/// A workspace told to open shells opens one, and says so on the machine it happens on.
///
/// The direction where a mistake is somebody else's prompt on your machine, with your shell, your
/// environment and your credentials, and nothing above it to say no. Which is why it is announced:
/// a workspace that let it happen silently would be one where the setting is the only evidence it
/// ever did.
#[test]
fn a_workspace_opens_a_shell_for_a_peer_only_when_told_to() {
    let sb = Sandbox::new("swarm-serve-shell");
    sb.git_init();
    let mut peer = Peer::beside_with(&sb, "linux-box", Shells { ours: true, ..Default::default() });

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    let work = sb.work().display().to_string();
    let pty = peer.ask_for_shell(&work).expect("a workspace that says yes opens one");
    assert!(!pty.is_empty(), "and mints a handle for it");

    assert!(
        s.pump(|s| s.texts().iter().any(|t| t.contains("opened a shell here"))),
        "and says so on the machine it is happening on:\n{}",
        s.transcript()
    );
}

/// And a peer that plays by the rules never asks, because the capability said no.
///
/// This is the *sender's* half and it is a courtesy, not the enforcement — which is exactly what
/// the mutation says: making the owner stop checking leaves this test green, because the frame is
/// never sent. What it does pin is the other reason the flag exists, which is compatibility: an
/// unknown message tag fails the frame and takes the connection with it, so a peer that sent one on
/// spec would knock a machine off the board rather than be told no.
///
/// [`a_workspace_refuses_a_shell_from_a_peer_that_asks_anyway`] is the enforcement.
///
/// `accepts_commands` is on by default and this is not, deliberately: starting an agent over there
/// is a thing with a permission layer over it and a transcript somebody can read afterwards, and a
/// pty is a prompt.
#[test]
fn a_well_behaved_peer_does_not_ask_for_a_shell_it_was_told_it_cannot_have() {
    let sb = Sandbox::new("swarm-refuse-shell");
    sb.git_init();
    // The default: nothing in the config says `accepts_shells`.
    let mut peer = Peer::beside_with(&sb, "linux-box", Shells::default());

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    let work = sb.work().display().to_string();
    let refused = peer.ask_for_shell(&work).expect_err("a workspace that was not told refuses");
    assert!(
        refused.contains("NotPermitted"),
        "and refuses by name rather than failing: {refused}"
    );
    assert!(
        !s.texts().iter().any(|t| t.contains("opened a shell here")),
        "and nothing was opened:\n{}",
        s.transcript()
    );
}

/// A peer that asks for a shell it was told it cannot have is **refused by the owner**.
///
/// The conformance point the well-behaved pair above cannot reach, and the only one that is a
/// security property rather than a courtesy: `NodeCapabilities` is something the other side could
/// have lied about or simply not read, so the owner checks on every open regardless of what its own
/// handshake advertised. Mutating that check away leaves every other test in this file green.
///
/// So the peer here is a **raw ASCP client** rather than a `neosh_swarm` node: a node would refuse
/// to send the frame on the sender's behalf, which is precisely the behaviour under test being
/// assumed rather than checked. It does the real handshake — the id is proven, and it is on the
/// workspace's allow-list — and then asks for something it has been told it cannot have, which is
/// the shape of every peer worth defending against: authorised, and wrong.
#[test]
fn a_workspace_refuses_a_shell_from_a_peer_that_asks_anyway() {
    use neosh_proto::{AscpMessage, NodeCapabilities, NodeInfo};

    let sb = Sandbox::new("swarm-lying-peer");
    sb.git_init();

    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe");
    let addr = probe.local_addr().expect("addr");
    drop(probe);
    let state = sb.root.join("state");
    std::fs::create_dir_all(&state).expect("state dir");
    let mine = neosh_swarm::Identity::load_or_create(&neosh_swarm::identity::key_path(&state))
        .expect("this workspace's key");
    let theirs = std::sync::Arc::new(neosh_swarm::Identity::ephemeral());
    // `accepts_shells` absent, which is the default and the state under test.
    sb.write_config(&format!(
        "[swarm]\nenabled = true\nlisten = \"\"\nheartbeat_secs = 1\n\n\
         [[swarm.peers]]\naddr = \"{addr}\"\nid = \"{}\"\nname = \"liar\"\n",
        theirs.id()
    ));

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
    let allowed = neosh_swarm::Allowed::load(None);
    allowed.add(mine.id().clone(), neosh_swarm::AllowedPeer {
        name: "the workspace under test".into(),
        alias: None,
        addr: None,
        source: neosh_swarm::Source::Paired,
    });

    // Listen, handshake, ask anyway, and report what came back.
    let asked = rt.spawn({
        let theirs = theirs.clone();
        async move {
            let sock = tokio::net::TcpListener::bind(addr).await.expect("listen");
            let (mut stream, _) = sock.accept().await.expect("the workspace dials");
            let info = NodeInfo {
                id: theirs.id().clone(),
                name: "liar".into(),
                os: "test".into(),
                version: "0.4.6".into(),
            };
            // Advertising `shells: true` about *itself* changes nothing about what it may ask for,
            // and is what a peer built to try this would do.
            let caps = NodeCapabilities {
                accepts_commands: true,
                accepts_approvals: false,
                streams: true,
                browse: true,
                shells: true,
                rich_stream: false,
                catalogue: false,
                live: false,
                projects: Vec::new(),
            };
            neosh_swarm::accept(&mut stream, &theirs, &allowed, &info, &caps).await.expect("paired");
            neosh_swarm::wire::write_message(&mut stream, &AscpMessage::PtyOpen {
                id: "sneaky".into(),
                cwd: None,
                cols: 80,
                rows: 24,
            })
            .await
            .expect("sent regardless");
            // Read past the heartbeats and the inventory for the answer to that one id.
            loop {
                match neosh_swarm::wire::read_message(&mut stream).await {
                    Ok(Some(AscpMessage::Refused { id, refusal })) if id == "sneaky" => {
                        return Some(Err(format!("{refusal:?}")));
                    }
                    Ok(Some(AscpMessage::PtyOpened { id, pty })) if id == "sneaky" => {
                        return Some(Ok(pty));
                    }
                    Ok(Some(_)) => continue,
                    _ => return None,
                }
            }
        }
    });

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    let answer = rt
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(25), asked).await
        })
        .expect("the peer finished")
        .expect("the peer did not panic");

    match answer {
        Some(Err(refusal)) => assert!(
            refusal.contains("NotPermitted"),
            "refused for the right reason rather than by failing: {refusal}"
        ),
        Some(Ok(pty)) => panic!("a shell was opened for a peer that was told it could not: {pty}"),
        None => panic!("the connection ended without an answer — every request is answered once"),
    }
    assert!(
        !s.texts().iter().any(|t| t.contains("opened a shell here")),
        "and nothing was opened:\n{}",
        s.transcript()
    );
}

/// A shell does not outlive the link that asked for it.
///
/// The cleanup nothing else would do, and conformance point 13. A pty has no timeout and no idea
/// the connection it was opened over has gone — so without this a machine going away leaves a login
/// shell running on somebody else's computer, with that user's environment, in one of their
/// directories, reachable by nothing and visible on no screen. It is the same class of thing as the
/// permission itself: a leak measured in "until the workspace stops".
///
/// Asserted on the child's own pid rather than on anything this workspace reports, because what is
/// under test is that a **process** is gone.
///
/// The peer says goodbye, which is the case a test can make deterministic. Everything after that is
/// shared with the abrupt one — a lid closing reaches the same `PeerDown` through a read that ends,
/// and the same `close_shells_for` behind it — so what is *not* covered here is how the link is
/// noticed, which is the swarm's business and has tests of its own, rather than what is done about
/// it, which is this.
#[test]
fn a_shell_dies_with_the_link_that_asked_for_it() {
    let sb = Sandbox::new("swarm-shell-cleanup");
    sb.git_init();
    let mut peer = Peer::beside_with(&sb, "linux-box", Shells { ours: true, ..Default::default() });

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    let work = sb.work();
    let pty = peer.ask_for_shell(&work.display().to_string()).expect("a shell");

    // The child says who it is. `$$` is POSIX, and `$SHELL` is pinned to `/bin/sh` for this suite
    // so the answer does not depend on whose dotfiles are installed.
    let pidfile = work.join("shell.pid");
    peer.type_into(&pty, &format!("echo $$ > {}\n", pidfile.display()));
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let pid = loop {
        assert!(std::time::Instant::now() < deadline, "the shell never reached a prompt");
        if let Ok(text) = std::fs::read_to_string(&pidfile)
            && let Ok(pid) = text.trim().parse::<i32>()
        {
            break pid;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(alive(pid), "the shell is running to begin with");

    // The machine goes away. Said out loud with a `Goodbye` rather than by dropping the runtime and
    // hoping the socket closes underneath it: what is under test is this workspace's cleanup, and a
    // teardown that races is a test that reports the wrong thing whenever it is slow.
    peer.hang_up();
    // Waited for on the workspace's own words rather than on a sleep. Until it has noticed the
    // machine is gone there is nothing for it to have cleaned up, and a test that started counting
    // before then would be timing the network.
    assert!(
        s.pump(|s| s.texts().iter().any(|t| t.contains("lost linux-box"))),
        "the workspace noticed the machine go:\n{}",
        s.transcript()
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while alive(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "pid {pid} is still running after the link that asked for it went away"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(peer);
}

/// Whether a process is still there, asked of the OS rather than of anything under test.
///
/// Signal 0 is the portable "does this exist and could I signal it" — it delivers nothing. A zombie
/// answers yes until it is reaped, which is fine here: what is being asserted is that the child was
/// killed, and `Serving`'s `Drop` waits after killing precisely so there is no zombie to see.
fn alive(pid: i32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `^N` does not ask which computer when none of them has this repository.
///
/// The question is worth asking when it is a decision, and a repository nobody else has a checkout
/// of has one possible answer. Asking anyway is a panel and a keypress spent confirming something
/// the workspace already knew — the same rule as not asking at all with no machine paired, one step
/// finer. The first cut asked the moment a second computer existed, which on a swarm of two made
/// every `^N` two keys instead of one for the whole time the other machine had nothing to do with
/// what you were working on.
#[test]
fn asking_which_computer_is_skipped_when_none_of_them_has_the_project() {
    let sb = Sandbox::new("swarm-no-question");
    sb.git_init();
    sb.git_remote("https://github.com/neoswarm/work.git");
    let mut peer = Peer::beside(&sb, "linux-box");
    // A machine that is up and useful, and has nothing to do with the repository you are in.
    peer.publish(
        vec![theirs("git:github.com/neoswarm/faraway", "faraway", "/srv/faraway", "/srv/faraway", None)],
        Vec::new(),
    );

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    // Wait for the machine itself, not for something it brought: a question that has not been
    // asked yet looks exactly like one that will not be, so a test that presses the key before the
    // peer has connected passes for the wrong reason. Its arrival is the one unambiguous signal.
    assert!(s.pump(|s| s.texts().iter().any(|t| t.contains("linux-box joined"))), "the peer joined");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("faraway"))),
        "and its project is on the panel:\n{:?}",
        s.sidebar_now()
    );
    // And that this workspace has settled on its *own* project key — the origin remote rather than
    // the `dir:` guess it answers with until it has read the repository. Until then the key is
    // deliberately not used to withhold the question, so a `^N` pressed in that window would be
    // testing the fallback rather than the rule.
    peer.wait_for_key("git:github.com/neoswarm/work");

    s.ctrl("n");
    // On the picker's own buffer rather than on the words in it: "New conversation" is also what an
    // unnamed conversation is called in the panel, so a text match passes before anything opens.
    assert!(
        s.pump(|s| !s.picker_named("[New conversation]").is_empty()),
        "the where-question is asked:\n{:?}",
        s.sidebar_now()
    );
    let rows = s.picker_named("[New conversation]");
    assert!(rows.iter().any(|l| l.contains("Here")), "and it is the one about directories:\n{rows:?}");
    // `This computer` is the machine picker's first row, and is what must not appear.
    assert!(
        !rows.iter().any(|l| l.contains("This computer")),
        "the which-computer question is not asked:\n{rows:?}"
    );
}

/// And it does ask when one of them has it, because then it is a decision.
#[test]
fn asking_which_computer_happens_when_one_of_them_has_the_project() {
    const KEY: &str = "git:github.com/neoswarm/work";
    let sb = Sandbox::new("swarm-yes-question");
    sb.git_init();
    sb.git_remote("https://github.com/neoswarm/work.git");
    let mut peer = Peer::beside(&sb, "linux-box");
    // The same repository, at a path of its own — which is the ordinary case and the one the
    // matching had to be made to see. With a conversation in it, purely so there is something on
    // screen to wait for: the repository merges into the `work` row that is already there and a
    // repository that is here as well deliberately wears no marker, so the machine connecting
    // changes nothing visible about the project itself.
    peer.publish(
        vec![theirs(KEY, "work", "/srv/checkouts/work", "/srv/checkouts/work", None)],
        vec![there("r1", "over there", KEY, "/srv/checkouts/work", "/srv/checkouts/work", None)],
    );

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    // The key first, for the reason the other test waits for it: until this workspace has read its
    // own origin it answers `dir:work`, and a `^N` pressed then is testing the fallback.
    peer.wait_for_key(KEY);
    // And then the merge, which is what makes this the same repository as the peer's.
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("over there"))),
        "the peer's work is in our tree:\n{:?}",
        s.sidebar_now()
    );

    s.ctrl("n");
    assert!(
        s.pump(|s| !s.picker_named("[New conversation — where?]").is_empty()),
        "which computer is asked first — instead the where-picker opened: {:?}",
        s.picker_named("[New conversation]")
    );
    let rows = s.picker_named("[New conversation — where?]");
    assert!(
        rows.iter().any(|l| l.contains("This computer")),
        "and this one is the row the cursor starts on, so `^N ⏎` is unchanged:\n{rows:?}"
    );
    assert!(
        rows.iter().any(|l| l.contains("linux-box")),
        "with the machine that has it beside it:\n{rows:?}"
    );
}

/// The cursor is a **band**, so what is marked on the row it is on keeps its own colour.
///
/// And, the reason this is a bug rather than a preference: keeps its own *motion*. A project being
/// fetched draws one cell of `Git.Fetching`, which is a `Frames` animation — and a run animates
/// because its winning highlight group says so. Drawn as a ranged group at priority 200 across the
/// whole row, the cursor won every character it covered, so the spinner stopped the moment you put
/// the cursor on the row and started again when you moved off. Exactly backwards: the row you are
/// standing on is the row you are asking about, and the spinner is the only thing on it saying the
/// answer is not in yet.
///
/// `line_hl_group` is the vocabulary the workspace already had for this and already used for diff
/// bands — under everything on the row, with every ranged group patched over it. Asserted on the
/// marks rather than on pixels, because what changed is which kind of mark the cursor is.
#[test]
fn a_marked_row_keeps_its_marks_under_the_cursor() {
    let sb = Sandbox::new("cursor-band");
    sb.git_init();
    // Something for the git plugin to put a badge on: an untracked file is the cheapest mark that
    // survives to the row.
    std::fs::write(sb.work().join("scratch.txt"), "x\n").expect("write");

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("?1"))),
        "the project row has a badge:\n{:?}",
        s.sidebar_now()
    );

    s.enter_panel();
    assert!(s.sidebar_seek("work"), "the cursor is on the badged row:\n{:?}", s.sidebar_now());
    let marks = s.sidebar_cursor_marks();
    assert!(
        marks.iter().any(|(g, _)| g.starts_with("Git.")),
        "the row has a badge on it at all: {marks:?}"
    );
    // The mechanism rather than the pixels, because the pixels are the renderer's: both marks are
    // *present* either way and what changed is which kind the cursor is, so a test that only listed
    // the groups on the row would pass with the bug in place. It did, which is how this ended up
    // asserting the shape. Who wins between a band and a run over it — and that a run's own
    // animation survives — is `render_line`'s, and is tested there.
    assert!(
        marks.iter().any(|(g, band)| g == "Sidebar.Selected" && *band),
        "and the cursor is a band under it, not a run over it: {marks:?}"
    );
}

/// `agent.append_prompt` is said at the end of the message that is sent — and only there.
///
/// The driver echoes the last user message it was handed, so the answer is the proof of what went
/// over the wire; the transcript's own row for the question is the proof of what did not.
#[test]
fn standing_instructions_end_every_question_and_never_reach_the_transcript() {
    let sb = Sandbox::new("append-prompt");
    let dir = sb.root.join("config/plugins/lab");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"lab\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("write manifest");
    std::fs::write(dir.join("main.ts"), SPOKEN_KNOB).expect("write plugin");
    sb.write_config(
        "[options]\n\"agent.model\" = \"lab/wordy\"\n\"agent.append_prompt\" = \"Be brief.\"\n",
    );

    let mut s = sb.start_letting_config_choose();
    s.wait_for("spoken ready");
    for c in ["h", "e", "l", "l", "o"] {
        s.key(c);
    }
    s.special("enter");
    s.wait_for("got[hello||Be brief.]");
    assert!(
        !s.chat_now().iter().any(|l| l.trim() == "Be brief."),
        "the instruction is on the wire, not in the transcript:\n{:?}",
        s.chat_now()
    );
}

/// A setting changed on the settings screen is in effect and in `config.toml`, and `r` takes it
/// back out of the file.
#[test]
fn the_settings_screen_saves_what_you_change_into_the_config_file() {
    let sb = Sandbox::new("settings-save");
    sb.write_config("# mine, keep this\n[options]\n\"sidebar.width\" = 40\n");
    let file = sb.root.join("config/config.toml");
    let read = || std::fs::read_to_string(&file).unwrap_or_default();

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.send(&command("settings.open"));
    s.wait_open("[settings]");
    assert!(
        s.pump(|s| s.float_named("[settings]").iter().any(|l| l.contains("Keep main checkouts up to date"))),
        "the General page names the setting people come for:\n{:?}",
        s.float_named("[settings]")
    );
    // The second row of General, one step along.
    s.key("j");
    s.key("l");
    s.drain_for(Duration::from_millis(1500));
    let text = read();
    assert!(text.contains("\"git.pull.auto\" = true"), "saved:\n{text}");
    assert!(text.starts_with("# mine, keep this\n"), "the rest of the file is left alone:\n{text}");
    assert!(text.contains("\"sidebar.width\" = 40"), "{text}");

    s.key("r");
    s.drain_for(Duration::from_millis(1500));
    let text = read();
    assert!(!text.contains("git.pull.auto"), "back to the default is the line coming out:\n{text}");
    assert!(text.contains("\"sidebar.width\" = 40"), "{text}");
}

/// A paste into a panel that has the keyboard goes to the panel, not into the composer behind it.
#[test]
fn a_paste_goes_to_the_panel_with_the_keyboard_rather_than_behind_it() {
    let sb = Sandbox::new("paste-panel");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.send(&command("settings.open"));
    s.wait_open("[settings]");
    s.key("/");
    assert!(s.pump(|s| !s.prompt_now().is_empty()), "the find field opened");
    s.send(r#"{"type":"paste","text":"auto pull"}"#);
    assert!(
        s.pump(|s| s.prompt_now().iter().any(|l| l.contains("auto pull"))),
        "the paste landed in the field:\n{:?}",
        s.prompt_now()
    );
    assert!(
        !s.composer_now().iter().any(|l| l.contains("auto pull")),
        "and not in the composer behind it:\n{:?}",
        s.composer_now()
    );
}

/// Another machine's checkout is always a block of its own, in its colour, with its worktrees
/// inside it — and when its commit is not ours, the block row says so.
#[test]
fn another_machine_is_a_block_of_its_own_and_says_when_it_is_on_another_commit() {
    const KEY: &str = "git:github.com/neoswarm/work";
    let sb = Sandbox::new("swarm-versions");
    sb.git_init();
    sb.git_remote("https://github.com/neoswarm/work.git");
    let peer = Peer::beside(&sb, "linux-box");
    // Same branch as ours, a commit we do not have.
    let mut main = theirs(KEY, "work", "/srv/checkouts/work", "/srv/checkouts/work", Some("trunk"));
    main.head = Some("deadbeef00000000000000000000000000000000".into());
    peer.publish(
        vec![
            main,
            theirs(KEY, "work", "/srv/checkouts/work/.wt/fix", "/srv/checkouts/work", Some("fix/the-thing")),
        ],
        vec![
            there("r1", "reading the parser", KEY, "/srv/checkouts/work", "/srv/checkouts/work", Some("trunk")),
            there("r2", "chasing the flake", KEY, "/srv/checkouts/work/.wt/fix", "/srv/checkouts/work", Some("fix/the-thing")),
        ],
    );

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    // Titles are clipped to the panel, so the needles are short. The commit travels with the
    // project list, which arrives on the peer's heartbeat rather than with its conversations — so
    // the wait is on the `≠` itself, not on the rows that happen to come first.
    assert!(
        s.pump(|s| {
            let rows = s.sidebar_now();
            ["@lb", "reading", "chasing", "fix/the-thing", "\u{2260} deadbee"]
                .iter()
                .all(|n| rows.iter().any(|l| l.contains(n)))
        }),
        "the machine's block arrives:\n{:?}",
        s.sidebar_now()
    );
    let rows = s.sidebar_now();
    let named = |needle: &str| rows.iter().position(|l| l.contains(needle));
    let block = named("@lb").expect("the block row");
    assert!(rows[block].contains("\u{2387} trunk"), "named by its branch:\n{rows:?}");
    assert!(rows[block].contains("\u{2260} deadbee"), "and its commit, which is not ours:\n{rows:?}");
    let conv = named("reading").expect("their conversation");
    let tree = named("fix/the-thing").expect("their worktree");
    let inside = named("chasing").expect("the conversation in it");
    assert!(block < conv && conv < tree && tree < inside, "nested, in order:\n{rows:?}");
    let indent = |row: usize| rows[row].chars().take_while(|c| *c == ' ' || *c == '\u{2502}').count();
    assert!(indent(inside) > indent(tree), "their worktree's conversation is inside it:\n{rows:?}");
    // And it is drawn in the machine's colour — the block row, and the rail down every row in it.
    // One machine is the blue, `Swarm.Host1`, and nothing about it is a local worktree's colour.
    let sidebar = s.buffer_named("[sidebar]").expect("the sidebar buffer");
    let groups = s.groups_of(sidebar);
    for row in [block, conv, tree, inside] {
        assert!(
            groups.get(row).is_some_and(|g| g.iter().any(|g| g == "Swarm.Host1")),
            "row {row} wears the machine's colour: {:?}",
            groups.get(row)
        );
    }
    assert!(
        groups.get(block).is_some_and(|g| g.iter().any(|g| g == "Swarm.Version")),
        "the commit that is not ours is marked: {:?}",
        groups.get(block)
    );
}

/// Yours first, then a section per other computer: a repository only over there is under that
/// machine's heading and nowhere else, and a checkout whose key is only a guess joins yours.
#[test]
fn another_computers_projects_are_in_its_own_section_under_yours() {
    let sb = Sandbox::new("swarm-sections");
    sb.git_init();
    sb.git_remote("https://github.com/neoswarm/work.git");
    let peer = Peer::beside(&sb, "linux-box");
    peer.publish(
        vec![
            // Theirs of *this* repository, before that machine has read its origin: a guess.
            theirs("dir:work", "work", "/srv/work", "/srv/work", Some("trunk")),
            // And one this machine has no clone of.
            theirs("git:github.com/neoswarm/faraway", "faraway", "/srv/faraway", "/srv/faraway", Some("main")),
        ],
        vec![there("r1", "over there", "git:github.com/neoswarm/faraway", "/srv/faraway", "/srv/faraway", Some("main"))],
    );

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    assert!(
        s.pump(|s| {
            let rows = s.sidebar_now();
            ["LINUX-BOX", "faraway", "over there", "\u{2387} trunk"]
                .iter()
                .all(|n| rows.iter().any(|l| l.contains(n)))
        }),
        "the machine's section arrives:\n{:?}",
        s.sidebar_now()
    );
    let rows = s.sidebar_now();
    let named = |needle: &str| rows.iter().position(|l| l.contains(needle));
    let work = named(" \u{25be} work").or_else(|| named(" \u{25b8} work")).expect("our project");
    let block = named("\u{2387} trunk").expect("their checkout of it, as a block inside ours");
    let add = named("+ Add project").expect("the add row");
    let heading = named("LINUX-BOX").expect("the machine's heading");
    let faraway = named("faraway").expect("their project");
    let conv = named("over there").expect("the conversation in it");
    assert!(work < block && block < add, "the guessed key joined ours:\n{rows:?}");
    assert!(add < heading && heading < faraway && faraway < conv, "theirs is under its heading:\n{rows:?}");
    assert_eq!(
        rows.iter().filter(|l| l.contains("faraway")).count(),
        1,
        "one row for their project — no block row repeating it:\n{rows:?}"
    );
    assert!(!rows[faraway].contains('@'), "the heading already says which machine:\n{rows:?}");
    assert!(rows[faraway].contains("\u{2387} main"), "and which branch it is on:\n{rows:?}");
    assert!(!rows[conv].contains('\u{2502}'), "no rail down the side:\n{rows:?}");
}

/// Folding a row about another machine folds it — it does not turn it into a project of yours.
///
/// The bug: the fold was written as a *project* var keyed by the row's name (`git:github.com/…`),
/// and the panel reads a project var about a name it has not seen as a directory arriving — so the
/// repository you had just folded jumped out of the machine's section and into yours.
#[test]
fn folding_a_row_about_another_machine_folds_it_and_moves_nothing() {
    let sb = Sandbox::new("swarm-fold");
    sb.git_init();
    sb.git_remote("https://github.com/neoswarm/work.git");
    let peer = Peer::beside(&sb, "linux-box");
    peer.publish(
        vec![theirs("git:github.com/neoswarm/faraway", "faraway", "/srv/faraway", "/srv/faraway", Some("main"))],
        vec![there("r1", "over there", "git:github.com/neoswarm/faraway", "/srv/faraway", "/srv/faraway", Some("main"))],
    );

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("over there"))),
        "their project arrives:\n{:?}",
        s.sidebar_now()
    );
    s.enter_panel();
    assert!(s.sidebar_seek("faraway"), "the cursor reaches it:\n{:?}", s.sidebar_now());
    s.special("enter");
    assert!(
        s.pump(|s| !s.sidebar_now().iter().any(|l| l.contains("over there"))),
        "folded:\n{:?}",
        s.sidebar_now()
    );
    s.drain_for(Duration::from_millis(1500));
    let rows = s.sidebar_now();
    let add = rows.iter().position(|l| l.contains("+ Add project")).expect("the add row");
    let faraway = rows.iter().position(|l| l.contains("faraway")).expect("still there");
    assert!(add < faraway, "still in the machine's section, not among ours:\n{rows:?}");
    assert_eq!(rows.iter().filter(|l| l.contains("faraway")).count(), 1, "and only once:\n{rows:?}");

    // And back, with the same key.
    s.special("enter");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("over there"))),
        "unfolded:\n{:?}",
        s.sidebar_now()
    );
}

impl Peer {
    /// Wait for something from the workspace under test, keeping track of who it is.
    fn wait_event(
        &mut self,
        what: &str,
        want: impl Fn(&neosh_swarm::SwarmEvent) -> bool,
    ) -> neosh_swarm::SwarmEvent {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            assert!(std::time::Instant::now() < deadline, "never saw {what}");
            let Some(event) = self.events.blocking_recv() else { panic!("the peer stopped") };
            if let neosh_swarm::SwarmEvent::PeerUp { node, .. } = &event {
                self.asker = Some(node.id.clone());
            }
            if let neosh_swarm::SwarmEvent::Catalogue { node, id, .. } = &event {
                self.handle.send(neosh_swarm::SwarmRequest::Catalogued {
                    node: node.clone(),
                    id: id.clone(),
                    result: Ok(self.catalogue.clone()),
                });
            }
            if want(&event) {
                return event;
            }
        }
    }

    /// Say something about one of this machine's conversations to whoever is watching it.
    fn stream(&self, session: &str, event: neosh_proto::StreamEvent) {
        self.handle.send(neosh_swarm::SwarmRequest::Stream {
            session: neosh_proto::SessionId(session.into()),
            event,
        });
    }
}

/// A conversation on another computer opens as a conversation: the real transcript in the pane, and
/// the composer sending to that machine.
///
/// It used to be a float of plain text with a one-line prompt behind `i`. What this pins is the
/// whole loop: the history arrives and is drawn as a transcript, what is typed and sent goes over as
/// a command, and what that machine's agent then does — the question, a tool call, the answer — is
/// drawn as the same cards and text a turn here would draw.
#[test]
fn another_computers_conversation_opens_as_one_you_can_talk_to() {
    use neosh_proto::{ContentBlock, Message, Role, StreamEvent};
    const KEY: &str = "git:github.com/neoswarm/faraway";
    let sb = Sandbox::new("swarm-mirror");
    sb.git_init();
    let mut peer = Peer::beside(&sb, "linux-box");
    peer.publish(
        vec![theirs(KEY, "faraway", "/srv/faraway", "/srv/faraway", Some("main"))],
        vec![there("r1", "over there", KEY, "/srv/faraway", "/srv/faraway", Some("main"))],
    );

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("over there"))),
        "their conversation arrives:\n{:?}",
        s.sidebar_now()
    );
    let theirs_id = peer.handle.id().0.clone();
    s.send(&format!(r#"{{"type":"command","name":"swarm.open","args":["{theirs_id}","r1"]}}"#));

    // Opening it asks for it.
    peer.wait_event("a subscription", |e| {
        matches!(e, neosh_swarm::SwarmEvent::Subscribed { session, .. } if session.0 == "r1")
    });
    let said = |role: Role, text: &str| Message {
        role,
        content: vec![ContentBlock::Text { text: text.into() }],
        at: None,
    };
    peer.stream("r1", StreamEvent::History {
        messages: vec![
            said(Role::User, "what is in here?"),
            said(Role::Assistant, "a parser and a flaky test"),
        ],
        live: None,
        interrupted: false,
    });
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("a parser and a flaky test"))),
        "the history is drawn as a transcript:\n{:?}",
        s.chat_now()
    );
    assert!(
        s.chat_now().iter().any(|l| l.contains("what is in here?")),
        "questions and answers both:\n{:?}",
        s.chat_now()
    );

    // What is typed here goes over there.
    s.type_text("run the tests");
    s.special("enter");
    let asked = peer.wait_event("the message", |e| {
        matches!(e, neosh_swarm::SwarmEvent::Command { .. })
    });
    let neosh_swarm::SwarmEvent::Command { command, session, .. } = asked else { unreachable!() };
    assert_eq!(session.0, "r1");
    assert_eq!(command, neosh_proto::AgentCommand::Send {
        text: "run the tests".into(),
        images: Vec::new(),
    });

    // And what that machine's agent does with it is drawn as a turn here would be.
    let call = neosh_proto::ToolCall {
        id: neosh_proto::ToolCallId("c1".into()),
        turn: neosh_proto::TurnId("t1".into()),
        name: "Bash".into(),
        input: serde_json::json!({ "command": "cargo test" }),
    };
    peer.stream("r1", StreamEvent::Asked { text: "run the tests".into(), images: 0, pictures: Vec::new() });
    peer.stream("r1", StreamEvent::TurnStarted { turn: "t1".into() });
    peer.stream("r1", StreamEvent::ToolStarted { turn: "t1".into(), call: call.clone() });
    peer.stream("r1", StreamEvent::ToolFinished {
        turn: "t1".into(),
        call,
        result: neosh_proto::ToolResult::ok("test result: ok. 12 passed"),
    });
    peer.stream("r1", StreamEvent::Token { turn: "t1".into(), text: "all twelve pass".into() });
    peer.stream("r1", StreamEvent::TurnEnded {
        turn: "t1".into(),
        stop_reason: neosh_proto::StopReason::EndTurn,
        usage: Default::default(),
    });
    assert!(
        s.pump(|s| {
            let chat = s.chat_now();
            chat.iter().any(|l| l.contains("run the tests"))
                && chat.iter().any(|l| l.contains("cargo test"))
                && chat.iter().any(|l| l.contains("all twelve pass"))
        }),
        "the question, the tool card and the answer:\n{:?}",
        s.chat_now()
    );
    // And the project panel says where you are: its row is lit.
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("\u{25b8} over there"))),
        "the remote row is the one you are in:\n{:?}",
        s.sidebar_now()
    );
}

/// Open another machine's conversation on the panel's own row, and a peer that has it ready.
///
/// What every test below starts from: the conversation `r1` on `linux-box`, open in this workspace's
/// pane, its history drawn.
fn opened_over_there(sb: &Sandbox, shells: Shells) -> (Session, Peer) {
    opened_over_there_with(sb, shells, |_, _| {})
}

/// [`opened_over_there`], with the conversation and the peer's catalogue set up first.
fn opened_over_there_with(
    sb: &Sandbox,
    shells: Shells,
    setup: impl FnOnce(&mut neosh_proto::AgentSummary, &mut Peer),
) -> (Session, Peer) {
    const KEY: &str = "git:github.com/neoswarm/faraway";
    sb.git_init();
    let mut peer = Peer::beside_with(sb, "linux-box", shells);
    let mut conversation = there("r1", "over there", KEY, "/srv/faraway", "/srv/faraway", Some("main"));
    setup(&mut conversation, &mut peer);
    peer.publish(
        vec![theirs(KEY, "faraway", "/srv/faraway", "/srv/faraway", Some("main"))],
        vec![conversation],
    );
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("over there"))),
        "their conversation arrives:\n{:?}",
        s.sidebar_now()
    );
    // By the row, with the keyboard, which is how anybody opens one.
    s.enter_panel();
    assert!(s.sidebar_seek("over there"), "the cursor reaches it:\n{:?}", s.sidebar_now());
    s.special("enter");
    peer.wait_event("a subscription", |e| {
        matches!(e, neosh_swarm::SwarmEvent::Subscribed { session, .. } if session.0 == "r1")
    });
    peer.stream("r1", neosh_proto::StreamEvent::History {
        messages: vec![neosh_proto::Message {
            role: neosh_proto::Role::Assistant,
            content: vec![neosh_proto::ContentBlock::Text { text: "ready when you are".into() }],
            at: None,
        }],
        live: None,
        interrupted: false,
    });
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("ready when you are"))),
        "the history is drawn:\n{:?}",
        s.chat_now()
    );
    (s, peer)
}

/// `↵` on another machine's conversation leaves the panel, exactly as it does on one of yours.
///
/// It opened the conversation and kept the keyboard in the sidebar, so the first thing typed after
/// choosing a conversation to talk to went into the panel's keys instead of into the conversation.
#[test]
fn opening_another_computers_conversation_puts_the_keyboard_in_it() {
    let sb = Sandbox::new("swarm-mirror-focus");
    let (mut s, mut peer) = opened_over_there(&sb, Shells::default());
    s.type_text("hello there");
    s.special("enter");
    let sent = peer.wait_event("the message", |e| matches!(e, neosh_swarm::SwarmEvent::Command { .. }));
    let neosh_swarm::SwarmEvent::Command { command, .. } = sent else { unreachable!() };
    assert_eq!(command, neosh_proto::AgentCommand::Send {
        text: "hello there".into(),
        images: Vec::new(),
    });
}

/// A turn over there ends here with the rows it ends with there.
///
/// Every turn's end used to rebuild the transcript from the history the owner sends back, and a
/// rebuild is the messages and nothing else: the plan the turn closed with, what it changed and what
/// it left running were drawn by the live path and then taken straight back off the screen — so a
/// conversation watched from another machine said less than the same conversation on its own.
#[test]
fn a_turn_over_there_ends_with_the_rows_it_ends_with_there() {
    use neosh_proto::{ContentBlock, Message, Role, StreamEvent};
    let sb = Sandbox::new("swarm-mirror-ending");
    let (mut s, peer) = opened_over_there(&sb, Shells::default());
    peer.stream("r1", StreamEvent::Asked { text: "make a plan".into(), images: 0, pictures: Vec::new() });
    peer.stream("r1", StreamEvent::TurnStarted { turn: "t1".into() });
    peer.stream("r1", StreamEvent::Activity {
        turn: "t1".into(),
        activity: neosh_proto::Activity::Plan {
            steps: vec![neosh_proto::PlanStep {
                text: "sharpen the pencils".into(),
                state: neosh_proto::PlanState::Done,
            }],
        },
    });
    peer.stream("r1", StreamEvent::Token { turn: "t1".into(), text: "planned it".into() });
    peer.stream("r1", StreamEvent::TurnEnded {
        turn: "t1".into(),
        stop_reason: neosh_proto::StopReason::EndTurn,
        usage: Default::default(),
    });
    assert!(
        s.pump(|s| {
            let chat = s.chat_now();
            chat.iter().any(|l| l.contains("planned it"))
                && chat.iter().any(|l| l.contains("sharpen the pencils"))
        }),
        "the answer and the plan it closed with:\n{:?}",
        s.chat_now()
    );
    // The history the turn's end brings, which is what the owner sends on the re-subscribe — and
    // then a notice, so there is something to wait on that is known to have come after it.
    let said = |role: Role, text: &str| Message {
        role,
        content: vec![ContentBlock::Text { text: text.into() }],
        at: None,
    };
    peer.stream("r1", StreamEvent::History {
        messages: vec![
            said(Role::Assistant, "ready when you are"),
            said(Role::User, "make a plan"),
            said(Role::Assistant, "planned it"),
        ],
        live: None,
        interrupted: false,
    });
    peer.stream("r1", StreamEvent::Notice {
        level: neosh_proto::MessageLevel::Info,
        text: "history landed".into(),
    });
    // Said with the machine it came from.
    s.wait_for("linux-box: history landed");
    assert!(
        s.chat_now().iter().any(|l| l.contains("sharpen the pencils")),
        "the plan is still under the answer:\n{:?}",
        s.chat_now()
    );
}

/// A conversation opened in the middle of a turn over there shows the turn as far as it has got.
///
/// An agent driver commits nothing until its whole loop is over, so the history a machine sent a
/// watcher mid-turn was the question and nothing under it — no tool calls, no words, no working
/// line — for as long as the turn went on.
#[test]
fn another_computers_conversation_opened_mid_turn_shows_the_turn_so_far() {
    use neosh_proto::{ContentBlock, Message, Role, StreamEvent};
    const KEY: &str = "git:github.com/neoswarm/faraway";
    let sb = Sandbox::new("swarm-mirror-midturn");
    sb.git_init();
    let mut peer = Peer::beside(&sb, "linux-box");
    let mut busy = there("r1", "over there", KEY, "/srv/faraway", "/srv/faraway", Some("main"));
    busy.state = neosh_proto::AgentState::Running;
    peer.publish(vec![theirs(KEY, "faraway", "/srv/faraway", "/srv/faraway", Some("main"))], vec![
        busy,
    ]);
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("over there"))),
        "their conversation arrives:\n{:?}",
        s.sidebar_now()
    );
    let theirs_id = peer.handle.id().0.clone();
    s.send(&format!(r#"{{"type":"command","name":"swarm.open","args":["{theirs_id}","r1"]}}"#));
    peer.wait_event("a subscription", |e| {
        matches!(e, neosh_swarm::SwarmEvent::Subscribed { session, .. } if session.0 == "r1")
    });
    let call = neosh_proto::ToolCall {
        id: neosh_proto::ToolCallId("c1".into()),
        turn: neosh_proto::TurnId("t1".into()),
        name: "Bash".into(),
        input: serde_json::json!({ "command": "cargo build" }),
    };
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
        - 90;
    peer.stream("r1", StreamEvent::History {
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: "build it please".into() }],
            at: None,
        }],
        live: Some(neosh_proto::LiveTurn {
            turn: "t1".into(),
            started_at: started,
            said: vec![
                neosh_proto::LiveSaid::Text { text: "halfway there".into() },
                neosh_proto::LiveSaid::Tool {
                    call,
                    result: Some(neosh_proto::ToolResult::ok("Finished dev profile")),
                },
            ],
            plan: Vec::new(),
        }),
        interrupted: false,
    });
    assert!(
        s.pump(|s| {
            let chat = s.chat_now();
            chat.iter().any(|l| l.contains("build it please"))
                && chat.iter().any(|l| l.contains("halfway there"))
                && chat.iter().any(|l| l.contains("cargo build"))
        }),
        "the question, what was said and the call it ran:\n{:?}",
        s.chat_now()
    );
    // And the rest of it arrives under what was already there.
    peer.stream("r1", StreamEvent::Token { turn: "t1".into(), text: "and it builds".into() });
    peer.stream("r1", StreamEvent::TurnEnded {
        turn: "t1".into(),
        stop_reason: neosh_proto::StopReason::EndTurn,
        usage: Default::default(),
    });
    assert!(
        s.pump(|s| {
            let chat = s.chat_now();
            let half = chat.iter().position(|l| l.contains("halfway there"));
            let rest = chat.iter().position(|l| l.contains("and it builds"));
            matches!((half, rest), (Some(a), Some(b)) if a < b)
        }),
        "the end of the turn is under its start:\n{:?}",
        s.chat_now()
    );
}

/// Another machine's rows say what its own panel says: that a turn finished unread, that one is
/// waiting on a question, and the marks its plugins put on a project — its git stats and its pull
/// request, read on that disk with that machine's credentials.
#[test]
fn another_computers_rows_say_what_its_panel_says() {
    const KEY: &str = "git:github.com/neoswarm/faraway";
    let sb = Sandbox::new("swarm-rows-say");
    sb.git_init();
    let peer = Peer::beside(&sb, "linux-box");
    let mut done = there("r1", "done one", KEY, "/srv/faraway", "/srv/faraway", Some("main"));
    done.unread = true;
    let mut asks = there("r2", "asks one", KEY, "/srv/faraway", "/srv/faraway", Some("main"));
    asks.state = neosh_proto::AgentState::Blocked;
    let mut project = theirs(KEY, "faraway", "/srv/faraway", "/srv/faraway", Some("main"));
    project.decorations = vec![serde_json::json!({
        "target": { "project": "/srv/faraway" },
        "badge": { "parts": [{ "text": "\u{2193}3", "hl": "Git.Behind" }, { "text": " " }, { "text": "#86", "hl": "Forge.Open" }] },
    })];
    peer.publish(vec![project], vec![done, asks]);

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    let row = |s: &Session, needle: &str| -> Option<(String, Vec<String>)> {
        let rows = s.sidebar_now();
        let groups = s.buffer_named("[sidebar]").map(|b| s.groups_of(b)).unwrap_or_default();
        let i = rows.iter().position(|l| l.contains(needle))?;
        Some((rows[i].clone(), groups.get(i).cloned().unwrap_or_default()))
    };
    assert!(
        s.pump(|s| row(s, "done one")
            .is_some_and(|(_, g)| g.iter().any(|g| g == "Status.Unread"))),
        "a turn nobody has read is amber, as one of yours is:\n{:?}\n{:?}",
        s.sidebar_now(),
        row(&s, "done one")
    );
    assert!(
        s.pump(|s| row(s, "asks one")
            .is_some_and(|(t, g)| t.contains('?') && g.iter().any(|g| g == "Status.Pending"))),
        "one waiting on a question says so:\n{:?}\n{:?}",
        s.sidebar_now(),
        row(&s, "asks one")
    );
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("faraway") && l.contains("#86"))),
        "the project wears the marks its own panel gives it:\n{:?}",
        s.sidebar_now()
    );
}

/// What this machine's panel wears on a project is what it advertises: another machine's row for it
/// is drawn from the very contributions drawn here, so a checkout with an untracked file in it says
/// `?1` on both screens without the other one running `git` against a disk it cannot see.
#[test]
fn a_projects_marks_go_to_the_machines_watching_it() {
    let sb = Sandbox::new("swarm-marks-out");
    sb.git_init();
    std::fs::write(sb.work().join("stray.txt"), "untracked").expect("a stray file");
    let mut peer = Peer::beside(&sb, "linux-box");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let marked = loop {
        assert!(std::time::Instant::now() < deadline, "the project never arrived with its marks");
        let Some(event) = peer.events.blocking_recv() else { panic!("the peer stopped") };
        if let neosh_swarm::SwarmEvent::PeerUp { capabilities, .. } = event
            && let Some(p) = capabilities.projects.iter().find(|p| !p.decorations.is_empty())
        {
            break p.decorations.clone();
        }
    };
    let said = serde_json::to_string(&marked).expect("json");
    assert!(said.contains("?1"), "the untracked count, as the panel here draws it: {said}");
}

/// A tiny real PNG, so what crosses the wire is decoded and kept for real.
fn png_bytes() -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([7, 8, 9, 255]));
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
        .expect("encode");
    bytes
}

/// A picture in another machine's conversation is drawn here, not named.
///
/// The transcript names a picture by its path, and a path over there is nothing this machine can
/// open — so a screenshot somebody pasted, or one the agent read, was the picture on one screen and
/// its file name on every other. The watcher asks for the ones it has not got, and the owner sends
/// only what the conversation shows.
#[test]
fn another_computers_pictures_are_drawn_here() {
    use base64::Engine as _;
    use neosh_proto::{ContentBlock, Message, Role, StreamEvent};
    let sb = Sandbox::new("swarm-mirror-pictures");
    let (mut s, mut peer) = opened_over_there(&sb, Shells::default());
    peer.stream("r1", StreamEvent::History {
        messages: vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::Image { path: "/srv/shots/screen.png".into(), media_type: "image/png".into() },
                ContentBlock::Text { text: "what is this".into() },
            ],
            at: None,
        }],
        live: None,
        interrupted: false,
    });
    let asked = peer.wait_event("the picture asked for", |e| {
        matches!(
            e,
            neosh_swarm::SwarmEvent::Command { command: neosh_proto::AgentCommand::Picture { .. }, .. }
        )
    });
    let neosh_swarm::SwarmEvent::Command { command, .. } = asked else { unreachable!() };
    assert_eq!(command, neosh_proto::AgentCommand::Picture { path: "/srv/shots/screen.png".into() });
    peer.stream("r1", StreamEvent::Image {
        path: "/srv/shots/screen.png".into(),
        media_type: "image/png".into(),
        data: base64::engine::general_purpose::STANDARD.encode(png_bytes()),
    });
    assert!(
        s.pump(|s| s.wire().contains("images/remote/")),
        "the picture is drawn from the copy here:\n{:?}",
        s.chat_now()
    );
}

/// A picture pasted into another machine's conversation goes with the words, as bytes.
///
/// It used to stay behind with a warning — `pictures stay on this computer` — so the one question
/// that most needed its screenshot was asked without it.
#[test]
fn a_picture_sent_to_another_computer_goes_with_the_words() {
    use base64::Engine as _;
    let sb = Sandbox::new("swarm-mirror-send-picture");
    let (mut s, mut peer) = opened_over_there(&sb, Shells::default());
    let shot = sb.root.join("work/shot.png");
    std::fs::write(&shot, png_bytes()).expect("write");
    s.send(&serde_json::json!({"type": "paste", "text": shot.display().to_string()}).to_string());
    s.type_text("what is this");
    s.special("enter");
    let sent = peer.wait_event("the message", |e| matches!(e, neosh_swarm::SwarmEvent::Command { .. }));
    let neosh_swarm::SwarmEvent::Command {
        command: neosh_proto::AgentCommand::Send { text, images },
        ..
    } = sent
    else {
        panic!("a message");
    };
    assert_eq!(text, "what is this");
    assert_eq!(images.len(), 1, "the picture went with it");
    assert_eq!(images[0].media_type, "image/png");
    let bytes = base64::engine::general_purpose::STANDARD.decode(&images[0].data).expect("base64");
    assert_eq!(bytes, png_bytes(), "the picture itself, not a path to it");
}

/// This machine sends a watcher the pictures its conversation shows — and nothing else on its disk.
///
/// The path in a request is a file here, and a watcher that could name any file it liked would be
/// reading this machine through a conversation. So the request is answered only for a picture the
/// transcript itself names.
#[test]
fn a_watcher_gets_the_pictures_a_conversation_shows_and_no_other_file() {
    let sb = Sandbox::new("swarm-pictures-out");
    sb.git_init();
    let shot = sb.root.join("work/shot.png");
    std::fs::write(&shot, png_bytes()).expect("write");
    let mut peer = Peer::beside(&sb, "linux-box");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.send(&serde_json::json!({"type": "paste", "text": shot.display().to_string()}).to_string());
    s.type_text("what is this");
    s.special("enter");

    // The conversation, once the roster says it has something in it.
    let (me, session) = loop {
        let e = peer.wait_event("the conversation", |e| {
            matches!(e, neosh_swarm::SwarmEvent::Inventory { agents, .. }
                if agents.iter().any(|a| a.message_count > 0))
        });
        let neosh_swarm::SwarmEvent::Inventory { node, agents, .. } = e else { unreachable!() };
        if let Some(a) = agents.iter().find(|a| a.message_count > 0) {
            break (node, a.session.clone());
        }
    };
    peer.handle.send(neosh_swarm::SwarmRequest::Subscribe { node: me.clone(), session: session.clone() });
    let history = peer.wait_event("the history", |e| {
        matches!(e, neosh_swarm::SwarmEvent::Stream {
            event: neosh_proto::StreamEvent::History { .. }, ..
        })
    });
    let neosh_swarm::SwarmEvent::Stream {
        event: neosh_proto::StreamEvent::History { messages, .. },
        ..
    } = history
    else {
        unreachable!()
    };
    let path = messages
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|b| match b {
            neosh_proto::ContentBlock::Image { path, .. } => Some(path.clone()),
            _ => None,
        })
        .expect("the question carries its picture");

    let ask = |id: &str, path: &str| neosh_swarm::SwarmRequest::Command {
        node: me.clone(),
        id: id.into(),
        session: session.clone(),
        command: neosh_proto::AgentCommand::Picture { path: path.into() },
    };
    peer.handle.send(ask("p1", &path));
    let sent = peer.wait_event("the picture", |e| {
        matches!(e, neosh_swarm::SwarmEvent::Stream {
            event: neosh_proto::StreamEvent::Image { .. }, ..
        })
    });
    let neosh_swarm::SwarmEvent::Stream {
        event: neosh_proto::StreamEvent::Image { path: said, media_type, .. },
        ..
    } = sent
    else {
        unreachable!()
    };
    assert_eq!(said, path);
    assert_eq!(media_type, "image/png");

    let elsewhere = sb.root.join("work/secret.png");
    std::fs::write(&elsewhere, png_bytes()).expect("write");
    peer.handle.send(ask("p2", &elsewhere.display().to_string()));
    let refused = peer.wait_event("the refusal", |e| {
        matches!(e, neosh_swarm::SwarmEvent::Answer { id, .. } if id == "p2")
    });
    let neosh_swarm::SwarmEvent::Answer { result, .. } = refused else { unreachable!() };
    assert!(
        matches!(result, Err(neosh_proto::Refusal::NotPermitted { .. })),
        "a file the conversation does not show is not sent: {result:?}"
    );
}

/// A turn over there that finishes with nobody watching it anywhere is news here, as one of yours
/// would be — and a conversation that stops to ask something is news too.
#[test]
fn another_computers_turn_ending_unwatched_is_a_notification_here() {
    const KEY: &str = "git:github.com/neoswarm/faraway";
    let sb = Sandbox::new("swarm-remote-alert");
    sb.git_init();
    let peer = Peer::beside(&sb, "linux-box");
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
        - 600;
    let mut busy = there("r1", "long job", KEY, "/srv/faraway", "/srv/faraway", Some("main"));
    busy.state = neosh_proto::AgentState::Running;
    busy.turn_started_at = Some(started);
    let project = || theirs(KEY, "faraway", "/srv/faraway", "/srv/faraway", Some("main"));
    peer.publish(vec![project()], vec![busy.clone()]);
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("long job"))),
        "the running conversation arrives:\n{:?}",
        s.sidebar_now()
    );
    let mut done = busy.clone();
    done.state = neosh_proto::AgentState::Idle;
    done.unread = true;
    peer.publish(vec![project()], vec![done]);
    s.wait_for("finished on linux-box");
}

/// A question the agent over there asks is asked here, and answered from here.
///
/// A conversation on a machine nobody is sitting at used to sit under a spinner until its question
/// timed out — in front of the one person who could answer it. And when it is answered anywhere
/// else first, the panel here comes down rather than staying up over a question that has gone.
#[test]
fn another_computers_agent_can_be_answered_from_here() {
    let sb = Sandbox::new("swarm-mirror-ask");
    let (mut s, mut peer) = opened_over_there(&sb, Shells::default());
    let question = |text: &str| neosh_proto::UserQuestion {
        question: text.into(),
        header: "DB".into(),
        options: vec![
            neosh_proto::QuestionOption { label: "Postgres".into(), description: String::new() },
            neosh_proto::QuestionOption { label: "SQLite".into(), description: String::new() },
        ],
        multi_select: false,
    };
    peer.stream("r1", neosh_proto::StreamEvent::Question {
        id: "q1".into(),
        questions: vec![question("Which database?")],
    });
    s.wait_for("Which database?");
    s.type_text("2");
    let answered = peer.wait_event("the answer", |e| matches!(e, neosh_swarm::SwarmEvent::Command { .. }));
    let neosh_swarm::SwarmEvent::Command { command, session, .. } = answered else { unreachable!() };
    assert_eq!(session.0, "r1");
    let neosh_proto::AgentCommand::Answer { id, answers } = command else {
        panic!("an answer, not {command:?}")
    };
    assert_eq!(id, "q1");
    let answers = answers.expect("answered, not dismissed");
    assert_eq!(answers[0].question, "Which database?");
    assert_eq!(answers[0].answers, vec!["SQLite".to_string()]);

    // Answered at the other keyboard first: the panel here goes.
    peer.stream("r1", neosh_proto::StreamEvent::Question {
        id: "q2".into(),
        questions: vec![question("Which cache?")],
    });
    s.wait_for("Which cache?");
    peer.stream("r1", neosh_proto::StreamEvent::Settled { id: "q2".into() });
    assert!(
        s.pump(|s| s.window_closed_for("[question]")),
        "the panel comes down when the question is settled elsewhere"
    );
}

/// A shell opened in another machine's conversation opens on that machine, in its directory.
#[test]
fn a_shell_in_another_computers_conversation_is_a_shell_over_there() {
    let sb = Sandbox::new("swarm-mirror-shell");
    let (mut s, mut peer) =
        opened_over_there(&sb, Shells { theirs: true, ours: false });
    s.send(r#"{"type":"command","name":"tab.new.term","args":[]}"#);
    let asked = peer.wait_event("a shell", |e| matches!(e, neosh_swarm::SwarmEvent::PtyOpen { .. }));
    let neosh_swarm::SwarmEvent::PtyOpen { cwd, .. } = asked else { unreachable!() };
    assert_eq!(cwd.as_deref(), Some("/srv/faraway"), "in the conversation's directory, over there");
}

/// An answer to a question nothing here is waiting on is refused by name, not taken silently.
#[test]
fn an_answer_to_nothing_is_refused() {
    let sb = Sandbox::new("swarm-answer-nothing");
    sb.git_init();
    let mut peer = Peer::beside(&sb, "linux-box");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    let node = peer.wait_up();
    let here = peer.wait_event("this workspace's conversations", |e| {
        matches!(e, neosh_swarm::SwarmEvent::Inventory { agents, .. } if !agents.is_empty())
    });
    let neosh_swarm::SwarmEvent::Inventory { agents, .. } = here else { unreachable!() };
    let session = agents[0].session.clone();
    assert!(agents[0].mode.is_some(), "a summary says which mode the conversation is in");
    peer.handle.send(neosh_swarm::SwarmRequest::Command {
        node,
        id: "a1".into(),
        session,
        command: neosh_proto::AgentCommand::Answer { id: "nope".into(), answers: None },
    });
    let answer = peer.wait_event("the refusal", |e| matches!(e, neosh_swarm::SwarmEvent::Answer { .. }));
    let neosh_swarm::SwarmEvent::Answer { result, .. } = answer else { unreachable!() };
    assert!(
        matches!(&result, Err(neosh_proto::Refusal::Failed { message }) if message.contains("already")),
        "refused by name: {result:?}"
    );
}

/// Another machine's conversation shows *that* machine's models: in the footer, in `^P`, and in
/// what choosing one sends.
///
/// The footer read this machine's catalogue for a model this machine may not serve, so it said
/// nothing useful, and `^P` listed this machine's providers — every choice in it a model the
/// conversation could not run on.
#[test]
fn another_computers_conversation_shows_its_own_models() {
    let sb = Sandbox::new("swarm-mirror-models");
    let (mut s, mut peer) = opened_over_there_with(&sb, Shells::default(), |conversation, peer| {
        conversation.model = Some("far-opus".into());
        conversation.instance = Some("far-cli".into());
        let credential: neosh_proto::CredentialInfo = serde_json::from_value(serde_json::json!({
            "instance": "far-cli",
            "display_name": "Far Away CLI",
            "driver": "claude-cli",
            "source": { "kind": "plan", "via": "far" },
            "account": "plan",
            "driver_available": true,
            "accepts_key": false,
        }))
        .expect("a credential");
        let model = |id: &str, name: &str, family: &str| {
            let mut info = neosh_proto::ModelInfo::undescribed(id, name);
            info.family = Some(family.into());
            neosh_proto::ModelEntry { instance: neosh_proto::InstanceId("far-cli".into()), model: info }
        };
        peer.catalogue = neosh_proto::ModelCatalogue {
            credentials: vec![credential],
            models: vec![model("far-opus", "Far Opus", "opus"), model("far-haiku", "Far Haiku", "haiku")],
        };
    });
    assert!(
        s.pump(|s| s.status_now().iter().any(|l| l.contains("far-opus"))),
        "the footer names the model that machine is using:\n{:?}",
        s.status_now()
    );

    // `^P` lists that machine's providers and models.
    s.send(r#"{"type":"command","name":"model.pick","args":[]}"#);
    s.wait_for("Far Away CLI");
    s.wait_for("Far Haiku");
    s.special("esc");

    // And choosing one tells that machine.
    s.send(r#"{"type":"command","name":"model.line","args":["haiku"]}"#);
    let chosen = peer.wait_event("the model", |e| matches!(e, neosh_swarm::SwarmEvent::Command { .. }));
    let neosh_swarm::SwarmEvent::Command { command, .. } = chosen else { unreachable!() };
    let neosh_proto::AgentCommand::SetModel { instance, model, .. } = command else {
        panic!("a model, not {command:?}")
    };
    assert_eq!((instance.as_str(), model.as_str()), ("far-cli", "far-haiku"));
}
