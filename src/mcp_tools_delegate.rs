//! Delegated-key tools: `sign` (the daemon signs with its peer key, which
//! never leaves the process) and `contract_grant` (owner-signed contract
//! amendment) — the federation operations a host asks the daemon to perform.

use crate::mcp::{tool_error, tool_result_json, Server};
use gossip::gossip_contract::{
	contract_id, params_from_config, tombstone_digest, WritePolicy, SIGNED_CRDT_V0_TAG,
};
use gossip::gossip_identity::PeerIdentity;

pub(crate) fn tool_schemas() -> Vec<serde_json::Value> {
	vec![
		serde_json::json!({
			"name": "sign",
			"description": "Sign a 32-byte payload hash with this daemon's ed25519 peer key. The daemon is the delegate: the key never leaves the process, so agents and CLIs ask it to sign instead of reading key files. Returns the signature, the public key, and the derived peer id, all hex.",
			"inputSchema": {
				"type": "object",
				"properties": {
					"payload_hash": {"type": "string", "description": "blake3/sha256 digest to sign, 64 hex chars"}
				},
				"required": ["payload_hash"]
			},
		}),
		serde_json::json!({
			"name": "contract_grant",
			"description": "Owner-signed contract params amendment: add a writer's public key to a hosted contract's allowlist. The key IS the policy, so amending params mints a NEW contract id; the returned tombstone signature lets the old contract point subscribers at the new one. This daemon's peer key must be among the contract's owners.",
			"inputSchema": {
				"type": "object",
				"properties": {
					"contract": {"type": "object", "description": "the current contract table, same shape as [[gossip.contracts]]: kind, owners, writers, writer_keys, kinds, max_entities, retention_secs"},
					"pubkey": {"type": "string", "description": "hex ed25519 public key to grant write access"}
				},
				"required": ["contract", "pubkey"]
			},
		}),
	]
}

impl Server {
	// The delegate keypair. Resolved per call from the same path the gossip
	// boot uses, minting on first use — cheap (one small file read), and a
	// Server field would ripple through every constructor for no gain.
	fn delegate_identity(&self) -> std::io::Result<PeerIdentity> {
		let key_path = if self.cfg.gossip.identity_path.trim().is_empty() {
			std::path::Path::new(&self.cfg.data_dir).join("peer.key")
		} else {
			std::path::PathBuf::from(self.cfg.gossip.identity_path.trim())
		};
		PeerIdentity::load_or_mint(&key_path)
	}

	pub(crate) fn tool_sign(&self, args: &serde_json::Value) -> serde_json::Value {
		let Some(hash_hex) = args.get("payload_hash").and_then(|v| v.as_str()) else {
			return tool_error("payload_hash is required (64 hex chars)");
		};
		let Some(digest) = gossip::gossip_contract::parse_key_hex(hash_hex) else {
			return tool_error("payload_hash must be exactly 64 hex chars (a 32-byte digest)");
		};
		let identity = match self.delegate_identity() {
			Ok(id) => id,
			Err(e) => return tool_error(&format!("peer key unavailable: {e}")),
		};
		let sig = identity.sign_digest(&digest);
		tool_result_json(&serde_json::json!({
			"signature": util::hex::encode(&sig),
			"pubkey": util::hex::encode(identity.pubkey()),
			"peer_id": util::hex::encode(identity.peer_id()),
		}))
	}

	pub(crate) fn tool_contract_grant(&self, args: &serde_json::Value) -> serde_json::Value {
		let Some(contract_val) = args.get("contract") else {
			return tool_error("contract is required (the current [[gossip.contracts]] table)");
		};
		let cfg: config::ContractConfig = match serde_json::from_value(contract_val.clone()) {
			Ok(c) => c,
			Err(e) => return tool_error(&format!("contract does not parse: {e}")),
		};
		let Some(params) = params_from_config(&cfg) else {
			return tool_error(
				"contract refused: unknown kind, writer policy, claim kind, or unparseable key",
			);
		};
		let Some(grantee_hex) = args.get("pubkey").and_then(|v| v.as_str()) else {
			return tool_error("pubkey is required (hex ed25519 public key)");
		};
		let Some(grantee) = gossip::gossip_contract::parse_key_hex(grantee_hex) else {
			return tool_error("pubkey must be 64 hex chars");
		};
		let identity = match self.delegate_identity() {
			Ok(id) => id,
			Err(e) => return tool_error(&format!("peer key unavailable: {e}")),
		};
		if !params.owners.contains(&identity.pubkey()) {
			return tool_error("this daemon's peer key is not among the contract's owners");
		}

		let old_id = contract_id(SIGNED_CRDT_V0_TAG, &params);
		let mut amended = params;
		amended.writers = match amended.writers {
			WritePolicy::Open => {
				return tool_error("the contract is already open to all writers; nothing to grant")
			}
			WritePolicy::OwnersOnly => WritePolicy::Allowlist(vec![grantee]),
			WritePolicy::Allowlist(mut keys) => {
				if keys.contains(&grantee) {
					return tool_error("that key is already an admissible writer");
				}
				keys.push(grantee);
				WritePolicy::Allowlist(keys)
			}
		};
		let new_id = contract_id(SIGNED_CRDT_V0_TAG, &amended);
		// The forward pointer: publish this signature in a Tombstone frame on
		// the old contract and subscribers verify it against the owners.
		let tombstone_sig = identity.sign_digest(&tombstone_digest(&old_id, &new_id));

		let mut writer_keys: Vec<String> = cfg.writer_keys.clone();
		writer_keys.push(grantee_hex.trim().to_string());
		tool_result_json(&serde_json::json!({
			"old_id": util::hex::encode(old_id),
			"new_id": util::hex::encode(new_id),
			"tombstone_sig": util::hex::encode(&tombstone_sig),
			"amended_contract": {
				"kind": cfg.kind,
				"owners": cfg.owners,
				"writers": "allowlist",
				"writer_keys": writer_keys,
				"kinds": cfg.kinds,
				"max_entities": cfg.max_entities,
				"retention_secs": cfg.retention_secs,
			},
		}))
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use gossip::gossip_identity::verify_sig_by;

	use crate::mcp::tools::is_error;

	fn body(v: &serde_json::Value) -> serde_json::Value {
		let text = v["content"][0]["text"].as_str().expect("text content");
		serde_json::from_str(text).expect("tool payload is json")
	}

	fn server_in(dir: &std::path::Path) -> Server {
		let mut cfg = config::Config::default_in(dir);
		cfg.data_dir = dir.to_string_lossy().to_string();
		crate::test_support::mcp_server_with_config(cfg)
	}

	// tokio tests: the default rig's Worker spawns onto the runtime.
	#[tokio::test]
	async fn sign_returns_a_signature_the_daemons_own_pubkey_verifies() {
		let dir = tempfile::tempdir().unwrap();
		let s = server_in(dir.path());
		let digest_hex = util::content_hash("payload to sign");
		let out = s.tool_sign(&serde_json::json!({"payload_hash": digest_hex}));
		assert!(!is_error(&out), "sign should succeed: {out}");
		let b = body(&out);

		let digest = gossip::gossip_contract::parse_key_hex(&digest_hex).unwrap();
		let pubkey = gossip::gossip_contract::parse_key_hex(b["pubkey"].as_str().unwrap()).unwrap();
		let sig_hex = b["tombstone_sig"]
			.as_str()
			.or(b["signature"].as_str())
			.unwrap();
		let sig: Vec<u8> = util::hex::decode(sig_hex).unwrap();
		assert!(
			verify_sig_by(&pubkey, &digest, &sig),
			"the returned signature verifies against the returned pubkey"
		);

		// The key never crosses the tool surface: only signature, pubkey, id.
		let keys: Vec<&String> = b.as_object().unwrap().keys().collect();
		assert_eq!(keys.len(), 3, "signature, pubkey, peer_id and nothing else");

		// Same daemon, second call: same identity (the key file persists).
		let again = body(&s.tool_sign(&serde_json::json!({"payload_hash": digest_hex})));
		assert_eq!(b["peer_id"], again["peer_id"]);
	}

	#[tokio::test]
	async fn sign_refuses_a_malformed_digest() {
		let dir = tempfile::tempdir().unwrap();
		let s = server_in(dir.path());
		assert!(is_error(
			&s.tool_sign(&serde_json::json!({"payload_hash": "short"}))
		));
		assert!(is_error(&s.tool_sign(&serde_json::json!({}))));
	}

	#[tokio::test]
	async fn contract_grant_amends_the_allowlist_and_moves_the_contract_id() {
		let dir = tempfile::tempdir().unwrap();
		let s = server_in(dir.path());

		// Make this daemon an owner: mint its key, then write a contract
		// naming that pubkey.
		let own_pubkey = body(&s.tool_sign(&serde_json::json!({
			"payload_hash": util::content_hash("x")
		})))["pubkey"]
			.as_str()
			.unwrap()
			.to_string();
		let contract = serde_json::json!({
			"kind": "signed-crdt-v0",
			"owners": [own_pubkey],
			"writers": "owners-only",
		});
		let grantee = PeerIdentity::from_bytes([42u8; 32]);
		let grantee_hex = util::hex::encode(grantee.pubkey());

		let out = s.tool_contract_grant(&serde_json::json!({
			"contract": contract,
			"pubkey": grantee_hex,
		}));
		assert!(!is_error(&out), "grant should succeed: {out}");
		let b = body(&out);
		assert_ne!(
			b["old_id"], b["new_id"],
			"amending params moves the key — the key IS the policy"
		);
		assert_eq!(b["amended_contract"]["writers"], "allowlist");
		assert!(b["amended_contract"]["writer_keys"]
			.as_array()
			.unwrap()
			.iter()
			.any(|k| k.as_str() == Some(grantee_hex.as_str())));

		// The tombstone signature verifies as an owner signature over (old, new).
		let old_id = gossip::gossip_contract::parse_key_hex(b["old_id"].as_str().unwrap()).unwrap();
		let new_id = gossip::gossip_contract::parse_key_hex(b["new_id"].as_str().unwrap()).unwrap();
		let owner_pk =
			gossip::gossip_contract::parse_key_hex(b["amended_contract"]["owners"][0].as_str().unwrap())
				.unwrap();
		let sig_hex = b["tombstone_sig"].as_str().unwrap();
		let sig: Vec<u8> = util::hex::decode(sig_hex).unwrap();
		assert!(verify_sig_by(
			&owner_pk,
			&tombstone_digest(&old_id, &new_id),
			&sig
		));
	}

	#[tokio::test]
	async fn contract_grant_refuses_a_non_owner_daemon() {
		let dir = tempfile::tempdir().unwrap();
		let s = server_in(dir.path());
		let stranger = PeerIdentity::from_bytes([9u8; 32]);
		let contract = serde_json::json!({
			"kind": "signed-crdt-v0",
			"owners": [util::hex::encode(stranger.pubkey())],
			"writers": "owners-only",
		});
		let out = s.tool_contract_grant(&serde_json::json!({
			"contract": contract,
			"pubkey": util::hex::encode(PeerIdentity::from_bytes([8u8; 32]).pubkey()),
		}));
		assert!(is_error(&out), "a non-owner daemon must not mint grants");
	}
}
