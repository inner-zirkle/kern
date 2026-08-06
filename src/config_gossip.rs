use serde::{Deserialize, Serialize};

use crate::base::constants::{GOSSIP_MAX_PEERS, GOSSIP_SEED_ADDR};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GossipConfig {
	pub enabled: bool,
	pub addr: String,
	pub discovery: bool,
	pub network_id: Option<String>,
	pub discovery_port: u16,
	pub peers: Vec<String>,
	pub seed: bool,
	pub seed_addr: String,
	// Small-world ring topology (FEDERATION_PLAN §2). Off = legacy flat peers.
	pub ring: bool,
	// Path of the ed25519 peer key file; empty = <data_dir>/peer.key.
	pub identity_path: String,
	// Anti-entropy cadence for contract kerns (FEDERATION_PLAN §4).
	pub sync_interval_secs: u64,
	// Contract ids (hex) to subscribe to on boot.
	pub subscriptions: Vec<String>,
	// Contracts this node hosts/owns.
	pub contracts: Vec<ContractConfig>,
}

// One `[[gossip.contracts]]` table: the policy whose hash is the contract key
// (FEDERATION_PLAN §3). Keys are hex-encoded ed25519 public keys.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContractConfig {
	pub kind: String,
	pub owners: Vec<String>,
	// "open" | "owners-only" | allowlist implied by non-empty `writers` list.
	pub writers: String,
	pub writer_keys: Vec<String>,
	pub kinds: Vec<String>,
	pub max_entities: u32,
	pub retention_secs: Option<u64>,
}

impl Default for ContractConfig {
	fn default() -> Self {
		Self {
			kind: "signed-crdt-v0".into(),
			owners: Vec::new(),
			writers: "owners-only".into(),
			writer_keys: Vec::new(),
			kinds: Vec::new(),
			max_entities: crate::base::constants::GOSSIP_REMOTE_KERN_ENTITY_CAP as u32,
			retention_secs: None,
		}
	}
}

impl GossipConfig {
	pub fn effective_seed(&self) -> Option<&str> {
		if !self.enabled || !self.seed {
			return None;
		}
		let addr = self.seed_addr.trim();
		(!addr.is_empty()).then_some(addr)
	}

	// The only peer source that runs before any inbound contact; still bounded by GOSSIP_MAX_PEERS.
	pub fn bootstrap_peers(&self) -> Vec<String> {
		if !self.enabled {
			return Vec::new();
		}
		let mut peers = self.peers.clone();
		if let Some(seed) = self.effective_seed() {
			if !peers.iter().any(|p| p == seed) {
				peers.push(seed.to_string());
			}
		}
		peers.truncate(GOSSIP_MAX_PEERS);
		peers
	}

	// A ':' in the id would corrupt the `kern:<id>:<addr>` announce wire format.
	pub fn effective_network_id(&self, generated: &str) -> String {
		match self.network_id.as_deref() {
			Some(id) if !id.is_empty() && !id.contains(':') => id.to_string(),
			Some(id) if !id.is_empty() => {
				tracing::warn!(
					target: "kern.gossip",
					network_id = %id,
					"[gossip] network_id must not contain ':'; falling back to the generated id"
				);
				generated.to_string()
			}
			_ => generated.to_string(),
		}
	}
}

impl Default for GossipConfig {
	fn default() -> Self {
		Self {
			enabled: false,
			addr: "0.0.0.0:7400".into(),
			discovery: true,
			network_id: None,
			discovery_port: 7475,
			peers: Vec::new(),
			// Dialing a public host is opt-in: federation is unauthenticated, so a
			// default-on seed would auto-join a stranger's network.
			seed: false,
			seed_addr: GOSSIP_SEED_ADDR.into(),
			// Ring routing is phase-gated off until a network opts in.
			ring: false,
			identity_path: String::new(),
			sync_interval_secs: 300,
			subscriptions: Vec::new(),
			contracts: Vec::new(),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn default_is_disabled_with_expected_field_values() {
		let c = GossipConfig::default();
		assert!(!c.enabled, "gossip is disabled by default");
		assert_eq!(c.addr, "0.0.0.0:7400");
		assert!(
			c.discovery,
			"discovery defaults on (only matters once enabled)"
		);
		assert_eq!(c.discovery_port, 7475);
		assert!(
			c.network_id.is_none(),
			"no pooling id by default — each daemon keeps its unique generated id"
		);
		assert!(c.peers.is_empty(), "no seed peers by default");
		assert!(!c.ring, "ring topology is opt-in (phase 2 switch)");
		assert!(
			c.identity_path.is_empty(),
			"peer key defaults beside the graph it identifies"
		);
		assert_eq!(c.sync_interval_secs, 300);
		assert!(c.subscriptions.is_empty(), "no boot subscriptions by default");
		assert!(c.contracts.is_empty(), "no hosted contracts by default");
		assert!(
			!c.seed,
			"dialing the public seed is opt-in, never a default"
		);
		assert_eq!(c.seed_addr, GOSSIP_SEED_ADDR);
	}

	#[test]
	fn disabled_gossip_bootstraps_nothing_at_all() {
		// `seed: true` on purpose: the disabled gate, not the seed default, is what
		// must silence this — otherwise the test passes for the wrong reason.
		let c = GossipConfig {
			seed: true,
			peers: vec!["10.0.0.5:7400".into()],
			..GossipConfig::default()
		};
		assert!(!c.enabled);
		assert_eq!(
			c.effective_seed(),
			None,
			"a default daemon must make zero outbound calls"
		);
		assert!(c.bootstrap_peers().is_empty());
	}

	#[test]
	fn enabling_gossip_alone_dials_nothing() {
		let c = GossipConfig {
			enabled: true,
			..GossipConfig::default()
		};
		assert_eq!(
			c.effective_seed(),
			None,
			"turning gossip on must not, by itself, dial the public seed"
		);
		assert!(c.bootstrap_peers().is_empty());
	}

	#[test]
	fn opting_into_the_seed_dials_the_default_addr() {
		let c = GossipConfig {
			enabled: true,
			seed: true,
			..GossipConfig::default()
		};
		assert_eq!(c.effective_seed(), Some(GOSSIP_SEED_ADDR));
		assert_eq!(c.bootstrap_peers(), vec![GOSSIP_SEED_ADDR.to_string()]);
	}

	#[test]
	fn an_explicit_seed_overrides_the_default() {
		let c = GossipConfig {
			enabled: true,
			seed: true,
			seed_addr: "seed.internal:7946".into(),
			..GossipConfig::default()
		};
		assert_eq!(c.effective_seed(), Some("seed.internal:7946"));
		assert!(!c.bootstrap_peers().iter().any(|p| p == GOSSIP_SEED_ADDR));
	}

	#[test]
	fn the_seed_turns_off_while_gossip_stays_on() {
		let mut c = GossipConfig {
			enabled: true,
			seed: false,
			peers: vec!["10.0.0.5:7400".into()],
			..GossipConfig::default()
		};
		assert_eq!(c.effective_seed(), None, "air-gapped LAN never phones out");
		assert_eq!(c.bootstrap_peers(), vec!["10.0.0.5:7400".to_string()]);

		c.seed = true;
		c.seed_addr = "   ".into();
		assert_eq!(
			c.effective_seed(),
			None,
			"a blank seed_addr also disables it"
		);
	}

	#[test]
	fn bootstrap_peers_never_exceed_the_peer_cap() {
		let c = GossipConfig {
			enabled: true,
			peers: (0..GOSSIP_MAX_PEERS + 10)
				.map(|i| format!("10.0.0.{i}:7400"))
				.collect(),
			..GossipConfig::default()
		};
		assert_eq!(c.bootstrap_peers().len(), GOSSIP_MAX_PEERS);
	}

	#[test]
	fn a_seed_already_in_peers_is_not_duplicated() {
		let c = GossipConfig {
			enabled: true,
			seed: true,
			peers: vec![GOSSIP_SEED_ADDR.into()],
			..GossipConfig::default()
		};
		assert_eq!(c.bootstrap_peers(), vec![GOSSIP_SEED_ADDR.to_string()]);
	}

	#[test]
	fn effective_network_id_prefers_a_valid_configured_id() {
		let c = GossipConfig {
			network_id: Some("team-alpha".into()),
			..GossipConfig::default()
		};
		assert_eq!(c.effective_network_id("generated-uuid"), "team-alpha");
	}

	#[test]
	fn effective_network_id_falls_back_when_unset_empty_or_invalid() {
		let mut c = GossipConfig::default();
		assert_eq!(c.effective_network_id("gen"), "gen", "unset -> generated");
		c.network_id = Some(String::new());
		assert_eq!(c.effective_network_id("gen"), "gen", "empty -> generated");
		c.network_id = Some("has:colon".into());
		assert_eq!(
			c.effective_network_id("gen"),
			"gen",
			"':' would corrupt the announce wire format"
		);
	}
}
