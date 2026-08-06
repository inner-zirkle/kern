use futures::{SinkExt, StreamExt};
use tokio_util::codec::{Decoder, Encoder, FramedRead, FramedWrite};

use super::adapter::{Adapter, DynRead, DynWrite};
use super::codec::Codec;
use super::error::{AdapterError, CodecError};

pub struct Channel<C>
where
	C: Codec
		+ Default
		+ Encoder<<C as Codec>::Frame, Error = CodecError>
		+ Decoder<Item = <C as Codec>::Frame, Error = CodecError>,
{
	reader: FramedRead<DynRead, C>,
	writer: FramedWrite<DynWrite, C>,
}

impl<C> Channel<C>
where
	C: Codec
		+ Default
		+ Encoder<<C as Codec>::Frame, Error = CodecError>
		+ Decoder<Item = <C as Codec>::Frame, Error = CodecError>,
{
	pub fn new<A: Adapter>(adapter: A, codec: C) -> Self {
		let (read_half, write_half) = Box::new(adapter).split();
		let reader = FramedRead::new(read_half, codec);
		let writer = FramedWrite::new(write_half, C::default());
		Self { reader, writer }
	}

	/// The read half's codec, so framing can be tightened for one phase of a
	/// conversation and loosened again — `verify_auth` caps the pre-auth frame
	/// through here. The write half is deliberately not exposed: what we send is
	/// ours already.
	pub fn decoder_mut(&mut self) -> &mut C {
		self.reader.decoder_mut()
	}

	pub async fn send(&mut self, frame: <C as Codec>::Frame) -> Result<(), AdapterError> {
		self
			.writer
			.send(frame)
			.await
			.map_err(adapter_err_from_codec)?;
		Ok(())
	}

	pub async fn recv(&mut self) -> Result<Option<<C as Codec>::Frame>, AdapterError> {
		match self.reader.next().await {
			Some(Ok(f)) => Ok(Some(f)),
			Some(Err(e)) => Err(adapter_err_from_codec(e)),
			None => Ok(None),
		}
	}
}

fn adapter_err_from_codec(e: CodecError) -> AdapterError {
	AdapterError::Codec(e)
}

#[cfg(test)]
mod tests {
	use super::super::adapter::InprocAdapter;
	use super::super::codec::JsonEnvelopeCodec;
	use super::Channel;
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
