use thiserror::Error;

#[derive(Debug, Error)]
pub enum AdapterError {
	#[error("adapter i/o: {0}")]
	Io(#[from] std::io::Error),
	#[error("adapter eof")]
	Eof,
	#[error("adapter codec: {0}")]
	Codec(#[from] CodecError),
	// The peer answered and refused this caller. Distinct from `Io` on purpose:
	// a refusal proves the server is *there*, so a client must never downgrade
	// it to "nothing is serving" and go act on the resource itself.
	#[error("adapter unauthenticated: {0}")]
	Unauthenticated(String),
	// The other direction: *this* caller refused *the endpoint*, before saying
	// anything to it. Distinct from `Io` because an absent socket and a socket
	// belonging to somebody else are opposite facts — one means no daemon, the
	// other means something is bound that this user does not own — and distinct
	// from `Unauthenticated` because no peer has been consulted, so it must
	// never be reported as the daemon's verdict.
	#[error("adapter untrusted endpoint: {0}")]
	UntrustedEndpoint(String),
	#[error("adapter: {0}")]
	Other(String),
}

#[derive(Debug, Error)]
pub enum CodecError {
	#[error("codec encode: {0}")]
	Encode(String),
	#[error("codec decode: {0}")]
	Decode(String),
}

#[derive(Debug, Error)]
pub enum RpcError {
	#[error("rpc adapter: {0}")]
	Adapter(String),
	#[error("rpc codec: {0}")]
	Codec(String),
	#[error("rpc method not found: {0}")]
	MethodNotFound(String),
	#[error("rpc deadline exceeded")]
	Deadline,
	#[error("rpc application error: {0}")]
	Application(String),
}

impl From<serde_json::Error> for CodecError {
	fn from(e: serde_json::Error) -> Self {
		CodecError::Decode(e.to_string())
	}
}

impl From<std::io::Error> for CodecError {
	fn from(e: std::io::Error) -> Self {
		CodecError::Decode(format!("io: {e}"))
	}
}

impl From<AdapterError> for RpcError {
	fn from(e: AdapterError) -> Self {
		RpcError::Adapter(e.to_string())
	}
}

impl From<CodecError> for RpcError {
	fn from(e: CodecError) -> Self {
		RpcError::Codec(e.to_string())
	}
}

#[cfg(test)]
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
