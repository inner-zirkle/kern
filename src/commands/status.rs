//! Who is serving and who is writing this directory.
//!
//! `kern health` describes the graph. This describes the *processes* around it,
//! which is the question asked right before running an offline admin command —
//! and the one that had no answer when a respawned hub flushed its stale graph
//! over a completed re-embed.

use crate::transport::kern_rpc::KernRpcClient;
use crate::transport::typed::{Endpoint, JsonEnvelopeCodec};

pub(super) async fn cmd_status(cfg: &crate::config::Config) {
	let kern_ep = Endpoint::kern();
	let hub_ep = Endpoint::hub();

	println!("data dir     {}", cfg.data_dir);
	println!("kern socket  {}", kern_ep.display());

	let caller = crate::rpc::caller_of(cfg);
	let daemon = probe(&kern_ep, &caller).await;
	match &daemon {
		Some(h) => println!(
			"daemon       serving  ({} kerns, {} entities, idle {}s)",
			h.kerns,
			h.entities,
			h.idle_ms / 1000
		),
		None => println!("daemon       not serving this directory"),
	}

	match probe(&hub_ep, &caller).await {
		Some(_) => println!("hub          running   {}", hub_ep.display()),
		None => println!("hub          not running"),
	}

	// Read AFTER the probes: a daemon that answers but holds no lock is the
	// state worth seeing, and it is exactly what an older binary produces.
	match crate::base::lock::holder(&cfg.data_dir) {
		Some(who) => {
			println!("writer lock  held by {who}");
			println!();
			println!("Offline admin commands (reembed, compact, gc) will refuse while it is held.");
		}
		None => {
			println!("writer lock  free");
			if daemon.is_some() {
				println!();
				println!(
					"A daemon is serving but holds no writer lock — it predates the lock, or could not \
					 take it. Offline admin commands will NOT be refused; stop it before running one."
				);
			}
		}
	}
}

// One attempt, no retry: status must answer instantly when nothing is there.
// A caller the daemon refuses reads as "not serving" here, the same as one that
// found nothing — this line describes reachability, and an unreachable daemon is
// unreachable either way. `route` is where the distinction has teeth.
async fn probe(
	ep: &Endpoint,
	auth: &crate::transport::kern_rpc::AuthReq,
) -> Option<crate::transport::kern_rpc::HealthRes> {
	KernRpcClient::<JsonEnvelopeCodec>::connect_endpoint_with_retry(
		ep,
		auth,
		1,
		std::time::Duration::ZERO,
	)
	.await
	.ok()?
	.health()
	.await
	.ok()
	.filter(|h| h.ok)
}
