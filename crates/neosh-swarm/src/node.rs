//! A running node: the listener, the diallers, and the peer table between them.
//!
//! The shape is one task per connection plus one loop that owns everything shared. Connections
//! push what they read into a single channel and the loop is the only thing that touches the peer
//! table, so there is no lock anywhere in here — a swarm where two peers arriving at once can
//! interleave their effects on a shared map is a swarm with a bug you reproduce once a fortnight.
//!
//! The host talks to the loop the same way: [`SwarmRequest`] in, [`SwarmEvent`] out, both over
//! channels. That is what makes the node testable without a host and the host testable without a
//! network.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use neosh_proto::{
    AgentCommand, AgentSummary, AscpMessage, ModelCatalogue, NodeCapabilities, NodeId, NodeInfo,
    Refusal,
    RemoteProject, SessionId, StreamEvent,
};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::allow::Allowed;
use crate::handshake::{accept, dial};
use crate::identity::Identity;
use crate::wire::{read_message, write_message};

/// A pty's bytes, as a JSON string can hold them.
///
/// Base64 and not, say, escaping: a pty carries whatever the program wrote, which is arbitrary
/// bytes — a lone `0x9b`, half a UTF-8 sequence split across two reads, the middle of a sixel — and
/// none of that is representable in a JSON string at all. The cost is a third more bytes on a
/// channel that is already a terminal's worth of traffic, which is nothing beside a length-prefixed
/// JSON frame per burst.
fn encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The other way. A frame that will not decode is dropped rather than failing the connection: it is
/// one burst of one terminal's output, and taking the link down over it would lose every
/// conversation on it as well.
fn decode(data: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(data).ok()
}

/// How a node is set up. Everything here comes from `[swarm]` in the user's config.
#[derive(Debug, Clone)]
pub struct SwarmConfig {
    /// What to call this machine on other people's screens. The hostname unless somebody chose.
    pub name: String,
    /// Where to listen. `None` means dial-only — a laptop that joins other machines without
    /// being joinable, which is the right setting for anything that moves between networks.
    pub listen: Option<SocketAddr>,
    /// Machines to keep a connection to. Added to by pairing while the node runs.
    pub peers: Vec<PeerAddress>,
    pub accepts_commands: bool,
    pub accepts_approvals: bool,
    /// Whether a peer may open a pty here. See [`neosh_proto::NodeCapabilities::shells`].
    pub accepts_shells: bool,
    /// How often to say "still here". Also the reconnect interval, which is deliberate: those are
    /// the same number in the sense that matters, which is how long a board may be wrong for.
    pub heartbeat: Duration,
}

impl Default for SwarmConfig {
    fn default() -> Self {
        Self {
            name: "neosh".into(),
            listen: None,
            peers: Vec::new(),
            accepts_commands: true,
            accepts_approvals: false,
            accepts_shells: false,
            heartbeat: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerAddress {
    pub addr: String,
    /// The id we expect to find there, when the user wrote one down.
    ///
    /// Optional because an address is how you *reach* a node and its key is how you *know* it: the
    /// allow-list is what authorises, and this only catches "I meant the other machine" earlier
    /// than the allow-list would.
    pub expect: Option<NodeId>,
}

/// Something the host wants the swarm to do.
#[derive(Debug, Clone)]
pub enum SwarmRequest {
    /// Local agents changed. Fanned out to every connected peer.
    Publish { full: bool, agents: Vec<AgentSummary>, gone: Vec<SessionId> },
    /// What this node offers, for the next `Presence`.
    Projects { projects: Vec<RemoteProject> },
    /// Ask a peer to do something with one of its agents.
    Command { node: NodeId, id: String, session: SessionId, command: AgentCommand },
    /// Answer a request that came in.
    Answer { node: NodeId, id: String, result: Result<Option<SessionId>, Refusal> },
    /// Ask a peer which of its directories start with `prefix`.
    Browse { node: NodeId, id: String, prefix: String },
    /// Answer a [`SwarmEvent::Browse`] that came in.
    Browsed { node: NodeId, id: String, result: Result<Vec<String>, Refusal> },
    /// Ask a peer which models it can run a conversation with.
    Catalogue { node: NodeId, id: String, refresh: bool },
    /// Answer a [`SwarmEvent::Catalogue`] that came in.
    Catalogued { node: NodeId, id: String, result: Result<ModelCatalogue, Refusal> },
    Subscribe { node: NodeId, session: SessionId },
    Unsubscribe { node: NodeId, session: SessionId },
    /// Something happened in a local session. Sent only to peers that subscribed to it.
    Stream { session: SessionId, event: StreamEvent },
    /// Authorise a machine and start keeping a connection to it, without a restart.
    ///
    /// `addr` is absent for one that dialled us: it knows where we are, and the address it happened
    /// to arrive from is not necessarily one we could dial back on.
    Pair { id: NodeId, name: String, addr: Option<String> },
    /// Withdraw authorisation. The connection, if there is one, goes with it.
    Unpair { id: NodeId },
    /// Ask a peer to open a shell. Answered once, with [`SwarmEvent::PtyOpened`].
    PtyOpen { node: NodeId, id: String, cwd: Option<String>, cols: u16, rows: u16 },
    /// Answer a [`SwarmEvent::PtyOpen`] that came in.
    PtyOpened { node: NodeId, id: String, result: Result<String, Refusal> },
    /// Bytes for a pty, in whichever direction this node is going. Keystrokes from the viewer,
    /// output from the owner — the same message, because the connection is symmetric and a pty is
    /// two pipes that do not otherwise differ.
    PtyData { node: NodeId, pty: String, data: Vec<u8> },
    /// The viewer's window changed size. Only ever travels from the side that is drawing.
    PtyResize { node: NodeId, pty: String, cols: u16, rows: u16 },
    /// Close it, from either end.
    PtyClose { node: NodeId, pty: String },
    /// The shell exited here. Only ever travels from the owner.
    PtyExit { node: NodeId, pty: String, status: Option<i32> },
    /// Dial a down peer again now, resetting its backoff, rather than waiting the delay out.
    Redial { id: NodeId },
    /// Close the connection to a peer and stop dialling it, keeping the pairing.
    ///
    /// [`SwarmRequest::Redial`] is the way back. The peer may still dial in — it is still on the
    /// allow-list — and a restart dials it again; this is for this workspace's lifetime only.
    Disconnect { id: NodeId },
    Shutdown,
}

/// Something the host needs to know.
#[derive(Debug, Clone)]
pub enum SwarmEvent {
    /// The listening socket is bound, on this address. Said once, at start.
    Listening { addr: SocketAddr },
    /// The listening socket could not be bound — most often a port another workspace holds.
    /// Dialling still works; a board should say the machine cannot be *reached*, not that the
    /// swarm is broken.
    ListenFailed { addr: SocketAddr, error: String },
    PeerUp { node: NodeInfo, capabilities: NodeCapabilities },
    /// A peer went away. `reason` is for the log and for a board that says why. `will_retry` says
    /// whether a dialler is still working on it — the difference between "reconnecting" and
    /// "gone", which is the whole of what a person would do differently.
    PeerDown { node: NodeId, reason: String, will_retry: bool },
    /// A dial did not produce a connection. `attempt` counts since the last success, so a board
    /// can say `try 4`; `node` is absent for an address whose machine we have never identified.
    DialFailed { node: Option<NodeId>, addr: String, attempt: u32, error: String },
    /// Reached a machine we have authorised, and it has not authorised us back.
    ///
    /// Pairing is a decision each machine makes, so this is the ordinary state between the two
    /// halves rather than an error — and it is the one a board most needs to name, because the
    /// only thing that ends it is somebody pressing a key on the *other* machine. Reported as its
    /// own event rather than as a [`SwarmEvent::DialFailed`] with a distinctive sentence in it: a
    /// board that has to match on an error string to draw the right row is one sentence rewrite
    /// away from drawing the wrong one.
    ///
    /// Carries the full [`NodeInfo`] because the handshake got that far. A row that has only ever
    /// been half-paired can therefore still say what the machine is actually called, what it is
    /// running and what it offers — all of which arrived before it stopped talking to us.
    Unapproved { node: NodeInfo, capabilities: NodeCapabilities, addr: String },
    Inventory { node: NodeId, full: bool, agents: Vec<AgentSummary>, gone: Vec<SessionId> },
    Stream { node: NodeId, session: SessionId, event: StreamEvent },
    /// A peer asked us to do something. The host must answer with [`SwarmRequest::Answer`].
    Command { node: NodeId, id: String, session: SessionId, command: AgentCommand },
    /// A command we sent has been answered. Exactly one of these per command, ever.
    Answer { node: NodeId, id: String, result: Result<Option<SessionId>, Refusal> },
    /// A peer wants to know which of our directories start with `prefix`. The host must answer with
    /// [`SwarmRequest::Browsed`].
    Browse { node: NodeId, id: String, prefix: String },
    /// A browse we sent has been answered. Exactly one of these per browse, ever.
    Browsed { node: NodeId, id: String, result: Result<Vec<String>, Refusal> },
    /// A peer wants our model catalogue. The host must answer with [`SwarmRequest::Catalogued`].
    Catalogue { node: NodeId, id: String, refresh: bool },
    /// A catalogue we asked for. A refusal arrives as [`Self::Answer`], as every refusal does.
    Catalogued { node: NodeId, id: String, catalogue: ModelCatalogue },
    /// A peer wants a shell here. The host must answer with [`SwarmRequest::PtyOpened`].
    PtyOpen { node: NodeId, id: String, cwd: Option<String>, cols: u16, rows: u16 },
    /// A shell we asked for has been answered. Exactly one of these per open, ever.
    PtyOpened { node: NodeId, id: String, result: Result<String, Refusal> },
    /// Bytes for a pty — the shell's output if we asked for it, a viewer's keystrokes if we own it.
    PtyData { node: NodeId, pty: String, data: Vec<u8> },
    /// A viewer's window changed size, so `SIGWINCH` on the pty we own.
    PtyResize { node: NodeId, pty: String, cols: u16, rows: u16 },
    /// The other end closed it.
    PtyClose { node: NodeId, pty: String },
    /// A shell we were watching exited over there.
    PtyExit { node: NodeId, pty: String, status: Option<i32> },
    /// A machine proved who it is and is not one we have paired with.
    ///
    /// The beginning of pairing rather than the end of a connection. Reported for both directions:
    /// a machine we dialled and did not recognise, and one that dialled us. Either way the id here
    /// has been *proven*, which is what makes it safe to show somebody and ask.
    Stranger { node: NodeInfo, dialled: bool },
    /// A peer wants to watch one of our sessions, so the host should start sending its events.
    Subscribed { node: NodeId, session: SessionId },
    Unsubscribed { node: NodeId, session: SessionId },
}

/// What the host holds.
///
/// Clonable so a spawned task can answer a peer without the host loop having to wait for it: the
/// only thing in here is a sender and an id, and every request on it is fire-and-forget.
#[derive(Clone)]
pub struct SwarmHandle {
    tx: mpsc::UnboundedSender<SwarmRequest>,
    id: NodeId,
}

impl SwarmHandle {
    pub fn id(&self) -> &NodeId {
        &self.id
    }

    /// Fire and forget. A closed channel means the swarm task is gone, which is not something the
    /// caller can do anything about and not a reason to fail whatever it was doing.
    pub fn send(&self, req: SwarmRequest) {
        let _ = self.tx.send(req);
    }
}

/// One connected peer, from the loop's point of view.
struct Connected {
    info: NodeInfo,
    capabilities: NodeCapabilities,
    out: mpsc::UnboundedSender<AscpMessage>,
    /// Sessions of *ours* this peer is watching.
    ///
    /// Held per peer rather than per session because that is the direction the question is asked
    /// in: when a local turn produces a token, the thing to answer is "who wants this", and a set
    /// per peer answers it with one pass over a handful of peers.
    watching: std::collections::HashSet<SessionId>,
}

/// What a connection task reports back to the loop.
enum Wire {
    Up {
        info: NodeInfo,
        capabilities: NodeCapabilities,
        /// The **only** sender for this connection once the loop holds it: `serve` gives its own
        /// clone up at announce time, so the loop dropping this is what actually closes the
        /// writer, sends the FIN, and lets both ends unwind. A serve that kept a clone was a
        /// connection nothing could close — dropped peers stayed as idle sockets for ever.
        out: mpsc::UnboundedSender<AscpMessage>,
        /// Whether this end opened the connection. Decides which of two simultaneous connections
        /// survives; see the tie-break where this is read.
        dialled: bool,
        /// The address this end dialled, when it did — how the loop learns which address a node
        /// id lives at, for a `Redial` or `Disconnect` aimed at a peer the allow-list only knows
        /// by key.
        addr: Option<String>,
    },
    Message { node: NodeId, message: AscpMessage },
    Down { node: Option<NodeId>, reason: String },
    Stranger { node: NodeInfo, dialled: bool },
}

/// Start a node. Returns the handle the host keeps and the events it should read.
///
/// The identity is shared rather than owned because both the listener and every dialler need it,
/// and it is immutable once loaded — the allow-list is decided by config, and changing it means a
/// reload, which restarts this.
pub fn spawn(
    identity: Arc<Identity>,
    allowed: Allowed,
    config: SwarmConfig,
    version: String,
) -> (SwarmHandle, mpsc::UnboundedReceiver<SwarmEvent>) {
    let (req_tx, req_rx) = mpsc::unbounded_channel();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel();
    let id = identity.id().clone();
    tokio::spawn(run(identity, allowed, config, version, req_rx, evt_tx));
    (SwarmHandle { tx: req_tx, id }, evt_rx)
}

async fn run(
    identity: Arc<Identity>,
    allowed: Allowed,
    config: SwarmConfig,
    version: String,
    mut requests: mpsc::UnboundedReceiver<SwarmRequest>,
    events: mpsc::UnboundedSender<SwarmEvent>,
) {
    let info = NodeInfo {
        id: identity.id().clone(),
        name: config.name.clone(),
        os: std::env::consts::OS.to_string(),
        version,
    };
    let mut caps = NodeCapabilities {
        accepts_commands: config.accepts_commands,
        accepts_approvals: config.accepts_approvals,
        streams: true,
        // A build fact rather than a setting: it says this binary understands the message, and what
        // decides whether the message is *answered* is `accepts_commands`, checked when one
        // arrives. A knob here would be a second permission for something already permitted.
        browse: true,
        // A setting rather than a build fact, which is where this parts company with `browse`
        // above: that one says "this binary understands the message" because answering it is
        // already covered by `accepts_commands`. Opening a shell is not covered by anything, so
        // what is advertised is what was asked for — and it is checked again when one arrives,
        // because a capability is a courtesy to the other end's UI and never the enforcement.
        shells: config.accepts_shells,
        // A build fact, like `browse`: this binary reads the tool and question events that make a
        // watched conversation a whole transcript.
        rich_stream: true,
        catalogue: true,
        live: true,
        projects: Vec::new(),
    };

    let (wire_tx, mut wire_rx) = mpsc::unbounded_channel::<Wire>();

    if let Some(addr) = config.listen {
        match TcpListener::bind(addr).await {
            Ok(sock) => {
                // The socket's own answer, not the config's: `:0` binds a port the config
                // never named, and the board should show the one a peer could actually dial.
                let bound = sock.local_addr().unwrap_or(addr);
                tracing::info!(addr = %bound, node = %identity.id().short(), "swarm listening");
                let _ = events.send(SwarmEvent::Listening { addr: bound });
                tokio::spawn(listen(
                    sock,
                    identity.clone(),
                    allowed.clone(),
                    info.clone(),
                    caps.clone(),
                    wire_tx.clone(),
                ));
            }
            // Not fatal. A machine that cannot listen can still dial, and refusing to start the
            // workspace because a port was taken would be a swarm feature breaking the editor.
            Err(e) => {
                tracing::warn!(%addr, "swarm could not listen: {e}");
                let _ = events.send(SwarmEvent::ListenFailed { addr, error: e.to_string() });
            }
        }
    }

    // One dialler task per address, and pairing adds more. Each is cancelled by its peer being
    // unpaired, which is why the handles are kept rather than detached and forgotten.
    let mut diallers: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    // A macro rather than a closure: a closure would have to borrow `caps`, which the loop below
    // also assigns to when the host reports its projects, and the borrow checker is right to
    // object. Everything here is cloned at spawn time anyway.
    macro_rules! start_dialling {
        ($peer:expr) => {{
            let peer: PeerAddress = $peer;
            if !diallers.contains_key(&peer.addr) {
                let addr = peer.addr.clone();
                let handle = tokio::spawn(keep_dialled(
                    peer,
                    identity.clone(),
                    allowed.clone(),
                    info.clone(),
                    caps.clone(),
                    wire_tx.clone(),
                    events.clone(),
                    config.heartbeat,
                ));
                diallers.insert(addr, handle);
            }
        }};
    }
    for peer in config.peers.clone() {
        start_dialling!(peer);
    }

    let mut peers: HashMap<NodeId, Connected> = HashMap::new();
    // Disconnected by the person, and not to be readmitted until they say so: a peer that dials
    // *us* would otherwise be back on its next heartbeat, which makes "disconnect" a button that
    // works for at most ten seconds. Still on the allow-list — this is "leave me alone for now",
    // and unpairing is the other, stronger verb.
    let mut paused: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
    // Where each node was last reachable, learned from the dial that found it. The allow-list is
    // the first answer; this is the fallback for a peer it only knows by key.
    let mut addr_of: HashMap<NodeId, String> = HashMap::new();
    // The last full picture of our own agents, kept so a peer that connects five minutes in gets a
    // snapshot rather than whatever delta happens next — which for an idle workspace is nothing at
    // all, so the board would stay empty until somebody typed.
    let mut mine: Vec<AgentSummary> = Vec::new();
    let mut seq: u64 = 0;
    let mut beat = tokio::time::interval(config.heartbeat);
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            Some(req) = requests.recv() => {
                match req {
                    SwarmRequest::Shutdown => {
                        for (_, p) in peers.iter() {
                            let _ = p.out.send(AscpMessage::Goodbye { message: None });
                        }
                        return;
                    }
                    SwarmRequest::Publish { full, agents, gone } => {
                        if full {
                            mine = agents.clone();
                        } else {
                            merge(&mut mine, &agents, &gone);
                        }
                        seq += 1;
                        let msg = AscpMessage::Inventory { seq, full, agents, gone };
                        for p in peers.values() {
                            let _ = p.out.send(msg.clone());
                        }
                    }
                    SwarmRequest::Projects { projects } => caps.projects = projects,
                    SwarmRequest::Pair { id, name, addr } => {
                        paused.remove(&id);
                        // Re-pairing keeps whatever this user decided to call it. Unpair is how
                        // you forget a machine; adding one back after a network moved is not, and
                        // having it revert to its hostname would make the rename feel unreliable
                        // in exactly the case somebody renamed it for.
                        let alias = allowed.get(&id).and_then(|p| p.alias);
                        allowed.add(id.clone(), crate::allow::Peer {
                            name,
                            alias,
                            addr: addr.clone(),
                            source: crate::allow::Source::Paired,
                        });
                        // Only dial if we were told where. A machine that dialled us will dial
                        // again on its own schedule, and inventing an address for it would mean
                        // connecting to whatever else is on that host and port.
                        if let Some(addr) = addr {
                            start_dialling!(PeerAddress { addr, expect: Some(id) });
                        }
                    }
                    SwarmRequest::Unpair { id } => {
                        paused.remove(&id);
                        let addr = allowed.get(&id).and_then(|p| p.addr);
                        let _ = allowed.remove(&id);
                        if let Some(addr) = addr
                            && let Some(task) = diallers.remove(&addr)
                        {
                            // Stop reconnecting to a machine we have just stopped trusting.
                            // Without this it would keep dialling and keep being refused, for ever.
                            task.abort();
                        }
                        if let Some(p) = peers.remove(&id) {
                            let _ = p.out.send(AscpMessage::Goodbye {
                                message: Some("no longer paired".into()),
                            });
                            let _ = events.send(SwarmEvent::PeerDown {
                                node: id,
                                reason: "unpaired".into(),
                                will_retry: false,
                            });
                        }
                    }
                    SwarmRequest::Redial { id } => {
                        // Whatever a disconnect held back is welcome again — for a peer that
                        // dials us, this is the whole of what "reconnect" can do.
                        paused.remove(&id);
                        // Already connected means there is nothing to hurry. Otherwise restart the
                        // dialler: a fresh task begins with a fresh backoff, which is the whole
                        // point of somebody pressing the key.
                        if !peers.contains_key(&id)
                            && let Some(addr) = allowed
                                .get(&id)
                                .and_then(|p| p.addr)
                                .or_else(|| addr_of.get(&id).cloned())
                        {
                            if let Some(task) = diallers.remove(&addr) {
                                task.abort();
                            }
                            start_dialling!(PeerAddress { addr, expect: Some(id) });
                        }
                    }
                    SwarmRequest::Disconnect { id } => {
                        paused.insert(id.clone());
                        if let Some(addr) = allowed
                            .get(&id)
                            .and_then(|p| p.addr)
                            .or_else(|| addr_of.get(&id).cloned())
                            && let Some(task) = diallers.remove(&addr)
                        {
                            task.abort();
                        }
                        if let Some(p) = peers.remove(&id) {
                            let _ = p.out.send(AscpMessage::Goodbye {
                                message: Some("disconnected".into()),
                            });
                        }
                        // Said even when there was no live connection: a peer mid-retry that is
                        // told to stop has still changed state, and a board that keeps saying
                        // "reconnecting" about a dialler that is gone is lying.
                        let _ = events.send(SwarmEvent::PeerDown {
                            node: id,
                            reason: "disconnected by you".into(),
                            will_retry: false,
                        });
                    }
                    SwarmRequest::Command { node, id, session, command } => {
                        match peers.get(&node) {
                            Some(p) => {
                                let _ = p.out.send(AscpMessage::Command { id, session, command });
                            }
                            // Answered locally rather than dropped: a command with no reply is a
                            // keystroke you cannot tell from a slow network.
                            None => {
                                let will_retry = allowed
                                    .get(&node)
                                    .and_then(|p| p.addr)
                                    .is_some_and(|a| diallers.contains_key(&a));
                                let _ = events.send(SwarmEvent::PeerDown {
                                    node,
                                    reason: "not connected".into(),
                                    will_retry,
                                });
                            }
                        }
                    }
                    SwarmRequest::Answer { node, id, result } => {
                        if let Some(p) = peers.get(&node) {
                            let _ = p.out.send(match result {
                                Ok(session) => AscpMessage::Ack { id, session },
                                Err(refusal) => AscpMessage::Refused { id, refusal },
                            });
                        }
                    }
                    SwarmRequest::Browse { node, id, prefix } => {
                        match peers.get(&node) {
                            // Never sent to a peer that has not said it understands the question:
                            // an unknown message tag fails the frame and takes the connection with
                            // it, so a directory picker aimed at an older machine would knock it
                            // off the board rather than come back empty. See
                            // `NodeCapabilities::browse`.
                            Some(p) if p.capabilities.browse => {
                                let _ = p.out.send(AscpMessage::Browse { id, prefix });
                            }
                            Some(p) => {
                                let _ = events.send(SwarmEvent::Browsed {
                                    node,
                                    id,
                                    result: Err(Refusal::NotPermitted {
                                        what: format!(
                                            "{} is running neosh {}, which cannot list directories \
                                             for another machine",
                                            p.info.name, p.info.version
                                        ),
                                    }),
                                });
                            }
                            // Answered locally rather than dropped, for the reason `Command` is:
                            // a request with no reply is indistinguishable from a slow network,
                            // and the caller is a field somebody is typing into.
                            None => {
                                let _ = events.send(SwarmEvent::Browsed {
                                    node,
                                    id,
                                    result: Err(Refusal::Busy {
                                        message: "not connected".into(),
                                    }),
                                });
                            }
                        }
                    }
                    SwarmRequest::Catalogue { node, id, refresh } => match peers.get(&node) {
                        // `browse`'s rule: never a message the far end cannot parse.
                        Some(p) if p.capabilities.catalogue => {
                            let _ = p.out.send(AscpMessage::Catalogue { id, refresh });
                        }
                        Some(p) => {
                            let _ = events.send(SwarmEvent::Answer {
                                node,
                                id,
                                result: Err(Refusal::NotPermitted {
                                    what: format!(
                                        "{} is running neosh {}, which cannot list its models for \
                                         another machine",
                                        p.info.name, p.info.version
                                    ),
                                }),
                            });
                        }
                        None => {
                            let _ = events.send(SwarmEvent::Answer {
                                node,
                                id,
                                result: Err(Refusal::Busy { message: "not connected".into() }),
                            });
                        }
                    },
                    SwarmRequest::Catalogued { node, id, result } => {
                        if let Some(p) = peers.get(&node) {
                            let _ = p.out.send(match result {
                                Ok(catalogue) => AscpMessage::Catalogued { id, catalogue },
                                Err(refusal) => AscpMessage::Refused { id, refusal },
                            });
                        }
                    }
                    SwarmRequest::Browsed { node, id, result } => {
                        if let Some(p) = peers.get(&node) {
                            let _ = p.out.send(match result {
                                Ok(paths) => AscpMessage::Browsed { id, paths },
                                Err(refusal) => AscpMessage::Refused { id, refusal },
                            });
                        }
                    }
                    SwarmRequest::PtyOpen { node, id, cwd, cols, rows } => {
                        // `browse`'s rule, and one more clause: that flag is only ever a
                        // compatibility check because answering is already covered by
                        // `accepts_commands`, and this one is a permission as well. Both failures
                        // are the same sentence to whoever pressed the key — no shell here — so
                        // they are one arm with the machine's own words in it.
                        match peers.get(&node) {
                            Some(p) if p.capabilities.shells => {
                                let _ = p.out.send(AscpMessage::PtyOpen { id, cwd, cols, rows });
                            }
                            Some(p) => {
                                let _ = events.send(SwarmEvent::PtyOpened {
                                    node,
                                    id,
                                    result: Err(Refusal::NotPermitted {
                                        what: format!(
                                            "{} does not open shells for other machines — set \
                                             `accepts_shells = true` under `[swarm]` there",
                                            p.info.name
                                        ),
                                    }),
                                });
                            }
                            None => {
                                let _ = events.send(SwarmEvent::PtyOpened {
                                    node,
                                    id,
                                    result: Err(Refusal::Busy {
                                        message: "not connected".into(),
                                    }),
                                });
                            }
                        }
                    }
                    SwarmRequest::PtyOpened { node, id, result } => {
                        if let Some(p) = peers.get(&node) {
                            let _ = p.out.send(match result {
                                Ok(pty) => AscpMessage::PtyOpened { id, pty },
                                Err(refusal) => AscpMessage::Refused { id, refusal },
                            });
                        }
                    }
                    SwarmRequest::PtyData { node, pty, data } => {
                        if let Some(p) = peers.get(&node) {
                            let _ = p.out.send(AscpMessage::PtyData { pty, data: encode(&data) });
                        }
                    }
                    SwarmRequest::PtyResize { node, pty, cols, rows } => {
                        if let Some(p) = peers.get(&node) {
                            let _ = p.out.send(AscpMessage::PtyResize { pty, cols, rows });
                        }
                    }
                    SwarmRequest::PtyClose { node, pty } => {
                        if let Some(p) = peers.get(&node) {
                            let _ = p.out.send(AscpMessage::PtyClose { pty });
                        }
                    }
                    SwarmRequest::PtyExit { node, pty, status } => {
                        if let Some(p) = peers.get(&node) {
                            let _ = p.out.send(AscpMessage::PtyExit { pty, status });
                        }
                    }
                    SwarmRequest::Subscribe { node, session } => {
                        if let Some(p) = peers.get(&node) {
                            let _ = p.out.send(AscpMessage::Subscribe { session });
                        }
                    }
                    SwarmRequest::Unsubscribe { node, session } => {
                        if let Some(p) = peers.get(&node) {
                            let _ = p.out.send(AscpMessage::Unsubscribe { session });
                        }
                    }
                    SwarmRequest::Stream { session, event } => {
                        // Only to peers that asked. This is the message family that would otherwise
                        // be one per token per peer, and the subscription is the whole of what
                        // keeps a twenty-machine swarm quiet.
                        //
                        // And only what each of them can read: a tool or question event to a peer
                        // built before they existed is a frame it cannot parse, which drops the
                        // link. It goes on getting the words, as it always did.
                        for p in peers.values() {
                            if event.is_rich() && !p.capabilities.rich_stream {
                                continue;
                            }
                            if event.needs_live() && !p.capabilities.live {
                                continue;
                            }
                            if p.watching.contains(&session) {
                                let _ = p.out.send(AscpMessage::Stream {
                                    session: session.clone(),
                                    event: event.clone(),
                                });
                            }
                        }
                    }
                }
            }
            Some(w) = wire_rx.recv() => match w {
                Wire::Up { info: peer_info, capabilities, out, dialled, addr } => {
                    let id = peer_info.id.clone();
                    if let Some(addr) = addr {
                        addr_of.insert(id.clone(), addr);
                    }
                    // Disconnected by the person, and it dialled back in. Shut the door quietly:
                    // no `PeerDown`, because it never came up.
                    if paused.contains(&id) {
                        drop(out);
                        continue;
                    }
                    // Two nodes that both dial each other end up with two connections, and both of
                    // them work. Something has to drop one, and *both ends have to drop the same
                    // one* — the first version of this kept whichever arrived later, which each
                    // side judged independently and therefore differently: each kept a socket the
                    // other had just abandoned, saw it close, reconnected, and did it again. It
                    // looked like a flapping network and was arithmetic.
                    //
                    // So the survivor is decided by something both sides compute identically: the
                    // connection dialled by whichever node has the smaller id. No clocks, no
                    // ordering, no negotiation.
                    if peers.contains_key(&id) {
                        let mine_wins = if dialled {
                            identity.id() < &id
                        } else {
                            &id < identity.id()
                        };
                        if !mine_wins {
                            // Close it quietly. Not a `PeerDown`: the peer is not going anywhere,
                            // this is one of two doors to it being shut.
                            drop(out);
                            continue;
                        }
                        if let Some(old) = peers.remove(&id) {
                            drop(old);
                        }
                    }
                    seq += 1;
                    let _ = out.send(AscpMessage::Inventory {
                        seq,
                        full: true,
                        agents: mine.clone(),
                        gone: Vec::new(),
                    });
                    peers.insert(id, Connected {
                        info: peer_info.clone(),
                        capabilities: capabilities.clone(),
                        out,
                        watching: Default::default(),
                    });
                    let _ = events.send(SwarmEvent::PeerUp { node: peer_info, capabilities });
                }
                Wire::Stranger { node, dialled } => {
                    let _ = events.send(SwarmEvent::Stranger { node, dialled });
                }
                Wire::Down { node, reason } => {
                    // Only if this *is* the connection we are using. The losing half of a
                    // simultaneous open reports itself down a moment after being dropped, and
                    // acting on that would take the winning connection out with it.
                    if let Some(id) = node
                        && peers.get(&id).is_some_and(|p| p.out.is_closed())
                    {
                        peers.remove(&id);
                        let will_retry = allowed
                            .get(&id)
                            .and_then(|p| p.addr)
                            .is_some_and(|a| diallers.contains_key(&a));
                        let _ = events.send(SwarmEvent::PeerDown { node: id, reason, will_retry });
                    }
                }
                Wire::Message { node, message } => match message {
                    AscpMessage::Presence { node: peer_info, capabilities, .. } => {
                        if let Some(p) = peers.get_mut(&node) {
                            p.info = peer_info.clone();
                            p.capabilities = capabilities.clone();
                        }
                        let _ = events.send(SwarmEvent::PeerUp { node: peer_info, capabilities });
                    }
                    AscpMessage::Inventory { full, agents, gone, .. } => {
                        let _ = events.send(SwarmEvent::Inventory { node, full, agents, gone });
                    }
                    AscpMessage::Subscribe { session } => {
                        if let Some(p) = peers.get_mut(&node) {
                            p.watching.insert(session.clone());
                        }
                        let _ = events.send(SwarmEvent::Subscribed { node, session });
                    }
                    AscpMessage::Unsubscribe { session } => {
                        if let Some(p) = peers.get_mut(&node) {
                            p.watching.remove(&session);
                        }
                        let _ = events.send(SwarmEvent::Unsubscribed { node, session });
                    }
                    AscpMessage::Stream { session, event } => {
                        let _ = events.send(SwarmEvent::Stream { node, session, event });
                    }
                    AscpMessage::Command { id, session, command } => {
                        // Refused here rather than passed up when this node does not accept them
                        // at all: the host should not have to re-derive a policy it already
                        // declared, and a refusal that never reaches the caller is a hang.
                        // A mode is every future prompt answered in advance, so it is held to
                        // the rule a single answer is.
                        let allowed = if matches!(
                            command,
                            AgentCommand::Approve { .. } | AgentCommand::SetMode { .. }
                        ) {
                            caps.accepts_commands && caps.accepts_approvals
                        } else {
                            caps.accepts_commands
                        };
                        if !allowed {
                            if let Some(p) = peers.get(&node) {
                                let _ = p.out.send(AscpMessage::Refused {
                                    id,
                                    refusal: Refusal::NotPermitted {
                                        what: "this node does not accept that".into(),
                                    },
                                });
                            }
                        } else {
                            let _ = events.send(SwarmEvent::Command { node, id, session, command });
                        }
                    }
                    AscpMessage::Browse { id, prefix } => {
                        // The same gate the command path applies, and deliberately the same one:
                        // a node that accepts commands already accepts `NewSession` with any path
                        // on its disk, so naming the directories that exist is strictly less than
                        // it has given away. A node that does not is not asked to list anything.
                        if caps.accepts_commands {
                            let _ = events.send(SwarmEvent::Browse { node, id, prefix });
                        } else if let Some(p) = peers.get(&node) {
                            let _ = p.out.send(AscpMessage::Refused {
                                id,
                                refusal: Refusal::NotPermitted {
                                    what: "this node does not accept commands".into(),
                                },
                            });
                        }
                    }
                    AscpMessage::Catalogue { id, refresh } => {
                        // Browse's gate, for Browse's reason: only somebody who may steer a
                        // conversation here has any use for the list of what it could run on.
                        if caps.accepts_commands {
                            let _ = events.send(SwarmEvent::Catalogue { node, id, refresh });
                        } else if let Some(p) = peers.get(&node) {
                            let _ = p.out.send(AscpMessage::Refused {
                                id,
                                refusal: Refusal::NotPermitted {
                                    what: "this node does not accept commands".into(),
                                },
                            });
                        }
                    }
                    AscpMessage::Catalogued { id, catalogue } => {
                        let _ = events.send(SwarmEvent::Catalogued { node, id, catalogue });
                    }
                    AscpMessage::Browsed { id, paths } => {
                        let _ = events.send(SwarmEvent::Browsed { node, id, result: Ok(paths) });
                    }
                    AscpMessage::PtyOpen { id, cwd, cols, rows } => {
                        // Refused here rather than passed up, exactly as a `Command` this node does
                        // not accept is: the host should not have to re-derive a policy it already
                        // declared, and this is the one policy where the answer being wrong means a
                        // stranger's shell. Checked on every open regardless of what the handshake
                        // advertised — a capability is a courtesy to the other end's UI, and the
                        // owner enforces.
                        if caps.shells {
                            let _ = events.send(SwarmEvent::PtyOpen { node, id, cwd, cols, rows });
                        } else if let Some(p) = peers.get(&node) {
                            let _ = p.out.send(AscpMessage::Refused {
                                id,
                                refusal: Refusal::NotPermitted {
                                    what: "this node does not open shells for other machines".into(),
                                },
                            });
                        }
                    }
                    AscpMessage::PtyOpened { id, pty } => {
                        let _ = events.send(SwarmEvent::PtyOpened { node, id, result: Ok(pty) });
                    }
                    AscpMessage::PtyData { pty, data } => {
                        if let Some(data) = decode(&data) {
                            let _ = events.send(SwarmEvent::PtyData { node, pty, data });
                        } else {
                            tracing::warn!(%pty, "pty frame was not base64");
                        }
                    }
                    AscpMessage::PtyResize { pty, cols, rows } => {
                        let _ = events.send(SwarmEvent::PtyResize { node, pty, cols, rows });
                    }
                    AscpMessage::PtyClose { pty } => {
                        let _ = events.send(SwarmEvent::PtyClose { node, pty });
                    }
                    AscpMessage::PtyExit { pty, status } => {
                        let _ = events.send(SwarmEvent::PtyExit { node, pty, status });
                    }
                    AscpMessage::Ack { id, session } => {
                        let _ = events.send(SwarmEvent::Answer {
                            node,
                            id,
                            result: Ok(session),
                        });
                    }
                    AscpMessage::Refused { id, refusal } => {
                        let _ = events.send(SwarmEvent::Answer {
                            node,
                            id,
                            result: Err(refusal),
                        });
                    }
                    AscpMessage::Goodbye { message } => {
                        if peers.remove(&node).is_some() {
                            let will_retry = allowed
                                .get(&node)
                                .and_then(|p| p.addr)
                                .is_some_and(|a| diallers.contains_key(&a));
                            let _ = events.send(SwarmEvent::PeerDown {
                                node,
                                reason: message.unwrap_or_else(|| "said goodbye".into()),
                                will_retry,
                            });
                        }
                    }
                    // Handshake messages after the handshake are noise. Not an error: a peer
                    // reconnecting can legitimately have one in flight.
                    other => tracing::debug!(?other, "unexpected message after handshake"),
                },
            },
            _ = beat.tick() => {
                seq += 1;
                let msg = AscpMessage::Presence {
                    node: info.clone(),
                    capabilities: caps.clone(),
                    seq,
                };
                for p in peers.values() {
                    let _ = p.out.send(msg.clone());
                }
            }
            else => return,
        }
    }
}

/// Apply a delta to the local picture.
fn merge(mine: &mut Vec<AgentSummary>, agents: &[AgentSummary], gone: &[SessionId]) {
    mine.retain(|a| !gone.contains(&a.session));
    for a in agents {
        match mine.iter_mut().find(|m| m.session == a.session) {
            Some(m) => *m = a.clone(),
            None => mine.push(a.clone()),
        }
    }
}

async fn listen(
    sock: TcpListener,
    identity: Arc<Identity>,
    allowed: Allowed,
    info: NodeInfo,
    caps: NodeCapabilities,
    wire: mpsc::UnboundedSender<Wire>,
) {
    loop {
        let Ok((stream, addr)) = sock.accept().await else { continue };
        let (identity, allowed, info, caps, wire) =
            (identity.clone(), allowed.clone(), info.clone(), caps.clone(), wire.clone());
        tokio::spawn(async move {
            let mut stream = stream;
            match accept(&mut stream, &identity, &allowed, &info, &caps).await {
                // No address: where an inbound connection happened to arrive from is not
                // necessarily somewhere it could be dialled back on.
                Ok(peer) => {
                    serve(stream, peer, wire, false, None).await;
                }
                // A machine that proved itself and is not on the list is somebody asking to pair,
                // not an error. It reaches the host, which can offer to add it.
                Err(crate::SwarmError::Stranger(s)) => {
                    let _ = wire.send(Wire::Stranger { node: s.node, dialled: false });
                }
                // At `debug`, not `warn`: on a network anybody can reach, a refused handshake is
                // the system working. A `warn` per port scan is a log nobody reads.
                Err(e) => tracing::debug!(%addr, "swarm refused an inbound connection: {e}"),
            }
        });
    }
}

/// Keep one configured peer connected, retrying for as long as the node runs.
///
/// The retry schedule backs off — 1s, 2s, 4s… capped at the heartbeat or 30s, whichever is longer
/// — and resets the moment a connection actually serves, so a machine that reboots is back within
/// seconds while one that is gone for the weekend is asked about twice a minute. Every failed dial
/// is reported with its attempt number, which is what lets a board say `try 4` instead of drawing
/// a spinner that never resolves.
async fn keep_dialled(
    peer: PeerAddress,
    identity: Arc<Identity>,
    allowed: Allowed,
    info: NodeInfo,
    caps: NodeCapabilities,
    wire: mpsc::UnboundedSender<Wire>,
    events: mpsc::UnboundedSender<SwarmEvent>,
    every: Duration,
) {
    enum Outcome {
        /// Connected, accepted, and served until it ended.
        Served,
        /// Reached the machine, and it has not allowed this one (yet). Carries what the handshake
        /// learned, because that is everything a board needs to draw the row properly and it is
        /// about to be the only thing we ever hear from this machine.
        Refused(Box<(NodeInfo, NodeCapabilities)>),
        /// Reached a machine we have not paired with. What "add a computer" is waiting for.
        Stranger,
        Failed(String),
    }

    let cap = every.max(Duration::from_secs(30));
    let mut attempt: u32 = 0;
    loop {
        let outcome = match TcpStream::connect(&peer.addr).await {
            Ok(mut stream) => match dial(&mut stream, &identity, &allowed, &info, &caps).await {
                Ok(found) => {
                    // The address said one machine and another answered. Worth refusing rather
                    // than accepting quietly: the allow-list would let it through, and "my laptop
                    // is showing the build server's agents" is a confusing way to find out that a
                    // DHCP lease moved.
                    if let Some(want) = &peer.expect
                        && &found.node.id != want
                    {
                        tracing::warn!(
                            addr = %peer.addr,
                            "expected node {} there, found {}",
                            want.short(),
                            found.node.id.short()
                        );
                        Outcome::Failed(format!(
                            "another machine answered ({})",
                            found.node.id.short()
                        ))
                    } else {
                        // Kept before `serve` takes it: whether this connection was ever announced
                        // is only known afterwards, and by then the one thing that would let a
                        // board explain the answer is gone.
                        let seen = (found.node.clone(), found.capabilities.clone());
                        if serve(stream, found, wire.clone(), true, Some(peer.addr.clone())).await {
                            Outcome::Served
                        } else {
                            Outcome::Refused(Box::new(seen))
                        }
                    }
                }
                Err(crate::SwarmError::Stranger(s)) => {
                    let _ = wire.send(Wire::Stranger { node: s.node, dialled: true });
                    Outcome::Stranger
                }
                Err(e) => {
                    tracing::debug!(addr = %peer.addr, "swarm handshake failed: {e}");
                    Outcome::Failed(e.to_string())
                }
            },
            Err(e) => {
                tracing::debug!(addr = %peer.addr, "swarm could not connect: {e}");
                Outcome::Failed(e.to_string())
            }
        };

        let delay = match outcome {
            // It connected and served. Whatever ended it may be a blip, so the next dial is
            // nearly immediate and the backoff starts over.
            Outcome::Served => {
                attempt = 0;
                Duration::from_secs(1)
            }
            // The machine is *there*, so backing off would only slow down the pairing it is
            // probably in the middle of; the flat heartbeat is the pace. That also means the row
            // flips to connected within one heartbeat of somebody saying yes over there, which is
            // what makes the second half of pairing feel like it worked rather than like it needs
            // a restart.
            Outcome::Refused(seen) => {
                attempt = 0;
                let (node, capabilities) = *seen;
                let _ = events.send(SwarmEvent::Unapproved {
                    node,
                    capabilities,
                    addr: peer.addr.clone(),
                });
                every
            }
            Outcome::Stranger => {
                attempt = 0;
                every
            }
            Outcome::Failed(error) => {
                attempt = attempt.saturating_add(1);
                let _ = events.send(SwarmEvent::DialFailed {
                    node: peer.expect.clone(),
                    addr: peer.addr.clone(),
                    attempt,
                    error,
                });
                cap.min(Duration::from_secs(1u64 << (attempt - 1).min(6)))
            }
        };
        tokio::time::sleep(delay).await;
    }
}

/// Pump one authenticated connection until it ends.
///
/// Returns whether the connection was ever announced — for a dialler, whether the far end accepted
/// us. `false` from a dial means "reached it, and it has not allowed this machine", which is a
/// different fact from the address being dead and is paced differently by the caller.
async fn serve(
    stream: TcpStream,
    peer: crate::Peer,
    wire: mpsc::UnboundedSender<Wire>,
    dialled: bool,
    addr: Option<String>,
) -> bool {
    let id = peer.node.id.clone();
    let (mut read, mut write) = stream.into_split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<AscpMessage>();

    // Writing happens in its own task so a slow peer cannot block the loop that feeds every other
    // peer. The channel is unbounded, which is a decision worth naming: bounding it would mean the
    // loop blocking on a stalled socket, which is exactly what this exists to avoid, and the
    // failure mode of unbounded — memory growing for a peer that never reads — is bounded in
    // practice by the connection dropping.
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if write_message(&mut write, &msg).await.is_err() {
                break;
            }
        }
        let _ = write.shutdown().await;
    });

    // The listener has decided — it checked the list before returning — so it is connected now.
    //
    // The dialler has not. It finished *its* half: it verified the peer and found it on its own
    // list, and sent a proof the peer has not yet judged. If the peer does not have *us* on its
    // list, it hangs up a moment later. Announcing a connection at this point produces "linux-box
    // joined the swarm" immediately followed by "lost linux-box" every time pairing is half done,
    // which is exactly when a person most needs to be told something true.
    //
    // So a dialler waits for one message. The listener's node sends a full inventory the instant it
    // accepts, so a real connection announces itself within a round trip, and a refused one never
    // announces at all.
    //
    // The sender is *moved* into the announce rather than cloned, so once the loop has it, it has
    // the only one — and the loop dropping it is what closes the writer, sends the FIN and lets
    // both ends unwind. A serve that kept a clone was a connection nothing could ever close: a
    // dropped peer stayed as an idle socket, and its dialler stayed wedged inside it.
    let mut announce = Some(out_tx);
    if !dialled {
        let _ = wire.send(Wire::Up {
            info: peer.node.clone(),
            capabilities: peer.capabilities.clone(),
            out: announce.take().expect("first take"),
            dialled,
            addr: addr.clone(),
        });
    }

    let reason = loop {
        match read_message(&mut read).await {
            Ok(Some(message)) => {
                if let Some(out) = announce.take() {
                    let _ = wire.send(Wire::Up {
                        info: peer.node.clone(),
                        capabilities: peer.capabilities.clone(),
                        out,
                        dialled,
                        addr: addr.clone(),
                    });
                }
                if wire.send(Wire::Message { node: id.clone(), message }).is_err() {
                    break "the swarm stopped".to_string();
                }
            }
            Ok(None) => break "connection closed".to_string(),
            Err(e) => break format!("{e}"),
        }
    };
    // Nothing to report down if nothing was ever reported up: a dialler the far end refused is a
    // stranger, which was already said, not a peer that went away.
    if announce.is_some() {
        writer.abort();
        return false;
    }
    writer.abort();
    let _ = wire.send(Wire::Down { node: Some(id), reason });
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use neosh_proto::{AgentState, ProjectKey};

    fn summary(id: &str) -> AgentSummary {
        AgentSummary {
            session: SessionId(id.into()),
            project: ProjectKey("git:example.com/x".into()),
            project_name: "x".into(),
            repo_root: None,
            branch: None,
            cwd: "/w/x".into(),
            label: id.into(),
            state: AgentState::Idle,
            message_count: 0,
            model: None,
            instance: None,
            mode: None,
            options: Vec::new(),
            context_tokens: 0,
            context_window: None,
            turn_started_at: None,
            updated_at: 0,
            usage: Default::default(),
            unread: false,
            interrupted: false,
            background: 0,
            decorations: Vec::new(),
        }
    }

    #[test]
    fn a_delta_adds_updates_and_removes() {
        let mut mine = vec![summary("a"), summary("b")];
        let mut updated = summary("a");
        updated.label = "renamed".into();
        merge(&mut mine, &[updated, summary("c")], &[SessionId("b".into())]);

        let labels: Vec<&str> = mine.iter().map(|a| a.label.as_str()).collect();
        assert_eq!(labels, vec!["renamed", "c"]);
    }

    /// A removal and an addition in one delta must not depend on which is applied first.
    #[test]
    fn a_session_removed_and_re_added_in_one_delta_survives() {
        let mut mine = vec![summary("a")];
        merge(&mut mine, &[summary("a")], &[SessionId("a".into())]);
        assert_eq!(mine.len(), 1, "the addition is the newer fact");
    }
}
