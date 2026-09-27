//! Getting an [`AscpMessage`] from one machine to another.
//!
//! Length-prefixed JSON: four bytes of big-endian length, then that many bytes of UTF-8. Chosen
//! over anything cleverer for one reason — you can read a capture of it. A swarm protocol is
//! debugged across machines you are not sitting at, and a binary encoding turns "what did it
//! actually send" into a tooling problem at exactly the moment you have no tooling.
//!
//! The length prefix is what makes it a message protocol rather than a byte stream. Newline
//! framing would have been simpler and is wrong the first time a conversation label contains one.

use neosh_proto::AscpMessage;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::SwarmError;

/// The largest frame that will be read.
///
/// A cap rather than trust, because the length is the first thing a peer sends and an
/// unauthenticated peer sends it *before* the handshake finishes. Without this, four bytes from
/// anything that can reach the port is a request to allocate as much memory as it likes.
///
/// Eight megabytes is far above any real message — the biggest is a conversation's history, and a
/// conversation that does not fit has other problems — and far below what hurts.
pub const MAX_FRAME: u32 = 8 * 1024 * 1024;

pub async fn write_message<W: AsyncWrite + Unpin>(
    w: &mut W,
    msg: &AscpMessage,
) -> Result<(), SwarmError> {
    let body = serde_json::to_vec(msg)?;
    let len = u32::try_from(body.len())
        .map_err(|_| SwarmError::Protocol("message will not fit in a frame".into()))?;
    if len > MAX_FRAME {
        return Err(SwarmError::Protocol(format!("message is {len} bytes, over the frame cap")));
    }
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

/// Read one message.
///
/// A clean end-of-stream is `Ok(None)` rather than an error: a peer that closed is not a peer that
/// failed, and the difference decides whether the board says "went away" or turns red.
pub async fn read_message<R: AsyncRead + Unpin>(
    r: &mut R,
) -> Result<Option<AscpMessage>, SwarmError> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len);
    if len > MAX_FRAME {
        // Fail the connection rather than skipping the frame. A peer that sent this is either
        // broken or hostile, and reading past it means trusting the same length field again.
        return Err(SwarmError::Protocol(format!(
            "peer announced a {len}-byte frame, over the {MAX_FRAME}-byte cap"
        )));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    Ok(Some(serde_json::from_slice(&body)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use neosh_proto::{NodeCapabilities, NodeId, NodeInfo};

    fn hello() -> AscpMessage {
        AscpMessage::Hello {
            version: neosh_proto::ASCP_VERSION,
            node: NodeInfo {
                id: NodeId("ab".into()),
                name: "mac-studio".into(),
                os: "macos".into(),
                version: "0.1.0".into(),
            },
            capabilities: NodeCapabilities {
                accepts_commands: true,
                accepts_approvals: false,
                shells: false,
                streams: true,
                browse: true,
                rich_stream: true,
                catalogue: true,
                live: true,
                projects: Vec::new(),
            },
            nonce: "00".into(),
        }
    }

    #[tokio::test]
    async fn a_message_survives_the_round_trip() {
        let mut buf = Vec::new();
        write_message(&mut buf, &hello()).await.expect("writes");
        let mut cursor = std::io::Cursor::new(buf);
        assert_eq!(read_message(&mut cursor).await.expect("reads"), Some(hello()));
    }

    /// The framing has to survive several messages back to back, which is the case a newline
    /// protocol gets right and a length-prefixed one gets wrong if the prefix is mis-sized.
    #[tokio::test]
    async fn several_messages_come_back_in_order() {
        let msgs = [hello(), AscpMessage::Goodbye { message: Some("bye".into()) }, hello()];
        let mut buf = Vec::new();
        for m in &msgs {
            write_message(&mut buf, m).await.expect("writes");
        }
        let mut cursor = std::io::Cursor::new(buf);
        for m in &msgs {
            assert_eq!(read_message(&mut cursor).await.expect("reads").as_ref(), Some(m));
        }
        assert_eq!(read_message(&mut cursor).await.expect("reads"), None, "and then it ends");
    }

    /// A label with a newline in it is exactly what newline framing would have broken on, so it is
    /// worth pinning that this one does not.
    #[tokio::test]
    async fn a_newline_inside_a_message_is_not_a_frame_boundary() {
        let msg = AscpMessage::Goodbye { message: Some("one\ntwo\nthree".into()) };
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).await.expect("writes");
        let mut cursor = std::io::Cursor::new(buf);
        assert_eq!(read_message(&mut cursor).await.expect("reads"), Some(msg));
    }

    #[tokio::test]
    async fn a_closed_stream_is_the_end_rather_than_a_failure() {
        let mut empty = std::io::Cursor::new(Vec::new());
        assert_eq!(read_message(&mut empty).await.expect("clean end"), None);
    }

    /// The length arrives before the handshake does, so an absurd one has to be refused rather
    /// than allocated.
    #[tokio::test]
    async fn an_oversized_frame_is_refused_without_allocating_it() {
        let mut buf = (MAX_FRAME + 1).to_be_bytes().to_vec();
        buf.extend_from_slice(b"{}");
        let mut cursor = std::io::Cursor::new(buf);
        let err = read_message(&mut cursor).await.expect_err("refused");
        assert!(format!("{err}").contains("cap"), "{err}");
    }

    #[tokio::test]
    async fn a_truncated_frame_is_an_error_not_a_hang() {
        let mut buf = 64u32.to_be_bytes().to_vec();
        buf.extend_from_slice(b"not sixty four bytes");
        let mut cursor = std::io::Cursor::new(buf);
        assert!(read_message(&mut cursor).await.is_err());
    }
}
