use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ResolveReq {
	pub root: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ResolveRes {
	pub ok: bool,
	#[serde(default)]
	pub endpoint: String,
	#[serde(default)]
	pub spawned: bool,
	#[serde(default)]
	pub err: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NodeLite {
	pub root: String,
	pub endpoint: String,
	pub pid: u32,
	pub alive: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HubStatusRes {
	pub ok: bool,
	#[serde(default)]
	pub nodes: Vec<NodeLite>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StopRes {
	pub ok: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UnloadReq {
	pub root: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UnloadRes {
	pub ok: bool,
	#[serde(default)]
	pub existed: bool,
	#[serde(default)]
	pub err: String,
}
