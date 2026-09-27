# Working on neosh

A terminal-first agent workspace: Rust core, TypeScript plugins, "the extensibility model of Neovim
and the feature set of Cursor".

The thesis is that **extensibility is the product**. Every capability ships on the same public API a
third-party plugin author would use. If a feature needs a private escape hatch, the API is wrong and
the API is what to fix.

Which means the user owns the workspace, not us: every part of it is theirs to replace, and a plugin
of their own is how they do it. New windows, panels and floats; new commands and keys on any of
them; different colours, different layout, a sidebar that is nothing like ours — none of that is a
feature we grant, it is what the API is for. Ours are the defaults, and a default is something you
turn off.

## Layout

| Crate | Owns |
|---|---|
| `neosh-proto` | Every wire type. `#[derive(TS)]` on all of them; generated `.ts` is committed. |
| `neosh-core` | Buffers, windows, floats, extmarks, highlights, focus, keymaps, the single-writer `Editor`. No I/O. |
| `neosh-script` | The `deno_core` host and the op surface. The only crate that knows deno exists. |
| `neosh-provider` | `trait Provider`, the shared SSE parser, and one driver per transport. |
| `neosh-agent` | `Session`, `Turn`, the tool registry, the permission layer, the agent loop. |
| `neosh-syntax` | Syntax highlighting: what a token *is*, never what colour it is. The only crate that knows a grammar exists. |
| `neosh-tui` | Terminal frontend. Owns **all** display-width maths. |
| `neosh` | The binary. Wires the tasks, owns config, hosts the chat — and is both the workspace (`daemon.rs`) and the terminal that views it (`client.rs`). |
| `neosh-plugins` | `plugins/` itself. One file of Rust holding the `include_dir!`s, because the tree an `include_dir!` reads has to be under the package root or it embeds here and publishes empty. |

`plugins/api/` is `@neosh/api` — generated protocol types plus a hand-written ergonomic layer.
`plugins/builtin/` are ordinary plugins that happen to ship in the binary. The directory is also a
crate (`plugins/Cargo.toml`, `plugins/lib.rs`) and nothing moved to make it one: `cargo package`
carries only what is under a package root, so a `neosh-script` embedding `../../plugins` built fine
in a checkout and would have uploaded a crate with no plugins and no API source in it. Publishing
is `docs/releasing.md`.

## Rules that are not negotiable

- **Anything neosh can put on screen, a plugin can put on screen — and take off it.** Windows,
  splits, floats, docked panels, extmarks, highlight groups, status segments, commands, keymaps and
  options are all public API, and every bundled plugin is written against exactly that: the sidebar,
  the model switcher, the palette, git, approvals. The test for a new feature is "could a third
  party have written this, and can they replace it?" — if the answer needs an op nobody else may
  call, the op is what to fix, not the feature. In practice that means a new capability lands as
  `ApiCall` + generated TS + a plugin that uses it, in that order; anything the binary draws for
  itself must also be disable-able (`plugins.disabled`) and overridable by a plugin that loads
  after it and wins.
- **Unicode width is not optional.** `unicode-width` + `unicode-segmentation`. Column maths on
  `char` counts corrupts layout on day one. Columns on the wire are UTF-8 byte offsets; motion steps
  by grapheme cluster; every display-width question is answered in `neosh-tui`.
- **No `unwrap()` on anything touching plugin input or a network response.**
- **Secrets never touch disk in plaintext and never appear in logs or event payloads.** They are
  `secrecy::SecretString`, read at request time, and structurally absent from `UiEvent`.
- **Every public API has a TS type generated from or checked against the Rust side.** Drift fails
  the build.
- **Streaming markdown caches to the last *complete* block boundary** and re-parses only the
  trailing partial block.
- **Which way a diff line went is a background; what the code says is the foreground.**
  `ExtmarkOpts::line_hl_group` bands the whole row and every ranged group is patched over it. A
  `Diff.*Line` group that sets a foreground silently un-highlights the code on it. Colours for code
  come from `neosh-syntax`, which maps grammar scopes onto `Syntax.*` and never loads a theme of its
  own — the palette owns colour, here as everywhere.
- **A panel is a surface, not a program.** Anything a bundled plugin draws, a third party must be
  able to add to and take keys in without forking it. Three mechanisms, and a new panel of ours uses
  all three or it is not finished: a buffer **kind** (`buf.create({ kind })`), which is what
  `KeymapScope::BufKind` binds against and what `win.list` finds it by; **contribution points**
  (`ext.contribute`/`ext.list`), which is how rows and verbs arrive from plugins we have never heard
  of; and **shared vars** (`vars`, scoped to the workspace, a conversation or a project) for anything
  about a thing rather than about us — `state` stays private and a favourite is not private. Every
  key in a panel is an ordinary binding pointed at a named command, so `^Z` lists it and `init.ts`
  can move it; a `switch` on `KeyContext` inside a plugin is the thing this replaced. Keep the
  capture, but only as a sink for keys nothing claimed.
- **A plugin can build on a plugin, and the four ways are named.** *Ask* it: `cmd.call` returns
  what a handler returned, and `import { api } from "plugin:<name>"` is the very module the host
  activated — declared with `requires` in the manifest, which is also what orders loading, so a
  name nothing provides fails at startup with the name in it. *Decorate* it: `<kind>.decoration`
  is a mark on a row the panel already draws, keyed by what the row is *about* and merged at draw
  time — data, never a row-renderer callback, because one slow decorator would be a panel that
  lags on `j`; sections sit `before`/`after` a named slot. *Recolour* it: highlights have owners
  and leave with them, `default: true` is `:hi default`, a window or a kind reads group names
  through a `winhighlight` map, and a theme is a contribution on `ui.theme` laid over a built-in
  variant. *Hear* it: the workspace's own events — `neosh.win.enter`, `neosh.cursor`,
  `neosh.mode`, `neosh.viewport` — are on the bus, and buffer/window vars are in memory and die
  with their buffer or window. One tier rule for keys, commands and colours — `builtin < plugin <
  user` — so `init.ts`, loaded first, still has the last word; a lower tier's command registration
  waits in the wings rather than failing. The host's own buffers have kinds
  (`neosh.transcript`, `neosh.composer`, `neosh.status`), the pickers do too, `deactivate` runs,
  `plugins.disabled` disables anything, `[activation]` holds a plugin until a command, event or
  kind asks for it, `[provides]` is how `ext.points()` knows who reads what and how a typo'd point
  gets reported, and `ListPanel` gives a third party's panel all of this for a kind and a `rows`
  function.
- **A pane is a rectangle, and a dock is measured against one.** Splitting arrived without a
  second geometry vocabulary: `WindowLayout::Docked` gained an optional `PaneId`, and a composer
  docked `Bottom` *in a pane* is the same call as one docked `Bottom` on the screen asked about a
  smaller rectangle. The tree that makes those rectangles is `PaneNode`, one level up, so nothing
  placing a status line has to reason about nesting. Two invariants, held by construction in
  `neosh_core::panes` rather than checked afterwards: **a split has at least two children**, and
  **a split is never directly inside a split of the same axis** — three panes in a row are one
  `Row` of three, or the second boundary moves a different amount from the first for the same
  keypress. Weights, never cells: a layout stored in cells has to be recomputed, badly, every time
  somebody drags the terminal's corner. The editor owns the shape and the host owns the contents,
  and `sync_panes` makes the difference true after every apply — so a pane a plugin created is
  furnished exactly like one a key created. **What was one per terminal is now one per pane**:
  `PaneState` is the transcript, the composer, the draft, the scroll, the search and the reading
  state, because every one of those was always about *one conversation being read in one place*.
  What is genuinely per terminal turned out to be two things — the status strip and `<C-c>` arming.
  `views_showing` became `panes_showing`, and `in_pane` is `in_view` one level down: a turn's
  output belongs to the pane reading that conversation, which is very often not the pane the
  keyboard is in. **Which pane has the keyboard is a decision, not a position** — so it is *sent*
  (`UiEvent::HomeChanged`) rather than derived. The frontend used to work it out as "the bottom-most
  docked window", which was exact until a terminal had two of them, and then the caret stayed in
  the pane you had just left. **And moving the keyboard into a pane is arriving in the conversation
  it is reading**: which model answers, how hard it thinks, what the context meter measures and what
  `⇧⇥` sets all resolve through the one conversation the store calls current, so a keyboard that
  moved without saying so left `^P` showing the model of the pane you had left and `^E` opening its
  options. Guarded on the store's current actually differing, because `enter` is a *history* and
  moving to the front of it on every redraw would make the sidebar's order mean "most recently
  drawn". `abandoning` counts panes for the same reason: asked per terminal, moving from one half of
  a split to the other reported the half you left as abandoned while it was still on screen a column
  away.
- **A prefix layer is affordable only if it teaches itself.** This workspace's rule is that
  inventing a prefix for one verb costs every user a concept to save one of them a keystroke —
  and panes and tabs are twenty-four verbs on a keyboard where every Ctrl-with-a-letter is already
  spoken for. `<C-w>` is Vim's, `ui.keys.window_prefix` moves it, and `ui.keys.delete_word` moved
  off it to `<C-BS>` because one key cannot both delete a word and wait to see whether a window
  verb follows: waiting is what makes every word-delete feel late. What pays for it is that
  **holding the prefix draws the list** — after `ui.keys.hint_delay` (300 ms), so typing `<C-w>v`
  fluently never flashes a panel and hesitating is answered — and the panel is built from the
  *live keymap*, so a rebinding in `init.ts` shows your letter and a plugin's verb appears with
  nobody writing glue. The tab strip carries the prefix permanently, which is where you find out
  it exists. A legend is a promise about a keyboard, so every key it prints is read back out of
  the registry rather than spelled in the renderer.
- **The tab strip is always there, and the tabs outrank the legend on it.** One row across the top
  of the main region — inside it, which is what `Dock::Top` is for: carved from the screen it would
  run across the project panel and read as the workspace's rather than as the region's. Always
  drawn, even with one tab, because a bar that appears when you make a second tab is a bar whose
  arrival moves every other row down and which nothing had ever mentioned. Numbered from one,
  because the key is `<C-w>1`. When the row is short the **legend** is what gives way, never the
  tab names: crowded out by three key names the bar said `+1` where a tab's title belongs, on the
  one row whose job is telling you what is open. The first hint is reserved
  for, since everything else is behind it. And **which pane has the keyboard is said with its
  edges** — `Pane.Active` on the borders that belong to it, tmux's answer, because four panes of
  one conversation look identical and the caret is one cell.
- **A tab belongs to a conversation, and the bar is one conversation's tabs.** A shell opened while
  reading one is *that* conversation's: it was started in its directory, about its work, and a bar
  that goes on drawing it after you have switched is a bar about the terminal rather than about what
  you are doing — four conversations in, every one of them showed the same three tabs and two of
  them were about none of it. `TabInfo::group` is the whole mechanism and is opaque to the editor,
  which only ever asks whether two of them are equal: the host fills it with a `SessionId`, and a
  plugin grouping its tabs by project would be using it exactly as intended. **A tab is filed once
  and moves only when you switch** — `arrive_in` is the one place, guarded on the conversation
  actually changing, because that is also what `rehome` calls when the keyboard merely crosses a
  split, and a group re-derived from the active pane on every pass would change bars under
  `<C-w>l`. `settle_tab_groups` is the other half and only ever fills a group that is *absent*: the
  editor makes a terminal's first tab before anything knows which conversation it is opening in, and
  a restored workspace puts one into a pane without ever arriving in it. Everything counted on the
  bar is counted in the visible tabs — `<C-w>1`…`9`, `<C-w>n`, the drag, and the last-one refusal,
  which is now a *conversation's* last tab, because closing it would land you in another
  conversation's tabs and that is a key about where you are looking deciding what you are working
  on. **Nothing is closed and nothing stops**: the tabs of the conversations you are not in are off
  the strip rather than gone, so a build left running in one is still running when you come back —
  which is why *deleting* a conversation hands its tabs to wherever each terminal landed, and
  archiving deliberately does not. A group nothing can ever show again is a pane, its windows and
  whatever is running in them, with no key on any keyboard that reaches it; an archived conversation
  is one `^T` from having its tabs back.
- **A panel you are in the middle of using has the keyboard.** `FloatConfig::modal` takes
  `KeymapScope::Global` out of the chain, and a key nothing claimed is swallowed rather than reaching
  the composer behind the float. Shadowing the keys a widget wants is the other half and not a
  substitute: a panel can name the keys it *wants* and never the ones it merely does not want to
  happen, so `^N` over the option sheet opened a conversation behind it and `^T` opened the project
  panel underneath with focus in neither. Nearer scopes still resolve, so a modal is exactly as
  rebindable as any other panel. `ui.modal_escape_keys` — `^Q` and `^R` — resolve globally anyway,
  consulted only *after* the panel has declined the key; **absent is not empty**, because the one
  thing between a broken panel and a lost terminal must not depend on somebody having declared a
  default. A modal that borrows a global key to open itself owes a binding to close itself, which is
  why `^E` shuts the sheet from inside. Answering a question is not this: `^T` is how you reach a
  question waiting in another conversation.
- **A default binding is a key every terminal sends.** Not "a key most people can press once they
  have found the setting": `F1` is brightness on a Mac out of the box, `⌥V` is `√`, and a key
  neosh never receives is a key no amount of rebinding will fix. That leaves Ctrl-with-a-letter,
  `⇧⇥`, `⏎`, `⌫` and `Esc` — and Ctrl with *punctuation* is not in the list, because a plain
  terminal cannot tell `Ctrl+/` from `Ctrl+7`. Arrows, `PgUp`, `Home` and `End` are bound wherever
  they mean something and are **never the only way** to do anything, which is also why the first
  key in a `ui.keys.*` list is the chord: the first one is what a hint row prints, and a legend is
  a promise about a keyboard. A capability that runs out of chords loses its key and keeps its
  command — `^K` runs it by name and `init.ts` can bind it — because inventing a prefix layer for
  one verb costs every user a concept to save one of them a keystroke. Alt is bound on the terms
  arrows are: only where a terminal-sendable route to the same thing already exists, so `⌥Y`
  copies the directory in chat and `yp` does it in the reader.
- **The cursor is on a character, and the frontend is the only thing that knows where that is.**
  Reading the transcript is a normal mode, so `$` is the last character rather than the space after
  it, `h` at column zero stays on its row, nothing parks past the end of one, and a selection
  contains the character the cursor is on — `v` then `y` copies a letter instead of reporting an
  empty selection. That last one is `SelectShape` on the window (`exclusive` / `inclusive` / `line`),
  not an invariant the caller re-establishes after every motion, because a motion that forgets to is
  a frame with the wrong rows lit. The motions are `crate::vim`, over plain rows, and deliberately
  not a second set of answers in `CursorMotion`: that vocabulary is the composer's and every text
  field's, and one `h` that means two things is a call whose behaviour depends on who is asking.
  **And the caret is placed from the rows that were drawn**, never from `cursor_row - top_line`:
  the transcript wraps, one buffer row is four screen rows, and the two counts agree until the
  first long paragraph and then diverge by one row per wrap — far enough in, the caret was off the
  window and therefore not drawn at all, which is a mode with no cursor in it. A window bends its
  own scroll to keep its cursor on screen and reports back what it actually drew, including `rows`,
  the buffer rows on the screen — which is what `^D` and `H`/`M`/`L` are counted in and is not the
  height.
- **A panel that does not fit is a place you move in too, and it says so on its own edge.** A float
  *asks* for a height and is given whatever the screen has, so `height: Max { n: 30 }` on a
  twenty-row terminal is ten rows of panel and twenty rows of content drawn nowhere, with no key
  pointed at it — which is what the key sheet, git status, a diff and another machine's transcript
  each were. `FloatConfig::scroll` puts one more scope in the chain while that float has focus,
  `BufKind { name: "neosh.scroll" }`, and the workspace binds the reader's motions there: `j`/`k`,
  `^D`/`^U`, `^F`/`^B`, `gg`/`G`, the arrows and the paging keys. A **scope** rather than a capture,
  so the three usual rules hold — `^Z` lists them, `init.ts` moves them, and a panel that wants `j`
  for itself binds it on *its* kind, which is nearer and wins. Below the panel's kind and above
  `Global`, so it survives a modal without ever taking a key from the thing it is scrolling. It is
  **opt-in and stays that way**: a scope beats a capture, so a picker whose filter takes printable
  characters through one would have `j` and `G` resolved out from under it, and typing a model's
  name would scroll. A panel that moves in itself already keeps its cursor on screen and says
  nothing here. What the frontend adds is the part nobody else can: **a bar in the right border
  while anything is hidden**, drawn from how many rows actually fit, which is a number known only
  after the fact and only there — in the *border*, because a scrollbar that took a content column
  would shorten every panel in the workspace including the ones with nothing to say. And the wheel
  goes to whatever has the keyboard: a notch over an opaque panel used to scroll the transcript
  behind it, so nothing on screen moved at all.
- **A key strip belongs on the border, not in the buffer.** `FloatConfig::footer` is the bottom edge,
  and it is where every legend in the workspace now lives — because written as the last row of
  content it is both the row that scrolls away exactly when it is wanted and the first row a short
  terminal clips. Both happened, to the same row: the picker's strip is the one that says how to get
  out, and on a sixteen-row screen it was the one that did not fit. It also costs a row of height,
  which every panel was paying for on every screen.
- **A list is a place you move in.** Anywhere there is a cursor over rows and no text field — the
  project panel, the transcript reader — the motions are Vim's and they take a count: `5j` is five
  rows you can *land on*, `^D`/`^U` are half of the panel's real height, `12G` is a row rather than
  twelve of anything, and a half-typed count is on screen because a first press with no visible
  effect is indistinguishable from a key that does nothing. The motion after it spends it and
  anything else ends it. Not in the core, and not in a picker: the panel keys live in `chat` mode,
  which is the mode the composer types in, so a digit that became a count globally is a digit you
  could no longer send. Two more things belong to the same list: the row you are *in* stays
  unfolded (`ListRow.expand`, and only ever one row — the cursor is what asks, everywhere else),
  and the foot is held against the bottom edge (`render({ pinned })`, padded in *drawn lines*
  because an unfolded row is several). A dock resizes in place with `win.resize` — reopening gives
  the window a new id and drops the keyboard, so a panel resized from inside itself used to throw
  you back to the composer on every press. And what a foot *says* is on the same budget: the plan
  strip is the one limit that would refuse the next request, plus anything else already critical,
  because three bars and a name and a sentence is five rows of a column whose job is your
  conversations. `⇥` steps it up to every window and then to all of it — contributed as a
  `sidebar.action` with `on: "custom"`, which is how a plugin puts a key on rows it owns rather
  than on every row in somebody else's panel.
- **Pairing is a decision each machine makes, and neither of them restarts.** A node presents its
  identity to anyone who connects — as an SSH server presents a host key — so adding a computer is
  typing an address and being shown a name and a fingerprint that came from the far end. The other
  machine is then told somebody is asking. Paired machines go in `$STATE`, never in the user's
  `config.toml`, and a `[[swarm.peers]]` entry written by hand is not the UI's to remove. A dialler
  does not announce a connection until the peer has sent it something: it sends the last message of
  the handshake and cannot tell whether the far end approved, so announcing there says "joined" and
  then "lost" every time pairing is half done. **And half done is a state with a name on it**, not
  a dial that failed: `LinkState::Waiting`, out of `SwarmEvent::Unapproved`, carrying the
  `NodeInfo` the handshake did get. Reported as a `DialFailed` with attempt 0 it rendered as
  `connecting…` — the one row with something actionable in it drawn as the one row with nothing to
  say — so adding a computer looked like a wrong address for as long as anybody watched. It does
  not spin, either: a spinner means *this* machine is working on it, and what this is waiting for
  is a person at the other end. **What a machine is called is a decision too.** A hostname is
  chosen by whoever set the box up and is `Mac` on both of them; `swarm.rename` is an SSH `Host`
  alias, kept beside the pairing in `$STATE`, substituted once where `SwarmNode` is built so every
  panel agrees, and never overwritten by the handshake that refreshes everything else. And the
  hostname it falls back to is asked of the *OS*: `HOSTNAME` is unexported under bash and unset
  under zsh, so reading only the environment named every machine in the swarm `neosh`.
- **An agent belongs to the machine it was started on.** ASCP moves *descriptions* of agents and
  *requests* to their owners, never the agent: its files, shell and credentials are there, and a
  conversation that could migrate is one whose `cwd` means something else afterwards. The owner may
  refuse anything, and enforces regardless of what it advertised in `NodeCapabilities`. ASCP does no
  NAT traversal — plain TCP, with Tailscale or the like underneath — and does not trust the network
  to say who may steer an agent: a node is its ed25519 public key, and authorisation is a list you
  wrote. `accepts_approvals` is separate from `accepts_commands` and off by default, because
  steering is a message and approving is a write to that machine's disk. **`Browse` is not a third
  permission**, though: a node that accepts commands already accepts `NewSession` with any `cwd` on
  its disk, so naming the directories that exist withholds strictly less than it has given away, and
  a knob for it would be a question nobody has the information to answer. `NodeCapabilities::browse`
  is therefore a *compatibility* flag — an unknown message tag fails the frame and takes the
  connection with it, so a directory picker aimed at an older peer would knock it off the board
  rather than come back empty. It defaults to `false`, which is what an older handshake decodes to,
  so "does not say" and "cannot" are one answer and a version bump was never needed.
- **A row that is not on this computer says which one, and the colour is the link.** The machine's
  *name* was on every remote row — up to fourteen columns of the narrowest panel in the workspace —
  and then it came off for a bare `@`, which said *not here* and nothing else: with two machines
  paired every remote row looked alike and which computer a conversation was on was a pause on the
  row and a read of the key card. So it is `@` and the machine's **short code** — `@ms` for
  `mac-studio` — three columns, worked out from the name (`machineCodes`: initials, the next
  candidate on a clash, assigned in a fixed order so a redraw never shuffles them) and yours to
  change: `@` on the row, `^T` in `^J`, the *Computers* page in settings, all writing the
  `swarm.codes` workspace var, because what you call a machine is a fact every panel should agree
  on. The code is said **once per block**, on the block's own row, and every row under it carries
  the block's colour instead. **And each machine has a colour of its own** — `Swarm.Host1`…`6`,
  `machineColors`, a hash of the node id so pairing a third machine does not repaint the first two,
  and the blue for the first — worn by its code, its `⎇` and the `·` of every conversation in its block, and by its
  rows in `^J`, in `^N`'s "which computer" and on the settings page, so the colour you learn in one
  panel is the machine in every other. The first cut had one blue for every machine, which said
  *not here* and never *where*. While a link is not up the code is `Swarm.Linking` pulsing amber
  while it is being dialled, `Swarm.Waiting` while it is a person at the far end being waited on,
  and `Swarm.Down` dim when nothing is dialling it — and its marks go to `Swarm.Down` too, because
  the colour of a place you can go is the wrong thing to draw on one you cannot. Connected is never
  the green of *finished*, which put the colour of done on rows where nothing had started. Up and down are **still**: a link that is up is a
  condition rather than an event and the mark is on every remote row at once, so motion on all of
  them is the panel vibrating. **And a row you can reach is drawn like one of yours.** Remote rows
  were grey whatever the link was doing, which made a block of conversations you could open and
  steer look disabled — the one thing the row must not say about the part of the panel you can use.
  `Sidebar.Remote` is for a machine that is *not* connected, where grey is true. Which computer by
  its full name, and what the link is doing in words, is the **key card**. **`@` rather than a
  cloud**, because a cloud is the world's mark for "not on this machine" everywhere except a
  terminal: what it actually reads as there is a cloud *service*, which is precisely the wrong idea
  about a laptop on the same desk. `ssh you@box` is a thing everybody in a terminal has typed, and
  it is ASCII, so there is no fallback glyph. And the panel is built from `swarm.nodes()` rather than
  `swarm.agents()`, which is the same list minus the machines that are not up: that filter is right
  for *what can I act on* and wrong for a list, because a machine going quiet does not take its
  conversations off your screen, it takes them out of reach.
- **Another machine is a block of its own, and never folded into ours.** The first cut put another
  machine's main checkout into the repository row, then split it out when its version differed —
  and "differed" was only ever as good as what that machine last said: an older neosh says nothing
  about its commit, a heartbeat can be late, and a conversation over there written against code this
  machine does not have sat in the list beside yours as though it were about your files. So a
  machine is always a `MachineBlock`: one row like a worktree — `@lb ⎇ trunk`, its code, its branch,
  and its full name when the column has room — with its main checkout's conversations and then its
  worktrees indented under it, their marks in its colour, and its own fold
  (`machine:<node>:<project key>`). Which version it is still matters and is still said, but only
  as news: `≠ 4f3a1c2` in `Swarm.Version` when its commit is not ours — amber, not the bold red of a
  diverged branch, because a build box one release behind is ordinary — and nothing when it matches
  or either side cannot say. Both commits come off the **files**, not out of `git`:
  `neosh_vcs::read_head` reads `HEAD`, the ref and `packed-refs`, cheap enough for
  `RemoteProject::head` on the inventory timer and for `git.heads` on every redraw.
- **Yours first, and then each other machine is a place.** `PROJECTS` is the repositories with a
  checkout on this disk and nothing else; after `+ Add project` comes one section per paired
  machine (the `machines` slot) — `@st STARFIGHTER` and a rule under it in its colour, the right
  column saying `offline`, `connecting` or `not allowed yet` when it cannot be reached — and under
  it the repositories that are **only** over there. They used to be mixed in with yours and ranked
  *above* them, because a project with no directory here sorted as rank `0`; each also said `@st`
  on its row and then again on a block row beneath it. In a machine's own section the machine goes
  without saying: the row is the name and the branch its checkout is on (`faraway ⎇ main`), and
  its conversations and worktrees hang straight off it, indented. A repository only
  elsewhere but on several machines sits under the best-connected one, with a block per machine
  inside. The heading is a row you can stand on — `↵` folds the section (`host:<node>`), `c`/`C`
  connect and disconnect, `t` is a shell in that machine's home, `n` is the path field pointed at
  it. And **a key that is only a guess joins the one project it names**: a machine that has not
  read its origin yet answers `dir:<name>`, which never matched your real key, so the repository
  you were sitting in appeared again as somebody else's — now `dir:neosh` meets the single local
  project called `neosh`, and only a single one, because a wrong merge is worse than two rows.
  **No line down the side**: a `│` rail beside every block read as a border and boxed the panel in;
  the indent says what hangs off what and the machine's colour rides on marks the rows already
  have. And **a fold of a row that is not a directory is not a project var** — `host:…`,
  `machine:…`, `git:…` go in the `sidebar.folded.elsewhere` workspace var, because a project var
  keyed by a name that is not a path was read back as a directory arriving, and the row you had just
  folded jumped into your own section as a project of yours. `sidebar.projects` holds absolute
  paths and nothing else, and anything else found there is dropped on load.
- **Another machine's conversation opens as a conversation, not as a window onto one.** It was a
  float of plain text with a one-line prompt behind `i` — no cards, no markdown, no working line —
  on the argument that the chat pane is *yours* and somebody else's conversation there would give
  the composer two meanings. What it meant in practice was that half the workspace was a
  second-class citizen of it. So `swarm.mirror` makes a **mirror**: a local `Session` with
  `mirror: Some(MirrorOf { node, session, machine })`, hidden like a placeholder (never listed,
  never saved, never where a view lands when it leaves another, dropped by `enter` when its pane
  leaves it and by `prune_mirrors` on the roster clock when a pane closes some other way), whose
  events are the other machine's stream **translated into `AgentEvent`s and handed to the same
  `on_agent_event`** a turn here goes through — so the transcript, the cards, the working line, the
  timing and the notifications are one piece of code whichever machine the agent is on, and a
  conversation over there cannot come to look different from one here. What you send goes through
  `start_turn_in`, which forwards `AgentCommand::Send` for a mirror; `Esc` forwards `Interrupt`.
  The question is drawn when the owner says it was asked (`StreamEvent::Asked`), which is the one
  order every watcher sees it in; every turn's end re-subscribes, and **the owner sends the
  history on every subscribe** (it used to be only the first, so a second viewer got nothing), so
  the copy here is re-synced to what was recorded. `ToolStarted`, `ToolFinished` and `Asked` are
  **rich** stream events and only go to a peer whose `NodeCapabilities::rich_stream` says it can
  parse them — an unknown tag fails the frame and drops the link — so an older neosh on either end
  degrades to the words live and the rest at the turn's end rather than to a dropped connection.
  Where you are is said twice: the remote row is lit `▸` in the project panel, and the status line
  carries `@lb linux-box` in that machine's colour, because the composer looks the same either way.
  **Opening one is arriving in it**: `↵` on the row leaves the panel exactly as it does on a row of
  yours, because a conversation you chose to talk to that left the keyboard in the sidebar sent
  your first sentence to the panel's keys. **And every key does to it what it does to yours, over
  there** — the mirror is intercepted at the host rather than taught to each plugin, so `^P` is
  `SetModel`, a rename is `Rename`, archiving is `Archive`, `<C-w>T` is a shell on that machine in
  its directory, `^N` starts on that machine, and the footer's model, options, mode and context
  are read off the owner's `AgentSummary` on every inventory, since either end can change them.
  **`^P` in a mirror is the owner's `^P`**: the model plugin asks what it always asks —
  `ProviderCredentials`, `AgentListModels` — and the host answers from the owner's
  `ModelCatalogue` (`AscpMessage::Catalogue`, asked as the mirror opens, cached per machine, parked
  calls answered when it lands), so the picker, `^E` and the footer's "no key" all describe the
  machine the turn runs on and no plugin learnt what a mirror is. Options travel on `SetModel`
  only when they were chosen off that catalogue, which is the one place they mean the same thing
  at both ends; signing in is refused, because a key belongs to the machine whose provider it is.
  Deleting is refused by name: nothing over the swarm destroys what it cannot put back. **A
  question is asked wherever somebody is watching** (`crate::relay`): the owner's asker is wrapped,
  a prompt that would reach a person here is also sent as `StreamEvent::Question`/`Permission`, the
  watcher raises it through its *own* `ask_user`/`permission_pre` hooks for the mirror, and the
  first answer from anywhere wins — `Settled` takes the others down through `neosh.prompt.withdrawn`,
  which the questions and approvals panels listen for. Policy runs first, so full access never puts
  a prompt on another screen; a permission answer and `SetMode` need `accepts_approvals`, because
  either is a write to that disk, and the watcher checks before drawing a picker whose answer would
  be refused. A watcher with nobody to ask sends *nothing* — answering "nobody answered" for a
  person it never asked would be the worst answer available.
- **A mirror says what the owner's screen says, so it is drawn live and rebuilt only when it
  missed something.** Every turn's end used to rebuild the mirror from the history it brings, and a
  rebuild is the messages and nothing else — the plan the turn closed with, what it changed, what
  it left running and the card timings went back off the screen, and `redraw_transcript` kept the
  old card rows, so `⇥` found the wrong card. A turn watched from its `TurnStarted` is drawn by the
  path that drew it over there (`mirror_whole`), and the history its end brings is taken as data
  and not drawn (`mirror_drawn`); a turn joined half way, or from a machine with no tool events,
  is rebuilt, by `resync_transcript`, which leaves the scroll, the cursor and the draft alone.
  **The history carries the turn in flight** — `StreamEvent::History { live: Some(LiveTurn) }`,
  the owner's `Round::said` and plan — because an agent driver commits nothing until its loop is
  over and a conversation opened mid-turn was the question and nothing under it. It carries
  `interrupted`, and it is **cut to fit a frame** (`swarm::fit_history`): a frame over the cap is
  the *link* failing, again on every reopen, so long outputs lose their middles, then the oldest
  turns go whole. A link that drops puts the mirror's turn down (`drop_mirror_turns`) and the link
  coming back resubscribes (`resubscribe_mirrors`), or the working line spun for ever over a
  transcript that had stopped. A provider error over there is `StreamEvent::Notice`, said here with
  the machine's name, and both new things are behind `NodeCapabilities::live`. **And a remote row
  wears what the owner's row wears.** `AgentSummary` carries `unread`, `interrupted`, `background`
  and a `Blocked` state stamped from `question.asking`/`permission.asking`; a watcher's
  subscription counts as somebody reading, so a turn that ends in front of one is not unread and
  opening a conversation from anywhere clears the mark everywhere. The git stats and the pull
  request are the owner's `sidebar.decoration` contributions themselves, shipped as data on
  `RemoteProject::decorations` and `AgentSummary::decorations` and merged by the watcher's panel
  exactly as its own are — so `↓3 #86 ✗2` is a `git status` and a `gh` call on *that* disk with
  *that* machine's credentials, and a third party's decoration crosses with nobody writing glue.
  **Pictures cross too, both ways, as bytes and never as paths.** A path in the owner's transcript
  is a file on the owner's disk, so drawn here it was the picture's *name*: the watcher asks for
  each one it lacks (`AgentCommand::Picture`), the owner answers with `StreamEvent::Image` **only
  for a picture that conversation's own transcript names** — anything else is refused, or a
  watcher could read that disk through a conversation — and the copies live in the mirror's own
  directory and go when it is let go. What you paste into a mirror goes on `AgentCommand::Send`'s
  `images`, kept by the owner exactly as a paste there is, and `Asked::pictures` says where so the
  question is drawn with it. **And a turn over there nobody watched is a notification here**
  (`alert_remote_turns`, off the roster): running to idle and `unread` is precisely a turn nobody
  saw end, and running to `Blocked` is one waiting for you — the owner raising them on its own
  screen told a machine in a cupboard.
- **A project is an identity, and a checkout is a machine and a path.** The panel grouped local
  conversations by directory and drew the other machines' work beside it — matched, in theory, on
  the project key, and in practice on a key the local side did not have: it was inferred from a
  remote agent's *cwd*, so the two only ever met when both machines happened to have the repository
  at the same absolute path. On any real pair of machines — `/Users/me/src/neosh` and
  `/home/me/neosh` — they never met, so the repository you were sitting in appeared **twice**: once
  as yours, and once again underneath as a project you had apparently never heard of, named `neosh ·
  fix/thing` because whichever remote conversation happened to sort first decided the name. That
  last part is the shape of the whole bug: `project_name` is a display string being used as a
  grouping key, and a display string parsed back apart is how a worktree ends up as a project with a
  branch stuck on the end of it, beside the row it belongs *under*. So there is one row per
  repository and the checkouts hang from it: `SessionInfo::project_key` is what the local side was
  missing, `AgentSummary::repo_root` and `branch` are what let a peer's tree nest rather than arrive
  as a project named after a branch, and `RemoteProject` carries both as well so that a tree with
  nothing open in it does not jump to the top level — a panel that reorganises itself when the last
  conversation in a directory ends is one you cannot learn. This machine's conversations sit directly
  under the repository and its worktrees one level down, named by their branch; every other machine
  is a block of its own beside them (see above). Two checkouts of one branch on two machines stay
  two rows: same name, different disks, different uncommitted work, and merging them would be a
  claim that is simply false. A repository wears the marker only when it
  is **not** here as well — on a swarm of two machines nearly every row is partly elsewhere, and
  what is true of every row does not earn a column on each of them; where it is news is a repository
  you have no clone of, which is the row you would otherwise press `↵` on expecting your own files.
  It keeps its own `Target` kind, because every verb bound against a project — fold, rename, archive,
  delete, remove from the list — is about a directory on *this* disk, and pointing one at a path
  that is not here is how a key deletes the wrong thing. It folds on the project *key*, and a remote
  tree on `<node>:<path>`, since there is no directory here to name either by — and that key is also
  what `same` compares, or the cursor snaps to the first of them on every redraw.
- **A verb about a machine goes on a row about that machine.** `^J` is the list of computers and is
  where one is added, renamed or removed; it is not where you find out that a link has dropped. The
  row is: a project greyed with a dim `@` is the moment somebody wants to reconnect, and sending
  them to a panel they have to already know exists is the panel answering a question with a
  scavenger hunt. So `c` connects and `C` disconnects, on any row that is about another machine, and
  the message that says it disconnected also says which key takes it back. `t` is the same argument
  one step further and is why it is not two keys: a terminal in the project under the cursor, on
  whichever machine that project is on. What a terminal is *for* is running something beside an
  agent that has just finished, and where that agent is running is exactly where the terminal has to
  be — a `t` that worked on local rows and not on remote ones would be this panel's own claim, that
  work over there is not a different kind of thing, contradicted on the one key where it matters.
- **A shell on another machine is bytes, and the emulator is on the side that draws.** Which is how
  ssh is arranged and for its three reasons: a full-screen program works, the side doing the drawing
  is authoritative about its own width, and the owner forwards a file descriptor rather than
  maintaining a screen whose size it was told about. So `term::Pipe` is the whole of the difference
  between a local terminal and a remote one and it is four methods wide — write, resize, close, and
  where the bytes come in — while the parser, the scrollback, the cursor, the cells and the key
  encoding stay in one copy, because those are the part that is hard to get right and none of it has
  anything to do with sockets. `accepts_shells` is **both** a compatibility flag and a permission,
  which is where it parts company with `browse`: that one is not a permission because a node
  accepting commands has already given away strictly more — `NewSession` takes any `cwd` on its disk
  — and a pty is not covered by anything, because `NewSession` starts an *agent*, a thing with a
  permission layer over it and a transcript somebody can read afterwards, and a prompt is a prompt.
  Off by default, checked on every open regardless of what the handshake advertised, said out loud
  on the machine it happens on, and **closed when the link closes**: a shell has no timeout and no
  idea the connection it was opened over has gone, so without that a laptop shutting its lid leaves
  a login shell running on somebody else's computer with nothing on any screen pointing at it.
- **Which computer is asked before where on it, and only when there is one to ask about.** With no
  machine paired — which is most workspaces — `^N` draws nothing extra and is the single key it has
  always been, and nor is it asked when the machines you *do* have are up and none of them has a
  checkout of the repository you are in: that question has one possible answer, and a panel that
  appears to confirm it is a keypress and a redraw spent on nothing. The moment a second computer
  that could take the work exists, which one is the first thing you would have to
  decide anyway, and asking it *first* is what makes it a decision rather than a path field you
  have to know scp's spelling to use. This computer is the first row and the cursor starts on it,
  so `^N ⏎` is unchanged. Every paired machine is a row including the ones that cannot be used,
  greyed with the reason — the path field's rule, for the path field's argument: skipping them is
  right about what can be *done* and wrong about what should be *said*. Choosing one hands you the
  path field seeded `<machine>:`, which is the state typing it would have reached, so the two
  routes cannot diverge. `n` on a project that is only elsewhere skips the question when one
  machine has it: the row already knows which machines and where on each.
- **What another machine knows about this one is a roster, not a snapshot.** `publish_inventory` was
  called when a peer connected and when a peer drove something here, and never once because somebody
  *sitting at this machine* started, renamed, archived or deleted a conversation — so every other
  computer's picture of this one was frozen at whenever it last dialled, and a project added an hour
  ago was invisible over there with nothing on screen to say why. A timer rather than a call at each
  of the places a conversation can change, because that list is long and was already wrong; it
  recomputes and compares, so an idle workspace with peers puts nothing on the wire. And what it
  advertises is the list its **own panel** shows — `sidebar.projects`, read the way
  `question.asking` is, because which places you work in is a fact about the workspace rather than
  about the panel that wrote it down. Derived from live conversations alone it was also a constant:
  every project on the list had one, so `RemoteProject::active` said `true` for all of them and a
  board drawing the flag drew one colour.
- **A picture is a row that the terminal decides the height of.** The transcript says a row is a
  picture — `ExtmarkOpts::image`, a path and a media type, the same two facts `ContentBlock::Image`
  carries — and says nothing about how tall it draws, because the answer is a division by the pixel
  size of a cell, which the terminal reports to the process attached to it (`TIOCGWINSZ`,
  `CSI 16 t`) and to nothing above the terminal boundary. So it is one buffer row that becomes
  several screen rows in `neosh-tui`, exactly as a row that wraps is, and the caret, the scroll
  and `rows` all read the one list. The row's text is the picture's *name*, which is what every
  terminal that cannot draw it shows and what `v y` copies. Support is **asked** at startup — the
  kitty graphics query, read raw off stdin before crossterm's reader exists, because a reply that
  reader took for keystrokes would be typed into the composer — and answered in one of two
  vocabularies: **placeholder cells** (kitty, Ghostty, anything under tmux), which is the right
  shape for a renderer that thinks in cells and the only one tmux can carry, or **placements** over
  the cells (WezTerm, Konsole), cut down every frame to what the windows painted after leave
  uncovered, because a float over a picture is a float. A tool that came back with a picture keeps
  it the way an edit keeps its diff — `ToolResult::images`, written into the store by the driver
  that read it — since a read of a screenshot is a read whose answer *is* the thing you wanted to
  see. `ui.images = false` leaves the mark off; `NEOSH_NO_IMAGES` is the terminal's own switch.
- `Editor::handles` is a **deny-list**. A new API call that is not added to it silently routes to
  the core.
- **A setting chosen on a screen is written down, in the file that is yours.** `opt.set` says what
  a value is *now* — `init.ts` choosing a width, `>` resizing the panel — and an editor that rewrote
  the config file every time a plugin did would be one you stopped trusting with the file. So
  saving is its own verb: `ApiCall::OptPersist` (`opt.save`) sets the value through `OptSet` first,
  so a wrong type is refused before anything is written, and then edits `[options]` in `config.toml`
  as a **document** (`toml_edit`) — every comment, section and blank line left as it was — writing
  the default as *no line at all*, checking the edited text still loads as a `Config`, and renaming
  a temporary file over the old one. Under `--clean` there is no file, and it says so rather than
  pretending. The **settings panel** is a plugin over exactly that: built from `opt.all()`, so a
  plugin's options have a section with nobody writing glue; curated `settings.page` contributions on
  top (`General` is ours); a control per declared type; and it applies and saves as you move,
  because nothing reversible asks. No Ctrl-letter is left, so it is `,` in the project panel,
  `/settings` and `^K` — and the panel's foot says `/settings`, because a capability with no key
  still has to be findable from the composer.
- **Standing instructions are said on the wire and never in the transcript.** `agent.append_prompt`
  goes on the end of the copy of the message a turn sends — `neosh_provider::append`, beside the
  spoken-option words and for their three reasons: every driver gets it, the transcript keeps what
  you typed (so it is said once per question rather than again for every question before it), and a
  tool result handed back mid-loop is nobody's message. Taken once per turn, and only when the turn
  asked something — a listening turn has no question for it to end.
- **A paste goes to the panel that has the keyboard.** It always went to the composer, so pasting a
  model's name into the picker's filter typed it, invisibly, into the message behind the picker —
  the one thing a modal promises never happens to a keystroke. A focused float is handed the text as
  `neosh.paste` (`{ win, text }`) and `onPaste` in `@neosh/api/ui` is how a panel takes it; the
  picker's filter and `prompt` take one line of it, a multi-line field takes it whole.
- **A main checkout can keep itself current, and only by a fast-forward.** `git.pull.auto` walks
  the panel's list on the fetch clock and fast-forwards each *main* checkout that tracks a branch,
  is behind and not ahead, and has nothing conflicted — the one route that adds commits and rewrites
  none — and never under a running turn, asked again after the fetch because a turn can start in
  the seconds it took. Worktrees are somebody's branch and are left alone. A tree git refuses to
  move is filed, not announced; a pull that brought something is one line, because it is news you
  did not ask for.
- **A driver's account of its own loop is not a content block.** Sub-agents, plans, compaction and
  the commands a driver accepts go in `ProviderEvent::Activity`; `TurnAssembler` never reads it. If
  something new has to be squeezed into `TextDelta` to be seen, the family is what to extend.
- **An agent driver keeps one process per conversation, keyed by `TurnRequest::conversation`.** Not
  per turn, and never one per driver — several conversations run at once, and a driver holding a
  single session id resumes the second into the first one's history. (ACP is the exception and says
  why: its session lives on the agent's side.) **And exactly one process, which means a turn that
  belongs to no conversation has to be *asked about* rather than keyed on.** A branch name, a
  thread title, a commit message — `Agent::complete` — sends an empty `conversation`, and empty is
  an ordinary map key: every one of them shared a slot that nothing ever emptied, so a workspace
  ran a second `claude` beside the one you were talking to, with the same flags and the same
  access, answering nobody, for as long as it lived. `TurnRequest::is_one_shot` is the question,
  and the answer is a slot nobody else can find and a process closed when the turn is. The other
  half is that an *interrupt* must not cost a conversation its process either: `<Esc>` asks the CLI
  to stop, and a CLI that stops is one to keep — the deadline the closing context question arms is
  not the deadline the interrupt armed, however much they share a timer.
- **What a driver calls a conversation is the conversation's, not the process's.** A vendor CLI
  keeps the history and hands back a handle; a driver that only remembers it in memory remembers it
  until the workspace stops, and then reopening a conversation starts an agent that has never heard
  of it — full transcript, empty model, and nothing on either side that reports it. It travels as
  `Activity::Resume`, is kept in `Session::resume`, and comes back as `TurnRequest::resume`.
- **A caller with no screen gets the API, not a vocabulary of its own.** `neosh agent` sends
  `ApiCall` over the socket a terminal attaches to and reads `ApiResponse` back
  (`ClientMessage::Call`), and subscribes to the very `PluginEvent` stream a plugin observes
  (`ClientMessage::Subscribe`) — so what a script can do is what a plugin can do, and a capability
  added for one arrives in the other with nobody writing glue. A `ControlRequest` enum listing the
  verbs a script may use is a second list to keep in step, which means a surface that is one release
  behind the one it mirrors; `CmdCall` and `CmdList` are why a *plugin's* verbs are scriptable
  without a subcommand each. **A control connection is not a view**: not in `Clients`, never sent a
  frame, never takes a terminal over, and never "somebody is watching" — which decides where a
  notification goes, so ten scripts polling must not make a workspace think a person is at it. It is
  not gated either: the socket is `0600` in the user's own runtime directory and does not leave the
  machine, so reaching it is already being the person at the keyboard, and the permissions that
  matter — the agent's — are unchanged. What is refused is what finishes only when a person does
  something (`ProviderSetCredential`, a clipboard paste) or when another machine answers
  (`SwarmProbe`, `SwarmCommand`), by name and with the fix, because a call that returns `ok` having
  done nothing is the worst answer available. Fanning out is `SessionNew { activate: false }` —
  creating a conversation from a script must not move the transcript out from under somebody
  reading one — and `neosh "a prompt"` therefore asks to attach *into* a named conversation
  (`Attach { session }`) rather than trusting that the newest one is where an arriving terminal
  lands: a workspace that has been sitting detached keeps a screen for whoever comes back, and that
  screen is somebody else's.
- **The workspace is a process; the terminal is a viewer of it.** `neosh` attaches to the workspace
  for your config directory, starting one if there is none. Closing a terminal detaches — turns keep
  running. `neosh stop` is what ends a workspace, and `^Q` must never be able to. `UiEvent` is a
  delta stream, so anything a client needs on reattach has to be *kept* by the editor and said again
  by `Editor::republish`; a value that is only forwarded is a value that comes back wrong.
- **The workspace you are looking at is not necessarily the binary you just built.** `cargo run`
  unlinks `target/debug/neosh`, writes a new one and then *attaches to the workspace already
  serving this config directory* — which goes on executing the inode it started with, now marked
  `(deleted)`. Every plugin runs there, the sidebar included, so a rebuilt panel is drawn by
  last night's code and nothing on screen says why: the change is what gets debugged, and the
  change was never the problem. `PROTOCOL_VERSION` refuses the pair that cannot talk; `BuildId`
  is the pair that can and should not both be trusted. It is a **notice, never a refusal** —
  everything works, and locking somebody out of their own running turns because a file changed
  is the worse answer — it is **captured at startup** or there is nothing left to stat by the
  time it is asked for, and the **terminal** is what says it, because a stale workspace is
  running the only code it has ever had and cannot know it is behind. An unreadable stamp is
  *unknown* rather than old, or the warning fires every time and stops meaning anything.
- **What the agent produced is the workspace's; where you are looking is yours.** Attaching joins,
  it does not take over, and it does not make a copy: every terminal has a **view** of its own —
  its own conversation on screen, transcript, scroll, cursor, folded cards, composer and panels —
  while the conversations, the turns running in them and everything a plugin registered are shared.
  A window belongs to exactly one view and a buffer does not, which is the whole mechanism;
  `neosh_core::Window` has always held the cursor, the anchor and the scroll, so the window map
  only had to gain an owner. A **client** is a socket and a **view** is a set of windows: `^Q`
  closes the client it was pressed in, read off the tag input arrives with and never from whoever
  spoke most recently, because the host can be behind the channel. A frame is therefore *routed* —
  `push_ui` reads the tag off the window an event names, and a terminal whose share is only the
  trailing flush is not woken up at all. A transcript is per view rather than per conversation
  because folding a card open is navigation: `⇥` on a diff must not move the rows under somebody
  else in the same conversation. `on_screen` is gone and `views_showing` replaced it — a turn draws
  once in each terminal showing its conversation, which may be none, and that is when the unread
  mark is set. A terminal attaching lands in the most recent conversation nobody else is reading;
  the last screen of all is kept for whoever comes back, which is what makes leaving mid-answer and
  returning find the answer still arriving. **The smallest-terminal rule is gone** — it existed
  because a card was shared content — and what is still merged is one window two terminals share.
  `republish` says only what the arriving view can see and has to stay idempotent, and
  `ViewportChanged` is still where the host learns a width.
- **A plugin says which terminal it is drawing into, and almost never has to.** Three rules in
  order: a float anchored to a window goes where that window is; a buffer only one terminal shows
  names it; otherwise it is the terminal being served, which for anything done in answer to a key
  is exact. So every float that builds its window per invocation — the palette, the model sheet,
  git — needed no change. A **dock** is the exception, because one that exists once exists in one
  place: `view.onOpen` is when to make it (fired for the terminals already here as well as the ones
  still to come) and `view.onClose` is when to let go. When it does have to be said, `KeyContext`
  carries the view, `win.open`/`float.open`/`session.switch` take one, and a command handler's
  third argument is the whole `neosh` namespace bound to the terminal the key was pressed in — so
  `here.win.open(...)` lands where the person pressing the key is looking. `SessionChanged` names
  the terminal that moved, because "the active conversation" has as many answers as there are
  screens; a question waits until some terminal is reading its conversation and opens there.
- **Every view gets every event.** `Agent` fans its stream out to one queue per view. Never a
  `broadcast` channel: lagging consumers drop the oldest, silently, under load — and a view that
  missed one `ToolFinished` has a card that spins forever with nothing to put it right.
- **A setting you cannot send is still a setting, and every model has some.** A knob is a
  `ProviderOptionDescriptor` the *driver* advertises — discovered where an endpoint says what it
  takes (OpenRouter's `supported_parameters`, `codex`'s `model/list`), written down where it will
  not — and a provider with no knobs listed is a bug in a catalogue, not a model without options.
  Some of them are words rather than parameters: `ultrathink` and `ultracode` are read by a
  *harness*, so they live on `claude-cli` and never on `anthropic`, they are **two rungs past the
  top of the same ladder** rather than a level and a switch beside it — one message carries one
  word, and a slider marked "how hard should it think" with an `off / on` under it is two questions
  about one decision — they travel as `prompt_injected_values`, and the injection happens once,
  above every driver, on the copy of the message this turn sends — never on the transcript, never on a tool
  round, and always with a sendable value put back in the selection's place. `^E` shows *all* of
  them at once and applies as you move, because a knob you cannot see is a knob you do not have.
- **`ENOENT` from a spawn is two different sentences, and the wrong one sends people hunting.**
  `Command::spawn` reports a working directory it cannot `chdir` into exactly as it reports a
  program it cannot find — "no such file or directory", os error 2 — and every driver wrote that up
  as `could not run "claude"`. So a conversation whose directory had gone (a worktree removed by
  hand, a project moved, a volume unmounted) failed with a sentence about `claude`, while every
  other conversation in the same workspace went on working, because they are the same binary in a
  directory that still exists. The one thing on screen said the one thing that was not the problem,
  and the only way through it is to already know this about `posix_spawn`. `drivers::spawn_failed`
  is the fix and it is shared by every driver: on the failure path only, ask the two questions the
  kernel answered with one bit — is the directory there, is the program on `PATH` — and name
  whichever is actually missing. The directory is checked first, because a conversation far more
  often outlives its checkout than its agent.
- **Take the transport that can do the most.** `claude` in stream-json mode with the control
  protocol; `codex app-server`, not `codex exec`; `agy -p= --input-format stream-json`, not the
  540 MB ACP runtime Google also ships — a vendor CLI the user already signed into beats a
  binary neosh would have to fetch, verify and keep updated on their behalf. The cheaper one is not simpler for long — it is
  the one where streaming, approvals and interrupts turn out to be impossible rather than missing.
- **The composer is the field; a menu over it is a suggestion about what is in it.** Anything that
  completes what is being typed mirrors it back with `onQuery`, matches on the *name* rather than
  fuzzily over descriptions — an accept key that runs things must never be pointed at a row nobody
  aimed at — and sits on `Anchor::Dock { dock: Bottom }`, which places a float flush against the
  strip so no caller subtracts its own height.
- **A conversation owns its permission mode**, and `None` means "nobody chose here, use the
  configured one" rather than a mode of its own. The shared `PermissionLayer` therefore holds the
  *configured* mode and is never written by `⇧⇥` — the moment it follows the active conversation,
  every conversation without a mode inherits the last one you touched. The configured default is
  full access.
- **A question is not a permission.** *May this happen* has one question, a yes-or-no answer, and a
  policy that can answer it without waking anybody; *which of these* has a list of them, sometimes
  several answers each, sometimes an answer nobody listed, and nothing in a mode or an allow-list
  knows which database you want. Routed through the permission path — which is where
  `AskUserQuestion` was — the configured `allow` said yes before anybody was asked, the bare yes
  carried no answers, and `claude` told its own model *"The user did not answer the questions."* So
  a question is its own family end to end: `UserQuestion` on the wire, `QuestionAsker` beside
  `PermissionAsker`, `ask_user` for a plugin to serve and `neosh.ask` for a plugin to raise, and a
  `DriverQuestioner` that does not *hold* a permission layer rather than one that declines to call
  it. **The question text is its own key** — the agent looks an answer up by the question it answers,
  so a generated id is an answer to nothing — and **nobody answering is a denial with a sentence in
  it**, never an `allow` with an empty map, because the empty map is what the agent reads as
  "ignored me".
- **A request nobody answers is a turn that never ends.** The control protocol blocks: the CLI waits
  for a `control_response` carrying the id it sent, for as long as that takes. So the two
  classifiers over one pipe must never both stand aside — `can_use_tool` asks *"would
  `ask_user_question` claim this?"* rather than testing the flag itself, because
  `requires_user_interaction` says *somebody has to see this*, not *this is a question*.
  `ExitPlanMode` sets it and carries a **plan**, so the permission path stood aside for the question
  path and the question path declined it for want of a question, and a conversation sat under
  `Running ExitPlanMode… 93m 15s` — a spinner nothing on screen could tell from work. Leaving plan
  mode is an ordinary permission, asked even under `--dangerously-skip-permissions`, and full access
  says yes to it. The other half is that a line reaching the bottom of the reader is **refused out
  loud** by request id rather than dropped: a refusal ends a turn badly and in the transcript,
  silence does not end it at all, and only one of the two is recoverable from the keyboard.
- **Everything irreversible asks, and nothing reversible does.** No exceptions on either side: a
  dialog that appears for some deletes and not others is a key whose behaviour you cannot predict
  from the row it is pointed at, and one charged for an action you can undo is what teaches people
  to clear dialogs without reading them. The question says what is at stake — how much, where, and
  what the alternative is — the destructive answer wears `Diagnostic.Error`, and the cursor starts
  on the one that changes nothing. `ui.confirm_destructive = false` is the way out, and it is the
  user's setting rather than the program's guess.
- **A worktree lives inside its project, and a display string is not a grouping key.** The sidebar
  nests a worktree under the repository it is a tree of, named by its branch — four scratch trees
  of one repository are not four projects — and it groups on `SessionInfo::repo_root`, never by
  parsing `neosh · fix/thing` back apart. Each nested tree is an ordinary project row: its own
  fold, rank and `n`; counts, unread marks and recency fold upward into the repository's row. A
  *relative* `worktree.root` puts trees inside the repository as `<repo>/<configured>/<branch>` —
  no `<repo>` level, nothing else's trees can land there — and `add_worktree` writes the directory
  into the repository's `.gitignore` (tracked, so every clone gets it; skipped when already
  ignored), or every `git status` reads as one giant untracked directory.
- **A branch is named by what you asked for, once you have asked.** A worktree you did not name
  starts on `wily-nimbus-7hq2`, because naming a branch before the work is a decision made at the
  worst possible moment — and the first message sent in it *is* that decision, arriving on its own, so
  the branch is renamed from it and never touched again. Only a name nobody chose: the mark is a
  session var written by whoever generated it (`git.branch.scratch`), never a pattern match on the
  shape of the name, which cannot tell `brisk-otter` we picked from `brisk-otter` you typed. At
  turn *start*, because the panel is drawn all through a turn — `git branch -m` is one ref write,
  so it is safe under a running agent and safe on uncommitted work. **One attempt, ever**: the mark
  is spent before the model is asked, or a cheap model's hiccup becomes a request per message —
  which is exactly why **an answer that arrives without its envelope has to count as an answer**.
  Asked for `{"branch": …}`, a model replies `fix/tab-strip-missing-in-worktree` on its own about
  one time in ten: it did the work and skipped the wrapper, `gen.json` read that as a failed
  request, the one attempt was already spent and the `log.info` that said so prints nowhere. Two
  worktrees in twenty-two kept the name nobody chose, for good, with the name they should have had
  sitting in the reply. `gen.field` is the fix and it is the host's, beside `extract_json` and for
  the same reason — every generating plugin has this problem and none should solve it twice. **Only
  for a value a wrong answer is recognisable in**, though: a branch name is checkable and one
  `git branch -m` from being fixed, while a *title* is any short line and so is `(mock provider has
  no script)` — pointed at titles this named thirteen test conversations after a driver's error
  message. Where nothing tells an answer from a remark, the envelope is the evidence and
  `gen.json` is the call. It is
  also why the turn-start listener is registered **before** the plugin's own first reads: what
  follows it is a `git status` per project, and the turn it exists for is the first one in a
  worktree that was made seconds ago. The
  *type* is the model's — `feature/`, `fix/`, `chore/` — so `git.branch.prefix` applies only to a
  name that arrived without one, or you get `feature/fix/the-login` under a type that is now wrong.
  `git.branch.model` is unset by default and the plugin sends **no selection at all** rather than a
  guess, so the fallback stays the host's: `gen.model`, then the model you are already talking to.
  And the host has to put *its own* label right — `ProjectFacts` is asked once per directory and
  read on every redraw, so `GitRenameBranch` is the one git write handled on the host loop, which
  is what makes the sidebar row follow instead of saying `wily-nimbus` until restart. The directory
  keeps the old name on purpose: moving it would invalidate the conversation's `cwd`.
- **A worktree occupies two namespaces, and the one that outlives it is the directory.** Which is
  precisely what the rule above arranges: the branch is renamed and the directory is not, so
  `git branch` stops mentioning `wily-nimbus` while `.worktrees/wily-nimbus` sits there forever —
  and a branch list, which is what a generated name was checked against, is therefore not an answer
  to *is this name free*. What that looked like from the keyboard is the shape this class of bug
  always has: a key that works nineteen times and then fails with `already exists`, naming
  something nothing on screen had said was taken. So both are asked — the branches, and the
  directory listing beside the tree — and the name carries a **tag** (`brisk-otter-k3f9`) because
  two words from two short lists are 1,672 names, and thirty trees in, a repository is about one in
  four to have drawn the same one twice. Sayable *and* unique: the words are what you read out,
  the tag is what keeps them apart, and neither is load-bearing on its own. When a directory is in
  the way anyway — a tree removed by hand, a listing longer than the completion cap — the **branch**
  is what you asked for and the directory is only where it went, so the counter goes on the
  directory (`feat/thing` in `feat-thing-2`) and never on the branch.
- **A number about a remote has an *as of*, or it is not a number about a remote.** `ahead` and
  `behind` come out of `git status --porcelain=v2 --branch`, which compares HEAD with the
  remote-tracking ref **on this disk** — so every panel drawing them was reporting the state of the
  world as of whenever this checkout last spoke to a server, and none of them had a call available
  to become right. `ApiCall::GitFetch` is that call, it answers with the fresh `RepoStatus` rather
  than `Unit` because "where do I now stand" is the question actually being asked, and it is gated
  as a **write** for the network rather than for the working tree: nothing in the tree moves, and it
  contacts a remote and writes refs, which is not something a plugin barred from `git pull` should
  be able to have the workspace do on its behalf. It is safe on a timer only because `run` closes
  stdin and unsets every askpass, so no credentials means a failure in a moment rather than a hang
  nobody can see — and a failure is *filed*, not announced, because a workspace fetching every three
  minutes on a train would otherwise be a toast every three minutes about a network you know is not
  there. What is said instead is the age beside the numbers: `now`, `12m`, `⚠ offline`. Without it,
  a panel that has not fetched since Tuesday and one that fetched nine seconds ago draw the same
  row.
- **A row that is talking to a remote says so on the row, and stops lying the moment it is done.**
  Two halves, both about the badge after a project's name. The git plugin keeps a count of
  checkouts with a fetch or a pull out (`repoBusy`), and while it is non-zero the badge is *led*
  with one glyph in `Git.Fetching` — a `Frames` spinner now, because the shimmer it used to be is
  invisible over a single cell — and that glyph is the whole of the short form, since a narrow row
  should keep the mark that says something is happening over the one that says what was true
  before it started. One column, held by the frontend, so `↓3` after it never moves. The other
  half is that the badge is built from a `git status` per project that nothing in the verbs
  refreshed: after `p` brought three commits in, the row went on saying `↓3` until a cursor move
  or a conversation switch happened to redraw it — wrong about the one number the key was pressed
  for. So the verbs *tell* the badge: `fetchRepository` and `pullRepository` fire the plugin's
  `onMoved` hook on the way in and on the way out, naming the directory and passing the fresh
  `RepoStatus` when they have one, and `decorate(only, known)` redraws that row alone, from that
  answer, with no subprocess. A verb with no `cwd` resolves the conversation's first, because
  everything said about a row is keyed by directory and `git.pull` from the palette is about a
  directory too. Corner progress is kept for what has no row; it is not a substitute for the row.
  **And it spins from the press, not from the network.** `p` used to do all of its deciding first —
  a `git status` over the whole tree, then perhaps a fetch — and only mark the row busy once it had
  settled on a route, so on anything bigger than a toy checkout the key visibly did nothing for a
  second, which is indistinguishable from a key that is not bound. What somebody wants from `p` is
  *that it heard them*; what it turns out to do is the second question. So `busy(cwd)` is claimed
  up front and answers with the one call that releases it — a releaser rather than a second
  counter, so a `finally` can call it over a path that already released — and it is released around
  the *question*, because nothing on this machine is working while a person reads two options and a
  spinner over that means nothing anywhere else either. The other half is that the redraw a
  *starting* operation asks for must cost nothing: `decorate` keeps the last `RepoStatus` per
  directory and draws from it while the checkout is busy rather than running a `git status` first,
  since the numbers a spinner is beside are exactly the ones the operation is about to change.
- **`/update` is the whole job on every kind of install.** It used to stop at `Update with: brew
  update && brew upgrade neosh` for anything a package manager owned, on the theory that driving
  somebody's package manager behind a keypress is how a machine ends up in a state its owner
  cannot explain — and from the keyboard that meant the most common install of all did not
  update, told you to go and type something elsewhere, and then wanted a second `/update` to
  restart into what you had typed. So `UpdateApply` **runs the manager**:
  `InstallMethod::upgrade_steps` is the argv list it execs (no shell), `upgrade_command` is the
  same list spelled for a person and appears only in the failure sentence, and every line the tool
  prints goes on the bus as `neosh.update.progress` (`UpdateProgress`, `\r` counted as a line end
  because brew and cargo redraw in place) so the progress row says `brew upgrade neosh  ==>
  Downloading …` rather than `Updating neosh…` for a minute. Then it is **checked**: an exit
  status answers "did the command run", not "is there a new neosh", and `brew upgrade` on a tap
  that has not caught up exits 0 having printed `already installed` — so the same `stat` that
  notices an upgrade run in another terminal is what decides between `Applied` and `Failed`, and
  `Delegated` is gone. The tool is found on `PATH` and then in the prefix that owns the keg,
  because a workspace started from a launcher has neither `brew` on its `PATH` nor any less of a
  Homebrew install. Stdin is closed, every askpass unset, `HOMEBREW_NO_AUTO_UPDATE` set because
  the refresh is the step before, one apply at a time workspace-wide, and a step is killed at
  forty-five minutes because `cargo install` is a compile and a prompt on a closed pipe is forever.
- **The verb on a panel row is rebuilt from the state, never fixed.** A `pull` button that is a
  no-op nine times out of ten is a button people learn not to press, and the tenth time is the one
  that mattered. So the sidebar's git row says `pull 3 commits` when there are three, `check for
  changes` when nothing is waiting — the honest verb for a count that is only as fresh as the last
  fetch — and, when the branch has gone both ways, it *names the choice*: `rebase or merge`, in
  those words, as a picker, because the two do different things to your own commits, one of them
  rewrites them, and neither is guessable from a row or from a yes-or-no over a question that was
  never stated. A fast-forward asks nothing, for the reason everything reversible asks nothing.
- **A key on a contributed row belongs to the section, not to "contributed".** `on: "custom"` meant
  *every* contributed row in the panel, which was indistinguishable from "mine" while there was one
  block in the column and wrong the moment there were two: the plan strip and the git block both
  want `⇥` on their own rows and both are right, and one `keymap.set` per action made that one
  winner and one plugin whose key silently did nothing. `custom:<section id>` is the narrow form,
  the panel binds each key **once** and dispatches to whichever claimant matches the row under the
  cursor — narrow before `any`, the same ranking the hint strip prints in — and the strip prints one
  line per key, because advertising two meanings for one press is being wrong about at least one.
- **A repository you do not have yet is a way a project arrives, and it always asks where.** Half of
  "add a project" was never a directory on this disk: it was a URL, and the answer was to leave
  neosh, clone in a shell, and come back to type the path. So an address in the `^O` field —
  `https://`, `git@host:owner/repo`, `file://`, or bare `owner/repo`, which is only safe to guess
  at because that field stopped having a menu to be a filter for — becomes an offer to fetch it.
  **Where it lands is asked every time**, because a clone writes a whole history onto a disk and
  people answer that differently — one root for work, another for what they are only reading — and
  choosing silently means the first thing anybody does with the feature is go and find where it put
  something. Asking costs nothing when the default is the first row and `↵` takes it. `clone.root`
  is that default and is **not** `worktree.root`: the latter spends `<root>/<repo>/` on one
  repository's branches, so a clone written to `<root>/<repo>` would be asked to create the very
  directory those branches live in. Anywhere else you pick is *remembered* (`clone.locations`, most
  recent handful) — which is the whole of adding a location: no list to curate, clone somewhere once
  and it is a row from then on. A destination that already exists is shown and refused rather than
  hidden, because the directory being there is very often the answer to "why is this not in my
  sidebar". It is the **git plugin's** verb and not the panel's, like every other write to a
  repository — the sidebar decides only that what you typed is an address — and a clone **lands you
  in it**, so `^K git.clone` and `^O` do the same thing rather than one of them fetching a
  repository and leaving you where you were. What it draws while it runs is a keyed progress row and
  never a modal: a large repository is minutes, and the phases are git's own words with a meter only
  where git gave a total, because a bar creeping along on an invented denominator is the one thing a
  progress display must not do.
- **A branch is a local fact and a pull request is not.** `feature/the-thing` with three commits on
  it reads identically whether it went green an hour ago, is failing, is a draft nobody has been
  asked to look at, or merged last week and left a worktree behind — four different things to do
  next, drawn as one row. `ApiCall::ForgePulls` is the other half, answered by `gh` for the reason
  every vendor CLI here is preferred to a binary neosh would fetch and keep updated: it already
  handles tokens, enterprise hosts, SSO and rate limits, and its answer is the one the user gets in
  their own terminal. **One call per repository, keyed by head branch** — six worktrees of one
  checkout are one request and six lookups, which is the whole reason it is affordable on a timer —
  and it is a **read**, ungated, because unlike `GitFetch` it writes nothing anywhere and gating it
  would be charging a permission for a fact. It **rejects rather than answering empty** when it
  cannot know: an empty list is the claim *this repository has no pull requests*, and no `gh`, not
  signed in and no forge remote would each be making it falsely. Those three are *states* rather
  than events — permanent until somebody acts — so they are said once and filed, never on the
  timer. And **a check that has not finished is not a check that passed**: the rollup is every run
  in every state at once, so failing outranks pending outranks passing, an empty rollup is `None`
  rather than green, and `SKIPPED` on a docs-only change must not paint the row red.
- **A mark that is always there is a mark you stop seeing.** The badge is `#86` in the state's
  colour, plus a check mark **only when it is news** — `✗2` failing, one turning glyph while they
  run, and nothing at all when they passed. The transcript's rule about tool calls, one panel along:
  a green tick beside every merged pull request from the last six months is a column nobody reads,
  and the one time something is red it has to be what the eye lands on. Merged and closed say
  nothing about checks at all, because a merged pull request's checks are *why* it merged and
  sixteen columns spent on that clipped the state it was qualifying down to `#86 mer…`.
- **What a conversation owned goes with it, and the question says so.** A delete frees the disk it
  was using or it has freed the wrong thing: the pictures pasted into it are files under
  `<state>/images` minted for one paste each, so `sessions::forget` removes the ones its messages
  name — and only those, and only inside the store, because a path in a message is a claim. A
  worktree is made *for* the conversations in it, so the last of them going takes the checkout too:
  `orphanedWorktrees` finds the trees a deletion empties (counted over every conversation the
  workspace knows of, archived and on-disk-only included, because a tree with an archived
  conversation still in it is a tree somebody may come back to), `worktreeLines` puts them in the
  dialog — the directory, that the branch stays, and how many uncommitted changes go with it — and
  `discardWorktrees` removes them afterwards, forced, from the repository rather than the tree.
  Asked of `git worktree list` rather than of a var, so the repository itself is never one. The
  sidebar's `X` and `x`-then-empty, the archive's `X`, `^X` and sweep all go through it; the git
  plugin's `d` was already this from the other direction. `project.forget` is the row's half —
  drop a project's row without deleting anything, for a directory that has already gone — so a
  removed tree does not sit in the column saying `nothing here yet`.
- **A project outlives the conversations in it — a repository does; a worktree does not.** The panel's list is written down (`sidebar.projects`,
  a workspace var) rather than worked out from where the conversations happen to be — derived, it
  deleted the directory you had worked in all month the moment you cleared out the last thread in
  it, and left nothing to start the next one from. A project arrives by being added or by a
  conversation being started in it, keeps the name the host gave it (`sidebar.name`, so emptying a
  worktree does not rename it to `wt-fe3c0d93`), and leaves by `X` on its heading and nothing else.
  Empty, that asks nothing — `o` puts it back; with conversations still in it, it is a delete of
  every one of them and asks like one. A *worktree* row is the exception, by the rule above it: the
  checkout exists for the conversations in it, so the last of them going takes the directory, and
  `X` on the row itself removes the checkout — asked, always, because a directory is not a row.
- **"Where?" is one field, and the field is the path field.** `^N` and `^O` ask the same question and
  ask it the same way: type nothing and it is a menu, type `/`, `~` or `./` and it completes
  directories, type `linux-box:` and it completes directories **on that computer**, and paste a
  repository address and it offers to clone it. `<Tab>` walks
  into the highlighted one and `↵` takes it — `pathPicker`'s bargain, which is why a completion row's
  label has to be the whole path and why a remote one is prefixed rather than bare. The scp spelling
  is not decoration: `host:path` means there what it means here, it is what makes one field able to
  point at two machines, and it is learned by reading a machine's row rather than by being told. What
  this replaced was a path sitting at the bottom of a list of every worktree the program had ever
  heard of — a verb whose distance from the cursor grew with how long you had used it — so
  `Another directory…` is above that list now, and under `^O` **there is no list at all**: moving
  the row up left the trees beneath it, where each was wrong one of two ways. One already on the
  panel is an offer to add what is added — the repository you are standing in was the second row —
  and one that is not on the panel is a scratch branch you removed with `X`, handed back. Its only
  filter was a live conversation's `cwd`, which is neither, so what survived was finished work,
  each row its own full path with the basename and branch repeated after it and truncated
  mid-branch. No filter fixes that, which is the point: `^O` means *somewhere I do not work yet*,
  and a list of the repository you are in cannot answer it. What `^N` offers is the panel's own
  list, which is the same rule and not an exception to it. Choosing a
  machine reopens the picker seeded with `<machine>:` rather than descending in place, because a
  second level whose rows lie about what is in the field is a `<Tab>` that jumps somewhere nobody
  asked for. **Every paired machine is a row, including the ones that cannot be used**, greyed with
  the reason on them: skipping them is right about what can be *done* and wrong about what should be
  *said*, and somebody who has just paired a computer and finds an empty list reads it as the feature
  not existing. A machine with no projects yet falls through to its home directory rather than
  showing none. And starting one over there **opens it** — `swarm.command` answers with the
  conversation `NewSession` made, which until it did meant the same key did visibly less for the same
  intention: you had to go and find, in `^J`, the thing you had just made.
- **Nothing there and not allowed to look are different answers.** They are the same empty list, and
  only one of them is fixed by typing a different path — so a path field drawing `no directory
  matches` over a refusal sends somebody off to check a path that was right all along. macOS is
  where this bites: TCC grants folder access to the **terminal**, so `~/Documents` with a year of
  work in it completes to nothing until a box is ticked in System Settings, and `chmod` does not
  help, the file's owner does not help, and `sudo` — which is what everybody reaches for — does not
  help either. The kernel hands over the one bit that separates the two and Rust throws it away:
  `EACCES` is the mode bits, `EPERM` is the privacy layer, and `ErrorKind::PermissionDenied` is
  both, so `crate::access` reads the raw errno and nothing else can. What it answers with names the
  folder, the application the grant actually belongs to and the pane it is granted in, because
  "permission denied" is true and unactionable; the application is a guess from `TERM_PROGRAM` and
  becomes "your terminal" when it cannot be one, since a workspace outlives the terminal that
  started it and a confidently wrong name is what somebody then hunts for in a list of thirty. It
  travels as `denied` on `ApiOk::Paths` and is drawn as a **notice** rather than a row — a row is a
  thing you can put the cursor on, and this one would do nothing — and over ASCP it is an ordinary
  `Refusal::Failed`, so a peer's protected folder reports itself without a wire change. **And what
  cannot be detected is not guessed at**: a denied `display notification` on macOS exits `0`, so
  nothing here claims to know whether a notification was ever seen.
- **What you have archived is not in the sidebar at all, and it is something you can empty.** The
  panel is the list you work in; a section of things you are finished with is the only part of that
  column that is never the answer, and it grows forever. The first cut left one dim row with a count
  behind — a door rather than a drawer — and that row is now **off by default** too, for the same
  argument one step further: a permanent line about what you are *not* doing is the line that
  column can least spare, and the way in was never the row. `^F` from anywhere, `a` from the panel
  — advertised in its key strip — or `^K`; `archive.sidebar = true` puts the count back for anybody
  who wants it. It opens as a **popup panel of its own** — its own plugin, its own buffer kind, every key
  an ordinary binding, `archive.action` for somebody else's verb, and the row you are on unfolds.
  A picker was the right shape for the conversation you archived this morning and regret, and forty
  minutes of dialogs for a workspace you have used since spring: so rows can be *ticked*
  (`<Space>`), every verb means what is ticked or the row under the cursor, and `^X` empties
  **what the panel is currently listing** — which is what the filter is for, since narrowing to one
  project and pressing it is that project and nothing else. It asks, in numbers, like the delete
  it is. What is on disk is in scope: `RESTORE_LIMIT` means a workspace loads the most recent two
  hundred conversations and the rest are files nothing could show, open or delete, so the panel
  reads `session.stored` as well as `session.list` and the host brings one in from its file the
  moment any verb names it — otherwise "empty the archive" reports success over a directory it
  never touched. **Nothing here deletes on a timer**: `archive.auto_days` *archives* what has gone
  idle, because archiving is reversible and free, and `archive.retention_days` only ever counts —
  `archive.sweep` is the same number with a person behind it.
- **A notification is for something you did not ask for and cannot see.** Both halves, and a
  message that fails either is not one. `MessageLevel` says how *bad* a thing is and never whether
  you need to know about it, which is how one channel ended up carrying a hundred and seventy call
  sites sorted by nothing: `favourited ~/proj` beside a row that had just grown a pin, `pulling…`
  stacked *above* the `up to date` that superseded it, and — the part that matters — no
  notification at all for the two events that stop a turn dead. So `NoticeKind` is the other axis:
  a **reply** is feedback for the key you pressed and does not stack, because two keys pressed
  quickly are two keys; **progress** is keyed and replaced in place, because it is a state and not
  a message; an **alert** is news, and the only kind that may leave the terminal. Where it goes is
  decided by where *you* are — nothing leaves the terminal about a conversation you are looking at,
  and that one line is what kills the noise. **And the raise belongs to the terminal, not the
  workspace**: it is an OSC the view writes on the stream the UI is already drawn on, for exactly
  the reason `Clipboard` is OSC 52 rather than a clipboard library — a coding agent runs on the big
  machine and you are sitting at a laptop, so anything the workspace raises itself is raised on the
  wrong computer. The desktop fallback is *only* for nothing being attached, when there is no
  stream and no wrong machine to be on. Away is focus (mode 1004) and, on a terminal that cannot
  say, idleness — never "assume focused", which is the one wrong answer that means a whole class of
  terminal never notifies. A turn shorter than `notify.min_turn` finished while you were still
  looking at the key that started it. And `unread` stays the *record*: this points at it once and
  gets out of the way.
- **And it goes in the corner you are not looking at, which is the top one.** A notice *overlays* —
  reflowing would move the field you are typing into, which is the one thing it must never do — so
  the only question it has left is what it covers, and the bottom-right corner answered it with the
  composer and the last rows of the transcript. Those are the line being typed and the newest thing
  the agent said: the two places somebody is certainly looking, and a notification is by definition
  about something they are not. The top of a transcript is scrolled-past text during a turn and
  empty space the rest of it. It stops short of whatever is docked across the top of the screen,
  which is the tab strip — always drawn, the only place panes and tabs announce themselves, and
  therefore never coverable — and the block grows *downwards* from there with the newest last, so a
  row already on screen does not move when another arrives.
- **A turn that finished while you were elsewhere is news until you go and look.** The panel says
  what is *happening* and stops the moment it stops, so an answer that arrived while you were in
  another conversation looks exactly like an answer you read yesterday. `SessionInfo::unread` is set
  when a turn ends off screen and cleared by *arriving* — no dismissal key, because a mark you clear
  by hand is a second chore attached to the first. It lives on the conversation and is persisted, so
  every list agrees and a restart does not lose it; it wears `Status.Unread`, the amber the palette
  already uses for *act now*, and it never moves — motion means "something is happening you cannot
  see", and this is the opposite. A folded project carries a dot for what it is hiding.
- **A conversation that is asking you something is not a conversation that is working.** Its turn is
  still in flight — blocked on the hook — so `active_turn` is set and every list draws the spinner,
  which is the one row in the panel that most needs finding drawn as the nineteen that do not. Which
  ones are waiting travels as `question.asking`, a **workspace var** written by whatever serves
  `ask_user` and read by whatever draws conversations, because it is a fact about them rather than
  about the panel that happened to learn it. It **outranks working** wherever the two meet, it wears
  `Status.Pending` and therefore *moves* — a block ends when you answer, which is exactly what
  `Status.Unread` must not do — and the queue behind it is in memory while the var is on disk, so it
  is announced once at startup, empty, and that is what clears the one left behind by a workspace
  that stopped mid-question.
- **A question is the last thing in the transcript until something answers it.** A turn's closing
  rows — its plan, what it left running, what it changed — are about the answer they close, so they
  splice in *above* a question steered in after that answer rather than at the end of the buffer,
  where they sat under a message you had just typed. And a question the turn ended without
  answering says so in a row of its own, out of the stop reason: a bare question reads exactly like
  one still being thought about, and a toast that has faded is not an answer to "why is nothing
  happening". A plugin veto is a `Refusal` carrying its reason and never `Cancelled`, which is what
  `<Esc>` means. **An entry in the running-turns map lives until `TurnEnded` arrives** — `<Esc>`
  flags it rather than removing it, because removing it early let a message typed in the gap start
  a *second* turn in one conversation, which the first turn's ending then dismantled. Steering never
  takes a message it cannot deliver: the tool-gap take is guarded on cancellation, and what stays in
  the queue becomes the next turn. Leaving the reader goes back to following the newest, and so does
  sending — whether it starts a turn or joins one.
- **An orchestrator is a plugin, and it drives conversations by name.** `agent.command` carries the
  same `AgentCommand` vocabulary `swarm.command` does, at a conversation named by id and defaulting
  to the one on screen; `swarm.command` aimed at this node runs it here rather than dialling itself.
  Watching was always per-conversation — `Token`, `ToolFinished`, `TurnEnded` all name one — so
  until this existed the local API could see every conversation in the workspace and drive exactly
  one of them, and fanning work out meant `SessionSwitch` before every message, which moves the
  transcript out from under whoever is reading it. **Which model answers is a hook**: `turn.route`
  fires after the words are settled and before the driver is resolved, may re-point the turn or veto
  it with a reason, and writes its choice back to the conversation so the footer and `^P` agree with
  the turn. **The declared permissions are enforced** — a tool, a provider driver, a *blocking* hook
  and a raw-cell surface each need the word in `plugin.toml`, an observer needs nothing, and drawing
  and reading stay free. **A plugin arrives as a git clone**: `neosh plugin add <url>`, validated
  before it is moved into place, named by its manifest, and never able to delete what you put in
  your own config directory by hand.
- **A card is a row until you ask for more.** A call that only *looked* at something folds to its
  header with the size of what came back on the end of it; a command keeps its output — a stack of
  them keeps the last one's — an edit keeps its diff, and a failure always shows.
- **A run of calls of one kind is one row**, naming as many of them as fit and counting the rest —
  a turn that read six files reads as one, and so does a stretch of it spent on `git add`, `git
  commit`, `git push`. Reads with reads and commands with commands, never one of each, and never an
  edit. A run is calls with *nothing drawn between them*, asked the same way by the live path and
  the replay, or switching away and back re-folds the conversation. A stack of commands keeps the
  **last one's output** — what a command printed is the answer, and the ones before it are how you
  got there — names each command by what it *is* rather than how it was spelled (`cargo test`, not
  `cargo test -p neosh-core -- --test-threads 2`), and gives every command back in full, with what
  it said, on `⇥`. A run **never continues past a failure**, so a failed command is the last call
  on its card and its output is the one showing. While a run is still going the row is fitted from
  the *end*: then it is a report of what is happening rather than an account of what happened, and
  the call being waited on is the one name that must never be the name that did not fit. Only a
  mark that is news gets drawn: `▸` while a call is out, `✗` if it failed, and nothing at all when
  it worked.
- **A card says what happened, not which tool did it.** `Ran cargo test`, not `Bash cargo test` —
  read off the arguments, like the colour. A call nothing here classifies keeps the name its author
  gave it, so a plugin's tool is never renamed. **A command's output folds from the middle**: the
  head is how it started and the tail is what it decided, and it is the second one you were waiting
  for. **A card is attached to what it did**: no blank row above it, and its body under a corner
  (`└`) at a four-column indent rather than a rule down the whole left side. Air between every two
  actions and a wall beside every diff were both structure said twice — the header is at column
  zero, the body is indented, and that is already a list.
- **A turn is something you can watch, not only something you read back.** A command is named by
  what it *does* and never by how it got there: a leading `cd /home/you/projects/thing &&` is the
  one part of that row you already knew — the conversation has a directory — and clipped to a
  header it is the only part that fits, so it comes off before the command is named and a stack of
  six folds to what was run rather than to `cd, cd, cd`. Reading is a place, and a turn writes
  below it: parked on the **last row** you are carried along with what arrives, anywhere else you
  stay exactly where you are, and neither happens while a selection or a search is holding two
  positions. And **the card the cursor is in is open** — the list rule one surface along, bounded
  by `chat.preview_lines` because nine hundred rows appearing under `j` is what the fold exists to
  prevent, and never fewer rows than the folded card showed. `⇥` stops meaning "open this" and
  starts meaning "keep it", `c`/`C` step call to call because `[`/`]` is a whole turn and `{`/`}`
  is one block for a run of nine cards, and a preview never fires while the tail is carrying you:
  following is watching, and a redraw settles the answer streaming above it.
- **A terminal cannot paste a picture, so pasting one is a key.** Bracketed paste is a text
  protocol: a screenshot arrives as nothing, and a dragged file arrives as its path. `^V` asks the
  system clipboard through whichever of `wl-paste`/`xclip`/`pngpaste`/`osascript`/`powershell` is
  there; a pasted line that is unambiguously a path to an image becomes that image. A clipboard is
  **asked what it holds, not told**: the best picture type *on offer* is the one requested, because
  asking for `image/png` and nothing else reads a JPEG, a WebP and a BMP as an empty clipboard. And
  a picture copied off a page is often not on the clipboard at all — what a browser leaves is an
  address (`text/uri-list`, an `<img src>` in `text/html`, a `data:` URI, a plain URL), so
  `from_clipboard` answers with bytes *or* a `Remote` to go and get. That fetch is spawned rather
  than awaited on the loop, carries the conversation it was asked in, and is the one attachment
  that says something on the way. A photograph too busy to fit as a PNG is sent as a JPEG rather
  than refused over an encoding. An attachment is a **chip above the field**, never a
  token in it — the message you send is the message you typed. `ContentBlock::Image` carries a
  *path*, not bytes: a transcript that is mostly base64 is one nothing can read back, so the bytes
  live once in the state directory and a driver reads them when it builds its request. The media
  type is read off the bytes, because a `.png` that is really a JPEG fails the turn.
- **An agent that speaks when nobody asked is still speaking to you.** An earlier fix stopped a turn
  `claude` starts for itself — a backgrounded command finishing, enqueued as a message to itself and
  answered — from being read as the answer to your next question, by draining the pipe before the
  prompt goes in and dropping what it found. This is the half it deferred: the same turn, *heard
  while it happens*. The reader outlives the turn the way the process does — stdout is read for the
  life of the child, not the length of an answer — so nothing is lost and a pipe nobody reads cannot
  fill and stop the CLI mid-sentence. It reports what nothing is reading (`Unasked`), and the host
  opens a turn to hold it: one that **says nothing** (`listen_only`, no user message pushed, and no
  drain, because dropping what the turn exists to show is the one mistake it cannot make), because
  a turn opened to hold an answer has no question above it. **A turn still ends on its own
  `result`** — `init` opens one and `result` closes one for their turns as much as ours, so a prompt
  queued behind one has two endings to wait for, and ending on the first is how a message that had
  not been read yet came back as `{"type":"thinking","text":""}`. **And the report has to outlive
  the turn it interrupted, because it is raised by that turn ending.** The driver notices an
  unasked turn precisely because *nothing is reading the pipe* — which is the moment the last turn
  let go of it, strictly before that turn's `TurnEnded` reaches the host loop. So `hear_out` was
  always called with the finished turn still in `turns`, declined on "a turn is already running",
  and nothing ever mentioned it again: a driver reports a *new* `init`, and that one had been and
  gone. What the agent said then sat in the pipe until the next message was sent, where
  `drain_between_turns` drops it by design. From the keyboard that is the whole bug in one
  sentence: a turn ends, you type, something is instantly running, and what the agent said in
  between is gone under your message — or, when its turn was still going, streamed in *below* the
  message as though it were the answer to it. `Host::unheard` holds the report and the ending that
  was in the way spends it, **before** the leftover steering starts its turn: what the agent had
  already begun saying came first, so it is heard first, and the message you typed meanwhile is
  steered into that listening turn and asked at the end of it. The listening turn also says what it
  is (`Reporting back`, not one of `VERBS`) and stops saying it the moment steering hands it a
  question — a spinner claiming to be working on something you never asked is what made your own
  message look like the thing that started it.
- **A vendor CLI outlives the turn, so an abandoned turn has to be *drained*.** `Live::lines` is
  per conversation. A turn that is walked away from — `<Esc>`, a switch — leaves the CLI mid-answer
  with the rest of it in the pipe, and the next turn reads it as its own: someone else's reply, and
  then nothing. Stop forwarding, interrupt, read to the `result` that ends it, and leave the process
  at a turn boundary.
- **Everything queued into one gap is one message.** A delegating driver is handed the *newest* user
  message and nothing else, so N queued messages meant N-1 questions drawn as asked and never put to
  anybody — which reads exactly like being ignored. `take_steering_into` joins them, in the order
  they were typed.
- **A repaint ticker is one ticker, and a flag alone cannot say so.** Motion on screen arms a 20 fps
  ticker and the first still frame takes it down, which is right — but `follow_motion(false)` then
  `follow_motion(true)` inside one 50 ms sleep leaves the sleeping ticker waking to a flag that is
  `true` again, so it carries on *and* a second one has been spawned beside it. Motion stopping and
  restarting is not an edge case in a workspace watching an agent work: it is what every tool call
  does, once when its spinner appears and once when it lands, so a turn with thirty of them is
  thirty chances to add another 20 fps of repaints that never go away. The ticker carries a
  generation, claimed on every call whichever way it went, and stops the moment it is not the live
  one.
- **A row is written once, so what it says has to stay true — which is what a clock is and a
  relative time is not.** A conversation happens *in* time and the transcript said nothing about
  it: an answer that took four minutes and one that took four seconds read identically, a question
  asked yesterday read exactly like the one asked a minute ago, and a card rebuilt from a stored
  conversation carried no duration at all, because the messages recorded what happened and never
  when. `Message::at` is the missing fact — stamped by whoever pushes the message, never by
  `Session`, which has no clock in it precisely so that it and the store stay deterministic, and
  `None` is *unknown* rather than the epoch. What it buys is one margin per turn, on the blank row
  above its question: when it was asked, and how long it took. Both fixed, and that is the whole
  design constraint — the transcript is written once and nothing comes back to it, so *how long
  ago* is a lie waiting to happen and a clock time never is. The relative reading is therefore
  offered on **exactly one row**, the newest turn's, which is the one a person walking back to
  their terminal is actually asking about; it is kept true by a tick that runs at half a minute
  rather than at a second, because nobody is watching it, and it **settles** — rewritten without
  the *ago* — the moment another question is drawn below it. It is **virtual text** flush right,
  not characters in the buffer: a transcript is an artefact you take pieces out of, so `y`, `ym`
  and `ya` have to copy what was said and not what neosh wrote in the margin about it, and only
  the frontend knows how wide a window is. And the date leads the margin only when the day changed
  *since the turn above* — never when it changed since today, which is a fact about when the row
  was drawn. One vocabulary for a duration everywhere the workspace prints one — `41s`, `4m 12s`,
  `2h 30m` — because the working line said `123m 4s` about the same turn the sidebar three columns
  to the left called `2h 03m`.
- **A flash is one thing happening; a burst is a redraw.** `Animation::Flash` is the only motion
  here that fires once, so it needs a moment to count from — and a highlight group, shared by every
  row using it, cannot hold one. The *mark* does: an extmark is created once, so the frontend keys
  the one-shot from the first time it saw that mark, and scrolling a row away and back cannot fire
  it again. More than a few arrivals at once are a bulk redraw — republishing on reattach mints
  every mark in a conversation — and none of them flash, because a transcript that blinks when you
  switch to it is the worst version of this. A spinner (`Animation::Frames`) is the other half:
  frames travel as a *name* because only the frontend knows what the terminal can draw, and it
  clips or pads every frame to the width of the run underneath, so a spinner can never move the
  column after it. The buffer keeps the glyph that was written — a frame is what a cell looks like,
  not what the line says.
- **Compaction moves the meter, because compaction is what moved the context.** A driver that has
  once answered "how full is it" turns off the estimate derived from usage, so the next answer only
  arrives with the next request — and a `Compacted` whose `after` nobody applied left the footer
  reporting the pre-compaction number for the rest of the conversation, under a card that said
  `180k → 12k`.
- **What the plan has left cannot be counted, only reported and then kept.** A subscription's
  allowance is opaque and is not a function of anything on this machine — a turn run in `claude`
  directly spent it too. `claude` says so on a `rate_limit_event` line and `codex` on
  `account/rateLimits/updated`; both are `Activity::Quota`, both are *kept* by the host, and a
  mid-turn line **patches** while a poll **replaces** — the first carries one window and says
  nothing about the others, the second is the only thing that can express a limit that has gone. A
  window id is a *string* and severity is the **vendor's** grade: the `limits` array grew three
  kinds between two releases, and 94% weekly is `critical` where 55% session is `normal`. Polling at
  rest reads the vendor CLI's own login — `SecretString`, request-time, never on disk, structurally
  absent from `UiEvent` — and is `usage.poll`. **Quota and token history never share an axis**: a
  percentage of an opaque allowance does not convert into a token count. History comes from the
  vendors' own transcripts, never from ours, and a bounded scan that hits its cap says so.
- **Redrawing a row means clearing its marks first.** A mark clamps rather than dies when the line
  under it is replaced, and at equal priority the *narrower* one wins — so a row rewritten every
  tick accumulates marks and ends up painted by the shortest one from several seconds ago.
- **A selection is two positions, so moving either end redraws it.** `WinMotion` and `WinSetCursor`
  differ on purpose — the first throws the selection away unless told to extend, the second leaves
  the anchor alone, which is what makes a *jump* take the text it jumped over — but both have to
  repaint. Only `WinMotion` did, so `v` and then anything but `hjkl` (`^U`, `{`, `[`, a search hit)
  selected six lines, showed none of them, and copied all six: a selection you cannot check is worse
  than none. `V` is not a second kind of selection either — it is the same pair of positions with an
  invariant re-established after every motion, because extending *upwards* from a line-start anchor
  otherwise drops the one line you definitely meant.
- **Arriving in a conversation reseats the view, not just the buffer.** The transcript's scroll
  offset, cursor and selection are *kept* — that is what lets a client attaching later be told where
  it is — so `enter_session` believing "back at the end" is not the same as the window being there.
  Read a long answer, go up, come out, switch project, and the next conversation was drawn from a
  row of a transcript that no longer existed. Reading is a place *in* a conversation, so the switch
  takes you out of it too. The rule, one more time: a value that is only forwarded is a value
  that comes back wrong.
- **Following the newest is not a row, and row zero is.** `top_line` is an `Option`: `None` is
  unscrolled — the frontend's own answer, the last screenful of a transcript and the first of
  everything else — and `Some(0)` is the top. Encoded as the same `0`, the first row of a
  conversation was the one place in it nothing could ask for: `^U` and `PageUp` up to the top asked
  for row zero, which was read as "follow the tail", and the window drew the *end* instead — cursor
  off screen, and nothing on the way there to say what had happened. A frontend decides where an
  unscrolled window sits, which is why the state has to be sayable rather than spelled with a row
  number that means something else.

## Verification

Per crate, not workspace-wide — the full run is heavy and `neosh`'s integration tests take about a
minute on their own:

```sh
cargo test -p neosh-core -- --test-threads 2
cargo test -p neosh --test builtin_plugins -- --test-threads 2
cargo test -p neosh --test workspace -- --test-threads 2
cargo test -p neosh --test plugin_manager -- --test-threads 2
# ts-rs reads the export directory from the environment, and an exported `TS_RS_EXPORT_DIR`
# beats `.cargo/config.toml` — so in a worktree this has to say which checkout it means, or
# the generated files land in another one and the diff below passes having read nothing.
TS_RS_EXPORT_DIR="$PWD/plugins/api/src/generated" cargo test -p neosh-proto export_bindings \
  && git diff --exit-code plugins/api/src/generated
cd plugins/api && npx tsc --noEmit
```

`tests/workspace.rs` runs a real `neosh --serve` and talks to it over a real socket, because what it
asserts is that one process survives another going away. Each sandbox gets its own socket via its
own `--config-dir` — which is also what stops the suite attaching to *your* workspace and stopping
it. `tests/plugin_manager.rs` does the same for installing: a real git repository, a real clone, and
a real workspace started afterwards to see whether the plugin actually ran — a `--config-dir` **and**
a `NEOSH_DATA_DIR` of its own, because that is where an install lands.

`tests/builtin_plugins.rs` starts a whole neosh per test and waits on what appears on screen, so it
is timing-sensitive: under load a handful of unrelated tests fail on a `wait_for` timeout, and a
different handful each run. A failure there is worth re-running alone (`--test-threads 1 <name>`)
before believing it.

Releasing is `scripts/bump.sh` — see `docs/releasing.md`. The crates and the binary packages share
one version and move together; `@neosh/api` and each `@neosh/<plugin>` have their own and move
only when their directory changed, because nothing depends on them and `publish-npm.yml` skips a
version npm already has. `tag.yml` runs `scripts/bump.sh --verify`, which fails a release tag on a
package that changed and kept its number — the one way that policy can lose a change.

`./scripts/check.sh` is everything CI runs, in four stages — `crates`, `binary`, `screen`, `web` —
and CI runs each stage on a runner of its own, the two big ones sliced across several
(`check.sh screen 1/3`). It prefers `cargo nextest` when installed, one process per test with one
retry under the `ci` profile in `.config/nextest.toml`, and falls back to `cargo test`. The
`binary` stage runs the scaffold check against the binary the suites just built rather than
`cargo run`, which rebuilt the largest crate under the dev profile on every run. Nothing caches the
workspace crates between runs on purpose: cargo decides freshness by mtime, a checkout gives every
file the same one, and a restored crate that is *believed* stale-or-not is worse than one rebuilt.

**Terminal behaviour is checked by driving the real binary**, not by reading the code. `scripts/shot.py`
runs neosh under a pty and prints what the screen actually looks like:

```sh
/tmp/ptyenv/bin/python scripts/shot.py --cols 110 --rows 34 --arg=--no-daemon '<c-p>' '<wait:2000>'
```

`--no-daemon` keeps it to one process, so a screenshot is of a workspace that starts and ends with
the shot rather than one left over from the last one.

`--color` prints what colour every run of every row is instead of the plain screen — the only way to
check a diff band, a syntax colour or a dim attribute, none of which the plain dump can see. `--grep`
narrows it to the rows containing a string.

Keys are literal text or `<c-p>` / `<cr>` / `<esc>` / `<s-tab>` / `<wait:400>`; `--arg=` passes
through to the binary. `<paste:text>` arrives as a real **bracketed paste**, which is a different
thing from typing the same characters — an image path becoming an attachment, a `/` that dismisses
the menu, a pasted `<esc>` that must not interrupt: none of it is reachable by typing.

Several real bugs — a float silently losing two rows of content, a binding that had never once
fired — were invisible to the test suite and obvious on screen.

After editing anything under `plugins/`, `touch plugins/lib.rs` before building: the plugin tree is
embedded with `include_dir!` and cargo will not notice otherwise.

---

# The command line

`neosh` is the terminal; `neosh "a prompt"` is the terminal with the first question already asked,
in a new conversation in this directory. Quote it when the first word is also a subcommand name —
`neosh status of the build` runs `neosh status` and then complains about `of`, which is an error
rather than a surprise, but an error all the same.

Everything else the workspace can be asked for a person is under `neosh agent`, which takes no
screen and answers `--json` for every verb: `start`, `ls`, `send`, `read`, `watch`, `wait`,
`interrupt`, `rename`, `archive`, `rm`, `models`, `model`, `commands`, `run`, `call`. `wait` exits
`0`/`1`/`2`/`124` — finished, failed, interrupted, timed out — so `&&` means what it looks like.
`run` and `call` are the two escape hatches: a plugin's command by name, and any `ApiCall` at all.
Ids are uuids and every verb takes an unambiguous prefix of one, or `.` for what a terminal is
looking at. `website/src/pages/docs/scripting.md` is the long version.

`neosh skill install` writes `crates/neosh/skills/neosh/SKILL.md` into `.claude/skills/`,
`.agents/skills/`, `.cursor/skills/` and `.gemini/skills/`, for the user or for a project. The
skill lives under the crate rather than at the repository root for the reason `plugins/` is its own
crate — `include_dir!` embeds what is under the package root and nothing else, so a tree at the
root would work in a checkout and publish empty — and `.claude-plugin/plugin.json` points at it
from the root rather than the other way round.

---

# Keys

Everything here is an ordinary binding in the same table your `init.ts` writes to, bound to a
*command name* rather than a callback. Setting the same key replaces it. `^Z` lists the live
registry — this table is what ships.

Every key here is one **every terminal sends on every platform**: Ctrl-with-a-letter, `⇧⇥`, `⏎`,
`⌫`, `Esc`. Nothing needs `fn`, nothing needs Option-as-Alt, and nothing is reachable only by an
arrow — arrows and `PgUp`/`Home`/`End` are bound wherever they mean something, and are never the
only way to do anything.

## Chat

| Key | Does |
|---|---|
| `⏎` | Send. While a turn is running, **steer** it: the message is held and taken in at the next gap. |
| `⇧⏎` | Newline, so a pasted snippet stays one message |
| `^Y` | Take the last thing you queued back into the composer, to change it or drop it. Readline's yank: `^U`/`^W` kill, `^Y` brings it back |
| `^V` | Attach the image on the clipboard — the picture itself, or the one it only names: a page's `<img>`, a URL, a file. A key rather than a paste, because a terminal's paste can only ever hand over text. Once sent it is drawn in the transcript on a terminal that draws pictures (kitty, Ghostty, WezTerm, Konsole, tmux with passthrough), and so is any picture the agent reads; elsewhere it is its name |
| `⌫` | On an empty composer, take the last attached image back off |
| `^P` | Pick a model. Mid-turn too — the running agent is told, and thinks the rest with it |
| `^E` | Everything this model can be told, on one panel: effort, thinking, fast mode, context, and whatever a driver invented. `h`/`l` along a row, `j`/`k` between them, arrows too, and it applies as you move. Nothing else reaches the keyboard while it is open; `^E` again closes it |
| `⇧⇥` | Permission mode — full access to start with, then ask, allow-listed, deny. Belongs to this conversation, saved with it, and takes effect on the turn that is running |
| `^T` | Projects and conversations. Switching is never refused — turns keep running where they are |
| `^J` | The computers in this workspace. Add one by its address, allow one that is asking, rename one (`^E`), or open what it is running. A machine this one has reached that has not allowed it back says so, and says which key to press over there |
| `^F` | What you have archived — see below. Filter it, put some back, or finally empty it |
| `^N` | New conversation. With another computer paired it asks **which** first — this one, or any of them, each marked with whether it can be reached; with none paired that question is not asked at all. Then, in a repository, where: here, a worktree you need not name, one kept inside the project, one you do name, an existing one, elsewhere. A worktree you did not name is named by your first message — `fix/composer-paste-truncation`, not `wily-nimbus-7hq2` |
| `^O` | Add a project. The filter line **is** the path field: `/`, `~` and `./` complete directories from the first keystroke, `⇥` walks into the highlighted one, `↵` takes what you typed. `linux-box:` completes on that computer instead. Paste a repository address — `https://…`, `git@…`, `file://…`, or just `owner/repo` — and it offers to **clone** it: it asks where, remembers the folders you pick, draws git's own progress while it fetches, and leaves you in the new project |
| `^B` | Toggle the sidebar |
| `^K` | Command palette |
| `/` | Completes a command by name — neosh's, and whatever the agent says it accepts. Keep typing; the composer is still the field, and `↵` sends what you typed when nothing matches |
| `^L` | What the plan has left, and where the week went. Live gauges per limit, and the last 30 days from the agents' own transcripts. The sidebar keeps one row of it; `⇥` on that row asks for more |
| `^G` | Git status |
| `^D` | Show what changed |
| `^S` | Read the transcript — see below |
| `⌥Y` | Copy this conversation's directory — in a worktree, the worktree's path. The one Alt chord that ships, because it is never the only way: `yp` in the reader, `y` in the project panel, `^K` by name |
| `^A` `^X` | Select everything, cut the selection |
| `^C` | Copy the selection, or clear the draft, or (twice) quit |
| `PgUp` `PgDn` `^End` | Scroll, and back to the newest message |
| `^R` | Reload configuration |
| `^Q` | Close this terminal. Whatever is running keeps running, and so does every other terminal open on this workspace — `neosh` puts you back |
| `^Z` | Every binding, live |
| `Esc` | Interrupt the turn in this conversation. The agent is asked to stop, so the conversation survives it |

Dragging an image onto the terminal pastes its path, and a pasted path to an image is attached
rather than typed out. What is attached sits above the field until the message goes.

Composer editing is a text field: `←`/`→` by character and `^←`/`^→` by word, `Home`/`End` and
`^Home`/`^End` for the ends, shift with any of them to select, `^⌫` and `^U` to delete a word or
back to the start of the line — `^W` used to be the word-delete and is now the window prefix, which
is `ui.keys.delete_word` and `ui.keys.window_prefix` if you want them the other way round. The capability ladder — `model.upgrade`, `model.downgrade` — has no
default key: it had `⌥↑`/`⌥↓`, which is not a key every terminal sends, and `^K` runs both by
name. Copying this conversation's directory — `session.copy.path`, which in a worktree is the
worktree's path — keeps `⌥Y` on the terms arrows are bound on: a key that means something where it
arrives and is never the only way — `^K` or `/copy` here, `y` on any row of the project panel, and
`yp` in the reader.

## Windows, panes and tabs — `<C-w>`

The one prefix in the workspace, and it earns it: twenty-four verbs, no Ctrl-letter left to give
them, and a list that draws itself when you hold the key. `ui.keys.window_prefix` moves it;
`^Z` lists every one of these as an ordinary binding, and `^K` runs any of them by name.

**Hold `<C-w>` and wait** — the panel below appears, read out of your keymap rather than out of this
table. `ui.keys.hint_delay = 0` shows it at once; a larger number keeps it out of the way.

| Key | Does |
|---|---|
| `v` `s` | Split: the new pane on the right, or below. Focus follows |
| `c` | Split to the right **on a new conversation** — the one-key version of splitting and then `^N` |
| `q` | Close this pane. The last one of a conversation is refused: `^T` goes elsewhere, `^Q` closes the terminal |
| `o` | Close every pane but this one |
| `h` `j` `k` `l` | Go to the pane that way. No wrap-around — the edge is where it stops |
| `w` | The next pane, or the next **tab** when this one has a single pane, so it always goes somewhere |
| `<` `>` | Narrower, wider — Vim's, and the same keys the project panel resizes with |
| `-` `+` | Shorter, taller. At an edge the other boundary moves rather than nothing happening |
| `H` `J` `K` `L` | **Move this pane** to that edge — Vim's, and the counterpart to `hjkl` moving *you* |
| `x` | Swap this pane with the next, keeping their places — how the one you care about gets the wide half |
| `=` | Give every pane an equal share |
| `t` | A tab. **Asks what goes in it**: a new conversation, a shell, or this conversation again |
| `T` | A tab with a **shell**, skipping the question. It is this conversation's shell, and it is on this conversation's bar |
| `S` | Split into a **shell** |
| `X` | Close this tab and everything in it. This conversation's last one is refused |
| `n` `p` | The next / previous tab on this bar, wrapping |
| `1`–`9` | That tab, counting from the left as the bar numbers them |
| `r` | Name this tab. `↵` on an empty name gives it back to being named by what is in it |
| `,` `.` | Move this tab along the bar |
| `?` | The list, now rather than after the delay |

A split shows what you were reading — Vim's answer for `:split`, and the only one that needs no
dialog — and each pane then has **its own transcript, composer, draft, scroll and search**. `^T` and
`^N` change what is in the pane you are in.

**The tabs belong to the conversation you are in.** A shell you open while reading one is that
conversation's shell, and switching with `^T` shows the tabs of where you have gone — the ones you
left are off the bar rather than closed, so the build still running in one is still running when you
come back to it, and it is on the bar again the moment you do. Which is also why `<C-w>X` refuses a
conversation's last tab where it used to refuse a terminal's: closing it would land you in some other
conversation's tabs, and a key about where you are looking should not decide what you are working on.

## A shell in a pane

`<C-w>t` **asks** what a new tab is for — a new conversation, a shell, or this conversation again —
and makes nothing until you have answered, because a tab that flickers into the bar and out again on
a key you cancelled is worse than the question taking a moment. `1`/`2`/`3` answer it directly, `jk`
move, `Esc` gives up. Every row says *what you get*: the first cut called the third one
`Conversation`, meaning the one you are in, which reads as *a* conversation and so was
indistinguishable from `New conversation` on the row below — and the likeliest intent is first,
because that is where the cursor starts and `⏎` is what people press. The three verbs behind it (`tab.new`, `tab.new.chat`, `tab.new.term`) stay separate
commands, so a script or a key can skip the question — which is what `<C-w>T` does.

The first row is the one that goes somewhere: a tab on a *new* conversation is that conversation's
first tab, so the bar it lands on is its own and the tabs you had open are the ones you came from.
The other two stay here. That is what the row says — `a fresh one here, with tabs of its own` — rather
than something you find out by watching the bar empty.


`<C-w>T` opens a tab with one, `<C-w>S` splits into one. A terminal pane is one window and no
composer, because a terminal *is* the field you type into; every key goes to the child — `^C`
interrupts *it*, `^D` ends *its* input — and the one key that does not is the window prefix, which
is how you get out of a full-screen program. Paste arrives bracketed, so an editor in there knows it
was pasted rather than typed.

It is `$SHELL -l`, in the conversation's directory, with `TERM=xterm-256color` and `NEOSH=1` set.
It draws through the raw-cell surface API a plugin already has — which turned out to carry exactly a
terminal cell — rather than through a buffer: a buffer is lines with marks on them, and rebuilding a
mark table sixty times a second for a program repainting arbitrary cells is not the same problem.
The caret is placed from the *child's* cursor, and the buffer under the surface is a scaffold of
spaces so there are rows for it to land on. A shell that exits says so and leaves its last frame up,
because what it printed before it went is usually why you were looking.

## Reading the transcript — `^S`

The answer is the artefact; this is how you get a piece of it out.

**A mode's keys are the mode's keys.** `^S` puts the editor in `Normal`, so `chat`'s bindings —
the ones that compose a message — stop firing and these get their keys back. Only four things are
bound in every mode, and they are the escape hatches: `^C`, `^Q`, `^R`, `^S`. Everything that acts
on the composer or scrolls without the cursor is `Chat` alone. A plugin that wants a key here binds
it in `normal`.

Two corollaries, both of which were bugs:

- **A chord is not the letter it is made of.** Chords are matched before the plain keys, and a
  chord this mode does not define does nothing at all. `^B` used to move a word left, `^A` used to
  put you back in the composer.
- **A key another mode claimed never arrives.** Reading's keys come through the *unbound* path,
  which is the last thing consulted — so `^D` was whatever the git plugin bound it to.

**The cursor is on a character, not between two.** Which is why the caret is a *block* here and a
bar in the composer, why `$` is the last character rather than the space after it, why `h` at column
zero stops rather than going to the row above, and why `v` then `y` copies the character you were
looking at instead of saying there is nothing there. A screen is counted in the buffer rows actually
on it, which after wrapping is not the window's height.

| Key | Does |
|---|---|
| `hjkl`, arrows | Move. A count first does it that many times: `5j` |
| `w` `b` `e` `ge` | By word. `W` `B` `E` by **WORD** — everything that is not a space |
| `0` `^` `$` `g_` | The start of the line, its first non-blank, its last character, its last non-blank |
| `f` `F` `t` `T` then `;` `,` | To a character on this line, or just before it. `;` again, `,` the other way |
| `%` | The bracket matching the one under the cursor, however many rows away |
| `gg` `G` | Ends of the transcript. With a count it is a row: `12G`, `12gg` |
| `^D` `^U` | Half a screen, or that many of them |
| `^F` `^B`, `PgUp`/`PgDn` | A screen |
| `^E` `^Y` | Scroll a line without moving the cursor — until it would go off the edge |
| `H` `M` `L` | The top, middle and bottom row **of the window** |
| `zz` `zt` `zb` | Put the cursor's line in the middle, at the top, at the bottom |
| `[` `]` | Previous / next **turn** |
| `c` `C` | Next / previous **tool call**. A count first: `3c`. The card you land on opens itself |
| `{` `}` | Previous / next **block** |
| `⇥` `za` | *Keep* the **tool card** under the cursor open — all of it, and it stays that way when you move off |
| `/` `?` then `n` `N` | Search, and step through the matches. `n` is *onwards*, so after `?` it goes up |
| `*` `#` | Search for the word under the cursor, forwards or back |
| `v` | Start a selection that motions extend. Again, or on a `V` selection, gives it up |
| `V` | The same by whole lines — both ways, so extending upwards keeps the line you started on |
| `o` | While selecting: the other end of what you have, so you can fix the end you got wrong |
| `gv` | The selection you last gave up, back where it was |
| `iw` `aw` `ip` `ap` `i"` `i(` `i[` `i{` … | While selecting: the word, paragraph, string or brackets the cursor is in. `a` takes the delimiters |
| `y` | Copy the selection, and leave |
| `yy` `Y` | Copy the line |
| `y` + a motion | Copy what it covers: `yw`, `y$`, `y5j`, `yG`. Whole lines when it crosses rows |
| `yi` + an object | Copy one without selecting it first — `yiw`, `yi"` |
| `yc` | Copy the **code block** the cursor is in, without its indent or language line |
| `ym` | Copy the whole **turn** — the question and everything it produced |
| `yp` | Copy this conversation's **directory** — in a worktree, the worktree's path. The one `y` that is not a piece of the transcript |
| `ya` | Copy the entire transcript. Which is why `ya`*w* is the one thing here that is not Vim's — `viwy` is |
| `i` `a` `o` `⏎` | Back to the composer. While selecting, `i` and `a` open a text object and `o` swaps ends |
| `^S` | And back out, the way you came in |
| `Esc` | One thing per press: the selection, then the search highlight, then the mode |

There is no `^V`: a blockwise selection is a rectangle rather than a range, and every consumer of a
selection would have to grow a case for it. The key says so rather than doing nothing. Nothing here
edits, either — no `d`, `x` or `p`, and `c` is a motion here rather than a change. The transcript
is an artefact you take pieces out of.

**The card you are standing on is open.** Everything a fold hides, up to `chat.preview_lines`, for
as long as the cursor is in it — so `c c c` walks a turn's work one call at a time and the rows go
back as you leave. `⇥` is how you keep one. And **reading the last row keeps reading it**: a turn
still running writes below you, and if you are parked at the end you are carried along with it.
Anywhere else you stay exactly where you are — `G` starts following again.

## Answering a question — the panel over the composer

What an agent gets when it asks *you* something. One question at a time, `⇧⇥` to go back and change
an earlier answer, and **typing is answering** — it goes into the composer, where you can see and
edit it, and the numbered shortcuts disappear to say a digit is now a character. Every key here is
an ordinary binding against the `neosh.question` buffer kind, so `^Z` lists it and `init.ts` can
move it.

| Key | Does |
|---|---|
| `↵` | Take the option under the cursor, and go on to the next question. On one that takes several, confirm what is ticked. With something typed, send that instead |
| `1`–`9` | Take that option, while nothing has been typed |
| `⇥` | Tick or untick the option under the cursor, on a question that takes more than one |
| `^P` `^N`, `↑` `↓` | Move |
| `⇧⇥` | Back to the previous question |
| type | Your own answer, for when none of them is it. `⌫` back to the options, `^W` a word, `^U` all of it |
| `Esc` | Dismiss. The agent is told nobody answered — which is a thing it can act on, not an error |

A question for a conversation you are not looking at waits rather than taking the screen: the footer
says how many are asking, `^T` is where you go, and it opens when you get there.

## The project panel — `^T`

| Key | Does |
|---|---|
| `↵` | Open a conversation, fold a project, or add one from the `+` row |
| `j` `k` | Move. With a count, that many rows you can land on: `5j` |
| `^D` `^U` | Half a screen; `PgUp`/`PgDn` for a whole one. Not `^F`/`^B` — those stay the archive and hiding the panel |
| `gg` `G` | The ends of the list, and `5gg` / `5G` for the fifth row |
| `>` `<` `=` | Wider, narrower, back to the default. It is the `sidebar.width` setting, so `config.toml` and this key say the same number |
| `n` | New conversation — the same question `^N` asks, about the project the cursor is on |
| `o` | Add a project |
| `f` | Pin a project to the top |
| `J` `K` | Reorder within a group |
| `r` | Rename a conversation |
| `y` | Copy the row's directory — a worktree's path, ready to paste into a shell |
| `t` | A **terminal** in this project, in a tab of its own — on this computer, or on the machine the row is on |
| `c` `C` | Connect to that computer, or disconnect from it. Only on a row that is about another machine; `^J` is still where one is added or removed |
| `@` | Choose the short code that computer is drawn with — the `ms` in `@ms` |
| `p` | Bring that repository up to date — fast-forwards silently, asks *rebase or merge* when the branch has gone both ways, and goes and looks when nothing is waiting. The row spins from the press, not from the network call three steps down (a git-plugin contribution) |
| `d` | Remove a worktree from disk — its branch stays, and it asks first (a git-plugin contribution) |
| `x` `X` | Archive, delete. Deleting the last conversation in a worktree takes the checkout with it, and the dialog says so — the branch stays. On a repository's heading, `X` takes the project off the list and leaves the directory; on a worktree's, it removes the checkout too |
| `,` | Settings — the panel below. `,` because it is what `⌘,` is on a Mac, without the key a terminal never receives |
| `a` | The archive — the popup below. Nothing archived is ever a row in *this* panel, and by default not even a count. An `archive.action` contribution, not a key this panel owns |
| `⇥` | On the plan rows: how much of it to show — the limit that binds, every limit, or all of it with the account and the sentence. `usage.sidebar.style` is where it starts, `usage.sidebar` turns it off. A key on a contributed row is named down to the section (`custom:plan`), so the panel sends the press to whichever block the cursor is over rather than to whoever registered it first |
| `?` | The keys for whatever row you are on, as a sheet |
| `Esc` | Back to the composer |

**The keys are beside what they act on, and the row's own keys are on a card next to it.** `^T` sits
on the `PROJECTS` heading and becomes `esc` while you are in the panel; `^O` sits on the `+ Add
project` row and becomes `o`. The strip at the foot keeps only the keys that have no row to sit on —
`^N`, `^F`, `^K`, `^B`, `^Z` outside the panel; `↵`, `?`, `esc` inside — laid out in columns with
the keys in `Sidebar.Key`, because a strip that said every verb for every row in one dim colour was
a paragraph nobody read twice. What a row can do is the **key card**: pause on a row and a float
appears just past the panel's rule, level with the row, one key per line — `↵ space fold or
unfold`, `n new conversation here`, `p pull`, `d remove worktree` — with the contributed verbs
after the panel's own. Built from the *registry* (`keymap.list` on the panel's kind, plus what each
verb says about itself in `ABOUT` and what a `sidebar.action` says about itself), so a key moved in
`init.ts` is printed as the letter that works. It waits `ui.keys.hint_delay` before appearing, the
window prefix's rule, so `^T j j ↵` never flashes one; once up it follows the cursor without
closing; it never takes the keyboard; `sidebar.legend = false` turns it off. `?` is still the whole
sheet.

## The repository is on the row it is about

Not a block above the list. Where a checkout stands is a fact about a *project*, and the panel
already draws one row per project — so it goes on that row, after the name, in the colour of what
each fact is:

```
 PROJECTS
──────────────────────────────────
 ▾ neosh ↓3 ~1 ?1  #86 ✗2       6
   ▸ Chasing the flake       12m
   ▾ ⎇ fix/the-thing ↑2  #91     1
     ▸ Try the grapheme path
   ▾ @lb ⎇ main ≠ 4f3a1c2        3
     · Rework the tab bar      8m
     ▾ ⎇ fix/tab-strip           1
       · Nearly there          now
```

**There was a `GIT` heading with the branch and a verb under it, and it was saying twice what the
row below it already said.** `statParts` has always put `✗2 ↓3 ↑1 ~4 ?7` after a project's name and
`pulls.ts` has always put `#86` after that, coloured by what the pull request *is* — `Forge.Merged`,
`Forge.Open`, `Forge.Draft`, `Forge.Closed`. So the block was a fourth and fifth row about **one**
of the rows underneath it, on a column whose whole job is your conversations. It was also the only
thing here that could disagree with itself: the block read `git status` for the conversation you
were in while the rows read one per project, on two clocks, so a fetch moved one a beat before the
other. And a contribution is workspace-wide while a conversation is per view, so two panes on two
repositories saw one block, about whichever conversation the workspace happened to call current — a
problem a row about a directory has never had, because there is one of them per directory.

**Each fact keeps its own colour, and that is what makes it a glance rather than a string.** A
conflict is a tree you cannot commit from, behind is work you have not got yet, ahead is work
nobody else has, and the two dirty counts are yours and undecided; worst first, nothing said about
zero, so a clean checkout is a row with nothing extra on it. `git.sidebar = false` takes the marks
off the rows — which is what somebody setting it always meant, and it does not stop the *reading*,
because the footer's branch segment is the same `git status`.

**The verb went onto the key, not away.** `p` on any row fast-forwards without asking, and when the
branch has gone both ways it **names the choice** — *rebase or merge*, in those words, as a picker,
because the two do different things to your own commits, one of them rewrites them, and neither is
guessable from a row. When nothing is waiting it fetches instead, which is the honest reading of
that key on a number only as fresh as the last fetch. All three used to live on the block's verb
row, so deleting the block without moving them would have quietly demoted `p` to `git pull` and
whatever it says about a divergence.

**Ahead and behind are asked for, not remembered.** They come off `git status --branch`, which
compares HEAD with the remote-tracking ref *on this disk* — so `↓0` drawn without ever fetching is
the world as of whenever this checkout last spoke to a server, which after an afternoon is a
sentence about breakfast. `ApiCall::GitFetch` is what makes it true: on `git.fetch.interval`
(180 s, `0` never), on arriving in a conversation, and on the key. Only the repository of the
conversation you are in, and only when its branch tracks one.

**And a number whose fetch failed says so.** The block spent a column on `now` / `12m` / `⚠ offline`;
a row spends one glyph on the only part of that you cannot afford to lose. `Git.Stale` is a dim `⚠`
appended to the stats — it *qualifies* them rather than replacing them, `↓3 ⚠` still being three
waiting, just three as of whenever the network last worked — and it appears only when the last fetch
did not reach the remote. `now` and `12m` were not worth a column: a fetch every three minutes is
fresh enough that saying so is noise. Being unreachable is not.

## Settings — `,` in the project panel, `/settings`, or `^K`

Every option in the workspace on one screen, **saved into `config.toml` as you change it** — see the
rule above. A rail of sections on the left (the curated `General` page, a section per namespace,
*Computers* with each machine's code, and *Keys*, the live keymap), the settings on the right with a
control for each declared type, and what the row under the cursor is, its default and whether you
changed it, at the foot. Every key is an ordinary binding on the `neosh.settings` kind.

| Key | Does |
|---|---|
| `⇥` `⇧⇥`, `]` `[`, `J` `K` | Next / previous section |
| `j` `k`, `^D` `^U`, `gg` `G` | Move |
| `h` `l`, `←` `→` | Along a switch or a ladder of values, or nudge a number |
| `<Space>` | The next value along |
| `↵` | Change it: toggle, cycle, or a field to type in. Prose (`agent.append_prompt`, `agent.system_prompt`) gets a field several lines tall — `⏎` is a new line, `esc` keeps it, `^C` throws it away. On *Keys*, runs the command; on *Computers*, changes the code |
| `r` | Back to the default, and the line out of `config.toml` |
| `/` | Find — every word, in any order, over names and descriptions |
| `y` | Copy the option's name |
| `Esc` `q` `^C` | One thing per press: the search, then the panel |

## The archive — `^F`, or `a` in the project panel

What you have finished with, and the one place in the workspace where throwing things away is what
you came to do. **Nothing about it is on screen until you press the key**: no section, no row, no
count — `archive.sidebar = true` if you want the count in the project panel. A popup, and a panel
rather than a picker: it has marks, and every verb means what is marked or — with nothing marked —
the row under the cursor. The strip at the foot says which.

Conversations past `RESTORE_LIMIT` are in here too, and behave like any other row: the workspace
brings one in from its file the first time a key names it. The header says how many are `on disk
only` when any are.

| Key | Does |
|---|---|
| `↵` | Put this one back and go to it |
| `u` | Put these back, and stay here. Tidying and travelling are different intentions. Not `^U` — that is half a screen, here as everywhere |
| `X` | Delete these, for good. It asks, and says how many messages and which projects |
| `^X` | Empty it — everything the panel is **currently showing**, so a filter first is how you empty one project's worth. Asks like the delete it is |
| `<Space>` `⇥` | Tick this row, and step down. Tick a run of them with one key repeated |
| `a` | Tick everything showing, or untick it |
| `e` | Copy these conversations to the clipboard, as markdown |
| `y` | Copy this conversation's directory |
| `/` | Filter — on the title, the project and the path. Every word, in any order |
| `s` `S` | The next ordering along; the next grouping along. They are `archive.sort` and `archive.group`, so `config.toml` and these keys say the same thing |
| `j` `k`, `^D` `^U`, `gg` `G` | Move, with counts — `5j`, `12G` — exactly as in the project panel |
| `?` | The keys |
| `Esc` | One thing per press: the marks, then the filter, then the panel |
| `^F` `q` `^C` | Close it |

The settings: `archive.sidebar` (the count in the project panel, off), `archive.sort`,
`archive.group`, `archive.width`, `archive.height`, and three about time — `archive.auto_days` archives what has been idle that long (reversible, so it may happen on
its own), `archive.retention_days` is what counts as old, and `archive.remind` says so once a day.
`archive.sweep` deletes what is older than that, and only when you run it. Nothing in here deletes
on a timer.

## Pickers

| Key | Does |
|---|---|
| `↵` | Choose |
| `^N`/`^P`, `^J`/`^K`, `↑`/`↓` | Move |
| type | Filter |
| `⇥` `⇧⇥`, `→` `←` | Between the two panes of the model picker |
| `Esc`, `^C` | Close |

In the model picker specifically — chords, because every bare letter a picker takes is a letter its
filter can never contain:

| Key | Does |
|---|---|
| `^S` | Sign in to the provider the rail is on |
| `^R` | Ask that endpoint what it serves again |
| `^A` | Add a model by id, for something the catalogue has never heard of |
| `^D` | Remove one you added |

All of these are settings — `ui.keys.next`, `ui.keys.accept`, and so on — so rebinding them changes
what the pickers say they do.
