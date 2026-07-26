use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};

use crate::base::types::{ChunkPart, ChunkPartKind, Entity};

/// PrivacyV0 scheme 0: xchacha20poly1305 (FEDERATION_PLAN §6). Entity text is
/// encrypted client-side BEFORE it enters the shared kern; the contract
/// validates signatures and content-hash ids over the ciphertext, so relay
/// peers store and route bytes they cannot read. The symmetric key travels
/// out-of-band — v0 is a file beside the peer key.
pub const PRIVACY_SCHEME_XCHACHA20: u8 = 0;

const CIPHERTEXT_PREFIX: &str = "enc0:";
const NONCE_LEN: usize = 24;

/// The 8-byte hint carried in params so a receiver knows WHICH key to try
/// without the hint identifying the key.
pub fn key_hint(key: &[u8; 32]) -> [u8; 8] {
	let mut h = blake3::Hasher::new();
	h.update(b"kern-privacy-hint");
	h.update(&[0u8]);
	h.update(key);
	let mut out = [0u8; 8];
	out.copy_from_slice(&h.finalize().as_bytes()[..8]);
	out
}

pub fn encrypt_text(key: &[u8; 32], plaintext: &str) -> String {
	use rand::RngExt;
	let mut rng = rand::rng();
	let mut nonce = [0u8; NONCE_LEN];
	for chunk in nonce.chunks_mut(8) {
		let r = rng.random::<u64>().to_le_bytes();
		chunk.copy_from_slice(&r[..chunk.len()]);
	}
	let cipher = XChaCha20Poly1305::new(&Key::from(*key));
	let ct = cipher
		.encrypt(&XNonce::from(nonce), plaintext.as_bytes())
		.expect("xchacha20poly1305 encryption is infallible for in-memory data");
	let mut body = Vec::with_capacity(NONCE_LEN + ct.len());
	body.extend_from_slice(&nonce);
	body.extend_from_slice(&ct);
	format!(
		"{CIPHERTEXT_PREFIX}{}",
		crate::base::util::hex::encode(body)
	)
}

pub fn decrypt_text(key: &[u8; 32], ciphertext: &str) -> Option<String> {
	let hex = ciphertext.strip_prefix(CIPHERTEXT_PREFIX)?;
	if hex.len() % 2 != 0 {
		return None;
	}
	let mut bytes = Vec::with_capacity(hex.len() / 2);
	for i in (0..hex.len()).step_by(2) {
		bytes.push(u8::from_str_radix(&hex[i..i + 2], 16).ok()?);
	}
	if bytes.len() <= NONCE_LEN {
		return None;
	}
	let (nonce, ct) = bytes.split_at(NONCE_LEN);
	let cipher = XChaCha20Poly1305::new(&Key::from(*key));
	let nonce = XNonce::try_from(nonce).ok()?;
	let pt = cipher.decrypt(&nonce, ct).ok()?;
	String::from_utf8(pt).ok()
}

/// Seal an entity for a private shared kern: ciphertext becomes the body,
/// the id is the content hash OF THE CIPHERTEXT (so `id_matches_body` and
/// writer signatures keep holding on relays), and the vector is dropped —
/// a relay cannot semantically route what it cannot read, and shipping the
/// plaintext's embedding would leak what encryption just hid.
pub fn encrypt_entity(key: &[u8; 32], entity: &Entity) -> Entity {
	let sealed_text = encrypt_text(key, &entity.text());
	let mut out = entity.clone();
	out.id = crate::base::util::content_hash(&sealed_text);
	out.statements = vec![sealed_text];
	out.chunks = vec![ChunkPart {
		kind: ChunkPartKind::StatementRef,
		text: String::new(),
		index: 0,
	}];
	out.vector = Vec::new().into();
	out.gnn_vector = Vec::new().into();
	out
}

/// Open a sealed entity on the local daemon: plaintext body, id restored to
/// the plaintext's content hash so local dedup and retrieval see one truth.
/// None = not ours to read (wrong key or not sealed).
pub fn decrypt_entity(key: &[u8; 32], entity: &Entity) -> Option<Entity> {
	let plaintext = decrypt_text(key, &entity.text())?;
	let mut out = entity.clone();
	out.id = crate::base::util::content_hash(&plaintext);
	out.statements = vec![plaintext];
	out.chunks = vec![ChunkPart {
		kind: ChunkPartKind::StatementRef,
		text: String::new(),
		index: 0,
	}];
	Some(out)
}

/// Load the contract's symmetric key, minting it owner-only on first use —
/// v0 key distribution is "copy this file", so the file has to exist.
pub fn load_or_mint_key(path: &std::path::Path) -> std::io::Result<[u8; 32]> {
	match std::fs::read_to_string(path) {
		Ok(text) => crate::gossip::contract::parse_key_hex(text.trim()).ok_or_else(|| {
			std::io::Error::new(
				std::io::ErrorKind::InvalidData,
				format!("contract key file {} is not 64 hex chars", path.display()),
			)
		}),
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
			if let Some(parent) = path.parent() {
				std::fs::create_dir_all(parent)?;
			}
			use rand::RngExt;
			let mut rng = rand::rng();
			let mut key = [0u8; 32];
			for chunk in key.chunks_mut(8) {
				chunk.copy_from_slice(&rng.random::<u64>().to_le_bytes());
			}
			use std::io::Write;
			let mut f = create_private(path)?;
			f.write_all(crate::base::util::hex::encode(key).as_bytes())?;
			Ok(key)
		}
		Err(e) => Err(e),
	}
}

#[cfg(unix)]
fn create_private(path: &std::path::Path) -> std::io::Result<std::fs::File> {
	use std::os::unix::fs::OpenOptionsExt;
	std::fs::OpenOptions::new()
		.write(true)
		.create_new(true)
		.mode(0o600)
		.open(path)
}

#[cfg(not(unix))]
fn create_private(path: &std::path::Path) -> std::io::Result<std::fs::File> {
	std::fs::OpenOptions::new()
		.write(true)
		.create_new(true)
		.open(path)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::base::types::EntityKind;
	use crate::gossip::contract::*;
	use crate::gossip::identity::PeerIdentity;

	fn plain_entity(text: &str) -> Entity {
		Entity {
			id: crate::base::util::content_hash(text),
			kind: EntityKind::Fact,
			statements: vec![text.to_string()],
			chunks: vec![ChunkPart {
				kind: ChunkPartKind::StatementRef,
				text: String::new(),
				index: 0,
			}],
			..Default::default()
		}
	}

	#[test]
	fn text_round_trips_and_a_wrong_key_reads_nothing() {
		let key = [7u8; 32];
		let ct = encrypt_text(&key, "the launch code is 0000");
		assert!(ct.starts_with("enc0:"));
		assert!(!ct.contains("launch"), "ciphertext leaks no plaintext");
		assert_eq!(
			decrypt_text(&key, &ct).as_deref(),
			Some("the launch code is 0000")
		);
		assert_eq!(
			decrypt_text(&[8u8; 32], &ct),
			None,
			"a wrong key fails closed, not garbled-open"
		);
		assert_ne!(
			encrypt_text(&key, "same text"),
			encrypt_text(&key, "same text"),
			"fresh nonce per seal — equal plaintexts are unlinkable"
		);
	}

	#[test]
	fn a_sealed_entity_still_satisfies_the_contract_gauntlet() {
		let key = [9u8; 32];
		let writer = PeerIdentity::from_bytes([3u8; 32]);
		let sealed = encrypt_entity(&key, &plain_entity("private sentinel"));
		assert!(
			crate::gossip::handler::id_matches_body(&sealed),
			"the id is the ciphertext's content hash — relays keep verifying"
		);
		let digest = entity_sig_digest(&sealed.id, 1);
		let se = SignedEntity {
			sig: writer.sign_digest(&digest),
			signer: writer.pubkey(),
			lamport: 1,
			entity: sealed,
		};
		let params = ParamsV0 {
			owners: vec![writer.pubkey()],
			writers: WritePolicy::OwnersOnly,
			kinds: None,
			max_entities: 10,
			retention_secs: None,
			private: Some(PrivacyV0 {
				scheme: PRIVACY_SCHEME_XCHACHA20,
				key_hint: key_hint(&key),
			}),
			legacy_network_id: None,
		};
		assert_eq!(
			SignedCrdt.validate_delta(
				&params,
				&ContractState::default(),
				&Delta { entities: vec![se] }
			),
			Ok(()),
			"signatures and ids hold over ciphertext"
		);
	}

	// Gate 6: the relay stores and forwards bytes it cannot read — its whole
	// serialized store never contains the sentinel; only a key holder sees it.
	#[test]
	fn a_relay_never_sees_the_plaintext_a_key_holder_recovers() {
		let key = [11u8; 32];
		let sentinel = "PLAINTEXT-SENTINEL-9f2e";
		let writer = PeerIdentity::from_bytes([4u8; 32]);

		let sealed = encrypt_entity(&key, &plain_entity(sentinel));
		let digest = entity_sig_digest(&sealed.id, 1);
		let se = SignedEntity {
			sig: writer.sign_digest(&digest),
			signer: writer.pubkey(),
			lamport: 1,
			entity: sealed,
		};
		let params = ParamsV0 {
			owners: Vec::new(),
			writers: WritePolicy::Open,
			kinds: None,
			max_entities: 10,
			retention_secs: None,
			private: Some(PrivacyV0 {
				scheme: PRIVACY_SCHEME_XCHACHA20,
				key_hint: key_hint(&key),
			}),
			legacy_network_id: None,
		};
		let cid = contract_id(SIGNED_CRDT_V0_TAG, &params);
		let kid = contract_kern_id(&cid);

		// The relay applies the delta like any tree hop.
		let mut g = crate::base::graph::GraphGnn::new();
		let mut k = crate::base::types::Kern::new(&kid, &g.root.id);
		k.root_id = g.root.root_id.clone();
		g.register(k);
		let mut state = ContractState::default();
		SignedCrdt.apply(
			&mut g,
			&kid,
			&params,
			&mut state,
			Delta { entities: vec![se] },
		);

		// Grep the relay's entire serialized kern for the sentinel.
		let kern_bytes = bincode::serde::encode_to_vec(
			g.kerns.get(&kid).expect("contract kern exists"),
			bincode::config::standard(),
		)
		.unwrap();
		let needle = sentinel.as_bytes();
		let leaked = kern_bytes.windows(needle.len()).any(|w| w == needle);
		assert!(
			!leaked,
			"the relay's store must never contain the plaintext"
		);

		// A key holder decrypts on merge into its local phantom.
		let stored = g.kerns[&kid].entities.values().next().unwrap();
		let opened = decrypt_entity(&key, stored).expect("key holder reads it");
		assert_eq!(opened.text(), sentinel);
		assert_eq!(
			opened.id,
			crate::base::util::content_hash(sentinel),
			"locally the plaintext hash is the id again"
		);
		assert!(
			decrypt_entity(&[12u8; 32], stored).is_none(),
			"a non-holder gets nothing"
		);
	}

	#[cfg(unix)]
	#[test]
	fn the_minted_contract_key_file_is_owner_only_and_reloads() {
		use std::os::unix::fs::PermissionsExt;
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("keys").join("contract.key");
		let first = load_or_mint_key(&path).unwrap();
		let mode = std::fs::metadata(&path).unwrap().permissions().mode();
		assert_eq!(mode & 0o777, 0o600);
		assert_eq!(first, load_or_mint_key(&path).unwrap());
	}
}
