use bytes::BytesMut;
use serde_json::Value;
use tokio_util::codec::{Decoder, Encoder};

use super::error::CodecError;

pub trait Codec: Send + 'static {
	type Frame: Send;
	fn encode(&mut self, frame: Self::Frame, dst: &mut BytesMut) -> Result<(), CodecError>;
	fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Frame>, CodecError>;
}

// No blanket `impl<T: Codec> Encoder<T::Frame> for T` without orphan-rule
// grief — each concrete codec carries delegating `Encoder`/`Decoder` impls.

// Wire framing: each frame is one Value's JSON text + '\n'.
//
// There is no length prefix, so nothing on the wire announces how big a frame
// will be: `FramedRead` simply keeps reading and doubling its buffer until a
// newline shows up. `max_frame_len` is the only thing that can stop that, and it
// stops it on the *buffer* rather than on a finished line — an endless line is
// refused at the cap instead of accreted. Off by default; `verify_auth` is the
// one caller that sets it, because it is the one read from a peer that has
// proven nothing.
#[derive(Default)]
pub struct JsonEnvelopeCodec {
	max_frame_len: Option<usize>,
}

impl JsonEnvelopeCodec {
	pub fn new() -> Self {
		Self::default()
	}

	pub fn set_max_frame_len(&mut self, max: Option<usize>) {
		self.max_frame_len = max;
	}
}

impl Codec for JsonEnvelopeCodec {
	type Frame = Value;

	fn encode(&mut self, frame: Self::Frame, dst: &mut BytesMut) -> Result<(), CodecError> {
		let s = serde_json::to_string(&frame).map_err(|e| CodecError::Encode(e.to_string()))?;
		if s.contains('\n') {
			return Err(CodecError::Encode("frame contained newline".into()));
		}
		dst.extend_from_slice(s.as_bytes());
		dst.extend_from_slice(b"\n");
		Ok(())
	}

	fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Frame>, CodecError> {
		// Loop, NOT recursion, over leading blank lines — N consecutive newlines
		// once recursed N deep (see json_many_consecutive_newlines_do_not_overflow).
		loop {
			let nl = src.iter().position(|&b| b == b'\n');
			if let Some(max) = self.max_frame_len {
				// `nl.unwrap_or(src.len())`, not `src.len()`: a peer may pipeline
				// several frames into one read, and the cap is about the line being
				// decoded, not about how much arrived behind it.
				if nl.unwrap_or(src.len()) > max {
					return Err(CodecError::Decode(format!("frame exceeds {max} bytes")));
				}
			}
			let Some(pos) = nl else {
				return Ok(None);
			};
			let line = src.split_to(pos + 1);
			let slice = &line[..pos];
			let trimmed = if slice.last() == Some(&b'\r') {
				&slice[..slice.len() - 1]
			} else {
				slice
			};
			if trimmed.is_empty() {
				continue;
			}
			let v: Value =
				serde_json::from_slice(trimmed).map_err(|e| CodecError::Decode(e.to_string()))?;
			return Ok(Some(v));
		}
	}
}

impl Encoder<Value> for JsonEnvelopeCodec {
	type Error = CodecError;
	fn encode(&mut self, item: Value, dst: &mut BytesMut) -> Result<(), Self::Error> {
		<Self as Codec>::encode(self, item, dst)
	}
}

impl Decoder for JsonEnvelopeCodec {
	type Item = Value;
	type Error = CodecError;
	fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
		<Self as Codec>::decode(self, src)
	}
}

#[cfg(test)]
mod tests {
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
