use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::base_constants::{GOSSIP_DIAL_TIMEOUT, GOSSIP_FETCH_TIMEOUT, GOSSIP_MAX_FRAME_BYTES};

use crate::gossip_identity::{verify_frame, PeerId, PeerIdentity};
use crate::gossip_types::*;

// Wire frame: big-endian u32 length prefix, then bincode of a SignedFrame
// whose body is the bincode of the GossipMessage.
pub(super) async fn encode_msg(
	stream: &mut TcpStream,
	identity: &PeerIdentity,
	lamport: u64,
	msg: &GossipMessage,
) -> Result<(), std::io::Error> {
	let body = bincode::serde::encode_to_vec(msg, bincode::config::standard())
		.map_err(std::io::Error::other)?;
	let frame = identity.sign_frame(lamport, body);
	let bytes = bincode::serde::encode_to_vec(&frame, bincode::config::standard())
		.map_err(std::io::Error::other)?;
	let len = (bytes.len() as u32).to_be_bytes();
	stream.write_all(&len).await?;
	stream.write_all(&bytes).await?;
	stream.flush().await?;
	Ok(())
}

// Reject a prefix over GOSSIP_MAX_FRAME_BYTES before allocating the body
// buffer, then verify the envelope signature before decoding the message —
// a forged frame is dropped (and counted) without buying any state.
pub(super) async fn decode_msg(stream: &mut TcpStream) -> Option<(PeerId, GossipMessage)> {
	let mut len_buf = [0u8; 4];
	stream.read_exact(&mut len_buf).await.ok()?;
	let len = u32::from_be_bytes(len_buf) as usize;
	if len > GOSSIP_MAX_FRAME_BYTES {
		return None;
	}
	let mut buf = vec![0u8; len];
	stream.read_exact(&mut buf).await.ok()?;
	let (frame, _): (SignedFrame, _) =
		bincode::serde::decode_from_slice(&buf, bincode::config::standard()).ok()?;
	let peer = verify_frame(&frame)?;
	bincode::serde::decode_from_slice(&frame.body, bincode::config::standard())
		.ok()
		.map(|(v, _)| (peer, v))
}

pub(super) async fn send_msg(
	addr: &str,
	identity: &PeerIdentity,
	lamport: u64,
	msg: &GossipMessage,
) -> Result<(), std::io::Error> {
	let mut stream = tokio::time::timeout(GOSSIP_DIAL_TIMEOUT, TcpStream::connect(addr))
		.await
		.map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "dial timeout"))??;
	encode_msg(&mut stream, identity, lamport, msg).await
}

pub(super) async fn send_and_receive(
	addr: &str,
	identity: &PeerIdentity,
	lamport: u64,
	msg: &GossipMessage,
) -> Option<GossipMessage> {
	let mut stream = tokio::time::timeout(GOSSIP_DIAL_TIMEOUT, TcpStream::connect(addr))
		.await
		.ok()?
		.ok()?;
	encode_msg(&mut stream, identity, lamport, msg).await.ok()?;
	tokio::time::timeout(GOSSIP_FETCH_TIMEOUT, decode_msg(&mut stream))
		.await
		.ok()?
		.map(|(_, reply)| reply)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::gossip_identity::invalid_sig_dropped;
	use tokio::net::TcpListener;

	fn sample_msg() -> GossipMessage {
		GossipMessage {
			kind: GossipKind::PeerExchange,
			id: "msg-1".into(),
			origin: "127.0.0.1:9999".into(),
			payload: GossipPayload::PeerExchange(PeerExchangePayload {
				peers: vec!["a".into(), "b".into()],
			}),
		}
	}

	#[tokio::test]
	async fn encode_decode_round_trips_over_loopback() {
		let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = listener.local_addr().unwrap().to_string();
		let msg = sample_msg();
		let identity = PeerIdentity::generate();
		let expected_peer = identity.peer_id();

		let server = tokio::spawn(async move {
			let (mut stream, _) = listener.accept().await.unwrap();
			decode_msg(&mut stream).await
		});

		let mut client = TcpStream::connect(&addr).await.unwrap();
		encode_msg(&mut client, &identity, 7, &msg).await.unwrap();

		let (peer, got) = server
			.await
			.unwrap()
			.expect("a signed message decodes on the server side");
		assert_eq!(peer, expected_peer, "the envelope names its signer");
		assert_eq!(got.kind, msg.kind);
		assert_eq!(got.id, msg.id);
		assert_eq!(got.origin, msg.origin);
		match got.payload {
			GossipPayload::PeerExchange(p) => {
				assert_eq!(p.peers, vec!["a".to_string(), "b".to_string()]);
			}
			other => panic!("round-trip changed the payload variant: {other:?}"),
		}
	}

	#[tokio::test]
	async fn a_tampered_envelope_is_dropped_and_counted() {
		let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = listener.local_addr().unwrap().to_string();
		let identity = PeerIdentity::generate();

		// Sign one body, then ship a different one under the same signature.
		let body = bincode::serde::encode_to_vec(&sample_msg(), bincode::config::standard()).unwrap();
		let mut frame = identity.sign_frame(3, body);
		frame.body = bincode::serde::encode_to_vec(
			&GossipMessage {
				id: "forged".into(),
				..sample_msg()
			},
			bincode::config::standard(),
		)
		.unwrap();
		let bytes = bincode::serde::encode_to_vec(&frame, bincode::config::standard()).unwrap();

		let server = tokio::spawn(async move {
			let (mut stream, _) = listener.accept().await.unwrap();
			decode_msg(&mut stream).await
		});

		use tokio::io::AsyncWriteExt;
		let mut client = TcpStream::connect(&addr).await.unwrap();
		let before = invalid_sig_dropped();
		client
			.write_all(&(bytes.len() as u32).to_be_bytes())
			.await
			.unwrap();
		client.write_all(&bytes).await.unwrap();
		client.flush().await.unwrap();

		assert!(
			server.await.unwrap().is_none(),
			"a body swap under a stale signature never decodes"
		);
		assert!(
			invalid_sig_dropped() > before,
			"the rejected frame is counted"
		);
	}

	#[tokio::test]
	async fn decode_rejects_frame_over_max_size() {
		let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = listener.local_addr().unwrap().to_string();

		let server = tokio::spawn(async move {
			let (mut stream, _) = listener.accept().await.unwrap();
			decode_msg(&mut stream).await
		});

		use tokio::io::AsyncWriteExt;
		let mut client = TcpStream::connect(&addr).await.unwrap();
		let oversized = ((GOSSIP_MAX_FRAME_BYTES + 1) as u32).to_be_bytes();
		client.write_all(&oversized).await.unwrap();
		client.flush().await.unwrap();

		let got = server.await.unwrap();
		assert!(got.is_none(), "an oversized prefix is refused, not buffered");
	}
}
