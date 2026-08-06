pub mod adapter;
pub mod channel;
pub mod codec;
pub mod error;
pub mod local;

pub use adapter::{Adapter, InprocAdapter};
pub use channel::Channel;
pub use codec::{Codec, JsonEnvelopeCodec};
pub use error::{AdapterError, CodecError, RpcError};
#[cfg(unix)]
pub use local::adopt_kern_listener;
#[cfg(windows)]
pub use local::NamedPipeAdapter;
#[cfg(unix)]
pub use local::UnixStreamAdapter;
pub use local::{
	bind_kern_listener, connect_kern, BindError, BindOutcome, Endpoint, LocalAdapter, LocalListener,
};
