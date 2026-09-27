/**
 * The sidebar: your projects, the conversations inside them, and which one is working.
 *
 * Two sections and nothing else. Favourites on top because the projects you actually live in are a
 * short list and a long one buries them; everything else under `PROJECTS`, most recently used
 * first, foldable so a project you are not in costs one line instead of ten.
 *
 * It is deliberately *not* a dashboard. Changed files, the model and the token meter all used to be
 * here and all of them are better in the footer, which is always visible and does not push your
 * conversations off the bottom of the screen. A panel that shows six kinds of thing teaches you to
 * stop reading it.
 *
 * The bottom four lines are the keys for whatever the cursor is on, and they change with it. That
 * is the part worth defending: a terminal UI that hides its verbs is a terminal UI you use three of.
 *
 * Motion is frugal — one shared clock in `@neosh/api/ui`, running only while a turn is in flight,
 * and only the working row animates. Continuous repaint costs power for no information.
 *
 * It is a plugin. Everything here is on the public API, `plugins.disabled = ["sidebar"]` turns it
 * off, and a sidebar of your own loads after this one and wins. But replacing it is the *last*
 * resort rather than the first, and three things are here so that it usually is not necessary:
 *
 * - **Every key in this panel is an ordinary binding**, on the buffer kind `neosh.sidebar`, pointed
 *   at a command with a name. `^Z` lists them, the palette runs them, and one line in your
 *   `init.ts` moves any of them. There is no private switch statement any more — there was, and
 *   what it meant was that adding one key to this panel meant forking all twelve hundred lines of
 *   it.
 * - **`sidebar.section` is a contribution point.** Anything you can describe as rows appears in the
 *   column, in the position you ask for, invoking your commands.
 * - **`sidebar.action` is a verb on a row.** Say the key, the label and the command; this panel
 *   binds it, shows it on the key card when it applies, and invokes your command with the row
 *   under the cursor as arguments.
 *
 * What a project *is* — pinned, ordered, folded — lives in shared project vars rather than in this
 * plugin's private state, so a panel of your own, a status segment or a picker all read the same
 * favourites rather than each starting empty.
 */

import { byteLength, projectScope, width } from "@neosh/api";
import { CLONE_EVENT } from "@neosh/api";
import type {
  AgentSummary,
  CloneProgress,
  GitHead,
  Contribution,
  Disposable,
  DrawnRow,
  KeymapEntry,
  Neosh,
  PluginContext,
  NodeInfo,
  SessionInfo,
  Message,
  LinkState,
  SwarmAgent,
  SwarmNode,
  SwarmStranger,
  BufferId,
  VarScope,
  ViewId,
  WindowId,
  WorktreeInfo,
} from "@neosh/api";
import {
  confirm,
  confirmDestructive,
  configureMotion,
  CursoredList,
  discardWorktrees,
  orphanedWorktrees,
  worktreeAt,
  worktreeLines,
  type Decoration,
  type DecorationItem,
  badgeWidth as badgeColumns,
  decorateRow,
  elapsed,
  fitBadge,
  fuzzy,
  type ListRow,
  type Match,
  mergeDecorations,
  meter,
  placeSections,
  type SectionItem,
  sectionRows as contributedRows,
  money,
  onTick,
  pathPicker,
  picker,
  type PickerItem,
  prettyKey,
  prompt,
  pulseBright,
  pulseHl,
  spinnerFrame,
} from "@neosh/api/ui";

/** What a row points at, so the cursor survives the list being rebuilt underneath it. */
type Target =
  | {
    kind: "project";
    cwd: string;
    /**
     * The other computers this project is also on, by name — display only, for the key card.
     *
     * Not part of what identifies the row: {@link same} compares projects by directory, so a peer
     * connecting between two frames must not move the cursor.
     */
    hosts?: string[];
  }
  | { kind: "session"; id: string; cwd: string }
  /** The row that opens another directory. A row rather than only a key, because a verb nobody
   * can see is a verb nobody uses. */
  | { kind: "add" }
  /**
   * A row somebody else contributed. `command` runs on `↵`, with `args` as given.
   *
   * `section` is which contribution it came from, and it is what lets a verb belong to *one*
   * plugin's block rather than to every contributed row in the column — see {@link applies}.
   */
  | { kind: "custom"; command?: string; args?: string[]; section?: string }
  /** A conversation on another computer. Addressed as `(node, session)`; the session id alone is
   * unique only on its own machine. `link` is where that machine stands, in words, for the key
   * card — which is where the machine's name went when it came off the row. */
  | {
    kind: "remote";
    node: string;
    session: string;
    cwd: string;
    host: string;
    link?: string;
  }
  /**
   * A project that exists only on other computers.
   *
   * A row of its own kind rather than a `project` with somebody else's path in it: every verb this
   * panel binds against `project` — fold, rename, archive, delete, remove from the list — is about
   * a directory on *this* disk, and pointing them at a path that is not here is how a key deletes
   * the wrong thing. What it can do is fold, and start a conversation over there.
   */
  | {
    kind: "elsewhere";
    key: string;
    name: string;
    /** Which computers have it, and where on each — enough to start one without asking again. */
    machines: Array<{ id: string; name: string; cwd: string }>;
    /** Where the link to the machine it is on stands, in words, for the key card. */
    link?: string;
  };

/**
 * One checkout, wherever it is.
 *
 * The unit the panel actually draws rows from, and the thing the first cut did not have. A
 * *project* is a {@link ProjectKey} — an identity, shared by every clone of one repository on every
 * machine — and a *checkout* is a directory on a disk. Four worktrees on this laptop and two on the
 * build box are six checkouts of one project, and the panel's whole job is to say that.
 *
 * `node` is what makes it a fact about a machine rather than about a path: absent is here, present
 * is over there, and every difference in how the row is drawn and what its keys do follows from
 * that one field rather than from a separate kind of row.
 */
interface Checkout {
  /** The path, on whichever machine `node` names. Opaque when it is not this one. */
  cwd: string;
  /** The machine, or `undefined` for this one. */
  node?: { id: string; name: string; link?: LinkState };
  /** The main checkout this is a tree of, on the same machine. Equal to `cwd` in the main one. */
  root?: string;
  /** The branch, which is what a tree's row is named. */
  branch?: string;
  /**
   * The commit it is on, when the machine that owns it said — see `RemoteProject.head`. What
   * decides whether another machine's main checkout is the same version as this one's.
   */
  head?: string;
  /** Conversations in it, on this machine. */
  sessions: SessionInfo[];
  /** Conversations in it, on another. Never both — a checkout is on one machine. */
  agents: SwarmAgent[];
  /** How this checkout's fold is remembered. A local path, or `<node>:<path>`. */
  fold: string;
  /**
   * What the machine that owns it decorates its row with — `RemoteProject.decorations`, which is
   * that machine's own `sidebar.decoration` contributions for this directory. Its git stats and its
   * pull request, as *it* reads them.
   */
  decorations?: unknown[];
}

/**
 * Everything one other computer has of a repository: its main checkout, and its worktrees.
 *
 * **Its own block, always, and never folded into ours.** The first cut put another machine's main
 * checkout into the repository row whenever it looked like the same thing — and "looked like" was
 * the whole problem: what a machine is on is only as fresh as the last thing it said, an older
 * neosh says nothing about its commit at all, and a conversation over there written against code
 * this machine does not have sat in the list beside yours as though it were about your files. So a
 * machine is a row like a worktree, named by its code and its branch, in *its* colour, with its
 * conversations and its worktrees under it and a rail of that colour down the left of the block —
 * so that wherever the cursor is, which computer it is on is the colour of the line beside it.
 */
interface MachineBlock {
  node: { id: string; name: string; link?: LinkState };
  /** Its main checkout of this repository, when it has one — its conversations are the block's. */
  main?: Checkout;
  /** Its worktrees of this repository, nested under the block. */
  trees: Checkout[];
  /** How the block's fold is remembered: `machine:<node>:<project key>`. */
  fold: string;
  /**
   * Whether its main checkout is on a different commit from ours: `true`, `false`, or `undefined`
   * when either side cannot say. Only ever news — drawn when it is `true`.
   */
  differs?: boolean;
}

/**
 * What the sidebar reads to find out what is not its own.
 *
 * Two points rather than one because they are answers to different questions. A *section* is
 * content — rows in the column, which the contributor owns and re-contributes when its data
 * changes. An *action* is a verb — a key on a row this panel already draws, which this panel binds
 * and invokes on the contributor's behalf so that the row under the cursor can be passed along.
 */
const POINT_SECTION = "sidebar.section";
const POINT_ACTION = "sidebar.action";
/**
 * Something to put *on* a row this panel already draws — a git badge on a project, a colour on a
 * conversation — keyed by what the row is about rather than where it is.
 *
 * The third kind of thing a plugin can do to a panel it did not write, and the one the original
 * contribution vocabulary left out: a section is rows of your own, an action is a verb on ours, and this is a mark on ours. Pure
 * data, merged at draw time, so a decorator costs the panel nothing on the paint path and can be
 * listed and disabled like any contribution. It is nvim-tree's decorator API as a contribution.
 */
const POINT_DECORATION = "sidebar.decoration";

/**
 * The blocks this panel draws, in order, for a section to sit `before` or `after`.
 *
 * Published rather than implied: `above`/`below` gave a contributor two slots and no way to land
 * between two of anybody else's sections. A section may also name another section's id.
 */
const SLOTS = ["projects", "add", "machines"] as const;
type Slot = (typeof SLOTS)[number];

/** The key a decoration is filed under: what the row is *about*. */
function targetKey(t: Target | undefined): string | null {
  if (!t) return null;
  if (t.kind === "project") return `project:${t.cwd}`;
  if (t.kind === "session") return `session:${t.id}`;
  return null;
}

/**
 * The marks another machine's panel wears on one of its rows, merged the way ours are.
 *
 * They arrive as that machine's own `sidebar.decoration` contributions — its git stats, its pull
 * request, whatever a plugin over there says — so the row here says exactly what the row there
 * says, and nothing on this machine has to know what any of it means.
 */
function carried(items: unknown[] | undefined): Decoration | undefined {
  if (!items || items.length === 0) return undefined;
  const merged = mergeDecorations(
    items.map((item) => ({ item }) as Contribution & { item: DecorationItem }),
  );
  return merged.values().next().value;
}

/** A turn in flight over there — working, or stopped on a question it is waiting for you to answer. */
function turning(a: AgentSummary): boolean {
  return a.state === "running" || a.state === "blocked";
}

/**
 * A verb on a row, contributed by somebody else.
 *
 * The command is invoked with the row under the cursor as arguments — `[kind, cwd]` for a project,
 * `[kind, cwd, sessionId]` for a conversation — because a key press carries no arguments of its
 * own and a plugin that had to track the cursor separately would be one race away from acting on
 * the wrong row.
 */
interface ActionItem {
  /** Key notation, as `keymap.set` takes it: `d`, `<C-y>`, `gd`. */
  key: string;
  /** What it does, for the key card and for `^Z`. */
  label: string;
  command: string;
  /**
   * Which rows it applies to. Defaults to `any`.
   *
   * `custom` is a row somebody contributed through `sidebar.section` — usually the contributor's
   * own. Without it a plugin can put rows in this column and then has nowhere to put a key that
   * belongs to them: `any` binds the key on every row in the panel and advertises it on all of
   * them, which is a verb about the plan gauge appearing while the cursor is on a conversation.
   *
   * `custom:<section id>` goes one step narrower and names *which* contributed block — `custom:git`
   * is a verb about the git rows and nowhere else. Bare `custom` is every contributed row in the
   * column, which stops being what anybody means the moment there are two blocks in it: the plan
   * and git both wanting `<Tab>` on their own rows is two correct requests and one key, and the
   * panel is what resolves it — see {@link applies}.
   */
  on?: string;
}

/** A contributed verb as this panel bound it: whose it is, and the binding its key points at. */
interface BoundAction extends ActionItem {
  plugin: string;
  /**
   * The command the key was bound to here — the panel's wrapper, not the contributor's `command`,
   * which is what the wrapper runs. `null` when the key was refused, one of ours, and so was never
   * bound at all.
   */
  binding: string | null;
}

/** A project as the panel thinks of it: a directory, and what is going on in it. */
/**
 * A repository, and every checkout of it anywhere in the swarm.
 *
 * **Keyed by identity, not by path.** That is the whole of what changed and what it fixes. The
 * panel used to group local conversations by directory and then draw the other machines' work
 * beside it — matched, in theory, on the project key, and in practice on a key the local side did
 * not have: it was inferred from a remote agent's *cwd*, so the two only ever met when both
 * machines happened to have the repository at the same absolute path. On any real pair of machines
 * — `/Users/me/src/neosh` and `/home/me/neosh` — they never met, so the repository you were sitting
 * in appeared twice: once as yours, and once again underneath as a project you had apparently never
 * heard of, named `neosh · fix/thing` because whichever remote conversation happened to sort first
 * decided the name.
 *
 * So there is one row per repository, and the checkouts hang under it: the local main checkout's
 * conversations directly, a worktree here as a row one level down, and every other machine as a
 * block of its own — see {@link MachineBlock}.
 */
interface Project {
  /** The cross-machine identity. This is what a project *is*. */
  key: string;
  /**
   * The main checkout on **this** machine, or `null` for a repository that is only elsewhere.
   *
   * Every verb this panel binds against a project — fold, rename, archive, delete, remove from the
   * list — is about a directory on this disk, so `null` is what tells the row to offer the ones
   * that are about a repository instead.
   */
  cwd: string | null;
  name: string;
  favorite: boolean;
  /** Conversations in the main checkout, here. */
  sessions: SessionInfo[];
  /**
   * The main checkouts on other machines — the directories themselves, not the work in them.
   *
   * What every verb on the repository row needs — which machine, and where on it — and a project
   * advertised with nothing open in it has no conversation to read that from. Derived from
   * conversations alone, `n`, `t`, `c` and `C` on such a row all did nothing at all, which is
   * precisely the row you press them on: the repository over there you have not started work in
   * yet.
   */
  mains: Checkout[];
  /**
   * Every worktree of it on **this** machine.
   *
   * A worktree used to be a top-level project — correct by the old rule that a worktree is a
   * project, and wrong as a *list*: four scratch trees of one repository read as four unrelated
   * projects, and the thing they have in common is the thing the column no longer said.
   */
  worktrees: Checkout[];
  /** Every other machine that has it, one block each. See {@link MachineBlock}. */
  machines: MachineBlock[];
  /** Which other computers have it, by name — display only, for the key card. */
  hosts: string[];
  /** The same computers by id, for looking up the short code each one is drawn with. */
  nodes: string[];
  /** The best link any machine holding it has, which is what the marker on the row is coloured by. */
  link?: LinkState;
}

const NS = "neosh.sidebar";
/** What this panel's buffer says it is. Everything a third party binds or finds hangs off this. */
const KIND = "neosh.sidebar";

/** Emitted on every cursor move, with the row under it as `data` — a {@link Target} or `null`. */
const EVENT_CURSOR = "sidebar.cursor";

/** The key card beside the panel. A kind of its own, so a theme or a plugin can find it. */
const KIND_KEYS = "neosh.sidebar.keys";

/**
 * What each of this panel's verbs says on each kind of row, for the key card.
 *
 * Keyed by command name and holding only the *words*: the key itself is read out of the registry
 * when the card is drawn, so a binding somebody moved in `init.ts` shows the letter that works —
 * a legend is a promise about a keyboard. A verb with no entry here is about the panel rather than
 * about a row (a motion, the width, the way out) and the card's foot covers those. Two keys with
 * the same words on one kind of row share a line, which is what `↵` and `space` on a project are.
 */
const ABOUT = new Map<string, Partial<Record<Target["kind"], string>>>();

/**
 * What this panel offers a plugin that imports it: `import { api } from "plugin:sidebar"`.
 *
 * The same answers are on the commands `sidebar.cursor` and `sidebar.rows` for a plugin that would
 * rather `cmd.call` than depend on us; this is the typed, zero-round-trip form for one that has
 * put `requires = ["sidebar"]` in its manifest. Filled in by `activate`, so a caller that reads it
 * before then sees a panel that is not open and has no rows — which is true.
 */
export const api = {
  /** The buffer kind, for `keymap.set` at `buf_kind` scope and `win.ofKind`. */
  kind: KIND,
  /** The contribution points this panel reads. */
  points: { section: POINT_SECTION, action: POINT_ACTION, decoration: POINT_DECORATION },
  /** The blocks a section may sit `before` or `after`, in the order they are drawn. */
  slots: SLOTS as readonly string[],
  /** The row under the cursor, or `null` when the panel is closed or on nothing. */
  cursor: (): Target | null => null,
  /** Every row that can be landed on, top to bottom. */
  rows: (): Target[] => [],
  /** Redraw now rather than on the next tick. */
  refresh: async (): Promise<void> => {},
};

/**
 * Project vars, which are shared rather than ours.
 *
 * These used to be three arrays in this plugin's private state, which worked exactly as long as
 * this was the only panel: a sidebar of your own started with no favourites, and pinning a project
 * here was invisible to a status segment that wanted to say so. They are namespaced `sidebar.*`
 * because that is what a var key is expected to look like, not because they belong to us — anybody
 * may set them, and this panel redraws when they do.
 */
const VAR_FAVORITE = "sidebar.favorite";
const VAR_RANK = "sidebar.rank";
const VAR_FOLDED = "sidebar.folded";
/**
 * The folded rows that are not a directory here — a machine's section, its block, a repository only
 * over there — as a workspace var holding their keys. Not `sidebar.folded`, which is a *project*
 * var: a project var is keyed by a directory, and one keyed by anything else was read back as a
 * directory somebody had mentioned.
 */
const VAR_FOLDED_AWAY = "sidebar.folded.elsewhere";
/**
 * The projects, in workspace scope. This list *is* what the panel shows.
 *
 * It used to be an index — a hint, with the real list derived from the conversations that happened
 * to live somewhere. That is the bug this replaced: deleting the last conversation in a directory
 * deleted the directory too, so clearing out a project you had spent a month in removed it from the
 * panel and there was nothing left to start the next conversation from. A project is a place you
 * work, not a property of the work in it.
 *
 * Kept current from three ends — every conversation's directory goes in, so does any directory
 * somebody sets a project var on, and so does anything added by hand — and taken out of by exactly
 * one thing, which is asking for it to go. See [`Arrangement.forget`].
 */
const VAR_KNOWN = "sidebar.projects";
/**
 * What a project is called, remembered for when there is nothing in it to ask.
 *
 * The name comes from the host, which reads it off the repository — `neosh (feat/thing)` for a
 * worktree rather than the directory's own `wt-fe3c0d93` — and it arrives stamped on a conversation.
 * A project with no conversations left has nobody to ask, and falling back to the basename renames
 * a worktree you have used for a fortnight into something you do not recognise the moment you empty
 * it. Written when it is known, read when it is not.
 */
const VAR_NAME = "sidebar.name";
/**
 * The checkout a directory is a linked worktree of, for when there is nothing in it to ask.
 *
 * Which tree belongs to which repository is read off a conversation's `repo_root`, so a worktree
 * you have emptied has nobody to say it — and without this it stops being a worktree the moment it
 * stops having conversations, jumps out of the repository it belongs to and lands at the top level
 * as a project of its own. Which is the worktree-nesting rule, arrived at from the other
 * direction: a tree does not become a project by being idle.
 */
const VAR_ROOT = "sidebar.root";
/**
 * What a project is called **across machines** — its {@link ProjectKey}, as a string.
 *
 * Remembered for the same reason the name and the root are: a project stays on the list after the
 * last conversation in it has gone, and the key is read off a conversation. Without it an emptied
 * repository falls back to `dir:<name>` and stops matching the same repository on the build box —
 * so the row that had been one would silently become two, on the day it had least going on.
 */
const VAR_KEY = "sidebar.key";
/**
 * Which conversations have stopped and are waiting on an answer.
 *
 * Written by whatever serves `ask_user` — the bundled `questions` plugin, or yours instead of it —
 * and read here, which is the whole reason it is a var and not that plugin's private state. Nothing
 * else in this column can be worked out: a conversation blocked on a question still has a turn in
 * flight, so `active_turn` is set and the row it draws is the spinner, identical to the twenty above
 * it that are actually thinking. An answer nobody is going to give looks like work in progress until
 * you happen to open it.
 *
 * So it outranks *working* wherever the two meet below, and it wears `Status.Pending` — the
 * palette's own group for waiting on something outside the program, which is what the footer's
 * `Question.Waiting` links to, so the two say the same thing in the same colour without this panel
 * having to know the other one exists.
 */
const VAR_ASKING = "question.asking";
/**
 * The same, for a permission prompt. Written by whatever serves `permission_pre` — the bundled
 * `approvals` plugin — and folded into the same set: a conversation blocked on "may I run this" is
 * blocked exactly as one blocked on "which database", and drew as an ordinary spinner until this
 * was read.
 */
const VAR_PERMITTING = "permission.asking";
/**
 * The short code each other computer is drawn with, where you chose one: `{ "<node id>": "gpu" }`.
 *
 * A workspace var rather than this panel's state, because a code is what you call a machine and
 * every panel that names one should say the same thing. Anything not in it is worked out from the
 * machine's name — see {@link machineCodes}.
 */
const VAR_CODES = "swarm.codes";

export async function activate({ neosh, subscriptions }: PluginContext) {
  await declareOptions(neosh);

  const applyMotion = async () => {
    configureMotion({
      enabled: (await neosh.opt.get<boolean>("ui.motion")) ?? true,
      ascii: (await neosh.opt.get<boolean>("ui.ascii_only")) ?? false,
    });
  };
  await applyMotion();

  // How you left it. Read once into memory; every later write goes to the cache and to the shared
  // var store together, so a redraw never waits on a round trip.
  const arrangement = new Arrangement(neosh);
  await arrangement.load(
    (await neosh.session.list({ includeArchived: true }).catch(() => [] as SessionInfo[]))
      .map((s) => s.cwd),
  );

  // Read once and then kept current from `vars.onChange`, like the arrangement and for the same
  // reason: a draw runs on a 100 ms tick while a turn is in flight and must not spend a round trip
  // per frame asking a question whose answer changes twice an hour.
  const waiting = {
    questions: asked(
      await neosh.vars.get<string[]>({ scope: "global" }, VAR_ASKING).catch(() => null),
    ),
    permissions: asked(
      await neosh.vars.get<string[]>({ scope: "global" }, VAR_PERMITTING).catch(() => null),
    ),
  };
  let asking = new Set([...waiting.questions, ...waiting.permissions]);

  // One panel per terminal.
  //
  // A workspace can have several and they are not copies of each other: which conversation is
  // open, which row the cursor is on, whether the column is showing at all. All of that is
  // navigation, and navigation is what a view *is* — so the buffer is per view too, since the
  // cursor and the unfolded row are drawn into its text.
  const panels = new Map<ViewId, Panel>();

  const makePanel = async (view: ViewId): Promise<Panel> => {
    // The kind is what makes this panel something other plugins can act on: it is the scope their
    // keymaps bind at and the handle `win.ofKind` finds it by. One argument, and the difference
    // between a panel you can extend and one you can only replace.
    const buf = await neosh.buf.create({ name: "[sidebar]", scratch: true, kind: KIND });
    const ns = await neosh.ns.create(NS);
    // In this terminal, and only this one. Everything else on `here` is the call it always was.
    const here = neosh.view.at(view);
    // The panel's own width, as of the last frame. The list needs it to unfold the row under the
    // cursor, and reading the option again per render would be a round trip inside a redraw.
    let panelWidth = 34;
    const list = new CursoredList<Target>(here, buf, ns, {
      width: () => panelWidth,
      // Said out loud on every move, so a plugin can follow the cursor without polling — a
      // preview of the conversation under it, a status segment naming the project. One event per
      // keystroke, which is what the composer already costs.
      onMove: () => void neosh.event.emit(EVENT_CURSOR, list.value ?? null),
    });
    let win: WindowId | null = null;
    let focused = false;
    // Typed before a motion and consumed by it. Shared with the key table and with the draw, which
    // is what puts it on screen while it is half typed.
    const count = { pending: "" };
    let capture: Disposable | null = null;
    let running = false;
    // The keys for the row under the cursor, beside the panel. Through `here`, so it opens on the
    // terminal this panel is on; `arming` is the delay before it appears, cancelled by leaving.
    const card = new KeyCard(here);
    let arming: Disposable | null = null;
    /** Where the card goes: just past the panel's rule, level with the cursor's row. */
    const cardAt = (): [number, number] => [panelWidth + 1, list.cursorScreenRow ?? 0];

    // Serialises redraws. Two overlapping refreshes interleave their `setLines` and `mark` calls
    // and leave highlights pointing at rows that have already been replaced.
    let drawing = false;
    let again = false;

    const draw = async () => {
      if (win === null) return;
      if (drawing) {
        again = true;
        return;
      }
      drawing = true;
      try {
        do {
          again = false;
          // Read per frame rather than cached at load: a setting arrives from `config.toml` after
          // this plugin has declared it, and a value captured at activation would be the default
          // forever.
          const [width, ascii, hints] = await Promise.all([
            neosh.opt.get<number>("sidebar.width").catch(() => null),
            neosh.opt.get<boolean>("ui.ascii_only").catch(() => null),
            neosh.opt.get<boolean>("sidebar.hints").catch(() => null),
          ]);
          panelWidth = width ?? 34;
          const built = await collect(here, arrangement, {
            width: panelWidth,
            ascii: ascii ?? false,
            hints: hints ?? true,
            focused,
            selected: list.value,
            actions: actions(),
            asking,
            count: count.pending,
            // Filled in by `collect`, which is what reads the swarm and the decorations.
            decorations: new Map(),
            links: new Map(),
            codes: new Map(),
            colors: new Map(),
            folded: (key) => arrangement.isFolded(key),
          });
          running = built.running;
          list.setRows(built.rows, same);
          await list.render({
            showCursor: focused,
            win: win ?? undefined,
            pinned: built.pinned,
          });
          // After the render, which is what decides which screen row the cursor's row is on. A
          // draw that changed nothing the card says is a draw the card ignores.
          if (focused && win !== null && card.isOpen()) {
            const [col, row] = cardAt();
            await card.draw(list.value, actions(), win, col, row);
          }
        } while (again);
      } finally {
        drawing = false;
      }
    };

    /**
     * The open in flight, so two callers cannot each make a window.
     *
     * `win` is assigned two `await`s after it is tested — a host round trip for the width, then
     * the window itself — so `if (win === null)` is a check whose answer is stale by the time it
     * is acted on. Two calls in that gap both saw `null`, both opened a column, and `win` kept
     * only the second: the first stayed on screen and could never be closed again, because
     * `close` closes `win`. Two sidebars, from pressing `^B` or `^T` twice quickly, or from a key
     * arriving while `view.onOpen` was still building the panel.
     *
     * Concurrent callers await the same open rather than starting another. They all wanted the
     * same end state and now they all get it — which is why this returns the promise instead of
     * dropping the second call, whose caller is about to `focus` something.
     */
    let opening: Promise<void> | null = null;

    const open = (): Promise<void> => {
      if (opening) return opening;
      const run = (async () => {
        if (win === null) {
          // Read defensively: a reload takes every declared option away and puts it back, and this
          // can be called from a view arriving in the gap. A panel that throws there used to take
          // the whole plugin runtime with it.
          const width = (await neosh.opt.get<number>("sidebar.width").catch(() => null)) ?? 34;
          win = await here.win.open(buf, "left", { size: width });
        }
        await draw();
        // And again once the frontend has said how tall the panel turned out to be. The foot is
        // held against the bottom edge, which is a measurement — and on the first frame there is
        // nothing to measure yet, so without this the strip sits under the last project until
        // something else happens to redraw.
        // Not held in `subscriptions`: the panel is opened and closed as often as `^B` is pressed,
        // and the runtime cancels a plugin's timers when it unloads anyway.
        neosh.timer.after(120, () => void draw());
      })();
      opening = run.finally(() => {
        opening = null;
      });
      return opening;
    };

    const leave = async () => {
      if (!focused) return;
      focused = false;
      capture?.dispose();
      capture = null;
      arming?.dispose();
      arming = null;
      await card.close();
      await here.focus.pop().catch(() => {});
      await draw();
    };

    /**
     * Put the key card up, after the delay the window prefix's list waits.
     *
     * `ui.keys.hint_delay` rather than a setting of this panel's own, because it is the same
     * bargain: a panel that appears while you are still typing is a flash, and one that appears
     * when you have paused is an answer. Both are read here rather than at activation, so a change
     * in `config.toml` takes effect the next time the panel is entered.
     */
    const armCard = async () => {
      const wanted = (await neosh.opt.get<boolean>("sidebar.legend").catch(() => null)) ?? true;
      if (!wanted) return;
      const delay = (await neosh.opt.get<number>("ui.keys.hint_delay").catch(() => null)) ?? 300;
      arming?.dispose();
      arming = neosh.timer.after(Math.max(0, delay), () => {
        arming = null;
        void (async () => {
          if (!focused || win === null) return;
          const [col, row] = cardAt();
          await card.open(win, col, row);
          await card.draw(list.value, actions(), win, col, row);
        })().catch((e: unknown) => neosh.log.warn(`sidebar: key card: ${String(e)}`));
      });
    };

    const close = async () => {
      // Let an open that is still in flight finish first. `^B` twice quickly is open-then-close,
      // and a close that reads `win` while the open has not assigned it yet sees `null`, returns
      // having done nothing, and leaves the column that arrives a moment later on screen — with
      // the panel now believing it is shut. The next `^B` opens a second one.
      if (opening) await opening.catch(() => {});
      if (win === null) return;
      const w = win;
      win = null;
      await leave();
      await neosh.win.close(w).catch(() => {});
    };

    const enter = async () => {
      await open();
      if (win === null || focused) return;
      focused = true;
      await neosh.focus.push(win);
      // Every key the bindings did not claim comes here, which is what makes `j`, `f` and `?` mean
      // something in the panel while `^Q` still quits.
      capture = await neosh.keymap.capture(win, `${NS}.key`).catch(() => null);
      // Land on the conversation you are in, not wherever the cursor happened to be — and *this*
      // terminal's, which is the whole reason the panel is per view.
      const current = await here.session.current().catch(() => null);
      if (current?.mirror) {
        // Another machine's conversation: its row is the remote one, under that machine's section.
        const m = current.mirror;
        arrangement.unfold(`host:${m.node}`);
        list.select((t) => t.kind === "remote" && t.node === m.node && t.session === m.session);
      } else if (current) {
        // Unfold the project it lives in, or "go to where I am" lands on a collapsed heading. For
        // a conversation in a worktree that is two folds: the repository's, then the worktree's.
        if (current.repo_root) arrangement.unfold(current.repo_root);
        arrangement.unfold(current.cwd);
        list.select((t) => t.kind === "session" && t.id === current.id);
      }
      await draw();
      await armCard();
    };

    return {
      view,
      here,
      buf,
      list,
      count,
      draw,
      open,
      close,
      enter,
      leave,
      isOpen: () => win !== null,
      isRunning: () => running,
      width: () => panelWidth,
      setWidth: (n: number) => {
        panelWidth = n;
        if (win !== null) void neosh.win.resize(win, n).then(draw).catch(() => {});
      },
      height: async () => {
        if (win === null) return 20;
        const v = await neosh.win.viewport(win).catch(() => null);
        return v?.height ?? 20;
      },
      dispose: () => {
        capture?.dispose();
        capture = null;
        arming?.dispose();
        arming = null;
        // Not closed here: the terminal has gone and the host took its windows with it. Closing a
        // window that is already gone is an error message about nothing.
        win = null;
        card.forget();
      },
    };
  };

  /** The panel in a named terminal, or the one being served when nothing is named. */
  const panel = (view?: ViewId): Panel | null => {
    if (view !== undefined) return panels.get(view) ?? null;
    // A command run by name rather than by key — `^K`, a plugin — names no terminal. There is
    // usually one, and when there is not, the one that has a window open is the one somebody is
    // looking at.
    const all = [...panels.values()];
    return all.find((p) => p.isOpen()) ?? all[0] ?? null;
  };

  const each = (fn: (p: Panel) => unknown) => {
    for (const p of panels.values()) void fn(p);
  };

  const drawAll = () => each((p) => p.draw());
  // The module-level `api`, filled in now that there is something behind it. "The" cursor with
  // several terminals is the served panel's — the same answer every by-name command gives.
  api.cursor = () => panel()?.list.value ?? null;
  api.rows = () => panel()?.list.values ?? [];
  api.refresh = async () => drawAll();

  // The verbs other plugins put on our rows. Bound here rather than by them so the row under the
  // cursor can be handed along; see `installActions`.
  const installed = installActions(neosh, panel, drawAll);
  const actions = installed.actions;
  subscriptions.push(installed.dispose);

  await registerCommands({ neosh, subscriptions, arrangement, panel, each });

  // Text changes: a turn started or ended, the conversation changed, a setting moved.
  subscriptions.push(neosh.agent.onTurnStart(drawAll));
  subscriptions.push(neosh.agent.onTurnEnd(drawAll));
  subscriptions.push(neosh.agent.onToolStart(drawAll));
  subscriptions.push(neosh.session.onChange(drawAll));
  // What a conversation is still running after its turn ended. The only state on these rows that
  // moves without a turn starting or ending — that is the whole point of it — so nothing else this
  // panel already listens to would ever bring it. Filtered to the one kind, because a driver
  // reports its context size every few seconds and redrawing the column for that is a column that
  // redraws for nothing.
  subscriptions.push(
    neosh.agent.onActivity((e) => {
      if (e.activity.kind === "background") drawAll();
    }),
  );
  // Somebody pinned, folded or tagged a project — possibly us, possibly a plugin that has never
  // heard of this one. Both arrive here, and the arrangement takes them in the same way.
  subscriptions.push(
    neosh.vars.onChange((e) => {
      if (arrangement.observe(e.scope, e.key, e.value)) drawAll();
      // A machine's code changed — `@` on a row, `^T` in `^J`, or anybody's plugin.
      if (e.scope.scope === "global" && e.key === VAR_CODES) drawAll();
      // Somebody started waiting on an answer, or stopped. Deleting the var is how "nobody is
      // asking" arrives, and it comes through here with `value` unset rather than as an empty list.
      if (e.scope.scope === "global" && (e.key === VAR_ASKING || e.key === VAR_PERMITTING)) {
        if (e.key === VAR_ASKING) waiting.questions = asked(e.value);
        else waiting.permissions = asked(e.value);
        asking = new Set([...waiting.questions, ...waiting.permissions]);
        drawAll();
      }
    }),
  );
  // A worktree moved. The conversations in it have already followed — the host re-points them —
  // but this list is a list of *directories*, and nothing about a conversation's `cwd` changing
  // says whether one project moved or one went and another arrived. Which is the whole difference:
  // the second reading loses the place you put the row in.
  subscriptions.push(
    neosh.project.onMove((e) => {
      void arrangement.relocate(e.from, e.to).then((moved) => {
        if (moved) drawAll();
      });
    }),
  );
  // Rows somebody contributed came or went. Redrawing on this is what stops a plugin that loads
  // after us contributing rows nobody sees until the next unrelated refresh.
  subscriptions.push(
    neosh.ext.onChange((e) => {
      if (e.point === POINT_SECTION || e.point === POINT_DECORATION) drawAll();
    }),
  );
  subscriptions.push(
    neosh.opt.onChange((e) => {
      if (e.name === "ui.motion" || e.name === "ui.ascii_only") {
        void applyMotion().then(drawAll);
      } else if (e.name === "sidebar.width") {
        // In place. Reopening was how this used to work, and it gave the panel a new window id and
        // dropped whatever had the keyboard — so resizing from inside the panel threw the cursor
        // back to the composer on every press.
        const n = typeof e.value === "number" ? e.value : null;
        if (n !== null) each((p) => p.setWidth(n));
      } else if (e.name.startsWith("sidebar.") || e.name === "ui.theme") {
        drawAll();
      }
    }),
  );

  // Attributes change: the shared 100 ms clock, and only while a turn is in flight. An idle
  // workspace has no timer at all.
  subscriptions.push(
    onTick(() => {
      each((p) => {
        if (p.isOpen() && p.isRunning()) void p.draw();
      });
    }),
  );

  await installFooter(neosh, subscriptions);

  const period = (await neosh.opt.get<number>("sidebar.refresh_ms")) ?? 4000;
  subscriptions.push(neosh.timer.every(period, drawAll));

  // One panel per terminal, and one for every terminal that was already here when this plugin
  // loaded. `sidebar.open` decides whether it starts showing, per view rather than once for the
  // workspace: `^B` in one window is `^B` in that window.
  const startOpen = (await neosh.opt.get<boolean>("sidebar.open")) ?? true;
  // Panels being built, keyed by the terminal they are for. The same check-then-act as `open`, one
  // level up and worse: `panels.set` lands after `makePanel` resolves, so two events for one view
  // in that gap built two complete panels — two buffers, two windows — and the map kept the
  // second. Reserved *synchronously*, before the first `await`, which is what makes the test and
  // the claim one step.
  const making = new Map<ViewId, Promise<Panel>>();
  subscriptions.push(
    neosh.view.onOpen((view) => {
      void (async () => {
        if (panels.has(view) || making.has(view)) return;
        const building = makePanel(view);
        making.set(view, building);
        let made: Panel;
        try {
          made = await building;
        } finally {
          making.delete(view);
        }
        panels.set(view, made);
        if (startOpen) await made.open();
      })().catch((e: unknown) => {
        // A panel that cannot be built is one terminal without a column, not a workspace without
        // plugins. Unhandled, the rejection stops the runtime and takes every other plugin with
        // it — which is what a reload racing this used to do.
        panels.delete(view);
        neosh.log.warn(`sidebar: ${String(e)}`);
      });
    }),
  );
  subscriptions.push(
    neosh.view.onClose((view) => {
      // The windows went with the terminal; what is left is what we were keeping about them, which
      // would otherwise be a capture pointed at a window that no longer exists.
      panels.get(view)?.dispose();
      panels.delete(view);
    }),
  );
  // And when *we* are the ones going away, which is not the same thing and is the one this panel
  // got wrong. `dispose` deliberately does not close a window, because its only caller was a
  // terminal that had already gone and taken its windows with it. A reload is the opposite: every
  // terminal is still there, still showing a column that nothing is going to take off the screen,
  // and nothing disposed the panels on this path at all. So `^R` left the old sidebar standing,
  // the reloaded plugin's `view.onOpen` built a second one beside it, and the workspace had two —
  // one of them drawn by code that no longer existed and answering no keys.
  //
  // Closed rather than merely forgotten, and fire-and-forget because a `Disposable` is
  // synchronous: the op is in flight before the runtime tears down, and a failure here means the
  // window was going anyway.
  subscriptions.push({
    dispose: () => {
      for (const p of panels.values()) {
        void p.close().catch(() => {});
        p.dispose();
      }
      panels.clear();
    },
  });
}

/**
 * Whether two rows are the *same row*, so the cursor survives the list being rebuilt under it.
 *
 * The panel redraws on a tick and re-contributed sections replace themselves wholesale, so this is
 * what stands between "the row I was on moved" and "I am somewhere else now". Every kind is
 * compared by what identifies it rather than by what it says: a conversation by its id, a project
 * by its directory, a remote by the pair that is unique across machines.
 *
 * **A contributed row is identified by its section and its verb.** It used to fall through to
 * `a.kind === b.kind`, which made every custom row in the column identical to every other — so on
 * the next redraw the cursor snapped to the *first* one, wherever it had been. Two rows of the plan
 * strip were enough to see it; a block whose second row is the button and whose first row is a
 * label made it a button you could not reach, because the cursor was back on the label before you
 * pressed the key.
 */
function same(a: Target, b: Target): boolean {
  if (a.kind === "remote") {
    return b.kind === "remote" && a.node === b.node && a.session === b.session;
  }
  if (a.kind === "session") return b.kind === "session" && a.id === b.id;
  if (a.kind === "project") return b.kind === "project" && a.cwd === b.cwd;
  // By its project key, which is the only identity it has: there is no directory here to name it
  // by. Left to fall through to `a.kind === b.kind`, every remote-only project in the column was
  // the same row, and the cursor snapped to the first of them on every redraw.
  if (a.kind === "elsewhere") return b.kind === "elsewhere" && a.key === b.key;
  if (a.kind === "custom") {
    // The args too: a section whose rows all run one command with a different argument each — a
    // list of branches, a list of pull requests — is exactly the case the command alone cannot tell
    // apart, and it is a common enough shape to be worth the join.
    return b.kind === "custom" && a.section === b.section && a.command === b.command &&
      (a.args ?? []).join("\0") === (b.args ?? []).join("\0");
  }
  return a.kind === b.kind;
}

async function declareOptions(neosh: Neosh): Promise<void> {
  await neosh.opt.declare({
    name: "sidebar.open",
    type: { type: "bool" },
    default: true,
    description: "Show the sidebar at startup.",
  });
  await neosh.opt.declare({
    name: "sidebar.width",
    type: { type: "int", min: 16, max: 120 },
    default: 34,
    description: "Sidebar width in columns.",
  });
  await neosh.opt.declare({
    name: "sidebar.hints",
    type: { type: "bool" },
    default: true,
    description:
      "Show the main keys at the foot of the panel. Turn it off once they are in your fingers.",
  });
  await neosh.opt.declare({
    name: "sidebar.legend",
    type: { type: "bool" },
    default: true,
    description:
      "While the panel has the keyboard, a card beside it lists every key for the row under the cursor. It appears after `ui.keys.hint_delay`, so moving through the list never flashes one.",
  });
  await neosh.opt.declare({
    name: "sidebar.refresh_ms",
    type: { type: "int", min: 200, max: 60000 },
    default: 4000,
    description: "How often to re-read the workspace when nothing has told us it changed.",
  });
}

// ---------------------------------------------------------------------------
// Arrangement
// ---------------------------------------------------------------------------

/**
 * Which projects are pinned, what order you put them in, and which ones are folded.
 *
 * All of it in **project vars**, which is the difference between an arrangement and a private
 * arrangement: a var is scoped to the project it describes and anybody may read or write it, so a
 * second panel sees the same favourites and setting `sidebar.favorite` from your own plugin is a
 * one-line way to pin something.
 *
 * Cached in memory and kept current from `vars.onChange`, because a panel redraws several times a
 * second while a turn is running and a round trip per project per frame is not a thing to pay for.
 * The cache is safe precisely *because* the change event is broadcast: every write, ours or
 * anybody's, comes back through the same door.
 *
 * Not options, for the reason it has never been options: an option is configuration *you* write,
 * and an editor that rewrites your config file because you pressed `f` is one you stop trusting
 * with the file.
 */
class Arrangement {
  /** Every project we know of, and its vars. The cache is the read path; vars are the truth. */
  private cache = new Map<string, Record<string, unknown>>();
  private known: string[] = [];
  /**
   * Which rows that are *not* a directory here are folded — a machine's section, its block inside
   * one of your projects, a repository that is only over there. See {@link VAR_FOLDED_AWAY}.
   */
  private away = new Set<string>();

  constructor(private readonly neosh: Neosh) {}

  async load(cwds: string[]): Promise<void> {
    const stored = await this.neosh.vars
      .get<string[]>({ scope: "global" }, VAR_KNOWN)
      .catch(() => null);
    this.away = new Set(strings(
      await this.neosh.vars.get<string[]>({ scope: "global" }, VAR_FOLDED_AWAY).catch(() => null),
    ));
    // Only directories. A fold of a row about another machine used to be written as a project var
    // keyed by that row's name — `host:…`, `git:github.com/…` — and `observe` then took the name for
    // a directory somebody had mentioned and put it on this list, where it came back as a project
    // of yours called whatever the name ended in: the row you had just folded jumped into your own
    // section and the machine's lost it. Anything like that already written down is dropped here.
    this.known = unique([...strings(stored), ...cwds]).filter(isDirectory);
    const all = await Promise.all(
      this.known.map((cwd) =>
        this.neosh.vars.all(projectScope(cwd)).catch(() => ({} as Record<string, unknown>))
      ),
    );
    this.known.forEach((cwd, i) => this.cache.set(cwd, all[i] ?? {}));
    if (stored === null) await this.migrate();
    // Written back whenever the startup list added to it — including the very first start, when
    // nothing was stored. The var is the list other plugins are told to read (a decorator asks it
    // which projects to decorate), and a list that only reached the disk once a *second* project
    // appeared was a list that said nothing on the day it mattered most.
    if (this.known.length !== strings(stored).length) {
      await this.neosh.vars.set({ scope: "global" }, VAR_KNOWN, this.known);
    }
  }

  /**
   * Bring across an arrangement made before any of this was shared.
   *
   * These three lived in this plugin's private state until project vars existed. Losing somebody's
   * pins because the store moved underneath them is not an acceptable way to ship an improvement,
   * so the old keys are read once — on the one startup where the index does not exist yet — and
   * written into vars. The old state is left where it is rather than deleted: it costs a few bytes,
   * and a downgrade that finds it still there is a downgrade that still works.
   */
  private async migrate(): Promise<void> {
    const [favorites, order, folded] = await Promise.all([
      this.neosh.state.get<string[]>("favorites").catch(() => null),
      this.neosh.state.get<string[]>("order").catch(() => null),
      this.neosh.state.get<string[]>("folded").catch(() => null),
    ]);
    const pins = strings(favorites);
    const ranks = strings(order);
    const shut = strings(folded);
    if (pins.length === 0 && ranks.length === 0 && shut.length === 0) return;

    await this.note(unique([...pins, ...ranks, ...shut]));
    await Promise.all([
      ...pins.map((cwd) => this.set(cwd, VAR_FAVORITE, true)),
      ...ranks.map((cwd, i) => this.set(cwd, VAR_RANK, i)),
      ...shut.map((cwd) => this.set(cwd, VAR_FOLDED, true)),
    ]);
    this.neosh.log.info(`brought ${this.known.length} project arrangements across to shared vars`);
  }

  /**
   * Take in a change somebody made — including our own writes, which arrive by the same route.
   *
   * Answers whether it was about a project at all, so a caller can skip a redraw for a var that has
   * nothing to do with this panel.
   */
  observe(scope: VarScope, key: string, value: unknown): boolean {
    if (scope.scope === "global" && key === VAR_FOLDED_AWAY) {
      this.away = new Set(strings(value));
      return true;
    }
    if (scope.scope !== "project") return false;
    // A project var about something that is not a directory is not a project arriving.
    if (!isDirectory(scope.cwd)) return false;
    const entry = this.cache.get(scope.cwd) ?? {};
    if (value === null || value === undefined) delete entry[key];
    else entry[key] = value;
    this.cache.set(scope.cwd, entry);
    // A project somebody else has just said something about is a project this panel should be
    // showing. Without this, pinning a directory from another plugin sets a var nothing ever reads.
    //
    // A var being *removed* is not somebody saying something about a project — it is the last thing
    // `forget` does, and treating it as an announcement would put the project straight back.
    if (value !== null && value !== undefined && !this.known.includes(scope.cwd)) {
      this.known.push(scope.cwd);
      void this.neosh.vars.set({ scope: "global" }, VAR_KNOWN, this.known);
    }
    return true;
  }

  /** Remember directories that turned up in the conversation list. */
  async note(cwds: string[]): Promise<void> {
    const fresh = cwds.filter((c) => isDirectory(c) && !this.known.includes(c));
    if (fresh.length === 0) return;
    this.known.push(...fresh);
    await Promise.all(
      fresh.map(async (cwd) => {
        this.cache.set(
          cwd,
          await this.neosh.vars.all(projectScope(cwd)).catch(() => ({})),
        );
      }),
    );
    await this.neosh.vars.set({ scope: "global" }, VAR_KNOWN, this.known);
  }

  /** Every project, whether or not there is anything going on in it. The panel's list. */
  all(): string[] {
    return [...this.known];
  }

  /** What this project was called the last time anything knew. `null` when nothing ever did. */
  name(cwd: string): string | null {
    const v = this.cache.get(cwd)?.[VAR_NAME];
    return typeof v === "string" && v !== "" ? v : null;
  }

  /** The checkout this one is a worktree of, if anything ever said so. */
  root(cwd: string): string | null {
    const v = this.cache.get(cwd)?.[VAR_ROOT];
    return typeof v === "string" && v !== "" && v !== cwd ? v : null;
  }

  /** What this project is called across machines, if anything ever said so. See {@link VAR_KEY}. */
  key(cwd: string): string | null {
    const v = this.cache.get(cwd)?.[VAR_KEY];
    return typeof v === "string" && v !== "" ? v : null;
  }

  /**
   * Write down what the host calls a project, for when it is empty.
   *
   * Called from the draw path with every project that has a conversation to read a name off, so it
   * has to be free when nothing has changed — which it is: the cache is checked first and the
   * common case is a comparison per project per frame.
   */
  remember(named: Array<{ cwd: string; name: string; root?: string; key?: string }>): void {
    for (const { cwd, name, root, key } of named) {
      if (name !== "" && this.name(cwd) !== name) void this.set(cwd, VAR_NAME, name);
      if (root !== undefined && root !== cwd && this.root(cwd) !== root) {
        void this.set(cwd, VAR_ROOT, root);
      }
      if (key !== undefined && key !== "" && this.key(cwd) !== key) {
        void this.set(cwd, VAR_KEY, key);
      }
    }
  }

  /**
   * A project's directory moved — a worktree that was relocated.
   *
   * **In place, keeping its position.** This is not `forget` plus `note`: the list is an order
   * somebody arranged by hand, and a project that left the bottom and came back at the top is a
   * reorder nobody asked for. The vars themselves have already travelled — the host moves the
   * project scope, which is where the pin and the fold live — so what is left is this list and the
   * cache in front of it.
   *
   * Written back only when something actually changed, because this runs on an event anybody may
   * raise and a `vars.set` per no-op is a disk write per no-op.
   *
   * A destination that is somehow already on the list collapses onto it rather than appearing
   * twice, and takes the earlier of the two positions: two rows for one directory is a panel where
   * the cursor lands on a project that is not the one it is pointing at.
   */
  async relocate(from: string, to: string): Promise<boolean> {
    if (from === to) return false;
    const at = this.known.indexOf(from);
    if (at < 0) return false;
    const already = this.known.indexOf(to);
    this.known[at] = to;
    // Both indices now name the destination. The later one goes, so the row keeps the earlier of
    // the two places in the order rather than jumping to wherever the arriving tree happened to be.
    if (already >= 0 && already !== at) this.known.splice(Math.max(at, already), 1);
    this.cache.set(to, await this.neosh.vars.all(projectScope(to)).catch(() => ({})));
    this.cache.delete(from);
    await this.neosh.vars.set({ scope: "global" }, VAR_KNOWN, this.known);
    return true;
  }

  /**
   * Take a project out of the list.
   *
   * The one thing that removes one, and the reason the list can be trusted to survive its
   * conversations. Only this panel's own vars go: another plugin's note about the directory is that
   * plugin's to keep, and a directory it still cares about comes back the next time it says so.
   */
  async forget(cwd: string): Promise<void> {
    this.known = this.known.filter((c) => c !== cwd);
    this.cache.delete(cwd);
    await this.neosh.vars.set({ scope: "global" }, VAR_KNOWN, this.known);
    await Promise.all(
      [VAR_FAVORITE, VAR_RANK, VAR_FOLDED, VAR_NAME, VAR_ROOT, VAR_KEY].map((key) =>
        this.neosh.vars.remove(projectScope(cwd), key).catch(() => {})
      ),
    );
  }

  isFavorite(cwd: string): boolean {
    return this.cache.get(cwd)?.[VAR_FAVORITE] === true;
  }

  isFolded(cwd: string): boolean {
    if (!isDirectory(cwd)) return this.away.has(cwd);
    return this.cache.get(cwd)?.[VAR_FOLDED] === true;
  }

  /** Where a project sorts. Anything you have never moved sorts after everything you have. */
  rank(cwd: string): number {
    const v = this.cache.get(cwd)?.[VAR_RANK];
    return typeof v === "number" && Number.isFinite(v) ? v : Number.MAX_SAFE_INTEGER;
  }

  async toggleFavorite(cwd: string): Promise<boolean> {
    const next = !this.isFavorite(cwd);
    await this.set(cwd, VAR_FAVORITE, next ? true : null);
    return next;
  }

  async toggleFold(cwd: string): Promise<void> {
    if (!isDirectory(cwd)) {
      // Straight through the set so the next frame is right without waiting for the event.
      if (this.away.has(cwd)) this.away.delete(cwd);
      else this.away.add(cwd);
      await this.neosh.vars.set({ scope: "global" }, VAR_FOLDED_AWAY, [...this.away]);
      return;
    }
    await this.set(cwd, VAR_FOLDED, this.isFolded(cwd) ? null : true);
  }

  /** Open a project without a keystroke — used when jumping to the conversation you are in. */
  unfold(cwd: string): void {
    if (!this.isFolded(cwd)) return;
    if (!isDirectory(cwd)) {
      void this.toggleFold(cwd);
      return;
    }
    void this.set(cwd, VAR_FOLDED, null);
  }

  /**
   * Move a project one place within its own group.
   *
   * Every rank in the group is written back, not just the two that swapped. Without that, the first
   * `K` on a project you have never moved would reorder against ranks that do not exist and jump
   * somewhere you did not ask for — the list has to mean what is on screen before it can be
   * rearranged.
   *
   * No wrapping: a reorder that teleports the thing you are dragging to the far end is a reorder
   * you undo.
   */
  async move(groups: Project[][], cwd: string, delta: number): Promise<boolean> {
    // A worktree moves among its siblings under the same repository; a project among its group.
    // In *paths* rather than in rows, because a repository's siblings now include checkouts on
    // other machines — which have no rank of their own to write, and must not take a place in the
    // order that a directory on this disk would then be ranked against.
    const under = groups
      .flat()
      .find((p) => p.worktrees.some((t) => !t.node && t.cwd === cwd))
      ?.worktrees.filter((t) => !t.node).map((t) => t.cwd);
    const group: string[] | undefined = under ??
      groups
        .find((g) => g.some((p) => p.cwd === cwd))
        ?.map((p) => p.cwd)
        .filter((c): c is string => c !== null);
    if (!group) return false;
    const from = group.indexOf(cwd);
    if (from < 0) return false;
    const to = from + delta;
    if (to < 0 || to >= group.length) return false;

    const moved = [...group];
    const [held] = moved.splice(from, 1);
    moved.splice(to, 0, held!);
    await Promise.all(moved.map((c, i) => this.set(c, VAR_RANK, i)));
    return true;
  }

  /** One write, straight through the cache so the next frame is right without waiting for the event. */
  private async set(cwd: string, key: string, value: unknown): Promise<void> {
    const entry = this.cache.get(cwd) ?? {};
    if (value === null) {
      delete entry[key];
      this.cache.set(cwd, entry);
      await this.neosh.vars.remove(projectScope(cwd), key);
      return;
    }
    entry[key] = value;
    this.cache.set(cwd, entry);
    await this.neosh.vars.set(projectScope(cwd), key, value);
  }
}

/**
 * Whether a key names a directory on this disk — an absolute path — rather than a row about another
 * machine (`host:<node>`, `machine:<node>:<key>`, `<node>:<path>`) or a repository's identity
 * (`git:github.com/…`, `dir:<name>`). Only the first kind may become a project var or a project.
 */
function isDirectory(key: string): boolean {
  return key.startsWith("/") || /^[A-Za-z]:[\\/]/.test(key);
}

function strings(v: unknown): string[] {
  return Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : [];
}

function unique(v: string[]): string[] {
  return [...new Set(v)];
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/**
 * One terminal's panel.
 *
 * Everything here is per view because all of it is navigation: which row the cursor is on, whether
 * the column is showing, how wide it is, a count half typed. The buffer is per view too, since the
 * cursor and the unfolded row are drawn into its text — two terminals reading one project list is
 * the same list rendered twice, and `j` in one must not move the other.
 */
interface Panel {
  readonly view: ViewId;
  /** The whole API, bound to this terminal. A window opened through it lands here. */
  readonly here: Neosh;
  readonly buf: BufferId;
  readonly list: CursoredList<Target>;
  /**
   * A count typed before a motion — `5j`, `12G`.
   *
   * State between two keystrokes, so it lives beside the panel rather than in the key table, and
   * it is drawn in the hint strip: a count you cannot see is a keypress that appears to have done
   * nothing at all.
   */
  readonly count: { pending: string };
  draw(): Promise<void>;
  open(): Promise<void>;
  close(): Promise<void>;
  enter(): Promise<void>;
  leave(): Promise<void>;
  isOpen(): boolean;
  /** Whether anything in this panel's list has a turn in flight, for the animation tick. */
  isRunning(): boolean;
  width(): number;
  setWidth(n: number): void;
  /** How many rows of panel there are, for the two keys that mean "a screen". */
  height(): Promise<number>;
  dispose(): void;
}

interface Wiring {
  neosh: Neosh;
  subscriptions: PluginContext["subscriptions"];
  arrangement: Arrangement;
  /**
   * The panel a key was pressed in.
   *
   * Every verb in this file starts here. A command run by name rather than by key names no
   * terminal, and then it is whichever one has the panel open — there is usually exactly one, and
   * "the one somebody is looking at" is the only answer that is ever right.
   */
  panel(view?: ViewId): Panel | null;
  /** Do something in every terminal's panel. */
  each(fn: (p: Panel) => unknown): void;
}

async function registerCommands(w: Wiring): Promise<void> {
  const { neosh, arrangement } = w;

  /** Rebuild the grouping the arrangement reorders against, without redrawing. */
  const groups = async (): Promise<Project[][]> => {
    const sessions = await neosh.session.list().catch(() => [] as SessionInfo[]);
    // No peers: this is only ever asked to work out what `J`/`K` reorder against, which is the
    // directories on this machine. Reading the swarm for it would be a round trip per keypress to
    // learn about rows that have no rank.
    return group(sessions, arrangement, []);
  };

  /**
   * Register one verb, and bind it inside this panel.
   *
   * Every key in this list goes through here, which is the point: there is no private key handler
   * any more, so `^Z` lists the panel's keys with the rest, `^K` runs them, and
   * `keymap.set("chat", "d", "…", { scope: { kind: "buf_kind", name: "neosh.sidebar" } })` from
   * anybody's `init.ts` replaces one. The scope is the buffer kind rather than the window, because
   * the window is opened and closed as you toggle the panel and a binding on it would go with it.
   *
   * `redraw` defaults to true because most verbs change what the column says. The ones that leave
   * the panel — or open a picker that redraws on the way back — pass false, since drawing into a
   * window that is losing focus is a frame of the wrong thing.
   */
  const verb = async (
    name: string,
    key: string | null,
    desc: string,
    fn: (p: Panel, target: Target | undefined, args: string[]) => Promise<void> | void,
    opts: { redraw?: boolean; on?: Partial<Record<Target["kind"], string>> } = {},
  ): Promise<void> => {
    // What the card beside the panel says for it, per kind of row it applies to. `desc` is the
    // sentence `^Z` prints for every row at once; this is the short form for the one row you are
    // on, which is a different sentence on a project than on a conversation.
    if (opts.on) ABOUT.set(name, opts.on);
    w.subscriptions.push(
      // The panel the key was pressed in. Every verb below acts on one terminal's column — its
      // cursor, its fold state, its half-typed count — and with several open, "the panel" is not
      // an answer. A command run by name rather than by key names no terminal and gets whichever
      // one is showing.
      await neosh.cmd.register(name, async (args, key) => {
        const p = w.panel(key?.view);
        if (p === null) return;
        await fn(p, p.list.value, args);
        // A count belongs to the motion typed straight after it. Anything else ends it — otherwise
        // a `5` you thought better of sits there and turns the next `j` into five.
        p.count.pending = "";
        if (opts.redraw !== false) await p.draw();
      }, { desc }),
    );
    if (key !== null) {
      await neosh.keymap.set("chat", key, name, {
        scope: { kind: "buf_kind", name: KIND },
        desc,
      });
    }
  };

  // ---- moving ----
  //
  // Vim's motions, because this is a cursor in a column in a terminal and that is the vocabulary
  // everybody's hands already have. A count works the way it does there — `5j` is five rows, `3G`
  // is the third — and it counts *rows you can land on*, so the headings and rules the cursor
  // already skips do not silently eat two of the five.
  const scope = { kind: "buf_kind", name: KIND } as const;
  const also = async (key: string, command: string, desc: string): Promise<void> => {
    await neosh.keymap.set("chat", key, command, { scope, desc });
  };

  /** Take the count that was typed in one panel, if one was, and forget it. */
  const take = (p: Panel, fallback = 1): number => {
    const n = Number.parseInt(p.count.pending, 10);
    p.count.pending = "";
    // Bounded, because the count is typed and `999999j` is a keystroke that would walk the list a
    // million times before drawing anything.
    return Number.isFinite(n) && n > 0 ? Math.min(n, 999) : fallback;
  };

  await verb(`${NS}.down`, "j", "Next row", (p) => void p.list.move(take(p)));
  await verb(`${NS}.up`, "k", "Previous row", (p) => void p.list.move(-take(p)));
  await also("<Down>", `${NS}.down`, "Next row");
  await also("<Up>", `${NS}.up`, "Previous row");
  await also("<C-n>", `${NS}.down`, "Next row");
  await also("<C-p>", `${NS}.up`, "Previous row");

  // A screen is however tall the panel turned out to be, which only the frontend knows — asked for
  // rather than assumed, the same way every other display measurement here is. Half a screen for
  // `^D`/`^U` because that is what it is everywhere, and a whole one loses the row you were on.
  const by = (fraction: number, sign: 1 | -1) => async (p: Panel): Promise<void> => {
    const rows = Math.max(2, await p.height());
    const step = Math.max(1, Math.floor(rows * fraction)) * take(p);
    // No wrapping on a page step: `^D` at the foot of the list means there is no more of it, and a
    // cursor that reappears at the top has thrown away the place you were reading from.
    p.list.move(sign * step, { wrap: false });
  };
  await verb(`${NS}.half.down`, "<C-d>", "Half a screen down", by(0.5, 1));
  await verb(`${NS}.half.up`, "<C-u>", "Half a screen up", by(0.5, -1));
  // A whole screen is `PgUp`/`PgDn` and deliberately *not* `^F`/`^B`. Those two are `^F` archive
  // and `^B` hide the panel — both of which somebody in this panel is more likely to want than a
  // page step in a column that is rarely taller than one screen, and a key that means something
  // else only while the cursor happens to be here is the worst kind of key.
  await verb(`${NS}.page.down`, "<PageDown>", "A screen down", by(1, 1));
  await verb(`${NS}.page.up`, "<PageUp>", "A screen up", by(1, -1));

  /** The count, when a verb wants the number itself rather than a repetition. */
  const typed = (p: Panel): number | null => {
    const n = Number.parseInt(p.count.pending, 10);
    p.count.pending = "";
    return Number.isFinite(n) && n > 0 ? n : null;
  };
  // With a count these are a *row* — `5gg` and `5G` are both the fifth — and without one they are
  // the two ends, exactly as in Vim.
  await verb(`${NS}.top`, "gg", "The first row, or the n-th with a count", (p) => {
    const n = typed(p);
    if (n === null) p.list.toEnd("first");
    else p.list.nth(n);
  });
  await verb(`${NS}.bottom`, "G", "The last row, or the n-th with a count", (p) => {
    const n = typed(p);
    if (n === null) p.list.toEnd("last");
    else p.list.nth(n);
  });

  /**
   * A digit, before a motion.
   *
   * One command for all ten, reading which digit it was off the key that ran it — so `^Z` lists it
   * once and `init.ts` can move it, rather than ten near-identical verbs. A leading `0` is not a
   * count anywhere and is left to whatever else might want the key.
   */
  w.subscriptions.push(
    await neosh.cmd.register(`${NS}.count`, async (_args, key) => {
      const p = w.panel(key?.view);
      const code = key?.key.code;
      if (p === null || code?.kind !== "char") return;
      if (p.count.pending === "" && code.c === "0") return;
      // Bounded while it is being typed, not only when it is read: three digits is more rows than
      // this panel will ever have, and it stops a leant-on key growing a string forever.
      if (p.count.pending.length < 3) p.count.pending += code.c;
      // Drawn straight away — the strip is the only thing saying the digit landed anywhere.
      await p.draw();
    }, { desc: "Begin a count for the next motion" }),
  );
  for (const digit of "0123456789") {
    await also(digit, `${NS}.count`, "Count for the next motion");
  }

  // ---- how wide the column is ----
  //
  // A resize rather than a reopen: reopening gives the window a new id and drops whatever had the
  // keyboard, so the panel would throw you back to the composer on every press. The width is the
  // *setting*, so it is the same number `config.toml` sets and one place decides it.
  const resize = (delta: number) => async (p: Panel): Promise<void> => {
    const now = (await neosh.opt.get<number>("sidebar.width")) ?? 34;
    const want = Math.max(16, Math.min(120, now + delta * take(p)));
    if (want !== now) await neosh.opt.set("sidebar.width", want);
  };
  await verb(`${NS}.wider`, ">", "Widen the panel", resize(2), { redraw: false });
  await verb(`${NS}.narrower`, "<lt>", "Narrow the panel", resize(-2), { redraw: false });
  await verb(`${NS}.width.reset`, "=", "Back to the default width", async () => {
    await neosh.opt.reset("sidebar.width");
  }, { redraw: false });

  // ---- leaving ----
  await verb(`${NS}.leave`, "<Esc>", "Back to the composer", (p) => p.leave(), { redraw: false });
  await neosh.keymap.set("chat", "q", `${NS}.leave`, { scope: { kind: "buf_kind", name: KIND } });
  await neosh.keymap.set("chat", "<C-c>", `${NS}.leave`, { scope: { kind: "buf_kind", name: KIND } });

  // ---- the row under the cursor ----
  await verb(
    `${NS}.select`,
    "<CR>",
    "Open a conversation, fold a project, or run the row",
    (p, target) => activateTarget(neosh, arrangement, target, p),
    {
      redraw: false,
      on: {
        session: "open",
        project: "fold or unfold",
        add: "add a project",
        custom: "run it",
        remote: "watch it",
        elsewhere: "fold or unfold",
      },
    },
  );
  await verb(`${NS}.fold`, "<Space>", "Fold or unfold this project", async (p, target) => {
    // A project that is only on other computers folds like any other. Its key is the project key,
    // because there is no directory on this disk to name it by.
    if (target?.kind === "elsewhere") {
      await arrangement.toggleFold(target.key);
      return;
    }
    if (target?.kind !== "project") return;
    await arrangement.toggleFold(target.cwd);
  }, { on: { project: "fold or unfold", elsewhere: "fold or unfold" } });
  await verb(`${NS}.favorite`, "f", "Pin this project to the top", async (p, target) => {
    let cwd = owningProject(target);
    if (cwd === null) return;
    // Pinning is about the top of the column, and a worktree is never at the top of the column —
    // `f` on one pins the repository it belongs to, the same way `f` on a conversation pins the
    // project it is in.
    const inside = (await neosh.session.list().catch(() => [] as SessionInfo[]))
      .find((s) => s.cwd === cwd && s.repo_root && s.repo_root !== s.cwd);
    if (inside?.repo_root) cwd = inside.repo_root;
    // No message. The row moves to the top of the column and gains a pin, in the panel the cursor
    // is already in — saying so in the corner as well is the same fact twice, and a corner that is
    // usually restating something visible is one people stop reading.
    await arrangement.toggleFavorite(cwd);
  }, { on: { project: "pin to the top", session: "pin its project to the top" } });

  // Shift moves the thing rather than the cursor — the one convention every list that can be
  // rearranged already shares.
  //
  // From a conversation row it moves the project that conversation is in. Requiring the cursor to
  // be on the heading first made the feature invisible: you are looking at the project when you are
  // looking at what is inside it.
  const reorder = (delta: number) => async (_p: Panel, target: Target | undefined) => {
    const cwd = owningProject(target);
    if (cwd === null) return;
    await arrangement.move(await groups(), cwd, delta);
  };
  // One line on the card for the pair — the same words on both, so they share it.
  const moving = { project: "move up or down", session: "move its project up or down" };
  await verb(`${NS}.move.down`, "J", "Move this project down", reorder(1), { on: moving });
  await verb(`${NS}.move.up`, "K", "Move this project up", reorder(-1), { on: moving });

  await verb(`${NS}.rename`, "r", "Rename this conversation", async (p, target) => {
    if (target?.kind !== "session") return;
    await renameSession(neosh, target.id);
  }, { on: { session: "rename" } });
  // The panel's own copy verb, same letter the transcript uses. What a row is *at* is the thing
  // you paste into a terminal, an editor or a message, and retyping a generated worktree path is
  // the kind of chore this panel exists to remove.
  await verb(`${NS}.copy.path`, "y", "Copy this row's directory", async (p, target) => {
    const cwd = owningProject(target);
    if (cwd === null) return;
    await neosh.edit.copy(cwd);
    neosh.notify(`copied ${cwd}`);
  }, { redraw: false, on: { project: "copy its directory", session: "copy its directory" } });
  // The everyday verb, and it is reversible. Archiving takes a conversation out of the list without
  // taking anything away, which is what people were reaching for `x` to do before `x` deleted
  // things. Where it goes is `a`, not four dim rows at the foot of this panel.
  await verb(`${NS}.archive`, "x", "Archive this conversation", async (p, target) => {
    if (target?.kind !== "session") return;
    await setArchived(neosh, target.id, true);
  }, { on: { session: "archive" } });
  // Shifted, because this is the one that cannot be undone.
  //
  // One key, two rows, and the same sentence: take this off the list for good. On a heading that is
  // the project — which is the verb that had been missing entirely, and the reason the panel used
  // to remove a project *for* you the moment you deleted the last thing in it.
  await verb(
    `${NS}.delete`,
    "X",
    "Delete this conversation, or remove this project",
    async (p, target) => {
      if (target?.kind === "project") {
        await removeProject(neosh, arrangement, target.cwd);
        return;
      }
      if (target?.kind !== "session") return;
      await deleteSession(neosh, target.id);
    },
    { on: { session: "delete", project: "remove from the list" } },
  );
  await verb(`${NS}.new`, "n", "New conversation in this project", async (p, target) => {
    // The same question `^N` asks, about the project you are looking at — which is the whole reason
    // the cursor is there. It used to create one outright, and that was the bug: `n` and `^N` were
    // one letter and a modifier apart and did visibly different things, so the only way to know
    // which one you wanted was to have already learned that they differ. Here is still the first
    // row, so `n ⏎` is what `n` always did.
    // A project that is only on other computers has no directory here to start in, so this asks
    // *that* machine — which is the one thing the row could always tell you and never do.
    if (target?.kind === "elsewhere") {
      await p.leave();
      await newRemoteConversation(neosh, target);
      return;
    }
    const cwd = owningProject(target) ?? undefined;
    await p.leave();
    await newConversation(neosh, arrangement, cwd);
  }, {
    redraw: false,
    on: {
      project: "new conversation here",
      session: "new conversation in this project",
      elsewhere: "new conversation over there",
    },
  });

  // ---- doors out of the panel ----
  //
  // `a` is not here. What you have archived is the `archive` plugin's panel, and the key on these
  // rows that opens it is an `archive.action` contribution — which is this panel's own extension
  // point, used by somebody else, rather than a door drawn in by hand. Turn that plugin off and the
  // key goes with it, instead of pointing at a command that is no longer there.
  await verb(`${NS}.shell`, "t", "Open a terminal here", async (p, target) => {
    // The same key on every kind of row, and the row decides where the shell lands: a directory on
    // this disk, or a directory on the machine the row is about. That is the whole claim of this
    // panel now — a project on the build box is not a different kind of thing, it is the same work
    // in a different place — and a verb that worked on one and not the other would be the panel
    // saying otherwise on the one key where it matters most. What you want a terminal *for* is to
    // run something beside an agent that has just finished, and where that agent is running is
    // exactly where the terminal has to be.
    const there = machineFor(target);
    await p.leave();
    if (there) {
      // A machine's heading names no directory, which is its home — where `ssh box` lands too.
      await neosh.swarm
        .shell(there.id, there.cwd === "" ? undefined : there.cwd)
        .catch((e: unknown) => neosh.notify(String(e), "warn"));
      return;
    }
    const cwd = owningProject(target);
    if (cwd === null) return;
    // The host's own verb, given a directory. `<C-w>T` opens one in the conversation you are in;
    // this one is about the row, which is very often not that.
    await neosh.cmd.exec("tab.new.term", [cwd]).catch((e: unknown) => {
      neosh.notify(String(e), "warn");
    });
  }, {
    redraw: false,
    on: {
      project: "terminal here",
      session: "terminal in this project",
      elsewhere: "terminal on that computer",
      remote: "terminal on that computer",
    },
  });
  await verb(`${NS}.connect`, "c", "Connect to this computer", async (p, target) => {
    // On the row rather than only in `^J`, because the row is where you find out: a project greyed
    // with a dim `@` is the moment you want to reconnect, and going somewhere else to do it means
    // first knowing that somewhere else exists. `^J` is still the list of machines and still where
    // one is added or removed — this is the two verbs that are about a machine you can already see.
    const there = machineFor(target);
    if (!there) return;
    await neosh.swarm.reconnect(there.id).catch((e: unknown) => neosh.notify(String(e), "warn"));
    neosh.notify(`dialling ${there.name}\u2026`);
    await p.draw();
  }, {
    redraw: false,
    on: { elsewhere: "connect to that computer", remote: "connect to that computer" },
  });
  await verb(`${NS}.disconnect`, "C", "Disconnect from this computer", async (p, target) => {
    const there = machineFor(target);
    if (!there) return;
    await neosh.swarm.disconnect(there.id).catch((e: unknown) => neosh.notify(String(e), "warn"));
    // What takes it back, said with the message: a verb whose undo is one letter away should say
    // so at the moment somebody might want it, not in a sheet they would have to go and open.
    neosh.notify(`disconnected ${there.name} \u2014 c takes it back`);
    await p.draw();
  }, {
    redraw: false,
    on: { elsewhere: "disconnect that computer", remote: "disconnect that computer" },
  });
  // `@`, because it is the character the code is drawn after — the key is the mark it changes.
  await verb(`${NS}.machine.code`, "@", "Choose this computer's short code", async (p, target) => {
    const there = machineFor(target);
    if (!there) return;
    const [nodes, chosen] = await Promise.all([
      neosh.swarm.nodes().catch(() => [] as SwarmNode[]),
      neosh.vars.get({ scope: "global" }, VAR_CODES).catch(() => null),
    ]);
    await chooseCode(neosh, there.id, there.name, machineCodes(nodes, chosen).get(there.id));
  }, {
    on: { elsewhere: "that computer's @code", remote: "that computer's @code" },
  });
  // The workspace's settings, from the panel that is about the workspace. `,` because it is the
  // key every Mac application opens its settings with (`⌘,`), without the `⌘` a terminal never
  // receives — and there is no Ctrl-letter left to give it. `/settings` and `^K` reach it too.
  await verb(`${NS}.settings`, ",", "Settings", async (p) => {
    await p.leave();
    await neosh.cmd.exec("settings.open").catch(() =>
      neosh.notify("the settings plugin is not loaded — see `plugins.disabled`", "warn")
    );
  }, { redraw: false });
  await verb(`${NS}.add`, "o", "Add a project", async (p) => {
    await p.leave();
    await neosh.cmd.exec("project.open").catch(() => {});
  }, { redraw: false });
  await verb(`${NS}.help`, "?", "The keys for this row", async (p) => {
    await neosh.cmd.exec("help.keys").catch(() => {});
  }, { redraw: false });

  /**
   * Where the keys nothing claimed go: nowhere.
   *
   * Not a handler — a sink. Without it an unbound letter in this panel falls through as unhandled
   * and the chat frontend types it into the composer, so pressing `z` here would silently start
   * writing a message. Every key that *does* something is a binding above; this is only what stops
   * the rest from doing something somewhere else.
   */
  w.subscriptions.push(
    await neosh.cmd.register(`${NS}.key`, () => {}, { desc: "Swallow an unbound key in the panel" }),
  );

  w.subscriptions.push(
    await neosh.cmd.register("sidebar.toggle", async (_args, key) => {
      // Toggling visibility, not focus: `^T` is how you get in. In *this* terminal: `^B` is about
      // the column in front of you and says nothing about anybody else's.
      const p = w.panel(key?.view);
      if (p === null) return;
      if (p.isOpen()) await p.close();
      else await p.open();
    }, { desc: "Show or hide the sidebar" }),
  );
  w.subscriptions.push(
    await neosh.cmd.register("sidebar.focus", async (_args, key) => {
      await w.panel(key?.view)?.enter();
    }, { desc: "Move into the project list" }),
  );
  w.subscriptions.push(
    await neosh.cmd.register("sidebar.refresh", async (_args, key) => {
      // By name and from no key, this means "the panel is stale" rather than "this one is", so it
      // redraws every terminal's.
      if (key) await w.panel(key.view)?.draw();
      else w.each((p) => p.draw());
    }, { desc: "Redraw the sidebar now" }),
  );
  // The two questions a plugin on top of this panel asks, as commands so `cmd.call` answers them
  // without a dependency. What a key press passes as arguments, a call gets as the answer.
  w.subscriptions.push(
    await neosh.cmd.register("sidebar.cursor", (_args, key) => {
      // The terminal the call came from, when it came from one; the served panel's otherwise.
      return w.panel(key?.view)?.list.value ?? null;
    }, { desc: "The row under the sidebar's cursor" }),
  );
  w.subscriptions.push(
    await neosh.cmd.register("sidebar.rows", (_args, key) => {
      return w.panel(key?.view)?.list.values ?? [];
    }, { desc: "Every row in the sidebar you can land on" }),
  );
  w.subscriptions.push(
    await neosh.cmd.register(
      "session.new",
      (args) => newConversation(neosh, arrangement, args[0]),
      { desc: "Start a new conversation — here, or in a worktree of its own" },
    ),
  );
  w.subscriptions.push(
    await neosh.cmd.register("session.new.here", async (p) => {
      await neosh.session.create();
    }, { desc: "Start a new conversation in this project, without asking where" }),
  );
  w.subscriptions.push(
    await neosh.cmd.register("session.copy.path", async (p) => {
      // The conversation's directory — which in a worktree is the worktree, and that is the point:
      // the path you want on the clipboard is the one your shell should cd to.
      const current = await neosh.session.current().catch(() => null);
      if (!current) return;
      await neosh.edit.copy(current.cwd);
      neosh.notify(`copied ${current.cwd}`);
    }, { desc: "Copy this conversation's directory to the clipboard" }),
  );
  // An Alt chord, which the default-binding rule keeps out of the defaults — on a Mac out of the
  // box `⌥Y` is `¥`, a key neosh never receives — with the one exception it makes for arrows: bound where
  // it means something, and never the only way. Chat mode has no Ctrl chord left to give this, and
  // every terminal-sendable route exists beside it — `yp` in the reader, `y` on any row of this
  // panel, `^K` and `/copy` by name — so a terminal that sends Alt gets a key in the composer and
  // one that does not has lost nothing.
  await neosh.keymap.set("chat", "<A-y>", "session.copy.path", {
    desc: "Copy this conversation's directory",
  });
  w.subscriptions.push(
    await neosh.cmd.register("session.close", async (args) => {
      // Through the same gate as the key, so the palette cannot become a way around the question.
      const id = args[0] ?? (await neosh.session.current().catch(() => null))?.id;
      if (id) await deleteSession(neosh, id);
    }, { desc: "Delete a conversation permanently" }),
  );
  w.subscriptions.push(
    await neosh.cmd.register("session.archive", async (args) => {
      const id = args[0] ?? (await neosh.session.current().catch(() => null))?.id;
      if (id) await setArchived(neosh, id, true);
    }, { desc: "Archive a conversation — it keeps everything and can come back" }),
  );
  w.subscriptions.push(
    await neosh.cmd.register("session.unarchive", async (args) => {
      const id = args[0];
      if (!id) {
        neosh.notify("session.unarchive needs a conversation id", "warn");
        return;
      }
      await setArchived(neosh, id, false);
    }, { desc: "Bring an archived conversation back" }),
  );
  w.subscriptions.push(
    await neosh.cmd.register("project.open", async (args) => {
      // Without this, a second project only ever appears by way of a worktree — which makes the
      // whole panel a thread list with extra steps.
      const path = args[0] ?? (await chooseDirectory(neosh));
      if (path === null || path.trim() === "") return;
      try {
        await neosh.session.create({ cwd: path.trim() });
      } catch (e) {
        // Almost always a path that is not there. The host's message names it, which is more
        // useful than anything this plugin could invent.
        neosh.notify(String(e), "warn");
      }
    }, { desc: "Add a project — start a conversation in another directory" }),
  );
  w.subscriptions.push(
    await neosh.cmd.register("project.remove", async (args, key) => {
      // The row under the cursor when there is one, so the palette entry does something useful from
      // inside the panel and a plugin can still name a directory outright.
      const here = w.panel(key?.view);
      const cwd = args[0]?.trim() || owningProject(here?.list.value);
      if (!cwd) {
        neosh.notify("project.remove needs a directory", "warn");
        return;
      }
      await removeProject(neosh, arrangement, cwd);
      // Every terminal's, because a project going away is a row missing from all of them.
      w.each((p) => p.draw());
    }, { desc: "Take a project off the list" }),
  );
  w.subscriptions.push(
    await neosh.cmd.register("project.forget", async (args) => {
      // The row, and only the row: no dialog and nothing deleted. For a directory that has already
      // gone — a worktree the git plugin or the archive just took off the disk — whose row would
      // otherwise sit here saying `nothing here yet` about a place that is not there. A project
      // with conversations still in it is refused, because forgetting it would not empty it and a
      // caller that wanted them deleted has `project.remove`.
      const cwd = args[0]?.trim();
      if (!cwd) {
        neosh.notify("project.forget needs a directory", "warn");
        return;
      }
      const inside = (await neosh.session.list({ includeArchived: true }).catch(() => []))
        .some((s) => s.cwd === cwd);
      if (inside) {
        neosh.notify(`${short(cwd)} still has conversations in it — \`project.remove\` deletes them`, "warn");
        return;
      }
      await arrangement.forget(cwd);
      w.each((p) => p.draw());
    }, { desc: "Drop a project's row without deleting anything — for a directory that has gone" }),
  );

  await neosh.keymap.set("chat", "<C-b>", "sidebar.toggle", { desc: "Toggle the sidebar" });
  await neosh.keymap.set("chat", "<C-t>", "sidebar.focus", { desc: "Projects and conversations" });
  await neosh.keymap.set("chat", "<C-n>", "session.new", { desc: "New conversation" });
  await neosh.keymap.set("chat", "<C-o>", "project.open", { desc: "Add a project" });

  // Watching a conversation on another machine.
  //
  // Not "switching" — there is nothing here to switch to, because the agent belongs to the computer
  // it was started on and always will. Subscribing is what makes it read like a local one: history
  // first, then everything as it happens.
  w.subscriptions.push(
    await neosh.cmd.register("swarm.open", async (args, key, here) => {
      const [node, session] = args;
      if (!node || !session) {
        neosh.notify("swarm.open needs a node and a conversation", "warn");
        return;
      }
      // Into the terminal the key was pressed in, and the panel lets go of the keyboard if it had
      // it: somebody who opened a conversation is about to type into it.
      if (!(await openRemote(here ?? neosh, node, session))) return;
      await w.panel(key?.view)?.leave();
    }, { desc: "Open a conversation on another computer" }),
  );
  w.subscriptions.push(
    await neosh.cmd.register("swarm.nodes", () => showNodes(neosh), {
      desc: "The computers in this workspace",
    }),
  );
  w.subscriptions.push(
    await neosh.cmd.register("swarm.add", () => addComputer(neosh), {
      desc: "Add a computer by its address",
    }),
  );
  // The short codes, as an answer for anybody else who names a machine — a settings page, a status
  // segment — so every panel says `@ms` about the same computer. `{ "<node id>": "ms" }`.
  w.subscriptions.push(
    await neosh.cmd.register("swarm.codes", async () => {
      const [nodes, chosen] = await Promise.all([
        neosh.swarm.nodes().catch(() => [] as SwarmNode[]),
        neosh.vars.get({ scope: "global" }, VAR_CODES).catch(() => null),
      ]);
      const me = (await neosh.swarm.self().catch(() => null))?.id;
      return Object.fromEntries(machineCodes(nodes.filter((n) => n.info.id !== me), chosen));
    }, { desc: "Each other computer's short code, by id" }),
  );
  // And their colours, as the palette group each is drawn in: `{ "<node id>": "Swarm.Host2" }`.
  w.subscriptions.push(
    await neosh.cmd.register("swarm.colors", async () => {
      const nodes = await neosh.swarm.nodes().catch(() => [] as SwarmNode[]);
      const me = (await neosh.swarm.self().catch(() => null))?.id;
      return Object.fromEntries(machineColors(nodes.filter((n) => n.info.id !== me)));
    }, { desc: "Each other computer's colour, as a highlight group, by id" }),
  );
  w.subscriptions.push(
    await neosh.cmd.register("swarm.code", async (args, key) => {
      // A machine named outright, or the one under the cursor in the panel the key came from.
      const [nodes, chosen] = await Promise.all([
        neosh.swarm.nodes().catch(() => [] as SwarmNode[]),
        neosh.vars.get({ scope: "global" }, VAR_CODES).catch(() => null),
      ]);
      const id = args[0] ?? machineFor(w.panel(key?.view)?.list.value)?.id;
      const node = nodes.find((n) => n.info.id === id || n.info.name === id);
      if (!node) {
        neosh.notify("swarm.code needs a computer", "warn");
        return false;
      }
      return await chooseCode(neosh, node.info.id, node.info.name, machineCodes(nodes, chosen).get(node.info.id));
    }, { desc: "Choose the short code a computer is drawn with" }),
  );
  w.subscriptions.push(
    // The machine-level verb, so a script or `^K` reaches it without going through a row. `t` in
    // the panel is this with the cursor's machine and directory filled in, which is the ordinary
    // way to press it — a name and a path is what you use when you are automating something.
    await neosh.cmd.register("swarm.shell", async (args) => {
      const [node, cwd] = args;
      if (!node) {
        neosh.notify("swarm.shell needs a computer", "warn");
        return;
      }
      await neosh.swarm
        .shell(node, cwd === "" ? undefined : cwd)
        .catch((e: unknown) => neosh.notify(String(e), "warn"));
    }, { desc: "Open a terminal on another computer" }),
  );
  // `^J` rather than a letter, because everything in chat mode has to be a chord — the composer is
  // the field, and a bare key would be a character you can no longer type.
  await neosh.keymap.set("chat", "<C-j>", "swarm.nodes", { desc: "Computers" });
  // Any change over there is a redraw here. Without it the panel is right only as often as its
  // four-second refresh, which is a long time to watch a spinner that has already stopped.
  w.subscriptions.push(neosh.swarm.onChange(() => w.each((p) => p.draw())));

  await neosh.hint.set("sessions", { keys: "^T", label: "conversations", priority: 20 });
  await neosh.hint.set("new", { keys: "^N", label: "new", priority: 21 });
}

/**
 * Bind the verbs other plugins contributed, and rebind them when the set changes.
 *
 * The panel binds these rather than the contributor doing it, and that is deliberate: a key press
 * carries no arguments, so a plugin that bound `d` itself would have no way to learn which row the
 * cursor was on except by tracking every move — one race away from acting on the wrong
 * conversation. Going through here, the command is invoked with the row as arguments and the
 * contributor never has to know the panel has a cursor at all.
 *
 * They are real bindings all the same. `^Z` lists them with the contributed label, `^K` runs the
 * command, and a user who dislikes the key rebinds it exactly as they would one of ours.
 */
function installActions(
  neosh: Neosh,
  panel: (view?: ViewId) => Panel | null,
  onChange: () => void,
): { actions: () => BoundAction[]; dispose: Disposable } {
  const scope = { kind: "buf_kind", name: KIND } as const;
  let current: Array<Contribution & { item: ActionItem }> = [];
  let bound: Array<{ key: string; command: string }> = [];
  let registered: Disposable[] = [];

  const sync = async () => {
    const got = await neosh.ext.list<ActionItem>(POINT_ACTION).catch(() => []);
    const valid = got.filter((c) =>
      typeof c.item?.key === "string" && typeof c.item?.command === "string"
    );

    // Everything from the previous round goes first. An action that was withdrawn must take its key
    // *and* its command with it, or the panel keeps a binding pointing at a wrapper around a
    // command whose plugin has gone.
    for (const b of bound) await neosh.keymap.del("chat", b.key, scope).catch(() => {});
    for (const d of registered) d.dispose();
    bound = [];
    registered = [];
    const reserved = await reservedKeys(neosh);

    // Every action still gets a command of its own — `^K` runs it by name whether or not it won a
    // key — but the *binding* is per key, not per action. Two plugins may each want `<Tab>` on
    // their own rows and both are right; one `keymap.set` per action made that the second one
    // overwriting the first, so whichever loaded last owned the key everywhere and the other
    // plugin's rows had a key that did nothing. One binding, and the row under the cursor decides
    // which of them it means.
    const sharing = new Map<string, Array<Contribution & { item: ActionItem }>>();
    for (const c of valid) {
      const name = `${NS}.action.${c.plugin}.${c.id}`;
      const command = await neosh.cmd.register(name, async (_args, key) => {
        // The row under *this* terminal's cursor. A contributed verb is pressed in one panel, and
        // there may be three of them open on three different rows.
        const target = panel(key?.view)?.list.value;
        if (!applies(c.item.on ?? "any", target)) return;
        await neosh.cmd.exec(c.item.command, argsFor(target)).catch((e: unknown) => {
          neosh.notify(String(e), "warn");
        });
        onChange();
      }, { desc: c.item.label }).catch(() => null);
      if (command) registered.push(command);
      // A contributed key does not get to take one of ours. `keymap.set` already refuses to let a
      // *bundled* default overwrite somebody's choice, but here the bundled plugin is the one doing
      // the binding on a third party's behalf, so that rule points the wrong way and the check is
      // ours to make. The command stays registered either way — `^K` still runs it, and the
      // contributor is told which key it did not get.
      if (reserved.has(c.item.key)) {
        neosh.log.warn(
          `${c.plugin} asked for '${c.item.key}' in the sidebar, which is already a panel key`,
        );
        continue;
      }
      const share = sharing.get(c.item.key);
      if (share) share.push(c);
      else sharing.set(c.item.key, [c]);
    }

    for (const [key, claimants] of sharing) {
      // The narrow verbs first: a verb about the row you are standing on beats one about every row,
      // which is the same ranking the key card prints them in and for the same reason. A key that
      // meant "cycle the plan detail" wherever the cursor was would be a key that did the wrong
      // thing on somebody else's block.
      const ranked = [
        ...claimants.filter((c) => (c.item.on ?? "any") !== "any"),
        ...claimants.filter((c) => (c.item.on ?? "any") === "any"),
      ];
      const name = ranked.length === 1 && ranked[0]
        ? `${NS}.action.${ranked[0].plugin}.${ranked[0].id}`
        : `${NS}.action.key.${key}`;
      if (ranked.length > 1) {
        const command = await neosh.cmd.register(name, async (_args, k) => {
          const target = panel(k?.view)?.list.value;
          const hit = ranked.find((c) => applies(c.item.on ?? "any", target));
          if (!hit) return;
          await neosh.cmd.exec(hit.item.command, argsFor(target)).catch((e: unknown) => {
            neosh.notify(String(e), "warn");
          });
          onChange();
        }, { desc: ranked.map((c) => c.item.label).join(" / ") }).catch(() => null);
        if (command) registered.push(command);
      }
      const label = ranked[0]?.item.label ?? key;
      await neosh.keymap.set("chat", key, name, { scope, desc: label }).catch(() => {});
      bound.push({ key, command: name });
    }
    current = valid;
    onChange();
  };

  void sync();
  const sub = neosh.ext.onChange((e) => {
    if (e.point === POINT_ACTION) void sync();
  });

  return {
    // With the command each one's key was bound under, which is how the key card finds the key
    // in the registry rather than trusting the key the contributor asked for: a user who moved it
    // in `init.ts` has the new letter on the card. A key that was refused has no binding and no
    // command, and the card leaves it out for the reason the strip did.
    actions: () =>
      current.map((c) => ({
        ...c.item,
        plugin: c.plugin,
        binding: bound.find((b) => b.key === c.item.key)?.command ?? null,
      })),
    dispose: {
      dispose() {
        sub.dispose();
        for (const b of bound) void neosh.keymap.del("chat", b.key, scope).catch(() => {});
        for (const d of registered) d.dispose();
      },
    },
  };
}

/**
 * The keys this panel has already spoken for. A contribution asking for one of these is a bug in it.
 *
 * Read off the registry rather than kept in a list here: the list this replaced was missing
 * `gg`, `G`, `^D`, `^U`, `⇥` and every digit — keys the panel binds a few hundred lines up — so a
 * contributed `G` silently replaced *go to the bottom* with no warning to anybody. A fact about
 * which keys are bound is a fact the keymap table already has.
 */
async function reservedKeys(neosh: Neosh): Promise<Set<string>> {
  const all = await neosh.keymap.list("chat").catch(() => []);
  return new Set(
    all
      .filter((k) =>
        k.scope.kind === "buf_kind" && k.scope.name === KIND &&
        !k.command.startsWith(`${NS}.action.`)
      )
      .map((k) => k.lhs),
  );
}

/**
 * Whether a contributed verb is about the row under the cursor.
 *
 * `custom:<section id>` is the narrow form and the one most contributors want: a verb about *my*
 * block rather than about every block anybody has put in this column. Bare `custom` is still every
 * contributed row, because a verb about contributions in general is a real thing to want — but it
 * is not what "a key on my rows" means, and until this existed it was the only spelling available.
 * Two plugins each asking for `<Tab>` on their own rows were one binding, one winner and one plugin
 * whose key quietly did nothing.
 */
function applies(on: string, target: Target | undefined): boolean {
  if (on === "any") return target !== undefined;
  if (on.startsWith("custom:")) {
    return target?.kind === "custom" && target.section === on.slice("custom:".length);
  }
  return target?.kind === on;
}

/** The row under the cursor, as arguments a command can act on. */
function argsFor(target: Target | undefined): string[] {
  if (!target) return [];
  if (target.kind === "session") return ["session", target.cwd, target.id];
  if (target.kind === "project") return ["project", target.cwd];
  // Which block it came from, so a verb bound across several sections can tell them apart without
  // the panel having to hand over the row's own command as well.
  if (target.kind === "custom") return ["custom", target.section ?? ""];
  return [target.kind];
}

/**
 * Start a conversation, asking where when there is something to ask.
 *
 * A conversation belongs to a directory — that is what makes the sidebar a list of projects rather
 * than a list of threads — and in a git repository "where" has a real answer that is not always
 * "here". A branch you want to work on without disturbing what is checked out is a *worktree*, and
 * having to create one, find it, and then open a conversation in it is three steps for one
 * intention.
 *
 * **`base` is which project is being asked about**, and it is the whole reason this takes an
 * argument: `^N` means the conversation you are in, and `n` in the panel means the row the cursor
 * is on, which is very often a different repository with different branches. Asking about the
 * active conversation's checkout from a row pointing somewhere else would offer worktrees that
 * have nothing to do with what was aimed at.
 *
 * Outside a repository the question has one answer, so it is not asked: `^N` stays a single key
 * everywhere the choice would be theatre. Inside one, `Here` is selected, so `^N ⏎` is what `^N`
 * always did.
 *
 * **The worktrees it offers are the ones on the panel's list**, which is what `arrangement` is for
 * here. Every tree `git worktree list` knows is not the same set: a scratch branch you finished
 * with in March is still a checkout on disk long after `X` took it off the list, so this picker
 * was offering back, under "an existing one", every project you had ever removed — twenty rows of
 * finished work above the three you are actually in, and choosing one put it straight back in the
 * sidebar. The list is the workspace's answer to *which places do I work in*, and
 * this question is asking exactly that. A worktree neosh has never heard of is not on it either,
 * which is correct rather than a casualty: `Another directory…` lists every tree git knows and is
 * how a directory joins the list in the first place.
 */
/**
 * Which computer, before where on it.
 *
 * Asked only when there is something to ask: with no machine paired — which is most workspaces —
 * this returns `here` without drawing anything, and `^N` is the single key it has always been. The
 * moment a second computer exists the question stops being theatre and starts being the first
 * thing you would have to decide anyway, and asking it *first* is what makes it one decision
 * rather than a path field you have to know the scp spelling of.
 *
 * This computer is the first row and the cursor starts on it, so `^N ⏎` is still what `^N` always
 * did. Every paired machine is a row, including the ones that cannot be used — the rule the path
 * field already follows: skipping them is right about what can be done and wrong about what should
 * be said, and somebody who has just paired a computer and finds it missing reads that as the
 * feature not existing.
 */
async function chooseMachine(
  neosh: Neosh,
  title: string,
  about?: { key: string | null },
): Promise<{ kind: "here" } | { kind: "node"; node: SwarmNode } | null> {
  const me = await neosh.swarm.self().catch(() => null);
  const all = (await neosh.swarm.nodes().catch(() => [] as SwarmNode[]))
    .filter((n) => n.info.id !== me?.id);
  // Only the machines this question could actually be answered with. A row for a computer that is
  // down is right in a *list* — `^J` draws every one of them, because a machine going quiet is
  // something you need told — and wrong in a question, where it is an option that cannot be taken.
  const peers = all.filter((n) => n.up && n.capabilities.accepts_commands);
  // And, when the question is about a particular project, only the machines that have it.
  //
  // Which is very often none of them, and then there is no question: `n` on a repository nobody
  // else has a checkout of has exactly one answer, and asking it is a keypress and a panel spent
  // confirming something the workspace already knew. It is the same rule as not asking at all with
  // no machine paired, one step finer — the first cut asked the moment a second computer existed,
  // which on a swarm of two made every `^N` two keys instead of one for the whole time the other
  // machine had nothing to do with what you were working on.
  // Only on a key that is a *fact*. `git:` is the normalised origin remote and means the same
  // thing on every machine; `dir:` is a fallback from a directory name, and it is what a repository
  // answers with for the first moment of a workspace's life — before the host has read its origin
  // — as well as what a directory that is not a checkout answers with for ever. Filtering on one
  // compares a guess against other machines' facts, matches nothing, and silently withholds the
  // question. The asymmetry decides it: a question asked needlessly costs a keypress, and one
  // silently not asked costs the capability. `ProjectKey::is_certain` is the same distinction on
  // the Rust side, drawn for the same reason.
  const certain = about?.key?.startsWith("git:") ? about.key : null;
  // Both of the things a machine tells you it has, because they arrive by different roads and at
  // different times. `capabilities.projects` is the places it *works in* and rides on `Presence`,
  // which is a heartbeat; the conversations ride on `Inventory`, which is sent the moment a link
  // comes up. So for the first heartbeat of every connection a peer can be visibly running a turn
  // in this very repository while its project list still says nothing about it — and asking only
  // the list meant the question was withheld from the machine most obviously able to answer it.
  //
  // A conversation open in a directory is also the stronger evidence of the two: the list is what a
  // machine says it works in, and a conversation is it working there.
  const has = (n: SwarmNode, key: string) =>
    n.capabilities.projects.some((p) => p.key === key) ||
    n.agents.some((a) => a.project === key);
  const usable = certain ? peers.filter((n) => has(n, certain)) : peers;
  if (usable.length === 0) return { kind: "here" };
  const ascii = (await neosh.opt.get<boolean>("ui.ascii_only").catch(() => false)) ?? false;

  type Row = { kind: "here" } | { kind: "node"; node: SwarmNode };
  const rows: PickerItem<Row>[] = [{
    label: "This computer",
    detail: me?.name ?? "here",
    keywords: "here local this machine",
    icon: ascii ? "*" : "\u25cf",
    hl: "Diagnostic.Ok",
    value: { kind: "here" },
  }];
  // Every paired machine is still a row, including the ones that cannot be used, greyed with the
  // reason on them: skipping them is right about what can be *done* and wrong about what should be
  // *said*, and somebody who has just paired a computer and finds it missing reads that as the
  // feature not existing. What `usable` decided is only whether to ask at all.
  for (const n of all) {
    const mark = marker(n.link);
    const can = n.up && n.capabilities.accepts_commands;
    const why = !n.up
      ? linkWords(n.link)
      : !n.capabilities.accepts_commands
        ? "read-only \u2014 it does not accept commands"
        : `${n.agents.length} ${n.agents.length === 1 ? "conversation" : "conversations"}`;
    rows.push({
      label: n.info.name,
      detail: [why, n.info.os].filter(Boolean).join("  \u00b7  "),
      keywords: `${n.info.id} ${n.info.os} computer machine remote host`,
      icon: mark.text,
      // The row's own colour says whether it can be used; the marker says what the link is doing.
      hl: can ? mark.hl : "Sidebar.Dim",
      value: { kind: "node", node: n },
    });
  }
  const chosen = await picker<Row>(neosh, rows, { title, width: 72, height: 12 });
  if (chosen === null) return null;
  if (chosen.kind === "here") return { kind: "here" };
  if (!chosen.node.up) {
    neosh.notify(`${chosen.node.info.name}: ${linkWords(chosen.node.link)}`, "info");
    return null;
  }
  if (!chosen.node.capabilities.accepts_commands) {
    neosh.notify(`${chosen.node.info.name} does not accept commands from here`, "info");
    return null;
  }
  return chosen;
}

/**
 * Start a conversation on a project that is only on other computers.
 *
 * The row already knows which machines have it and where on each, so with one there is nothing to
 * ask and with several the question is which computer — never a path, which is the thing nobody
 * standing at this keyboard can answer about somebody else's disk.
 */
async function newRemoteConversation(
  neosh: Neosh,
  target: { name: string; machines: Array<{ id: string; name: string; cwd: string }> },
): Promise<void> {
  const machines = target.machines;
  let pick = machines[0];
  if (machines.length > 1) {
    const chosen = await picker(
      neosh,
      machines.map((m) => ({
        label: m.name,
        detail: m.cwd,
        keywords: m.id,
        icon: "\u2601",
        hl: "Swarm.Up",
        value: m,
      })),
      { title: `${target.name} \u2014 which computer?`, width: 72 },
    );
    if (!chosen) return;
    pick = chosen;
  }
  if (!pick) return;
  if (pick.cwd === "") {
    // A machine rather than a directory on it — its section heading. Where on it is the question,
    // asked the way `^N` asks it once a machine is chosen: the path field, pointed over there.
    const prefix = /[:\s]/.test(pick.name) ? pick.id.slice(0, 8) : pick.name;
    const chosen = await whereTo(neosh, [], {
      title: `New conversation on ${pick.name}`,
      seed: `${prefix}:`,
    });
    if (chosen?.kind === "host") await startThere(neosh, chosen);
    return;
  }
  await startThere(neosh, {
    node: pick.id,
    cwd: pick.cwd,
    label: `${pick.cwd} on ${pick.name}`,
  });
}

async function newConversation(
  neosh: Neosh,
  arrangement: Arrangement,
  base?: string,
): Promise<void> {
  const current = await neosh.session.current().catch(() => null);
  // In another machine's conversation, *here* is a directory on that machine — so the question is
  // the one `^N` asks after choosing it, the path field pointed over there, and it starts on the
  // directory you are in: `^N ↵` is a new conversation beside this one, as it is on your own.
  if (!base && current?.mirror) {
    const there = (await neosh.swarm.nodes().catch(() => [] as SwarmNode[]))
      .find((n) => n.info.id === current.mirror?.node);
    if (!there?.up) {
      neosh.notify(`${current.mirror.machine} is not connected — c on its row in ^T reconnects`, "warn");
      return;
    }
    const chosen = await whereTo(neosh, [], {
      title: `New conversation on ${there.info.name}`,
      seed: `${hostPrefix(there)}:${current.cwd}`,
    });
    if (chosen?.kind === "host") await startThere(neosh, chosen);
    return;
  }
  const here = base ?? current?.cwd;
  // Which computer, first — and only when there is one to ask about *and* it could answer. A
  // repository none of the connected machines has a checkout of has one possible answer, and
  // asking is a panel spent confirming it. See {@link chooseMachine}.
  //
  // The key comes from a **live conversation** rather than from what the panel wrote down. The var
  // is filled by the draw, and the draw's own source is `SessionInfo::project_key`, which the host
  // fills once it has read the repository's origin — so for the first moment of a workspace's life
  // the written-down answer is the fallback guess, `dir:<name>`. Read from there, `^N` pressed in
  // that window compared a guess against the real keys other machines advertise, matched nothing,
  // and silently skipped a question it should have asked. The var is still the fallback, because it
  // is the only answer for a project with nothing open in it.
  const machine = await chooseMachine(neosh, "New conversation \u2014 where?", {
    key: here ? await projectKey(neosh, arrangement, here) : null,
  });
  if (machine === null) return;
  if (machine.kind === "node") {
    // The path field, pointed at that machine: the same picker, seeded exactly as somebody typing
    // `linux-box:` would have reached, so the two routes cannot diverge. No local rows under it —
    // every one of them means "on this computer", which is the answer that was just not chosen.
    const chosen = await whereTo(neosh, [], {
      title: `New conversation on ${machine.node.info.name}`,
      seed: `${hostPrefix(machine.node)}:`,
    });
    if (chosen === null) return;
    if (chosen.kind === "host") await startThere(neosh, chosen);
    return;
  }
  // Empty outside a repository, which is also how this knows there is nothing to ask about: a
  // worktree is a git idea, and a directory that is not a checkout has exactly one answer to
  // "where does this conversation go".
  const trees = await neosh.git.worktrees(here ? { cwd: here } : undefined)
    .catch(() => [] as WorktreeInfo[]);
  // The git plugin may also be switched off, in which case the rows that lead into it would lead
  // nowhere. Offering an action that cannot happen is worse than not offering it.
  const commands = new Set((await neosh.cmd.list().catch(() => [])).map((c) => c.name));
  const canBranch = trees.length > 0 && commands.has("git.worktree.new.auto");
  const canInside = trees.length > 0 && commands.has("git.worktree.new.inside");
  const canName = trees.length > 0 && commands.has("git.worktree.new");
  // On the list, and not merely on disk. `here` is dropped because you are already in it.
  const known = new Set(arrangement.all());
  const others = trees.filter((t) => t.path !== here && known.has(t.path));

  if (!canBranch && !canInside && !canName && others.length === 0) {
    await neosh.session.create(here ? { cwd: here } : undefined);
    return;
  }

  const rows: Array<PickerItem<Where>> = [
    {
      label: "Here",
      detail: here ?? "this project",
      icon: "●",
      hl: "Diagnostic.Info",
      value: { kind: "here" },
    },
  ];
  // First, and above the one that asks for a name, because it is the answer more often: a branch
  // named before the work is a decision made at the worst possible moment, and the whole reason
  // this row exists is that having to make it is what stops people starting.
  if (canBranch) {
    rows.push({
      label: "In a new worktree",
      detail: "a clean branch, named for you, nothing to answer",
      keywords: "branch worktree scratch new fresh",
      icon: "+",
      hl: "Diagnostic.Ok",
      value: { kind: "scratch" },
    });
  }
  if (canInside) {
    // Where it will land, said in the row: a choice between two places is only a choice if both
    // rows name theirs. A relative `worktree.root` renames this directory, so it is read rather
    // than assumed.
    const configured = ((await neosh.opt.get<string>("worktree.root").catch(() => "")) ?? "")
      .trim();
    const dir = configured !== "" && !configured.startsWith("/")
      ? configured.replace(/\/+$/, "")
      : ".worktrees";
    rows.push({
      label: "In a new worktree, in this project",
      detail: `kept in ${dir}/ — travels with the repository`,
      keywords: "branch worktree inside project local",
      icon: "⌂",
      hl: "Accent",
      value: { kind: "inside" },
    });
  }
  if (canName) {
    rows.push({
      label: "In a new worktree, named…",
      detail: "a branch of its own, checked out somewhere else",
      keywords: "branch worktree name",
      icon: "+",
      hl: "Sidebar.Dim",
      value: { kind: "new" },
    });
  }
  // Somewhere else, before the list of trees rather than after it. Not because it is the commonest
  // answer — it is not — but because the list above it is unbounded, and a verb that sits under an
  // unbounded list is a verb whose distance from the cursor depends on how long you have used the
  // program. It is also the row that says the field completes paths, and a sentence about how to
  // type is no use once you have already scrolled past it looking for somewhere to type.
  rows.push(elsewhereRow("Another directory…"));
  for (const t of others) {
    const label = t.branch ?? t.head ?? t.path;
    rows.push({
      label,
      detail: `worktree  ·  ${t.path}`,
      keywords: `${t.path} worktree`,
      icon: "⎇",
      hl: "Git.Branch",
      value: { kind: "tree", path: t.path, label },
    });
  }

  const chosen = await whereTo(neosh, rows, { title: "New conversation" });
  if (chosen === null) return;
  // Every row below that reaches for git names the repository it is about. `^N` from a
  // conversation and `n` from a row on a different project are the same code path, and the only
  // thing that tells them apart is this argument.
  const at = here ? [here] : [];
  switch (chosen.kind) {
    case "here":
      await neosh.session.create(here ? { cwd: here } : undefined);
      return;
    case "scratch":
      await neosh.cmd.exec("git.worktree.new.auto", at)
        .catch((e: unknown) => neosh.notify(String(e), "warn"));
      return;
    case "inside":
      await neosh.cmd.exec("git.worktree.new.inside", at)
        .catch((e: unknown) => neosh.notify(String(e), "warn"));
      return;
    case "new":
      // Positionally: branch, path, cwd. The first two are what the command asks for when they are
      // missing, which is exactly what this row means.
      await neosh.cmd.exec("git.worktree.new", here ? ["", "", here] : [])
        .catch((e: unknown) => neosh.notify(String(e), "warn"));
      return;
    case "tree":
      // No message: creating a conversation switches to it, so the transcript is empty, the
      // composer has the keyboard and the sidebar row is selected. All three say it already.
      await neosh.session.create({ cwd: chosen.path });
      return;
    case "host":
      await startThere(neosh, chosen);
      return;
    case "elsewhere": {
      const path = chosen.path ?? (await pathPicker(neosh, "New conversation"));
      if (path === null || path.trim() === "") return;
      try {
        await neosh.session.create({ cwd: path.trim() });
      } catch (e) {
        neosh.notify(String(e), "warn");
      }
    }
  }
}

// ---------------------------------------------------------------------------
// Where — one field for every answer to "which directory, on which computer"
// ---------------------------------------------------------------------------

/**
 * What a directory is called across machines, asked of the workspace rather than of the panel.
 *
 * A conversation in it is the authority — the host stamps `project_key` on every one of them from
 * the repository's origin — and a project with nothing open falls back to what the panel wrote down
 * the last time one was. Both, in that order, because only the first is guaranteed current and only
 * the second survives a project being emptied.
 *
 * `repo_root` counts as well as `cwd`: a worktree and its repository are one project, and `n` on
 * either means a conversation in the same repository.
 */
async function projectKey(
  neosh: Neosh,
  arrangement: Arrangement,
  cwd: string,
): Promise<string | null> {
  const sessions = await neosh.session.list().catch(() => [] as SessionInfo[]);
  const live = sessions.find((s) => (s.cwd === cwd || s.repo_root === cwd) && s.project_key);
  return live?.project_key || arrangement.key(cwd);
}

/**
 * Every answer `^N` and `^O` accept.
 *
 * `elsewhere` carries its path when the field already produced one, and leaves it off when the row
 * was chosen from the menu — the difference between "you typed a directory" and "you asked to be
 * asked for one".
 */
type Where =
  | { kind: "here" }
  | { kind: "scratch" }
  | { kind: "inside" }
  | { kind: "new" }
  | { kind: "elsewhere"; path?: string }
  | { kind: "tree"; path: string; label: string }
  /** A repository to fetch first and open second. */
  | { kind: "clone"; url: GitUrl }
  /** On another computer, in one of its directories. */
  | { kind: "host"; node: string; cwd: string; label: string }
  /** Look at one machine's directories rather than every machine's projects. */
  | { kind: "machine"; node: string; name: string }
  /** A machine that is on the list and cannot be used yet. `↵` says which of those it is. */
  | { kind: "unreachable"; name: string; why: string };

/** The row that drops through to a path, wherever a caller wants it in the order. */
function elsewhereRow(label: string): PickerItem<Where> {
  return {
    label,
    // What the field does, said on the row that is about the field. The alternative was a hint
    // strip nobody reads until they already know what they are looking for.
    detail: "or type a path — `/`, `~` and `./` complete as you go",
    keywords: "path folder directory type browse elsewhere",
    icon: "…",
    hl: "Sidebar.Dim",
    value: { kind: "elsewhere" },
  };
}

/**
 * What the field is completing against, read off what has been typed.
 *
 * Three states and one field, because the alternative is a mode — and a mode is a thing you have to
 * be in before you can type, which is the entire complaint this replaced. A path is a path the
 * moment it starts like one; `<machine>:` is scp's syntax and means there what it means here, which
 * is the only reason it is worth having a syntax at all.
 */
type Field =
  | { kind: "rows" }
  | { kind: "local"; path: string }
  | { kind: "remote"; node: SwarmNode; path: string }
  | { kind: "clone"; url: GitUrl };

/** Whether a query has started to look like a path rather than a search. */
function looksLikePath(q: string): boolean {
  return q.startsWith("/") || q.startsWith("~") || q.startsWith("./") || q.startsWith("../");
}

/** A repository to clone, and the two names its destination is built from. */
type GitUrl = { url: string; owner: string; repo: string; host: string };

/**
 * The git plugin's verb for fetching one.
 *
 * Called by name rather than imported, which is what keeps this panel free of a hard dependency on
 * git: `requires` would order the two plugins and make one unloadable without the other, and the
 * sidebar has to work in a workspace where git is disabled. Absent, the address row is not offered
 * at all.
 */
const CLONE_COMMAND = "git.clone";

/** The characters GitHub, GitLab and the rest allow in an account or repository name. */
const NAME = /^[A-Za-z0-9._-]+$/;

/**
 * A repository address, or `null` for anything that is not one.
 *
 * Four spellings, because all four are things people have in the clipboard: the `https://` URL a
 * browser's address bar holds, the `git@host:owner/repo` one the *Clone* button offers, `ssh://`
 * for the same thing written in full, and bare `owner/repo`.
 *
 * **The shorthand is only safe because this field has no menu behind it.** `^O` used to rank rows
 * against what you typed, so `docs/api` was a filter; with the worktree list gone it matches
 * nothing, and a two-segment name is far likelier to be a repository than a relative directory
 * nobody said what it was relative to. `Type a path…` stays on screen either way, so the other
 * reading is still one keystroke away — and the row names the full URL it resolved to rather than
 * echoing the shorthand, so what is about to be fetched is never a guess the reader has to make.
 */
function readGitUrl(q: string): GitUrl | null {
  const text = q.trim().replace(/\/+$/, "");
  if (text === "" || looksLikePath(text)) return null;
  const strip = (s: string) => s.replace(/\.git$/, "");

  // `git@github.com:owner/repo.git`, and the `ssh://git@host/owner/repo` spelling of the same.
  const scp = /^(?:ssh:\/\/)?([A-Za-z0-9._-]+@)([A-Za-z0-9.-]+)[:/]+(.+)$/.exec(text);
  if (scp) {
    const parts = strip(scp[3]!).split("/").filter(Boolean);
    const repo = parts.pop();
    const owner = parts.pop();
    if (!repo || !owner || !NAME.test(repo) || !NAME.test(owner)) return null;
    return { url: text, owner, repo, host: scp[2]! };
  }

  // `file:///srv/mirrors/thing` — a local mirror or a bare repository on a mounted volume. The
  // host is empty by construction, which is why it cannot go through the branch below: `[^/]+`
  // needs a character and `file://` is followed straight by the path's leading slash.
  const file = /^file:\/\/(\/.+)$/.exec(text);
  if (file) {
    const parts = strip(file[1]!).split("/").filter(Boolean);
    const repo = parts.pop();
    if (!repo || !NAME.test(repo)) return null;
    const owner = parts.pop() ?? "";
    return { url: text, owner: NAME.test(owner) ? owner : "", repo, host: "" };
  }

  const web = /^(https?|ssh|git):\/\/([^/]+)\/(.+)$/.exec(text);
  if (web) {
    const parts = strip(web[3]!).split("/").filter(Boolean);
    const repo = parts.pop();
    const owner = parts.pop();
    if (!repo || !owner || !NAME.test(repo) || !NAME.test(owner)) return null;
    // The host with any `user@` in front of it dropped: it belongs to the URL, which is kept
    // whole, and not to the sentence naming where this is coming from.
    return { url: text, owner, repo, host: web[2]!.replace(/^.*@/, "") };
  }

  // `owner/repo`. Exactly two segments — `a/b/c` is a path far more often than it is anything
  // else, and guessing which two of the three were meant is a guess nobody asked for.
  const parts = text.split("/");
  if (parts.length === 2 && parts.every((p) => NAME.test(p)) && !text.includes("@")) {
    const [owner, repo] = parts as [string, string];
    const bare = strip(repo);
    if (!NAME.test(bare)) return null;
    return { url: `https://github.com/${owner}/${bare}`, owner, repo: bare, host: "github.com" };
  }
  return null;
}

/**
 * Split `linux-box:/home/me/src` into the machine and the path.
 *
 * Matched on what the machine is *called here* — the alias if you set one, since that is the name
 * every panel shows and therefore the only one you could have read off the screen — and on the
 * start of its id, which is what you have when two machines share a name. A colon in a query that
 * names no machine is not a host prefix: it is a directory with a colon in it, which is legal on
 * every filesystem this runs on.
 */
function readField(query: string, nodes: SwarmNode[], clone: boolean): Field {
  const at = query.indexOf(":");
  if (at > 0) {
    const who = query.slice(0, at).toLowerCase();
    const node = nodes.find((n) =>
      n.info.name.toLowerCase() === who || n.info.id.toLowerCase().startsWith(who)
    );
    if (node) return { kind: "remote", node, path: query.slice(at + 1) };
  }
  // After the machines and before the paths. After, because a machine you have named `git` is
  // still yours and `git:` should reach it — the check above is an exact match on a name you
  // chose, so it cannot be shadowed by a spelling nobody registered. Before the paths, because a
  // URL is not one and `looksLikePath` would not have claimed it anyway; the order that matters
  // is that `~/repos/thing` never reaches the shorthand branch, which is why that test is inside
  // `readGitUrl` rather than out here.
  const url = clone ? readGitUrl(query) : null;
  if (url) return { kind: "clone", url };
  return looksLikePath(query) ? { kind: "local", path: query } : { kind: "rows" };
}

/** How a machine's rows are prefixed, and what `<Tab>` puts back in the field. */
function hostPrefix(node: SwarmNode): string {
  // The name, unless it has a colon or a space in it — in which case the id is the only thing that
  // round-trips through a field whose separator is a colon.
  return /[:\s]/.test(node.info.name) ? node.info.id.slice(0, 8) : node.info.name;
}

/**
 * The machines, as rows you can go into.
 *
 * Every paired machine, including the ones that cannot be used — greyed, with the reason on them.
 * The list used to skip anything not `up` and not accepting commands, which is right about what can
 * be *done* and wrong about what should be *said*: somebody who has just paired a computer and is
 * looking for it finds an empty list and nothing anywhere to explain it, which reads as the feature
 * not existing rather than as the machine not being ready.
 */
function machineRows(nodes: SwarmNode[], ascii: boolean): PickerItem<Where>[] {
  const out: PickerItem<Where>[] = [];
  // The colour each machine wears in the project panel, so the row you pick here is the colour of
  // the block the conversation will appear in.
  const colors = machineColors(nodes);
  for (const n of nodes) {
    const usable = n.up && n.capabilities.accepts_commands;
    const projects = n.capabilities.projects;
    const conversations = projects.reduce((sum, p) => sum + p.sessions, 0);
    const working = projects.reduce((sum, p) => sum + p.running, 0);
    const state = n.link.state === "up"
      ? [
        `${projects.length} ${projects.length === 1 ? "project" : "projects"}`,
        conversations > 0 ? `${conversations} open` : "",
        working > 0 ? `${working} working` : "",
        n.capabilities.accepts_commands ? "" : "read-only — nothing can be started there",
      ].filter(Boolean).join("  ·  ")
      : n.link.state === "waiting"
        ? "has not allowed this computer yet — ^J there"
        : n.link.state === "down"
          ? "disconnected — ^J to reconnect"
          : "connecting…";
    const [icon, hl] = n.link.state === "up"
      ? usable
        ? [ascii ? "*" : "●", colors.get(n.info.id) ?? "Swarm.Up"]
        : [ascii ? "-" : "◐", "Sidebar.Dim"]
      : n.link.state === "waiting"
        ? [ascii ? "!" : "◍", pulseHl("Status.Pending")]
        : n.link.state === "down"
          ? [ascii ? "-" : "○", "Sidebar.Dim"]
          : [spinnerFrame(), "Status.Streaming"];
    out.push({
      label: n.info.name,
      // The prefix on every row, so the syntax is learned by reading rather than by being told.
      detail: `${state}   ${hostPrefix(n)}:`,
      keywords: `${n.info.id} ${n.info.os} computer machine remote host ${hostPrefix(n)}`,
      icon,
      hl,
      value: usable
        ? { kind: "machine", node: n.info.id, name: hostPrefix(n) }
        // Nothing to go into, so `↵` says why rather than opening an empty list — and stays here,
        // because being told a machine is not ready is not a reason to lose the list you were in.
        : { kind: "unreachable", name: n.info.name, why: state },
    });
  }
  return out;
}

/** One machine's directories, once you are inside it. */
function projectRows(node: SwarmNode, ascii: boolean): PickerItem<Where>[] {
  const prefix = hostPrefix(node);
  return node.capabilities.projects.map((p) => ({
    // The prefixed path, because `<Tab>` puts a row's *label* back in the field and a bare name
    // would take the field somewhere that is not a directory on any machine.
    label: `${prefix}:${p.cwd}`,
    detail: [
      p.name,
      p.sessions > 0
        ? `${p.sessions} ${p.sessions === 1 ? "conversation" : "conversations"}`
        : "nothing open",
      p.running > 0 ? `${p.running} working` : "",
    ].filter(Boolean).join("  ·  "),
    keywords: `${p.name} ${p.key} remote`,
    // Filled when somebody is working in it, hollow when nobody is. The one thing the flag could
    // not say while every project on the list was one with a conversation in it.
    icon: p.active ? (ascii ? "*" : "●") : (ascii ? "-" : "○"),
    hl: p.active ? "Diagnostic.Ok" : "Sidebar.Dim",
    value: {
      kind: "host" as const,
      node: node.info.id,
      cwd: p.cwd,
      label: `${p.name} on ${node.info.name}`,
    },
  }));
}

/**
 * The one question both `^N` and `^O` ask: which directory, on which computer.
 *
 * A picker whose filter line **is** the field. Type nothing and it is the menu the caller passed;
 * type `/`, `~` or `./` and it is a directory completer; type `linux-box:` and it is a directory
 * completer pointed at that machine. Nothing has to be navigated to first, which was the whole
 * complaint: the path was a row at the bottom of a list, under an unbounded number of worktrees,
 * and reaching it meant arrowing past everything the program had ever been told about.
 *
 * `<Tab>` walks into the highlighted directory and `↵` takes it, which is `pathPicker`'s bargain
 * and is why a completion row's label has to be the whole path.
 *
 * Choosing a machine reopens this seeded with `<machine>:` rather than descending in place: the
 * picker owns its own filter line, and a second level whose rows lie about what is in the field is
 * a `<Tab>` that jumps somewhere nobody asked for.
 */
async function whereTo(
  neosh: Neosh,
  base: PickerItem<Where>[],
  opts: { title: string; seed?: string; clone?: boolean },
): Promise<Where | null> {
  const ascii = (await neosh.opt.get<boolean>("ui.ascii_only").catch(() => false)) ?? false;
  let seed = opts.seed ?? "";

  /**
   * The machines, and the rows built from them. Reread rather than captured, because the list is
   * live: a peer connecting, or gaining a project, while this is open has to change the rows under
   * the cursor. Captured once, `subscribe` would re-rank the same stale array for ever and the
   * subscription would be a promise the picker was not keeping.
   */
  let nodes: SwarmNode[] = [];
  // Mutated in place, never reassigned: `reload` re-ranks the very array the picker was handed,
  // so a fresh array here would be a list that never caught up with anything.
  const rows: PickerItem<Where>[] = [];
  const refill = async () => {
    const me = (await neosh.swarm.self().catch(() => null))?.id ?? null;
    // This machine is in `nodes()` like any other — a fleet plugin walking the list reaches its
    // own node, which is deliberate — and it is the one row that must not be here: every other row
    // in this picker already means "on this computer", and `mymac:/tmp` would otherwise start a
    // local conversation by the remote path and then try to *watch* it over a socket to ourselves.
    nodes = (await neosh.swarm.nodes().catch(() => [] as SwarmNode[]))
      .filter((n) => n.info.id !== me);
    // No `Add a computer…` row, deliberately. The swarm is on by default, so on every
    // single-machine workspace — which is most of them — that row would be a permanent line in a
    // menu pressed many times a day, about a thing its reader is not doing. It is the archive-count
    // argument one panel along. This picker lists the computers you *have*; getting one is `^J`,
    // which has a key of its own and a line in the legend.
    const machines = machineRows(nodes, ascii);
    rows.splice(0, rows.length, ...base, ...machines);
    return machines.length > 0;
  };

  // The last refusal reported, so a folder macOS is hiding is said once and not once per keystroke.
  let saidDenied: string | null = null;

  for (;;) {
    const anyMachines = await refill();

    const chosen = await picker<Where>(neosh, rows, {
      title: opts.title,
      width: 84,
      height: 14,
      query: seed,
      placeholder: "nothing matches — <CR> takes the path you typed",
      hints: anyMachines
        ? "↵ choose   ⇥ into a directory   / a path   name: another computer"
        : "↵ choose   ⇥ into a directory   / or ~ for a path",
      source: async (query) => {
        const field = readField(query, nodes, opts.clone ?? false);
        if (field.kind === "clone") {
          // One row, and no directory completions under it: what has been typed is an address, so
          // there is nothing on this disk it could be completing towards. The label is the *full*
          // URL rather than what was typed — `owner/repo` is a shorthand this expanded, and the
          // one moment to say what it expanded to is before anything is fetched.
          const { url } = field;
          // The detail is the URL and nothing else. It wrapped onto a second line with a sentence
          // about what `↵` does on the end of it — which the hint strip two rows below already
          // says, on every row, for free. What only this row can tell you is what the shorthand
          // turned into.
          return [{
            // `owner` is empty for a `file://` mirror that is not two levels deep, and
            // `Clone /thing` reads as a path rather than as a repository.
            label: `Clone ${url.owner ? `${url.owner}/` : ""}${url.repo}`,
            detail: url.url,
            icon: ascii ? "v" : "⤓",
            hl: "Diagnostic.Ok",
            value: { kind: "clone" as const, url },
          }];
        }
        if (field.kind === "rows") {
          // Our own ranking, because a `source` replaces the picker's filtering wholesale. Same
          // fuzzy scorer the picker uses, over the same label-and-keywords, so a query that is not
          // a path behaves exactly as it would with no source at all.
          if (query.trim() === "") return rows;
          return rows
            .map((item) => ({ item, m: fuzzy(`${item.label} ${item.keywords ?? ""}`, query) }))
            .filter((r): r is { item: PickerItem<Where>; m: Match } => r.m !== null)
            .sort((a, b) => b.m.score - a.m.score)
            .map((r) => r.item);
        }
        if (field.kind === "local") {
          const answer = await neosh.path
            .complete(field.path)
            .catch(() => ({ paths: [] as string[], denied: undefined }));
          // Same rule the remote branch below follows: a directory the OS refused is said out
          // loud, never drawn as a directory with nothing in it. Once per reason — this runs on
          // every keystroke.
          if (answer.denied && answer.denied !== saidDenied) {
            saidDenied = answer.denied;
            neosh.notify(answer.denied, "warn");
          } else if (!answer.denied) {
            saidDenied = null;
          }
          return answer.paths.map((path) => ({
            label: path,
            icon: ascii ? ">" : "▸",
            hl: "Sidebar.Dim",
            value: { kind: "elsewhere" as const, path },
          }));
        }
        const prefix = hostPrefix(field.node);
        // Nothing typed after the colon: what that machine offers, which is a short list of real
        // places rather than the contents of `/`. Typing on turns it into a directory listing.
        //
        // A machine with nothing on that list falls through to its home directory rather than
        // showing an empty one — a computer you have just paired has never had a project on it,
        // and "no rows" is the answer least likely to be read as "and here is where to start".
        const offered = field.path === "" ? projectRows(field.node, ascii) : [];
        if (offered.length > 0) return offered;
        const paths = await neosh.swarm
          .browse(field.node.info.id, field.path === "" ? "~/" : field.path)
          .catch((e) => {
            // A peer that went away mid-keystroke, or one too old to answer. Said out loud rather
            // than drawn as an empty directory, which is the one reading that is never true.
            neosh.notify(String(e), "warn");
            return [] as string[];
          });
        return paths.map((path) => ({
          label: `${prefix}:${path}`,
          detail: field.node.info.name,
          icon: ascii ? ">" : "▸",
          hl: "Sidebar.Remote",
          value: {
            kind: "host" as const,
            node: field.node.info.id,
            cwd: path,
            label: `${path} on ${field.node.info.name}`,
          },
        }));
      },
      // What `↵` means when the field holds something no row offered — which for a directory you
      // know the name of is the ordinary case, not the exception.
      freeform: (query) => {
        const field = readField(query, nodes, opts.clone ?? false);
        // A URL is deliberately not freeform-able. `↵` on the address row is what clones, and this
        // is the path that runs when *no* row matched — so answering here would mean a clone
        // starting from a keystroke aimed at a row the reader could not see.
        if (field.kind === "clone") return null;
        if (field.kind === "local") return { kind: "elsewhere", path: query.trim() };
        if (field.kind === "remote" && field.path.trim() !== "") {
          return {
            kind: "host",
            node: field.node.info.id,
            cwd: field.path.trim(),
            label: `${field.path.trim()} on ${field.node.info.name}`,
          };
        }
        return null;
      },
      // A machine connecting, or gaining a project, while the list is open. Same reason the
      // computers panel subscribes: a row that only changes on the next press is a row that was
      // wrong when it was read. `refill` first, or the reload re-ranks the same stale array.
      subscribe: (reload) => neosh.swarm.onChange(() => void refill().then(reload)),
    });

    if (chosen === null) return null;
    if (chosen.kind === "machine") {
      // Back into the same picker with the machine's prefix already typed, which is exactly the
      // state somebody would have reached by typing it — so the two routes cannot diverge.
      seed = `${chosen.name}:`;
      continue;
    }
    if (chosen.kind === "unreachable") {
      neosh.notify(`${chosen.name}: ${chosen.why}`, "info");
      seed = "";
      continue;
    }
    return chosen;
  }
}

/** Start a conversation on another machine, and go and look at it. */
async function startThere(
  neosh: Neosh,
  chosen: { node: string; cwd: string; label: string },
): Promise<void> {
  // Started *there*. The agent runs on that machine, against that machine's files, which is the
  // point — this is not a way to work on a remote directory from here.
  try {
    const session = await neosh.swarm.command(chosen.node, "", {
      command: "new_session",
      cwd: chosen.cwd,
      title: null,
    });
    // Opened, not merely reported. Starting a conversation here switches to it, and one started
    // over there that only produced a toast was the same key doing visibly less for the same
    // intention — you had to go and find, in `^J`, the thing you had just made.
    if (session) {
      await neosh.cmd.exec("swarm.open", [chosen.node, session]).catch(() => {});
      return;
    }
    neosh.notify(`started ${chosen.label}`);
  } catch (e) {
    neosh.notify(String(e), "warn");
  }
}

async function activateTarget(
  neosh: Neosh,
  arrangement: Arrangement,
  target: Target | undefined,
  p: Panel,
): Promise<void> {
  if (!target) return;
  // Somebody else's row. Nothing here knows what it does, which is the point — the contribution
  // named a command and this invokes it.
  if (target.kind === "custom") {
    if (!target.command) return;
    await neosh.cmd.exec(target.command, target.args).catch((e: unknown) => {
      neosh.notify(String(e), "warn");
    });
    await p.draw();
    return;
  }
  // A conversation on another computer, opened as a conversation: the pane arrives in a mirror of
  // it and the keyboard goes to its composer, exactly as `↵` on one of yours does. Leaving the panel
  // only once the mirror is there, so a machine that has gone keeps you on the row that says so.
  if (target.kind === "remote") {
    if (await openRemote(p.here, target.node, target.session)) await p.leave();
    return;
  }
  if (target.kind === "add") {
    await p.leave();
    await neosh.cmd.exec("project.open").catch(() => {});
    return;
  }
  // A project that is only on other computers. There is nothing here to switch to, so `↵` does
  // what it does on any project row with something in it: folds. Starting work in it is `n`, which
  // asks which computer — see {@link newConversation}.
  if (target.kind === "elsewhere") {
    await arrangement.toggleFold(target.key);
    await p.draw();
    return;
  }
  if (target.kind === "project") {
    // A pinned project with nothing in it yet: `↵` should start something, not fold an empty list.
    // A repository whose conversations are all in its worktrees is not that — the row has children
    // to fold, which is what `s.repo_root === target.cwd` finds.
    const sessions = await neosh.session.list().catch(() => [] as SessionInfo[]);
    if (!sessions.some((s) => s.cwd === target.cwd || s.repo_root === target.cwd)) {
      await p.here.session.create({ cwd: target.cwd });
      await p.leave();
      return;
    }
    await arrangement.toggleFold(target.cwd);
    await p.draw();
    return;
  }
  try {
    await p.here.session.switch(target.id);
  } catch (e) {
    neosh.notify(String(e), "warn");
    return;
  }
  await p.leave();
}

/** The project a row belongs to — its own, or the one its conversation is in. */
/**
 * Which machine a row is about, or `null` for one on this computer.
 *
 * The one question every remote verb asks, and it is asked of the *row* rather than of a mode the
 * panel is in — which is what lets `t`, `c` and `C` be one key each rather than one key each per
 * kind of row. A repository on several machines answers with the first, which is the one its own
 * marker is coloured by: a project you can reach on one of two computers is a project you can
 * reach, and the row already said so.
 */
function machineFor(
  target: Target | undefined,
): { id: string; name: string; cwd?: string } | null {
  if (target?.kind === "remote") {
    return { id: target.node, name: target.host, cwd: target.cwd };
  }
  if (target?.kind === "elsewhere") {
    const first = target.machines[0];
    return first ? { id: first.id, name: first.name, cwd: first.cwd } : null;
  }
  return null;
}

function owningProject(target: Target | undefined): string | null {
  if (!target) return null;
  return target.kind === "project" || target.kind === "session" ? target.cwd : null;
}

/**
 * Which directory to add — here, or on another computer.
 *
 * **This is the path field, and nothing else.** It used to open onto every checkout `git worktree
 * list` could see, and every one of those rows was wrong in one of two ways. The trees already on
 * the panel were an offer to add what was added — the repository you are standing in was the
 * second row — and the trees that were *not* on it were the scratch branches you finished with in
 * March and took off the list with `X`, handed straight back. Its one filter was a live
 * conversation's `cwd`, which is neither of those things, so what survived it was exactly the set
 * nobody wanted: twenty paths of finished work, each one drawn as its own full path with its
 * basename and branch repeated after it, in a float too narrow for either, so the rows truncated
 * mid-branch and read as noise. That was the same twenty rows
 * {@link newConversation} had already stopped offering, still here under the other key.
 *
 * And there is no filter that fixes it, which is the point: `^O` means *somewhere I do not work
 * yet*, and the one list that cannot answer that is a list of the repository you are already in.
 * So the filter line **is** the path field from the first keystroke — `/`, `~` and `./` complete
 * directories as you go — and `Type a path…` stays as the row that says so, because a field whose
 * behaviour is only in the hint strip is a field you have to be told about twice.
 *
 * The machines stay, because "add a project" never had a reason to mean "on this computer".
 * `linux-box:` completes over there and `↵` starts the conversation there; the row for it arrives
 * in the panel the way every other remote conversation's does.
 *
 * And an address is the third thing this field takes. Paste `https://github.com/owner/repo`, the
 * `git@` spelling, or just `owner/repo`, and the row becomes *clone it* — because "add a project"
 * is as often about a repository you do not have yet as about a directory you do, and the
 * alternative was leaving neosh, cloning in a shell, and coming back to type the path.
 *
 * Answers `null` for anything that was not a local directory — including a remote one, which has
 * been acted on by then. The caller's job is to open a path here, and a path on somebody else's
 * disk is not one. A clone answers with where it landed, which *is* a local directory, so the
 * project opens exactly as a typed one does.
 */
async function chooseDirectory(neosh: Neosh): Promise<string | null> {
  const rows: PickerItem<Where>[] = [elsewhereRow("Type a path…")];
  // Whether an address is worth recognising at all. The git plugin is what clones — and it can be
  // switched off — so without it a pasted URL is not a row this could act on. Offering a verb that
  // cannot happen is worse than not offering it, which is the rule `^N`'s worktree rows follow one
  // function up.
  const clone = (await neosh.cmd.list().catch(() => [])).some((c) => c.name === CLONE_COMMAND);

  const chosen = await whereTo(neosh, rows, { title: "Add project", clone });
  if (chosen === null) return null;
  if (chosen.kind === "host") {
    await startThere(neosh, chosen);
    return null;
  }
  if (chosen.kind === "clone") {
    // The git plugin owns the asking and the fetching, exactly as it owns making a worktree: this
    // panel decides that what you typed is an address, and nothing more.
    //
    // `null` on the way out even when it worked, because a successful clone has *already* opened
    // the conversation — it does that so `^K git.clone` lands you in the repository too, rather
    // than fetching one and leaving you where you were. Returning the path here would be a second
    // `session.create` on the same directory, which is two conversations for one keypress.
    // Only the URL. The shorthand has already been expanded into one, which is what this panel's
    // parse is *for* — the row had to name what it resolved to before anything was fetched. Git
    // reads the owner and repository back off it rather than being handed them, so `git.clone
    // <url>` is a complete call from a key, the palette or a script, and there are not two parses
    // to keep in step.
    await neosh.cmd
      .call(CLONE_COMMAND, [chosen.url.url])
      .catch((e: unknown) => neosh.notify(String(e), "warn"));
    return null;
  }
  if (chosen.kind !== "elsewhere") return null;
  // Chosen from the menu rather than typed: that row means "ask me", and this is the asking.
  return chosen.path ?? (await pathPicker(neosh, "Add project"));
}

/**
 * Put a conversation away, or bring it back.
 *
 * Asks nothing, on purpose. A confirmation is the price of an irreversible action, and this one is
 * reversible by one key from the archive — charging for it would teach you to dismiss dialogs,
 * which is exactly the habit that makes the delete dialog useless.
 */
async function setArchived(neosh: Neosh, session: string, archived: boolean): Promise<void> {
  try {
    await neosh.session.archive(session, archived);
    neosh.notify(archived ? "archived — `a` in the panel finds it" : "unarchived");
  } catch (e) {
    neosh.notify(String(e), "warn");
  }
}

/**
 * Delete a conversation, having asked. Answers whether it went.
 *
 * The file goes from disk and there is no undo, so this always stops and asks — including for a
 * conversation with nothing in it yet. It used to skip the question there, on the grounds that a
 * dialog for something with nothing to lose is friction; what that actually bought was a key whose
 * behaviour depended on state you cannot see from the row, so the one time it did ask was the one
 * time your fingers were already through it.
 *
 * `ui.confirm_destructive = false` is still the way out, and it is a setting rather than a guess.
 */
async function deleteSession(neosh: Neosh, session: string): Promise<boolean> {
  const info = (await neosh.session.list({ includeArchived: true }).catch(() => [] as SessionInfo[]))
    .find((s) => s.id === session);
  const count = info?.message_count ?? 0;
  // The checkout this was the last conversation in, if it was one. A worktree is made for the
  // conversations in it, so the last of them going takes the directory too — said in the same
  // dialog, because a delete that quietly removes a directory is a delete whose question did not
  // say what was at stake.
  const trees = await orphanedWorktrees(neosh, [session]);
  const detail = [
    count === 0
      ? "Nothing has been said in it yet."
      : `${count} ${count === 1 ? "message" : "messages"}, in ${info?.project || basename(info?.cwd ?? "")}.`,
    ...worktreeLines(trees),
    info?.archived
      ? "It is archived, so leaving it here costs nothing."
      : "Archiving keeps every word of it and takes it out of your list.",
  ];
  const ok = await confirmDestructive(
    neosh,
    `Delete "${clip(info?.label ?? "this conversation", 48)}"?`,
    { yes: "Delete", no: "Keep", detail },
  );
  if (!ok) return false;
  try {
    await neosh.session.close(session);
  } catch (e) {
    neosh.notify(String(e), "warn");
    return false;
  }
  const removed = await discardWorktrees(neosh, trees);
  if (removed > 0) neosh.notify(`removed the worktree at ${trees[0]?.path ?? ""}`);
  return true;
}

/**
 * Take a project off the list.
 *
 * Empty, this asks nothing: no file moves, the row goes, and `o` puts the directory back in one
 * keystroke — a dialog for that is the kind you learn to clear without reading. With conversations
 * still in it, removing the project is deleting every one of them, so it asks as the delete it is
 * and says how many and where they would otherwise go.
 *
 * The conversations go first and the project only goes if they all did: a project removed anyway
 * would take a row that still had something in it off the list.
 *
 * Removing the *last* project used to be the one case this could not finish: the store refused to
 * delete the very last conversation in the workspace. It no longer does — what you land in is a
 * placeholder, which is in no list and so in no project — so `X` here can leave the panel with
 * nothing but `+ Add project` on it.
 */
async function removeProject(
  neosh: Neosh,
  arrangement: Arrangement,
  cwd: string,
): Promise<void> {
  const inside = (await neosh.session.list({ includeArchived: true }).catch(() => [] as SessionInfo[]))
    .filter((s) => s.cwd === cwd);
  const name = inside[0]?.project || arrangement.name(cwd) || basename(cwd);
  // A worktree row is a directory this workspace made for the conversations in it, and removing
  // the row is removing the last reason for the checkout to be on the disk. The repository itself
  // is never this: `worktreeAt` answers only for a tree `git worktree list` says is one, and never
  // for the main checkout, so `X` on a repository's row is what it always was — the row goes and
  // the directory stays.
  const root = inside.find((s) => s.repo_root)?.repo_root ?? arrangement.root(cwd) ?? "";
  const tree = await worktreeAt(neosh, cwd, root);
  const trees = tree ? [tree] : [];

  if (inside.length > 0 || trees.length > 0) {
    const many = inside.length === 1 ? "conversation" : `${inside.length} conversations`;
    const question = inside.length === 0
      ? `Remove the worktree "${clip(name, 40)}"?`
      : trees.length > 0
      ? `Remove the worktree "${clip(name, 40)}" and its ${many}?`
      : `Remove "${clip(name, 40)}" and its ${many}?`;
    const ok = await confirmDestructive(neosh, question, {
      yes: trees.length > 0 ? "Remove" : "Delete",
      no: "Keep",
      detail: [
        ...(inside.length > 0 ? [atStake(inside)] : []),
        ...worktreeLines(trees),
        ...(inside.length > 0
          ? ["`x` on a conversation archives it instead, and an archived one is already out of the way."]
          : []),
      ],
    });
    if (!ok) return;
    for (const s of inside) {
      try {
        await neosh.session.close(s.id);
      } catch (e) {
        neosh.notify(String(e), "warn");
        return;
      }
    }
  }

  const removed = await discardWorktrees(neosh, trees);
  await arrangement.forget(cwd);
  neosh.notify(
    removed > 0 ? `removed the worktree at ${cwd}` : `removed ${short(cwd)} — \`o\` adds it back`,
  );
}

/**
 * How much is about to go, in a sentence.
 *
 * What is at stake is the point of the question, and the number that says it is how many of these
 * you would still have found in the panel: something you archived a month ago going is a different
 * size of loss from a conversation you were in this morning.
 */
function atStake(inside: SessionInfo[]): string {
  const archived = inside.filter((s) => s.archived).length;
  if (inside.length === 1) {
    return archived === 1
      ? "It is archived, and it goes from disk with the project."
      : "It goes from disk with the project.";
  }
  if (archived === inside.length) return `All ${inside.length} are archived, and all of them go from disk.`;
  if (archived === 0) return `All ${inside.length} go from disk.`;
  const live = inside.length - archived;
  return `${live} of them ${live === 1 ? "is" : "are"} still in your list, and all ${inside.length} go from disk.`;
}

/**
 * The computers in this workspace: yours, theirs, and the ones asking to join.
 *
 * A picker rather than a panel section, for the reason the archive is: it is a thing you look at
 * when you want it, and rows that are only occasionally the answer do not belong in the column you
 * work in.
 *
 * Machines asking to join sit at the top, because they are the only rows that are waiting on you.
 * Your own identity is at the foot, where it is out of the way but always somewhere you can point
 * at when the other machine asks who you are.
 */
async function showNodes(neosh: Neosh): Promise<void> {
  const me = await neosh.swarm.self().catch(() => null);
  if (!me) {
    // On by default, so off means somebody turned it off — or `--clean`, which has nowhere to
    // keep the key that *is* this machine's identity.
    neosh.notify(
      "the swarm is off — remove `enabled = false` from `[swarm]`, or leave --clean",
      "info",
    );
    return;
  }
  // What the listener managed, said on this machine's own row: an address a peer could dial, or
  // why there is not one. Written by the host as a workspace var, because it is a fact about the
  // workspace and not about whichever panel happened to ask.
  const listening = await neosh.vars
    .get<{ addr: string | null; error?: string }>({ scope: "global" }, "swarm.listen")
    .catch(() => null);
  const ascii = (await neosh.opt.get<boolean>("ui.ascii_only").catch(() => false)) ?? false;

  type Row =
    | { kind: "node"; node: SwarmNode }
    | { kind: "stranger"; stranger: SwarmStranger }
    | { kind: "add" }
    | { kind: "self" };

  // The last answer from the host, so a spinner frame costs a redraw rather than two round trips.
  // At 80ms a tick, rebuilding these over the API would be the most talkative thing in the
  // workspace, to animate one glyph.
  let nodes: SwarmNode[] = [];
  let strangers: SwarmStranger[] = [];
  // The short code each machine is drawn with in the project panel, so this list says the same
  // thing: the `@ms` you see on a row there is the `@ms` beside its name here.
  let codes = new Map<string, string>();
  let colors = new Map<string, string>();

  /** Whether anything on screen is mid-dial, and therefore whether the tick has any work. */
  const spinning = () =>
    nodes.some((n) => n.link.state === "connecting" || n.link.state === "retrying");

  const build = (): PickerItem<Row>[] => {
    const rows: PickerItem<Row>[] = [];

    for (const s of strangers) {
      rows.push({
        label: `${s.info.name}`,
        detail: s.dialled
          ? `found at ${s.addr ?? "an address you gave"}  ·  ${fingerprint(s.info.id)}  ·  ↵ to add`
          : `wants to join  ·  ${fingerprint(s.info.id)}  ·  ↵ to allow`,
        keywords: `${s.info.id} pending join pair new`,
        // A machine asking to join is the one row here that wants answering, so it wears the
        // colour the workspace already uses for *act now* and pulses on the same 1 Hz duty cycle
        // as every other waiting indicator. Nothing else in this list moves unless it is working.
        icon: "?",
        hl: pulseHl("Status.Pending"),
        value: { kind: "stranger", stranger: s },
      });
    }

    for (const n of nodes) {
      const running = n.agents.filter(turning).length;
      // One sentence per link state, because they are different answers to "why is it not here" —
      // and the fourth of them is the reason this reads the way it does. `waiting` is the far half
      // of pairing: reached, proven, and not yet allowed over there. It used to arrive as
      // `connecting` with attempt 0, which printed a bare `connecting…` and threw the only useful
      // sentence away, so adding a computer looked like a network fault for as long as anybody
      // was willing to watch it.
      const state = n.link.state === "up"
        ? `${n.agents.length} ${n.agents.length === 1 ? "conversation" : "conversations"}`
        : n.link.state === "waiting"
          // Reads on from the label, which is the machine's name — naming it again here gave
          // `linux-box  allow this computer on linux-box`. What the sentence has to carry is the
          // key and the fact that it is pressed somewhere else.
          ? "has not allowed this computer yet — ^J there"
          : n.link.state === "connecting"
            ? n.link.attempt > 0
              ? `connecting — try ${n.link.attempt}, ${n.reason ?? "no answer"}`
              : "connecting…"
            : n.link.state === "retrying"
              ? n.link.attempt > 0
                ? `reconnecting — try ${n.link.attempt}`
                : "reconnecting…"
              : `disconnected — ${n.reason ?? "it dials in"}  ·  ^R reconnects`;
      // The glyph says what the row is doing; the sentence says what to do about it. A spinner
      // only ever means "this machine is working on it" — never over `waiting`, where nothing on
      // this computer is trying and the motion would be a promise nobody is keeping.
      const [icon, hl] = n.link.state === "up"
        ? [ascii ? "*" : "●", colors.get(n.info.id) ?? "Swarm.Up"]
        : n.link.state === "waiting"
          ? [ascii ? "!" : "◍", pulseHl("Status.Pending")]
          : n.link.state === "down"
            ? [ascii ? "-" : "○", "Sidebar.Dim"]
            : [spinnerFrame(), "Status.Streaming"];
      rows.push({
        label: `${n.info.name}  @${codes.get(n.info.id) ?? ""}`,
        detail: [
          state,
          running > 0 ? `${running} working` : "",
          n.up && !n.capabilities.accepts_commands ? "read-only" : "",
          n.info.os,
          fingerprint(n.info.id),
        ].filter(Boolean).join("  ·  "),
        keywords: `${n.info.os} ${n.info.id} ${n.link.state}`,
        icon,
        hl,
        value: { kind: "node", node: n },
      });
    }

    rows.push({
      // The `+` is the icon now that this list has a gutter, not the first character of the label:
      // with both it read `+ + Add a computer…`.
      label: "Add a computer…",
      detail: "its address on your network, or through Tailscale",
      keywords: "pair join new machine host",
      icon: "+",
      hl: "Sidebar.Dim",
      value: { kind: "add" },
    });
    rows.push({
      label: `This computer  ·  ${me.name}`,
      // The fingerprint is what somebody at the other machine compares against. Shown here so
      // "what is my id" never means leaving the program to run a command. Beside it, whether
      // this machine can be dialled at all — the first question when adding it from over there.
      detail: [
        fingerprint(me.id),
        listening?.addr
          ? `listening on ${listening.addr}`
          : listening?.error
            ? `not listening — ${listening.error}`
            : "dial-only",
        "^Y copies the full id",
      ].join("  ·  "),
      keywords: `self me ${me.id}`,
      value: { kind: "self" },
    });
    return rows;
  };

  /** Ask the host again. Everything else here draws from what this last brought back. */
  const fetch = async () => {
    let chosen: unknown;
    [nodes, strangers, chosen] = await Promise.all([
      neosh.swarm.nodes().catch(() => []),
      neosh.swarm.strangers().catch(() => []),
      neosh.vars.get({ scope: "global" }, VAR_CODES).catch(() => null),
    ]);
    nodes = nodes.filter((n) => n.info.id !== me.id);
    codes = machineCodes(nodes, chosen);
    colors = machineColors(nodes);
  };

  const redraw = () => {
    items.splice(0, items.length, ...build());
  };
  const refill = async () => {
    await fetch();
    redraw();
  };

  await fetch();
  const items: PickerItem<Row>[] = build();

  const chosen = await picker(neosh, items, {
    title: "Computers",
    width: 84,
    height: 14,
    hints: "↵ open   ^E rename   ^T code   ^R reconnect   ^D disconnect   ^X remove   ^Y my id",
    // The list keeps up while it is open: a machine connecting, dropping, or moving from
    // "connecting" to a row of conversations changes under the cursor rather than on reopen.
    subscribe: (reload) => {
      const changed = neosh.swarm.onChange(() => {
        void refill().then(reload);
      });
      // And the spinner turns on the shared clock, off what the last change brought back rather
      // than off a fresh pair of API calls: a dial takes seconds, the clock ticks twelve times a
      // second, and asking the host what it knows on every frame to move one glyph would make
      // this panel the noisiest thing in the workspace. Nothing ticks while nothing is dialling,
      // so a list of connected machines costs exactly as much as it did before.
      const ticked = onTick(() => {
        if (!spinning()) return;
        redraw();
        reload();
      });
      return {
        dispose() {
          changed.dispose();
          ticked.dispose();
        },
      };
    },
    // Chords, because every bare letter a picker takes is a letter its filter can never contain.
    ownKeys: ["<C-y>", "<C-x>", "<C-r>", "<C-d>", "<C-e>", "<C-t>"],
    async onKey(key, ctx) {
      if (key.key.code.kind !== "char" || !key.key.mods.ctrl) return;
      switch (key.key.code.c.toLowerCase()) {
        case "y":
          await neosh.edit.copy(me.id);
          neosh.notify("copied this computer's id");
          return "handled";
        case "r": {
          const row = ctx.item;
          if (row?.kind !== "node" || row.node.link.state === "up") return "handled";
          await neosh.swarm.reconnect(row.node.info.id).catch((e) => neosh.notify(String(e), "warn"));
          neosh.notify(`dialling ${row.node.info.name}…`);
          await refill();
          return "reload";
        }
        case "d": {
          const row = ctx.item;
          if (row?.kind !== "node" || row.node.link.state === "down") return "handled";
          await neosh.swarm.disconnect(row.node.info.id).catch((e) => neosh.notify(String(e), "warn"));
          neosh.notify(`disconnected ${row.node.info.name} — ^R takes it back`);
          await refill();
          return "reload";
        }
        case "e": {
          const row = ctx.item;
          if (row?.kind !== "node") return "handled";
          // Prefilled with what the row says, so this is an edit rather than a blank field you
          // have to remember the old name to fill in. Clearing it is how you go back to the
          // hostname, which is why an empty answer is not the same as `Esc`.
          const next = await prompt(neosh, `Call ${row.node.info.name}`, {
            initial: row.node.info.name,
            width: 60,
          });
          if (next === null) return "handled";
          try {
            await neosh.swarm.rename(row.node.info.id, next.trim() === "" ? null : next.trim());
            await refill();
            return "reload";
          } catch (e) {
            // Almost always "that one is in your config file", which names the field to change.
            neosh.notify(String(e), "warn");
            return "handled";
          }
        }
        case "t": {
          // The code after the `@` on every row that is on this machine. Asked here, beside the
          // rename, because it is the other half of what a machine is called.
          const row = ctx.item;
          if (row?.kind !== "node") return "handled";
          if (await chooseCode(neosh, row.node.info.id, row.node.info.name, codes.get(row.node.info.id))) {
            await refill();
            return "reload";
          }
          return "handled";
        }
        case "x": {
          const row = ctx.item;
          if (row?.kind !== "node") return "handled";
          try {
            await neosh.swarm.unpair(row.node.info.id);
            neosh.notify(`removed ${row.node.info.name}`);
            await refill();
            return "reload";
          } catch (e) {
            // Almost always "that one is in your config file", which names the fix.
            neosh.notify(String(e), "warn");
            return "handled";
          }
        }
        default:
          return;
      }
    },
  });
  if (chosen === null) return;

  switch (chosen.kind) {
    case "self":
      await neosh.edit.copy(me.id);
      neosh.notify("copied this computer's id");
      return;
    case "add":
      await addComputer(neosh);
      return;
    case "stranger": {
      const s = chosen.stranger;
      // Asked even though we already know who it is, because knowing who it is *is* the question:
      // the fingerprint on screen has to be the one the person at the other machine is reading out.
      const ok = await confirm(neosh, `Add ${s.info.name}?`, {
        yes: "Add",
        no: "Not now",
        detail: [
          `${s.info.os}  ·  neosh ${s.info.version}`,
          fingerprint(s.info.id),
          "Check that fingerprint matches what the other computer shows under `This computer`.",
        ],
      });
      if (!ok) return;
      await neosh.swarm.pair(s.info.id, { name: s.info.name, addr: s.addr ?? undefined });
      return;
    }
    case "node": {
      // A row you cannot open should say why in the terms the row is in. "Nothing running" is true
      // of a machine that has not allowed this one yet, and is not what is wrong with it.
      if (chosen.node.link.state === "waiting") {
        neosh.notify(
          `${chosen.node.info.name} has not allowed this computer yet — press ^J there and add `
            + `${me.name}`,
          "info",
        );
        return;
      }
      if (!chosen.node.up) {
        neosh.notify(
          `${chosen.node.info.name} is not connected — ${chosen.node.reason ?? "still dialling"}`,
          "info",
        );
        return;
      }
      const newest = [...chosen.node.agents].sort((a, b) => b.updated_at - a.updated_at)[0];
      if (!newest) {
        neosh.notify(`${chosen.node.info.name} has nothing running`, "info");
        return;
      }
      await neosh.cmd.exec("swarm.open", [chosen.node.info.id, newest.session]).catch(() => {});
    }
  }
}

// ---------------------------------------------------------------------------
// A conversation on another computer
// ---------------------------------------------------------------------------

/**
 * Open a conversation on another machine, as a conversation.
 *
 * It used to be a float: a plain-text window onto the other machine's words, `i` for a one-line
 * prompt and no cards, no markdown, no working line — somewhere you could watch and could barely
 * talk to. The reasoning was that the chat pane is *yours*, and somebody else's conversation there
 * would give its composer two meanings. What it turned out to mean in practice is that the part of
 * the workspace on another machine was a second-class citizen of it.
 *
 * So the workspace mirrors it (`swarm.mirror`): a conversation in the pane, drawn by the very code
 * that draws yours — the same transcript, the same cards, the same composer — whose turns run on the
 * machine they belong to. What you type goes there, `Esc` asks it to stop, and nothing about it is
 * saved or listed here, because that machine lists and keeps it. The project panel says where you
 * are: its row is lit, and the status line names the machine.
 */
async function openRemote(neosh: Neosh, node: string, session: string): Promise<boolean> {
  try {
    await neosh.swarm.mirror(node, session);
    return true;
  } catch (e) {
    neosh.notify(String(e).replace(/^Error:\s*/, ""), "warn");
    return false;
  }
}

/**
 * Add a machine by dialling it.
 *
 * The address is all you type. Everything else — its name, its fingerprint, whether it is even a
 * neosh — is read off the machine itself, because a public key typed from memory is a public key
 * typed wrong, and because the point of showing a fingerprint is that it came from the far end
 * rather than from the same person who typed the address.
 */
async function addComputer(neosh: Neosh): Promise<void> {
  const addr = await prompt(neosh, "Add a computer — hostname:7717", { width: 60 });
  if (addr === null || addr.trim() === "") return;
  // A port is easy to forget and there is only one sensible default.
  const target = addr.includes(":") ? addr.trim() : `${addr.trim()}:7717`;

  let found;
  // A dial across a network somebody just typed the address of is the one thing here that can take
  // long enough to look broken — a wrong hostname sits on a TCP timeout — so it turns while it
  // waits. Progress is keyed and replaced in place, which is what makes this one line rather than
  // a column of `asking…`.
  const turning = onTick(() => {
    neosh.progress("swarm.probe", `${spinnerFrame()} asking ${target}…`);
  });
  try {
    // A state that stops being true the moment the far end answers or does not.
    neosh.progress("swarm.probe", `${spinnerFrame()} asking ${target}…`);
    found = await neosh.swarm.probe(target);
  } catch (e) {
    // `String(e)` here would read `NeoshError: not found: …: swarm i/o: Connection refused (os
    // error 61)`, which names three layers of plumbing before it gets to the part a person can act
    // on. What went wrong is that nothing answered, and what to check is spelling and whether the
    // other neosh is running.
    neosh.notify(
      `nothing answered at ${target} — is neosh running there, with `
        + "`listen` set in its `[swarm]`?",
      "warn",
    );
    neosh.log.warn(`swarm probe of ${target} failed: ${String(e)}`);
    return;
  } finally {
    turning.dispose();
    neosh.done("swarm.probe");
  }

  const ok = await confirm(neosh, `Add ${found.name}?`, {
    yes: "Add",
    no: "Cancel",
    detail: [
      `${target}  ·  ${found.os}  ·  neosh ${found.version}`,
      fingerprint(found.id),
      "Check that fingerprint matches what that computer shows under `This computer`.",
    ],
  });
  if (!ok) return;

  await neosh.swarm.pair(found.id, { name: found.name, addr: target });
  // Both halves have to happen, and only one of them is ours. Saying so now saves the ten minutes
  // otherwise spent wondering why the machine is listed and permanently unreachable.
  neosh.notify(`${found.name} added — now allow this computer over there, with ^J`);
  // And back to the panel, which is where that second half becomes visible: the new row sits on
  // `allow this computer on <name>` until somebody does it and then turns over to its
  // conversations, live, without a key being pressed here. Dropping the user back at the composer
  // instead is what made pairing feel like it had silently failed — the one thing worth watching
  // was on a panel they had just been taken out of.
  await showNodes(neosh);
}

/**
 * A node id, in groups, for reading aloud.
 *
 * Sixty-four hex characters is not something a person can compare. Four groups of four from the
 * front is — and it is the *front* rather than a hash of the whole because that is what both
 * machines display, so the two are comparable by eye.
 */
function fingerprint(id: string): string {
  const head = id.slice(0, 16);
  return (head.match(/.{1,4}/g) ?? [head]).join(" ");
}

async function renameSession(neosh: Neosh, session: string): Promise<void> {
  const current = (await neosh.session.list()).find((s) => s.id === session);
  const next = await prompt(neosh, "Rename conversation", {
    initial: current?.title ?? "",
    width: 60,
  });
  if (next === null) return;
  await neosh.session.rename(session, next.trim() === "" ? null : next);
}

// ---------------------------------------------------------------------------
// Content
// ---------------------------------------------------------------------------

/**
 * Projects, as two ordered groups: pinned, then the rest.
 *
 * A project is a directory you have worked in — added by hand, or arrived at because a conversation
 * was started there — and it stays one until you remove it. Every known directory is seeded, empty
 * or not, which is the whole of the fix for a project vanishing when you cleared out its
 * conversations: the list is written down (see [`VAR_KNOWN`]) rather than inferred from what
 * happens to be in it, and `X` on the heading is the only thing that shortens it.
 */
/**
 * Every repository the workspace knows about, and every checkout of each.
 *
 * Local conversations and remote ones go in through the same door and come out in the same tree,
 * which is the point: a conversation on the build box is not a different kind of thing from one
 * here, it is the same work in a different place, and a panel that files them separately makes you
 * check two places for one project.
 *
 * Merged on {@link SessionInfo.project_key}, which the host stamps from the origin remote — so the
 * repository is the same repository whatever each machine calls its path. A directory with no
 * remote falls back to `dir:<name>`, and two machines with a `neosh` directory each will be treated
 * as having one project: that *is* a guess, and it is the guess somebody with the same folder on
 * two computers means.
 */
function group(
  sessions: SessionInfo[],
  arrangement: Arrangement,
  peers: SwarmNode[],
): Project[][] {
  // ---- what is here -------------------------------------------------------
  const here = new Map<string, SessionInfo[]>();
  for (const cwd of arrangement.all()) here.set(cwd, []);
  for (const s of sessions) {
    const list = here.get(s.cwd);
    if (list) list.push(s);
    else here.set(s.cwd, [s]);
  }

  // Which checkout each directory is a linked worktree of, learned from its conversations — and
  // from what was written down the last time one of them said so, because a project stays on the
  // list after its conversations have gone and an emptied worktree is still a worktree.
  const rootOf = new Map<string, string>();
  const keyOf = new Map<string, string>();
  const branchOf = new Map<string, string>();
  for (const s of sessions) {
    if (s.repo_root && s.repo_root !== s.cwd && !rootOf.has(s.cwd)) rootOf.set(s.cwd, s.repo_root);
    if (s.project_key && !keyOf.has(s.cwd)) keyOf.set(s.cwd, s.project_key);
    if (s.branch && !branchOf.has(s.cwd)) branchOf.set(s.cwd, s.branch);
  }
  for (const cwd of here.keys()) {
    const remembered = arrangement.root(cwd);
    if (remembered && !rootOf.has(cwd)) rootOf.set(cwd, remembered);
    const key = arrangement.key(cwd);
    if (key && !keyOf.has(cwd)) keyOf.set(cwd, key);
  }

  // `session.list()` is most-recently-used first, so index in it is recency. Anything you have
  // never dragged sorts by that, after everything you have.
  const recency = new Map<string, number>();
  sessions.forEach((s, i) => {
    if (!recency.has(s.cwd)) recency.set(s.cwd, i);
  });

  // ---- what is elsewhere --------------------------------------------------
  // One entry per (machine, directory), from both of the things a peer tells us: the conversations
  // it has open, and the places it *works in*. The second is what makes a quiet project over there
  // visible at all — without it a repository somebody cleared out this morning is a row that
  // silently vanished, which is the one thing a list must never do.
  const over = new Map<string, Checkout>();
  const at = (node: SwarmNode, cwd: string): Checkout => {
    const fold = `${node.info.id}:${cwd}`;
    let found = over.get(fold);
    if (!found) {
      found = {
        cwd,
        node: { id: node.info.id, name: node.info.name, link: node.link },
        sessions: [],
        agents: [],
        fold,
      };
      over.set(fold, found);
    }
    return found;
  };
  const keyThere = new Map<string, string>();
  const nameThere = new Map<string, string>();
  for (const n of peers) {
    // The places it works in first, so a directory with a conversation in it is described by the
    // richer of the two answers and one without one still gets a row.
    //
    // Not from a machine that has not allowed this one yet. Its project list arrived in the
    // handshake — the half of pairing that did happen — so this is not about what may be shown; it
    // is that `waiting` is the far end explicitly saying no, and a one-sided pairing filling your
    // column with somebody else's repositories is a list growing on its own. Every other state is
    // the rule this panel already follows in the other direction: a machine that has been up and
    // has gone quiet keeps its rows, because a list that silently shortens is one you cannot tell
    // from a list that never had them.
    if (n.link.state === "waiting") continue;
    for (const p of n.capabilities.projects) {
      const c = at(n, p.cwd);
      c.root = p.repo_root ?? undefined;
      c.branch = p.branch ?? undefined;
      c.head = p.head ?? undefined;
      c.decorations = p.decorations ?? [];
      keyThere.set(c.fold, p.key);
      nameThere.set(c.fold, p.name);
    }
    for (const agent of n.agents) {
      const c = at(n, agent.cwd);
      c.root = agent.repo_root ?? c.root;
      c.branch = agent.branch ?? c.branch;
      c.agents.push({ node: n.info, agent });
      keyThere.set(c.fold, agent.project);
      if (!nameThere.has(c.fold)) nameThere.set(c.fold, agent.project_name);
    }
  }
  // Newest first, as `agents()` handed them over: a machine's own order is per machine, and two of
  // them interleaved by nothing would shuffle every time either one ticked.
  for (const c of over.values()) c.agents.sort((a, b) => b.agent.updated_at - a.agent.updated_at);

  // ---- one row per repository --------------------------------------------
  const projects = new Map<string, Project>();
  const project = (key: string, name: string): Project => {
    let found = projects.get(key);
    if (!found) {
      found = {
        key,
        cwd: null,
        name,
        favorite: false,
        sessions: [],
        mains: [],
        worktrees: [],
        machines: [],
        hosts: [],
        nodes: [],
      };
      projects.set(key, found);
    }
    return found;
  };

  const localKey = (cwd: string) => keyOf.get(cwd) ?? `dir:${basename(cwd)}`;
  // The main checkouts here. Claimed before the trees, so a repository always has its own row by
  // the time anything asks to nest under it.
  for (const [cwd, list] of here) {
    if (rootOf.has(cwd)) continue;
    const p = project(localKey(cwd), "");
    // Two local checkouts with one key is a repository cloned twice on one machine, which is rare
    // and is not a thing this panel can draw as one row: the second becomes a tree of the first,
    // which is at least true about them sharing an identity.
    if (p.cwd === null) {
      p.cwd = cwd;
      // What the host decided to call it. For a worktree that is the repository and the branch,
      // rather than whatever the directory happens to be named — a row reading `wt-fe3c0d93` tells
      // you nothing about which checkout it is. With nothing in it there is nobody to ask, so the
      // last answer is kept: emptying a project must not also rename it.
      p.name = list[0]?.project || arrangement.name(cwd) || basename(cwd);
      p.favorite = arrangement.isFavorite(cwd);
      p.sessions = list;
    } else {
      p.worktrees.push({
        cwd,
        branch: branchOf.get(cwd),
        sessions: list,
        agents: [],
        fold: cwd,
      });
    }
  }
  // The trees here.
  for (const [cwd, list] of here) {
    const root = rootOf.get(cwd);
    if (!root) continue;
    // Keyed by the *repository's* identity rather than the tree's: they are the same, since a
    // worktree shares its checkout's origin remote, and going through the root is what makes that
    // true even when the tree itself was never visited and has no key of its own.
    const key = keyOf.get(cwd) ?? localKey(root);
    const p = project(key, basename(root));
    if (p.cwd === null) {
      // A tree whose main checkout is not on the panel's list at all — taken off with `X`, or
      // never added, which is the ordinary state of somebody who only ever works in worktrees. The
      // repository still needs a row for the tree to hang from, and that row is a **real place**:
      // the checkout is on this disk, `↵` can start a conversation in it, and `t` can open a
      // terminal there. Leaving `cwd` null would make it a row whose only verb is folding, about a
      // directory that is sitting right there.
      p.cwd = root;
      p.name = arrangement.name(root) || basename(root);
      p.favorite = arrangement.isFavorite(root);
    }
    p.worktrees.push({
      cwd,
      root,
      branch: list[0]?.branch ?? branchOf.get(cwd),
      sessions: list,
      agents: [],
      fold: cwd,
    });
  }

  // A key that is only a guess on one side — `dir:<name>`, which is what a machine answers until it
  // has read the repository's origin, and what a directory with no remote always answers — meets
  // the one project on the other side with that name. Only when exactly one does: two local
  // `neosh` checkouts is a question this cannot answer, and a wrong merge is worse than two rows.
  // Without it the repository you are sitting in appeared a second time, underneath, as a project
  // on the other machine called the same thing.
  const leaf = (k: string) => k.replace(/\/+$/, "").split(/[/:]/).pop() ?? k;
  const ours = [...projects.values()].filter((p) => p.cwd !== null);
  const alias = (key: string): string => {
    if (projects.has(key)) return key;
    const guessed = key.startsWith("dir:");
    const hits = ours.filter((p) =>
      guessed
        ? leaf(p.key) === key.slice(4) || basename(p.cwd as string) === key.slice(4)
        : p.key.startsWith("dir:") && p.key.slice(4) === leaf(key)
    );
    return hits.length === 1 ? hits[0]!.key : key;
  };

  // And everything on another machine, one block per machine under the same repository row.
  for (const c of over.values()) {
    const node = c.node;
    if (!node) continue;
    const key = alias(keyThere.get(c.fold) ?? `dir:${basename(c.cwd)}`);
    const name = nameThere.get(c.fold) ?? basename(c.cwd);
    // `project_name` is `neosh · fix/thing` for a tree, and a tree never names its repository — so
    // the leading segment is dropped rather than becoming a project called after a branch. The
    // path is the fallback, since a machine that only has trees of a repository still has to put
    // them somewhere.
    const p = project(key, isTree(c) ? basename(c.root ?? c.cwd) : name);
    if (p.name === "") p.name = isTree(c) ? basename(c.root ?? c.cwd) : name;
    if (!p.hosts.includes(node.name)) p.hosts.push(node.name);
    if (!p.nodes.includes(node.id)) p.nodes.push(node.id);
    if (rank(node.link) > rank(p.link)) p.link = node.link;
    let block = p.machines.find((b) => b.node.id === node.id);
    if (!block) {
      block = { node, trees: [], fold: `machine:${node.id}:${key}` };
      p.machines.push(block);
    }
    // Its main checkout is the block's own row; anything else it has is a tree under it — including
    // a second clone of the repository on the same disk, which is at least true about sharing one.
    if (!isTree(c) && !block.main) {
      block.main = c;
      p.mains.push(c);
    } else {
      block.trees.push(c);
    }
  }
  for (const p of projects.values()) {
    p.hosts.sort();
    // By machine name, and each machine's trees by branch: a list you can find a computer in, in
    // the same place every time.
    p.machines.sort((a, b) => a.node.name.localeCompare(b.node.name) || a.node.id.localeCompare(b.node.id));
    for (const b of p.machines) b.trees.sort((x, y) => treeName(x).localeCompare(treeName(y)));
  }

  // ---- order --------------------------------------------------------------
  // A project's recency is its newest conversation *anywhere in it* — a repository whose only
  // activity is in a worktree should float with that work, not sink because its own checkout is
  // quiet.
  const NEVER = Number.MAX_SAFE_INTEGER;
  const newest = (p: Project): number =>
    Math.min(
      p.cwd === null ? NEVER : recency.get(p.cwd) ?? NEVER,
      ...p.worktrees.map((t) => (t.node ? NEVER : recency.get(t.cwd) ?? NEVER)),
    );

  const sort = (a: Project, b: Project) => {
    // A repository that is only on other computers has no rank here to read — and ranking it `0`
    // put every one of them above every project of yours you had never dragged.
    const ra = a.cwd === null ? Number.MAX_SAFE_INTEGER : arrangement.rank(a.cwd);
    const rb = b.cwd === null ? Number.MAX_SAFE_INTEGER : arrangement.rank(b.cwd);
    if (ra !== rb) return ra - rb;
    const fa = newest(a);
    const fb = newest(b);
    return fa !== fb ? fa - fb : a.name.localeCompare(b.name);
  };
  // Ours before theirs inside a repository: a tree on this disk is one you can `↵` into and start
  // work in, and the ones you cannot are the tail of the list rather than mixed through it.
  //
  // Inside our half it is the order a project's own row uses — the rank you dragged it to, then
  // the newest conversation in it, then the name — because `J`/`K` reorder a worktree among its
  // siblings and write exactly that rank. Sorted by name alone, those writes would land on disk
  // and change nothing on screen, which is a key that does its work and appears not to. Their half
  // is by name and nothing else: a directory on somebody else's machine has no rank here to read,
  // and giving it a place in the order would put it between two rows that do.
  const byTree = (a: Checkout, b: Checkout) => {
    const side = Number(Boolean(a.node)) - Number(Boolean(b.node));
    if (side !== 0) return side;
    if (a.node || b.node) return treeName(a).localeCompare(treeName(b));
    const ra = arrangement.rank(a.cwd);
    const rb = arrangement.rank(b.cwd);
    if (ra !== rb) return ra - rb;
    const fa = recency.get(a.cwd) ?? NEVER;
    const fb = recency.get(b.cwd) ?? NEVER;
    return fa !== fb ? fa - fb : treeName(a).localeCompare(treeName(b));
  };

  const all = [...projects.values()];
  for (const p of all) p.worktrees.sort(byTree);
  return [all.filter((p) => p.favorite).sort(sort), all.filter((p) => !p.favorite).sort(sort)];
}

/**
 * Whether each machine's main checkout is the version this one is: the commit HEAD resolves to on
 * each side, read off the files (`git.heads` here, `RemoteProject.head` there).
 *
 * The block is its own row whatever the answer — this only decides whether the row *says* so, and
 * it says so only when it is news: a machine on a different commit shows it beside the branch, and
 * the same commit or a side that cannot tell (an older neosh sends no commit) shows nothing.
 */
function compareVersions(p: Project, mine: GitHead | null | undefined): void {
  const ours = mine?.commit ?? undefined;
  for (const b of p.machines) {
    const theirs = b.main?.head;
    b.differs = ours && theirs ? !(ours.startsWith(theirs) || theirs.startsWith(ours)) : undefined;
  }
}

/**
 * A colour for every other computer, as the palette group it is drawn in — `Swarm.Host1`…`6`.
 *
 * Stable: a machine's preferred colour is a hash of its id, so it does not change when another
 * machine is paired, and only a clash moves one — to the next colour nobody has, in id order. The
 * first colour is the blue, so a workspace with one other computer draws it blue.
 */
function machineColors(peers: SwarmNode[]): Map<string, string> {
  const out = new Map<string, string>();
  const taken = new Set<number>();
  const sorted = [...peers].sort((a, b) => a.info.id.localeCompare(b.info.id));
  // One machine is the blue, whatever its id hashes to: that is the colour a person who has paired
  // one computer was told "connected" is.
  if (sorted.length === 1) {
    out.set(sorted[0]!.info.id, "Swarm.Host1");
    return out;
  }
  for (const n of sorted) {
    let h = 0;
    for (const ch of n.info.id) h = (h * 31 + ch.charCodeAt(0)) >>> 0;
    let i = h % HOST_COLORS;
    for (let tries = 0; tries < HOST_COLORS && taken.has(i); tries++) i = (i + 1) % HOST_COLORS;
    taken.add(i);
    out.set(n.info.id, `Swarm.Host${i + 1}`);
  }
  return out;
}

/** How many machine colours the palette defines. More machines than this share them. */
const HOST_COLORS = 6;

/**
 * A short code for every other computer — `ms` for `mac-studio`, `lb` for `linux-box` — so a row
 * that is somewhere else can say *where* in three columns rather than fourteen.
 *
 * The code is what goes after the `@` on every remote row. The first cut drew a bare `@`, which
 * said "not here" and nothing else: with two machines paired, every remote row looked the same and
 * which computer a conversation was running on was a fact you had to pause on the row to read off
 * the key card. Two letters is the most a row that narrow can spend and the least that tells two
 * machines apart at a glance.
 *
 * Worked out from the name you see — the one `^J` renames — as the initials of its first two words
 * (`MacBook-Pro` counts as three), or the first two letters of a one-word name; a clash takes the
 * next candidate, then a digit. Assigned in a fixed order so a redraw never shuffles them. **Yours
 * wins**: the `swarm.codes` workspace var maps a node id to the code you chose, and those are
 * claimed before anything is worked out.
 */
function machineCodes(peers: SwarmNode[], chosen: unknown): Map<string, string> {
  const out = new Map<string, string>();
  const taken = new Set<string>();
  const wanted = chosen && typeof chosen === "object" ? chosen as Record<string, unknown> : {};
  for (const n of peers) {
    const code = wanted[n.info.id];
    if (typeof code === "string" && /^[A-Za-z0-9]{1,4}$/.test(code)) {
      out.set(n.info.id, code.toLowerCase());
      taken.add(code.toLowerCase());
    }
  }
  const rest = peers
    .filter((n) => !out.has(n.info.id))
    .sort((a, b) => a.info.id.localeCompare(b.info.id));
  for (const n of rest) {
    const code = codeCandidates(n.info.name).find((c) => !taken.has(c)) ??
      // Nine machines called the same thing is somebody testing this function.
      `${n.info.id.slice(0, 2).toLowerCase()}`;
    out.set(n.info.id, code);
    taken.add(code);
  }
  return out;
}

/** What a machine could be called in two columns, best first. */
function codeCandidates(name: string): string[] {
  const words = name
    .replace(/([a-z])([A-Z])/g, "$1 $2")
    .split(/[^A-Za-z0-9]+/)
    .filter((w) => w !== "")
    .map((w) => w.toLowerCase());
  if (words.length === 0) return ["pc"];
  const first = words[0]!;
  const out: string[] = [];
  const add = (c: string) => {
    if (c !== "" && !out.includes(c)) out.push(c);
  };
  // `box2` is `b2`, not `bo`: the number is usually the only thing telling it from `box1`.
  const numbered = /^([a-z]+)(\d+)$/.exec(first);
  if (words.length === 1 && numbered) add(`${numbered[1]![0]}${numbered[2]}`.slice(0, 3));
  if (words.length >= 2) {
    add(`${first[0]}${words[1]![0]}`);
    add(`${first[0]}${words[words.length - 1]![0]}`);
    if (words.length >= 3) add(`${words[1]![0]}${words[2]![0]}`);
  }
  add(first.slice(0, 2));
  for (const w of words.slice(1)) add(`${first[0]}${w.slice(0, 2)}`);
  for (let i = 2; i <= 9; i++) add(`${first[0]}${i}`);
  return out;
}

/**
 * Ask what to call a machine in two or three columns, and write it into `swarm.codes`.
 *
 * Answers whether anything changed. Empty goes back to the worked-out code; anything that is not
 * one to four letters or digits is refused with the reason rather than silently trimmed.
 */
async function chooseCode(
  neosh: Neosh,
  id: string,
  name: string,
  current: string | undefined,
): Promise<boolean> {
  const next = await prompt(neosh, `Code for ${name} — shown as @code on its rows`, {
    initial: current ?? "",
    width: 60,
  });
  if (next === null) return false;
  const code = next.trim().toLowerCase();
  if (code !== "" && !/^[a-z0-9]{1,4}$/.test(code)) {
    neosh.notify("a code is one to four letters or digits", "warn");
    return false;
  }
  const all = await neosh.vars.get<Record<string, string>>({ scope: "global" }, VAR_CODES)
    .catch(() => null);
  const codes: Record<string, string> = { ...(all && typeof all === "object" ? all : {}) };
  if (code === "") delete codes[id];
  else codes[id] = code;
  await neosh.vars.set({ scope: "global" }, VAR_CODES, codes);
  neosh.notify(code === "" ? `${name} is back to its own code` : `${name} is @${code}`);
  return true;
}

/** Whether a checkout is a worktree rather than the main one on its machine. */
function isTree(c: Checkout): boolean {
  return c.root !== undefined && c.root !== c.cwd;
}

/** What a tree's row says it is: the branch, or the directory on a detached head. */
function treeName(c: Checkout): string {
  return c.branch ?? basename(c.cwd);
}

/**
 * How reachable a link is, for picking the best of several.
 *
 * A project on two computers, one up and one not, is a project you can reach — and the row that
 * says otherwise is one you would not press.
 */
function rank(l: LinkState | undefined): number {
  return l?.state === "up" ? 3 : l?.state === "waiting" ? 2 : l?.state === "down" || !l ? 0 : 1;
}

interface DrawOptions {
  width: number;
  ascii: boolean;
  hints: boolean;
  focused: boolean;
  selected: Target | undefined;
  /** The verbs other plugins put on our rows, for the key card. */
  actions: BoundAction[];
  /** Which conversations are waiting on an answer from you. See [`VAR_ASKING`]. */
  asking: Set<string>;
  /** Marks other plugins put on our rows, by [`targetKey`]. See [`POINT_DECORATION`]. */
  decorations: Map<string, Decoration>;
  /** A count typed but not yet spent, drawn at the foot the way Vim draws it. */
  count: string;
  /**
   * Whether a row is folded shut, by its fold key.
   *
   * A function rather than the {@link Arrangement} itself, because a fold key is not always a
   * directory: a repository that is only on other machines folds on its project key, and a
   * worktree over there on `<machine>:<path>`. What the row builders need is the question, and
   * passing the whole arrangement to answer it invited them to reach for the rest of it.
   */
  folded: (key: string) => boolean;
  /**
   * Where the link to each machine stands, by node id.
   *
   * Read on every remote row, because that is what the marker on it is coloured by. A row about a
   * machine this one cannot currently reach is not the same row as one about a machine it can —
   * you can open and steer the second and only read about the first — and until this existed the
   * panel drew both identically, which meant `↵` on a row was a coin toss you could not see.
   */
  links: Map<string, LinkState>;
  /** Each other computer's short code, by node id — what follows the `@`. See {@link machineCodes}. */
  codes: Map<string, string>;
  /** Each other computer's colour, as a palette group, by node id. See {@link machineColors}. */
  colors: Map<string, string>;
  /** The conversation on another machine this terminal is in, when it is in one. */
  mirrored?: { node: string; session: string };
}

/** How a project's fold is remembered: its directory here, or its identity when it is only there. */
function foldOf(p: Project): string {
  return p.cwd ?? p.key;
}

/**
 * The waiting set, out of whatever was in the var.
 *
 * Checked rather than trusted, like every contribution: this is JSON written by a plugin this one
 * has never heard of, and a panel that throws on it is a panel a third party can take off the screen
 * by writing a string where a list goes.
 */
function asked(value: unknown): Set<string> {
  if (!Array.isArray(value)) return new Set();
  return new Set(value.filter((v): v is string => typeof v === "string"));
}

async function collect(
  neosh: Neosh,
  arrangement: Arrangement,
  opts: DrawOptions,
): Promise<{ rows: ListRow<Target>[]; running: boolean; pinned: number }> {
  const rows: ListRow<Target>[] = [];

  // Failures absorbed: a panel that renders an exception instead of your conversations is worse
  // than one showing less.
  const all = await neosh.session
    .list({ includeArchived: true })
    .catch(() => [] as SessionInfo[]);
  const sessions = all.filter((s) => !s.archived);
  let running = sessions.some((s) => s.active_turn);
  const now = Date.now();

  // What the other computers are running, and how each of them is reachable.
  //
  // Built from `nodes()` rather than `agents()`, which is the same list minus the machines that
  // are *not* up. That filter is right for "what can I act on" and wrong for a panel: a machine
  // going quiet does not take its conversations off your screen, it takes them out of reach, and
  // those are different rows. Drawing only the reachable ones meant a link dropping silently
  // shortened the panel, which is the one thing a list must never do — you cannot tell it from a
  // list that never had them. Now every one of them is drawn and the marker beside it says which.
  //
  // One call; empty and harmless on a single machine.
  const me = (await neosh.swarm.self().catch(() => null))?.id ?? null;
  const peers = (await neosh.swarm.nodes().catch(() => [] as SwarmNode[]))
    .filter((n) => n.info.id !== me);
  // A turn over there has a clock on its row too, and a clock that moves every few seconds is one
  // that looks stuck.
  running ||= peers.some((n) => n.link.state === "up" && n.agents.some(turning));

  // Marks other plugins put on our rows. Read every frame for the reason sections are: a
  // decorator re-contributes in place when its data changes, and the read is one call.
  const decorations = mergeDecorations(
    await neosh.ext.list<DecorationItem>(POINT_DECORATION).catch(() => []),
  );
  const links = new Map(peers.map((n) => [n.info.id, n.link] as const));
  const codes = machineCodes(
    peers,
    await neosh.vars.get({ scope: "global" }, VAR_CODES).catch(() => null),
  );
  const colors = machineColors(peers);
  // Which remote conversation is on screen here, so its row is lit the way yours is when you are in
  // it: where you are is the one thing the panel must never leave you to work out.
  const current = await neosh.session.current().catch(() => null);
  const mirrored = current?.mirror
    ? { node: current.mirror.node, session: current.mirror.session }
    : undefined;
  opts = {
    ...opts,
    decorations,
    links,
    codes,
    colors,
    mirrored,
    folded: (key) => arrangement.isFolded(key),
  };
  const projects = group(sessions, arrangement, peers).flat();
  // Which version of each repository this machine has, for the ones another machine also has a
  // main checkout of — one call for all of them, answered off the disk with no `git` process, and
  // nothing at all on a workspace with no peers.
  const local = projects
    .filter((p) => p.cwd !== null && p.mains.length > 0)
    .map((p) => p.cwd as string);
  const heads = await neosh.git.heads(local).catch(() => [] as Array<GitHead | null>);
  const mine = new Map(local.map((cwd, i) => [cwd, heads[i] ?? null] as const));
  for (const p of projects) {
    if (p.cwd !== null && p.mains.length > 0) compareVersions(p, mine.get(p.cwd));
  }

  // A directory that turned up in the conversation list and we had not seen before. Noted rather
  // than fetched inline: the draw runs on a tick and must not wait on a round trip per project.
  void arrangement.note(all.map((s) => s.cwd));
  // And what the host calls it, which checkout a tree hangs off, and what it is called across
  // machines — so the row still says all three once the last conversation in it has gone.
  arrangement.remember([
    ...projects
      .filter((p) => p.cwd !== null && p.sessions.length > 0)
      .map((p) => ({ cwd: p.cwd as string, name: p.name, key: p.key })),
    ...projects.flatMap((p) =>
      p.worktrees
        .filter((t) => !t.node && t.sessions.length > 0)
        .map((t) => ({
          cwd: t.cwd,
          name: treeName(t),
          root: t.root,
          key: t.sessions[0]?.project_key || p.key,
        }))
    ),
  ]);

  // Rows other plugins own. Read every frame rather than cached, because a contribution is replaced
  // in place when its author's data changes and re-reading is one call.
  const sections = await neosh.ext.list<SectionItem>(POINT_SECTION).catch(() => []);
  const order = placeSections(SLOTS, sections);
  const section = (c: Contribution & { item: SectionItem }) =>
    rows.push(...contributedRows<Target>(c, {
      width: opts.width,
      custom: (command, args, id) => ({ kind: "custom", command, args, section: id }),
    }));

  /** One machine's block under a repository: its row, then its conversations and its worktrees. */
  const pushBlock = (b: MachineBlock) => {
    rows.push(machineRow(b, opts, now));
    if (arrangement.isFolded(b.fold)) return;
    const rail = { hl: railOf(b.node.id, opts), at: 3 };
    for (const r of b.main?.agents ?? []) rows.push(remoteRow(r, opts, now, 1, rail));
    for (const t of b.trees) {
      rows.push(treeRow(t, opts, now, 1, rail));
      if (arrangement.isFolded(t.fold)) continue;
      for (const r of t.agents) rows.push(remoteRow(r, opts, now, 2, rail));
    }
  };

  // The blocks this panel draws, each the same shape: so that a section can sit before or after
  // any of them by name, and the foot is whatever lands after the last one.
  const blocks: Record<Slot, () => void> = {
    projects: () => {
      // How to make the column wider, on the heading of the column it makes wider — the idiom the
      // plan block already uses for `^L`, and it costs no row of a panel whose rows are its whole
      // job. It has to be *somewhere*: a project name clipped to `neosh-websit…` is a row asking
      // to be widened, and `>` was bound, documented and invisible — advertised on contributed
      // rows only, which is the one kind of row most people never stand on. Only while the panel
      // has the keyboard, because that is when the key does anything.
      // The key on the heading is the one that gets you in, and then the one that gets you out:
      // a key beside the thing it acts on, which is where a key is found rather than learned.
      rows.push(...heading("PROJECTS", opts.width, opts.focused ? "esc" : "^T"));
      // Yours, and only yours: a repository with a checkout on this disk. What is only on another
      // computer is in that computer's section, below — see `machines`.
      for (const p of projects.filter((p) => p.cwd !== null)) {
        rows.push(projectRow(p, opts, now));
        if (arrangement.isFolded(foldOf(p))) continue;
        // Its own conversations, then the ones in a main checkout somewhere else. Under the same
        // row and in the same column, because they are the same project being worked on: a
        // separate block would make you check two places for one repository, and that is the whole
        // complaint this answers.
        for (const s of p.sessions) rows.push(sessionRow(s, now, opts));
        // Its worktrees on this machine, inside it. Each is a checkout row one level down, because a
        // worktree of a repository is not a neighbour of the repository and the column should say so.
        for (const t of p.worktrees) {
          rows.push(treeRow(t, opts, now));
          if (arrangement.isFolded(t.fold)) continue;
          for (const s of t.sessions) rows.push(sessionRow(s, now, opts, 1));
        }
        // Every other machine that has it, as a block of its own: its row, then its conversations
        // and its worktrees under a rail in its colour — see {@link MachineBlock}.
        for (const b of p.machines) pushBlock(b);
        if (p.sessions.length === 0 && p.worktrees.length === 0 && p.machines.length === 0) {
          rows.push({ text: "     nothing here yet", hl: "Sidebar.Dim", inert: true });
        }
      }
    },
    add: () => {
      rows.push(blank());
      rows.push({
        text: " + Add project",
        hl: "Accent",
        // The key for this row, on this row. `^O` from the composer; `o` from anywhere in the
        // panel, which is the same question asked from nearer by.
        right: { text: `${opts.focused ? "o" : "^O"} `, hl: "Sidebar.Key" },
        value: { kind: "add" },
      });
    },
    // Every other computer, one section each, in its own colour: a heading that says which machine
    // and whether it can be reached, and under it the repositories that are only over there.
    //
    // They used to be mixed in with yours — ranked above them, even, since a project with no rank
    // here sorted as `0` — each carrying `@st` on its row and then again on a block row under it
    // that said the same thing. What it looked like was a column where you could not tell at a
    // glance which rows were yours. So yours are yours, up top, and a machine is a place: its name,
    // its colour, and what is there.
    machines: () => {
      const elsewhere = projects.filter((p) => p.cwd === null);
      for (const node of [...peers].sort((a, b) => a.info.name.localeCompare(b.info.name))) {
        const id = node.info.id;
        const mine = elsewhere
          .filter((p) => bestMachine(p, opts)?.node.id === id)
          .sort((a, b) => lastMoved(b) - lastMoved(a) || a.name.localeCompare(b.name));
        const fold = `host:${id}`;
        rows.push(blank());
        rows.push(...machineHeading(node, mine.length, opts));
        if (arrangement.isFolded(fold)) continue;
        for (const p of mine) {
          rows.push(projectRow(p, opts, now, id));
          if (arrangement.isFolded(foldOf(p))) continue;
          if (p.machines.length === 1) {
            // One machine: the project row *is* the machine's checkout, so its conversations and
            // worktrees hang straight off it, on a rail under its arrow.
            const b = p.machines[0]!;
            const rail = { hl: railOf(b.node.id, opts), at: 1 };
            for (const r of b.main?.agents ?? []) rows.push(remoteRow(r, opts, now, 0, rail));
            for (const t of b.trees) {
              rows.push(treeRow(t, opts, now, 0, rail));
              if (arrangement.isFolded(t.fold)) continue;
              for (const r of t.agents) rows.push(remoteRow(r, opts, now, 1, rail));
            }
          } else {
            // Several: one block each, exactly as under a project of yours.
            for (const b of p.machines) pushBlock(b);
          }
        }
      }
    },
    // No `archived` block any more: what you have put away is the archive plugin's panel, and its
    // row in this column arrives as a `sidebar.section` contribution like any third party's.
  };

  // Everything after the last of our own blocks is the panel's foot: rows somebody contributed
  // below the list, and the key strip. Counted so the list can hold them against the bottom edge —
  // a plan gauge that sits under the last project is in a different place every time a project is
  // added or folded, which is the one thing a status strip must not be.
  let body = 0;
  for (const entry of order) {
    if (typeof entry === "string") {
      blocks[entry]();
      body = rows.length;
    } else {
      section(entry);
    }
  }

  if (opts.hints) rows.push(...hints(opts));

  return { rows, running, pinned: rows.length - body };
}

/** A section heading, with a rule under it — what turns a column of text into sections. */
function heading(text: string, width: number, hint?: string): ListRow<Target>[] {
  return [
    {
      text: ` ${text}`,
      hl: "Sidebar.Heading",
      // On the heading rather than beside the title: it is an answer to "and then what", which is
      // a question you ask after reading the section, not while finding it.
      right: hint ? { text: `${hint} `, hl: "Sidebar.Key" } : undefined,
      inert: true,
    },
    { text: "─".repeat(Math.max(1, width)), hl: "Separator", inert: true },
  ];
}

function blank(): ListRow<Target> {
  return { text: "", inert: true };
}

/**
 * A repository, wherever its checkouts are.
 *
 * One row per repository, and everything under it: its conversations here, its conversations on
 * another machine, and every worktree of it anywhere. What used to be two rows — yours, and then
 * `neosh · fix/thing` further down as a project you had apparently never heard of — is one, because
 * they were always one repository and the panel was matching them on a path that is different on
 * every computer.
 */
function projectRow(
  p: Project,
  opts: DrawOptions,
  now: number,
  /** The machine whose section this is drawn in, which therefore goes without saying on the row. */
  under?: string,
): ListRow<Target> {
  const fold = foldOf(p);
  const folded = opts.folded(fold);
  const arrow = opts.ascii ? (folded ? ">" : "v") : folded ? "▸" : "▾";
  // "Inside it" is everything the fold hides, on every machine: a repository whose only running
  // turn is in a scratch tree on the build box is still a repository where something is happening,
  // and the folded count has to count what folding hid.
  const within = [...p.sessions, ...p.worktrees.flatMap((t) => t.sessions)];
  const outside = p.machines.flatMap(blockAgents);
  const busy = within.find((s) => s.active_turn);
  const busyThere = outside.find((r) => turning(r.agent));
  const here = p.sessions.some((s) => s.is_active);
  // Something in here has stopped and is waiting on an answer. It is still a turn in flight, so
  // `busy` finds it too — this is what decides which of the two the row says. Over everything
  // inside for the reason the count is: a question asked in a scratch tree of this repository is a
  // question in this repository, and folding is what hid the row that would otherwise say so.
  const waiting = within.some((s) => opts.asking.has(s.id)) ||
    outside.some((r) => r.agent.state === "blocked");
  const total = within.length + outside.length;

  // The count is the useful thing when a project is folded, and the elapsed time is the useful
  // thing when something inside it is working. Never both — there is one column. A turn running on
  // another machine counts, and reads exactly as one running here: it is the same work.
  const right = busy
    ? { text: `${turnFor(busy, now)} `, hl: waiting ? "Status.Pending" : "Status.Working" }
    : busyThere?.agent.turn_started_at
    ? {
      text: `${elapsed(Math.max(0, now - busyThere.agent.turn_started_at * 1000))} `,
      hl: waiting ? "Status.Pending" : "Status.Working",
    }
    : total > 0
    ? { text: `${total} `, hl: "Sidebar.Dim" }
    : { text: "" };

  // The star sits directly after the name, and costs nothing on a project that has not got one.
  // It used to be a fixed column *before* the fold arrow, which meant every project name in the
  // narrowest panel in the workspace paid two columns, permanently, for a mark on three or four
  // rows — and the right-hand edge is not the answer either: that column already belongs to the
  // count and the elapsed time, which is a number you read against the rows above and below it.
  // Attached to the name it is a property of the thing it is beside, which is what it is. It keeps
  // its own highlight because the world has already decided what colour a favourite is — a grey
  // star is a star you have to decode.
  //
  // A star rather than a heart, and the reason is font fallback: `♥` is U+2665, which Unicode
  // classifies as an emoji even though its default presentation is text. A terminal that has a
  // colour-emoji font installed routes it there, and what comes back is somebody else's artwork
  // at somebody else's weight — commonly an outline, which is what a filled glyph is not. Every
  // heart codepoint has that problem. `★` is U+2605, `Emoji=No`, so no terminal has any reason to
  // leave the font it is drawing the rest of the row in.
  const star = p.favorite ? (opts.ascii ? " *" : " ★") : "";

  // Which other computers have this repository, in one column. The whole reason a project key is a
  // normalised git remote rather than a path: on two machines the path is different and this is
  // the same.
  //
  // Drawn only when this repository is **not** here as well, and that is the change. A marker on
  // every row that is partly elsewhere is a marker on most rows in a swarm of two machines, which
  // is a column spent saying something that is true of everything — and the panel's own rule is
  // that what is true of every row does not earn a column on each of them. Where it *is* news is a
  // repository you have not cloned here at all, which is the row you would otherwise press `↵` on
  // expecting your own files. The conversations underneath say which of them are where.
  // Which computer, by its code, and how many more when it is on several: the best-connected one
  // is named because that is the one `↵` and `n` would reach.
  // In a machine's own section that machine goes without saying; what is still news is any *other*
  // machine that has it too.
  const others = p.nodes
    .filter((id) => id !== under)
    .map((id) => ({ id, link: opts.links.get(id) }))
    .sort((a, b) => rank(b.link) - rank(a.link) || a.id.localeCompare(b.id));
  const best = others[0];
  const sky = p.cwd === null && best
    ? marker(best.link, opts.codes.get(best.id), others.length - 1, opts.colors.get(best.id))
    : null;
  // A repository that is only on one other machine says which branch that checkout is on, dim,
  // after its name — the one fact about it the block row used to carry. Only when it is not the
  // name again, which for a checkout named after its branch it would be.
  const only = p.cwd === null && p.machines.length === 1 ? p.machines[0]?.main?.branch : undefined;
  const on = only && only !== p.name ? `${opts.ascii ? " " : " ⎇ "}${only}` : "";

  // What is finished and unseen inside a project you have folded shut. Only when folded, because
  // folding is the thing that hid it: with the project open the conversation says so on its own
  // row, and saying it twice on two adjacent lines is how a panel teaches you to stop reading it.
  //
  // It sits after the name rather than in the right-hand column: that column is already spoken for
  // by the elapsed time of whatever is running, and a project can perfectly well have one turn
  // still going and another that finished an hour ago.
  // A question in there outranks it, and for the same reason it does on the conversation's own row:
  // one of them is news that will keep, and the other is a turn that has stopped until you answer.
  // Only one mark, because there is one column and two would be a puzzle rather than a summary.
  // A third thing the fold can be hiding, and it sits between the other two: a question stops the
  // workspace until you answer, a killed turn lost work, and an unread answer is waiting patiently.
  // Over there as much as here: a repository is one row whichever machine the work is on.
  const cut = folded && !waiting
    ? within.filter((s) => !s.active_turn && s.interrupted).length +
      outside.filter((r) => !turning(r.agent) && r.agent.interrupted).length
    : 0;
  const unseen = folded && !waiting && cut === 0
    ? within.filter((s) => !s.active_turn && s.unread).length +
      outside.filter((r) => !turning(r.agent) && r.agent.unread).length
    : 0;
  // Fourth and last rung: a folded project hiding a conversation that is still running something.
  // Below all three, because a question stops the workspace, a killed turn lost work, an unread
  // answer is waiting — and this is none of those. Counted over the whole group, conversations
  // included, because folded is exactly the state where the rows that know are the ones you
  // cannot see.
  const busyBg = folded && !waiting && cut === 0 && unseen === 0
    ? within.filter((s) => !s.active_turn && (s.background?.length ?? 0) > 0).length +
      outside.filter((r) => !turning(r.agent) && (r.agent.background ?? 0) > 0).length
    : 0;
  // Whichever rung is being reported, drawn the same way and told apart by the glyph and the
  // colour. One column, one mark — two would be a puzzle rather than a summary.
  const [count, dot, dotHl] = cut > 0
    ? [cut, opts.ascii ? "x" : "✗", "Diagnostic.Error"]
    : unseen > 0
    ? [unseen, opts.ascii ? "!" : "●", "Status.Unread"]
    // Hollow where the other two are solid: the difference between something here for you and
    // something here still happening.
    : [busyBg, opts.ascii ? "o" : "○", "Status.Monitoring"];
  const mark = folded && waiting
    ? " ?"
    : count === 0
    ? ""
    : count === 1
    ? ` ${dot}`
    : ` ${dot}${count}`;
  const markHl = folded && waiting ? "Status.Pending" : dotHl;

  // The star's two columns come off the name that is about to carry it, so a favourite and the
  // project under it still end in the same place — clipping the name is what a panel this narrow
  // does, and letting the mark run two columns past everything else is not.
  const target: Target = p.cwd === null
    // Only on other computers: a row of the same shape with different verbs behind it, because
    // every verb bound against a project — fold, rename, archive, delete, remove from the list —
    // is about a directory on *this* disk, and pointing one at a path that is not here is how a
    // key deletes the wrong thing.
    ? {
      kind: "elsewhere",
      key: p.key,
      name: p.name,
      machines: machinesOf(p),
      link: linkWords(p.link),
    }
    : { kind: "project", cwd: p.cwd, ...(p.hosts.length > 0 ? { hosts: p.hosts } : {}) };
  // Everything the name and the badge have to share. The badge gives up its least important parts
  // before a single character comes off the name — a project you cannot read the name of is not a
  // row you can use, and how far behind the remote it is keeps until you widen the panel with `>`.
  const room = opts.width - 8 - byteLength(mark) - byteLength(star) - (sky ? 1 + width(sky.text) : 0);
  // The branch is the first thing to give way — it is context, and the name is the row.
  const branchFits = on !== "" && Array.from(p.name).length + width(on) <= room;
  const on_ = branchFits ? on : "";
  // A repository only over there wears what that machine's panel puts on it — the machine whose
  // section this is, or the one `↵` would reach.
  const theirs = p.cwd === null
    ? (p.machines.find((b) => b.node.id === under) ?? bestMachine(p, opts))?.main?.decorations
    : undefined;
  const decoration = fitBadge(
    p.cwd === null ? carried(theirs) : opts.decorations.get(targetKey(target) ?? ""),
    room,
    Array.from(p.name).length,
  );
  const name = clip(p.name, Math.max(6, room - badgeColumns(decoration) - width(on_)));
  const spans: Array<{ from: number; to: number; hl: string }> = [];
  if (star !== "") {
    const from = byteLength(` ${arrow} ${name}`);
    spans.push({ from, to: from + byteLength(star), hl: "Sidebar.Favorite" });
  }
  if (on_ !== "") {
    const from = byteLength(` ${arrow} ${name}${star}`);
    spans.push({ from, to: from + byteLength(on_), hl: "Sidebar.Dim" });
  }
  if (mark !== "") {
    const from = byteLength(` ${arrow} ${name}${star}${on_}`);
    spans.push({ from, to: from + byteLength(mark), hl: markHl });
  }
  const sky_ = sky ? ` ${sky.text}` : "";
  if (sky) {
    const from = byteLength(` ${arrow} ${name}${star}${on_}${mark} `);
    spans.push({ from, to: from + byteLength(sky.text), hl: sky.hl });
  }
  return decorateRow({
    text: ` ${arrow} ${name}${star}${on_}${mark}${sky_}`,
    // A project's name is a directory name, and directory names are long. Clipped it is the same
    // eight characters as the three others you have open beside it, which is the panel failing at
    // the one thing it is for. The star and the unread dot are left off deliberately: they
    // survived the clip and are already on the row.
    full: ` ${arrow} ${p.name}`,
    indent: 3,
    // `here` is this panel's opinion; everything else is a decorator's to colour.
    hl: here ? "Directory" : decoration?.hl ?? "Sidebar.Dim",
    spans: spans.length > 0 ? spans : undefined,
    right,
    value: target,
  }, decoration, Boolean(busy) || Boolean(busyThere));
}

/** Every machine a repository is on, and where on each — enough to start one without asking again. */
function machinesOf(p: Project): Array<{ id: string; name: string; cwd: string }> {
  const out: Array<{ id: string; name: string; cwd: string }> = [];
  const add = (c: Checkout) => {
    if (!c.node || out.some((m) => m.id === c.node?.id)) return;
    out.push({ id: c.node.id, name: c.node.name, cwd: c.cwd });
  };
  // The main checkouts first, and from the *checkouts* rather than from the conversations in them:
  // `n` on a repository row means a conversation in the repository, and a repository nobody has
  // started work in yet is exactly the row that has conversations to read nothing from.
  for (const b of p.machines) {
    const c = b.main ?? b.trees[0];
    if (c) add(c);
  }
  return out;
}

/**
 * A checkout that is not the main one here: a worktree on this machine, or one on another machine
 * inside that machine's block.
 *
 * One level inside what it belongs to, and the same row whichever machine it is on — same arrow,
 * same branch glyph, same name, same count — because it is the same kind of thing. `rail` is the
 * group of the machine's block when it is inside one: the `│` down the left and the `⎇` wear it, so
 * a worktree over there is the colour of the computer it is on.
 */
function treeRow(
  c: Checkout,
  opts: DrawOptions,
  now: number,
  depth = 0,
  rail?: Rail,
): ListRow<Target> {
  const folded = opts.folded(c.fold);
  const arrow = opts.ascii ? (folded ? ">" : "v") : folded ? "▸" : "▾";
  const busy = c.sessions.find((s) => s.active_turn);
  const busyThere = c.agents.find((r) => turning(r.agent));
  const here = c.sessions.some((s) => s.is_active);
  const total = c.sessions.length + c.agents.length;
  const waiting = c.sessions.some((s) => opts.asking.has(s.id)) ||
    c.agents.some((r) => r.agent.state === "blocked");
  const right = busy
    ? { text: `${turnFor(busy, now)} `, hl: waiting ? "Status.Pending" : "Status.Working" }
    : busyThere?.agent.turn_started_at
    ? {
      text: `${elapsed(Math.max(0, now - busyThere.agent.turn_started_at * 1000))} `,
      hl: waiting ? "Status.Pending" : "Status.Working",
    }
    : total > 0
    ? { text: `${total} `, hl: "Sidebar.Dim" }
    : { text: "" };
  const cut = folded && !waiting
    ? c.sessions.filter((s) => !s.active_turn && s.interrupted).length +
      c.agents.filter((r) => !turning(r.agent) && r.agent.interrupted).length
    : 0;
  const unseen = folded && !waiting && cut === 0
    ? c.sessions.filter((s) => !s.active_turn && s.unread).length +
      c.agents.filter((r) => !turning(r.agent) && r.agent.unread).length
    : 0;
  const mark = folded && waiting
    ? " ?"
    : cut > 0
    ? cut === 1
      ? opts.ascii ? " x" : " ✗"
      : opts.ascii ? ` x${cut}` : ` ✗${cut}`
    : unseen === 0 ? "" : unseen === 1
    ? opts.ascii ? " !" : " ●"
    : opts.ascii ? ` !${unseen}` : ` ●${unseen}`;
  // The branch glyph — what says "this row is a checkout" at a glance, so the name can be just the
  // branch. In the branch colour here, and in the machine's inside another machine's block. No
  // ASCII stand-in earns its column, so ASCII goes without.
  const glyph = opts.ascii ? "" : "⎇ ";
  const link = c.node ? opts.links.get(c.node.id) ?? c.node.link : undefined;
  // A tree over there that is somehow not inside a block still says where it is. Inside one, the
  // block's row already has, and the rail says it on every row after.
  const sky = c.node && !rail ? marker(link, opts.codes.get(c.node.id), 0, opts.colors.get(c.node.id)) : null;
  const pad = indentFor(depth, rail);
  const label = treeName(c);
  const target: Target = c.node
    ? {
      kind: "elsewhere",
      key: c.fold,
      name: label,
      machines: [{ id: c.node.id, name: c.node.name, cwd: c.cwd }],
      link: linkWords(link),
    }
    : { kind: "project", cwd: c.cwd };
  // As on a project row, and two columns tighter: the branch name is what this row is, so the
  // badge is what gives way. See `projectRow`.
  const room = opts.width - 8 - (width(pad) - 3) - byteLength(glyph) - byteLength(mark) -
    (sky ? 1 + width(sky.text) : 0);
  const decoration = fitBadge(
    c.node ? carried(c.decorations) : opts.decorations.get(targetKey(target) ?? ""),
    room,
    Array.from(label).length,
  );
  const name = clip(label, Math.max(6, room - badgeColumns(decoration)));
  const spans: Array<{ from: number; to: number; hl: string }> = [...railSpan(pad, rail)];
  if (glyph !== "") {
    const at = byteLength(`${pad}${arrow} `);
    spans.push({ from: at, to: at + byteLength(glyph), hl: rail?.hl ?? "Git.Branch" });
  }
  if (mark !== "") {
    const at = byteLength(`${pad}${arrow} ${glyph}${name}`);
    spans.push({
      from: at,
      to: at + byteLength(mark),
      hl: folded && waiting ? "Status.Pending" : cut > 0 ? "Diagnostic.Error" : "Status.Unread",
    });
  }
  const sky_ = sky ? ` ${sky.text}` : "";
  if (sky) {
    const at = byteLength(`${pad}${arrow} ${glyph}${name}${mark} `);
    spans.push({ from: at, to: at + byteLength(sky.text), hl: sky.hl });
  }
  return decorateRow({
    text: `${pad}${arrow} ${glyph}${name}${mark}${sky_}`,
    // A branch name is as long as somebody made it, and this row is two columns narrower than a
    // project's. The unread mark is left off the unfolded form: it survived the clip and is
    // already on the row.
    full: `${pad}${arrow} ${glyph}${label}`,
    indent: byteLength(`${pad}${arrow} `),
    // A checkout you can reach reads like one of ours; only one on a machine that cannot be reached
    // right now is greyed, because that is the one you cannot act on.
    hl: here
      ? "Directory"
      : c.node
      ? link?.state === "up" ? "Sidebar.Dim" : "Sidebar.Remote"
      : decoration?.hl ?? "Sidebar.Dim",
    spans: spans.length > 0 ? spans : undefined,
    right,
    value: target,
  }, decoration, Boolean(busy) || Boolean(busyThere));
}

/**
 * The machine a repository that is only elsewhere belongs to in the column — the one `↵` and `n`
 * would reach: connected before not, then by name, so it does not move between sections as links
 * come and go on the others.
 */
function bestMachine(p: Project, opts: DrawOptions): MachineBlock | undefined {
  return [...p.machines].sort((a, b) =>
    rank(opts.links.get(b.node.id)) - rank(opts.links.get(a.node.id)) ||
    a.node.name.localeCompare(b.node.name)
  )[0];
}

/** When anything in a repository over there last moved, for ordering a machine's section. */
function lastMoved(p: Project): number {
  return Math.max(0, ...p.machines.flatMap(blockAgents).map((r) => r.agent.updated_at));
}

/**
 * A machine's section heading: `@st STARFIGHTER`, and a rule under it in the machine's colour.
 *
 * The same shape as `PROJECTS` above it — a heading and a rule — because it is the same kind of
 * thing: a place, and what is there. The code and the rule wear the machine's colour, so the
 * section is found by colour before it is read, and they go dim together when it cannot be reached;
 * the right-hand column says why in a word. It is a row you can stand on: `↵` folds the whole
 * section, and `c`, `C`, `t` and `n` act on the machine.
 */
function machineHeading(node: SwarmNode, projects: number, opts: DrawOptions): ListRow<Target>[] {
  const id = node.info.id;
  const link = opts.links.get(id) ?? node.link;
  const code = marker(link, opts.codes.get(id), 0, opts.colors.get(id));
  const folded = opts.folded(`host:${id}`);
  const state = link?.state === "up"
    ? folded && projects > 0 ? `${projects}` : ""
    : link?.state === "waiting"
    ? "not allowed yet"
    : link?.state === "connecting" || link?.state === "retrying"
    ? "connecting"
    : "offline";
  const lead = ` ${code.text} `;
  const room = Math.max(4, opts.width - width(lead) - (state === "" ? 1 : width(state) + 2));
  const name = clip(node.info.name.toUpperCase(), room);
  return [
    {
      text: `${lead}${name}`,
      full: `${lead}${node.info.name.toUpperCase()}`,
      indent: 1,
      hl: "Sidebar.Heading",
      spans: [{ from: 1, to: 1 + byteLength(code.text), hl: code.hl }],
      right: { text: state === "" ? "" : `${state} `, hl: link?.state === "up" ? "Sidebar.Dim" : code.hl },
      value: {
        kind: "elsewhere",
        key: `host:${id}`,
        name: node.info.name,
        machines: [{ id, name: node.info.name, cwd: "" }],
        link: linkWords(link),
      },
    },
    { text: "─".repeat(Math.max(1, opts.width)), hl: railOf(id, opts), inert: true },
  ];
}

/**
 * The row a machine's block hangs from: `@ms ⎇ main`, in that machine's colour.
 *
 * Named like a worktree — by its branch — with the machine's code in front, because *which
 * computer* is the first thing to know about everything under it. When its main checkout is on a
 * different commit from ours the commit goes beside the branch, after a `≠`, since the branch
 * alone would leave you assuming they are the same; when they match, or either side cannot say,
 * nothing is added — a mark that is always there is a mark you stop seeing. The machine's full name
 * follows when the column has room for it. A machine with only worktrees of the repository is named
 * by its name rather than a branch.
 */
function machineRow(b: MachineBlock, opts: DrawOptions, now: number): ListRow<Target> {
  const folded = opts.folded(b.fold);
  const arrow = opts.ascii ? (folded ? ">" : "v") : folded ? "▸" : "▾";
  const link = opts.links.get(b.node.id) ?? b.node.link;
  const up = link?.state === "up";
  const code = marker(link, opts.codes.get(b.node.id), 0, opts.colors.get(b.node.id));
  const railHl = railOf(b.node.id, opts);
  const agents = blockAgents(b);
  const busy = agents.find((r) => turning(r.agent));
  const waiting = up && agents.some((r) => r.agent.state === "blocked");
  const right = busy?.agent.turn_started_at
    ? {
      text: `${elapsed(Math.max(0, now - busy.agent.turn_started_at * 1000))} `,
      hl: waiting ? "Status.Pending" : "Status.Working",
    }
    : agents.length > 0
    ? { text: `${agents.length} `, hl: "Sidebar.Dim" }
    : { text: "" };
  // What folding the block hid, as a project row says it: a question, then a turn that was cut
  // off, then an answer nobody has read.
  const settled = agents.filter((r) => !turning(r.agent));
  const cut = folded && up && !waiting ? settled.filter((r) => r.agent.interrupted).length : 0;
  const unseen = folded && up && !waiting && cut === 0
    ? settled.filter((r) => r.agent.unread).length
    : 0;
  const [count, dot, dotHl] = cut > 0
    ? [cut, opts.ascii ? "x" : "✗", "Diagnostic.Error"]
    : [unseen, opts.ascii ? "!" : "●", "Status.Unread"];
  const mark = folded && waiting
    ? " ?"
    : count === 0
    ? ""
    : count === 1
    ? ` ${dot}`
    : ` ${dot}${count}`;
  const markHl = folded && waiting ? "Status.Pending" : dotHl;
  // Its main checkout's marks, as that machine's panel draws them on the row for it.
  const decoration = carried(b.main?.decorations);
  // Named by its branch when it said one. Its directory's name is not a branch, and put where a
  // branch goes it read as one — `⎇ neosh` for a checkout nobody had asked the branch of.
  const branch = b.main?.branch;
  const glyph = branch && !opts.ascii ? "⎇ " : "";
  const what = branch ?? b.node.name;
  const commit = b.differs && b.main?.head
    ? `${opts.ascii ? " != " : " ≠ "}${b.main.head.slice(0, 7)}`
    : "";
  const lead = `   ${arrow} `;
  const head = `${lead}${code.text} ${glyph}`;
  const room = Math.max(
    6,
    opts.width - width(head) - width(commit) - width(mark) -
      (right.text === "" ? 1 : width(right.text) + 1),
  );
  const fitted = fitBadge(decoration, room, Array.from(what).length);
  const name = clip(what, Math.max(6, room - badgeColumns(fitted)));
  // The machine's own name, when it is not already the label and there is room for all of it.
  // Not beside a badge: the badge is about the branch, and goes directly after it.
  const left = room - width(name) - badgeColumns(fitted);
  const named = branch && !fitted?.badge && left >= width(b.node.name) + 2
    ? `  ${b.node.name}`
    : "";
  const spans: Array<{ from: number; to: number; hl: string }> = [];
  let at = byteLength(lead);
  spans.push({ from: at, to: at + byteLength(code.text), hl: code.hl });
  at = byteLength(`${head}`) - byteLength(glyph);
  if (glyph !== "") spans.push({ from: at, to: at + byteLength(glyph), hl: railHl });
  at = byteLength(`${head}${name}`);
  if (commit !== "") {
    spans.push({ from: at, to: at + byteLength(commit), hl: "Swarm.Version" });
    at += byteLength(commit);
  }
  if (mark !== "") {
    spans.push({ from: at, to: at + byteLength(mark), hl: markHl });
    at += byteLength(mark);
  }
  if (named !== "") spans.push({ from: at, to: at + byteLength(named), hl: "Sidebar.Dim" });
  return decorateRow({
    text: `${head}${name}${commit}${mark}${named}`,
    full: `${head}${what}${commit}  ${b.node.name}`,
    indent: byteLength(lead),
    hl: up ? undefined : "Sidebar.Remote",
    spans,
    right,
    value: {
      kind: "elsewhere",
      key: b.fold,
      name: branch ? `${what} on ${b.node.name}` : b.node.name,
      machines: [{ id: b.node.id, name: b.node.name, cwd: (b.main ?? b.trees[0])?.cwd ?? "" }],
      link: linkWords(link),
    },
  }, fitted, Boolean(busy));
}

/** Every conversation in a machine's block: its main checkout's, then its worktrees'. */
function blockAgents(b: MachineBlock): SwarmAgent[] {
  return [...(b.main?.agents ?? []), ...b.trees.flatMap((t) => t.agents)];
}

/**
 * The `│` down the left of everything on one machine, and where it stands: `hl` is the machine's
 * group, `at` the column — under the arrow of the row it hangs from, which is column 3 for a block
 * inside one of your projects and column 1 for a project in that machine's own section.
 */
interface Rail {
  hl: string;
  at: number;
}

/**
 * The group a machine is drawn in — its rail, its `⎇`, its section's rule — which is its colour
 * while it can be reached and `Swarm.Down` while it cannot. Still in both cases: the rail is on every
 * row of the block, and a block that pulsed while a link was being dialled would be the whole panel
 * moving.
 */
function railOf(id: string, opts: DrawOptions): string {
  return opts.links.get(id)?.state === "up" ? opts.colors.get(id) ?? "Swarm.Up" : "Swarm.Down";
}

/**
 * The indent a row at `depth` starts with.
 *
 * Plain spaces. There was a `│` drawn into it down the left of everything on one machine, and it
 * read as a border — a column of lines beside every block that made the panel look boxed in rather
 * than grouped. The indent already says what hangs off what; the machine's colour is carried by the
 * marks each row already has (the `·` of a conversation, the `⎇` of a checkout) instead.
 */
function indentFor(depth: number, _rail?: Rail): string {
  return "   " + "  ".repeat(depth);
}

/** Nothing now: see {@link indentFor}. Kept as the one place a rail would be coloured. */
function railSpan(_pad: string, _rail?: Rail): Array<{ from: number; to: number; hl: string }> {
  return [];
}

/**
 * Where a row is when it is not here: `@` and the machine's code, in the machine's colour.
 *
 * `@`, because in a terminal that is already the word for it: anyone who has typed `ssh you@box`
 * reads it as *over there, on that host*. A cloud was the first answer and it is the world's mark
 * for "not on this machine" everywhere except a terminal — where what it actually reads as is a
 * cloud *service*, which is precisely the wrong idea about a laptop on the same desk.
 *
 * **The code says which host and the colour says it again**: `@ms` in that machine's own colour
 * (`color`, a `Swarm.Host*` group) while it can be reached, a pulsing amber while it is being
 * dialled or waiting on somebody over there, dim when nothing is dialling it. `more` counts the
 * other computers a repository is also on, as `+1` — the code named is the one `↵` would reach.
 */
function marker(
  link: LinkState | undefined,
  code?: string,
  more = 0,
  color?: string,
): { text: string; hl: string } {
  const text = `@${code ?? ""}${more > 0 ? `+${more}` : ""}`;
  switch (link?.state) {
    case "up":
      return { text, hl: color ?? "Swarm.Up" };
    case "connecting":
    case "retrying":
      return { text, hl: "Swarm.Linking" };
    case "waiting":
      return { text, hl: "Swarm.Waiting" };
    default:
      // No entry at all is a machine the panel has a conversation from and no link row for, which
      // is a peer that went away between the two reads. Down is the honest reading of that, and it
      // is also the quietest — a row that shouts about a race is worse than one that under-reports
      // for a tick.
      return { text, hl: "Swarm.Down" };
  }
}

/** How a link reads in words, for the key card — where there is room to be exact. */
function linkWords(link: LinkState | undefined): string {
  switch (link?.state) {
    case "up":
      return "connected";
    case "connecting":
      return link.attempt > 0 ? `connecting, try ${link.attempt}` : "connecting";
    case "retrying":
      return link.attempt > 0 ? `reconnecting, try ${link.attempt}` : "reconnecting";
    case "waiting":
      return "has not allowed this computer yet";
    default:
      return "not connected";
  }
}

/**
 * A conversation on another computer.
 *
 * Inside that machine's block, one level down, with the block's rail beside it — the rail and the
 * block row above say which computer, so the row itself spends its columns on the conversation.
 * Every column means what it means on a conversation row here, including the right-hand one: how
 * long the turn that is running has been running, and when it last moved otherwise.
 *
 * **Drawn like one of yours while it can be reached.** It used to be grey whatever the link was
 * doing, which made a block of conversations you could open and steer look disabled — the one thing
 * the row must not say about the part of the panel you can use. Grey is kept for a machine that is
 * not connected, where it is true.
 */
function remoteRow(
  r: SwarmAgent,
  opts: DrawOptions,
  now: number,
  depth = 0,
  /** The machine's rail, when the row is under something that already says which machine. */
  rail?: Rail,
): ListRow<Target> {
  const link = opts.links.get(r.node.id);
  // Every state a conversation of yours can be in, in the order `sessionRow` ranks them, off what
  // the machine it is on says about it: asking you something, working, cut off, finished while
  // nobody was looking, still running something in the background. Only while the link is up —
  // a machine that cannot be reached is one whose marks are about a moment that has passed.
  const up = link?.state === "up";
  const asking = up && r.agent.state === "blocked";
  const working = !asking && r.agent.state === "running";
  const interrupted = up && !working && !asking && Boolean(r.agent.interrupted);
  const unread = up && !working && !asking && !interrupted && Boolean(r.agent.unread);
  const background = up && !working && !asking && !interrupted && !unread &&
    (r.agent.background ?? 0) > 0;
  // The one you are in, over there. Lit like a conversation of yours you are in: the same `▸`, the
  // same colour, the whole title.
  const here = opts.mirrored?.node === r.node.id && opts.mirrored.session === r.agent.session;
  const mark = rail
    ? { text: "", hl: "" }
    : marker(link, opts.codes.get(r.node.id), 0, opts.colors.get(r.node.id));
  // The state glyph a local conversation carries, in the same column it carries it in, so a
  // project's conversations read as one list whichever machine each of them is on.
  const glyph = asking
    ? "?"
    : working
    ? (here ? spinnerFrame() : opts.ascii ? "*" : "◍")
    : interrupted
    ? (opts.ascii ? "x" : "✗")
    : unread
    ? (opts.ascii ? "!" : "●")
    : background
    ? (opts.ascii ? "o" : "○")
    : here
    ? (opts.ascii ? ">" : "▸")
    : opts.ascii ? "." : "·";
  // Anything that says more than *here* or *idle* outranks the machine's colour on the mark.
  const stated = asking || working || interrupted || unread || background;
  const pad = indentFor(depth, rail);
  const lead = `${pad}${glyph} `;
  // The same clock a local row uses, in the same column: how long the turn that is running has
  // been running — `ago` is for "last touched" and rounds a whole minute away, which on a turn that
  // has been going twenty seconds reads as `now` for ever — and otherwise when it last moved, like
  // a conversation here: a list where only some rows say how old they are looks half-drawn.
  const age = (working || asking) && r.agent.turn_started_at
    ? elapsed(Math.max(0, now - r.agent.turn_started_at * 1000))
    : r.agent.updated_at > 0
    ? ago(now / 1000 - r.agent.updated_at)
    : "";
  const tail = mark.text === "" ? "" : ` ${mark.text}`;
  const decoration = carried(r.agent.decorations);
  // Counted rather than guessed, as `sessionRow` does: the lead, the code after the title, and the
  // age column with a column of air either side of it.
  const room = Math.max(
    8,
    opts.width - width(lead) - width(tail) - (age === "" ? 1 : width(age) + 2) -
      badgeColumns(decoration),
  );
  const label = clip(r.agent.label, room);
  const spans = [...railSpan(pad, rail)];
  // Under something that already named the machine, the conversation's own mark wears its colour —
  // the one column every conversation row has, so a block reads as one machine's without a line
  // drawn down beside it. Not while it is working: then the mark is the working colour's.
  if (rail && !stated && !here) {
    const from = byteLength(pad);
    spans.push({ from, to: from + byteLength(glyph), hl: rail.hl });
  }
  if (mark.text !== "") {
    spans.push({
      from: byteLength(`${lead}${label} `),
      to: byteLength(`${lead}${label}${tail}`),
      hl: mark.hl,
    });
  }
  return decorateRow({
    text: `${lead}${label}${tail}`,
    full: `${lead}${r.agent.label}${tail}`,
    indent: byteLength(lead),
    expand: here,
    hl: asking
      ? "Status.Pending"
      : here
      ? working && pulseBright() ? "Status.Working" : "Accent"
      : working
      ? "Status.Monitoring"
      : interrupted
      ? "Diagnostic.Error"
      : unread
      ? "Status.Unread"
      : background
      ? "Status.Monitoring"
      : up
      ? undefined
      : "Sidebar.Remote",
    spans: spans.length > 0 ? spans : undefined,
    right: {
      text: age === "" ? "" : `${age} `,
      hl: asking
        ? "Status.Pending"
        : working
        ? "Status.Working"
        : interrupted
        ? "Diagnostic.Error"
        : unread
        ? "Status.Unread"
        : background
        ? "Status.Monitoring"
        : "Sidebar.Dim",
    },
    value: {
      kind: "remote",
      node: r.node.id,
      session: r.agent.session,
      cwd: r.agent.cwd,
      host: r.node.name,
      link: linkWords(link),
    },
  }, decoration, working || asking);
}

function sessionRow(
  s: SessionInfo,
  now: number,
  opts: DrawOptions,
  depth = 0,
): ListRow<Target> {
  // The agent has stopped and is waiting on you. First, because it is the only state here that a
  // turn being in flight does not already describe: the turn *is* in flight — blocked on the
  // question — so without this the row is a spinner, indistinguishable from one that is thinking,
  // and the way you find it is by opening conversations until one of them asks you something.
  const asking = opts.asking.has(s.id);
  // Working is the only state that moves, and only where the work is. An idle row's status is
  // deliberately static: twenty animating rows carry no more information than one and cost twenty
  // times the attention.
  const working = !asking && Boolean(s.active_turn);
  // Finished while you were somewhere else. The one row in this column that is asking for
  // something, so it is the one row that gets the attention colour — and it does not move, because
  // it is not going to stop being true on its own and a mark that pulses forever is a mark you
  // learn to look past. It goes away by being opened.
  const unread = !working && !asking && !s.interrupted && s.unread;
  // The turn that was running here never ended, because the workspace it was running in stopped:
  // the machine was shut down, the process was killed. It outranks `unread` — a turn that finished
  // while you were away and a turn that was killed are both news, and only one of them lost work —
  // and it does not move, because nothing is happening here: it already happened.
  const interrupted = !working && !asking && s.interrupted;
  // Something the agent started and stopped waiting for — a shell it put in the background, a
  // sub-agent it let go of. Last in the order on purpose: every state above it is a block or news
  // — and a killed turn is both — while this is neither. It is the answer to "it said it was done,
  // is it?", which is only a question once nothing louder is true.
  const running = !working && !asking && !interrupted && !unread &&
    (s.background?.length ?? 0) > 0;
  const glyph = asking
    // A question mark, in both alphabets. Every other glyph here has an ASCII understudy because
    // the Unicode one is prettier; this one is already the character that means what it means, and
    // a shape people have to learn would be worse in either.
    ? "?"
    : working
      ? s.is_active
        ? spinnerFrame()
        : opts.ascii ? "*" : "◍"
      // The same mark a tool call that failed wears, and for the same reason: this did not finish.
      // A conversation is the largest thing in the workspace that can fail to finish.
      : interrupted
        ? opts.ascii ? "x" : "✗"
      : unread
        ? opts.ascii ? "!" : "●"
        : running
          // Hollow where unread is solid: the same size of mark, and the difference between them
          // is the difference between "there is something here for you" and "there is something
          // here still happening".
          ? opts.ascii ? "o" : "○"
          : s.is_active
            ? opts.ascii ? ">" : "▸"
            : " ";
  // The clock, and it keeps running while the question sits there: one asked four minutes ago and
  // one asked while you were reading this row are not the same news. Off the turn rather than off
  // `working`, because a question can be raised by a plugin with no turn behind it at all — and
  // then the honest number is when the conversation last moved, not a turn that never started.
  const right = s.active_turn
    ? turnFor(s, now)
    : s.updated_at > 0 ? ago(now / 1000 - s.updated_at) : "";
  // Two more columns per level: a conversation inside a worktree sits inside the worktree's row
  // the way the worktree sits inside its repository's.
  const pad = "   " + "  ".repeat(depth);
  const target: Target = { kind: "session", id: s.id, cwd: s.cwd };
  // What is left for the title, counted rather than guessed at. The indent, the glyph and the
  // space after it come off the front; the age column and the space keeping it off the panel edge
  // come off the end, plus one column of air so a title cannot run into a timestamp. The old
  // number was a constant two columns larger than the prefix it stood for, which is two characters
  // of every title in the panel spent on nothing.
  const room = Math.max(
    8,
    opts.width - pad.length - 2 - (right === "" ? 0 : right.length + 2) -
      badgeColumns(opts.decorations.get(targetKey(target) ?? "")),
  );
  return decorateRow({
    text: `${pad}${glyph} ${clip(s.label, room)}`,
    // The title is the only thing telling two conversations in one project apart, and a generated
    // title is a sentence rather than a word — so the column cuts it at about the point where it
    // was going to say which of them this is.
    full: `${pad}${glyph} ${s.label}`,
    // The conversation you are in is not a row you are considering, it is where you are — so it
    // says its whole name whether or not the cursor is on it. Everything else in this column is
    // scannable because it is one line each; the current row is the one place that trade is wrong,
    // and there is only ever one of it.
    expand: s.is_active,
    indent: pad.length + 2,
    hl: asking
      // The palette's group for waiting on something outside the program — which here is a person,
      // and the reason it is the one row in this column that moves. `Status.Unread` deliberately
      // does not: that is news, and news does not stop being true if you ignore it. This is a
      // *block*, it ends the moment you answer, and until then nothing in the workspace goes on.
      ? "Status.Pending"
      : working
        ? s.is_active && pulseBright() ? "Status.Working" : "Status.Monitoring"
        // Red, and the palette's red for something that went wrong rather than a status colour of
        // its own: work was lost here. The whole row again, for the reason the unread mark takes
        // the whole row.
        : interrupted
          ? "Diagnostic.Error"
        : unread
          // The whole row, not only the dot: a single coloured character five columns in is findable
          // if you already know it is there, which is the one thing you cannot assume about the
          // conversation you have forgotten you started.
          ? "Status.Unread"
          : running
            // The colour the panel already uses for work happening somewhere you are not, dimmed
            // and — the whole point — still. Motion here would be a promise that you have to do
            // something, and you do not: it finishes whether or not you look.
            ? "Status.Monitoring"
            : s.is_active
              ? "Accent"
              : undefined,
    // A trailing space so the age does not sit flush against the panel's edge rule.
    right: {
      text: right === "" ? "" : `${right} `,
      hl: asking
        ? "Status.Pending"
        : working
          ? "Status.Working"
          : interrupted
            ? "Diagnostic.Error"
            : unread
            ? "Status.Unread"
            : running ? "Status.Monitoring" : "Sidebar.Dim",
    },
    value: target,
  }, opts.decorations.get(targetKey(target) ?? ""), Boolean(s.active_turn));
}

/**
 * How long the running turn has been running.
 *
 * A conversation that says it is working without saying for how long is indistinguishable from one
 * that is wedged, which is the moment you reach for `^C` and lose the answer. The host stamps the
 * start, so this survives the panel being closed and reopened mid-turn — a timer counted in here
 * would restart at zero and quietly lie.
 */
function turnFor(s: SessionInfo, now: number): string {
  const started = s.turn_started_at;
  if (!started || started <= 0) return "…";
  return elapsed(Math.max(0, now - started * 1000));
}

/**
 * The main keys, at the foot.
 *
 * Only the main ones. This strip used to carry every verb for the row under the cursor, packed two
 * to a line in one dim colour, and what that looked like was a paragraph — a row of `f ★  JK move
 * n new  y path` is read once and then never again. The verbs for a row are on the key card beside
 * the panel now, one per line and level with the row they are about, and `?` is the whole sheet.
 * What stays here is what you press from wherever you are: the way in, the way out, and the doors
 * to the rest of the workspace.
 *
 * Laid out as a grid rather than packed: the keys line up in columns and wear `Sidebar.Key`, so
 * the eye lands on the letters and the words are what they mean. Cells come off the end when the
 * column is narrow, except the last, which is the way to everything that did not fit.
 */
function hints(opts: DrawOptions): ListRow<Target>[] {
  const kind = opts.selected?.kind;
  const cells: Array<[string, string]> = !opts.focused
    // `^T` is on the heading and `^O` on the add row, beside what they act on. What is left is
    // what has no row of its own.
    ? [
      ["^N", "new"],
      ["^F", "archive"],
      ["^K", "palette"],
      ["^B", "hide"],
      // How to reach settings from the composer, which has no key of its own to give it.
      ["/settings", ""],
      ["^Z", "keys"],
    ]
    : [
      // What `↵` does *here*, because a strip that says `open` over a project is a strip you
      // learn to distrust.
      [
        "↵",
        kind === "project"
          ? "fold"
          : kind === "add"
          ? "add project"
          : kind === "custom"
          ? "run"
          : kind === "remote"
          ? "watch"
          : kind === "elsewhere"
          ? "fold"
          : "open",
      ],
      ["?", "keys"],
      [",", "settings"],
      ["esc", "back"],
    ];
  const lines = grid(cells, opts.width, 3);
  return [
    blank(),
    { text: "─".repeat(Math.max(1, opts.width)), hl: "Separator", inert: true },
    // Half a motion, at the foot, where Vim puts it: without it a count is a keypress with no
    // effect until the next one, which is indistinguishable from a key that does nothing. On the
    // first key line rather than on the rule above it — the rule is a row of full width, and a
    // flush-right virtual text has nowhere to sit on a row that is already full.
    ...lines.map((line, i) => ({
      text: line.text,
      spans: line.spans,
      hl: "Sidebar.Dim",
      right: i === 0 && opts.count !== "" ? { text: `${opts.count} `, hl: "Accent" } : undefined,
      inert: true,
    })),
  ];
}

/**
 * Key cells in aligned columns, at most two lines of them.
 *
 * Every cell is padded to the widest, so the keys make a column and the words make another — a
 * strip you can scan down rather than read along. How many columns is worked out from the width
 * the panel actually has, and what does not fit in two lines is dropped from the end, keeping the
 * last cell: that one is `keys`, the way to everything dropped, and a strip that truncates its own
 * escape hatch is worse than one that never mentioned it.
 */
function grid(
  cells: Array<[string, string]>,
  width: number,
  maxLines = 2,
): Array<{ text: string; spans: Array<{ from: number; to: number; hl: string }> }> {
  const cols = (t: string) => Array.from(t).length;
  const room = Math.max(1, width - 1);
  // A cell with no words is a key that says what it is on its own — `/settings` — so it has no
  // space after it to pad.
  const cell = Math.max(...cells.map(([k, l]) => cols(k) + (l === "" ? 0 : 1 + cols(l))));
  const perLine = Math.max(1, Math.floor((room + 2) / (cell + 2)));
  let shown = cells;
  if (cells.length > perLine * maxLines) {
    const last = cells[cells.length - 1];
    shown = cells.slice(0, perLine * maxLines - 1);
    if (last) shown.push(last);
  }
  const lines: Array<{ text: string; spans: Array<{ from: number; to: number; hl: string }> }> =
    [];
  for (let i = 0; i < shown.length; i += perLine) {
    let text = " ";
    const spans: Array<{ from: number; to: number; hl: string }> = [];
    for (const [j, [key, label]] of shown.slice(i, i + perLine).entries()) {
      if (j > 0) text += "  ";
      const from = byteLength(text);
      text += key;
      spans.push({ from, to: byteLength(text), hl: "Sidebar.Key" });
      if (label !== "") text += ` ${label}`;
      // Pad to the column, except the last cell on the line — trailing spaces are what a
      // flush-right count would have to sit on top of.
      const used = cols(key) + (label === "" ? 0 : 1 + cols(label));
      if (j < perLine - 1) text += " ".repeat(Math.max(0, cell - used));
    }
    lines.push({ text: text.trimEnd(), spans });
  }
  return lines;
}

/** One line of the key card: the keys, and what they do on this row. */
interface LegendLine {
  keys: string[];
  label: string;
}

/**
 * What the card beside the panel says for the row under the cursor.
 *
 * Built from the registry and nothing else. `maps` is every binding on this panel's kind as the
 * host holds it right now; {@link ABOUT} says which of this panel's verbs apply to this kind of row
 * and in what words, and the contributed verbs say so themselves. So a key moved in `init.ts`
 * shows the letter that works, a verb a plugin withdrew is gone, and a key nothing in the panel
 * claims is not advertised. Two keys with the same words share a line.
 *
 * The contributed verbs about *this row* come before the ones about any row, which is the same
 * ranking the dispatcher uses when two plugins want one key — so the line for a key is what that
 * key would actually do here, and never a second meaning for the same press.
 */
function legendLines(
  target: Target | undefined,
  maps: KeymapEntry[],
  actions: BoundAction[],
): LegendLine[] {
  const kind = target?.kind;
  if (kind === undefined) return [];
  const keysOf = new Map<string, string[]>();
  for (const m of maps) {
    if (m.scope.kind !== "buf_kind" || m.scope.name !== KIND) continue;
    const list = keysOf.get(m.command) ?? [];
    list.push(m.lhs);
    keysOf.set(m.command, list);
  }
  const lines: LegendLine[] = [];
  const add = (command: string, label: string, share: boolean) => {
    const keys = keysOf.get(command);
    if (!keys || keys.length === 0) return;
    const pretty = keys.map(prettyKey);
    // Only this panel's own verbs share a line, and only with each other: `↵` and `space` both
    // say `fold or unfold` because they *are* the same verb. A contributed verb whose label
    // happens to match one of ours is a different verb — the archive plugin's `a` opens the
    // archive, `x` puts a conversation in it — and one line for both would be a lie about `a`.
    const shared = share ? lines.find((l) => l.label === label) : undefined;
    if (shared) shared.keys.push(...pretty.filter((k) => !shared.keys.includes(k)));
    else lines.push({ keys: pretty, label });
  };
  for (const [command, about] of ABOUT) {
    const label = about[kind];
    if (label) add(command, label, true);
  }
  const applicable = actions.filter((a) => applies(a.on ?? "any", target));
  const ranked = [
    ...applicable.filter((a) => (a.on ?? "any") !== "any"),
    ...applicable.filter((a) => (a.on ?? "any") === "any"),
  ];
  const seen = new Set<string>();
  for (const a of ranked) {
    if (a.binding === null || seen.has(a.key)) continue;
    seen.add(a.key);
    add(a.binding, a.label, false);
  }
  return lines;
}

/**
 * What the card is called, by the kind of row it is about.
 *
 * This is also where the machine went. A remote row used to spend up to twelve of the panel's
 * columns saying `mac-studio`, on every one of them, about a block of rows that are all on
 * `mac-studio` — and it clipped the conversation's own name to make room. What the row needs at a
 * glance is one bit and a colour: not here, and reachable or not. Which computer, and what the
 * link is doing in words, is exactly the sort of thing the card exists for — it appears beside the
 * row you pause on, and a border has room for a sentence where a 34-column panel does not.
 */
function legendTitle(target: Target | undefined): string {
  switch (target?.kind) {
    case "session":
      return "conversation";
    case "project":
      // A project you also have on other computers says so here rather than on the row.
      return target.hosts && target.hosts.length > 0
        ? `project \u00b7 also on ${clip(target.hosts.join(", "), 40)}`
        : "project";
    case "add":
      return "add a project";
    case "remote":
      // The name of the machine, and what the link to it is doing — because `↵` on this row does
      // something quite different depending on the answer, and the row cannot say which.
      return `on ${clip(target.host, 24)}${target.link ? ` \u00b7 ${target.link}` : ""}`;
    case "elsewhere": {
      // Which machine, and what the link to it is doing — the two facts that came off the row when
      // the machine's name did, and the two `↵` behaves completely differently on. A border has
      // room for a sentence; a column of a narrow panel has room for one glyph.
      const names = clip(target.machines.map((m) => m.name).join(", "), 30);
      return `on ${names}${target.link ? ` \u00b7 ${target.link}` : ""}`;
    }
    case "custom":
      return target.section ?? "this row";
    default:
      return "keys";
  }
}

/**
 * The key card: every key for the row under the cursor, beside the panel and level with the row.
 *
 * The strip at the foot used to try to say this and could not — two dim lines, packed, and the
 * verbs that did not fit dropped. A card has room: one key per line, the key in its own colour,
 * the words after it, as wide as the widest line and as tall as there are lines. It sits in the
 * main region just past the panel's rule, at the row of the thing it is about, so the eye goes
 * from the row to its keys and back without reading anything else.
 *
 * It appears after `ui.keys.hint_delay`, the rule the window prefix already follows: somebody who
 * presses `^T j j ↵` fluently never sees it, and somebody who pauses is answered. Once open it
 * follows the cursor without closing — a card that blinked on every `j` would be worse than none.
 * It never takes the keyboard, and it goes with the panel's focus.
 *
 * One per panel, which is one per terminal, opened through the panel's own view-bound `neosh` so
 * it lands on the screen the panel is on.
 */
class KeyCard {
  private buf: BufferId | null = null;
  private win: WindowId | null = null;
  private ns: number | null = null;
  private maps: KeymapEntry[] = [];
  /** What is on it and where, so a tick that changes nothing costs nothing. */
  private drawn = "";

  constructor(private readonly neosh: Neosh) {}

  isOpen(): boolean {
    return this.win !== null;
  }

  /**
   * Put the card up beside `anchor`, level with `row` of it.
   *
   * The registry is read here, once per opening, rather than on every move: a binding changes
   * when a plugin loads or `init.ts` runs, both of which are before anybody is in the panel.
   */
  async open(anchor: WindowId, col: number, row: number): Promise<void> {
    if (this.win !== null) return;
    this.maps = await this.neosh.keymap.list("chat").catch(() => [] as KeymapEntry[]);
    const buf = await this.neosh.buf.create({
      name: "[sidebar keys]",
      scratch: true,
      kind: KIND_KEYS,
    });
    this.buf = buf;
    this.ns = await this.neosh.ns.create(KIND_KEYS);
    this.win = await this.neosh.float.open(buf, {
      anchor: { kind: "window", win: anchor },
      offset: { row, col },
      width: { kind: "auto" },
      height: { kind: "auto" },
      border: "rounded",
      focusable: false,
      // Under the panels somebody opens on purpose — the key sheet, a picker, a question — and
      // over the transcript it is drawn across.
      z: 120,
    });
    this.drawn = "";
  }

  async draw(
    target: Target | undefined,
    actions: BoundAction[],
    anchor: WindowId,
    col: number,
    row: number,
  ): Promise<void> {
    if (this.win === null || this.buf === null || this.ns === null) return;
    const lines = legendLines(target, this.maps, actions);
    const title = legendTitle(target);
    const signature = JSON.stringify([title, lines, col, row]);
    if (signature === this.drawn) return;
    this.drawn = signature;
    const keyWidth = Math.max(1, ...lines.map((l) => width(l.keys.join(" "))));
    const rows = lines.length === 0
      ? [{ text: "  nothing to do here  ", marks: [] }]
      : lines.map((l) => {
        const keys = l.keys.join(" ");
        const pad = " ".repeat(keyWidth - width(keys));
        const text = `  ${keys}${pad}   ${l.label}  `;
        return {
          text,
          marks: [{ col: 2, opts: { hlGroup: "Sidebar.Key", endCol: 2 + byteLength(keys) } }],
        };
      });
    await this.neosh.buf.render(this.buf, this.ns, 0, -1, rows);
    await this.neosh.float.configure(this.win, {
      anchor: { kind: "window", win: anchor },
      // One row up, so the first key is level with the row it is about rather than the border.
      offset: { row: row - 1, col },
      width: { kind: "auto" },
      height: { kind: "auto" },
      border: "rounded",
      title: ` ${title} `,
      footer: " ? every key   esc back ",
      focusable: false,
      z: 120,
    });
  }

  async close(): Promise<void> {
    const win = this.win;
    this.forget();
    if (win !== null) await this.neosh.win.close(win).catch(() => {});
  }

  /** Drop the card without closing it, for when its terminal has already gone. */
  forget(): void {
    this.win = null;
    this.buf = null;
    this.ns = null;
    this.drawn = "";
  }
}

/**
 * The always-visible numbers: how long the current turn has run, how full the window is, and what
 * it has cost.
 *
 * These live in the footer rather than the panel because the panel can be hidden and these are the
 * things you want while you are typing. They are also the reason the sidebar no longer carries a
 * usage block — one home per number.
 */
async function installFooter(neosh: Neosh, subscriptions: PluginContext["subscriptions"]) {
  const refresh = async () => {
    const [current, selection] = await Promise.all([
      neosh.session.current().catch(() => null),
      neosh.agent.selection().catch(() => null),
    ]);
    if (!current) return;

    // Where you are, when it is not here: `@lb linux-box` on the strip, in that machine's colour,
    // for as long as the conversation on screen is one of its. The composer looks exactly like the
    // one for a conversation of yours — that is the point — so something on screen has to say which
    // computer a message typed into it goes to.
    if (current.mirror) {
      const [codes, colors, nodes] = await Promise.all([
        neosh.cmd.call<Record<string, string> | null>("swarm.codes").catch(() => null),
        neosh.cmd.call<Record<string, string> | null>("swarm.colors").catch(() => null),
        neosh.swarm.nodes().catch(() => [] as SwarmNode[]),
      ]);
      const id = current.mirror.node;
      const up = nodes.find((n) => n.info.id === id)?.link.state === "up";
      const code = codes?.[id] ?? "";
      await neosh.status.set("swarm.here", {
        text: `@${code} ${current.mirror.machine}${up ? "" : " · offline"}`,
        short: `@${code}`,
        hl: up ? colors?.[id] ?? "Swarm.Up" : "Swarm.Down",
        priority: 3,
      });
    } else {
      await neosh.status.clear("swarm.here");
    }

    if (current.active_turn) {
      await neosh.status.set("turn", {
        text: `${spinnerFrame()} ${turnFor(current, Date.now())}`,
        // The group that sweeps. Nothing here drives the animation — the frontend does, at its own
        // rate, which is why this can be set once per second and still look continuous.
        hl: "Status.Streaming",
        align: "right",
        priority: 5,
      });
    } else {
      await neosh.status.clear("turn");
    }

    const model = selection
      ? (await neosh.agent.listModels(selection.instance).catch(() => []))
          .find((e) => e.model.id === selection.model)?.model
      : undefined;
    const u = current.usage;
    // The context meter is *not* here. It was, and so was one in the usage plugin, and for as long
    // as no model in the catalogue reported a window only one of them ever drew — so two gauges of
    // the same thing, disagreeing about what "used" means, sat in the same footer the moment one
    // did. The usage plugin owns it: it is named for the job and it reads the context the last
    // request actually carried, rather than adding up totals that count a cached prompt twice.
    if (u.input_tokens + u.output_tokens === 0) {
      await neosh.status.clear("cost");
      return;
    }

    const p = model?.pricing;
    if (!p) return;
    // Cache reads are billed at their own rate, which is the whole reason to count them separately:
    // a session that looks expensive by token count is often cheap because most of it was a hit.
    const cost =
      (u.input_tokens * p.input_per_mtok +
        u.output_tokens * p.output_per_mtok +
        u.cache_read_tokens * p.cache_read_per_mtok +
        u.cache_write_tokens * p.cache_write_per_mtok) /
      1_000_000;
    await neosh.status.set("cost", {
      text: money(cost),
      hl: "Comment",
      align: "right",
      priority: 12,
    });
  };

  await refresh();
  subscriptions.push(neosh.agent.onTurnStart(() => void refresh()));
  subscriptions.push(neosh.agent.onTurnEnd(() => void refresh()));
  subscriptions.push(neosh.session.onChange(() => void refresh()));
  // The cost depends on the model's prices, so a switch changes it without a turn happening.
  subscriptions.push(neosh.agent.onSelectionChange(() => void refresh()));
  // The clock only ticks while something is running, so this costs nothing at rest.
  subscriptions.push(onTick(() => void refresh()));
}

function basename(cwd: string): string {
  return cwd.split("/").filter(Boolean).pop() ?? cwd;
}

function short(cwd: string): string {
  return basename(cwd);
}

function clip(s: string, n: number): string {
  const chars = Array.from(s);
  return chars.length <= n ? s : `${chars.slice(0, Math.max(1, n - 1)).join("")}…`;
}

function ago(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return "";
  if (seconds < 60) return "now";
  return elapsed(seconds * 1000).replace(/ \d+s$/, "").replace(/ \d+m$/, "");
}
