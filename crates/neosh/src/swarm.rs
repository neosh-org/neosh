//! The host's half of ASCP: what the other machines are running, and how to ask them things.
//!
//! [`neosh_swarm`] owns sockets and signatures. This owns the *picture* — which peers are up, what
//! each of them has, and which project on this machine is the same project as one over there — and
//! translates between a conversation as the session store knows it and an
//! [`AgentSummary`](neosh_proto::AgentSummary) as the wire does.
//!
//! Kept apart from `host.rs` because it is the one part of the workspace that is *about* other
//! workspaces, and because everything here has to survive a peer that is lying, stale or gone.

use std::collections::HashMap;

use neosh_proto::{
    AgentState, AgentSummary, LinkState, NodeCapabilities, NodeId, NodeInfo, ProjectKey, SessionId,
    SessionInfo,
};

/// One peer and everything it has told us.
#[derive(Debug, Clone)]
pub struct Remote {
    pub info: NodeInfo,
    pub capabilities: NodeCapabilities,
    /// Its agents, most recently updated first.
    pub agents: Vec<AgentSummary>,
    /// Where the connection stands. When it is not [`LinkState::Up`] the row stays rather than
    /// vanishing: a machine you were working with going quiet is information, and a list that
    /// silently shortens is one you cannot tell from a list that never had it.
    pub link: LinkState,
    /// Whether it has ever been up this run — what tells `connecting` from `reconnecting`, which
    /// are the same state to the dialler and different sentences to a person.
    ever_up: bool,
    /// Why it is not up, when it is not.
    pub reason: Option<String>,
    /// What this user calls it, if they have said. Held beside `info` rather than written into it
    /// because every handshake replaces `info` wholesale, and a name a person chose must not be
    /// something a reconnect can undo.
    pub alias: Option<String>,
}

impl Remote {
    pub fn up(&self) -> bool {
        matches!(self.link, LinkState::Up)
    }

    /// What to call this machine: the name its owner here gave it, else the one it announces.
    ///
    /// Falls back through to the id when a machine has neither, which happens for exactly one row
    /// — a config peer with no `name`, before its first handshake — and is still better than a
    /// blank label in a list you pick from.
    pub fn display_name(&self) -> String {
        self.alias
            .clone()
            .filter(|a| !a.is_empty())
            .or_else(|| Some(self.info.name.clone()).filter(|n| !n.is_empty()))
            .unwrap_or_else(|| self.info.id.short().to_string())
    }

    /// The state a dial that did not produce a connection leaves the row in.
    ///
    /// Deliberately *not* sticky against [`LinkState::Waiting`]: this is only reached when a dial
    /// actually failed, and a machine that was waiting on a person and has now stopped answering
    /// its port has genuinely changed state. Being told to press a key on a computer that is
    /// switched off is worse than being told nothing.
    fn not_up(&self, attempt: u32) -> LinkState {
        if self.ever_up {
            LinkState::Retrying { attempt }
        } else {
            LinkState::Connecting { attempt }
        }
    }
}

/// Every node this one knows about, and what it has.
#[derive(Debug, Default)]
pub struct Swarm {
    peers: HashMap<NodeId, Remote>,
    /// The projects this machine has, by key, so a local row can say which peers share it.
    local_projects: HashMap<ProjectKey, String>,
}

impl Swarm {
    /// A machine we know about before it has said anything: paired, and about to be dialled — or
    /// waited for. Gives the board a row from the first frame, so "connecting…" is a state a
    /// person can watch rather than a silence they have to explain.
    ///
    /// `dialled` is whether a dialler is working on it. A peer that only ever dials *us* is
    /// [`LinkState::Down`] until it shows up — nothing on this machine is trying, and a row that
    /// says "connecting" about nobody's work is a promise nothing is keeping.
    pub fn seed(&mut self, id: NodeId, name: String, dialled: bool) {
        self.peers.entry(id.clone()).or_insert_with(|| Remote {
            info: NodeInfo { id, name, os: String::new(), version: String::new() },
            capabilities: NodeCapabilities {
                accepts_commands: false,
                accepts_approvals: false,
                streams: false,
                browse: true,
                shells: false,
                rich_stream: false,
                catalogue: false,
                live: false,
                projects: Vec::new(),
            },
            agents: Vec::new(),
            link: if dialled { LinkState::Connecting { attempt: 0 } } else { LinkState::Down },
            ever_up: false,
            reason: None,
            alias: None,
        });
    }

    /// What this user calls a machine, applied to a row that may already exist.
    ///
    /// Separate from [`Swarm::seed`] because it is called for both — a peer being seeded at boot
    /// and one renamed while the panel is open — and because `seed` deliberately does nothing to a
    /// row that is already there, which is right for a link state and wrong for a name.
    pub fn set_alias(&mut self, id: &NodeId, alias: Option<String>) {
        if let Some(p) = self.peers.get_mut(id) {
            p.alias = alias.filter(|a| !a.trim().is_empty()).map(|a| a.trim().to_string());
        }
    }

    /// A machine we authorised, reached, and that has not authorised us back.
    ///
    /// Fills the row in from the handshake even though the connection went no further: name, OS,
    /// version and what it offers all arrived, and a half-paired row that can say `mini · macOS ·
    /// neosh 0.2.0` is one you can tell you dialled the right computer.
    pub fn unapproved(&mut self, info: NodeInfo, capabilities: NodeCapabilities) {
        let id = info.id.clone();
        self.seed(id.clone(), info.name.clone(), true);
        if let Some(p) = self.peers.get_mut(&id) {
            p.info = info;
            p.capabilities = capabilities;
            p.link = LinkState::Waiting;
            // The state is the whole story here, and a sentence beside it would be the same
            // sentence twice. `reason` is for the things a state cannot say.
            p.reason = None;
        }
    }

    pub fn peer_up(&mut self, info: NodeInfo, capabilities: NodeCapabilities) {
        let entry = self.peers.entry(info.id.clone()).or_insert_with(|| Remote {
            info: info.clone(),
            capabilities: capabilities.clone(),
            agents: Vec::new(),
            link: LinkState::Up,
            ever_up: true,
            reason: None,
            alias: None,
        });
        // `alias` is untouched on purpose: a handshake refreshes everything the machine says about
        // itself, and what it is called here is not one of those things.
        entry.info = info;
        entry.capabilities = capabilities;
        entry.link = LinkState::Up;
        entry.ever_up = true;
        entry.reason = None;
    }

    pub fn peer_down(&mut self, node: &NodeId, reason: String, will_retry: bool) {
        if let Some(p) = self.peers.get_mut(node) {
            p.link = if will_retry { p.not_up(0) } else { LinkState::Down };
            p.reason = Some(reason);
            // Its agents are *not* cleared. They were real a moment ago and are probably still
            // running; what changed is that we can no longer see them. A board that empties a
            // machine's rows the instant a wifi hiccup drops a connection is one that makes you
            // think work was lost.
        }
    }

    /// A dial at a peer failed. Moves the count along and keeps the freshest reason; never
    /// touches a row that is up, because the dialler saying "could not connect" about a
    /// connection the listener holds is the losing half of a simultaneous open talking.
    pub fn dial_failed(&mut self, node: &NodeId, attempt: u32, error: String) {
        if let Some(p) = self.peers.get_mut(node)
            && !p.up()
        {
            p.link = p.not_up(attempt);
            p.reason = Some(error);
        }
    }

    /// Take in what a peer says it is running.
    pub fn inventory(
        &mut self,
        node: &NodeId,
        full: bool,
        agents: Vec<AgentSummary>,
        gone: Vec<SessionId>,
    ) {
        let Some(p) = self.peers.get_mut(node) else {
            // Inventory from a node we have no record of. Only possible if the peer went down
            // between the message arriving and this running; dropping it is right, because the
            // reconnect brings a full snapshot.
            return;
        };
        if full {
            p.agents = agents;
        } else {
            p.agents.retain(|a| !gone.contains(&a.session));
            for a in agents {
                match p.agents.iter_mut().find(|m| m.session == a.session) {
                    Some(m) => *m = a,
                    None => p.agents.push(a),
                }
            }
        }
        p.agents.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    }

    pub fn peers(&self) -> impl Iterator<Item = &Remote> {
        self.peers.values()
    }

    pub fn peer(&self, node: &NodeId) -> Option<&Remote> {
        self.peers.get(node)
    }

    /// Every agent on every peer that is still up.
    ///
    /// A peer that has gone keeps its rows in [`Self::peers`] — where the fact that it is down is
    /// visible next to them — but is left out here, because this is what a list of things you can
    /// act on is built from and you cannot act on those.
    pub fn agents(&self) -> impl Iterator<Item = (&NodeInfo, &AgentSummary)> {
        self.peers
            .values()
            .filter(|p| p.up())
            .flat_map(|p| p.agents.iter().map(move |a| (&p.info, a)))
    }

    /// Tell the swarm which projects this machine has, so it can answer [`Self::hosts_of`].
    pub fn set_local_projects(&mut self, projects: HashMap<ProjectKey, String>) {
        self.local_projects = projects;
    }

    /// Which *other* machines have this project, by name.
    ///
    /// The question the sidebar asks to put `mac-studio · linux-box` beside a project row. Only
    /// peers that are up, and only projects they have a conversation in — a machine that has the
    /// directory but has never opened it there is not somewhere the project is "on" in the sense a
    /// person means.
    pub fn hosts_of(&self, key: &ProjectKey) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .peers
            .values()
            .filter(|p| p.up() && p.agents.iter().any(|a| &a.project == key))
            .map(|p| p.info.name.as_str())
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }
}

/// A local conversation, as the wire describes it.
///
/// `project` is the cross-machine key rather than the path, which is the whole reason two machines
/// can agree they have the same project. `cwd` goes across as well, but only so the other end can
/// *show* it and hand it back in a `NewSession` — it is a path in this machine's filesystem and
/// means nothing over there.
pub fn summarise(
    info: &SessionInfo,
    project: ProjectKey,
    selection: Option<&neosh_proto::ModelSelection>,
    mode: Option<neosh_proto::PermissionMode>,
) -> AgentSummary {
    AgentSummary {
        session: info.id.clone(),
        project,
        project_name: info.project.clone(),
        cwd: info.cwd.clone(),
        // What lets a peer's panel nest this the way ours does — a worktree under the repository
        // it is a tree of, rather than beside it under a name with the branch stuck on the end.
        repo_root: info.repo_root.clone(),
        branch: info.branch.clone(),
        label: info.label.clone(),
        state: state_of(info),
        message_count: info.message_count,
        model: selection.map(|s| s.model.to_string()),
        instance: selection.map(|s| s.instance.to_string()),
        mode,
        options: selection.map(|s| s.options.clone()).unwrap_or_default(),
        context_tokens: info.context_tokens,
        context_window: info.context_window,
        turn_started_at: info.turn_started_at,
        updated_at: info.updated_at,
        usage: info.usage.clone(),
        unread: info.unread,
        interrupted: info.interrupted,
        background: info.background.len() as u32,
        // The host's to fill: they are contributions, and this knows nothing of the registry.
        decorations: Vec::new(),
    }
}

/// How much of a frame a conversation's history may take. Under the swarm's
/// eight-megabyte frame cap with room for the envelope around it, because a frame over
/// the cap is not refused, it is the *connection* failing — every conversation on that link, and
/// again on every reconnect, since opening the conversation is what sends it.
pub const HISTORY_BUDGET: usize = 6 * 1024 * 1024;

/// A conversation's history cut down to fit in one frame, if it has to be.
///
/// What goes first is what a transcript folds anyway: the middles of long tool outputs, reasoning,
/// and the long strings inside a call's arguments — a file written whole is a diff card over there,
/// and the card shows a few lines of it. Only if that is not enough do the oldest turns go, whole,
/// because the newest one is the one somebody opened this to read. What was said is never clipped.
pub fn fit_history(
    mut messages: Vec<neosh_proto::Message>,
    mut live: Option<neosh_proto::LiveTurn>,
) -> (Vec<neosh_proto::Message>, Option<neosh_proto::LiveTurn>) {
    use neosh_proto::ContentBlock as B;
    let size_of = |m: &neosh_proto::Message| serde_json::to_vec(m).map_or(0, |v| v.len() + 1);
    let live_size = |l: &Option<neosh_proto::LiveTurn>| {
        l.as_ref().and_then(|l| serde_json::to_vec(l).ok()).map_or(0, |v| v.len())
    };
    let total = |m: &[neosh_proto::Message], l: &Option<neosh_proto::LiveTurn>| {
        m.iter().map(size_of).sum::<usize>() + live_size(l)
    };
    if total(&messages, &live) <= HISTORY_BUDGET {
        return (messages, live);
    }
    for cap in [64 * 1024, 16 * 1024, 4 * 1024, 1024] {
        for m in &mut messages {
            for b in &mut m.content {
                match b {
                    B::ToolResult { content, .. } => clip_middle(content, cap),
                    B::Thinking { text, .. } => clip_middle(text, cap),
                    B::ToolUse { input, .. } => clip_json(input, cap),
                    B::Text { .. } | B::Image { .. } => {}
                }
            }
        }
        if let Some(l) = live.as_mut() {
            for item in &mut l.said {
                if let neosh_proto::LiveSaid::Tool { call, result } = item {
                    clip_json(&mut call.input, cap);
                    if let Some(r) = result {
                        clip_middle(&mut r.content, cap);
                    }
                }
            }
        }
        if total(&messages, &live) <= HISTORY_BUDGET {
            return (messages, live);
        }
    }
    // Whole turns off the front. A turn starts where somebody said something, so a tool result is
    // never left behind without the call it answers.
    let opens = |m: &neosh_proto::Message| {
        m.role == neosh_proto::Role::User
            && m.content.iter().any(|b| matches!(b, B::Text { .. } | B::Image { .. }))
    };
    let sizes: Vec<usize> = messages.iter().map(size_of).collect();
    let mut sum: usize = sizes.iter().sum::<usize>() + live_size(&live);
    let mut cut = 0;
    while sum > HISTORY_BUDGET && cut + 1 < messages.len() {
        sum -= sizes[cut];
        cut += 1;
        while cut + 1 < messages.len() && !opens(&messages[cut]) {
            sum -= sizes[cut];
            cut += 1;
        }
    }
    messages.drain(..cut);
    (messages, live)
}

/// `text` with its middle taken out when it is longer than `cap` bytes: the head is how it started
/// and the tail is what it decided, which is how a card folds it anyway.
fn clip_middle(text: &mut String, cap: usize) {
    if text.len() <= cap {
        return;
    }
    let half = cap / 2;
    let mut head = half;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - half;
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    let gone = tail - head;
    *text = format!(
        "{}\n… {gone} bytes not sent from the other computer …\n{}",
        &text[..head],
        &text[tail..]
    );
}

/// Every string inside a call's arguments, clipped the same way.
fn clip_json(value: &mut serde_json::Value, cap: usize) {
    match value {
        serde_json::Value::String(s) => clip_middle(s, cap),
        serde_json::Value::Array(items) => items.iter_mut().for_each(|v| clip_json(v, cap)),
        serde_json::Value::Object(map) => map.values_mut().for_each(|v| clip_json(v, cap)),
        _ => {}
    }
}

/// What a conversation looks like from outside.
///
/// `Blocked` is not derivable from `SessionInfo` — the host knows about a pending approval and the
/// store does not — so it is applied by the caller afterwards. Coarse on purpose: this is sent on
/// every change, and a state with more cases than a person can act on is bytes for nothing.
fn state_of(info: &SessionInfo) -> AgentState {
    if info.active_turn.is_some() { AgentState::Running } else { AgentState::Idle }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn said(role: neosh_proto::Role, block: neosh_proto::ContentBlock) -> neosh_proto::Message {
        neosh_proto::Message { role, content: vec![block], at: None }
    }

    fn question(text: &str) -> neosh_proto::Message {
        said(neosh_proto::Role::User, neosh_proto::ContentBlock::Text { text: text.into() })
    }

    fn output(n: usize) -> neosh_proto::Message {
        said(neosh_proto::Role::User, neosh_proto::ContentBlock::ToolResult {
            tool_use_id: neosh_proto::ToolCallId("t".into()),
            content: "x".repeat(n),
            is_error: false,
            images: Vec::new(),
        })
    }

    fn size(messages: &[neosh_proto::Message]) -> usize {
        serde_json::to_vec(messages).map_or(0, |v| v.len())
    }

    /// A conversation that fits goes as it is.
    #[test]
    fn a_history_that_fits_is_left_alone() {
        let messages = vec![question("hello"), output(1000)];
        let (out, _) = fit_history(messages.clone(), None);
        assert_eq!(out, messages);
    }

    /// One enormous command output is clipped from the middle rather than dropping the link — or
    /// the conversation.
    #[test]
    fn a_huge_tool_output_is_clipped_from_the_middle() {
        let messages = vec![question("build it"), output(HISTORY_BUDGET * 2)];
        let (out, _) = fit_history(messages, None);
        assert!(size(&out) <= HISTORY_BUDGET);
        assert_eq!(out.len(), 2, "nothing dropped, only clipped");
        let neosh_proto::ContentBlock::ToolResult { content, .. } = &out[1].content[0] else {
            panic!("still a result");
        };
        assert!(content.contains("bytes not sent"), "says so");
    }

    /// When clipping is not enough the oldest turns go, whole, and the newest stays.
    #[test]
    fn the_oldest_turns_go_first_and_whole() {
        let mut messages = Vec::new();
        for i in 0..4000 {
            messages.push(question(&format!("question {i} {}", "y".repeat(2000))));
            messages.push(output(10));
        }
        let (out, _) = fit_history(messages, None);
        assert!(size(&out) <= HISTORY_BUDGET);
        let neosh_proto::ContentBlock::Text { text } = &out[0].content[0] else {
            panic!("starts at a question, not an orphaned result");
        };
        assert!(text.starts_with("question"));
        let neosh_proto::ContentBlock::Text { text } = &out[out.len() - 2].content[0] else {
            panic!("the newest question is kept");
        };
        assert!(text.starts_with("question 3999"));
    }

    fn node(id: &str, name: &str) -> NodeInfo {
        NodeInfo {
            id: NodeId(id.into()),
            name: name.into(),
            os: "test".into(),
            version: "0.1.0".into(),
        }
    }

    fn caps() -> NodeCapabilities {
        NodeCapabilities {
            accepts_commands: true,
            accepts_approvals: false,
            streams: true,
            browse: true,
            shells: false,
            rich_stream: true,
            catalogue: true,
            live: true,
            projects: Vec::new(),
        }
    }

    fn agent(session: &str, project: &str, updated: i64) -> AgentSummary {
        AgentSummary {
            session: SessionId(session.into()),
            project: ProjectKey(project.into()),
            project_name: project.into(),
            cwd: format!("/w/{session}"),
            repo_root: None,
            branch: None,
            label: session.into(),
            state: AgentState::Idle,
            message_count: 0,
            model: None,
            instance: None,
            mode: None,
            options: Vec::new(),
            context_tokens: 0,
            context_window: None,
            turn_started_at: None,
            updated_at: updated,
            usage: Default::default(),
            unread: false,
            interrupted: false,
            background: 0,
            decorations: Vec::new(),
        }
    }

    fn with_two() -> Swarm {
        let mut s = Swarm::default();
        s.peer_up(node("aa", "mac-studio"), caps());
        s.peer_up(node("bb", "linux-box"), caps());
        s.inventory(
            &NodeId("aa".into()),
            true,
            vec![agent("s1", "git:x/neosh", 10), agent("s2", "git:x/api", 20)],
            vec![],
        );
        s.inventory(&NodeId("bb".into()), true, vec![agent("s3", "git:x/neosh", 30)], vec![]);
        s
    }

    #[test]
    fn a_full_inventory_replaces_and_sorts_by_recency() {
        let s = with_two();
        let a = s.peer(&NodeId("aa".into())).expect("peer");
        assert_eq!(a.agents[0].session, SessionId("s2".into()), "most recent first");
    }

    #[test]
    fn a_delta_adds_updates_and_removes() {
        let mut s = with_two();
        let mut renamed = agent("s1", "git:x/neosh", 40);
        renamed.label = "renamed".into();
        s.inventory(
            &NodeId("aa".into()),
            false,
            vec![renamed, agent("s9", "git:x/new", 50)],
            vec![SessionId("s2".into())],
        );
        let a = s.peer(&NodeId("aa".into())).expect("peer");
        let labels: Vec<&str> = a.agents.iter().map(|x| x.label.as_str()).collect();
        assert_eq!(labels, vec!["s9", "renamed"], "s2 gone, s1 updated, s9 added, newest first");
    }

    /// The question behind "which computers is this project on".
    #[test]
    fn a_project_says_which_other_machines_have_it() {
        let s = with_two();
        assert_eq!(s.hosts_of(&ProjectKey("git:x/neosh".into())), vec!["linux-box", "mac-studio"]);
        assert_eq!(s.hosts_of(&ProjectKey("git:x/api".into())), vec!["mac-studio"]);
        assert!(s.hosts_of(&ProjectKey("git:x/nowhere".into())).is_empty());
    }

    /// A machine that has gone quiet keeps its rows — they were real a moment ago and are probably
    /// still running. What changed is that we cannot see them.
    #[test]
    fn a_peer_that_goes_down_keeps_its_rows_but_leaves_the_actionable_list() {
        let mut s = with_two();
        s.peer_down(&NodeId("bb".into()), "connection closed".into(), true);

        let bb = s.peer(&NodeId("bb".into())).expect("still listed");
        assert!(!bb.up());
        assert_eq!(bb.reason.as_deref(), Some("connection closed"));
        assert_eq!(bb.agents.len(), 1, "and its conversations are still described");

        let live: Vec<&str> = s.agents().map(|(_, a)| a.label.as_str()).collect();
        assert_eq!(live.len(), 2, "but they are not things you can act on");
        assert!(!live.contains(&"s3"));
        // And it stops counting towards "this project is on these machines".
        assert_eq!(s.hosts_of(&ProjectKey("git:x/neosh".into())), vec!["mac-studio"]);
    }

    /// Coming back is not a fresh peer: the row is the same one, now up again.
    #[test]
    fn a_peer_that_comes_back_is_the_same_row() {
        let mut s = with_two();
        s.peer_down(&NodeId("bb".into()), "gone".into(), true);
        s.peer_up(node("bb", "linux-box"), caps());
        let bb = s.peer(&NodeId("bb".into())).expect("peer");
        assert!(bb.up());
        assert_eq!(bb.reason, None, "and it is no longer explaining itself");
    }

    /// The difference a person reads: a peer that was here is `reconnecting`, one that never
    /// answered is still `connecting`, and one nothing is dialling is `down` — three sentences
    /// the dialler's single loop cannot tell apart on its own.
    #[test]
    fn a_lost_peer_retries_and_a_never_seen_peer_connects() {
        let mut s = Swarm::default();
        s.seed(NodeId("aa".into()), "mac-studio".into(), true);
        assert_eq!(
            s.peer(&NodeId("aa".into())).expect("seeded").link,
            LinkState::Connecting { attempt: 0 },
            "a seeded row is being dialled from the first frame"
        );

        s.dial_failed(&NodeId("aa".into()), 3, "connection refused".into());
        let aa = s.peer(&NodeId("aa".into())).expect("peer");
        assert_eq!(aa.link, LinkState::Connecting { attempt: 3 }, "never up, still connecting");
        assert_eq!(aa.reason.as_deref(), Some("connection refused"));

        s.peer_up(node("aa", "mac-studio"), caps());
        s.peer_down(&NodeId("aa".into()), "connection closed".into(), true);
        s.dial_failed(&NodeId("aa".into()), 1, "no route to host".into());
        assert_eq!(
            s.peer(&NodeId("aa".into())).expect("peer").link,
            LinkState::Retrying { attempt: 1 },
            "once it has been up, the same failure reads as reconnecting"
        );

        s.peer_down(&NodeId("aa".into()), "disconnected by you".into(), false);
        assert_eq!(
            s.peer(&NodeId("aa".into())).expect("peer").link,
            LinkState::Down,
            "and with no dialler behind it, it is simply down"
        );
    }

    /// A peer that only dials us is not "connecting": nothing here is trying.
    #[test]
    fn a_seeded_peer_nobody_dials_is_down_not_connecting() {
        let mut s = Swarm::default();
        s.seed(NodeId("aa".into()), "laptop".into(), false);
        assert_eq!(s.peer(&NodeId("aa".into())).expect("seeded").link, LinkState::Down);
    }

    /// The losing half of a simultaneous open reports a failure a moment after the winning half
    /// connected. That report must not repaint an up row.
    #[test]
    fn a_dial_failure_never_downgrades_an_up_peer() {
        let mut s = with_two();
        s.dial_failed(&NodeId("aa".into()), 1, "connection reset".into());
        let aa = s.peer(&NodeId("aa".into())).expect("peer");
        assert!(aa.up(), "the connection the listener holds is the truth");
        assert_eq!(aa.reason, None);
    }

    /// The far half of pairing is its own row, and it says what it is waiting for.
    ///
    /// This is what "connecting…" for ever was: the dialler reached the machine, proved who it
    /// was, and the far end — which has not been told about this one — simply stopped talking.
    /// Reported as `DialFailed` with attempt 0, it landed on `Connecting { attempt: 0 }`, and the
    /// panel's `attempt > 0` branch was the only one that printed the reason. So the single state
    /// with something actionable in it was the single state that said nothing.
    #[test]
    fn a_machine_that_has_not_allowed_this_one_says_so_rather_than_connecting_for_ever() {
        let mut s = Swarm::default();
        s.seed(NodeId("aa".into()), "the address you typed".into(), true);

        s.unapproved(node("aa", "mini"), caps());
        let aa = s.peer(&NodeId("aa".into())).expect("peer");
        assert_eq!(aa.link, LinkState::Waiting, "reached, and waiting on a person over there");
        assert!(!aa.up(), "a handshake that got no further is not a connection");
        assert_eq!(
            aa.info.name, "mini",
            "the handshake got far enough to learn the real name, so the row stops saying the \
             address somebody typed"
        );
        assert_eq!(aa.reason, None, "the state is the whole story; a sentence would repeat it");

        // Somebody presses the key over there. The dialler is still retrying at the heartbeat, so
        // this arrives without anything happening on this machine.
        s.peer_up(node("aa", "mini"), caps());
        assert!(s.peer(&NodeId("aa".into())).expect("peer").up());
    }

    /// Waiting is not sticky: a machine that stops answering its port has genuinely changed state,
    /// and telling somebody to go and press a key on a computer that is off is worse than silence.
    #[test]
    fn a_waiting_peer_that_goes_away_stops_saying_go_and_press_a_key() {
        let mut s = Swarm::default();
        s.seed(NodeId("aa".into()), "mini".into(), true);
        s.unapproved(node("aa", "mini"), caps());
        s.dial_failed(&NodeId("aa".into()), 2, "connection refused".into());
        let aa = s.peer(&NodeId("aa".into())).expect("peer");
        assert_eq!(aa.link, LinkState::Connecting { attempt: 2 });
        assert_eq!(aa.reason.as_deref(), Some("connection refused"));
    }

    /// A name somebody chose outlives everything the machine says about itself.
    #[test]
    fn an_alias_wins_over_the_announced_name_and_survives_a_reconnect() {
        let mut s = Swarm::default();
        s.seed(NodeId("aa".into()), "Mac".into(), true);
        assert_eq!(s.peer(&NodeId("aa".into())).expect("peer").display_name(), "Mac");

        s.set_alias(&NodeId("aa".into()), Some("  the loud one  ".into()));
        assert_eq!(
            s.peer(&NodeId("aa".into())).expect("peer").display_name(),
            "the loud one",
            "trimmed, because a name with an accidental space is a name that sorts oddly"
        );

        // A handshake replaces everything the machine announces. It must not replace this.
        s.peer_up(node("aa", "Mac"), caps());
        assert_eq!(s.peer(&NodeId("aa".into())).expect("peer").display_name(), "the loud one");

        // And clearing it gives the announced name back, so a rename is undoable without anybody
        // having to remember what the machine used to be called.
        s.set_alias(&NodeId("aa".into()), None);
        assert_eq!(s.peer(&NodeId("aa".into())).expect("peer").display_name(), "Mac");
    }

    /// Seeding is idempotent and never clobbers a row that has learned anything.
    #[test]
    fn seeding_an_existing_row_changes_nothing() {
        let mut s = with_two();
        s.seed(NodeId("aa".into()), "renamed".into(), true);
        let aa = s.peer(&NodeId("aa".into())).expect("peer");
        assert_eq!(aa.info.name, "mac-studio");
        assert!(aa.up());
    }

    /// Inventory from a node we have no record of is dropped rather than inventing a peer with no
    /// name, no capabilities and no way to be talked to.
    #[test]
    fn inventory_from_an_unknown_node_is_dropped() {
        let mut s = Swarm::default();
        s.inventory(&NodeId("zz".into()), true, vec![agent("s1", "p", 1)], vec![]);
        assert_eq!(s.peers().count(), 0);
    }
}
