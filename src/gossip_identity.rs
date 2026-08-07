//! Peer identity and frame authentication: an ed25519 keypair per daemon, the
//! peer id derived from the public key, and sign/verify over `(seq, body)` so
//! a frame body cannot be swapped under its signature.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

use crate::gossip_types::SignedFrame;

/// blake3 of the ed25519 public key — the peer's stable federation identity.
pub type PeerId = [u8; 32];

// Frames whose envelope signature failed to verify. Invalid-sig traffic is
// free to send, so it is dropped before any per-peer state is allocated and
// only counted here.
static INVALID_SIG: AtomicU64 = AtomicU64::new(0);

pub fn invalid_sig_dropped() -> u64 {
	INVALID_SIG.load(Ordering::Relaxed)
}

/// The daemon's ed25519 keypair. The secret never leaves this process — the
/// daemon is the delegate; callers ask it to sign (see mcp delegate tools).
pub struct PeerIdentity {
	key: SigningKey,
}

impl PeerIdentity {
	pub fn generate() -> Self {
		use rand::RngExt;
		let mut rng = rand::rng();
		let mut seed = [0u8; 32];
		for chunk in seed.chunks_mut(8) {
			chunk.copy_from_slice(&rng.random::<u64>().to_le_bytes());
		}
		Self {
			key: SigningKey::from_bytes(&seed),
		}
	}

	pub fn from_bytes(seed: [u8; 32]) -> Self {
		Self {
			key: SigningKey::from_bytes(&seed),
		}
	}

	/// Load the peer key, minting it owner-only on first boot — the same
	/// pattern as `mint_token` in `config/serve.rs`. A corrupt file is a loud
	/// error, never a silent regeneration: regenerating changes who we are.
	pub fn load_or_mint(path: &Path) -> std::io::Result<Self> {
		match std::fs::read_to_string(path) {
			Ok(text) => {
				let seed = crate::gossip_contract::parse_key_hex(text.trim()).ok_or_else(|| {
					std::io::Error::new(
						std::io::ErrorKind::InvalidData,
						format!("peer key file {} is not 64 hex chars", path.display()),
					)
				})?;
				Ok(Self::from_bytes(seed))
			}
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
				if let Some(parent) = path.parent() {
					std::fs::create_dir_all(parent)?;
				}
				let id = Self::generate();
				let hex = crate::util::hex::encode(id.key.to_bytes());
				use std::io::Write;
				let mut f = create_private(path)?;
				f.write_all(hex.as_bytes())?;
				Ok(id)
			}
			Err(e) => Err(e),
		}
	}

	pub fn pubkey(&self) -> [u8; 32] {
		self.key.verifying_key().to_bytes()
	}

	pub fn peer_id(&self) -> PeerId {
		peer_id_of_pubkey(&self.pubkey())
	}

	pub fn loc(&self) -> f64 {
		loc_of(&self.peer_id())
	}

	/// Sign an arbitrary 32-byte digest. This is the delegate primitive: the
	/// mcp `sign` tool routes here so the key never crosses the socket.
	pub fn sign_digest(&self, digest: &[u8; 32]) -> Vec<u8> {
		self.key.sign(digest).to_bytes().to_vec()
	}

	/// Wrap an encoded message body in a signed wire envelope.
	pub fn sign_frame(&self, lamport: u64, body: Vec<u8>) -> SignedFrame {
		let digest = frame_digest(&body, lamport);
		SignedFrame {
			pubkey: self.pubkey(),
			sig: self.sign_digest(&digest),
			lamport,
			body,
		}
	}
}

pub fn peer_id_of_pubkey(pubkey: &[u8; 32]) -> PeerId {
	*blake3::hash(pubkey).as_bytes()
}

/// Ring location on the circle `[0, 1)` — the first 8 id bytes as a fraction
/// of the u64 space.
pub fn loc_of(id: &PeerId) -> f64 {
	let mut first = [0u8; 8];
	first.copy_from_slice(&id[..8]);
	// Keep 53 bits so the division is exact and the result stays strictly
	// below 1.0 — `u64::MAX as f64` rounds UP to 2^64 and would yield 1.0.
	(u64::from_le_bytes(first) >> 11) as f64 / (1u64 << 53) as f64
}

/// What the envelope signature covers: blake3(body || lamport_le). Binding
/// the lamport stops replaying an old body under a fresh clock.
pub fn frame_digest(body: &[u8], lamport: u64) -> [u8; 32] {
	let mut h = blake3::Hasher::new();
	h.update(body);
	h.update(&lamport.to_le_bytes());
	*h.finalize().as_bytes()
}

/// Verify a wire envelope. Returns the sender's PeerId on success; a failure
/// is counted and yields None. Cheap by construction — one hash, one ed25519
/// verify, no allocation of per-peer state — because invalid frames are free
/// to send and must not buy the sender anything.
pub fn verify_frame(f: &SignedFrame) -> Option<PeerId> {
	let ok = (|| {
		let vk = VerifyingKey::from_bytes(&f.pubkey).ok()?;
		let sig = Signature::from_slice(&f.sig).ok()?;
		let digest = frame_digest(&f.body, f.lamport);
		vk.verify_strict(&digest, &sig).ok()
	})();
	match ok {
		Some(()) => Some(peer_id_of_pubkey(&f.pubkey)),
		None => {
			INVALID_SIG.fetch_add(1, Ordering::Relaxed);
			None
		}
	}
}

pub fn verify_sig_by(pubkey: &[u8; 32], digest: &[u8; 32], sig: &[u8]) -> bool {
	let Ok(vk) = VerifyingKey::from_bytes(pubkey) else {
		return false;
	};
	let Ok(sig) = Signature::from_slice(sig) else {
		return false;
	};
	vk.verify(digest, &sig).is_ok()
}

// Owner-only from the moment the file exists — same rationale as the
// mcp-token minting in `config/serve.rs`.
#[cfg(unix)]
pub(crate) fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
	use std::os::unix::fs::OpenOptionsExt;
	std::fs::OpenOptions::new()
		.write(true)
		.create_new(true)
		.mode(0o600)
		.open(path)
}

#[cfg(not(unix))]
pub(crate) fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
	std::fs::OpenOptions::new()
		.write(true)
		.create_new(true)
		.open(path)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn peer_id_is_blake3_of_the_pubkey_and_deterministic() {
		let id = PeerIdentity::from_bytes([7u8; 32]);
		assert_eq!(id.peer_id(), *blake3::hash(&id.pubkey()).as_bytes());
		let again = PeerIdentity::from_bytes([7u8; 32]);
		assert_eq!(id.peer_id(), again.peer_id(), "same seed, same identity");
		let other = PeerIdentity::from_bytes([8u8; 32]);
		assert_ne!(id.peer_id(), other.peer_id());
	}

	#[test]
	fn loc_lands_on_the_unit_circle() {
		for seed in 0..32u8 {
			let id = PeerIdentity::from_bytes([seed; 32]);
			let l = id.loc();
			assert!((0.0..1.0).contains(&l), "loc {l} outside [0,1)");
		}
		assert_eq!(loc_of(&[0u8; 32]), 0.0);
		assert!(loc_of(&[0xffu8; 32]) < 1.0, "max id still strictly below 1");
	}

	#[test]
	fn a_signed_frame_verifies_and_a_tampered_one_is_counted() {
		let id = PeerIdentity::generate();
		let frame = id.sign_frame(9, b"hello ring".to_vec());
		assert_eq!(
			verify_frame(&frame),
			Some(id.peer_id()),
			"honest frame verifies to the signer's id"
		);

		let before = invalid_sig_dropped();
		let mut tampered_body = frame.clone();
		tampered_body.body = b"evil body".to_vec();
		assert_eq!(verify_frame(&tampered_body), None, "body swap fails");

		let mut tampered_clock = frame.clone();
		tampered_clock.lamport += 1;
		assert_eq!(verify_frame(&tampered_clock), None, "lamport replay fails");

		let mut wrong_key = frame.clone();
		wrong_key.pubkey = PeerIdentity::generate().pubkey();
		assert_eq!(verify_frame(&wrong_key), None, "foreign key fails");

		// The counter is process-global and other tests bump it in parallel,
		// so assert the floor this test alone guarantees.
		assert!(
			invalid_sig_dropped() >= before + 3,
			"every rejected frame is counted"
		);
	}

	#[test]
	fn load_or_mint_mints_once_then_reloads_the_same_key() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("state").join("peer.key");
		let first = PeerIdentity::load_or_mint(&path).unwrap();
		let second = PeerIdentity::load_or_mint(&path).unwrap();
		assert_eq!(
			first.peer_id(),
			second.peer_id(),
			"a reload is the same identity, not a new one"
		);
	}

	#[cfg(unix)]
	#[test]
	fn the_minted_key_file_is_owner_only() {
		use std::os::unix::fs::PermissionsExt;
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("peer.key");
		PeerIdentity::load_or_mint(&path).unwrap();
		let mode = std::fs::metadata(&path).unwrap().permissions().mode();
		assert_eq!(mode & 0o777, 0o600, "peer key never briefly world-readable");
	}

	#[test]
	fn a_corrupt_key_file_is_a_loud_error_not_a_new_identity() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("peer.key");
		std::fs::write(&path, "not hex at all").unwrap();
		let err = match PeerIdentity::load_or_mint(&path) {
			Err(e) => e,
			Ok(_) => panic!("a corrupt key file must not become a fresh identity"),
		};
		assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
	}

	#[test]
	fn verify_sig_by_checks_an_arbitrary_digest_signature() {
		let id = PeerIdentity::generate();
		let digest = frame_digest(b"payload", 0);
		let sig = id.sign_digest(&digest);
		assert!(verify_sig_by(&id.pubkey(), &digest, &sig));
		assert!(!verify_sig_by(
			&id.pubkey(),
			&frame_digest(b"other", 0),
			&sig
		));
		assert!(!verify_sig_by(
			&PeerIdentity::generate().pubkey(),
			&digest,
			&sig
		));
	}
}
