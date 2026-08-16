//! Tests extracted from wire.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;

	// RFC 6455 section 1.3's own worked example.
	#[test]
	fn the_rfc_example_key_yields_the_rfc_example_accept() {
		let accept = base64(&sha1(format!("dGhlIHNhbXBsZSBub25jZQ=={GUID}").as_bytes()));
		assert_eq!(accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
	}
}
