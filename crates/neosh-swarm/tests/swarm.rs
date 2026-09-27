//! Two nodes, running for real, seeing each other's agents.
//!
//! The handshake tests prove a connection can be trusted. These prove it is *useful*: that
//! inventory arrives without being asked for, that a peer connecting late is caught up rather than
//! left with an empty board, that a command reaches the machine that owns the agent and comes back
//! answered, and that a stream goes only to whoever subscribed.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use neosh_proto::{
    AgentCommand, AgentState, AgentSummary, ProjectKey, Refusal, SessionId, StreamEvent,
};
use neosh_swarm::allow::{Allowed, Peer as AllowedPeer, Source};
use neosh_swarm::identity::Identity;
use neosh_swarm::node::{PeerAddress, SwarmConfig, SwarmEvent, SwarmHandle, SwarmRequest};
use tokio::sync::mpsc::UnboundedReceiver;

/// Long enough that a wedge fails the test rather than hanging the suite.
const PATIENCE: Duration = Duration::from_secs(10);

fn known(name: &str) -> AllowedPeer {
    AllowedPeer { name: name.into(), alias: None, addr: None, source: Source::Paired }
}

fn summary(id: &str, project: &str) -> AgentSummary {
    AgentSummary {
        session: SessionId(id.into()),
        project: ProjectKey(format!("git:example.com/{project}")),
        project_name: project.into(),
        repo_root: None,
        branch: None,
        cwd: format!("/w/{project}"),
        label: format!("working on {id}"),
        state: AgentState::Running,
        message_count: 3,
        model: Some("claude-opus-5".into()),
        instance: None,
        mode: None,
        options: Vec::new(),
        context_tokens: 0,
        context_window: None,
        turn_started_at: Some(1000),
        updated_at: 1000,
        usage: Default::default(),
        unread: false,
        interrupted: false,
        background: 0,
        decorations: Vec::new(),
    }
}

/// Read events until one matches, or give up. Anything that does not match is discarded, which is
/// what makes these tests indifferent to heartbeats and reconnects arriving in between.
async fn wait_for<T>(
    rx: &mut UnboundedReceiver<SwarmEvent>,
    mut f: impl FnMut(&SwarmEvent) -> Option<T>,
) -> T {
    tokio::time::timeout(PATIENCE, async {
        loop {
            let ev = rx.recv().await.expect("the swarm is still running");
            if let Some(v) = f(&ev) {
                return v;
            }
        }
    })
    .await
    .expect("waited too long for an event that never came")
}

struct Pair {
    a: SwarmHandle,
    a_rx: UnboundedReceiver<SwarmEvent>,
    b: SwarmHandle,
    b_rx: UnboundedReceiver<SwarmEvent>,
}

/// Two nodes that have been introduced, `b` listening and `a` dialling it.
async fn pair(b_accepts_commands: bool) -> Pair {
    pair_with(b_accepts_commands, false).await
}

/// The same, saying whether `b` will open a shell for anybody. See
/// [`neosh_proto::NodeCapabilities::shells`].
async fn pair_with(b_accepts_commands: bool, b_accepts_shells: bool) -> Pair {
    // Bind first to learn the port, then hand the address to the node, which binds it again — the
    // socket is dropped in between. A fixed port would make these tests unable to run in parallel
    // or alongside a real neosh.
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("probe");
    let addr: SocketAddr = probe.local_addr().expect("addr");
    drop(probe);

    let a_id = Identity::ephemeral();
    let b_id = Identity::ephemeral();
    let (aa, ba) = (Allowed::load(None), Allowed::load(None));
    aa.add(b_id.id().clone(), known("b"));
    ba.add(a_id.id().clone(), known("a"));

    let b_cfg = SwarmConfig {
        name: "linux-box".into(),
        listen: Some(addr),
        accepts_commands: b_accepts_commands,
        accepts_approvals: false,
        accepts_shells: b_accepts_shells,
        heartbeat: Duration::from_millis(200),
        ..Default::default()
    };
    let (b, b_rx) = neosh_swarm::node::spawn(Arc::new(b_id), ba, b_cfg, "0.1.0".into());

    let a_cfg = SwarmConfig {
        name: "mac-studio".into(),
        peers: vec![PeerAddress { addr: addr.to_string(), expect: None }],
        heartbeat: Duration::from_millis(200),
        ..Default::default()
    };
    let (a, a_rx) = neosh_swarm::node::spawn(Arc::new(a_id), aa, a_cfg, "0.1.0".into());

    Pair { a, a_rx, b, b_rx }
}

/// The board fills itself in. Nobody polls, and nobody asks.
#[tokio::test]
async fn a_node_learns_what_the_other_is_running() {
    let mut p = pair(true).await;

    let name = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::PeerUp { node, .. } => Some(node.name.clone()),
        _ => None,
    })
    .await;
    assert_eq!(name, "linux-box", "the dialler sees the machine it dialled");

    p.b.send(SwarmRequest::Publish {
        full: true,
        agents: vec![summary("s1", "neosh"), summary("s2", "api")],
        gone: Vec::new(),
    });

    let agents = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::Inventory { agents, .. } if agents.len() == 2 => Some(agents.clone()),
        _ => None,
    })
    .await;
    assert_eq!(agents[0].label, "working on s1");
    assert_eq!(agents[0].state, AgentState::Running);
    assert_eq!(agents[0].cwd, "/w/neosh", "the owner's path, for showing");
    assert_eq!(agents[1].project, ProjectKey("git:example.com/api".into()));
}

/// A node that publishes before anybody is connected must not be invisible afterwards.
///
/// This is the case a naive fan-out gets wrong: the inventory went out to nobody, and the next one
/// is a delta that arrives when something changes — which on a workspace somebody has walked away
/// from is never. The board would be empty and correct-looking.
#[tokio::test]
async fn a_peer_connecting_late_is_caught_up_rather_than_left_empty() {
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("probe");
    let addr: SocketAddr = probe.local_addr().expect("addr");
    drop(probe);

    let a_id = Identity::ephemeral();
    let b_id = Identity::ephemeral();
    let (aa, ba) = (Allowed::load(None), Allowed::load(None));
    aa.add(b_id.id().clone(), known("b"));
    ba.add(a_id.id().clone(), known("a"));

    let (b, _b_rx) = neosh_swarm::node::spawn(
        Arc::new(b_id),
        ba,
        SwarmConfig {
            name: "b".into(),
            listen: Some(addr),
            heartbeat: Duration::from_millis(200),
            ..Default::default()
        },
        "0.1.0".into(),
    );
    // Said into an empty room.
    b.send(SwarmRequest::Publish {
        full: true,
        agents: vec![summary("earlier", "neosh")],
        gone: Vec::new(),
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (_a, mut a_rx) = neosh_swarm::node::spawn(
        Arc::new(a_id),
        aa,
        SwarmConfig {
            name: "a".into(),
            peers: vec![PeerAddress { addr: addr.to_string(), expect: None }],
            heartbeat: Duration::from_millis(200),
            ..Default::default()
        },
        "0.1.0".into(),
    );

    let agents = wait_for(&mut a_rx, |e| match e {
        SwarmEvent::Inventory { full: true, agents, .. } if !agents.is_empty() => {
            Some(agents.clone())
        }
        _ => None,
    })
    .await;
    assert_eq!(agents[0].session, SessionId("earlier".into()));
}

/// Deltas, and the removal that has to reach the other side.
#[tokio::test]
async fn a_conversation_that_ends_stops_being_on_the_board() {
    let mut p = pair(true).await;
    wait_for(&mut p.a_rx, |e| matches!(e, SwarmEvent::PeerUp { .. }).then_some(())).await;

    p.b.send(SwarmRequest::Publish {
        full: true,
        agents: vec![summary("s1", "neosh")],
        gone: Vec::new(),
    });
    wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::Inventory { agents, .. } if agents.len() == 1 => Some(()),
        _ => None,
    })
    .await;

    p.b.send(SwarmRequest::Publish {
        full: false,
        agents: Vec::new(),
        gone: vec![SessionId("s1".into())],
    });
    let gone = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::Inventory { gone, .. } if !gone.is_empty() => Some(gone.clone()),
        _ => None,
    })
    .await;
    assert_eq!(gone, vec![SessionId("s1".into())]);
}

/// A command reaches the owner, and the owner's answer comes back.
#[tokio::test]
async fn a_command_reaches_the_machine_that_owns_the_agent() {
    let mut p = pair(true).await;
    let b_id = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::PeerUp { node, .. } => Some(node.id.clone()),
        _ => None,
    })
    .await;

    p.a.send(SwarmRequest::Command {
        node: b_id,
        id: "c1".into(),
        session: SessionId("s1".into()),
        command: AgentCommand::Send { text: "carry on".into(), images: Vec::new() },
    });

    let (id, text) = wait_for(&mut p.b_rx, |e| match e {
        SwarmEvent::Command { id, command: AgentCommand::Send { text, .. }, .. } => {
            Some((id.clone(), text.clone()))
        }
        _ => None,
    })
    .await;
    assert_eq!(text, "carry on", "the owner is told what to do, and by whom");
    assert_eq!(id, "c1");
}

/// A node that does not accept commands refuses them itself, without troubling its host.
///
/// The refusal has to come from the transport rather than from policy the host re-derives: a host
/// that forgot to check would otherwise turn "this machine is read-only" into a suggestion.
#[tokio::test]
async fn a_read_only_node_refuses_without_asking_its_host() {
    let mut p = pair(false).await;
    let b_id = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::PeerUp { node, capabilities } => {
            assert!(!capabilities.accepts_commands, "and it said so in the handshake");
            Some(node.id.clone())
        }
        _ => None,
    })
    .await;

    p.a.send(SwarmRequest::Command {
        node: b_id,
        id: "c1".into(),
        session: SessionId("s1".into()),
        command: AgentCommand::Send { text: "do a thing".into(), images: Vec::new() },
    });

    // The owning host must never hear about it. Waiting for a couple of heartbeats is the only way
    // to assert an absence, and a heartbeat here is 200ms.
    let heard = tokio::time::timeout(Duration::from_millis(700), async {
        loop {
            match p.b_rx.recv().await {
                Some(SwarmEvent::Command { .. }) => return true,
                Some(_) => continue,
                None => return false,
            }
        }
    })
    .await;
    assert!(heard.is_err(), "a read-only node's host is not asked to enforce it");
}

/// A directory listing reaches the owner and comes back under the id it was asked with.
#[tokio::test]
async fn browsing_a_peer_answers_with_that_machines_directories() {
    let mut p = pair(true).await;
    let b_id = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::PeerUp { node, .. } => Some(node.id.clone()),
        _ => None,
    })
    .await;

    p.a.send(SwarmRequest::Browse { node: b_id.clone(), id: "b1".into(), prefix: "/src/".into() });

    let (id, prefix) = wait_for(&mut p.b_rx, |e| match e {
        SwarmEvent::Browse { id, prefix, .. } => Some((id.clone(), prefix.clone())),
        _ => None,
    })
    .await;
    assert_eq!((id.as_str(), prefix.as_str()), ("b1", "/src/"), "asked, verbatim");

    let a_id = wait_for(&mut p.b_rx, |e| match e {
        SwarmEvent::PeerUp { node, .. } => Some(node.id.clone()),
        _ => None,
    })
    .await;
    p.b.send(SwarmRequest::Browsed {
        node: a_id,
        id: "b1".into(),
        result: Ok(vec!["/src/neosh/".into()]),
    });

    let answer = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::Browsed { id, result, .. } if id == "b1" => Some(result.clone()),
        _ => None,
    })
    .await;
    assert_eq!(answer.expect("answered"), vec!["/src/neosh/".to_string()]);
}

/// A read-only node is not asked to list its directories either.
///
/// Browsing follows `accepts_commands` rather than a permission of its own — a node that will start
/// an agent at any path you name has already given away more than the names of its directories —
/// which means the gate has to be the same gate, applied in the same place, and not a second one
/// somebody remembered to write.
#[tokio::test]
async fn a_read_only_node_will_not_be_browsed() {
    let mut p = pair(false).await;
    let b_id = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::PeerUp { node, .. } => Some(node.id.clone()),
        _ => None,
    })
    .await;

    p.a.send(SwarmRequest::Browse { node: b_id, id: "b1".into(), prefix: "/".into() });

    let heard = tokio::time::timeout(Duration::from_millis(700), async {
        loop {
            match p.b_rx.recv().await {
                Some(SwarmEvent::Browse { .. }) => return true,
                Some(_) => continue,
                None => return false,
            }
        }
    })
    .await;
    assert!(heard.is_err(), "its host is not asked to enforce what it already declared");
}

/// Streams go to subscribers and to nobody else.
#[tokio::test]
async fn a_stream_arrives_only_after_subscribing() {
    let mut p = pair(true).await;
    let b_id = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::PeerUp { node, .. } => Some(node.id.clone()),
        _ => None,
    })
    .await;

    // Before subscribing: silence.
    p.b.send(SwarmRequest::Stream {
        session: SessionId("s1".into()),
        event: StreamEvent::Token { turn: "t1".into(), text: "unheard".into() },
    });
    let early = tokio::time::timeout(Duration::from_millis(400), async {
        loop {
            match p.a_rx.recv().await {
                Some(SwarmEvent::Stream { .. }) => return true,
                Some(_) => continue,
                None => return false,
            }
        }
    })
    .await;
    assert!(early.is_err(), "nothing streams to a peer that did not ask");

    p.a.send(SwarmRequest::Subscribe { node: b_id, session: SessionId("s1".into()) });
    wait_for(&mut p.b_rx, |e| matches!(e, SwarmEvent::Subscribed { .. }).then_some(())).await;

    p.b.send(SwarmRequest::Stream {
        session: SessionId("s1".into()),
        event: StreamEvent::Token { turn: "t1".into(), text: "heard".into() },
    });
    let text = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::Stream { event: StreamEvent::Token { text, .. }, .. } => Some(text.clone()),
        _ => None,
    })
    .await;
    assert_eq!(text, "heard");
}

/// Answers find their way home, carrying whichever id they were asked under.
#[tokio::test]
async fn an_answer_comes_back_under_the_id_it_was_asked_with() {
    let mut p = pair(true).await;
    let b_id = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::PeerUp { node, .. } => Some(node.id.clone()),
        _ => None,
    })
    .await;

    p.a.send(SwarmRequest::Command {
        node: b_id,
        id: "c9".into(),
        session: SessionId("nope".into()),
        command: AgentCommand::Interrupt,
    });
    let (from, id) = wait_for(&mut p.b_rx, |e| match e {
        SwarmEvent::Command { node, id, .. } => Some((node.clone(), id.clone())),
        _ => None,
    })
    .await;

    p.b.send(SwarmRequest::Answer {
        node: from,
        id: id.clone(),
        result: Err(Refusal::NoSuchAgent),
    });
    // The dialler's host sees the refusal as an ordinary inbound message; the node layer does not
    // interpret it, which is why this test asserts on the raw arrival rather than on a callback.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(id, "c9");
}

/// Two machines that both dial each other settle on one connection and stay there.
///
/// The bug this pins looked like a flapping network and was arithmetic. Both ends had two working
/// connections, both dropped "the older one", and each judged that independently — so each kept the
/// socket the other had just abandoned, saw it close, reconnected, and did it again, several times
/// a second. It only appears when both nodes list each other, which is the normal way to configure
/// a swarm and not something either handshake test does.
#[tokio::test]
async fn two_nodes_that_dial_each_other_settle_on_one_connection() {
    let mut ports = Vec::new();
    for _ in 0..2 {
        let s = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("probe");
        ports.push(s.local_addr().expect("addr"));
    }
    let (a_addr, b_addr) = (ports[0], ports[1]);
    drop(ports);

    let a_id = Identity::ephemeral();
    let b_id = Identity::ephemeral();
    let (aa, ba) = (Allowed::load(None), Allowed::load(None));
    aa.add(b_id.id().clone(), known("b"));
    ba.add(a_id.id().clone(), known("a"));

    // Each listens *and* dials the other, which is what a real pair of machines does.
    let mk = |id: Identity, allowed: Allowed, listen, peer: SocketAddr, name: &str| {
        neosh_swarm::node::spawn(
            Arc::new(id),
            allowed,
            SwarmConfig {
                name: name.into(),
                listen: Some(listen),
                peers: vec![PeerAddress { addr: peer.to_string(), expect: None }],
                heartbeat: Duration::from_millis(150),
                ..Default::default()
            },
            "0.1.0".into(),
        )
    };
    let (a, mut a_rx) = mk(a_id, aa, a_addr, b_addr, "a");
    let (_b, _b_rx) = mk(b_id, ba, b_addr, a_addr, "b");

    wait_for(&mut a_rx, |e| matches!(e, SwarmEvent::PeerUp { .. }).then_some(())).await;

    // Now watch for a while. A flap shows up as `PeerDown`; before the tie-break there were several
    // per second, so a couple of seconds is far more than enough to catch it.
    let flapped = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match a_rx.recv().await {
                Some(SwarmEvent::PeerDown { reason, .. }) => return reason,
                Some(_) => continue,
                None => return "the swarm stopped".to_string(),
            }
        }
    })
    .await;
    assert!(flapped.is_err(), "the connection dropped: {flapped:?}");

    // And the surviving connection is a working one, not a half-closed socket nobody reads.
    a.send(SwarmRequest::Publish {
        full: true,
        agents: vec![summary("s1", "neosh")],
        gone: Vec::new(),
    });
    let mut b_rx = _b_rx;
    let seen = wait_for(&mut b_rx, |e| match e {
        SwarmEvent::Inventory { agents, .. } if !agents.is_empty() => Some(agents.len()),
        _ => None,
    })
    .await;
    assert_eq!(seen, 1, "and inventory still flows over whichever half survived");
}

/// History first, then everything as it happens.
///
/// The order is the point: a watcher that received only deltas would show an empty conversation
/// until the next token, which for an idle agent is never. Subscribing to something nobody is
/// typing at has to still show you what it said.
#[tokio::test]
async fn a_subscriber_is_given_the_conversation_before_the_next_token() {
    let mut p = pair(true).await;
    let b_id = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::PeerUp { node, .. } => Some(node.id.clone()),
        _ => None,
    })
    .await;

    let s1 = SessionId("s1".into());
    p.a.send(SwarmRequest::Subscribe { node: b_id, session: s1.clone() });
    wait_for(&mut p.b_rx, |e| matches!(e, SwarmEvent::Subscribed { .. }).then_some(())).await;

    // What a host does on being subscribed to: say what has been said.
    p.b.send(SwarmRequest::Stream {
        session: s1.clone(),
        event: StreamEvent::History { messages: Vec::new(), live: None, interrupted: false },
    });
    p.b.send(SwarmRequest::Stream {
        session: s1.clone(),
        event: StreamEvent::TurnStarted { turn: "t1".into() },
    });
    p.b.send(SwarmRequest::Stream {
        session: s1.clone(),
        event: StreamEvent::Token { turn: "t1".into(), text: "hello".into() },
    });

    let mut order = Vec::new();
    tokio::time::timeout(PATIENCE, async {
        while order.len() < 3 {
            if let Some(SwarmEvent::Stream { event, .. }) = p.a_rx.recv().await {
                order.push(match event {
                    StreamEvent::History { .. } => "history",
                    StreamEvent::TurnStarted { .. } => "started",
                    StreamEvent::Token { .. } => "token",
                    _ => "other",
                });
            }
        }
    })
    .await
    .expect("all three arrived");
    assert_eq!(order, vec!["history", "started", "token"], "and in the order they were sent");
}

/// Unsubscribing stops it. Otherwise "close the conversation" leaves a machine sending tokens to
/// somebody who walked away, which is the cost this whole subscription mechanism exists to avoid.
#[tokio::test]
async fn unsubscribing_stops_the_stream() {
    let mut p = pair(true).await;
    let b_id = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::PeerUp { node, .. } => Some(node.id.clone()),
        _ => None,
    })
    .await;
    let s1 = SessionId("s1".into());

    p.a.send(SwarmRequest::Subscribe { node: b_id.clone(), session: s1.clone() });
    wait_for(&mut p.b_rx, |e| matches!(e, SwarmEvent::Subscribed { .. }).then_some(())).await;
    p.b.send(SwarmRequest::Stream {
        session: s1.clone(),
        event: StreamEvent::Token { turn: "t".into(), text: "one".into() },
    });
    wait_for(&mut p.a_rx, |e| matches!(e, SwarmEvent::Stream { .. }).then_some(())).await;

    p.a.send(SwarmRequest::Unsubscribe { node: b_id, session: s1.clone() });
    wait_for(&mut p.b_rx, |e| matches!(e, SwarmEvent::Unsubscribed { .. }).then_some(())).await;
    p.b.send(SwarmRequest::Stream {
        session: s1,
        event: StreamEvent::Token { turn: "t".into(), text: "two".into() },
    });

    let after = tokio::time::timeout(Duration::from_millis(600), async {
        loop {
            match p.a_rx.recv().await {
                Some(SwarmEvent::Stream { .. }) => return true,
                Some(_) => continue,
                None => return false,
            }
        }
    })
    .await;
    assert!(after.is_err(), "nothing arrives after unsubscribing");
}

/// Pairing, from both ends, without a restart.
///
/// The flow a person actually goes through: you type an address on one machine, it tells you what
/// is there, and the machine at the other end is told somebody is asking. Neither side had to be
/// configured in advance and neither is restarted.
#[tokio::test]
async fn two_strangers_can_be_introduced_while_they_are_running() {
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("probe");
    let addr: SocketAddr = probe.local_addr().expect("addr");
    drop(probe);

    let a_id = Identity::ephemeral();
    let b_id = Identity::ephemeral();
    let (a_key, b_key) = (a_id.id().clone(), b_id.id().clone());
    // Nobody knows anybody.
    let (aa, ba) = (Allowed::load(None), Allowed::load(None));

    let (b, mut b_rx) = neosh_swarm::node::spawn(
        Arc::new(b_id),
        ba,
        SwarmConfig {
            name: "linux-box".into(),
            listen: Some(addr),
            heartbeat: Duration::from_millis(150),
            ..Default::default()
        },
        "0.1.0".into(),
    );
    let (a, mut a_rx) = neosh_swarm::node::spawn(
        Arc::new(a_id),
        aa,
        SwarmConfig {
            name: "mac-studio".into(),
            peers: vec![PeerAddress { addr: addr.to_string(), expect: None }],
            heartbeat: Duration::from_millis(150),
            ..Default::default()
        },
        "0.1.0".into(),
    );

    // A dialled an address it knows nothing about, and is told what is there — by name, with an id
    // that was *proven* rather than claimed. That is what a confirmation dialog can show.
    let found = wait_for(&mut a_rx, |e| match e {
        SwarmEvent::Stranger { node, dialled: true } => Some(node.clone()),
        _ => None,
    })
    .await;
    assert_eq!(found.name, "linux-box");
    assert_eq!(found.id, b_key, "and it is the machine that is actually there");

    // B, meanwhile, is told somebody is asking.
    let asking = wait_for(&mut b_rx, |e| match e {
        SwarmEvent::Stranger { node, dialled: false } => Some(node.clone()),
        _ => None,
    })
    .await;
    assert_eq!(asking.id, a_key);
    assert_eq!(asking.name, "mac-studio");

    // Both people say yes. No restart, no config file.
    a.send(SwarmRequest::Pair {
        id: b_key.clone(),
        name: "linux-box".into(),
        addr: Some(addr.to_string()),
    });
    b.send(SwarmRequest::Pair { id: a_key, name: "mac-studio".into(), addr: None });

    let up = wait_for(&mut a_rx, |e| match e {
        SwarmEvent::PeerUp { node, .. } => Some(node.name.clone()),
        _ => None,
    })
    .await;
    assert_eq!(up, "linux-box", "and the next reconnect simply works");

    // And it is a real connection, not merely an authorised one.
    b.send(SwarmRequest::Publish {
        full: true,
        agents: vec![summary("s1", "neosh")],
        gone: Vec::new(),
    });
    let seen = wait_for(&mut a_rx, |e| match e {
        SwarmEvent::Inventory { agents, .. } if !agents.is_empty() => Some(agents.len()),
        _ => None,
    })
    .await;
    assert_eq!(seen, 1);
}

/// Half a pairing says which half is missing, rather than dialling in silence for ever.
///
/// This is the state a person is actually in for the minute between adding a computer here and
/// walking over to allow this one there — and it used to be indistinguishable from a wrong
/// address. The dialler completes the handshake, sends its proof last, and the far end simply
/// stops talking to a machine it has not been told about; reported as a dial *failure*, that
/// became `connecting…` with no reason attached, for as long as anybody was willing to watch.
#[tokio::test]
async fn a_machine_that_has_not_allowed_us_back_says_so_rather_than_dialling_in_silence() {
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("probe");
    let addr: SocketAddr = probe.local_addr().expect("addr");
    drop(probe);

    let a_id = Identity::ephemeral();
    let b_id = Identity::ephemeral();
    let (a_key, b_key) = (a_id.id().clone(), b_id.id().clone());

    // A has added B — the first half of pairing, done. B has never heard of A.
    let (aa, ba) = (Allowed::load(None), Allowed::load(None));
    aa.add(b_key.clone(), known("the address you typed"));

    let (b, mut b_rx) = neosh_swarm::node::spawn(
        Arc::new(b_id),
        ba,
        SwarmConfig {
            name: "linux-box".into(),
            listen: Some(addr),
            heartbeat: Duration::from_millis(150),
            ..Default::default()
        },
        "0.1.0".into(),
    );
    let (_a, mut a_rx) = neosh_swarm::node::spawn(
        Arc::new(a_id),
        aa,
        SwarmConfig {
            name: "mac-studio".into(),
            peers: vec![PeerAddress { addr: addr.to_string(), expect: Some(b_key.clone()) }],
            heartbeat: Duration::from_millis(150),
            ..Default::default()
        },
        "0.1.0".into(),
    );

    // Not `DialFailed`: nothing failed. The machine is there, it is the right machine, and it has
    // said its name — all of which the row can now show while it waits on a person.
    let waiting = wait_for(&mut a_rx, |e| match e {
        SwarmEvent::Unapproved { node, .. } => Some(node.clone()),
        _ => None,
    })
    .await;
    assert_eq!(waiting.id, b_key, "and it is the machine we meant to reach");
    assert_eq!(
        waiting.name, "linux-box",
        "the handshake got far enough to learn what it is really called, so the row stops \
         repeating the address somebody typed"
    );

    // Over there, the other half of the same moment: somebody is asking.
    let asking = wait_for(&mut b_rx, |e| match e {
        SwarmEvent::Stranger { node, dialled: false } => Some(node.id.clone()),
        _ => None,
    })
    .await;
    assert_eq!(asking, a_key);

    // The person walks over and presses the key. Nothing happens on A — the dialler is already
    // retrying at the heartbeat, which is what makes the row turn over on its own.
    b.send(SwarmRequest::Pair { id: a_key, name: "mac-studio".into(), addr: None });
    let up = wait_for(&mut a_rx, |e| match e {
        SwarmEvent::PeerUp { node, .. } => Some(node.name.clone()),
        _ => None,
    })
    .await;
    assert_eq!(up, "linux-box");
}

/// Unpairing stops the reconnecting, which is the half that is easy to forget.
///
/// A node that is unpaired but still dialled would knock on the door for ever and be turned away
/// for ever — quietly, in a log nobody reads.
#[tokio::test]
async fn unpairing_disconnects_and_stops_dialling() {
    let mut p = pair(true).await;
    let b_id = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::PeerUp { node, .. } => Some(node.id.clone()),
        _ => None,
    })
    .await;

    p.a.send(SwarmRequest::Unpair { id: b_id.clone() });
    let reason = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::PeerDown { node, reason, .. } if node == &b_id => Some(reason.clone()),
        _ => None,
    })
    .await;
    assert_eq!(reason, "unpaired");

    // It does not come back a moment later.
    let returned = tokio::time::timeout(Duration::from_millis(800), async {
        loop {
            match p.a_rx.recv().await {
                Some(SwarmEvent::PeerUp { .. }) => return true,
                Some(_) => continue,
                None => return false,
            }
        }
    })
    .await;
    assert!(returned.is_err(), "an unpaired machine stays unpaired");
}

/// Disconnecting holds the door shut in both directions — the dialler stops, and a peer that
/// dials *us* is turned away — and `Redial` is the one thing that opens it again.
#[tokio::test]
async fn a_disconnect_holds_until_a_redial_lifts_it() {
    let mut p = pair(true).await;
    let b_id = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::PeerUp { node, .. } => Some(node.id.clone()),
        _ => None,
    })
    .await;

    p.a.send(SwarmRequest::Disconnect { id: b_id.clone() });
    let (reason, will_retry) = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::PeerDown { node, reason, will_retry } if node == &b_id => {
            Some((reason.clone(), *will_retry))
        }
        _ => None,
    })
    .await;
    assert_eq!(reason, "disconnected by you");
    assert!(!will_retry, "nothing is dialling a machine you told to stop");

    // `b` keeps dialling nobody here — `a` dialled it — but even if it did, the pause holds. What
    // must not happen is `a`'s own dialler bringing it straight back.
    let returned = tokio::time::timeout(Duration::from_millis(800), async {
        loop {
            match p.a_rx.recv().await {
                Some(SwarmEvent::PeerUp { .. }) => return true,
                Some(_) => continue,
                None => return false,
            }
        }
    })
    .await;
    assert!(returned.is_err(), "disconnected means disconnected, not ten seconds of quiet");

    // The pairing survived, so reconnecting is one request, not a ceremony.
    p.a.send(SwarmRequest::Redial { id: b_id.clone() });
    let name = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::PeerUp { node, .. } if node.id == b_id => Some(node.name.clone()),
        _ => None,
    })
    .await;
    assert_eq!(name, "linux-box");
}

/// A dial that fails is reported, with a count that moves — which is what lets a board say
/// `connecting — try 3` instead of pretending nothing is happening.
#[tokio::test]
async fn failed_dials_are_reported_with_a_moving_attempt_count() {
    // An address nothing listens on: bind to learn a free port, then close it.
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("probe");
    let addr = probe.local_addr().expect("addr");
    drop(probe);

    let a_id = Identity::ephemeral();
    let expect = Identity::ephemeral().id().clone();
    let cfg = SwarmConfig {
        name: "mac-studio".into(),
        peers: vec![PeerAddress { addr: addr.to_string(), expect: Some(expect.clone()) }],
        heartbeat: Duration::from_millis(200),
        ..Default::default()
    };
    let (_a, mut a_rx) = neosh_swarm::node::spawn(Arc::new(a_id), Allowed::load(None), cfg, "0.1.0".into());

    let first = wait_for(&mut a_rx, |e| match e {
        SwarmEvent::DialFailed { node, attempt, .. } if node.as_ref() == Some(&expect) => {
            Some(*attempt)
        }
        _ => None,
    })
    .await;
    assert_eq!(first, 1);

    let second = wait_for(&mut a_rx, |e| match e {
        SwarmEvent::DialFailed { attempt, .. } if *attempt > 1 => Some(*attempt),
        _ => None,
    })
    .await;
    assert_eq!(second, 2, "the count is per run of failures, not per event");
}

/// The listener says what it managed, which is what a board's "this computer" row shows.
#[tokio::test]
async fn the_listener_reports_the_address_it_bound() {
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("probe");
    let addr = probe.local_addr().expect("addr");
    drop(probe);

    let id = Identity::ephemeral();
    let cfg = SwarmConfig {
        name: "linux-box".into(),
        listen: Some(addr),
        heartbeat: Duration::from_millis(200),
        ..Default::default()
    };
    let (_h, mut rx) = neosh_swarm::node::spawn(Arc::new(id), Allowed::load(None), cfg, "0.1.0".into());

    let bound = wait_for(&mut rx, |e| match e {
        SwarmEvent::Listening { addr } => Some(*addr),
        _ => None,
    })
    .await;
    assert_eq!(bound, addr);
}

/// A shell is never *sent* to a machine that has not said it opens them.
///
/// Two things at once, and the second is the one that would hurt. An unknown message tag fails the
/// frame and takes the connection with it, so a `PtyOpen` aimed at a node built before this existed
/// would knock every conversation on that link off the board rather than come back empty — which is
/// why this is checked before sending rather than answered by the far end.
///
/// And it is *answered*, locally, exactly once. A request that is simply dropped is indistinguishable
/// from a slow network, and what is waiting on it is a pane somebody just opened.
#[tokio::test]
async fn a_shell_is_not_offered_to_a_machine_that_does_not_open_them() {
    let mut p = pair(true).await;
    let peer = wait_for_peer(&mut p.a_rx).await;
    p.a.send(SwarmRequest::PtyOpen {
        node: peer.clone(),
        id: "s1".into(),
        cwd: None,
        cols: 80,
        rows: 24,
    });

    let refusal = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::PtyOpened { id, result, .. } if id == "s1" => Some(result.clone()),
        _ => None,
    })
    .await;
    let Err(Refusal::NotPermitted { what }) = refusal else {
        panic!("a machine that does not open shells refuses by name: {refusal:?}");
    };
    assert!(
        what.contains("linux-box") && what.contains("accepts_shells"),
        "the sentence names the machine and the setting to change: {what}"
    );
}

/// The whole of a remote terminal, at the layer that carries it.
///
/// Bytes both ways over one handle, because a pty is two pipes and the connection is symmetric —
/// the same message tag carries a keystroke going one way and a screenful going the other. Resize
/// only ever goes towards the owner: the side that is *drawing* is the side that knows how wide it
/// is, which is the arrangement that makes a full-screen program work.
#[tokio::test]
async fn a_shell_carries_bytes_both_ways_over_one_handle() {
    let mut p = pair_with(true, true).await;
    let peer = wait_for_peer(&mut p.a_rx).await;
    let asker = wait_for_peer(&mut p.b_rx).await;

    p.a.send(SwarmRequest::PtyOpen {
        node: peer.clone(),
        id: "s1".into(),
        cwd: Some("/w/neosh".into()),
        cols: 100,
        rows: 30,
    });
    let asked = wait_for(&mut p.b_rx, |e| match e {
        SwarmEvent::PtyOpen { id, cwd, cols, rows, .. } if id == "s1" => {
            Some((cwd.clone(), *cols, *rows))
        }
        _ => None,
    })
    .await;
    assert_eq!(asked, (Some("/w/neosh".into()), 100, 30), "asked for, as asked for");

    p.b.send(SwarmRequest::PtyOpened {
        node: asker.clone(),
        id: "s1".into(),
        result: Ok("t1".into()),
    });
    let handle = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::PtyOpened { id, result: Ok(pty), .. } if id == "s1" => Some(pty.clone()),
        _ => None,
    })
    .await;
    assert_eq!(handle, "t1", "the handle is the owner's to mint");

    // Towards the owner: a keystroke.
    p.a.send(SwarmRequest::PtyData {
        node: peer.clone(),
        pty: handle.clone(),
        data: b"ls\r".to_vec(),
    });
    let typed = wait_for(&mut p.b_rx, |e| match e {
        SwarmEvent::PtyData { pty, data, .. } if pty == "t1" => Some(data.clone()),
        _ => None,
    })
    .await;
    assert_eq!(typed, b"ls\r", "keystrokes arrive as the bytes they were");

    // And back: whatever the program wrote, which is very often not valid UTF-8 at all. This one
    // is a lone 0x9b and a split sequence — the case a JSON string cannot hold and the reason the
    // payload is base64 rather than escaped.
    let printed = vec![0x1b, b'[', b'2', b'J', 0x9b, 0xf0, 0x9f];
    p.b.send(SwarmRequest::PtyData {
        node: asker.clone(),
        pty: handle.clone(),
        data: printed.clone(),
    });
    let drawn = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::PtyData { pty, data, .. } if pty == "t1" => Some(data.clone()),
        _ => None,
    })
    .await;
    assert_eq!(drawn, printed, "arbitrary bytes survive the round trip");

    p.a.send(SwarmRequest::PtyResize {
        node: peer.clone(),
        pty: handle.clone(),
        cols: 120,
        rows: 40,
    });
    let sized = wait_for(&mut p.b_rx, |e| match e {
        SwarmEvent::PtyResize { pty, cols, rows, .. } if pty == "t1" => Some((*cols, *rows)),
        _ => None,
    })
    .await;
    assert_eq!(sized, (120, 40), "the drawing side is what decides how big it is");

    p.b.send(SwarmRequest::PtyExit {
        node: asker.clone(),
        pty: handle.clone(),
        status: Some(127),
    });
    let gone = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::PtyExit { pty, status, .. } if pty == "t1" => Some(*status),
        _ => None,
    })
    .await;
    assert_eq!(gone, Some(127), "why the prompt went away, which is the useful half");
}

/// The id of whoever is on the other end, once the link is up.
async fn wait_for_peer(rx: &mut UnboundedReceiver<SwarmEvent>) -> neosh_proto::NodeId {
    wait_for(rx, |e| match e {
        SwarmEvent::PeerUp { node, .. } => Some(node.id.clone()),
        _ => None,
    })
    .await
}
