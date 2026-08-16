//! Tests extracted from typed.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;

	#[test]
	fn io_error_into_codec_is_a_decode_carrying_the_original_message() {
		let io = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe is gone");
		let codec: CodecError = io.into();
		assert!(matches!(codec, CodecError::Decode(_)));
		let shown = codec.to_string();
		assert!(
			shown.starts_with("codec decode:"),
			"displayed as a decode error: {shown}"
		);
		assert!(
			shown.contains("pipe is gone"),
			"original io message survives: {shown}"
		);
	}

	#[test]
	fn serde_error_into_codec_preserves_the_serde_message() {
		let serde_err = serde_json::from_str::<serde_json::Value>("{ not json").unwrap_err();
		let original = serde_err.to_string();
		let codec: CodecError = serde_err.into();
		assert!(matches!(codec, CodecError::Decode(_)));
		assert!(
			codec.to_string().contains(&original),
			"serde message preserved"
		);
	}

	#[test]
	fn rpc_error_absorbs_adapter_and_codec_via_from() {
		let a: RpcError = AdapterError::Eof.into();
		assert!(matches!(a, RpcError::Adapter(_)));
		assert!(a.to_string().contains("eof"), "{a}");

		let c: RpcError = CodecError::Encode("bad frame".into()).into();
		assert!(matches!(c, RpcError::Codec(_)));
		assert!(c.to_string().contains("bad frame"), "{c}");
	}
}
mod tests_2 {
	use super::*;
	use tokio::io::{AsyncReadExt, AsyncWriteExt};

	#[tokio::test]
	async fn inproc_reader_drains_leftover_across_small_reads() {
		let (a, b) = InprocAdapter::pair();
		let (_ar, mut aw) = Box::new(a).split();
		let (mut br, _bw) = Box::new(b).split();

		aw.write_all(b"hello").await.unwrap();

		let mut got = Vec::new();
		let mut chunk = [0u8; 2];
		while got.len() < 5 {
			let n = br.read(&mut chunk).await.unwrap();
			assert!(n > 0, "reader makes progress");
			got.extend_from_slice(&chunk[..n]);
		}
		assert_eq!(&got, b"hello", "leftover bytes are drained across reads");
	}
}
mod tests_3 {
	use super::*;
	use serde_json::json;

	fn enc<C: Codec>(c: &mut C, frame: C::Frame, b: &mut BytesMut) -> Result<(), CodecError> {
		c.encode(frame, b)
	}
	fn dec<C: Codec>(c: &mut C, b: &mut BytesMut) -> Result<Option<C::Frame>, CodecError> {
		c.decode(b)
	}

	#[test]
	fn json_roundtrip_single_frame() {
		let mut c = JsonEnvelopeCodec::new();
		let mut buf = BytesMut::new();
		enc(&mut c, json!({"id": 1, "method": "ping"}), &mut buf).unwrap();
		let got = dec(&mut c, &mut buf).unwrap().expect("one frame");
		assert_eq!(got, json!({"id": 1, "method": "ping"}));
		assert!(dec(&mut c, &mut buf).unwrap().is_none());
	}

	#[test]
	fn json_decodes_multiple_frames_from_one_buffer() {
		let mut c = JsonEnvelopeCodec::new();
		let mut buf = BytesMut::new();
		enc(&mut c, json!({"a": 1}), &mut buf).unwrap();
		enc(&mut c, json!({"b": 2}), &mut buf).unwrap();
		assert_eq!(dec(&mut c, &mut buf).unwrap().unwrap(), json!({"a": 1}));
		assert_eq!(dec(&mut c, &mut buf).unwrap().unwrap(), json!({"b": 2}));
		assert!(dec(&mut c, &mut buf).unwrap().is_none());
	}

	#[test]
	fn json_tolerates_crlf_and_skips_blank_lines() {
		let mut c = JsonEnvelopeCodec::new();
		let mut buf = BytesMut::from(&b"\n\r\n{\"ok\":true}\r\n"[..]);
		let got = dec(&mut c, &mut buf).unwrap().expect("frame after blanks");
		assert_eq!(got, json!({"ok": true}));
		assert!(dec(&mut c, &mut buf).unwrap().is_none());
	}

	#[test]
	fn json_many_consecutive_newlines_do_not_overflow() {
		let mut c = JsonEnvelopeCodec::new();
		let mut bytes = vec![b'\n'; 100_000];
		bytes.extend_from_slice(b"{\"v\":42}\n");
		let mut buf = BytesMut::from(&bytes[..]);
		assert_eq!(dec(&mut c, &mut buf).unwrap().unwrap(), json!({"v": 42}));
	}

	// The cap has to bite on an *incomplete* line, because that is the only shape
	// an endless frame ever has: a decoder that only measures finished lines
	// never gets a finished line to measure.
	#[test]
	fn a_capped_codec_refuses_before_the_line_is_even_complete() {
		let mut c = JsonEnvelopeCodec::new();
		c.set_max_frame_len(Some(16));
		let mut buf = BytesMut::from(&b"{\"token\":\"aaaaaaaaaaaaaaaaaaaa"[..]);
		let err = dec(&mut c, &mut buf).expect_err("30 bytes with no newline is over 16");
		assert!(err.to_string().contains("exceeds 16 bytes"), "{err}");
	}

	// And it must measure the line being decoded, not the buffer: a client is
	// free to write its auth frame and its first call in one go, and a cap that
	// counted what arrived behind the frame would refuse that client.
	#[test]
	fn a_capped_codec_measures_the_line_not_what_is_queued_behind_it() {
		let mut c = JsonEnvelopeCodec::new();
		c.set_max_frame_len(Some(16));
		let mut buf = BytesMut::from(&b"{\"a\":1}\n{\"b\":\"pipelined and long\"}\n"[..]);
		assert_eq!(dec(&mut c, &mut buf).unwrap().unwrap(), json!({"a": 1}));
	}

	#[test]
	fn json_partial_line_yields_none_until_newline() {
		let mut c = JsonEnvelopeCodec::new();
		let mut buf = BytesMut::from(&b"{\"partial\":1}"[..]);
		assert!(
			dec(&mut c, &mut buf).unwrap().is_none(),
			"incomplete line -> None"
		);
		buf.extend_from_slice(b"\n");
		assert_eq!(
			dec(&mut c, &mut buf).unwrap().unwrap(),
			json!({"partial": 1})
		);
	}
}
mod tests_4 {
	use super::Channel;
	use super::InprocAdapter;
	use super::JsonEnvelopeCodec;
	use serde_json::json;

	#[tokio::test]
	async fn channel_roundtrip_json_envelope() {
		let (a, b) = InprocAdapter::pair();
		let mut ca = Channel::new(a, JsonEnvelopeCodec::new());
		let mut cb = Channel::new(b, JsonEnvelopeCodec::new());
		ca.send(json!({"hello": "world"})).await.unwrap();
		let got = cb.recv().await.unwrap().unwrap();
		assert_eq!(got["hello"], "world");
	}

	#[tokio::test]
	async fn recv_returns_none_on_closed_adapter() {
		let (a, b) = InprocAdapter::pair();
		let ca = Channel::new(a, JsonEnvelopeCodec::new());
		let mut cb = Channel::new(b, JsonEnvelopeCodec::new());
		drop(ca);
		assert!(cb.recv().await.unwrap().is_none(), "EOF -> Ok(None)");
	}
}
mod cwd_tag_tests {
	use super::*;

	#[test]
	fn path_tag_is_stable_and_nonempty() {
		let dir = std::env::current_dir().unwrap();
		let a = path_tag(&dir);
		let b = path_tag(&dir);
		assert_eq!(a, b, "same path must yield the same tag");
		assert_eq!(a.len(), 16, "tag is 16 hex chars");
		assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
	}

	#[test]
	fn endpoint_kern_includes_tag() {
		let dir = std::env::current_dir().unwrap();
		let ep = Endpoint::kern();
		assert!(
			ep.display().contains(&path_tag(&dir)),
			"endpoint scoped by cwd tag"
		);
	}

	#[test]
	fn kern_for_cwd_matches_kern() {
		let dir = std::env::current_dir().unwrap();
		assert_eq!(
			Endpoint::kern().display(),
			Endpoint::kern_for(&dir).display(),
			"hub-computed endpoint must match the node's own"
		);
	}

	#[test]
	fn parse_round_trips_display() {
		let ep = Endpoint::hub();
		assert_eq!(Endpoint::parse(&ep.display()).display(), ep.display());
	}
}
