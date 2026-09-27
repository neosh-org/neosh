/**
 * `@neosh/api` — the entire plugin surface.
 *
 * This file is both the implementation and the types. It is embedded into the host binary and
 * transpiled at load, and it is what plugin authors type-check against, so the two cannot drift
 * apart — there is no second copy to forget to update.
 *
 * Everything here is built on the wire types in `./generated/`, which are emitted from the Rust
 * side by ts-rs and drift-checked in CI. Nothing in this file reaches the host except as an
 * `ApiCall`, which means an out-of-process plugin in another language has exactly this surface.
 */

import type { Activity } from "./generated/Activity";
import type { AttachmentInfo } from "./generated/AttachmentInfo";
import type { ApiCall } from "./generated/ApiCall";
import type { ApiError } from "./generated/ApiError";
import type { ApiOk } from "./generated/ApiOk";
import type { ApiResponse } from "./generated/ApiResponse";
import type { BranchInfo } from "./generated/BranchInfo";
import type { Direction } from "./generated/Direction";
import type { PaneId } from "./generated/PaneId";
import type { TabId } from "./generated/TabId";
import type { TabInfo } from "./generated/TabInfo";
import type { BufferId } from "./generated/BufferId";
import type { Capability } from "./generated/Capability";
import type { CommitInfo } from "./generated/CommitInfo";
import type { CommandEntry } from "./generated/CommandEntry";
import type { AgentCommand } from "./generated/AgentCommand";
import type { AgentState } from "./generated/AgentState";
import type { AgentSummary } from "./generated/AgentSummary";
import type { Contribution } from "./generated/Contribution";
import type { AccountKind } from "./generated/AccountKind";
import type { Brand } from "./generated/Brand";
import type { CredentialInfo } from "./generated/CredentialInfo";
import type { CredentialSource } from "./generated/CredentialSource";
import type { CursorMotion } from "./generated/CursorMotion";
import type { CursorShape } from "./generated/CursorShape";
import type { SelectShape } from "./generated/SelectShape";
import type { ScrollAmount } from "./generated/ScrollAmount";
import type { DiffTarget } from "./generated/DiffTarget";
import type { Dock } from "./generated/Dock";
import type { Gravity } from "./generated/Gravity";
import type { Hint } from "./generated/Hint";
import type { ExtmarkId } from "./generated/ExtmarkId";
import type { FileChange } from "./generated/FileChange";
import type { FileState } from "./generated/FileState";
import type { ExtmarkInfo } from "./generated/ExtmarkInfo";
import type { ExtmarkOpts } from "./generated/ExtmarkOpts";
import type { FloatConfig } from "./generated/FloatConfig";
import type { HighlightDef } from "./generated/HighlightDef";
import type { HighlightEntry } from "./generated/HighlightEntry";
import type { HighlightSpec } from "./generated/HighlightSpec";
import type { HlTarget } from "./generated/HlTarget";
import type { HookName } from "./generated/HookName";
import type { HookOutcome } from "./generated/HookOutcome";
import type { HookPayload } from "./generated/HookPayload";
import type { InstanceConfig } from "./generated/InstanceConfig";
import type { KeyContext } from "./generated/KeyContext";
import type { KeymapEntry } from "./generated/KeymapEntry";
import type { KeymapScope } from "./generated/KeymapScope";
import type { MessageLevel } from "./generated/MessageLevel";
import type { Mode } from "./generated/Mode";
import type { ModelEntry } from "./generated/ModelEntry";
import type { ModelInfo } from "./generated/ModelInfo";
import type { ModelSelection } from "./generated/ModelSelection";
import type { ModelTier } from "./generated/ModelTier";
import type { NamespaceId } from "./generated/NamespaceId";
import type { OptionChoice } from "./generated/OptionChoice";
import type { OptionEntry } from "./generated/OptionEntry";
import type { OptionSelection } from "./generated/OptionSelection";
import type { OptionSpec } from "./generated/OptionSpec";
import type { OptionType } from "./generated/OptionType";
import type { OptionValue } from "./generated/OptionValue";
import type { PermissionDecision } from "./generated/PermissionDecision";
import type { PermissionMode } from "./generated/PermissionMode";
import type { PermissionOption } from "./generated/PermissionOption";
import type { PermissionOptionKind } from "./generated/PermissionOptionKind";
import type { QuestionAnswer } from "./generated/QuestionAnswer";
import type { QuestionOption } from "./generated/QuestionOption";
import type { UserQuestion } from "./generated/UserQuestion";
import type { PluginEvent } from "./generated/PluginEvent";
import type { PointInfo } from "./generated/PointInfo";
import type { PluginInfo } from "./generated/PluginInfo";
import type { PluginManifest } from "./generated/PluginManifest";
import type { Pricing } from "./generated/Pricing";
import type { QuotaCredits } from "./generated/QuotaCredits";
import type { QuotaSample } from "./generated/QuotaSample";
import type { QuotaSeverity } from "./generated/QuotaSeverity";
import type { QuotaSnapshot } from "./generated/QuotaSnapshot";
import type { InstallMethod } from "./generated/InstallMethod";
import type { UpdateOutcome } from "./generated/UpdateOutcome";
import type { UpdateProgress } from "./generated/UpdateProgress";
import type { UpdateStatus } from "./generated/UpdateStatus";
import type { QuotaSource } from "./generated/QuotaSource";
import type { QuotaWindow } from "./generated/QuotaWindow";
import type { UsageBucket } from "./generated/UsageBucket";
import type { UsageHistory } from "./generated/UsageHistory";
import type { UsageResolution } from "./generated/UsageResolution";
import type { UsageScanSource } from "./generated/UsageScanSource";
import type { CostBasis } from "./generated/CostBasis";
import type { DriverCommand } from "./generated/DriverCommand";
import type { PlanState } from "./generated/PlanState";
import type { PlanStep } from "./generated/PlanStep";
import type { ProviderEvent } from "./generated/ProviderEvent";
import type { TaskId } from "./generated/TaskId";
import type { TaskStatus } from "./generated/TaskStatus";
import type { ProviderOptionDescriptor } from "./generated/ProviderOptionDescriptor";
import type { Message } from "./generated/Message";
import type { Rect } from "./generated/Rect";
import type { RepoInfo } from "./generated/RepoInfo";
import type { GitHead } from "./generated/GitHead";
import type { RepoStatus } from "./generated/RepoStatus";
import type { SessionId } from "./generated/SessionId";
import type { SessionInfo } from "./generated/SessionInfo";
import type { StatusAlign } from "./generated/StatusAlign";
import type { StatusSegment } from "./generated/StatusSegment";
import type { StopReason } from "./generated/StopReason";
import type { SurfaceCell } from "./generated/SurfaceCell";
import type { TextEdit } from "./generated/TextEdit";
import type { SurfaceId } from "./generated/SurfaceId";
import type { ToolCall } from "./generated/ToolCall";
import type { ToolDef } from "./generated/ToolDef";
import type { ToolResult } from "./generated/ToolResult";
import type { ImageFile } from "./generated/ImageFile";
import type { TurnRequest } from "./generated/TurnRequest";
import type { Usage } from "./generated/Usage";
import type { NodeCapabilities } from "./generated/NodeCapabilities";
import type { NodeId } from "./generated/NodeId";
import type { LinkState } from "./generated/LinkState";
import type { NodeInfo } from "./generated/NodeInfo";
import type { ProjectKey } from "./generated/ProjectKey";
import type { RemoteProject } from "./generated/RemoteProject";
import type { StreamEvent } from "./generated/StreamEvent";
import type { LiveSaid } from "./generated/LiveSaid";
import type { LiveTurn } from "./generated/LiveTurn";
import type { ImageData } from "./generated/ImageData";
import type { SwarmAgent } from "./generated/SwarmAgent";
import type { SwarmNode } from "./generated/SwarmNode";
import type { SwarmStranger } from "./generated/SwarmStranger";
import type { VarScope } from "./generated/VarScope";
import type { ViewId } from "./generated/ViewId";
import type { ViewInfo } from "./generated/ViewInfo";
import type { Viewport } from "./generated/Viewport";
import type { WindowId } from "./generated/WindowId";
import type { WindowInfo } from "./generated/WindowInfo";
import type { WindowLayout } from "./generated/WindowLayout";
import type { ChecksState } from "./generated/ChecksState";
import type { PullRequest } from "./generated/PullRequest";
import type { PullState } from "./generated/PullState";
import type { WorktreeInfo } from "./generated/WorktreeInfo";

export type {
  AccountKind, Activity, ApiError, BranchInfo, Brand, BufferId, Capability, CommandEntry, CommitInfo,
  AgentCommand, AgentState, AgentSummary,
  Contribution, CredentialInfo, CredentialSource, CursorShape, DiffTarget, Direction, Dock, CursorMotion, ExtmarkId, ExtmarkInfo, ExtmarkOpts, FileChange, FileState, FloatConfig,
  PaneId, TabId, TabInfo,
  Gravity, HighlightDef, HighlightEntry, HighlightSpec, Hint, HlTarget, HookName, HookOutcome, HookPayload, InstanceConfig, KeyContext,
  AttachmentInfo,
  KeymapEntry, KeymapScope, MessageLevel, Mode, ModelEntry, ModelInfo, ModelSelection, ModelTier, NamespaceId,
  OptionChoice, OptionEntry, OptionSelection, OptionSpec, OptionType, OptionValue,
  DriverCommand, PlanState, PlanStep, TaskId, TaskStatus,
  Message, PermissionDecision, PermissionMode, PermissionOption, PermissionOptionKind, PluginEvent, PluginInfo, PluginManifest, PointInfo, Pricing, ProviderEvent, ProviderOptionDescriptor,
  QuestionAnswer, QuestionOption, UserQuestion,
  CostBasis, QuotaCredits, QuotaSample, QuotaSeverity, QuotaSnapshot, QuotaSource, QuotaWindow,
  InstallMethod, UpdateOutcome, UpdateProgress, UpdateStatus,
  UsageBucket, UsageHistory, UsageResolution, UsageScanSource,
  Rect, RepoInfo, GitHead, RepoStatus, ScrollAmount, SelectShape, SessionId, SessionInfo, StatusAlign, StatusSegment, StopReason,
  SurfaceCell, SurfaceId, TextEdit, ToolCall, ToolDef, ToolResult, TurnRequest, Usage, ImageFile,
  NodeCapabilities, NodeId, NodeInfo, ProjectKey, RemoteProject, StreamEvent, LiveSaid, LiveTurn, ImageData,
  LinkState, SwarmAgent, SwarmNode, SwarmStranger,
  VarScope, ViewId, ViewInfo, Viewport,
  WindowId, WindowInfo, WindowLayout,
  ChecksState,
  PullRequest,
  PullState,
  WorktreeInfo,
};

// ---------------------------------------------------------------------------
// Columns
// ---------------------------------------------------------------------------

/**
 * UTF-8 byte length of a string.
 *
 * Every column in this API is a **UTF-8 byte offset**, matching Neovim and for the same reason: it
 * keeps display-width math in exactly one place, the frontend. JavaScript's `.length` is UTF-16
 * code units, which agrees with bytes only for ASCII — so a highlight placed with `.length` on a
 * line containing an emoji or any CJK lands in the wrong column, or inside a character.
 */
export function byteLength(s: string): number {
  let n = 0;
  for (const ch of s) {
    const c = ch.codePointAt(0) ?? 0;
    n += c < 0x80 ? 1 : c < 0x800 ? 2 : c < 0x10000 ? 3 : 4;
  }
  return n;
}

/**
 * Byte offset of each code point in `s`, plus one final entry holding the total length.
 *
 * `byteOffsets(s)[i]` is the column to pass for the `i`th code point, so a match found with
 * `Array.from(s)` can be turned into marks without measuring the prefix again per position.
 */
export function byteOffsets(s: string): number[] {
  const out: number[] = [];
  let n = 0;
  for (const ch of s) {
    out.push(n);
    const c = ch.codePointAt(0) ?? 0;
    n += c < 0x80 ? 1 : c < 0x800 ? 2 : c < 0x10000 ? 3 : 4;
  }
  out.push(n);
  return out;
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/** The ops the host installs. Everything else is built on these. */
interface CoreOps {
  op_neosh_send(msg: unknown): void;
  op_neosh_next(): Promise<unknown>;
  op_neosh_width(text: string): number;
  op_neosh_clip(text: string, columns: number): string;
}
declare const Deno: { core: { ops: CoreOps } };

/**
 * How many terminal columns a string occupies.
 *
 * Use this for any layout with a column in it. JavaScript offers `String.length` (UTF-16 units) and
 * `Array.from(s).length` (code points), and both are wrong for the text a model produces: `"日本"`
 * is 2 code points and **4** columns, `"👋🏽"` is 2 code points and **2**, `"é"` may be 2 code points
 * and **1**. Padding with either draws a ragged rule the first time a CJK model name appears.
 *
 * Synchronous — it is an op, not a host call, so calling it per row costs nothing. It uses the same
 * measurement the renderer does, so a plugin and the frontend agree by construction.
 */
export function width(text: string): number {
  return Deno.core.ops.op_neosh_width(text);
}

/** Truncate to at most `columns` columns, cutting on a grapheme boundary rather than mid-character. */
export function clipToWidth(text: string, columns: number): string {
  return Deno.core.ops.op_neosh_clip(text, Math.max(0, Math.floor(columns)));
}

/** Pad on the right to exactly `columns`, clipping if it is already wider. */
export function padToWidth(text: string, columns: number): string {
  const clipped = clipToWidth(text, columns);
  return clipped + " ".repeat(Math.max(0, columns - width(clipped)));
}

/** Thrown when the host refuses a call. Carries the structured reason. */
export class NeoshError extends Error {
  constructor(readonly cause_: ApiError) {
    super(describeError(cause_));
    this.name = "NeoshError";
  }
}

function describeError(e: ApiError): string {
  switch (e.kind) {
    case "not_found": return `not found: ${e.what}`;
    case "invalid_argument": return `invalid argument: ${e.message}`;
    case "denied": return `denied: ${e.reason}`;
    case "not_permitted": return `not permitted: ${e.capability}`;
    case "busy": return `busy: ${e.message}`;
    case "internal": return `internal error: ${e.message}`;
  }
}

let seq = 0;
const pending = new Map<string, { resolve: (v: ApiOk) => void; reject: (e: unknown) => void }>();

function send(msg: unknown): void {
  Deno.core.ops.op_neosh_send(msg);
}

/**
 * Issue a call that wants an answer.
 *
 * Calls from one plugin are applied by the host in the order they were issued, so a caller that
 * fires several mutations without awaiting cannot race itself.
 */
function call(plugin: string, c: ApiCall, view?: ViewId): Promise<ApiOk> {
  const id = `${plugin}#${++seq}`;
  return new Promise<ApiOk>((resolve, reject) => {
    pending.set(id, { resolve, reject });
    send({ type: "plugin", plugin, msg: { type: "call", id, call: c, view } });
  });
}

/** Fire-and-forget. Used on the streaming path so appending a token is not round-trip bound. */
function notify(plugin: string, c: ApiCall, view?: ViewId): void {
  send({ type: "plugin", plugin, msg: { type: "notify", call: c, view } });
}

function settle(id: string, response: ApiResponse): void {
  const p = pending.get(id);
  if (!p) return;
  pending.delete(id);
  if (response.status === "ok") p.resolve(response.value);
  else p.reject(new NeoshError(response.error));
}

/** Narrow an `ApiOk` to the variant a call is documented to return. */
function expect<K extends ApiOk["ok"]>(v: ApiOk, kind: K): Extract<ApiOk, { ok: K }> {
  if (v.ok !== kind) {
    throw new Error(`host returned ${v.ok} where ${kind} was expected`);
  }
  return v as Extract<ApiOk, { ok: K }>;
}

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

export interface Disposable {
  dispose(): void;
}

export interface PluginContext {
  readonly neosh: Neosh;
  readonly pluginId: string;
  readonly config: unknown;
  /** Anything pushed here is disposed when the plugin unloads. */
  readonly subscriptions: Disposable[];
}

/** A plugin's entry module exports this. */
export type Activate = (ctx: PluginContext) => void | Promise<void>;

export interface VirtText {
  text: string;
  hlGroup?: string;
}

export interface MarkOptions {
  endCol?: number;
  hlGroup?: string;
  /**
   * A background for the whole rendered row, not just the bytes the mark covers.
   *
   * It sits *under* every ranged `hlGroup` on the row rather than competing with them, so a row can
   * be banded — selected, changed, at fault — and the text on it keeps whatever colour said what it
   * was. `hlGroup` across the same span would replace that colour instead, which is how a selected
   * row ends up the only one that has stopped saying anything.
   */
  lineHlGroup?: string;
  virtText?: VirtText[];
  virtTextPos?: ExtmarkOpts["virt_text_pos"];
  onDelete?: ExtmarkOpts["on_delete"];
  priority?: number;
  /**
   * The row is a picture from this column on, and this is where it is.
   *
   * The row's text from that column is what a terminal that cannot draw pictures shows — its
   * name — and one that can (kitty, Ghostty, WezTerm, Konsole, tmux with passthrough) draws the
   * picture there instead, as many screen rows tall as it decides. Still one row of the buffer
   * however tall it draws, exactly as a row that wraps is. The path is on the workspace's own
   * disk; a picture the agent read or you attached is already there, under the state directory.
   */
  image?: { path: string; mediaType: string };
}

/** One mark of a {@link DrawnRow}, positioned on that row. */
export interface DrawnMark {
  /**
   * UTF-8 byte offset into the row's text — the unit every column on the wire uses. Build it with
   * {@link byteLength}, never `.length`.
   */
  col: number;
  opts?: MarkOptions;
}

/** One row of a repaint: its text, and everything drawn on it. */
export interface DrawnRow {
  text: string;
  marks?: DrawnMark[];
}

export interface FloatOptions {
  anchor?: FloatConfig["anchor"];
  offset?: { row: number; col: number };
  width?: FloatConfig["width"];
  height?: FloatConfig["height"];
  z?: number;
  border?: FloatConfig["border"];
  borderHl?: string;
  title?: string;
  /**
   * A strip on the bottom border: what the keys here do.
   *
   * On the border rather than in the buffer, because a key strip written as a row of content is one
   * that scrolls away exactly when it is wanted, and is the first thing clipped on a terminal too
   * short for the panel. It costs no content row and cannot be scrolled off.
   *
   * Clipped to the border's width like the title, so it is for a legend and not for prose.
   */
  footer?: string;
  closeOnBlur?: boolean;
  focusable?: boolean;
  /**
   * Take the keyboard while this float has focus.
   *
   * Global bindings stop resolving: `^N`, `^T`, `^G` and the rest do nothing until it closes,
   * instead of opening a second panel behind the first. Your own bindings still work — window,
   * buffer and kind scopes are all nearer than global — and so does anything in
   * `ui.modal_escape_keys` (`^Q` and `^R` by default), so a panel that forgets to bind a way out
   * is never a terminal somebody has to kill. A key nothing claimed is swallowed rather than
   * falling through to the composer.
   *
   * For a panel you are meant to answer before doing anything else: a question, a confirmation, a
   * control sheet. Not for a hint or a hover card.
   */
  modal?: boolean;
  /**
   * This panel is a place you can move in when it does not fit.
   *
   * You ask for a height and get whatever there is: `{ kind: "max", n: 30 }` on a twenty-row
   * terminal is ten rows of panel and twenty rows of content drawn nowhere, with no key pointed at
   * them and nothing on screen to say they exist. Set this and the workspace's scroll keys resolve
   * here — `j`/`k`, `^D`/`^U`, `^F`/`^B`, `gg`/`G`, the arrows and the paging keys — along with the
   * mouse wheel, and a bar appears down the right border while anything is hidden.
   *
   * They are ordinary bindings on `{ kind: "buf_kind", name: "neosh.scroll" }`, so `^Z` lists them
   * and `init.ts` moves them. That scope sits *below* your buffer's own kind, so a key you bind on
   * your panel is still yours — and above `global`, so it works under `modal` too.
   *
   * For a panel of rows you read: a key sheet, a diff, a status, a help screen. **Not** for one with
   * a cursor of its own — a picker, a list — which already scrolls to keep the cursor on screen and
   * whose filter would lose `j` and `G` to this.
   */
  scroll?: boolean;
}

export interface Neosh {
  readonly version: number;
  readonly buf: BufferApi;
  readonly win: WindowApi;
  /** The panes of the main region. See {@link PaneApi}. */
  readonly pane: PaneApi;
  /** The tabs each terminal has. See {@link TabApi}. */
  readonly tab: TabApi;
  readonly float: FloatApi;
  readonly edit: EditApi;
  readonly ns: NamespaceApi;
  readonly hl: HighlightApi;
  readonly ui: RawCellApi;
  readonly cmd: CommandApi;
  readonly keymap: KeymapApi;
  readonly focus: FocusApi;
  readonly agent: AgentApi;
  readonly tool: ToolApi;
  readonly hook: HookApi;
  readonly provider: ProviderApi;
  readonly git: GitApi;
  readonly gen: GenApi;
  readonly session: SessionApi;
  readonly view: ViewApi;
  readonly status: StatusApi;
  readonly hint: HintApi;
  readonly opt: OptionApi;
  readonly state: StateApi;
  readonly vars: VarApi;
  /** The directories somebody works in. See {@link ProjectApi}. */
  readonly project: ProjectApi;
  readonly ext: ExtensionApi;
  readonly event: EventApi;
  readonly swarm: SwarmApi;
  readonly quota: QuotaApi;
  /** Whether there is a newer neosh, and becoming it. */
  readonly update: UpdateApi;
  readonly rtp: RuntimePathApi;
  readonly path: PathApi;
  readonly timer: TimerApi;
  readonly log: Logger;
  /**
   * Say something in the corner, as a reply to a key the user just pressed.
   *
   * The default and by far the commonest case: feedback for a keystroke. It does not stack — a
   * second one replaces the first, because two keys pressed quickly are two keys and the reply
   * you want is the one for the second — it lives about six seconds, and it never leaves the
   * terminal.
   *
   * What it is *not* for is the thing the user can already see. `favourited ~/proj` next to a row
   * that just grew a pin is the same fact printed twice, and a corner that is usually saying
   * something you did not need is a corner people stop reading. See
   * {@link Neosh.progress} and {@link Neosh.alert} for the two things that are not this.
   */
  notify(message: string, level?: MessageLevel): void;
  /**
   * Say what is happening, in a row that gets replaced rather than stacked.
   *
   * Keyed: writing the same key again replaces the row, and {@link Neosh.done} takes it away. This
   * is what `pulling…` should have been — it was pushed onto the message stack, and so was the
   * `up to date` that superseded it, which is how one pull drew two rows.
   *
   * A row nobody finishes is dropped after a minute, so a plugin that crashes mid-operation cannot
   * leave a permanent claim on screen. Relying on that is a row that lies for up to a minute.
   */
  progress(key: string, message: string): void;
  /** Take a progress row off, because the thing it was about has finished. */
  done(key: string): void;
  /**
   * News: something happened that the user did not ask for.
   *
   * Drawn in the corner like a message, and — if the host works out that nobody is looking —
   * raised outside the terminal as well, as an escape sequence the terminal turns into a real
   * notification. Whether that happens is the host's decision and not yours: only it knows which
   * conversation is on screen, which terminals are attached and whether any has focus.
   *
   * Needs `notify` in `plugin.toml`, because a plugin that can interrupt somebody who is in
   * another application is a capability rather than a way of drawing. Rejected with
   * `not permitted` otherwise.
   *
   * @param session Which conversation this is about, if it is about one. The test for "can they
   * see this already" is asked against it; an alert about no conversation is never on screen.
   */
  alert(
    title: string,
    message: string,
    opts?: { level?: MessageLevel; session?: SessionId },
  ): Promise<void>;
  /** Ask the host whether a side effect is allowed. */
  permit(capability: Capability): Promise<PermissionDecision>;
  /**
   * Ask the *person* a question, and wait for the answer.
   *
   * Not a permission and not a picker. A permission asks whether something may happen and policy
   * can answer it without waking anybody; a picker is a list you opened. This is the panel an agent
   * gets when it asks you which library, which approach, which of these to enable — several
   * questions in one sitting, some of them taking more than one answer, any of them answerable with
   * something nobody listed.
   *
   * Whatever serves the `ask_user` hook draws it, so your question and the agent's look the same
   * and a plugin that replaces the panel replaces both.
   *
   * `null` is *nobody answered* — dismissed, or timed out. Not an error: treating it as one means
   * reporting a failure every time somebody presses `<Esc>`.
   */
  ask(questions: UserQuestion[]): Promise<QuestionAnswer[] | null>;
  /**
   * A terminal attached to a workspace that was already running. Only a plugin drawing raw cells
   * needs this — the core forwards a surface's cells and keeps no copy, so they have to be painted
   * again; everything in a buffer is republished without help.
   */
  onViewAttached(cb: () => void): Disposable;
  /**
   * The workspace is stopping. `deactivate` is called after this; both are bounded, so say your
   * goodbye quickly. Reload is not this: a reloaded plugin gets `deactivate` and nothing here.
   */
  onShutdown(cb: () => void): Disposable;
  /**
   * What the agent is allowed to do without asking, and how to change it.
   *
   * `setMode` lasts for this session only. A mode switched on to get through one task should not
   * still be on next week, and writing it to a file is exactly how that happens — so it is not
   * written to one.
   */
  readonly permission: PermissionApi;
}

export interface BufferApi {
  /**
   * `kind` is what this buffer *is* — `neosh.sidebar`, `acme.tasks`. Say it if anything you draw is
   * a panel somebody else might want to extend: it is what `keymap.set(..., { scope: { kind } })`
   * binds against and what `win.list()` reports, and it costs one argument. Reverse domain by
   * convention.
   */
  create(opts?: { name?: string; scratch?: boolean; kind?: string }): Promise<BufferId>;
  lineCount(buf: BufferId): Promise<number>;
  /**
   * `end` is exclusive. Negative indices are `len + 1 + i`, as in Neovim's `nvim_buf_set_lines`:
   * **`-1` is one past the last line**, so `getLines(buf, 0, -1)` is the whole buffer. To address
   * the last line itself, use `-2, -1`.
   */
  getLines(buf: BufferId, start: number, end: number): Promise<string[]>;
  /**
   * Range replacement. There is no whole-document write, by design — streaming a response must not
   * resend the document once per token.
   *
   * `setLines(buf, 0, -1, lines)` replaces the buffer; `setLines(buf, -1, -1, lines)` appends.
   */
  setLines(buf: BufferId, start: number, end: number, lines: string[]): Promise<void>;
  /**
   * Replace a range of rows **and** `ns`'s marks on them, in one call. What a panel should use to
   * draw itself.
   *
   * The atomic form of `setLines` + `ns.clear` + a `ns.mark` per mark. That sequence is correct at
   * rest and wrong in flight: each call is a round trip, the frontend draws on a ~16 ms deadline
   * that knows nothing about how far through a repaint you are, and a frame landing after the clear
   * draws every row with no marks at all — in `Normal`, which is near-white. A dim panel redrawing
   * ten times a second flashes bright. This has no halfway state to observe, and costs one round
   * trip instead of one per mark.
   *
   * Indices resolve as {@link setLines}'s do, and the clear covers exactly the rows written — so a
   * partial repaint leaves the rest of the panel alone. Other namespaces are untouched, which is
   * what lets an overlay survive the panel under it redrawing.
   */
  render(
    buf: BufferId,
    ns: NamespaceId,
    start: number,
    end: number,
    rows: DrawnRow[],
  ): Promise<void>;
  /** Append to the final line without resending it. The streaming fast path. */
  appendText(buf: BufferId, text: string): Promise<void>;
  setName(buf: BufferId, name: string): Promise<void>;
  /** Declare — or with `null`, withdraw — what this buffer is. See `create`. */
  setKind(buf: BufferId, kind: string | null): Promise<void>;
  kind(buf: BufferId): Promise<string | null>;
  onChange(
    buf: BufferId,
    cb: (e: { buf: BufferId; start: number; oldEnd: number; newEnd: number }) => void,
  ): Promise<Disposable>;
}

/**
 * The panes of the main region, and the tabs they live in.
 *
 * A pane is *where*, not *what*: it owns no buffer and draws nothing. You split one, then dock
 * windows into it with {@link WindowApi.open} — which is why splitting a chat and splitting a
 * terminal are the same two calls.
 *
 * Everything here is what the workspace's own `<C-w>` keys are built from. There is no private
 * path: `<C-w>v` is `pane.split(await pane.active(), "right")` and nothing else, so a plugin that
 * wants a different window manager can have one without forking anything.
 */
export interface PaneApi {
  /** The pane this terminal's keys belong to. */
  active(view?: ViewId): Promise<PaneId>;
  /**
   * Divide a pane in two, and answer with the new one.
   *
   * The new pane is *empty* — nothing is drawn in it until something is docked into it. Focus does
   * not move either: a plugin splitting to show you something and a person pressing a key to go
   * and work there are different intentions, and only the second one wants the keyboard.
   */
  split(pane: PaneId, dir: Direction): Promise<PaneId>;
  /**
   * Close a pane and every window docked in it.
   *
   * Rejects on the last pane of the last tab: a terminal with nowhere to draw has no key that
   * would bring a pane back.
   */
  close(pane: PaneId): Promise<void>;
  focus(pane: PaneId): Promise<void>;
  /**
   * Move the keyboard to the nearest pane in a direction, and answer with it — or `null` at the
   * edge of the layout, which is an ordinary outcome of holding a key down rather than an error.
   * There is deliberately no wrap-around.
   */
  focusDir(dir: Direction, view?: ViewId): Promise<PaneId | null>;
  /**
   * Move the boundary on a pane's `dir` side. `delta` is in weight units — an even split is
   * {@link WEIGHT} each — so a resize means the same thing at every terminal size and survives the
   * terminal being resized under it. A pane with no boundary on that side is a no-op.
   */
  resize(pane: PaneId, dir: Direction, delta: number): Promise<void>;
  /**
   * Exchange two panes' places, keeping what is in each. The weights belong to the slots, so
   * swapping into the wide one is how you make a pane wide.
   */
  swap(pane: PaneId, withPane: PaneId): Promise<void>;
  /**
   * Send a pane to the far edge of its tab — the counterpart to {@link PaneApi.focusDir}, which
   * moves the keyboard rather than the pane. It arrives with an even share rather than the size it
   * had: it is among different neighbours now, and carrying the old number over would make it the
   * odd one out for a reason nobody could see.
   */
  moveToEdge(pane: PaneId, dir: Direction): Promise<void>;
  /** Give every pane of the active tab an equal share, at every level. */
  equalize(view?: ViewId): Promise<void>;
}

/**
 * The tabs of one terminal. Each is a title, a tree of panes, and the conversation it belongs to.
 *
 * **A tab belongs to a conversation, and the bar shows one conversation's tabs** — that is
 * {@link TabInfo.group}, and it is why a shell opened while reading one conversation is not still
 * on the bar after you have switched to another. The tabs of the conversations you are not in are
 * not closed and nothing in them stops; they are off the strip until you go back.
 *
 * Which splits these calls in two. {@link TabApi.list} answers with *every* tab of a terminal,
 * whatever bar it is on, because "what is open here" is a real question — and {@link TabApi.step},
 * {@link TabApi.move} and the numbers on the strip all count the tabs of one conversation, because
 * a key about where you are looking must not change what you are working on.
 */
export interface TabApi {
  /**
   * Every tab of a terminal, its panes, and which one is on screen.
   *
   * All of them, including the ones on other conversations' bars: filter by {@link TabInfo.group}
   * for what is drawn, which is the tabs sharing the active tab's group.
   */
  list(view?: ViewId): Promise<{ tabs: TabInfo[]; active: TabId }>;
  /**
   * Open a tab with one empty pane. `activate` goes to it — `false` is for putting work somewhere
   * without moving the screen out from under whoever is reading it.
   *
   * On the bar you are looking at unless `group` says otherwise: a tab opened while reading a
   * conversation is that conversation's, which is what a new tab almost always wants.
   */
  create(opts?: {
    title?: string;
    activate?: boolean;
    view?: ViewId;
    group?: string;
  }): Promise<TabId>;
  /**
   * Move a tab to another conversation's bar, or to nobody's with `null`. See
   * {@link TabInfo.group}.
   *
   * It keeps its panes, its windows and anything running in them — this says which bar it is on
   * and nothing else, so moving the tab you are *in* changes which other tabs are beside it.
   */
  group(tab: TabId, group: string | null): Promise<void>;
  /** Close a tab and every pane in it. Rejects on a conversation's last tab. */
  close(tab: TabId): Promise<void>;
  select(tab: TabId): Promise<void>;
  /**
   * Go `delta` tabs along, wrapping — a tab bar is a list you can see all of. Along *this
   * conversation's* tabs: stepping into another one's would be a key that changes what you are
   * working on.
   */
  step(delta: number, view?: ViewId): Promise<void>;
  /**
   * Move a tab along the bar, clamped at the ends. This is a drag; a drag that teleports is a bug.
   *
   * `to` counts the tabs on the bar, which is the number printed on the strip — the tabs of other
   * conversations sit between these and are not places a drag can land.
   */
  move(tab: TabId, to: number): Promise<void>;
  /** Name a tab, or pass `null` to give it back its derived name. */
  rename(tab: TabId, title: string | null): Promise<void>;
}

export interface WindowApi {
  /**
   * `gravity` is which end short content settles against: `"start"` (the default) pins it to the
   * top, `"end"` to the bottom, which is what makes a transcript read as a conversation rather
   * than as a document that happens to be in a window.
   *
   * `wrap` makes long lines fold rather than clip. Docks clip by default — a side panel that
   * wrapped a long path would reflow every row below it — but a text field is prose and wants
   * this on. A bottom dock that wraps also grows to show the folded rows, `size` acting as its
   * floor.
   *
   * `pane` docks the window inside one pane of the main region rather than against the screen.
   * The same four edges either way — a composer at the foot of a pane is this call with a `pane`,
   * and a status line along the foot of the terminal is this call without one. Omit it for
   * anything that belongs to the whole terminal: a sidebar docked into a pane would be a sidebar
   * that disappears when you close that split.
   */
  open(
    buf: BufferId,
    dock: Dock,
    opts?: { size?: number; gravity?: Gravity; wrap?: boolean; pane?: PaneId },
  ): Promise<WindowId>;
  close(win: WindowId): Promise<void>;
  /**
   * Change how wide (or tall) a docked window is, without closing it.
   *
   * Reopening is not the same thing: the window id changes and whatever had the keyboard loses it,
   * so a panel resized from inside itself would throw the cursor back to the composer on every
   * press. `null` gives the dock its default extent back. Floats are configured with
   * {@link FloatApi.configure}, and are refused here.
   */
  resize(win: WindowId, size: number | null): Promise<void>;
  setBuf(win: WindowId, buf: BufferId): Promise<void>;
  /** `col` is a UTF-8 byte offset, not a character or display column. */
  cursor(win: WindowId): Promise<{ row: number; col: number }>;
  setCursor(win: WindowId, row: number, col: number): Promise<void>;
  /**
   * Put a buffer row at the top of a window, or hand the scroll position back.
   *
   * `null` is *unscrolled*, which is where a window starts and is not the same place as row `0`:
   * a window that follows its content — the transcript — shows its last screenful unscrolled and
   * its first row at `0`. Anything else shows the same thing either way.
   */
  scrollTo(win: WindowId, topLine: number | null): Promise<void>;
  /**
   * Move a window's scroll by a line, a half screen, a screen, or all the way to an end.
   *
   * What {@link scrollTo} cannot express, because the arithmetic is not yours to do: "half a screen"
   * is counted in the buffer rows the frontend actually drew — which on a wrapping window is fewer
   * than its height — and the floor is the last screenful rather than the last line, so the bottom
   * is a full panel and not one row above an empty one. Both numbers are a frame old by the time
   * {@link viewport} hands them to you; this reads them where they live.
   *
   * The keys bound on `neosh.scroll` run exactly this. Call it directly for a scroll a key did not
   * ask for — following output, or a button on a panel of your own.
   */
  scroll(win: WindowId, amount: ScrollAmount): Promise<void>;
  /**
   * How big this window actually is, in cells.
   *
   * `null` until the frontend has drawn it once. This is the only way to learn real geometry:
   * everything about display width is resolved by the frontend, so a plugin sizing a meter or
   * deciding what to drop at 60 columns asks rather than computing an answer it cannot compute
   * correctly.
   */
  viewport(win: WindowId): Promise<Viewport | null>;
  /**
   * Every window that is open, and what is in it.
   *
   * How you find somebody else's panel. A window id belongs to whoever opened it and changes every
   * time the panel is reopened, so this plus a buffer `kind` is the only way to say "the sidebar,
   * whichever window that is right now" — and therefore the only way to act on one you did not
   * open.
   */
  list(): Promise<WindowInfo[]>;
  /** The open windows showing a buffer of this kind. Sugar over `list()`, which is the common case. */
  ofKind(kind: string): Promise<WindowInfo[]>;
  /**
   * Remap group names for one window, or for every window of a buffer kind — Neovim's
   * `winhighlight`. `{ Normal: "Acme.Panel", "Sidebar.Selected": "Acme.Sel" }` on
   * `{ kind: "neosh.sidebar" }` recolours the sidebar without redefining the groups anything else
   * draws with. A window's map sits over its kind's. An empty map clears; the remap is yours and
   * goes when your plugin does.
   */
  setHighlights(
    target: { win: WindowId } | { kind: string },
    map: Record<string, string>,
  ): Promise<void>;
}

/**
 * Moving and editing text where a window's cursor is.
 *
 * Verbs rather than positions, because grapheme and word boundaries are genuinely hard and nobody
 * should have to get them right twice. A text field in your plugin behaves the same way the
 * composer does because they are asking the same question, not because they each reimplemented it.
 */
export interface EditApi {
  /**
   * Move the cursor. `select` extends a selection from wherever it was anchored — anchoring first
   * if nothing was — which is shift-and-arrow. Without it the selection is dropped.
   */
  move(win: WindowId, motion: CursorMotion, opts?: { select?: boolean }): Promise<void>;
  /** Edit at the cursor. Typing over a selection replaces it, as everywhere else. */
  apply(win: WindowId, edit: TextEdit): Promise<void>;
  /** Anchor a selection where the cursor is, or drop the one there is. */
  select(win: WindowId, on: boolean): Promise<void>;
  /**
   * What the two ends of the selection *mean*.
   *
   * `"exclusive"` is a text field's: the cursor sits between characters and the one it is on is
   * not selected. `"inclusive"` is a normal mode's — the cursor is *on* a character and that
   * character is in — and `"line"` takes whole rows in whichever direction the selection runs.
   * Dropping a selection puts this back to `"exclusive"`.
   */
  selectShape(win: WindowId, shape: SelectShape): Promise<void>;
  /**
   * What the caret looks like here: a bar between two characters, or a block on one.
   *
   * The one thing on screen that says whether keys are being typed or obeyed, before any of them
   * is pressed.
   */
  cursorShape(win: WindowId, shape: CursorShape): Promise<void>;
  /** What is selected. `""` when nothing is. */
  selection(win: WindowId): Promise<string>;
  /**
   * Put text on the system clipboard.
   *
   * A capability rather than something you could do yourself: the runtime has no terminal, and the
   * frontend is the only thing holding the stream this has to travel down. Over SSH it reaches the
   * terminal you are sitting at, which a clipboard library on the remote host would not.
   */
  copy(text: string): Promise<void>;
}

export interface FloatApi {
  open(buf: BufferId, opts?: FloatOptions): Promise<WindowId>;
  configure(win: WindowId, opts?: FloatOptions): Promise<void>;
  close(win: WindowId): Promise<void>;
}

export interface NamespaceApi {
  create(name: string): Promise<NamespaceId>;
  /** `col` is a UTF-8 byte offset. */
  mark(ns: NamespaceId, buf: BufferId, row: number, col: number, opts?: MarkOptions): Promise<ExtmarkId>;
  getMark(ns: NamespaceId, buf: BufferId, id: ExtmarkId): Promise<ExtmarkInfo | null>;
  allMarks(ns: NamespaceId, buf: BufferId): Promise<ExtmarkInfo[]>;
  delMark(ns: NamespaceId, buf: BufferId, id: ExtmarkId): Promise<void>;
  clear(ns: NamespaceId, buf: BufferId, start?: number, end?: number): Promise<void>;
}

export interface HighlightApi {
  /**
   * Declare a semantic group. Prefer `link` so an unknown theme still looks right.
   *
   * Yours from then on: a theme switch leaves it alone, and unloading your plugin takes it back to
   * the theme's definition or away. `default: true` is Neovim's `:hi default` — define only if
   * nobody has — which is what to use for the groups your plugin introduces, so a user's `init.ts`
   * wins whichever of you loaded first.
   */
  define(
    name: string,
    def: { link: string } | HighlightSpec,
    opts?: { default?: boolean },
  ): Promise<void>;
  /**
   * What a group is, and what it resolves to after following links. Both `null` for a name nobody
   * defined. The way to compute "a little dimmer than `Normal`" rather than guess at it.
   */
  get(name: string): Promise<{ def: HighlightDef | null; resolved: HighlightSpec | null }>;
  /** Every group, with which plugin owns it (`owner` absent for the theme's own). */
  list(): Promise<HighlightEntry[]>;
  /** Undo your definition of a group. Rejects for a group another plugin owns. */
  reset(name: string): Promise<void>;
  /**
   * Groups changed — defined, reset, or all of them on a theme switch. `names` says which. A
   * panel that cached a colour reads it again here; nothing else needs to, because the frontend
   * redraws on its own.
   */
  onChange(cb: (e: { names: string[] }) => void): Disposable;
}

export interface RawCellApi {
  claim(win: WindowId, rect: Rect): Promise<SurfaceId>;
  put(surface: SurfaceId, cells: SurfaceCell[]): Promise<void>;
  release(surface: SurfaceId): Promise<void>;
}

/** What a command handler is given and what it may give back. */
/**
 * What runs when a command does. `here` is the whole namespace bound to the terminal the key was
 * pressed in — absent when nothing pressed a key — so a window it opens lands where the person is.
 */
export type CommandHandler = (
  args: string[],
  key?: KeyContext,
  here?: Neosh,
) => unknown | Promise<unknown>;

export interface CommandApi {
  /**
   * Register a command by name. Keys bind to the name; `cmd.exec` runs it; `cmd.call` runs it and
   * returns what the handler returned, so a command is also how one plugin asks another a
   * question — `sidebar.cursor`, `git.status.of` — without importing it.
   */
  register(name: string, fn: CommandHandler, opts?: { desc?: string }): Promise<Disposable>;
  /** Run a command and do not wait for it. A key press, from code. */
  exec(name: string, args?: string[]): Promise<void>;
  /**
   * Run a command and wait for its answer.
   *
   * Whatever the handler returned, as JSON — `null` for a handler that returned nothing. Rejects
   * with the handler's error if it threw, with `not found` if nothing registered the name, and
   * after a long timeout if the owner never answered. Routed through the host, so it works for
   * the host's own commands (which answer `null`) and does not care which plugin owns the name.
   *
   * For a typed, zero-round-trip call into a plugin you depend on, `import { api } from
   * "plugin:<name>"` instead — see the `requires` manifest field.
   */
  call<T = unknown>(name: string, args?: string[]): Promise<T>;
  list(): Promise<CommandEntry[]>;
}

export interface KeymapApi {
  /**
   * Bind a key to a *command name*, never to a callback.
   *
   * That indirection is what makes every binding listable and remappable by the user, and it lets
   * the host resolve routing without calling into a plugin.
   *
   * Scope resolves window → buffer → buffer kind → global, first match winning. `{ kind: "buf_kind",
   * name: "neosh.sidebar" }` is the one to reach for when the thing you are binding into is
   * somebody else's panel: a window id is private to whoever opened it and dies with the window,
   * whereas a kind is a name the panel publishes and every window of that kind — including ones
   * opened tomorrow — is covered by one call.
   */
  set(mode: Mode, lhs: string, command: string, opts?: { scope?: KeymapScope; desc?: string }): Promise<void>;
  del(mode: Mode, lhs: string, scope?: KeymapScope): Promise<void>;
  list(mode?: Mode): Promise<KeymapEntry[]>;
  /**
   * While `win` is focused, receive every key the keymaps did not claim.
   *
   * For widgets that need raw input: a filter box, a text field, a modal list. Bindings still win,
   * so `<C-q>` keeps quitting while your picker is open — you get what nothing else wanted. The
   * command is invoked with a `KeyContext`, so one handler can switch on the key.
   *
   * Dispose to release. A capture is also dropped when the window closes or your plugin unloads,
   * so a crash cannot leave the keyboard pointing at nothing.
   */
  capture(win: WindowId, command: string): Promise<Disposable>;
}

export interface FocusApi {
  push(win: WindowId): Promise<void>;
  pop(): Promise<void>;
  current(): Promise<WindowId | null>;
  /**
   * The keyboard moved. `win` is `null` when nothing has it — the composer. The same fact also
   * arrives as `neosh.win.enter` / `neosh.win.leave` on the event bus, with the buffer's kind,
   * which is the form to use when you only care about one panel.
   */
  onChange(cb: (e: { win: WindowId | null }) => void): Disposable;
}

export interface AgentApi {
  /**
   * Send a message. Anything on the composer's attachment row goes with it.
   *
   * `images` are extra paths to attach on the way through, for a plugin that has *produced* a
   * picture rather than one somebody pasted — a rendered chart, a screenshot it took. The
   * bytes are copied into the workspace, so a temporary file may be handed over and forgotten.
   */
  send(text: string, opts?: { images?: string[] }): Promise<void>;
  cancel(): Promise<void>;
  /**
   * Do something to a conversation by id, rather than to whichever one is on screen.
   *
   * The same vocabulary `swarm.command` carries to another machine — steer, interrupt, re-model,
   * rename, archive, start — pointed at a conversation here. That symmetry is the point: an
   * orchestrator that fans work out over several conversations and joins the results is one
   * program whether the conversations are on this laptop or spread over the swarm, and until this
   * existed it was only writable for the ones that were somewhere else.
   *
   * Everything that *watches* a conversation already names one — `onToken`, `onTurnEnd`,
   * `sessions.messages` — so this is the half that was missing. Without it, driving a second
   * conversation meant `sessions.switch` first, which moves the screen out from under whoever is
   * reading it.
   *
   * Omit `session` for the conversation on screen. Answers with the conversation the command was
   * about, which is how `new_session` says what it made.
   *
   * ```ts
   * // Ask three conversations the same thing without touching the screen.
   * for (const s of await neosh.session.list()) {
   *   await neosh.agent.command({ command: "send", text: "status?" }, s.id);
   * }
   * ```
   */
  command(command: AgentCommand, session?: string): Promise<string | null>;
  selection(): Promise<ModelSelection | null>;
  /** Hot-swap the model. Takes effect on the next turn. */
  setSelection(selection: ModelSelection): Promise<void>;
  /**
   * Every reachable model, each paired with the instance that serves it.
   *
   * The pairing matters: a model id is unique per instance, not globally, so a picker that dropped
   * it would have to guess the owner — and guessing wrong sends the conversation to a different
   * endpoint with no visible error.
   *
   * Answers from a session cache. Discovery is a network round trip per configured provider, so
   * pass `refresh` only when you have reason to believe a lineup changed.
   */
  listModels(instance?: string, opts?: { refresh?: boolean }): Promise<ModelEntry[]>;
  listInstances(): Promise<InstanceConfig[]>;
  /**
   * What the driver behind this conversation accepts as a slash command.
   *
   * Reported by the driver at its handshake, not configured. Which commands exist depends on the
   * install — `claude` counts project `.claude/commands/`, plugin commands and MCP prompts among
   * its own — so any list written down in a plugin would be wrong on the first machine that had
   * one of its own. Empty until the conversation has run a turn: there has been nothing to ask.
   */
  driverCommands(): Promise<DriverCommand[]>;
  /**
   * Replace what is in the composer, caret at the end.
   *
   * For completion: a `/` menu, an `@file` menu, a path menu. Pair with the `composerChanged`
   * event, which is the other half — one says what has been typed, this puts the answer back.
   */
  setDraft(text: string): Promise<void>;
  /**
   * Attach an image to whatever is about to be sent.
   *
   * With a path, that file. Without one, whatever image is on the system clipboard — which is
   * the only way a picture can reach a terminal at all: bracketed paste is a text protocol, and a
   * screenshot pasted into one arrives as nothing. That is why `^V` is a key rather than a paste.
   *
   * The bytes are copied into the workspace's own directory, sniffed for what they actually are
   * rather than what they are called, and shrunk if they are enormous. Rejects when there is no
   * image to be had, with a reason worth showing.
   */
  attach(path?: string): Promise<AttachmentInfo>;
  /** What is attached to the composer right now, oldest first. */
  attachments(): Promise<AttachmentInfo[]>;
  /**
   * Take something off the attachment row: the one at `index`, or the newest.
   *
   * Answers with what came off, or nothing if there was nothing there — the row may have
   * gone out with a send between asking and answering, and that is not an error.
   */
  detach(index?: number): Promise<AttachmentInfo | null>;
  /** Take the whole attachment row off. Answers with what was on it. */
  detachAll(): Promise<AttachmentInfo[]>;
  /**
   * Where each configured provider's key comes from — and never what it is.
   *
   * There is deliberately no call that returns a secret. A plugin can find out that `anthropic` is
   * authenticated from the keychain, and can ask the host to collect a new key; it cannot read one,
   * so it cannot leak one.
   */
  credentials(): Promise<CredentialInfo[]>;
  /**
   * Ask the host to collect an API key for `instance` from the keyboard.
   *
   * The host runs this prompt itself: a plugin one would have to put the key in a buffer, and a
   * buffer is drawn — the value would cross the frontend boundary and land in whatever it logs.
   * What comes back is whether a key was stored, never the key.
   *
   * Rejects when the instance signs in on its own (a CLI login) or needs no key at all, and when it
   * already has one unless you pass `replace`.
   */
  setCredential(instance: string, opts?: { replace?: boolean }): Promise<boolean>;
  /** Drop a stored key from memory and from the keychain. The environment is not ours to clear. */
  forgetCredential(instance: string): Promise<void>;
  /**
   * The model this conversation will use changed, whoever changed it.
   *
   * Not the same as `opt.onChange` for `agent.model`: that option is a preference, and the
   * selection also moves when a conversation is restored, when a stored model turns out not to
   * authenticate, and when a provider registers late and the model somebody asked for finally
   * becomes reachable. Anything that names the model — a footer, a context meter measuring against
   * its window — wants this one.
   */
  onSelectionChange(cb: (e: { selection: ModelSelection }) => void): Disposable;
  /**
   * The composer's text changed — a keystroke, a paste, a conversation switch, a send.
   *
   * The other half of {@link AgentApi.setDraft}. Completion of any kind is these two: watch what
   * has been typed, offer something, put the answer back.
   */
  onComposerChange(cb: (e: { text: string }) => void): Disposable;
  /**
   * What a driver's own loop said about itself — a sub-agent, a plan, a compaction, how full its
   * context is.
   *
   * The only signal that moves *during* a turn. Everything else about usage arrives when the turn
   * ends, which for an agent driver can be twenty minutes after the number changed.
   */
  onActivity(cb: (e: { session: SessionId; turn: string; activity: Activity }) => void): Disposable;
  /**
   * A turn has begun.
   *
   * Every turn event says which conversation it belongs to. A workspace runs several at once, so
   * anything that draws a turn has to check: by the time one ends, the conversation it ran in may
   * not be the one on screen. `neosh.session.list()` flags the active one.
   */
  onTurnStart(cb: (e: { session: string; turn: string }) => void): Disposable;
  /** One streamed chunk of assistant text. Chunks are provider-sized, not characters. */
  onToken(cb: (e: { session: string; turn: string; text: string }) => void): Disposable;
  onThinking(cb: (e: { session: string; turn: string; text: string }) => void): Disposable;
  onTurnEnd(
    cb: (e: { session: string; turn: string; stopReason: StopReason; usage: Usage }) => void,
  ): Disposable;
  /**
   * A tool is about to run.
   *
   * Distinct from the `tool_pre` hook: that one is asked *whether* the call may proceed and can
   * veto it. This is told that it is happening, cannot influence it, and is therefore what a
   * transcript wants.
   */
  onToolStart(cb: (e: { session: string; turn: string; call: ToolCall }) => void): Disposable;
  onToolEnd(
    cb: (e: { session: string; turn: string; call: ToolCall; result: ToolResult }) => void,
  ): Disposable;
}

export interface ToolApi {
  /** Lands in the same namespace and shape as a built-in or MCP tool. */
  register(
    def: { name: string; description: string; inputSchema: Record<string, unknown> },
    handler: (input: unknown) => ToolResult | Promise<ToolResult>,
  ): Promise<Disposable>;
  list(): Promise<ToolDef[]>;
}

export interface HookApi {
  /**
   * Register a hook.
   *
   * `blocking: false` (the default) is a pure observer whose return value is ignored — that is what
   * stops an audit plugin from wedging the agent loop. A blocking hook is awaited and may veto;
   * **a blocking hook that does not answer in time is treated as a veto**, so a policy plugin fails
   * closed.
   */
  register(
    hook: HookName,
    fn: (payload: HookPayload) => HookOutcome | Promise<HookOutcome>,
    opts?: { blocking?: boolean; timeoutMs?: number },
  ): Promise<Disposable>;
}

export interface ProviderApi {
  /**
   * Register a model provider implemented in this plugin.
   *
   * Supporting another vendor is a plugin, not a core change. Because a stream cannot cross the
   * RPC boundary as a return value, `handler` receives an `emit` callback and pushes events until
   * it emits `message_stop`.
   */
  register(
    driver: string,
    instances: InstanceConfig[],
    handler: (req: TurnRequest, emit: (e: ProviderEvent) => void, signal: { cancelled: boolean }) => void | Promise<void>,
    opts?: {
      /**
       * This driver runs its own agent loop — it has its own tools, and calls them itself.
       *
       * neosh then sends it no tool list, does not execute the calls in its stream, and records the
       * conversation in the shape that actually happened. Leaving it off for such a driver makes
       * the host run every tool call a second time.
       */
      agentLoop?: boolean;
    },
  ): Promise<Disposable>;
}

/**
 * Version control.
 *
 * The plugin runtime has no process access, so `git` is a host capability rather than something a
 * plugin shells out to. One implementation means a sidebar, a branch picker and a commit UI agree
 * about what "dirty" means instead of each parsing porcelain slightly differently.
 *
 * Reads are free. Writes go through the permission layer as `exec` of `git <verb>`, so
 * `permissions.allow_commands = ["git"]` covers them and a policy hook watching exec sees them.
 *
 * Every call rejects with `not_found` when neosh was started outside a repository — check once with
 * `status()` rather than guarding each call.
 */
export interface GitApi {
  /** The working tree's state. `cwd` is any checkout; omitted, the one this conversation is in. */
  status(opts?: { cwd?: string }): Promise<RepoStatus>;
  /** Local branches, most recently committed first. */
  branches(opts?: { includeRemote?: boolean; cwd?: string }): Promise<BranchInfo[]>;
  /**
   * Every checkout of the repository.
   *
   * `cwd` picks which repository to ask about; without it the answer is the one this conversation
   * is in, which is what a status bar or a branch picker means. A panel means the other thing —
   * it lists several projects at once, and the row under the cursor is not always the conversation
   * you are in.
   */
  worktrees(opts?: { cwd?: string }): Promise<WorktreeInfo[]>;
  log(limit?: number): Promise<CommitInfo[]>;
  /** The patch. Pass `stat` for `--stat`, which is what a prompt wants. */
  diff(target?: DiffTarget, opts?: { stat?: boolean }): Promise<string>;
  /** What this branch would merge into: `origin/HEAD`, else `main`/`master`. */
  defaultBranch(): Promise<string | null>;
  /**
   * Where HEAD is in each of these directories — the branch, and the commit it resolves to — or
   * `null` for one that is not a repository.
   *
   * Read off the repository's files, with no `git` process behind it, so it is cheap enough to ask
   * on every redraw of a panel. What it is for is comparing checkouts: whether the copy of a
   * repository on another machine ([`RemoteProject.head`]) is the version this one has.
   */
  heads(cwds: string[]): Promise<Array<GitHead | null>>;
  createBranch(name: string, opts?: { from?: string }): Promise<void>;
  /**
   * Move a branch to another name — `git branch -m`.
   *
   * One ref write. The working tree is untouched, so this is safe on a branch that is checked out
   * and safe while an agent is editing files against it — which is the case it exists for: naming
   * a worktree's branch from the first message, once there is a message to name it from.
   *
   * `cwd` is the checkout the branch belongs to, and you almost always want it: the worktree being
   * renamed is very often not the one the active conversation is standing in.
   *
   * Fails if `next` is taken — a generated name does not get to overwrite somebody's branch. Ask
   * `branches()` and pick a free one.
   */
  renameBranch(name: string, next: string, opts?: { cwd?: string }): Promise<void>;
  checkout(rev: string): Promise<void>;
  /** Empty `paths` stages everything, like `git add .` from the repository root. */
  stage(paths?: string[]): Promise<void>;
  unstage(paths?: string[]): Promise<void>;
  commit(message: string): Promise<CommitInfo>;
  /**
   * `git fetch --prune`, answering with where the tree stands *after* it.
   *
   * **The one call that makes `ahead` and `behind` mean anything.** `status()` reads them off
   * `git status --branch`, which compares HEAD with the remote-tracking ref sitting on this disk —
   * so a panel drawing `↓0` from it is reporting the state of the world as of whenever this
   * checkout last spoke to a remote. Call this first and the number is news; do not, and it is
   * archaeology.
   *
   * It answers with the fresh {@link RepoStatus} rather than nothing, so "fetch and see where I am"
   * is one round trip. Prunes, because a picker offering six branches that were deleted when their
   * pull requests merged is worse than one that is a fetch behind.
   *
   * A write, and the reason is the network rather than the working tree, which does not move:
   * this contacts a remote and writes refs. It fails rather than hangs when there are no
   * credentials — stdin is closed and every askpass unset — which is what makes it safe to put on
   * a timer. Expect it to reject routinely: no remote, no network, a key the host will not take.
   */
  fetch(opts?: { cwd?: string }): Promise<RepoStatus>;
  /**
   * Every pull request the forge knows about for this repository, open and closed.
   *
   * **One call answers for every checkout.** They come back keyed by
   * {@link PullRequest.branch}, so a repository with six worktrees is one request and six lookups
   * — which is the difference between a panel that costs a round trip every few minutes and one
   * that costs six.
   *
   * Answered by `gh`, the CLI the user has already signed into, so tokens, enterprise hosts, SSO
   * and rate limits are already handled and the answer is the one they get in their own terminal.
   *
   * **Rejects rather than answering empty** when it cannot know — no `gh`, not signed in, no
   * remote on a forge. An empty list is the claim "this repository has no pull requests", and
   * three of the four ways this fails would be making it falsely. Catch it, say it once, and do
   * not say it again every tick.
   */
  pulls(opts?: { cwd?: string }): Promise<PullRequest[]>;
  /**
   * `git pull`, answering with git's own summary — "Already up to date.", the fast-forward range —
   * because those are different answers and a caller showing neither is a caller nobody trusts.
   * `cwd` picks the repository, as everywhere; absent means the conversation's own.
   *
   * `rebase` replays this branch's commits on top of what arrived instead of merging them. It is
   * the answer to a diverged branch and it rewrites local commits, so ask before you set it — the
   * caller this exists for is a panel that has just said "diverged" and offered the choice.
   */
  pull(opts?: { cwd?: string; rebase?: boolean }): Promise<string>;
  addWorktree(
    path: string,
    branch: string,
    opts?: { create?: boolean; cwd?: string },
  ): Promise<void>;
  /**
   * `cwd` names the repository the worktree belongs to. `git worktree remove` must run from a
   * checkout other than the one being removed, and the active conversation may be standing in
   * exactly that one.
   */
  removeWorktree(path: string, opts?: { force?: boolean; cwd?: string }): Promise<void>;
  /**
   * Move a worktree to `dest` — `git worktree move`, and everything that has to follow it.
   *
   * A worktree's path is an identity here, not a coordinate: it is the key of the project facts
   * every list draws, of the conversations living in it, of the project vars holding a pin and a
   * fold state, and of the working directory each vendor CLI was started in. This call moves all
   * of them, which is why it is a call rather than a `git worktree move` you could have run
   * yourself — the git part is the part that was never hard.
   *
   * `dest` is the full path it lands at. Its parent is created; the leaf must not exist.
   *
   * Refused while a turn is running anywhere in the tree, because a CLI holds its working
   * directory from the moment it is spawned and would otherwise write the file it is editing into
   * the place the tree used to be. Interrupt it and ask again.
   *
   * `cwd` names the repository, as everywhere.
   */
  moveWorktree(path: string, dest: string, opts?: { cwd?: string }): Promise<void>;
  /**
   * Clone `url` into `path`, resolving to the path once it is there.
   *
   * The one call here with no `cwd`: everything else asks a repository a question, and this one
   * arrives before there is a repository to ask. `path` is absolute and its parent need not
   * exist — cloning into a location you have just invented is the ordinary case.
   *
   * **It reports progress while it runs**, on the bus as {@link CLONE_EVENT}, so a caller that
   * wants to draw a clone rather than block on one subscribes before awaiting this. Keyed by
   * `path`, because a workspace may be cloning two things at once.
   *
   * Needs `vcs_write` in the manifest, like every other call here that changes a disk.
   */
  clone(url: string, path: string): Promise<string>;
}

/**
 * What {@link GitApi.clone} says about itself while it runs, on {@link EventsApi.on}.
 *
 * `phase` is git's own word for what it is doing — `Receiving objects`, `Resolving deltas` — and
 * `percent` is absent for the phases that have no total, which draw as a spinner rather than as a
 * bar. `done` arrives exactly once per clone, on success and on failure alike, because a panel
 * drawing itself from these has no other way to learn it may stop.
 */
export const CLONE_EVENT = "neosh.git.clone";

/**
 * Where an update says how it is getting on, while {@link UpdateApi.apply} runs.
 *
 * On the bus rather than back to the caller, because the caller is not always the thing drawing
 * — `neosh agent run update` asks and the sidebar's plugin shows it. The payload is
 * {@link UpdateProgress}; the last one has `done` set, whatever happened.
 */
export const UPDATE_EVENT = "neosh.update.progress";

/** One {@link CLONE_EVENT} payload. */
export interface CloneProgress {
  /** Which clone this is about. The key, since two may be running. */
  path: string;
  url: string;
  phase: string;
  percent?: number | null;
  done?: boolean;
  /** Present, with git's own last word in it, only when the clone failed. */
  error?: string;
}

/**
 * One-shot generation: a prompt through a model, outside the conversation.
 *
 * Branch names, commit messages, thread titles and PR descriptions are all this call. It is
 * deliberately *not* `agent.send` — nothing here enters session history, so asking for a commit
 * message does not change what the agent believes it was asked to do.
 *
 * The model is `gen.model` when set, else the conversation's own. Point that option at something
 * cheap; naming a branch does not need a frontier model.
 */
export interface GenApi {
  complete(prompt: string, opts?: { system?: string; selection?: ModelSelection }): Promise<string>;
  /**
   * Same, but parse the answer as JSON.
   *
   * The host tolerates what models actually return — code fences, a "Sure!" preamble — so callers
   * do not each reimplement that. Rejects if there is no JSON in the response at all.
   */
  json<T = unknown>(prompt: string, opts?: { system?: string; selection?: ModelSelection }): Promise<T>;
  /**
   * One value, asked for as JSON and accepted however it comes back.
   *
   * The shape almost every generating plugin actually wants: a branch name, a thread title, a PR
   * subject — one string, asked for as `{"branch": …}` because that is how you pin a model down,
   * and answered as a bare `fix/composer-paste` often enough to matter. Both are the same answer,
   * and {@link GenApi.json} rejects the second one — which is a correct name thrown away, with
   * nothing on screen to say so.
   *
   * So the key is passed down and a bare reply is read as its value. Everything
   * {@link GenApi.json} tolerates is tolerated first and unchanged; this is only what happens when
   * there is no JSON at all. Rejects on an empty answer, or on prose it will not guess at.
   *
   * For one value only. A commit message is a subject and a body, and there is no answering the
   * question of which one a lone paragraph is — that stays {@link GenApi.json}.
   *
   * And only for a value you would know was wrong on sight. A branch name is that, and a wrong one
   * is one rename away; a thread title is any short line, and so is a refusal or a driver's own
   * error message — where nothing distinguishes an answer from a remark, the envelope is the
   * evidence, and {@link GenApi.json} is the call.
   */
  field(
    prompt: string,
    key: string,
    opts?: { system?: string; selection?: ModelSelection },
  ): Promise<string>;
}

/**
 * Conversations.
 *
 * A workspace is not one conversation: a branch you are on, a review you are half-way through, a
 * question from yesterday. These are the verbs a thread list needs; what it looks like is yours.
 *
 * Conversations are saved to the state directory as you go and restored at startup, so switching
 * away from one is not a way to lose it.
 */
/**
 * The terminals looking at this workspace.
 *
 * A workspace can have several and they are not copies of each other: each has its own
 * conversation on screen, its own scroll position, its own composer and its own panels. What they
 * share is the work — the conversations themselves, the turns running in them, everything a plugin
 * registered.
 *
 * A plugin that owns a **dock** has to open one panel per view, and `onOpen` is when. A plugin that
 * only opens floats in answer to a key needs none of this: the host puts a float in the terminal
 * whose key press opened it.
 */
export interface ViewApi {
  /** Every terminal, and what each is looking at. */
  list(): Promise<ViewInfo[]>;
  /** The one being served — the terminal whose key press is running. */
  current(): Promise<ViewInfo | null>;
  /**
   * The whole `neosh` namespace, bound to one terminal.
   *
   * Every call on it is the call it always was, except that a window opened through it lands
   * there. The same object a command handler is given as its third argument.
   */
  at(view: ViewId): Neosh;
  /**
   * A terminal arrived. Open your panel in it.
   *
   * Fired for every view that already exists when the plugin loads, too, so a plugin does not have
   * to decide whether it was here first.
   */
  onOpen(cb: (view: ViewId) => void): Disposable;
  /** A terminal went away. Its windows are already closed; let go of what you were keeping. */
  onClose(cb: (view: ViewId) => void): Disposable;
}

export interface SessionApi {
  /**
   * Most recently active first, with exactly one flagged `is_active`.
   *
   * Archived conversations are left out unless you ask for them. That is what archiving is for, and
   * a list that included them by default would make every caller responsible for remembering.
   */
  list(opts?: { includeArchived?: boolean }): Promise<SessionInfo[]>;
  current(): Promise<SessionInfo>;
  /**
   * Start one. It inherits the model and system prompt you are using — starting a conversation
   * should not silently change what you are talking to.
   *
   * `cwd` opens it against another checkout, which is how a second project becomes visible.
   */
  create(opts?: { cwd?: string; title?: string; activate?: boolean }): Promise<SessionInfo>;
  /**
   * Look at another conversation.
   *
   * Never refused, including while a turn is running. A turn belongs to its conversation and keeps
   * streaming into it; what you see is rebuilt from whichever one you switched to, and switching
   * back puts you in the middle of the answer where you left it.
   */
  switch(session: SessionId): Promise<void>;
  /**
   * Close one. Closing the active conversation moves to the most recently used other; closing the
   * last one is an error, because there is always somewhere for the next thing you type.
   *
   * A turn running in it is cancelled: there is about to be nowhere to put its answer.
   *
   * A conversation this workspace never loaded — one past the restore cap, which {@link stored}
   * is how you find — is deleted from disk just the same. One verb, whether the store is holding it
   * or only the directory is.
   */
  close(session: SessionId): Promise<void>;
  /** Pass `null` to clear a title and go back to the first-message label. */
  rename(session: SessionId, title: string | null): Promise<void>;
  /**
   * Put a conversation away, or bring it back.
   *
   * Not `close`: nothing is deleted, every message survives, and `list({ includeArchived: true })`
   * still finds it. Archiving the active conversation moves you to the most recently used other
   * one — or to a fresh empty one if there is no other.
   */
  archive(session: SessionId, archived?: boolean): Promise<void>;
  /**
   * Every conversation *on disk*, loaded or not — newest first.
   *
   * {@link list} answers about the workspace's store, and the store is a window rather than the
   * whole directory: a workspace restores the most recent few hundred conversations and leaves the
   * rest as files. Those files are in no list, which is fine until something has to say what has
   * accumulated or take it away — so an archive that only ever asked `list` would report a number
   * that was not the number, and empty itself down to a directory that was still full.
   *
   * The rows are ordinary {@link SessionInfo}s, so one renderer draws both. Which of them this
   * workspace is actually holding is the difference between the two calls, and a caller that cares
   * asks both and compares ids.
   *
   * It reads and parses every file, so it is answered off the host loop and is not something to put
   * on a redraw.
   */
  stored(): Promise<SessionInfo[]>;
  /** The conversation itself, for a transcript view that renders rather than replays. */
  messages(session?: SessionId): Promise<Message[]>;
  /**
   * Fires whenever a terminal is looking at a different conversation — switched, created, closed.
   *
   * `view` is which terminal moved. A workspace can have several and each is somewhere, so
   * "the active conversation" is a question with as many answers as there are screens.
   */
  onChange(cb: (e: { session: SessionId; view: ViewId }) => void): Disposable;
}

/**
 * The status line — the composer footer.
 *
 * The host owns the strip: one line, always visible, never scrolled. Plugins own what is in it.
 * That split is what lets the model switcher and the git plugin each put something there without
 * either knowing the other exists.
 *
 * Setting the same key again replaces that segment, so updating a meter every tick does not need a
 * clear first and cannot leave two. Segments are namespaced per plugin, so two plugins choosing
 * `"model"` cannot collide, and unloading a plugin takes its segments with it.
 */
/**
 * The shortcut row under the composer.
 *
 * Whoever owns a feature owns its hint, which is the only arrangement that stays true: the row is
 * built from what is actually registered right now, so a plugin that is switched off takes its
 * shortcut with it rather than leaving a key advertised that no longer does anything.
 *
 * Write the key the way the user would press it — `^P`, `⇧⏎`, `^Z` — not the way a keymap spells
 * it. Hints are dropped from the end when the terminal is too narrow, so put the one you would
 * most want seen at the lowest priority.
 */
export interface HintApi {
  set(key: string, hint: { keys: string; label: string; priority?: number }): Promise<void>;
  clear(key: string): Promise<void>;
}

export interface PermissionApi {
  mode(): Promise<PermissionMode>;
  setMode(mode: PermissionMode): Promise<PermissionMode>;
}

export interface StatusApi {
  /**
   * `keys` is drawn immediately after `text`, dimmed — the key that changes this thing, beside the
   * thing it changes. Write it the way the user would press it (`^P`, `^Z`), not the way a keymap
   * spells it.
   *
   * `short` is the same thing said in less room, and the strip asks for it before it drops your
   * segment. Give one to anything wide: without it a segment costs its full width or nothing, so
   * the widest thing in the strip is the first thing to vanish on a narrow terminal — which is
   * usually the thing worth the most. It is not a truncation and the host will not invent one; it
   * is the fact with a part left out, and only you know which part that is.
   *
   * `priority` is where the segment sits *and* what the strip gives up first, in reverse.
   */
  set(
    key: string,
    segment: {
      text: string;
      /** The same fact in fewer columns, used before this segment is dropped for want of room. */
      short?: string;
      keys?: string;
      hl?: string;
      align?: StatusAlign;
      priority?: number;
    },
  ): Promise<void>;
  clear(key: string): Promise<void>;
}

export interface OptionApi {
  /**
   * Declare an option this plugin owns.
   *
   * neosh's own settings are declared through this same call at startup, so there is nothing the
   * built-in options can do that yours cannot — including being set from `config.toml` and shown
   * by a settings UI that has never heard of your plugin.
   *
   * Names are dot-separated lowercase. Namespace yours under your plugin id.
   */
  declare(spec: OptionSpec): Promise<Disposable>;
  /** Typed read. Rejects if the option was never declared. */
  get<T = OptionValue>(name: string): Promise<T>;
  /** Full entry, including type, default and owner — or `null` if undeclared. */
  entry(name: string): Promise<OptionEntry | null>;
  /**
   * Set a declared option. Rejects on an unknown name or a value that does not match the declared
   * type, rather than quietly doing nothing.
   */
  set(name: string, value: OptionValue): Promise<void>;
  /** Restore the declared default. */
  reset(name: string): Promise<void>;
  /**
   * Set an option **and write it down** — into `[options]` in the user's `config.toml`, so it is
   * still the value after a restart. `undefined` puts the default back and takes the line out.
   *
   * The settings screen's verb, and deliberately not what `set` does: a plugin choosing a value at
   * runtime is saying what it is *now*, and an editor that rewrote your config file every time one
   * did would be one you stopped trusting with the file. Use this where a person picked a value on
   * a screen that says it is saved. The rest of the file is left exactly as it was.
   *
   * Rejects for a value of the wrong type before anything is written, and under `--clean`, which
   * has no file to write into.
   */
  save(name: string, value?: OptionValue): Promise<void>;
  all(): Promise<OptionEntry[]>;
  /** Fires for every option, not just your own: a setting is shared state. */
  onChange(cb: (e: { name: string; value: OptionValue }) => void): Disposable;
}

/**
 * What your plugin remembers between runs.
 *
 * Small facts a panel needs so it is still arranged the way you left it: which projects are pinned,
 * what order you dragged them into, which sections are folded. Deliberately *not* options — an
 * option is configuration the user writes, and a plugin that rewrites someone's config file because
 * they pressed a key is one they stop trusting with the file.
 *
 * Keyed by your plugin id, which the host knows and you cannot forge, so no other plugin can read
 * or clobber what you store. It is plain JSON on disk: **nothing secret belongs here.**
 */
export interface StateApi {
  /**
   * `null` when nothing was stored — indistinguishable from having stored `null`, which no caller
   * has ever needed to tell apart.
   */
  get<T = unknown>(key: string): Promise<T | null>;
  set(key: string, value: unknown): Promise<void>;
  remove(key: string): Promise<void>;
}

/**
 * What *everybody* remembers about a project or a conversation.
 *
 * The counterpart to `state`, and the difference is who may look. State is keyed by your plugin and
 * private, which is right for your fold set and wrong for "this project is a favourite" — with
 * state, a sidebar of somebody's own starts with no favourites and pinning one in ours is invisible
 * to it. A var is scoped to the thing it describes, and anyone may read or write it.
 *
 * Namespace your keys (`sidebar.favorite`, `acme.colour`) for the reason options are namespaced:
 * nothing stops two plugins choosing `colour`, and a prefix is what makes them not want to.
 *
 * Persisted, shared, and plain JSON on disk: **nothing secret belongs here.** A conversation's vars
 * are deleted with it; a project's outlive every conversation in it, because a project is a
 * directory and the directory is still there.
 */
export interface VarApi {
  get<T = unknown>(scope: VarScope, key: string): Promise<T | null>;
  set(scope: VarScope, key: string, value: unknown): Promise<void>;
  remove(scope: VarScope, key: string): Promise<void>;
  /** Everything on one scope, in one round trip. What a panel reads per project rather than per key. */
  all(scope: VarScope): Promise<Record<string, unknown>>;
  /**
   * A var changed, whoever changed it — including you. The signal to redraw on.
   *
   * `value` is `undefined` when it was removed.
   */
  onChange(
    cb: (e: { scope: VarScope; key: string; value: unknown }) => void,
  ): Disposable;
}

/**
 * The directories somebody works in, and the one thing that can happen to one.
 *
 * A project *is* its path everywhere else in this API — `projectScope` keys vars by it, a panel's
 * list is a list of them, `SessionInfo.cwd` names one. Which is exactly why a path that changes
 * needs saying out loud rather than inferring: from the outside, a worktree that moved and a
 * project that was deleted while another was added are the same two facts in the same order.
 */
export interface ProjectApi {
  /**
   * A project's directory is now somewhere else — a worktree that was relocated.
   *
   * The host has already moved everything it owns by the time this arrives: the conversations, the
   * project vars, the names and branches every list draws. What is left is whatever *you* keyed by
   * the old path — a list of directories, a cache, a decoration target — and the point of getting
   * both ends in one event is that you can re-key in place instead of dropping a row and gaining a
   * stranger.
   */
  onMove(cb: (e: { from: string; to: string }) => void): Disposable;
}

/** Sugar for the two scopes anything with a panel spends its time in. */
export function projectScope(cwd: string): VarScope {
  return { scope: "project", cwd };
}

export function sessionScope(session: SessionId): VarScope {
  return { scope: "session", session };
}

/**
 * How your plugin puts something in somebody else's panel.
 *
 * A *point* is a name a plugin agrees to read — `sidebar.section`, `project.action`, `palette.entry`
 * — and a contribution is a JSON item on it, conventionally carrying the name of a command to run.
 * The indirection is the whole trick: the sidebar renders rows it did not write and invokes commands
 * it has never heard of, and neither side imports the other.
 *
 * Data rather than a callback on purpose. A contribution can be listed by the palette, described in
 * `^Z` and disabled by the user, none of which is possible for a function held inside your closure.
 *
 * Your contributions are withdrawn when your plugin unloads, so `plugins.disabled` takes your rows
 * with it and there is no way to leave a row behind pointing at a command that no longer exists.
 */
export interface ExtensionApi {
  /**
   * Put an item on a point, replacing whatever you had there under the same `id`.
   *
   * Higher `priority` sorts first; ties break on plugin and id, so the order is stable across
   * restarts rather than being whatever order plugins happened to activate in.
   */
  contribute(
    point: string,
    id: string,
    item: unknown,
    opts?: { priority?: number },
  ): Promise<Disposable>;
  remove(point: string, id: string): Promise<void>;
  /** Everything on a point, in order, whoever contributed it. What a panel calls when it draws. */
  list<T = unknown>(point: string): Promise<Array<Contribution & { item: T }>>;
  /**
   * Somebody added to or withdrew from a point. Redraw.
   *
   * Without this a plugin that loads after your panel has drawn contributes rows nobody sees until
   * the next unrelated refresh — which on a quiet workspace is several seconds of a panel missing
   * half of itself.
   */
  onChange(cb: (e: { point: string }) => void): Disposable;
  /**
   * Every point anybody reads or writes: who declared it (`[provides] points` in their manifest)
   * and who has something on it. A point with contributors and no readers is almost always a
   * typo, and neosh says so at startup.
   */
  points(): Promise<PointInfo[]>;
  /**
   * Every plugin the workspace knows about, with its manifest and what became of it — `loaded`,
   * `held` until one of its activation triggers, or `failed` with the reason. The list a plugins
   * panel is drawn from.
   */
  plugins(): Promise<PluginInfo[]>;
}

/**
 * Saying that something happened, to whoever cares.
 *
 * Plugin-defined and broadcast — Neovim's `User` autocmd. Nothing validates the names; namespace
 * them like everything else.
 *
 * Fire and forget by construction. There is no reply and no way to be blocked, because an emitter
 * that could be is an emitter every listener is on the critical path of, which is how one slow
 * plugin wedges a panel. When you need an answer, register a command or read a contribution point.
 *
 * The host emits one of its own: **`neosh.ready`**, `from: "neosh"`, once every plugin has loaded.
 * Your `activate` returning is not that moment — the others are still loading alongside you, so a
 * command you would call is a name nothing answers to yet, a contribution point somebody else
 * fills is still empty, and a model a plugin registers is not selectable. Anything that depends on
 * the *rest* of the workspace goes in a `neosh.ready` listener rather than at the end of
 * `activate`. It is said again after `^R`, which is the same fact being true a second time.
 */
/** What the host says about a window on the bus: `neosh.win.enter`, `.leave`, `.open`. */
export interface WindowEvent {
  win: WindowId;
  buf: BufferId | null;
  /** The buffer's kind — the field to filter on. */
  kind: string | null;
}

export interface EventApi {
  emit(name: string, data?: unknown): Promise<void>;
  /**
   * Listen. `from` is the plugin that emitted it, stamped by the host — one of the few things in a
   * plugin message nobody can forge.
   *
   * You hear your own events too. Filtering on `from === ctx.plugin` is how you skip them, and it
   * is deliberately your choice: a panel that reacts to its own writes uniformly has one code path
   * instead of two.
   */
  /**
   * Hear one event by name. `kind` keeps only events whose `data.kind` matches — the way to
   * listen for `neosh.win.enter` on the sidebar and nothing else.
   *
   * The host's own, `from: "neosh"`: `neosh.ready`; `neosh.win.enter` / `neosh.win.leave` /
   * `neosh.win.open` (a {@link WindowEvent}); `neosh.win.close` (`{ win }`); `neosh.cursor`
   * (`{ win, row, col }`); `neosh.mode` (`{ mode }`); `neosh.viewport` (`{ win, width, height }`).
   * Neovim's autocmds, on the same bus a plugin's own events travel.
   */
  on(name: string, cb: (e: { data: unknown; from: string }) => void, opts?: { kind?: string }): Disposable;
  /** Every event, whatever it is called. For a logger or a debugger, rarely for a feature. */
  onAny(cb: (e: { name: string; data: unknown; from: string }) => void): Disposable;
}

/**
 * The other computers.
 *
 * ASCP — see `docs/ascp/SPEC.md`. Everything here is a *description* of what another machine is
 * running, or a *request* to it, and never a handle: an agent belongs to the node it was started on
 * for the whole of its life, because its files, its shell and its credentials are there.
 *
 * Empty and harmless when no swarm is configured, which is the default. A plugin that draws remote
 * agents needs no special case for the single-machine setup — `nodes()` is simply empty.
 */
/**
 * What the plan has left, and what the week went on.
 *
 * Two questions that look alike and are not. {@link list} is *now* — an opaque fraction of an
 * allowance the vendor enforces, which is the number that decides whether to start something.
 * {@link usage} is *history* — tokens and their money-equivalent, read out of the vendor CLIs' own
 * transcripts, which is the number that explains where the week went. Neither converts into the
 * other, and a chart that put them on one axis would be inventing an exchange rate.
 *
 * Everything here is data. Nothing in this API draws, which is what makes the bundled strip
 * replaceable by a panel of your own that reads exactly the same calls.
 */
/**
 * Finding out there is a newer neosh, and becoming it.
 *
 * The half that decides everything is not *is there something newer* but *may I replace this file*.
 * A binary under a Homebrew prefix belongs to Homebrew, and writing over it leaves `brew` describing
 * a version that is not on disk. So {@link UpdateStatus.method} is read off the running executable's
 * path, and only a `standalone` install is ever overwritten — everything else is handed the command
 * that would do it, for a person to run.
 */
export interface UpdateApi {
  /**
   * What is running, what is published, and what updating would mean on this machine.
   *
   * Answers from the last check unless `force`, so a panel row asking on every redraw costs
   * nothing. `latest` absent is *unknown* rather than up to date — a failed check that reads as
   * "you are current" is a machine that never updates and never says why, so draw
   * {@link UpdateStatus.error} when it is there.
   */
  check(force?: boolean): Promise<UpdateStatus>;
  /**
   * The same answer, from this machine alone, never touching the network.
   *
   * Which is enough for the half that matters most often. {@link UpdateStatus.restart_pending} is a
   * `stat` of one file — *is the binary on disk the binary this workspace is running* — and it goes
   * true when somebody runs `brew upgrade neosh` in another terminal, which is how most installs
   * are actually updated and the moment nothing used to say anything. Cheap enough to poll on a
   * tick; {@link check} is not, and a panel that used it for this would be a workspace making an
   * HTTP request every thirty seconds.
   */
  local(): Promise<UpdateStatus>;
  /**
   * Update, by whichever route this install takes.
   *
   * A `standalone` binary is downloaded, checked against the published sha256 and swapped in with
   * a rename — which is atomic and is allowed while the old binary is still executing, and is why
   * the answer carries `restart_required` rather than the new version simply being live. A managed
   * one is updated by **running its manager** — `brew update && brew upgrade neosh`, `npm install
   * -g`, `cargo install --force` — and then checked: a manager that exited 0 and left the binary
   * unchanged is `failed` with the command to run by hand, never `applied`.
   *
   * **It reports progress while it runs**, on the bus as {@link UPDATE_EVENT}: a `brew upgrade` is
   * a fetch and a `cargo install` is a compile, and a call that answers once at the end is a
   * progress row that spins on nothing for minutes. One at a time, workspace-wide — a second call
   * while one is running comes back `failed` saying so.
   */
  apply(): Promise<UpdateOutcome>;
  /**
   * Restart the workspace so a downloaded update takes effect.
   *
   * Rejects with `Busy`, naming them, while any turn is in flight in *any* conversation — a
   * restart ends turns belonging to terminals the caller cannot see. `force` goes ahead anyway,
   * and is for a caller that has said out loud what would be lost.
   */
  restart(force?: boolean): Promise<void>;
}

export interface QuotaApi {
  /**
   * The latest snapshot for every instance that has one, freshest observation first.
   *
   * Answers from what the workspace kept rather than by asking a vendor, so it costs nothing and is
   * safe to call on every redraw. `observed_at` is how stale each one is — draw it, because a
   * percentage with no age on it is one people will trust for longer than they should.
   */
  list(): Promise<QuotaSnapshot[]>;
  /**
   * Ask the vendor now, for one instance or for every instance that can be asked.
   *
   * Resolves as soon as the request is *made*. A poll is a network round trip or a process spawn,
   * so a panel that awaited the answer would be a panel that opens late; the answer arrives at
   * {@link onChange}, the same way an unprompted one does, so there is one code path rather than
   * two.
   */
  refresh(instance?: string): Promise<void>;
  /**
   * Publish a snapshot for an instance your own driver serves.
   *
   * The other half of {@link ProviderApi.register}: a provider written as a plugin knows its
   * vendor's allowance and nothing in the workspace does. Reported this way it is kept, sampled,
   * broadcast and drawn exactly like a built-in one. Rejects for an instance your plugin did not
   * register a driver for — a figure anybody could spoof is not one this should draw as fact.
   */
  report(snapshot: QuotaSnapshot): Promise<void>;
  /**
   * Every percentage this workspace has seen in a span, so a gauge can be a line.
   *
   * `since` and `until` are unix **seconds**, `until` exclusive. Sampled on change, so the points
   * are unevenly spaced and the gaps are real: a stretch with no samples is a stretch where the
   * machine was off, and drawing a straight line across it invents usage that did not happen.
   */
  history(opts: { since: number; until: number; instance?: string }): Promise<QuotaSample[]>;
  /**
   * Tokens and their money-equivalent over a span, from the vendor CLIs' transcripts.
   *
   * Not from this workspace's conversations: a turn you ran in `claude` directly spent the same
   * allowance, and a history that could not see it would answer a different question. Reads
   * thousands of files, so this is a call a panel makes when it opens — never one it makes on a
   * redraw.
   *
   * `cost_usd` is what those tokens would cost at API rates. It is not money spent: a subscription
   * bills separately. Check `fully_priced` before putting a currency symbol in a heading.
   */
  usage(opts: {
    since: number;
    until: number;
    resolution: UsageResolution;
    /** IANA zone to bucket days in. Defaults to this machine's. */
    timeZone?: string;
  }): Promise<UsageHistory>;
  /**
   * Any account's allowance changed, whatever moved it: a driver reporting mid-turn, a poll
   * landing, a plugin publishing its own.
   *
   * One thing to listen to instead of knowing which vendors exist — which is what lets a strip draw
   * a provider that shipped after it did.
   */
  onChange(cb: (snapshot: QuotaSnapshot) => void): Disposable;
}

export interface SwarmApi {
  /** This machine, or `null` when the swarm is not running. */
  self(): Promise<NodeInfo | null>;
  /** Every node known, up or down. A node that has gone keeps its agents, marked `up: false`. */
  nodes(): Promise<SwarmNode[]>;
  /** Every agent on every reachable node, flattened, each carrying the node it belongs to. */
  agents(): Promise<SwarmAgent[]>;
  /**
   * Which *other* machines have this project.
   *
   * The answer to "is this project on more than this computer". Empty when it is only here. Keyed
   * on {@link ProjectKey}, which is the normalised git remote — the one thing about a checkout that
   * is the same on every machine that has it.
   */
  hostsOf(project: ProjectKey): Promise<string[]>;
  /**
   * Ask a node to do something with one of its agents.
   *
   * Settles when that machine answers — every ASCP command is answered exactly once — and rejects
   * with the owner's reason when it says no. A node may refuse anything; `NodeCapabilities` on its
   * {@link SwarmNode} says in advance what it is likely to accept, so a menu can grey out a verb
   * rather than offering one that will bounce.
   *
   * Answers with the conversation a `new_session` created, and `null` for everything else — which
   * is what makes starting something over there a place you can then go: pass it to
   * {@link subscribe}, or to `swarm.open`, without waiting for the next inventory to notice it.
   */
  command(node: NodeId, session: string, command: AgentCommand): Promise<string | null>;
  /**
   * Which directories on another machine start with `prefix` — `path.complete`, asked over there.
   *
   * The same answers in the same shape as the local one: each entry as you would have typed it,
   * trailing separator and all, so a single field can complete against either machine and the only
   * thing that changes is which one is asked. Directory names only.
   *
   * {@link NodeCapabilities.projects} is what a machine *offers*, and it is the short list of
   * places it already works in; this is how you reach the directory over there that neither machine
   * has ever opened. Rejects when the peer is not connected, when it does not accept commands, and
   * when it is running a neosh too old to have heard of the question — the message says which, and
   * an empty list is a directory with nothing in it rather than a failure.
   *
   * Passing this machine's own node id answers locally, so a picker that walks `nodes()` needs no
   * special case for the row that is itself.
   */
  browse(node: NodeId, prefix?: string): Promise<string[]>;
  /**
   * Open a **shell** on another machine, in a tab of its own.
   *
   * The same terminal `<C-w>T` opens here, with the pty on the other end of a link: the emulator
   * runs in this workspace, exactly as it does over ssh, and what crosses the wire is bytes. Every
   * key goes to the child, `^C` interrupts *it*, and the window prefix is the one way out.
   *
   * `cwd` is a path on the **owner** — learnt from a conversation over there or from
   * {@link NodeCapabilities.projects}, never invented — and `undefined` is that user's home
   * directory.
   *
   * Rejects when the peer is not connected, when it is too old to have heard of the question, and,
   * the one that is a decision rather than a fact, when it has not been told to open shells for
   * other machines. That is `accepts_shells` under `[swarm]`, off by default and deliberately not
   * covered by `accepts_commands`: starting an agent over there is a thing with a permission layer
   * over it, and a prompt is not. The rejection names the setting.
   */
  shell(node: NodeId, cwd?: string): Promise<void>;
  /**
   * Watch a remote conversation: its history now, then everything as it happens, delivered to
   * {@link onStream}.
   *
   * Drop it when you stop looking. A subscription is the difference between a quiet swarm and one
   * where every machine sends every token to every other one.
   */
  subscribe(node: NodeId, session: string): Promise<void>;
  unsubscribe(node: NodeId, session: string): Promise<void>;
  /**
   * Open a conversation on another machine **as a conversation** — in the pane, with the same
   * transcript, cards and composer as one of yours, what you send going to that machine and `Esc`
   * asking it to stop. Answers with the local conversation that shows it (its `mirror` says whose it
   * is), after moving the terminal that asked into it. Opening the same one again finds it rather
   * than making a second. It is never listed and never saved, and it is let go of when nothing
   * shows it.
   */
  mirror(node: NodeId, session: string, opts?: { view?: ViewId }): Promise<SessionInfo>;
  /**
   * Ask what machine is at an address, without joining it.
   *
   * The first half of pairing. A node presents its identity to anything that connects — as an SSH
   * server presents a host key — so what comes back was *proven*, not claimed, which is what makes
   * it safe to show somebody and ask. Rejects, with the address in the message, when nothing
   * answers.
   */
  probe(addr: string): Promise<NodeInfo>;
  /**
   * Authorise a machine and start connecting to it.
   *
   * Immediate: no restart. Written to the state directory rather than to `config.toml`, because an
   * editor that edits your config file because you pressed a key is one you stop trusting with it.
   */
  pair(node: NodeId, opts?: { name?: string; addr?: string }): Promise<void>;
  /** Withdraw authorisation and stop connecting. Refuses for a machine your config declared. */
  unpair(node: NodeId): Promise<void>;
  /**
   * Call a machine something else — an SSH `Host` alias, and for the same reasons.
   *
   * A hostname is chosen by whoever set the machine up, is often `Mac` or `ubuntu` on every box
   * somebody owns, and is not what its owner calls it. This wins over the announced name
   * everywhere a `SwarmNode` is read, so every panel agrees; `null` gives the announced name back.
   *
   * Refuses for a machine your `config.toml` declared, naming the `name` field to change instead —
   * an alias stored beside a config peer would be reverted by the next reload.
   */
  rename(node: NodeId, name: string | null): Promise<void>;
  /**
   * Dial a down peer again now, rather than waiting out its retry delay.
   *
   * Also what lifts a {@link disconnect}: for a peer that dials *this* machine, being willing to
   * answer again is the whole of what "reconnect" can mean.
   */
  reconnect(node: NodeId): Promise<void>;
  /**
   * Close the connection to a peer and stop dialling it, keeping the pairing.
   *
   * Holds until {@link reconnect} or a restart — the peer stays authorised, so this is "leave it
   * alone for now", and {@link unpair} is the stronger verb. Its `SwarmNode` row goes
   * `link: down`, with its agents still described.
   */
  disconnect(node: NodeId): Promise<void>;
  /**
   * Machines that proved who they are and have not been paired with.
   *
   * `dialled` says which question to ask: `true` is "this is what is at that address — add it?",
   * `false` is "this machine wants to join — allow it?". Same button, different question.
   */
  strangers(): Promise<SwarmStranger[]>;
  /** A node joined, left, changed what it is running, or asked to join. Redraw. */
  onChange(cb: () => void): Disposable;
  onStream(
    cb: (e: { node: NodeId; session: string; event: StreamEvent }) => void,
  ): Disposable;
}

export interface RuntimePathApi {
  /**
   * Add a directory to search for plugins.
   *
   * Only effective before discovery runs, which means: from `init.ts`. That is the point — your
   * config is the thing that decides what else loads, exactly as `init.lua` is.
   */
  add(path: string): Promise<void>;
  list(): Promise<string[]>;
}

/**
 * Completing a typed path.
 *
 * The runtime has no filesystem — deliberately — and a path field without completion is a path
 * field you type wrong. Narrow on purpose: directory names, one level, never file contents and
 * never a recursive walk.
 */
export interface PathApi {
  /**
   * Directories whose path begins with `prefix`, each with a trailing `/` so the answer can go
   * straight back into the field and be completed again.
   *
   * `~` expands against the home directory. A prefix with no `/` completes against the active
   * conversation's directory, which is what someone typing `src` means.
   *
   * `denied` is set when the list is empty *because the operating system refused*, rather than
   * because nothing matched. The two are the same empty array and they are not the same answer:
   * macOS grants folder access to the **terminal**, so `~/Documents` full of projects reads as
   * empty until somebody ticks a box, and a field that draws "no directory matches" over that
   * sends people to go and check a path that was right all along. When it is set it says which
   * folder, which application and which System Settings pane, and it is meant to be shown.
   */
  complete(prefix: string): Promise<{ paths: string[]; denied?: string }>;
}

export interface TimerApi {
  /**
   * Run `fn` once, no sooner than `ms` from now.
   *
   * The returned `Disposable` cancels it, and **every timer is cancelled when your plugin
   * unloads** — which the global `setTimeout` cannot promise, because it has no idea who called it.
   * Prefer this one; reach for the global only when porting code that expects it.
   */
  after(ms: number, fn: () => void): Disposable;
  /** Run `fn` repeatedly until disposed. */
  every(ms: number, fn: () => void): Disposable;
  /**
   * Coalesce bursts: calling the returned function repeatedly runs `fn` once, `ms` after the last
   * call. The shape almost every "re-render after the tokens stop" wants.
   */
  debounce<A extends unknown[]>(ms: number, fn: (...args: A) => void): ((...args: A) => void) & Disposable;
}

export interface Logger {
  info(msg: string): void;
  warn(msg: string): void;
  error(msg: string): void;
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

interface Registered {
  commands: Map<string, (args: string[], key?: KeyContext, here?: Neosh) => unknown>;
  tools: Map<string, (input: unknown) => ToolResult | Promise<ToolResult>>;
  hooks: Map<HookName, (p: HookPayload) => HookOutcome | Promise<HookOutcome>>;
  providers: Map<string, (req: TurnRequest, emit: (e: ProviderEvent) => void, signal: { cancelled: boolean }) => unknown>;
  bufferListeners: Map<number, Array<(e: { buf: BufferId; start: number; oldEnd: number; newEnd: number }) => void>>;
  optionListeners: Array<(e: { name: string; value: OptionValue }) => void>;
  /// The protocol version this plugin was handed, kept so a namespace can be rebuilt for a
  /// view long after `__createContext` returned.
  version: number;
  sessionListeners: Array<(e: { session: SessionId; view: ViewId }) => void>;
  viewOpenListeners: Array<(view: ViewId) => void>;
  viewCloseListeners: Array<(view: ViewId) => void>;
  selectionListeners: Array<(e: { selection: ModelSelection }) => void>;
  composerListeners: Array<(e: { text: string }) => void>;
  activityListeners: Array<(e: { session: SessionId; turn: string; activity: Activity }) => void>;
  varListeners: Array<(e: { scope: VarScope; key: string; value: unknown }) => void>;
  projectMovedListeners: Array<(e: { from: string; to: string }) => void>;
  swarmListeners: Array<() => void>;
  quotaListeners: Array<(snapshot: QuotaSnapshot) => void>;
  swarmStreamListeners: Array<
    (e: { node: NodeId; session: string; event: StreamEvent }) => void
  >;
  contributionListeners: Array<(e: { point: string }) => void>;
  highlightListeners: Array<(e: { names: string[] }) => void>;
  focusListeners: Array<(e: { win: WindowId | null }) => void>;
  viewListeners: Array<() => void>;
  shutdownListeners: Array<() => void>;
  /**
   * Listeners by event name, plus `null` for the ones that asked for everything.
   *
   * Filtered here rather than in the host on purpose: a subscription table on the far side of the
   * boundary is one the host can only ever guess is current, and getting it wrong means a plugin
   * silently stops hearing things. Broadcasting everything and matching a string is cheap.
   */
  eventListeners: Map<string | null, Array<(e: { name: string; data: unknown; from: string }) => void>>;
  options: Set<string>;
  /// Timer handles this plugin armed, cleared on unload.
  timers: Set<number>;
  agentListeners: { [K in "turnStart" | "token" | "thinking" | "turnEnd" | "toolStart" | "toolEnd"]: Array<(e: never) => void> };
  streams: Map<string, { cancelled: boolean }>;
  subscriptions: Disposable[];
}

const plugins = new Map<string, Registered>();

function reg(plugin: string): Registered {
  let r = plugins.get(plugin);
  if (!r) {
    r = {
      commands: new Map(),
      tools: new Map(),
      hooks: new Map(),
      providers: new Map(),
      bufferListeners: new Map(),
      optionListeners: [],
      version: 0,
      sessionListeners: [],
      viewOpenListeners: [],
      viewCloseListeners: [],
      selectionListeners: [],
      composerListeners: [],
      activityListeners: [],
      varListeners: [],
      projectMovedListeners: [],
      swarmListeners: [],
      quotaListeners: [],
      swarmStreamListeners: [],
      contributionListeners: [],
      highlightListeners: [],
      focusListeners: [],
      viewListeners: [],
      shutdownListeners: [],
      eventListeners: new Map(),
      options: new Set(),
      timers: new Set(),
      agentListeners: { turnStart: [], token: [], thinking: [], turnEnd: [], toolStart: [], toolEnd: [] },
      streams: new Map(),
      subscriptions: [],
    };
    plugins.set(plugin, r);
  }
  return r;
}

/** A listener on one event name, or on all of them when `name` is `null`. */
function eventListener(
  r: Registered,
  name: string | null,
  cb: (e: { name: string; data: unknown; from: string }) => void,
): Disposable {
  const list = r.eventListeners.get(name) ?? [];
  r.eventListeners.set(name, list);
  return listener(list, cb);
}

function listener<T>(list: Array<(e: T) => void>, cb: (e: T) => void): Disposable {
  list.push(cb as (e: never) => void as (e: T) => void);
  return {
    dispose() {
      const i = list.indexOf(cb);
      if (i >= 0) list.splice(i, 1);
    },
  };
}

function floatConfig(o: FloatOptions = {}): FloatConfig {
  return {
    anchor: o.anchor ?? { kind: "screen" },
    offset: o.offset ?? { row: 0, col: 0 },
    width: o.width ?? { kind: "auto" },
    height: o.height ?? { kind: "auto" },
    z: o.z ?? 100,
    border: o.border ?? "rounded",
    border_hl: o.borderHl ?? null,
    title: o.title ?? null,
    footer: o.footer ?? null,
    close_on_blur: o.closeOnBlur ?? false,
    focusable: o.focusable ?? true,
    modal: o.modal ?? false,
    scroll: o.scroll ?? false,
  };
}

function markOpts(o: MarkOptions = {}): ExtmarkOpts {
  return {
    end_col: o.endCol ?? null,
    hl_group: o.hlGroup ?? null,
    line_hl_group: o.lineHlGroup ?? null,
    virt_text: (o.virtText ?? []).map((v) => ({ text: v.text, hl_group: v.hlGroup ?? null })),
    virt_text_pos: o.virtTextPos ?? "eol",
    on_delete: o.onDelete ?? "clamp",
    priority: o.priority ?? 0,
    image: o.image ? { path: o.image.path, media_type: o.image.mediaType } : null,
  };
}

/**
 * Build the whole `neosh` namespace over one pair of call functions.
 *
 * Called once per plugin with calls that say nothing about where they land, which is the ordinary
 * `neosh`: the host works out which terminal a window belongs in, and for anything done in answer
 * to a key that is exact. And again, per view, with calls that name it — which is what a command
 * handler is handed as its third argument. Inside a handler, `here.win.open(...)` is a panel in
 * the terminal the key was pressed in, and every other call on `here` is the call it always was.
 */
function build(
  plugin: string,
  version: number,
  r: ReturnType<typeof reg>,
  view: ViewId | null,
): Neosh {
  // On the envelope rather than in each call: a call comes *from* a terminal, exactly as a key
  // press does, and it is the same fact for all of them. It matters for far more than opening a
  // window — which conversation is on screen, what is in the composer, what `session.list` marks
  // as the one you are in are all questions with an answer per terminal.
  const c = (x: ApiCall) => call(plugin, x, view ?? undefined);
  const n = (x: ApiCall) => notify(plugin, x, view ?? undefined);

  const api: Neosh = {
    version,
    notify(message, level) {
      n({ call: "notify", level: level ?? "info", message, kind: "reply" });
    },
    progress(key, message) {
      n({ call: "notify", level: "info", message, kind: "progress", key });
    },
    done(key) {
      n({ call: "notify_done", key });
    },
    async alert(title, message, opts) {
      // Awaited rather than fired and forgotten, unlike the three above: this is the one that can
      // be refused — for want of `notify` in the manifest — and a capability error nobody sees is
      // a plugin that silently never notifies.
      await c({
        call: "alert",
        level: opts?.level ?? "info",
        title,
        message,
        session: opts?.session,
      });
    },
    async permit(capability) {
      return expect(await c({ call: "permission_check", capability }), "permission").decision;
    },
    onViewAttached(cb) {
      return listener(r.viewListeners, cb);
    },
    onShutdown(cb) {
      return listener(r.shutdownListeners, cb);
    },
    async ask(questions) {
      return expect(await c({ call: "ask_user", questions }), "answers").answers ?? null;
    },
    opt: {
      async declare(spec) {
        await c({ call: "opt_declare", spec });
        r.options.add(spec.name);
        return { dispose: () => r.options.delete(spec.name) };
      },
      async get<T = OptionValue>(name: string): Promise<T> {
        const entry = expect(await c({ call: "opt_get", name }), "option").entry;
        if (!entry) throw new Error(`option ${name} is not declared`);
        return entry.value as T;
      },
      async entry(name) {
        return expect(await c({ call: "opt_get", name }), "option").entry;
      },
      async set(name, value) {
        await c({ call: "opt_set", name, value });
      },
      async reset(name) {
        await c({ call: "opt_reset", name });
      },
      async save(name, value) {
        await c({ call: "opt_persist", name, value: value ?? null });
      },
      async all() {
        return expect(await c({ call: "opt_all" }), "options").options;
      },
      onChange(cb) {
        return listener(r.optionListeners, cb);
      },
    },
    edit: {
      async move(win, motion, opts) {
        await c({ call: "win_motion", win, motion, select: opts?.select ?? false });
      },
      async apply(win, edit) {
        await c({ call: "win_edit", win, edit });
      },
      async select(win, on) {
        await c({ call: "win_select", win, on });
      },
      async selectShape(win, shape) {
        await c({ call: "win_select_shape", win, shape });
      },
      async cursorShape(win, shape) {
        await c({ call: "win_cursor_shape", win, shape });
      },
      async selection(win) {
        return expect(await c({ call: "win_selection", win }), "text").text;
      },
      async copy(text) {
        await c({ call: "clipboard_write", text });
      },
    },
    path: {
      async complete(prefix) {
        const answer = expect(await c({ call: "path_complete", prefix }), "paths");
        return { paths: answer.paths, denied: answer.denied ?? undefined };
      },
    },
    state: {
      async get<T = unknown>(key: string): Promise<T | null> {
        const value = expect(await c({ call: "state_get", key }), "json").value;
        return (value ?? null) as T | null;
      },
      async set(key, value) {
        await c({ call: "state_set", key, value });
      },
      async remove(key) {
        await c({ call: "state_delete", key });
      },
    },
    vars: {
      async get<T = unknown>(scope: VarScope, key: string): Promise<T | null> {
        const value = expect(await c({ call: "var_get", scope, key }), "json").value;
        return (value ?? null) as T | null;
      },
      async set(scope, key, value) {
        await c({ call: "var_set", scope, key, value });
      },
      async remove(scope, key) {
        await c({ call: "var_delete", scope, key });
      },
      async all(scope) {
        return expect(await c({ call: "var_all", scope }), "vars").vars;
      },
      onChange(cb) {
        return listener(r.varListeners, cb);
      },
    },
    project: {
      onMove(cb) {
        return listener(r.projectMovedListeners, cb);
      },
    },
    ext: {
      async contribute(point, id, item, opts) {
        await c({ call: "ext_contribute", point, id, item, priority: opts?.priority ?? 0 });
        return {
          dispose() {
            void c({ call: "ext_remove", point, id });
          },
        };
      },
      async remove(point, id) {
        await c({ call: "ext_remove", point, id });
      },
      async list<T = unknown>(point: string) {
        const got = expect(await c({ call: "ext_list", point }), "contributions").contributions;
        return got as Array<Contribution & { item: T }>;
      },
      onChange(cb) {
        return listener(r.contributionListeners, cb);
      },
      async points() {
        return expect(await c({ call: "ext_points" }), "points").points;
      },
      async plugins() {
        return expect(await c({ call: "plugin_list" }), "plugins").plugins;
      },
    },
    swarm: {
      async self() {
        return expect(await c({ call: "swarm_self" }), "swarm_self").node ?? null;
      },
      async nodes() {
        return expect(await c({ call: "swarm_nodes" }), "swarm_nodes").nodes;
      },
      async agents() {
        return expect(await c({ call: "swarm_agents" }), "swarm_agents").agents;
      },
      async probe(addr) {
        const found = expect(await c({ call: "swarm_probe", addr }), "swarm_self").node;
        if (!found) throw new NeoshError({ kind: "not_found", what: addr });
        return found;
      },
      async pair(node, opts) {
        await c({
          call: "swarm_pair",
          node,
          name: opts?.name ?? "",
          addr: opts?.addr ?? null,
        });
      },
      async unpair(node) {
        await c({ call: "swarm_unpair", node });
      },
      async rename(node, name) {
        await c({ call: "swarm_rename", node, name });
      },
      async reconnect(node) {
        await c({ call: "swarm_reconnect", node });
      },
      async disconnect(node) {
        await c({ call: "swarm_disconnect", node });
      },
      async strangers() {
        return expect(await c({ call: "swarm_strangers" }), "swarm_strangers").strangers;
      },
      async hostsOf(project) {
        return expect(await c({ call: "swarm_hosts_of", project }), "names").names;
      },
      async command(node, session, command) {
        const done = await c({ call: "swarm_command", node, session, command });
        // Only `new_session` says anything, so an `ok` of any other shape is an older peer
        // answering a command that had nothing to report — which is a success, not a mismatch.
        return done.ok === "swarm_commanded" ? done.session ?? null : null;
      },
      async browse(node, prefix) {
        return expect(await c({ call: "swarm_browse", node, prefix: prefix ?? "" }), "paths").paths;
      },
      async shell(node, cwd) {
        await c({ call: "swarm_shell", node, cwd: cwd ?? null });
      },
      async subscribe(node, session) {
        await c({ call: "swarm_subscribe", node, session });
      },
      async unsubscribe(node, session) {
        await c({ call: "swarm_unsubscribe", node, session });
      },
      async mirror(node, session, opts) {
        return expect(
          await c({ call: "swarm_mirror", node, session, view: opts?.view ?? null }),
          "session",
        ).session;
      },
      onChange(cb) {
        return listener(r.swarmListeners, cb);
      },
      onStream(cb) {
        return listener(r.swarmStreamListeners, cb);
      },
    },
    update: {
      async check(force) {
        return expect(
          await c({ call: "update_check", force: force ?? false, local: false }),
          "update",
        ).update;
      },
      async local() {
        return expect(
          await c({ call: "update_check", force: false, local: true }),
          "update",
        ).update;
      },
      async apply() {
        return expect(await c({ call: "update_apply" }), "update_applied").outcome;
      },
      async restart(force) {
        await c({ call: "update_restart", force: force ?? false });
      },
    },
    quota: {
      async list() {
        return expect(await c({ call: "quota_list" }), "quotas").quotas;
      },
      async refresh(instance) {
        await c({ call: "quota_refresh", instance: instance ?? null });
      },
      async report(snapshot) {
        await c({ call: "quota_report", snapshot });
      },
      async history(opts) {
        const got = await c({
          call: "quota_history",
          instance: opts.instance ?? null,
          since: opts.since,
          until: opts.until,
        });
        return expect(got, "quota_history").samples;
      },
      async usage(opts) {
        const got = await c({
          call: "usage_history",
          since: opts.since,
          until: opts.until,
          resolution: opts.resolution,
          time_zone: opts.timeZone ?? null,
        });
        return expect(got, "usage_history").history;
      },
      onChange(cb) {
        return listener(r.quotaListeners, cb);
      },
    },
    event: {
      async emit(name, data) {
        await c({ call: "event_emit", name, data: data ?? null });
      },
      on(name, cb, opts) {
        const kind = opts?.kind;
        return eventListener(r, name, (e) => {
          if (kind !== undefined) {
            const d = e.data as { kind?: unknown } | null | undefined;
            if (d?.kind !== kind) return;
          }
          cb({ data: e.data, from: e.from });
        });
      },
      onAny(cb) {
        return eventListener(r, null, cb);
      },
    },
    timer: {
      after(ms, fn) {
        const id = setTimeout(() => {
          r.timers.delete(id);
          fn();
        }, ms);
        r.timers.add(id);
        return {
          dispose() {
            clearTimeout(id);
            r.timers.delete(id);
          },
        };
      },
      every(ms, fn) {
        const id = setInterval(fn, ms);
        r.timers.add(id);
        return {
          dispose() {
            clearInterval(id);
            r.timers.delete(id);
          },
        };
      },
      debounce<A extends unknown[]>(ms: number, fn: (...args: A) => void) {
        let pending: number | undefined;
        const run = (...args: A) => {
          if (pending !== undefined) {
            clearTimeout(pending);
            r.timers.delete(pending);
          }
          const id = setTimeout(() => {
            pending = undefined;
            // Prune here too, not only on the re-arm and dispose paths: a debounce that settles is
            // the *normal* outcome, and leaving its handle behind grows the set once per burst for
            // the life of the plugin.
            r.timers.delete(id);
            fn(...args);
          }, ms);
          pending = id;
          r.timers.add(id);
        };
        run.dispose = () => {
          if (pending !== undefined) {
            clearTimeout(pending);
            r.timers.delete(pending);
            pending = undefined;
          }
        };
        return run;
      },
    },
    rtp: {
      async add(path) {
        await c({ call: "rtp_add", path });
      },
      async list() {
        return expect(await c({ call: "rtp_list" }), "paths").paths;
      },
    },
    log: {
      info: (message) => n({ call: "log", level: "info", message }),
      warn: (message) => n({ call: "log", level: "warn", message }),
      error: (message) => n({ call: "log", level: "error", message }),
    },
    buf: {
      async create(opts) {
        return expect(await c({
          call: "buf_create",
          name: opts?.name ?? null,
          scratch: opts?.scratch ?? false,
          kind: opts?.kind ?? null,
        }), "buf").buf;
      },
      async lineCount(buf) {
        return expect(await c({ call: "buf_line_count", buf }), "count").n;
      },
      async getLines(buf, start, end) {
        return expect(await c({ call: "buf_get_lines", buf, start, end }), "lines").lines;
      },
      async setLines(buf, start, end, lines) {
        await c({ call: "buf_set_lines", buf, start, end, lines });
      },
      async render(buf, ns, start, end, rows) {
        await c({
          call: "buf_render",
          buf,
          ns,
          start,
          end,
          lines: rows.map((r) => ({
            text: r.text,
            marks: (r.marks ?? []).map((m) => ({ col: m.col, ...markOpts(m.opts) })),
          })),
        });
      },
      async appendText(buf, text) {
        await c({ call: "buf_append_text", buf, text });
      },
      async setName(buf, name) {
        await c({ call: "buf_set_name", buf, name });
      },
      async setKind(buf, kind) {
        await c({ call: "buf_set_kind", buf, kind });
      },
      async kind(buf) {
        return expect(await c({ call: "buf_get_kind", buf }), "maybe_text").text ?? null;
      },
      async onChange(buf, cb) {
        await c({ call: "buf_attach", buf });
        const list = r.bufferListeners.get(buf) ?? [];
        r.bufferListeners.set(buf, list);
        return listener(list, cb);
      },
    },
    pane: {
      async active(view) {
        const { tabs, active } = expect(await c({ call: "tab_list", view }), "tabs");
        // The active tab, or the first — a terminal always has at least one, and a `tabs` that
        // came back empty is a workspace that has gone wrong in a way this cannot paper over.
        const tab = tabs.find((t) => t.id === active) ?? tabs[0];
        if (!tab) throw new Error("this terminal has no tabs");
        return tab.active_pane;
      },
      async split(pane, dir) {
        const p = expect(await c({ call: "pane_split", pane, dir }), "pane").pane;
        if (p === null) throw new Error(`pane ${pane} could not be split`);
        return p;
      },
      async close(pane) {
        await c({ call: "pane_close", pane });
      },
      async focus(pane) {
        await c({ call: "pane_focus", pane });
      },
      async focusDir(dir, view) {
        return expect(await c({ call: "pane_focus_dir", dir, view }), "pane").pane;
      },
      async resize(pane, dir, delta) {
        await c({ call: "pane_resize", pane, dir, delta });
      },
      async swap(pane, withPane) {
        await c({ call: "pane_swap", pane, with: withPane });
      },
      async moveToEdge(pane, dir) {
        await c({ call: "pane_move_edge", pane, dir });
      },
      async equalize(view) {
        await c({ call: "pane_equalize", view });
      },
    },
    tab: {
      async list(view) {
        const r = expect(await c({ call: "tab_list", view }), "tabs");
        return { tabs: r.tabs, active: r.active };
      },
      async create(opts) {
        return expect(
          await c({
            call: "tab_new",
            view: opts?.view,
            title: opts?.title,
            activate: opts?.activate ?? true,
            group: opts?.group,
          }),
          "tab",
        ).tab;
      },
      async group(tab, group) {
        await c({ call: "tab_group", tab, group });
      },
      async close(tab) {
        await c({ call: "tab_close", tab });
      },
      async select(tab) {
        await c({ call: "tab_select", tab });
      },
      async step(delta, view) {
        await c({ call: "tab_step", delta, view });
      },
      async move(tab, to) {
        await c({ call: "tab_move", tab, to });
      },
      async rename(tab, title) {
        await c({ call: "tab_rename", tab, title });
      },
    },
    win: {
      async open(buf, dock, opts) {
        const layout: WindowLayout = {
          kind: "docked",
          pane: opts?.pane ?? null,
          dock,
          size: opts?.size ?? null,
          gravity: opts?.gravity ?? "start",
          wrap: opts?.wrap ?? null,
        };
        return expect(await c({ call: "win_open", buf, layout }), "win").win;
      },
      async close(win) {
        await c({ call: "win_close", win });
      },
      async resize(win, size) {
        await c({ call: "win_resize", win, size });
      },
      async setBuf(win, buf) {
        await c({ call: "win_set_buf", win, buf });
      },
      async cursor(win) {
        const v = expect(await c({ call: "win_get_cursor", win }), "cursor");
        return { row: v.row, col: v.col };
      },
      async setCursor(win, row, col) {
        await c({ call: "win_set_cursor", win, row, col });
      },
      async scrollTo(win, topLine) {
        await c({ call: "win_scroll_to", win, top_line: topLine });
      },
      async scroll(win, amount) {
        await c({ call: "win_scroll", win, amount });
      },
      async viewport(win) {
        return expect(await c({ call: "win_get_viewport", win }), "viewport").viewport ?? null;
      },
      async list() {
        return expect(await c({ call: "win_list" }), "windows").windows;
      },
      async ofKind(kind) {
        const windows = expect(await c({ call: "win_list" }), "windows").windows;
        return windows.filter((w) => w.kind === kind);
      },
      async setHighlights(target, map) {
        const t: HlTarget = "win" in target
          ? { kind: "window", win: target.win }
          : { kind: "kind", name: target.kind };
        await c({ call: "win_set_highlights", target: t, map });
      },
    },
    float: {
      async open(buf, opts) {
        return expect(await c({ call: "float_open", buf, config: floatConfig(opts) }), "win").win;
      },
      async configure(win, opts) {
        await c({ call: "float_configure", win, config: floatConfig(opts) });
      },
      async close(win) {
        await c({ call: "win_close", win });
      },
    },
    ns: {
      async create(name) {
        return expect(await c({ call: "ns_create", name }), "ns").ns;
      },
      async mark(ns, buf, row, col, opts) {
        return expect(await c({ call: "mark_set", ns, buf, row, col, opts: markOpts(opts) }), "mark").id;
      },
      async getMark(ns, buf, id) {
        return expect(await c({ call: "mark_get", ns, buf, id }), "mark_info").info;
      },
      async allMarks(ns, buf) {
        return expect(await c({ call: "mark_all", ns, buf }), "marks").marks;
      },
      async delMark(ns, buf, id) {
        await c({ call: "mark_del", ns, buf, id });
      },
      async clear(ns, buf, start, end) {
        await c({ call: "mark_clear", ns, buf, start: start ?? null, end: end ?? null });
      },
    },
    hl: {
      async define(name, def, opts) {
        const d: HighlightDef =
          "link" in def ? { kind: "link", to: def.link } : { kind: "spec", spec: def };
        await c({ call: "hl_define", name, def: d, default: opts?.default ?? false });
      },
      async get(name) {
        const v = expect(await c({ call: "hl_get", name }), "highlight");
        return { def: v.def ?? null, resolved: v.resolved ?? null };
      },
      async list() {
        return expect(await c({ call: "hl_list" }), "highlights").groups;
      },
      async reset(name) {
        await c({ call: "hl_reset", name });
      },
      onChange(cb) {
        return listener(r.highlightListeners, cb);
      },
    },
    ui: {
      async claim(win, rect) {
        return expect(await c({ call: "surface_claim", win, rect }), "surface").surface;
      },
      async put(surface, cells) {
        await c({ call: "surface_put", surface, cells });
      },
      async release(surface) {
        await c({ call: "surface_release", surface });
      },
    },
    cmd: {
      async register(name, fn, opts) {
        await c({ call: "cmd_register", name, desc: opts?.desc ?? null });
        r.commands.set(name, fn);
        return {
          dispose: () => {
            r.commands.delete(name);
            n({ call: "cmd_unregister", name });
          },
        };
      },
      async exec(name, args) {
        await c({ call: "cmd_exec", name, args: args ?? [] });
      },
      async call(name, args) {
        return expect(await c({ call: "cmd_call", name, args: args ?? [] }), "json").value as never;
      },
      async list() {
        return expect(await c({ call: "cmd_list" }), "commands").commands;
      },
    },
    keymap: {
      async set(mode, lhs, command, opts) {
        await c({ call: "keymap_set", mode, lhs, command, scope: opts?.scope ?? null, desc: opts?.desc ?? null });
      },
      async del(mode, lhs, scope) {
        await c({ call: "keymap_del", mode, lhs, scope: scope ?? null });
      },
      async list(mode) {
        return expect(await c({ call: "keymap_list", mode: mode ?? null }), "keymaps").keymaps;
      },
      async capture(win, command) {
        await c({ call: "keymap_capture", win, command });
        return { dispose: () => n({ call: "keymap_release", win }) };
      },
    },
    focus: {
      async push(win) {
        await c({ call: "focus_push", win });
      },
      async pop() {
        await c({ call: "focus_pop" });
      },
      async current() {
        return expect(await c({ call: "focus_current" }), "focused_win").win;
      },
      onChange(cb) {
        return listener(r.focusListeners, cb);
      },
    },
    agent: {
      async send(text, opts) {
        await c({ call: "agent_send", text, images: opts?.images ?? [] });
      },
      async cancel() {
        await c({ call: "agent_cancel" });
      },
      async command(command, session) {
        const v = await c({ call: "agent_command", session: session ?? null, command });
        return expect(v, "maybe_session").session;
      },
      async selection() {
        return expect(await c({ call: "agent_get_selection" }), "selection").selection;
      },
      async setSelection(selection) {
        await c({ call: "agent_set_selection", selection });
      },
      async listModels(instance, opts) {
        const v = await c({
          call: "agent_list_models",
          instance: instance ?? null,
          refresh: opts?.refresh ?? false,
        });
        return expect(v, "models").models;
      },
      async listInstances() {
        return expect(await c({ call: "agent_list_instances" }), "instances").instances;
      },
      async driverCommands() {
        return expect(await c({ call: "agent_driver_commands" }), "driver_commands").commands;
      },
      async setDraft(text) {
        await c({ call: "chat_set_draft", text });
      },
      async attach(path) {
        const v = await c({ call: "chat_attach", path: path ?? null });
        // Exactly one, because exactly one was asked for. An empty answer would mean the host
        // silently attached nothing, which it does not — it rejects.
        const [one] = expect(v, "attachments").attachments;
        if (!one) throw new Error("nothing was attached");
        return one;
      },
      async attachments() {
        return expect(await c({ call: "chat_attachments" }), "attachments").attachments;
      },
      async detach(index) {
        const v = await c({ call: "chat_detach", index: index ?? null });
        return expect(v, "attachments").attachments[0] ?? null;
      },
      async detachAll() {
        return expect(await c({ call: "chat_detach_all" }), "attachments").attachments;
      },
      async credentials() {
        return expect(await c({ call: "provider_credentials" }), "credentials").credentials;
      },
      async setCredential(instance, opts) {
        // Does not settle until the prompt closes — the answer is what the user did.
        const v = await c({
          call: "provider_set_credential",
          instance,
          replace: opts?.replace ?? false,
        });
        return expect(v, "bool").value;
      },
      async forgetCredential(instance) {
        await c({ call: "provider_forget_credential", instance });
      },
      onSelectionChange: (cb) => listener(r.selectionListeners, cb),
      onComposerChange: (cb) => listener(r.composerListeners, cb),
      onActivity: (cb) => listener(r.activityListeners, cb),
      onTurnStart: (cb) =>
        listener(r.agentListeners.turnStart as Array<(e: { session: string; turn: string }) => void>, cb),
      onToken: (cb) =>
        listener(r.agentListeners.token as Array<(e: { session: string; turn: string; text: string }) => void>, cb),
      onThinking: (cb) =>
        listener(r.agentListeners.thinking as Array<(e: { session: string; turn: string; text: string }) => void>, cb),
      onTurnEnd: (cb) =>
        listener(
          r.agentListeners.turnEnd as Array<
            (e: { session: string; turn: string; stopReason: StopReason; usage: Usage }) => void
          >,
          cb,
        ),
      onToolStart: (cb) =>
        listener(
          r.agentListeners.toolStart as Array<(e: { session: string; turn: string; call: ToolCall }) => void>,
          cb,
        ),
      onToolEnd: (cb) =>
        listener(
          r.agentListeners.toolEnd as Array<
            (e: { session: string; turn: string; call: ToolCall; result: ToolResult }) => void
          >,
          cb,
        ),
    },
    tool: {
      async register(def, handler) {
        await c({
          call: "tool_register",
          def: {
            name: def.name,
            description: def.description,
            input_schema: def.inputSchema,
            source: { kind: "builtin" },
          },
        });
        r.tools.set(def.name, handler);
        return {
          dispose: () => {
            r.tools.delete(def.name);
            n({ call: "tool_unregister", name: def.name });
          },
        };
      },
      async list() {
        return expect(await c({ call: "tool_list" }), "tools").tools;
      },
    },
    hook: {
      async register(hook, fn, opts) {
        const blocking = opts?.blocking ?? false;
        await c({ call: "hook_register", hook, blocking, timeout_ms: opts?.timeoutMs ?? null });
        r.hooks.set(hook, fn);
        return {
          dispose: () => {
            r.hooks.delete(hook);
            n({ call: "hook_unregister", hook });
          },
        };
      },
    },
    git: {
      async status(opts) {
        return expect(await c({ call: "git_status", cwd: opts?.cwd ?? null }), "status").status;
      },
      async branches(opts) {
        const v = await c({
          call: "git_branches",
          include_remote: opts?.includeRemote ?? false,
          cwd: opts?.cwd ?? null,
        });
        return expect(v, "branches").branches;
      },
      async worktrees(opts) {
        const v = await c({ call: "git_worktrees", cwd: opts?.cwd ?? null });
        return expect(v, "worktrees").worktrees;
      },
      async log(limit) {
        return expect(await c({ call: "git_log", limit: limit ?? 20 }), "commits").commits;
      },
      async diff(target, opts) {
        const v = await c({
          call: "git_diff",
          target: target ?? { kind: "unstaged" },
          stat: opts?.stat ?? false,
        });
        return expect(v, "text").text;
      },
      async defaultBranch() {
        return expect(await c({ call: "git_default_branch" }), "maybe_text").text ?? null;
      },
      async heads(cwds) {
        if (cwds.length === 0) return [];
        return expect(await c({ call: "git_heads", cwds }), "heads").heads;
      },
      async createBranch(name, opts) {
        await c({ call: "git_create_branch", name, from: opts?.from ?? null });
      },
      async renameBranch(name, next, opts) {
        await c({ call: "git_rename_branch", old: name, new: next, cwd: opts?.cwd ?? null });
      },
      async checkout(rev) {
        await c({ call: "git_checkout", rev });
      },
      async stage(paths) {
        await c({ call: "git_stage", paths: paths ?? [] });
      },
      async unstage(paths) {
        await c({ call: "git_unstage", paths: paths ?? [] });
      },
      async commit(message) {
        return expect(await c({ call: "git_commit", message }), "commit").commit;
      },
      async addWorktree(path, branch, opts) {
        await c({
          call: "git_add_worktree",
          path,
          branch,
          create: opts?.create ?? false,
          cwd: opts?.cwd ?? null,
        });
      },
      async fetch(opts) {
        const v = await c({ call: "git_fetch", cwd: opts?.cwd ?? null });
        return expect(v, "status").status;
      },
      async pulls(opts) {
        const v = await c({ call: "forge_pulls", cwd: opts?.cwd ?? null });
        return expect(v, "pulls").pulls;
      },
      async pull(opts) {
        const v = await c({ call: "git_pull", cwd: opts?.cwd ?? null, rebase: opts?.rebase ?? false });
        return expect(v, "text").text;
      },
      async removeWorktree(path, opts) {
        await c({
          call: "git_remove_worktree",
          path,
          force: opts?.force ?? false,
          cwd: opts?.cwd ?? null,
        });
      },
      async moveWorktree(path, dest, opts) {
        await c({ call: "git_move_worktree", path, dest, cwd: opts?.cwd ?? null });
      },
      async clone(url, path) {
        const v = await c({ call: "git_clone", url, path });
        return expect(v, "text").text;
      },
    },
    gen: {
      async complete(prompt, opts) {
        const v = await c({
          call: "gen_complete",
          prompt,
          system: opts?.system ?? null,
          json: false,
          selection: opts?.selection ?? null,
        });
        return expect(v, "text").text;
      },
      async json(prompt, opts) {
        const v = await c({
          call: "gen_complete",
          prompt,
          system: opts?.system ?? null,
          json: true,
          selection: opts?.selection ?? null,
        });
        return expect(v, "json").value as never;
      },
      async field(prompt, key, opts) {
        const v = await c({
          call: "gen_complete",
          prompt,
          system: opts?.system ?? null,
          json: true,
          field: key,
          selection: opts?.selection ?? null,
        });
        const value = (expect(v, "json").value as Record<string, unknown>)?.[key];
        // A key that came back as something other than a non-empty string is an answer to a
        // different question, and the caller is about to name a branch after it.
        if (typeof value !== "string" || value.trim() === "") {
          throw new Error(`the model returned no ${key}`);
        }
        return value.trim();
      },
    },
    view: {
      async list() {
        return expect(await c({ call: "view_list" }), "views").views;
      },
      async current() {
        const views = expect(await c({ call: "view_list" }), "views").views;
        return views.find((v) => v.current) ?? null;
      },
      at: (id) => build(plugin, version, r, id),
      onOpen(cb) {
        // The terminals that were already here, as well as the ones still to come. A plugin loaded
        // after them missed their arrival, and "open a panel in every view" would otherwise mean
        // every view *from now on* — which is every view except the one you are sitting in.
        //
        // Announced once each: a view that arrives while the list is in flight arrives by the
        // event too, and `seen` is what stops it being announced twice.
        const seen = new Set<ViewId>();
        const announce = (id: ViewId) => {
          if (seen.has(id)) return;
          seen.add(id);
          cb(id);
        };
        const d = listener(r.viewOpenListeners, announce);
        void c({ call: "view_list" })
          .then((v) => {
            for (const info of expect(v, "views").views) announce(info.view);
          })
          .catch(() => {});
        return d;
      },
      onClose: (cb) => listener(r.viewCloseListeners, cb),
    },
    session: {
      async list(opts) {
        const v = await c({
          call: "session_list",
          include_archived: opts?.includeArchived ?? false,
        });
        return expect(v, "sessions").sessions;
      },
      async current() {
        return expect(await c({ call: "session_current" }), "session").session;
      },
      async create(opts) {
        const v = await c({
          call: "session_new",
          cwd: opts?.cwd ?? null,
          title: opts?.title ?? null,
          activate: opts?.activate ?? true,
        });
        return expect(v, "session").session;
      },
      async switch(session) {
        await c({ call: "session_switch", session });
      },
      async close(session) {
        await c({ call: "session_close", session });
      },
      async rename(session, title) {
        await c({ call: "session_rename", session, title });
      },
      async archive(session, archived) {
        await c({ call: "session_archive", session, archived: archived ?? true });
      },
      async stored() {
        return expect(await c({ call: "sessions_stored" }), "sessions").sessions;
      },
      async messages(session) {
        const v = await c({ call: "session_messages", session: session ?? null });
        return expect(v, "messages").messages;
      },
      onChange: (cb) => listener(r.sessionListeners, cb),
    },
    permission: {
      async mode() {
        return expect(await c({ call: "permission_get_mode" }), "permission_mode").mode;
      },
      async setMode(mode) {
        return expect(await c({ call: "permission_set_mode", mode }), "permission_mode").mode;
      },
    },
    hint: {
      async set(key, hint) {
        await c({
          call: "hint_set",
          key,
          hint: { keys: hint.keys, label: hint.label, priority: hint.priority ?? 0 },
        });
      },
      async clear(key) {
        await c({ call: "hint_clear", key });
      },
    },
    status: {
      async set(key, segment) {
        await c({
          call: "status_set",
          key,
          segment: {
            text: segment.text,
            short: segment.short ?? null,
            keys: segment.keys ?? null,
            hl: segment.hl ?? null,
            align: segment.align ?? "left",
            priority: segment.priority ?? 0,
          },
        });
      },
      async clear(key) {
        await c({ call: "status_clear", key });
      },
    },
    provider: {
      async register(driver, instances, handler, opts) {
        await c({
          call: "provider_register_driver",
          driver,
          instances,
          agent_loop: opts?.agentLoop ?? false,
        });
        r.providers.set(driver, handler);
        return { dispose: () => r.providers.delete(driver) };
      },
    },
  };
  return api;
}

/** Build the API object handed to one plugin. Internal; the host calls this. */
export function __createContext(plugin: string, config: unknown, version: number): PluginContext {
  const r = reg(plugin);
  r.version = version;
  return {
    neosh: build(plugin, version, r, null),
    pluginId: plugin,
    config,
    subscriptions: r.subscriptions,
  };
}

/** Route one host message. Internal; the host's bootstrap calls this. */
export async function __dispatch(plugin: string, msg: Record<string, unknown>): Promise<void> {
  const r = reg(plugin);

  if (msg.type === "response") {
    settle(msg.id as string, msg.response as ApiResponse);
    return;
  }

  if (msg.type === "request") {
    const id = msg.id as string;
    const req = msg.request as Record<string, unknown>;
    const respond = (response: unknown) =>
      send({ type: "plugin", plugin, msg: { type: "response", id, response } });

    try {
      if (req.type === "run_tool") {
        const h = r.tools.get(req.name as string);
        if (!h) {
          respond({ type: "error", message: `plugin ${plugin} has no tool ${req.name}` });
          return;
        }
        respond({ type: "tool", result: await h(req.input) });
      } else if (req.type === "command") {
        const name = req.name as string;
        const h = r.commands.get(name);
        if (!h) {
          respond({ type: "error", message: `plugin ${plugin} has no command ${name}` });
          return;
        }
        const value = await h((req.args as string[]) ?? [], undefined);
        // `undefined` is not JSON; a handler that returned nothing answers `null`.
        respond({ type: "command", value: value === undefined ? null : value });
      } else if (req.type === "hook") {
        const h = r.hooks.get(req.hook as HookName);
        // A hook the plugin no longer has must not block the action: continue, do not veto.
        const outcome: HookOutcome = h ? await h(req.payload as HookPayload) : { action: "continue" };
        respond({ type: "hook", outcome });
      } else if (req.type === "provider_stream") {
        const streamId = req.stream as string;
        const tr = req.request as TurnRequest;
        const h = r.providers.get(tr.selection.instance);
        const byDriver = h ?? [...r.providers.values()][0];
        if (!byDriver) {
          respond({ type: "error", message: `plugin ${plugin} has no provider` });
          return;
        }
        const signal = { cancelled: false };
        r.streams.set(streamId, signal);
        // Answer immediately: a stream cannot be a return value across this boundary.
        respond({ type: "provider_accepted" });
        void (async () => {
          try {
            await byDriver(tr, (e) => notify(plugin, { call: "provider_emit", stream: streamId, event: e }), signal);
          } finally {
            r.streams.delete(streamId);
          }
        })();
      } else {
        respond({ type: "error", message: `unknown request ${String(req.type)}` });
      }
    } catch (e) {
      respond({ type: "error", message: e instanceof Error ? e.message : String(e) });
    }
    return;
  }

  if (msg.type === "event") {
    const ev = msg.event as PluginEvent;
    try {
      await dispatchEvent(plugin, r, ev);
    } catch (e) {
      // A listener that throws is one plugin's bug, and it used to be every plugin's: an
      // unhandled rejection stops the runtime, and the runtime is shared. Reported and survived.
      const what = e instanceof Error ? (e.stack ?? e.message) : String(e);
      notify(plugin, { call: "log", level: "error", message: `handling ${ev.type}: ${what}` });
    }
    return;
  }
}

async function dispatchEvent(
  plugin: string,
  r: ReturnType<typeof reg>,
  ev: PluginEvent,
): Promise<void> {
  {
    switch (ev.type) {
      case "command_invoked": {
        const h = r.commands.get(ev.name);
        if (!h) break;
        // The third argument is the whole namespace bound to the terminal the key was pressed in.
        // A handler that opens a panel writes `here.win.open(...)` and it lands where the person
        // pressing the key is looking, without having to say so or to know that views exist.
        const here = ev.key ? build(plugin, r.version, r, ev.key.view) : undefined;
        await h(ev.args ?? [], ev.key ?? undefined, here);
        break;
      }
      case "buffer_changed": {
        for (const cb of r.bufferListeners.get(ev.buf) ?? []) {
          cb({ buf: ev.buf, start: ev.start, oldEnd: ev.old_end, newEnd: ev.new_end });
        }
        break;
      }
      case "turn_started":
        for (const cb of r.agentListeners.turnStart)
          (cb as (e: unknown) => void)({ session: ev.session, turn: ev.turn });
        break;
      case "token":
        for (const cb of r.agentListeners.token)
          (cb as (e: unknown) => void)({ session: ev.session, turn: ev.turn, text: ev.text });
        break;
      case "thinking_token":
        for (const cb of r.agentListeners.thinking)
          (cb as (e: unknown) => void)({ session: ev.session, turn: ev.turn, text: ev.text });
        break;
      case "turn_ended":
        for (const cb of r.agentListeners.turnEnd)
          (cb as (e: unknown) => void)({
            session: ev.session,
            turn: ev.turn,
            stopReason: ev.stop_reason,
            usage: ev.usage,
          });
        break;
      case "tool_started":
        for (const cb of r.agentListeners.toolStart)
          (cb as (e: unknown) => void)({ session: ev.session, turn: ev.turn, call: ev.call });
        break;
      case "tool_finished":
        for (const cb of r.agentListeners.toolEnd)
          (cb as (e: unknown) => void)({
            session: ev.session,
            turn: ev.turn,
            call: ev.call,
            result: ev.result,
          });
        break;
      case "hook_observed": {
        const h = r.hooks.get(ev.hook);
        if (h) await h(ev.payload);
        break;
      }
      case "provider_cancel": {
        const s = r.streams.get(ev.stream);
        if (s) s.cancelled = true;
        break;
      }
      case "option_changed":
        for (const cb of r.optionListeners) cb({ name: ev.name, value: ev.value });
        break;
      case "session_changed":
        for (const cb of r.sessionListeners) cb({ session: ev.session, view: ev.view });
        break;
      case "view_attached":
        for (const cb of r.viewOpenListeners) cb(ev.view);
        break;
      case "view_closed":
        for (const cb of r.viewCloseListeners) cb(ev.view);
        break;
      case "selection_changed":
        for (const cb of r.selectionListeners) cb({ selection: ev.selection });
        break;
      case "composer_changed":
        for (const cb of r.composerListeners) cb({ text: ev.text });
        break;
      case "activity":
        for (const cb of r.activityListeners)
          cb({ session: ev.session, turn: ev.turn, activity: ev.activity });
        break;
      case "var_changed":
        for (const cb of r.varListeners)
          cb({ scope: ev.scope, key: ev.key, value: ev.value });
        break;
      case "project_moved":
        for (const cb of r.projectMovedListeners) cb({ from: ev.from, to: ev.to });
        break;
      case "quota":
        for (const cb of [...r.quotaListeners]) cb(ev.snapshot);
        break;
      case "swarm_changed":
        for (const cb of [...r.swarmListeners]) cb();
        break;
      case "swarm_stream":
        for (const cb of [...r.swarmStreamListeners]) {
          cb({ node: ev.node, session: ev.session, event: ev.event });
        }
        break;
      case "contributions_changed":
        for (const cb of r.contributionListeners) cb({ point: ev.point });
        break;
      case "focus_changed":
        for (const cb of r.focusListeners) cb({ win: ev.win ?? null });
        break;
      case "view_attached":
        for (const cb of r.viewListeners) cb();
        break;
      case "shutdown":
        for (const cb of r.shutdownListeners) cb();
        break;
      case "highlight_changed":
        for (const cb of r.highlightListeners) cb({ names: ev.names });
        break;
      case "event": {
        // Copied before iterating: a listener that unsubscribes itself — the ordinary shape of
        // "wait for the thing to happen once" — would otherwise shorten the array underneath the
        // loop and skip whoever was next.
        const named = [...(r.eventListeners.get(ev.name) ?? [])];
        const all = [...(r.eventListeners.get(null) ?? [])];
        const e = { name: ev.name, data: ev.data ?? null, from: ev.from };
        for (const cb of named) cb(e);
        for (const cb of all) cb(e);
        break;
      }
    }
  }
}

/** Dispose everything a plugin registered. Internal. */
export function __teardown(plugin: string): void {
  const r = plugins.get(plugin);
  if (!r) return;
  // Before the disposers: a timer that fires mid-teardown would call into a plugin that is halfway
  // gone.
  for (const id of r.timers) {
    clearTimeout(id);
  }
  r.timers.clear();
  for (const d of r.subscriptions) {
    try {
      d.dispose();
    } catch {
      // A failing disposer must not stop the rest from running.
    }
  }
  plugins.delete(plugin);
}
