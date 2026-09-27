//! Canonical encodings of every ASCP message, written to disk for other implementations.
//!
//! A specification prose can be read two ways; a byte string cannot. These are what an
//! implementation in another language checks itself against — and because they are *generated from
//! the reference implementation* rather than typed out beside it, a change to a message type that
//! nobody updated the spec for shows up here as a diff.
//!
//! Run with `NEOSH_WRITE_VECTORS=1` to regenerate; otherwise this asserts the committed files still
//! match, which is what makes it a drift check rather than a script.

use std::path::PathBuf;

use neosh_proto::*;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/ascp/vectors")
}

/// Pretty-printed with sorted keys is not what goes on the wire — the wire is compact — but it is
/// what a person diffs, and every JSON parser produces the same value from both.
///
/// Sorted *by this function*, rather than by leaving it to `serde_json::Value`. Its object is a
/// `BTreeMap` — which sorts — only while the `preserve_order` feature is off, and `deno_core` turns
/// that feature on for everything in the graph. So which order these vectors came out in depended on
/// whether the crate that pulls in V8 happened to be in the build: `-p neosh-proto` sorted them and
/// agreed with the committed files, `--workspace` emitted declaration order and called every one of
/// them out of date. A canonical encoding that depends on who else is being compiled is not one.
fn canonical(v: &impl serde::Serialize) -> String {
    let value: serde_json::Value = serde_json::to_value(v).expect("serialises");
    serde_json::to_string_pretty(&sorted(value)).expect("prints") + "\n"
}

/// The same value with every object's keys in name order, whichever map is underneath.
///
/// Rebuilding in sorted order is what makes it hold both ways: an `IndexMap` keeps the order things
/// were inserted in, so inserting them sorted is sorted, and a `BTreeMap` was going to sort them
/// regardless.
fn sorted(v: serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(map) => {
            let mut pairs: Vec<_> = map.into_iter().collect();
            pairs.sort_by(|(a, _), (b, _)| a.cmp(b));
            serde_json::Value::Object(pairs.into_iter().map(|(k, v)| (k, sorted(v))).collect())
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(sorted).collect())
        }
        scalar => scalar,
    }
}

fn check(name: &str, body: String) {
    let path = dir().join(name);
    if std::env::var_os("NEOSH_WRITE_VECTORS").is_some() {
        std::fs::create_dir_all(dir()).expect("vector dir");
        std::fs::write(&path, &body).expect("write vector");
        return;
    }
    let found = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("{}: {e}. Run with NEOSH_WRITE_VECTORS=1 to create it.", path.display())
    });
    assert_eq!(found, body, "{} is out of date — regenerate it", path.display());
}

fn node(id: &str, name: &str) -> NodeInfo {
    NodeInfo {
        id: NodeId(id.into()),
        name: name.into(),
        os: "linux".into(),
        version: "0.1.0".into(),
    }
}

fn caps() -> NodeCapabilities {
    NodeCapabilities {
        accepts_commands: true,
        accepts_approvals: false,
        shells: false,
        streams: true,
        browse: true,
        rich_stream: false,
        catalogue: false,
        live: false,
        projects: vec![RemoteProject {
            key: ProjectKey("git:github.com/neoswarm/neosh".into()),
            name: "neosh".into(),
            cwd: "/home/me/src/neosh".into(),
            repo_root: Some("/home/me/src/neosh".into()),
            branch: Some("main".into()),
            head: None,
            active: true,
            sessions: 2,
            running: 1,
            decorations: Vec::new(),
        }],
    }
}

fn agent() -> AgentSummary {
    AgentSummary {
        session: SessionId("01J8XPQ4".into()),
        project: ProjectKey("git:github.com/neoswarm/neosh".into()),
        project_name: "neosh".into(),
        cwd: "/home/me/src/neosh".into(),
        repo_root: Some("/home/me/src/neosh".into()),
        branch: Some("main".into()),
        label: "make the parser faster".into(),
        state: AgentState::Running,
        message_count: 12,
        model: Some("claude-opus-5".into()),
        instance: None,
        mode: None,
        options: Vec::new(),
        context_tokens: 0,
        context_window: None,
        turn_started_at: Some(1_770_000_000),
        updated_at: 1_770_000_042,
        usage: Usage::default(),
        unread: false,
        interrupted: false,
        background: 0,
        decorations: Vec::new(),
    }
}

/// One file per message family, so an implementer can work through them in order.
#[test]
fn every_message_has_a_canonical_encoding() {
    let a = "1f8b7a3c";
    let b = "9e2d4401";

    check(
        "01-hello.json",
        canonical(&AscpMessage::Hello {
            version: ASCP_VERSION,
            node: node(a, "mac-studio"),
            capabilities: caps(),
            nonce: "aa".repeat(32),
        }),
    );
    check(
        "02-welcome.json",
        canonical(&AscpMessage::Welcome {
            version: ASCP_VERSION,
            node: node(b, "linux-box"),
            capabilities: caps(),
            signature: "cc".repeat(64),
            nonce: "bb".repeat(32),
        }),
    );
    check("03-proof.json", canonical(&AscpMessage::Proof { signature: "dd".repeat(64) }));
    check(
        "04-presence.json",
        canonical(&AscpMessage::Presence { node: node(b, "linux-box"), capabilities: caps(), seq: 7 }),
    );
    check(
        "05-inventory-full.json",
        canonical(&AscpMessage::Inventory {
            seq: 8,
            full: true,
            agents: vec![agent()],
            gone: Vec::new(),
        }),
    );
    check(
        "06-inventory-delta.json",
        canonical(&AscpMessage::Inventory {
            seq: 9,
            full: false,
            agents: Vec::new(),
            gone: vec![SessionId("01J8XPQ4".into())],
        }),
    );
    check(
        "07-subscribe.json",
        canonical(&AscpMessage::Subscribe { session: SessionId("01J8XPQ4".into()) }),
    );
    check(
        "08-stream-token.json",
        canonical(&AscpMessage::Stream {
            session: SessionId("01J8XPQ4".into()),
            event: StreamEvent::Token { turn: "t1".into(), text: "the parser ".into() },
        }),
    );
    check(
        "09-command-send.json",
        canonical(&AscpMessage::Command {
            id: "c1".into(),
            session: SessionId("01J8XPQ4".into()),
            command: AgentCommand::Send { text: "try the other approach".into(), images: Vec::new() },
        }),
    );
    check(
        "10-ack.json",
        canonical(&AscpMessage::Ack { id: "c1".into(), session: None }),
    );
    check(
        "11-refused.json",
        canonical(&AscpMessage::Refused {
            id: "c2".into(),
            refusal: Refusal::NotPermitted { what: "this node does not accept that".into() },
        }),
    );
    check("12-goodbye.json", canonical(&AscpMessage::Goodbye { message: None }));
    check(
        "13-browse.json",
        canonical(&AscpMessage::Browse { id: "b1".into(), prefix: "/home/me/src/".into() }),
    );
    check(
        "14-browsed.json",
        canonical(&AscpMessage::Browsed {
            id: "b1".into(),
            // Each one as the asker would have typed it, trailing separator and all — which is the
            // part an implementation in another language gets wrong, because a `read_dir` gives you
            // bare names and a field needs a path.
            paths: vec!["/home/me/src/neosh/".into(), "/home/me/src/dotfiles/".into()],
        }),
    );
}

/// The exact bytes a peer has to sign.
///
/// The single most implementable-wrong part of ASCP: a challenge that differs by one newline
/// verifies on neither side, and the symptom is "the handshake fails" with nothing to point at.
#[test]
fn the_challenge_string_is_exactly_this() {
    let dialler = NodeId("1f8b7a3c".into());
    let listener = NodeId("9e2d4401".into());
    let nonce = "aa".repeat(32);
    let built = format!("ascp/{ASCP_VERSION}\n{nonce}\n{dialler}\n{listener}");

    check("challenge.txt", format!("{built}\n"));
    // Spelled out again here so the test fails if the format string is edited, rather than
    // silently agreeing with itself.
    assert_eq!(
        built,
        format!(
            "ascp/0\n\
             aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
             1f8b7a3c\n\
             9e2d4401"
        )
    );
}

/// Every spelling of one repository, and what each becomes.
#[test]
fn project_key_normalisation_is_pinned() {
    let cases = [
        "git@github.com:neoswarm/neosh.git",
        "git@github.com:neoswarm/neosh",
        "https://github.com/neoswarm/neosh.git",
        "https://github.com/neoswarm/neosh",
        "https://github.com/neoswarm/neosh/",
        "ssh://git@github.com/neoswarm/neosh.git",
        "ssh://git@github.com:22/neoswarm/neosh.git",
        "https://someone:token@github.com/neoswarm/neosh.git",
        "https://example.com/Ann/Repo",
        "https://example.com/ann/repo",
    ];
    let mut out = String::from("# input -> ProjectKey\n");
    for c in cases {
        let key = ProjectKey::from_remote(c).map(|k| k.0).unwrap_or_else(|| "(none)".into());
        out.push_str(&format!("{c}\t{key}\n"));
    }
    out.push_str(&format!("(no remote, dir \"neosh\")\t{}\n", ProjectKey::from_dir_name("neosh").0));
    check("project-keys.tsv", out);
}
