//! The terminal frontend: crossterm in, ratatui out.
//!
//! Everything terminal-specific is confined here. The rest of the crate works against
//! [`Mirror`](crate::mirror::Mirror) and [`InputEvent`], which is what makes the snapshot tests
//! possible and what a second frontend would replace.

use std::io::{self, Stdout};

use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange, Event,
    KeyCode as CtCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::cursor::SetCursorStyle;
use crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    disable_raw_mode, enable_raw_mode,
};
use crossterm::{execute, ExecutableCommand};
use neosh_proto::{InputEvent, KeyCode, KeyMods, KeyPress};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;

use crate::graphics::Graphics;
use crate::mirror::Mirror;
use crate::render;
use crate::theme::{ColorDepth, Theme};

/// Translate a crossterm event.
///
/// Returns `None` for events that carry nothing the protocol models — notably key *release*, which
/// on terminals with the Kitty protocol would otherwise deliver every keystroke twice.
pub fn to_input_event(ev: Event) -> Option<InputEvent> {
    match ev {
        // One notch is one notch. How many rows that *means* is the host's decision
        // (`ui.scroll_rows`), because the frontend does not know what it is scrolling — a
        // transcript, a panel, a picker — and three rows is a different distance in each.
        Event::Mouse(crossterm::event::MouseEvent { kind, .. }) => match kind {
            crossterm::event::MouseEventKind::ScrollUp => Some(InputEvent::Scroll { rows: -1 }),
            crossterm::event::MouseEventKind::ScrollDown => Some(InputEvent::Scroll { rows: 1 }),
            // Clicks, drags and motion are deliberately dropped. Capture is on for the wheel, and
            // a frontend that started acting on clicks would be one that has to answer where every
            // cell is — which is the host's map, not this one's.
            _ => None,
        },
        Event::Key(KeyEvent { code, modifiers, kind, .. }) => {
            if kind == KeyEventKind::Release {
                return None;
            }
            let code = match code {
                CtCode::Char(c) => KeyCode::Char { c: c.to_string() },
                CtCode::Enter => KeyCode::Enter,
                CtCode::Tab => KeyCode::Tab,
                CtCode::BackTab => KeyCode::BackTab,
                CtCode::Backspace => KeyCode::Backspace,
                CtCode::Delete => KeyCode::Delete,
                CtCode::Insert => KeyCode::Insert,
                CtCode::Esc => KeyCode::Esc,
                CtCode::Left => KeyCode::Left,
                CtCode::Right => KeyCode::Right,
                CtCode::Up => KeyCode::Up,
                CtCode::Down => KeyCode::Down,
                CtCode::Home => KeyCode::Home,
                CtCode::End => KeyCode::End,
                CtCode::PageUp => KeyCode::PageUp,
                CtCode::PageDown => KeyCode::PageDown,
                CtCode::F(n) => KeyCode::F { n },
                _ => return None,
            };
            // Terminals already fold shift into the character, so reporting it as well would make
            // `<S-a>` and `A` two different bindings for the same keystroke. `BackTab` is the same
            // situation spelled differently: it *is* shift-tab, arriving as its own sequence, and
            // crossterm reports the modifier as well. Keeping both means the key that arrives is
            // shift-back-tab, which is not a key anybody can write down or press.
            let shift = modifiers.contains(KeyModifiers::SHIFT)
                && !matches!(code, KeyCode::Char { .. } | KeyCode::BackTab);
            Some(InputEvent::Key {
                key: KeyPress {
                    code,
                    mods: KeyMods {
                        ctrl: modifiers.contains(KeyModifiers::CONTROL),
                        alt: modifiers.contains(KeyModifiers::ALT),
                        shift,
                        meta: modifiers.contains(KeyModifiers::SUPER),
                    },
                },
            })
        }
        // Bracketed paste arrives whole rather than as a burst of synthetic keystrokes, so a
        // pasted `<Esc>` cannot interrupt the agent.
        Event::Paste(text) => Some(InputEvent::Paste { text }),
        Event::Resize(width, height) => Some(InputEvent::Resize { width, height }),
        // Whether this terminal is the window the user is looking at, which is the one thing that
        // decides whether a notification is worth raising outside it. Only terminals that
        // implement mode 1004 send these, so silence here means "cannot say" rather than "no" —
        // see `InputEvent::Focus`.
        Event::FocusGained => Some(InputEvent::Focus { on: true }),
        Event::FocusLost => Some(InputEvent::Focus { on: false }),
    }
}

pub struct TerminalFrontend {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    theme: Theme,
    /// Whether the keyboard enhancement flags were pushed, and therefore have to be popped.
    ///
    /// Kept rather than re-asked on the way out: `supports_keyboard_enhancement` queries the
    /// terminal and waits for a reply, and doing that while tearing down — possibly on a panic
    /// path, possibly after the terminal has gone away — is how an exit hangs.
    enhanced: bool,
    /// The shape last asked of the terminal, so the escape goes out when it changes and not on
    /// every frame.
    cursor_shape: neosh_proto::CursorShape,
    /// Pictures, on a terminal that draws them. See [`crate::graphics`].
    graphics: Graphics,
    /// The size last drawn at, so the cell size is asked again only when it may have changed.
    size: (u16, u16),
}

/// Ask the terminal for unambiguous keys, if it knows how.
///
/// Without this, three things are impossible rather than merely awkward. `Cmd`/`Super` never
/// arrives at all, so a `<D-…>` binding is accepted, listed, shown in the footer and never fires.
/// `Ctrl` with anything that is not a letter collapses onto the C0 control it shares a byte with —
/// `Ctrl+/` is indistinguishable from `Ctrl+7`, which is why neither is a key anybody can usefully
/// bind. And `Esc` is ambiguous with the start of an escape sequence, which is what makes a
/// terminal wait before deciding you pressed it.
///
/// Failure is not an error. Most terminals do not implement this, `execute` on an unsupported
/// sequence is a no-op there, and the answer is simply the keyboard everyone has always had.
fn enable_enhanced_keys(stdout: &mut Stdout) -> bool {
    // An environment variable rather than an option, because this runs before there is any config:
    // the terminal has to be in the right mode before the first keystroke, and config arrives from
    // a plugin runtime that has not started yet. It exists because "my terminal claims to support
    // this and is wrong about it" is a thing that happens, and the symptom — every key arriving
    // twice, or `Esc` never arriving — is one you cannot fix from inside a program you cannot type
    // into.
    if std::env::var_os("NEOSH_NO_ENHANCED_KEYS").is_some() {
        return false;
    }
    if !matches!(crossterm::terminal::supports_keyboard_enhancement(), Ok(true)) {
        return false;
    }
    // Deliberately not `REPORT_EVENT_TYPES`: it adds key-release events, which this frontend
    // discards, so the only thing it would buy is twice the input to throw half of away.
    let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS;
    execute!(stdout, PushKeyboardEnhancementFlags(flags)).is_ok()
}

impl TerminalFrontend {
    /// Take over the terminal.
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        // Focus reporting is requested unconditionally. A terminal that does not implement it
        // never replies, which is exactly the state the host reads as "cannot say" and falls back
        // to idleness for — so there is nothing to detect and nothing to degrade.
        execute!(stdout, EnterAlternateScreen, EnableBracketedPaste, EnableFocusChange)?;
        // Whether this terminal draws pictures, asked before anything else reads from it: the
        // reply comes back on stdin, and the keyboard reader started below would take it for
        // typing.
        let graphics = Graphics::detect(crate::graphics::probe(&mut stdout));
        // The wheel, and nothing else. Without this a terminal in the alternate screen turns each
        // notch into arrow keys — which are bindings — so scrolling fired `ui.keys.prev` thirty
        // times and the screen jumped everywhere. The cost is that the terminal's own text
        // selection now needs a modifier held (Option on macOS, Shift most elsewhere), which is
        // why it is a setting: `ui.mouse = false` gives the selection back.
        let _ = execute!(stdout, crossterm::event::EnableMouseCapture);
        let enhanced = enable_enhanced_keys(&mut stdout);
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(Self {
            terminal,
            theme: Theme::new(ColorDepth::detect()),
            enhanced,
            cursor_shape: Default::default(),
            graphics,
            size: (0, 0),
        })
    }

    pub fn size(&self) -> io::Result<(u16, u16)> {
        let r = self.terminal.size()?;
        Ok((r.width, r.height))
    }

    /// Draw one frame, and report where every window ended up.
    ///
    /// The geometry goes back to the core as `ViewportChanged`. Without it a plugin cannot learn
    /// how wide its pane is, and therefore cannot size a meter, decide what to drop at 60 columns,
    /// or page by a screenful — all of which are the frontend's knowledge, not the core's.
    pub fn draw(&mut self, mirror: &Mirror) -> io::Result<Vec<crate::Geometry>> {
        // One synchronized update per frame. ratatui hides the cursor while it paints and the
        // caret is shown again only at the end, so a terminal that renders mid-frame catches it
        // hidden — at the 20 fps a spinner redraws, that is a caret blinking twenty times a
        // second. Terminals without mode 2026 ignore both escapes.
        self.terminal.backend_mut().execute(BeginSynchronizedUpdate)?;
        let frame = self.draw_frame(mirror);
        let end = self.terminal.backend_mut().execute(EndSynchronizedUpdate);
        let geometry = frame?;
        end?;
        Ok(geometry)
    }

    fn draw_frame(&mut self, mirror: &Mirror) -> io::Result<Vec<crate::Geometry>> {
        self.theme.sync(&mirror.highlights);
        let theme = self.theme.clone();
        let mut drawn = render::Drawn::default();
        let mut geometry = Vec::new();
        // How big a cell is can change with the window — a font size, a screen — and the only
        // notice is the terminal's size changing. Asked then, and not on every frame.
        let size = self.terminal.size().map(|r| (r.width, r.height)).unwrap_or_default();
        if size != self.size {
            self.size = size;
            self.graphics.refresh_cell();
        }
        let gfx = &mut self.graphics;
        self.terminal.draw(|f| {
            geometry = render::resolve_layout(mirror, f.area());
            drawn = render::draw_with(f, mirror, &theme, gfx);
        })?;
        // What the frame owes the terminal about pictures, after the cells and before the caret:
        // a placement moves the cursor to say where it goes, and the caret is put back below.
        self.flush_graphics(&drawn)?;
        // Placed after the draw, because ratatui hides the cursor for each frame it paints.
        match drawn.caret {
            Some((x, y)) => {
                self.terminal.show_cursor()?;
                self.terminal.set_cursor_position((x, y))?;
            }
            None => self.terminal.hide_cursor()?,
        }
        // And what shape it is. A bar sits between two characters and a block sits on one, which
        // is the difference between a field you are typing into and a mode where `d` is a verb —
        // the one thing on screen that answers "what will this key do" before you press it.
        // Steady rather than blinking: a transcript is something you read.
        let shape = drawn
            .caret_win
            .and_then(|w| mirror.windows.get(&w))
            .map(|w| w.cursor_shape)
            .unwrap_or_default();
        if self.cursor_shape != shape {
            self.cursor_shape = shape;
            let _ = self.terminal.backend_mut().execute(match shape {
                neosh_proto::CursorShape::Block => SetCursorStyle::SteadyBlock,
                neosh_proto::CursorShape::Underline => SetCursorStyle::SteadyUnderScore,
                neosh_proto::CursorShape::Bar => SetCursorStyle::SteadyBar,
            });
        }
        // Handed back as the protocol's own rectangle, not ratatui's: the binary wires the
        // frontend to the core and has no business depending on a rendering library.
        //
        // What a window reports is the room it has for *content*, which on a bordered float is not
        // the rectangle it was laid out into.
        //
        // `Extent` already says this from the other end — an extent measures content, and the
        // border is chrome the frontend adds afterwards — precisely so that no caller has to
        // subtract two from every number it computes. Reporting the outer box put the subtraction
        // back, on the one axis nobody thinks to check: a panel that pads its key strip down to the
        // height it was told it had ends up two lines longer than the window it is drawn in, and
        // the strip is silently off the bottom of it. Rows and columns are the same mistake in two
        // directions, so both come off here.
        let content = |win: &neosh_proto::WindowId, r: Rect| -> Rect {
            let bordered = matches!(
                mirror.windows.get(win).map(|w| &w.layout),
                Some(neosh_proto::WindowLayout::Float { config })
                    if config.border != neosh_proto::BorderStyle::None
            );
            if !bordered {
                return r;
            }
            Rect {
                x: r.x.saturating_add(1),
                y: r.y.saturating_add(1),
                width: r.width.saturating_sub(2),
                height: r.height.saturating_sub(2),
            }
        };
        // The top line comes off the frame rather than out of the mirror: a window whose scroll
        // was bent to keep its cursor on screen is showing a different first row than it was
        // asked to, and that is the number anything paging by a screenful has to page from.
        Ok(geometry
            .into_iter()
            .map(|(win, outer): (neosh_proto::WindowId, Rect)| {
                let r = content(&win, outer);
                crate::Geometry {
                win,
                rect: neosh_proto::Rect { row: r.y, col: r.x, width: r.width, height: r.height },
                top_line: drawn
                    .tops
                    .iter()
                    .find(|(w, _)| *w == win)
                    .map(|(_, (t, _))| *t)
                    .or_else(|| mirror.windows.get(&win).and_then(|w| w.top_line))
                    .unwrap_or(0),
                rows: drawn
                    .tops
                    .iter()
                    .find(|(w, _)| *w == win)
                    .map(|(_, (_, n))| *n)
                    .unwrap_or(0),
                }
            })
            .collect())
    }

    /// Write the escapes a frame's pictures need.
    ///
    /// In the placement vocabulary the pictures are drawn over the cells, so each is first cut
    /// down to what the windows painted after it leave uncovered — a float over a picture is a
    /// float, not a picture with a hole in it — and then said only if the set changed.
    fn flush_graphics(&mut self, drawn: &render::Drawn) -> io::Result<()> {
        if !self.graphics.enabled() {
            return Ok(());
        }
        let mut placed = Vec::new();
        for p in &drawn.images {
            let later: Vec<(u16, u16, u16, u16)> = drawn.layers[p.layer + 1..]
                .iter()
                .map(|r| (r.x, r.y, r.width, r.height))
                .collect();
            placed.extend(crate::graphics::uncovered(*p, &later));
        }
        self.graphics.transmit_pending();
        self.graphics.commit(&placed);
        let mut out = io::stdout().lock();
        self.graphics.flush(&mut out)
    }

    /// Put text on the system clipboard, via OSC 52.
    ///
    /// The escape rather than a clipboard library, for one reason that decides it: neosh is used
    /// over SSH, and a library talks to the X11 or Wayland display on the machine the process is
    /// running on — which is the wrong machine. OSC 52 travels back through the same stream the UI
    /// is drawn on, so it reaches the terminal the human is actually sitting at.
    ///
    /// Terminals that do not implement it ignore it silently. That is the honest failure here: we
    /// cannot detect support, and the alternative — refusing to copy on the chance it will not
    /// work — is worse than a key that does nothing on an old terminal.
    pub fn copy(&mut self, text: &str) -> io::Result<()> {
        use std::io::Write;
        // Terminals cap what they will accept, and a truncated payload is worse than a refused one:
        // you would paste a fragment and not know. 64 KiB is well inside every implementation.
        const LIMIT: usize = 64 * 1024;
        let payload = if text.len() > LIMIT { &text[..floor_char(text, LIMIT)] } else { text };
        let encoded = base64(payload.as_bytes());
        // Straight to stdout, which is the stream the backend draws on. Going through the backend
        // would need an unstable ratatui accessor for no gain: this is one escape, not a frame.
        let mut out = io::stdout().lock();
        write!(out, "\x1b]52;c;{encoded}\x07")?;
        out.flush()
    }

    /// Raise a notification outside the terminal, via whichever OSC this one speaks.
    ///
    /// The escape rather than a notification library, for the reason OSC 52 is the escape rather
    /// than a clipboard library and it is the same reason: a library talks to the D-Bus or
    /// NSUserNotificationCenter of the machine the *process* is on, and a coding agent runs on the
    /// big machine while you sit at a laptop somewhere else. This travels back up the stream the UI
    /// is drawn on, so it comes out where the person is.
    ///
    /// Terminals that implement none of these ignore all of them silently, which is the honest
    /// failure: there is no capability query for this, and refusing to notify on the chance it will
    /// not land is worse than a sequence that does nothing. `notify.desktop = "always"` is the way
    /// out for someone whose terminal is one of those.
    pub fn alert(&mut self, osc: AlertOsc, bell: bool, title: &str, body: &str) -> io::Result<()> {
        use std::io::Write;
        let title = sanitize_alert(title);
        let body = sanitize_alert(body);
        // Everything was control characters. Raising an empty notification is a notification that
        // says nothing, which is worse than the one we failed to describe.
        if title.is_empty() && body.is_empty() {
            return Ok(());
        }
        let seq = alert_sequence(osc, &title, &body);
        let mut out = io::stdout().lock();
        if !seq.is_empty() {
            write!(out, "{}", through_multiplexer(&seq))?;
        }
        // Off by default and deliberately last: a bell is the crudest possible version of this —
        // it says something happened and never which thing — but it is also the only one some
        // terminals have, and many turn it into a mark on the tab.
        if bell {
            write!(out, "\x07")?;
        }
        out.flush()
    }
}

/// The escape itself, so it can be checked without a terminal to write it to.
fn alert_sequence(osc: AlertOsc, title: &str, body: &str) -> String {
        match osc {
            AlertOsc::None => String::new(),
            // One field, so the title has to be folded into it. An em dash rather than a newline:
            // OSC 9 implementations differ on whether the body wraps, and none of them agree on
            // what a newline in it means.
            AlertOsc::Osc9 => {
                let text =
                    if title.is_empty() || body.is_empty() {
                        format!("{title}{body}")
                    } else {
                        format!("{title} \u{2014} {body}")
                    };
                format!("\x1b]9;{text}\x07")
            }
            AlertOsc::Osc777 => format!("\x1b]777;notify;{title};{body}\x07"),
            // Kitty's, which is the only one with a grammar. Two chunks because title and body are
            // separate payloads: `d=0` says more is coming, `p=body` says what the second one is.
            AlertOsc::Osc99 => format!(
                "\x1b]99;i=neosh:d=0;{title}\x1b\\\x1b]99;i=neosh:d=1:p=body;{body}\x1b\\"
            ),
        }
}

impl TerminalFrontend {

    /// Hand the terminal back.
    ///
    /// Called explicitly on shutdown *and* from `Drop`, because a panic that skipped this would
    /// leave the user's terminal in raw mode with no echo — effectively broken until they blindly
    /// type `reset`.
    pub fn leave(&mut self) -> io::Result<()> {
        disable_raw_mode()?;
        // Before leaving the alternate screen, and only if we pushed: popping a stack we never
        // pushed to would take away whatever the *shell* had set, and a terminal left in enhanced
        // mode after neosh exits is a shell where `Esc` behaves differently than it did.
        if self.enhanced {
            let _ = self.terminal.backend_mut().execute(PopKeyboardEnhancementFlags);
        }
        // The caret shape is the terminal's, not ours: a shell inheriting a block cursor because
        // neosh happened to exit while reading is a terminal that looks broken.
        self.terminal.backend_mut().execute(SetCursorStyle::DefaultUserShape)?;
        // The pictures go with the screen. A terminal keeps what it was sent until told, and a
        // shell inheriting every screenshot of the afternoon is a leak with nobody to free it.
        if let Some(bye) = self.graphics.farewell() {
            use std::io::Write;
            let _ = self.terminal.backend_mut().write_all(bye.as_bytes());
        }
        self.terminal.backend_mut().execute(DisableBracketedPaste)?;
        self.terminal.backend_mut().execute(DisableFocusChange)?;
        // Not `?`: a terminal that never enabled capture has nothing to disable, and failing the
        // whole teardown over it would leave the alternate screen up.
        let _ = self.terminal.backend_mut().execute(crossterm::event::DisableMouseCapture);
        self.terminal.backend_mut().execute(LeaveAlternateScreen)?;
        self.terminal.show_cursor()?;
        Ok(())
    }
}

/// Which escape a terminal understands as "raise a notification".
///
/// There is no way to ask. Terminals do not answer a capability query for this, so the choice is
/// made from the environment and can be overridden by `notify.osc` when the guess is wrong — which
/// it will sometimes be, because `$TERM` is routinely a lie told by a multiplexer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlertOsc {
    /// `OSC 9` — one string, no title. iTerm2, WezTerm, Windows Terminal, foot, ConEmu.
    ///
    /// The default when nothing is recognised, because it is the widest-supported of the three and
    /// an unrecognised OSC is discarded rather than printed.
    #[default]
    Osc9,
    /// `OSC 777;notify` — title and body. urxvt, foot, WezTerm, and most things that grew this
    /// from urxvt's extension.
    Osc777,
    /// `OSC 99` — kitty's, which is the only one of the three with a specified grammar.
    Osc99,
    /// Raise nothing. Not the same as the feature being off: this is a terminal we have been told
    /// cannot do it, and the desktop fallback is what covers that case.
    None,
}

impl AlertOsc {
    /// Guess from the environment.
    ///
    /// Ordered most-specific first. A multiplexer sets `$TERM` to its own thing, so the variables
    /// the *outer* terminal set are the more reliable evidence and are checked before it.
    ///
    /// `NEOSH_NOTIFY_OSC` overrides the guess, and is an environment variable for the same reason
    /// `NEOSH_NO_ENHANCED_KEYS` is: this is a fact about the terminal, and in a workspace being
    /// viewed over SSH the terminal is a process on another machine that has never read — and must
    /// not read — the workspace's configuration. `notify.osc` in `config.toml` reaches the terminal
    /// a `--no-daemon` neosh owns itself, which is the case where the two are the same machine.
    pub fn detect() -> Self {
        let env = |k: &str| std::env::var(k).unwrap_or_default();
        if let Some(named) = Self::parse_named(&env("NEOSH_NOTIFY_OSC")) {
            return named;
        }
        if !env("KITTY_WINDOW_ID").is_empty() || env("TERM").contains("kitty") {
            return Self::Osc99;
        }
        match env("TERM_PROGRAM").as_str() {
            // All three implement OSC 9. WezTerm also does 777, but doing both would raise two.
            "iTerm.app" | "WezTerm" | "vscode" | "Hyper" => return Self::Osc9,
            _ => {}
        }
        if !env("WT_SESSION").is_empty() {
            return Self::Osc9;
        }
        let term = env("TERM");
        if term.contains("foot") || term.contains("rxvt") {
            return Self::Osc777;
        }
        Self::Osc9
    }

    /// The named sequences, without `auto` — which cannot be here because `detect` calls this and
    /// `auto` would be a guess that asks to be guessed again.
    fn parse_named(s: &str) -> Option<Self> {
        match s {
            "9" => Some(Self::Osc9),
            "777" => Some(Self::Osc777),
            "99" => Some(Self::Osc99),
            "off" | "none" => Some(Self::None),
            _ => None,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::detect()),
            other => Self::parse_named(other),
        }
    }
}

/// Strip everything that could end the escape sequence early.
///
/// This text comes from an agent, a driver, a plugin or a filename, and none of those is trusted to
/// contain no control characters. An unescaped `BEL` or `ESC` in the middle of an OSC terminates it
/// there and dumps the remainder onto the screen as literal text — which corrupts the frame, and on
/// a terminal that acts on what follows is considerably worse than that. Semicolons go too, because
/// `OSC 777` separates its fields with them and a title containing one would move the body.
///
/// Length is capped for the same reason a clipboard payload is: notification daemons truncate, and
/// a truncated escape is a frame with the tail of a sentence in it.
fn sanitize_alert(s: &str) -> String {
    const LIMIT: usize = 256;
    let mut out = String::with_capacity(s.len().min(LIMIT));
    for c in s.chars() {
        if out.chars().count() >= LIMIT {
            out.push('\u{2026}');
            break;
        }
        match c {
            // Newlines become spaces rather than vanishing: a two-line message that becomes one
            // word is worse than one that reads as a sentence.
            '\n' | '\r' | '\t' => out.push(' '),
            ';' => out.push(','),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out.trim().to_string()
}

/// Wrap an escape so it survives a multiplexer.
///
/// tmux eats any sequence it does not understand rather than forwarding it, so an OSC written from
/// inside tmux reaches tmux and stops. The passthrough hands it on verbatim — with every `ESC` in
/// the payload doubled, which is how the wrapper knows where the payload ends.
///
/// It needs `allow-passthrough on` in the user's tmux configuration, which we cannot set and do not
/// try to. Unset, this is a sequence tmux discards, which is the same outcome as not sending it.
fn through_multiplexer(seq: &str) -> String {
    if std::env::var_os("TMUX").is_some() {
        return wrap_tmux(seq);
    }
    // GNU screen has the same problem and a simpler wrapper, and identifies itself the same way
    // tmux does not: by owning `$TERM` outright.
    if std::env::var("TERM").unwrap_or_default().starts_with("screen")
        && std::env::var_os("TMUX").is_none()
        && std::env::var_os("STY").is_some()
    {
        return format!("\x1bP{seq}\x1b\\");
    }
    seq.to_string()
}

#[cfg(test)]
mod alert_tests {
    use super::*;

    /// An escape that ends early dumps the rest of itself onto the screen as literal text. This
    /// text comes from an agent, a driver or a filename, so none of it is trusted.
    #[test]
    fn a_message_cannot_end_the_escape_it_travels_in() {
        let nasty = "done\u{1b}[2J\u{7}rm -rf /";
        let clean = sanitize_alert(nasty);
        assert!(!clean.contains('\u{1b}'), "no ESC: {clean:?}");
        assert!(!clean.contains('\u{7}'), "no BEL: {clean:?}");
        assert_eq!(clean, "done[2Jrm -rf /");
    }

    /// `OSC 777` separates title from body with a semicolon, so one in the title would move the
    /// body. Nothing else in the pipeline can put it back.
    #[test]
    fn a_semicolon_cannot_move_the_body_into_the_title() {
        assert_eq!(sanitize_alert("a;b"), "a,b");
    }

    /// A newline becomes a space rather than vanishing: a two-line message that becomes one word
    /// is worse than one that reads as a sentence.
    #[test]
    fn newlines_become_spaces_and_the_whole_thing_is_bounded() {
        assert_eq!(sanitize_alert("one\ntwo"), "one two");
        let long = "x".repeat(1000);
        let out = sanitize_alert(&long);
        assert!(out.chars().count() <= 257, "{} chars", out.chars().count());
        assert!(out.ends_with('\u{2026}'));
    }

    #[test]
    fn each_terminal_gets_the_shape_it_understands() {
        // OSC 9 has one field, so the title is folded into it.
        assert_eq!(alert_sequence(AlertOsc::Osc9, "fix/login", "finished"), "\u{1b}]9;fix/login — finished\u{7}");
        assert_eq!(alert_sequence(AlertOsc::Osc777, "fix/login", "finished"), "\u{1b}]777;notify;fix/login;finished\u{7}");
        assert!(alert_sequence(AlertOsc::Osc99, "fix/login", "finished").starts_with("\u{1b}]99;i=neosh:d=0;fix/login\u{1b}\\"));
        assert_eq!(alert_sequence(AlertOsc::None, "fix/login", "finished"), "");
    }

    /// tmux forwards nothing it does not understand, so an OSC written from inside it reaches tmux
    /// and stops. The wrapper doubles every ESC in the payload, which is how it knows where the
    /// payload ends.
    #[test]
    fn a_multiplexer_gets_the_escape_handed_through_it() {
        let wrapped = wrap_tmux("\u{1b}]9;hi\u{7}");
        assert_eq!(wrapped, "\u{1b}Ptmux;\u{1b}\u{1b}]9;hi\u{7}\u{1b}\\");
    }

    /// Guessing is all there is — no terminal answers a capability query for this — so the
    /// override has to win, and `auto` must not resolve to itself.
    #[test]
    fn a_named_sequence_beats_the_guess() {
        assert_eq!(AlertOsc::parse("777"), Some(AlertOsc::Osc777));
        assert_eq!(AlertOsc::parse("off"), Some(AlertOsc::None));
        assert_eq!(AlertOsc::parse("nonsense"), None);
        assert!(AlertOsc::parse("auto").is_some());
    }
}

/// The tmux passthrough itself, split out because the environment is not something a test can set
/// without racing every other test in the process.
fn wrap_tmux(seq: &str) -> String {
    format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b"))
}

/// Put the terminal back, from anywhere — including a panic hook.
///
/// A panic inside the alternate screen is invisible: the message is printed onto a screen that is
/// then torn down, so the user sees their prompt and nothing else. Every error is reported as
/// "it just doesn't start". Restoring *before* the message prints is what makes a crash legible.
pub fn restore_terminal() {
    let _ = disable_raw_mode();
    let mut out = io::stdout();
    let _ = out.execute(DisableBracketedPaste);
    let _ = out.execute(DisableFocusChange);
    let _ = out.execute(crossterm::event::DisableMouseCapture);
    // Whatever pictures were sent, freed — said to every terminal, because on this path nothing
    // remembers which kind this was, and one that draws none discards the escape.
    {
        use std::io::Write;
        let _ = out.write_all(b"\x1b_Ga=d,d=A,q=2\x1b\\");
    }
    let _ = out.execute(LeaveAlternateScreen);
    let _ = out.execute(crossterm::cursor::Show);
}

impl Drop for TerminalFrontend {
    fn drop(&mut self) {
        let _ = self.leave();
    }
}

/// The largest index at or below `at` that is a character boundary.
fn floor_char(s: &str, at: usize) -> usize {
    let mut i = at.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Standard base64, which OSC 52 requires.
///
/// Written out rather than pulled in: it is fifteen lines, it is the only thing we would use a
/// dependency for, and a base64 crate in the tree invites someone to reach for it in a place where
/// the right answer is not to encode anything at all.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[(n & 63) as usize] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventState;

    fn key(code: CtCode, mods: KeyModifiers, kind: KeyEventKind) -> Event {
        Event::Key(KeyEvent { code, modifiers: mods, kind, state: KeyEventState::NONE })
    }

    #[test]
    fn ctrl_combinations_survive_translation() {
        let ev = to_input_event(key(CtCode::Char('p'), KeyModifiers::CONTROL, KeyEventKind::Press));
        match ev {
            Some(InputEvent::Key { key }) => {
                assert_eq!(key.code, KeyCode::Char { c: "p".into() });
                assert!(key.mods.ctrl);
            }
            other => panic!("expected a key event, got {other:?}"),
        }
    }

    #[test]
    fn key_release_is_dropped_so_keystrokes_do_not_double() {
        assert!(
            to_input_event(key(CtCode::Char('a'), KeyModifiers::NONE, KeyEventKind::Release))
                .is_none()
        );
        assert!(
            to_input_event(key(CtCode::Char('a'), KeyModifiers::NONE, KeyEventKind::Repeat))
                .is_some(),
            "auto-repeat is a real keystroke"
        );
    }

    #[test]
    fn shift_is_not_reported_for_characters_that_already_encode_it() {
        let ev = to_input_event(key(CtCode::Char('A'), KeyModifiers::SHIFT, KeyEventKind::Press));
        match ev {
            Some(InputEvent::Key { key }) => {
                assert_eq!(key.code, KeyCode::Char { c: "A".into() });
                assert!(!key.mods.shift, "otherwise <S-a> and A become different bindings");
            }
            other => panic!("unexpected {other:?}"),
        }
        // For non-character keys, shift is meaningful.
        let ev = to_input_event(key(CtCode::Tab, KeyModifiers::SHIFT, KeyEventKind::Press));
        assert!(matches!(ev, Some(InputEvent::Key { key }) if key.mods.shift));
    }

    /// Regression: crossterm reports shift-tab as `BackTab` *and* sets the shift modifier, so a
    /// binding written `<S-Tab>` — which is one key press, not two things at once — never matched
    /// what arrived. It stayed listed in the key sheet the whole time, which is the worst way for
    /// a binding to be broken.
    #[test]
    fn back_tab_does_not_also_report_the_shift_that_produced_it() {
        let ev = to_input_event(key(CtCode::BackTab, KeyModifiers::SHIFT, KeyEventKind::Press));
        match ev {
            Some(InputEvent::Key { key }) => {
                assert_eq!(key.code, KeyCode::BackTab);
                assert!(!key.mods.shift);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn escape_maps_to_esc_so_it_can_reach_the_interrupt() {
        let ev = to_input_event(key(CtCode::Esc, KeyModifiers::NONE, KeyEventKind::Press));
        assert!(matches!(ev, Some(InputEvent::Key { key }) if key.code == KeyCode::Esc));
    }

    #[test]
    fn paste_arrives_whole_rather_than_as_keystrokes() {
        let ev = to_input_event(Event::Paste("multi\nline\u{1b}text".into()));
        match ev {
            Some(InputEvent::Paste { text }) => assert!(text.contains('\u{1b}')),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn resize_is_forwarded() {
        assert_eq!(
            to_input_event(Event::Resize(120, 40)),
            Some(InputEvent::Resize { width: 120, height: 40 })
        );
    }
}

#[cfg(test)]
mod clipboard_tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_test_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn multibyte_text_survives_the_round_trip() {
        // The payload is bytes, not characters: encoding UTF-8 as if it were latin-1 would paste
        // mojibake, which is the classic OSC 52 bug.
        let encoded = base64("héllo \u{4e16}\u{754c}".as_bytes());
        assert_eq!(encoded, "aMOpbGxvIOS4lueVjA==");
    }

    #[test]
    fn a_payload_over_the_limit_is_cut_on_a_character_boundary() {
        let long = "\u{4e16}".repeat(40_000); // 120 KB of three-byte characters
        assert!(std::str::from_utf8(&long.as_bytes()[..floor_char(&long, 64 * 1024)]).is_ok());
    }
}
