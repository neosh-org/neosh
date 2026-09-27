/**
 * There is a newer neosh, and here is what pressing this would do.
 *
 * Three things, and the third is the one this was missing. Saying *there is an update* is news you
 * did not ask for and cannot otherwise see, which is the definition of an alert — so it is raised
 * once, and never again for the same version. Saying *what updating means here* is not one sentence:
 * a binary Homebrew owns is updated by running `brew`, one inside `node_modules` by running npm,
 * and a standalone one by a download and a rename. The host works out which and does it; this
 * draws how it is going and what came of it.
 *
 * And then there is *you are not running what you installed*, which is the state almost everybody
 * ends up in and the one nothing said a word about. Most installs are managed, so most updates
 * happen in another terminal: `brew upgrade neosh` finishes, reports success, and the workspace
 * carries on executing the file it started with — for days, because a workspace here outlives the
 * terminals that look at it. Nothing had noticed, so `/update` said "you are on the newest" while
 * the newest sat on disk unused. The host now answers that with a `stat` rather than a registry,
 * this polls it, and the row says the one thing there is to do.
 *
 * `/update` is that one thing, end to end: check, download if it is ours to download, and **restart
 * when nothing is running**. It used to stop at "installed — restart to use it" and want a second
 * `/update`, which is a program telling you it has finished half a job and asking you to remember
 * the other half. The restart is still refused while a turn is in flight — a restart ends turns in
 * conversations this terminal cannot see — but that is a question with names in it, not a shrug.
 *
 * The row lives under the plan strip, because both are facts about the workspace rather than about
 * a conversation, and the plan is the thing you look at when you look down there. It is only ever
 * present when there is something to say — an update waiting, or a restart owed — so the column's
 * job stays your conversations.
 */
import type { Neosh, PluginContext, UpdateProgress, UpdateStatus } from "@neosh/api";
import { UPDATE_EVENT } from "@neosh/api";
import { confirmDestructive, onTick, spinnerFrame } from "@neosh/api/ui";

const NS = "update";
/** The section id, so the row can be taken off again by name. */
const ROW = "update";

/**
 * How often to look at all.
 *
 * Every thirty seconds, and almost always free: what it costs is one `stat` of the running
 * executable, which is what answers "has something replaced me". The *network* is on its own much
 * slower clock below, and the host holds a floor under that regardless of what is asked here.
 */
const TICK_MS = 30_000;

/** How long an answer about the registry is good for, unless `update.interval` says otherwise. */
const DEFAULT_INTERVAL_S = 6 * 60 * 60;

export async function activate({ neosh, subscriptions }: PluginContext) {
  /** The version we have already interrupted somebody about. */
  let announced: string | null = null;
  /** And the restart we have already interrupted somebody about, which is a different sentence. */
  let announcedRestart: string | null = null;
  /** What is drawn, so a tick that changes nothing is not a round trip. */
  let drawn: string | null = null;
  /**
   * When the network was last *asked*, rather than when it last answered.
   *
   * Counted from the ask so a machine with no network settles into one attempt per interval instead
   * of one per tick forever — the git block's rule, for the same reason.
   */
  let askedAt: number | null = null;

  await declareOptions(neosh);

  const settings = async () => ({
    on: (await neosh.opt.get<boolean>("update.check").catch(() => true)) ?? true,
    interval: (await neosh.opt.get<number>("update.interval").catch(() => DEFAULT_INTERVAL_S)) ??
      DEFAULT_INTERVAL_S,
  });

  /**
   * What the row says, given what the host worked out. `null` when there is nothing to say.
   *
   * The right-hand hint is the *command*, not `↵`. `↵` is true — the cursor can land here and open
   * it — but it is true the way every row in the panel is true, and it was the only thing the row
   * ever said about how to update. Reaching it means knowing the sidebar has to be focused before
   * `j` does anything (`^T`), then that `G` skips a workspace's worth of conversations to get to a
   * strip pinned at the bottom. Three facts, none of them on screen, to press a key that is already
   * two characters from the composer somebody is sitting in. So the row advertises the way in that
   * needs no navigation at all, and stays openable for anybody who is passing.
   */
  const rowFor = (s: UpdateStatus) => {
    if (s.restart_pending) {
      // A restart outranks an update, always: it is the nearer half of the same job, it is free,
      // and offering to download something while a downloaded thing is waiting is a panel that has
      // lost track of what it is doing. `Status.Pending` because this one is *act now* and a
      // restart is one key away, which is exactly what that amber means everywhere else.
      return {
        text: `restart for ${s.restart_version ?? "the update"}`,
        hl: "Status.Pending",
        right: { text: "/update", hl: "Comment" },
        command: NS,
      };
    }
    if (!s.behind || !s.latest) return null;
    // What the key would *do* is on the row, because a key whose effect you cannot predict from
    // the row it is pointed at is one people learn not to press.
    return {
      text: `neosh ${s.latest} available`,
      hl: "Status.Unread",
      right: { text: "/update", hl: "Comment" },
      command: NS,
    };
  };

  /** Put the row where it goes, or take it away, and say the news once. */
  const draw = async (status: UpdateStatus) => {
    const row = rowFor(status);
    const key = JSON.stringify(row);
    if (key !== drawn) {
      drawn = key;
      if (row) {
        // Under the plan rather than above it: the plan is the thing somebody came to look at, and
        // a row that pushes it down every time a release happens is a row that moved their gauge.
        await neosh.ext.contribute("sidebar.section", ROW, { after: "plan", rows: [row] }, {
          priority: -9,
        }).catch(() => {});
      } else {
        await neosh.ext.remove("sidebar.section", ROW).catch(() => {});
      }
    }

    // A restart owed is the more urgent of the two and the one nobody would otherwise find out
    // about: the update has already happened somewhere else, in another terminal, minutes ago.
    // Once per version, like the other one — a notification that repeats is one people turn off.
    if (status.restart_pending) {
      const v = status.restart_version ?? "?";
      if (v !== announcedRestart) {
        announcedRestart = v;
        await neosh.alert(
          `neosh ${status.restart_version ?? "update"} is installed`,
          "This workspace is still running the old one. Type /update in the chat to restart it.",
          { level: "info" },
        ).catch(() => {});
      }
      // And nothing about a newer release while one is already waiting: two notifications about one
      // job is a person being asked to work out which of them supersedes the other.
      return;
    }

    // Once per version. A notification that repeats is one people turn off, and this one has to
    // survive being ignored for a week.
    if (status.behind && status.latest && status.latest !== announced) {
      announced = status.latest;
      await neosh.alert(
        `neosh ${status.latest}`,
        // The thing to type, in a notification that may well be read on a phone. "Update from the
        // sidebar" is an instruction to go and find something; this is the instruction — and it is
        // the same one on every kind of install now that the host runs a package manager itself,
        // where this used to send a Homebrew user off to a shell to type the command.
        "A new version is out. Type /update in the chat.",
        { level: "info" },
      ).catch(() => {});
    }
  };

  /**
   * Look, and draw what came back.
   *
   * `network: false` is the ordinary tick and costs a `stat`. Asking the registry is the caller's
   * decision because it is the expensive half and because turning it off has to be possible —
   * `update.check = false` is somebody who updates their machine themselves and does not want a
   * program phoning a website about it. The local half is not covered by that setting and should
   * not be: "the file under you changed" is a fact about this machine, it is the one that makes a
   * workspace wrong about itself, and no network was involved in learning it.
   */
  const refresh = async (network = false) => {
    const status = await (network ? neosh.update.check(true) : neosh.update.local())
      .catch(() => null);
    if (!status) return null;
    await draw(status);
    return status;
  };

  /**
   * Restart, having said what it costs.
   *
   * The host refuses while any turn is in flight and names them, which is the answer to *may I* —
   * so this asks the person the question the host cannot: it is their work, they can see the names,
   * and a restart is not something to do to somebody quietly. Reversible things ask nothing, and
   * this one is not: the turns end.
   */
  const restart = async () => {
    try {
      await neosh.update.restart();
      return true;
    } catch (e) {
      // The host's own sentence, verbatim, as the detail: it names the conversations, and "something
      // is running" sends somebody hunting through a workspace for which one.
      const go = await confirmDestructive(
        neosh,
        "Restart now and end what is running?",
        { yes: "Restart", no: "Wait", detail: [String(e)] },
      ).catch(() => false);
      if (!go) {
        // Not silence: they said no to *this* restart, and the row is still there saying the job is
        // half done. Saying so is what stops the row reading as something that did nothing.
        await neosh.notify("Still waiting — /update when the turns have finished.", "info");
        return false;
      }
      try {
        await neosh.update.restart(true);
        return true;
      } catch (again) {
        await neosh.notify(String(again), "warn");
        return false;
      }
    }
  };

  // `/update` — the whole verb, and the one to tell people about.
  //
  // The three below are the *pieces*: check, apply, restart. Naming them was right and advertising
  // them was not, because "update neosh" is one intention and a menu of three is a question about
  // internal state — which of them applies depends on whether a check has run, whether a download
  // has happened, and whether a restart is owed, none of which anybody tracks. Typing `/update`
  // used to match all three and pick none, so the answer to "how do I update" was a list.
  //
  // Registered under the bare namespace so the slash menu, which matches on name and scores an
  // exact prefix by length, puts it above the three it is made of.
  subscriptions.push(
    await neosh.cmd.register(NS, async () => {
      // Finish what is already half done before starting anything new, and before anything that can
      // fail. A binary waiting on disk needs no network to restart onto, so asking the registry
      // first — which is what this used to do — meant a machine that had updated and then gone
      // offline could no longer complete its own update: the check failed, the failure was
      // reported, and the restart sitting one line below was never reached.
      const local = await neosh.update.local().catch(() => null);
      if (local?.restart_pending) {
        await draw(local);
        return void (await restart());
      }

      // Forced, because this is somebody asking *now*: the poll may be hours stale and "you are on
      // the newest" from a cache is the one answer this must not give wrongly.
      const s = await neosh.update.check(true).catch(() => null);
      askedAt = Date.now();
      if (!s) return neosh.notify("Could not check for updates", "warn");
      await draw(s);
      if (s.method === "development") return neosh.notify(development(s), "warn");
      if (s.error) return neosh.notify(`Update check failed: ${s.error}`, "warn");
      if (!s.behind) return neosh.notify(`neosh ${s.current} is the newest`, "info");
      return neosh.cmd.exec(`${NS}.apply`);
    }, { desc: "Update neosh" }),
  );

  subscriptions.push(
    await neosh.cmd.register(`${NS}.check`, async () => {
      const s = await refresh(true);
      askedAt = Date.now();
      if (!s) return neosh.notify("Could not check for updates", "warn");
      if (s.restart_pending) {
        return neosh.notify(
          `neosh ${s.restart_version ?? "an update"} is installed — /update to restart`,
          "info",
        );
      }
      if (s.method === "development") return neosh.notify(development(s), "warn");
      if (s.error) return neosh.notify(`Update check failed: ${s.error}`, "warn");
      if (!s.behind) return neosh.notify(`neosh ${s.current} is the newest`, "info");
      return neosh.notify(`neosh ${s.latest} is available`, "info");
    }, { desc: "Check for a newer neosh" }),
  );

  subscriptions.push(
    await neosh.cmd.register(`${NS}.apply`, async () => {
      // Raised and *always* cleared. A progress notice is keyed and replaced in place because it is
      // a **state**, so one that is never `done` is a state with no end: `Updating neosh…` sat
      // there, and the reply that followed it never got a look in. From the keyboard that is the
      // whole bug in a sentence: `/update` says "updating…", the row goes, and nothing has
      // happened or been said.
      //
      // What the row says is the host's own account of the update, off the bus: the step that is
      // running (`brew update`, `brew upgrade neosh`, `downloading neosh-aarch64-apple-darwin`)
      // and the last line it printed. A `brew upgrade` is a minute of somebody else's server and
      // a `cargo install` is a compile, and a row that said `Updating neosh…` for the whole of
      // either was one you could not tell from a hang. The spinner has its own clock, so a step
      // that prints nothing for a while still turns.
      //
      // `finally`, not a call per branch. Every arm of the switch below returns, and one added
      // later that forgets would put the hang straight back.
      let phase = "checking";
      let line: string | null = null;
      const draw = () => {
        const tail = line ? `  ${line}` : "";
        neosh.progress(`${NS}`, `${spinnerFrame()} updating neosh — ${phase}${tail}`);
      };
      draw();
      const watching = neosh.event.on(UPDATE_EVENT, (e) => {
        const p = e.data as UpdateProgress | undefined;
        if (!p || p.done) return;
        phase = p.phase;
        line = p.line ? clip(p.line, 72) : null;
        draw();
      });
      const ticking = onTick(draw);
      try {
        return await applyUpdate();
      } finally {
        watching.dispose();
        ticking.dispose();
        neosh.done(`${NS}`);
      }
    }, { desc: "Update neosh to the newest version" }),
  );

  async function applyUpdate() {
    {
      const outcome = await neosh.update.apply().catch((e) => ({
        outcome: "failed" as const,
        reason: String(e),
      }));
      switch (outcome.outcome) {
        case "up_to_date":
          await refresh();
          return neosh.notify(`neosh ${outcome.version} is the newest`, "info");
        case "applied": {
          await refresh();
          // And then finish it. Asked before it happens rather than after — `restart` puts the
          // names of anything running in front of a person and lets them say no — but *done*,
          // because an update that stops one step short of being the thing you are running is an
          // update that has not happened, and the step it stopped short of was never on screen.
          if (await restart()) return;
          return neosh.notify(
            `neosh ${outcome.version} is installed — /update to restart into it`,
            "info",
          );
        }
        default:
          // The host's sentence, whole. For a managed install it ends with the command to run by
          // hand — the one thing this used to say *instead* of updating, and now says only when
          // updating did not work: the tool was not on the workspace's PATH, or it ran and left
          // the binary as it was because the tap has not caught up with the release yet.
          return neosh.notify(`Update failed: ${outcome.reason}`, "error");
      }
    }
  }

  subscriptions.push(
    await neosh.cmd.register(`${NS}.restart`, async () => void (await restart()), {
      desc: "Restart the workspace to finish an update",
    }),
  );

  // No `sidebar.action` of its own, deliberately. A contributed row already runs its `command` on
  // `↵`, and one row that means the one thing there is to do — update, or restart once it is
  // downloaded — is better than two keys for two states that never coexist.
  //
  // If it ever wants one, the spelling is `on: "custom:update"`. Bare `custom` is every contributed
  // row in the column, which is how a key added here used to be advertised on the plan strip and
  // push that strip's own `<Tab>` hint off the end of the row.

  /** Whether the registry is due to be asked again. `interval = 0` is never. */
  const due = async () => {
    const s = await settings();
    if (!s.on || s.interval <= 0) return false;
    return askedAt === null || Date.now() - askedAt >= s.interval * 1000;
  };

  await refresh();

  // Checked on a short tick against the wall clock rather than scheduled at the interval, and the
  // difference is the whole reason a second machine never heard about a release. A timer armed for
  // six hours is armed on a *monotonic* clock, which does not advance while a laptop is asleep — so
  // on a machine that is shut for twenty hours a day, a six-hour timer takes the better part of a
  // week, and a workspace left running for a fortnight had genuinely never asked. `Date.now()` is
  // the clock that keeps counting. It also means changing `update.interval` takes effect without
  // restarting anything, and that a machine that was asleep asks *once* when it wakes rather than
  // banking up every check it missed.
  subscriptions.push(
    await neosh.timer.every(TICK_MS, () => {
      void (async () => {
        if (await due()) {
          askedAt = Date.now();
          await refresh(true);
        } else {
          // The cheap half, every tick: `brew upgrade` in the next terminal along should show up in
          // this panel in under a minute, and it costs a `stat` to say so.
          await refresh();
        }
      })();
    }),
  );

  // Not at `activate`: the first thing a workspace does on opening is start a conversation and load
  // eleven plugins, and a network round trip in the middle of that is a second of a slower start
  // for a number nobody has looked at yet.
  subscriptions.push(
    neosh.event.on("neosh.ready", () => {
      void (async () => {
        if (await due()) {
          askedAt = Date.now();
          await refresh(true);
        }
      })();
    }),
  );

  // Somebody just arrived. Which is the moment the answer is worth having — a workspace that has
  // been sitting detached for a week is exactly the one running a version nobody has looked at.
  subscriptions.push(neosh.view.onOpen(() => void refresh()));

  subscriptions.push({
    dispose: () => void neosh.ext.remove("sidebar.section", ROW).catch(() => {}),
  });
}

/** The first `n` characters of a tool's line, with an ellipsis when there were more. */
function clip(text: string, n: number): string {
  const chars = [...text];
  return chars.length <= n ? text : `${chars.slice(0, n - 1).join("")}…`;
}

async function declareOptions(neosh: Neosh) {
  await neosh.opt.declare({
    name: "update.check",
    type: { type: "bool" },
    default: true,
    description:
      "Ask the releases page whether there is a newer neosh. Turning this off stops the network " +
      "check only — a workspace still says when the binary under it has been replaced, because " +
      "that is a fact about this machine and is what makes it wrong about itself.",
  }).catch(() => {});
  await neosh.opt.declare({
    name: "update.interval",
    type: { type: "int", min: 0, max: 604800 },
    default: DEFAULT_INTERVAL_S,
    description: "Seconds between checks for a newer neosh. 0 never asks.",
  }).catch(() => {});
}

/**
 * What a development build answers `/update` with.
 *
 * It used to say "neosh 0.4.11 is the newest", because a checkout is never offered an update — and
 * a `target/debug/neosh` left serving a workspace is exactly what plain `neosh` attaches to
 * afterwards, so a person with 0.5.0 installed was told they had the newest while running last
 * week's build. The host's sentence, with the same facts: which build, what is released, what to do.
 */
function development(s: UpdateStatus): string {
  if (s.latest && newer(s.latest, s.current)) {
    return `Development build ${s.current} — neosh ${s.latest} is released. ` +
      "Rebuild, or `neosh stop` and run the installed release.";
  }
  return s.latest
    ? `Development build ${s.current}, the newest release — rebuild from your checkout to update it.`
    : `Development build ${s.current} — rebuild from your checkout to update it.`;
}

/** Whether `a` is a later version than `b`, by numbered parts; anything unparseable is not. */
function newer(a: string, b: string): boolean {
  const parts = (v: string) => v.replace(/^v/, "").split(".").map((p) => Number(p));
  const x = parts(a);
  const y = parts(b);
  if ([...x, ...y].some((n) => !Number.isInteger(n))) return false;
  for (let i = 0; i < Math.max(x.length, y.length); i++) {
    const d = (x[i] ?? 0) - (y[i] ?? 0);
    if (d !== 0) return d > 0;
  }
  return false;
}
