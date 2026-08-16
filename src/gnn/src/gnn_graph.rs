//! The training-side graph view: nodes with feature vectors, weighted edges,
//! and the normalised sparse adjacency the GCN layers multiply through.

use std::collections::HashMap;

use crate::gnn_tensor::SparseMatrix;
use crate::gnn_tensor::Tensor;
use rayon::prelude::*;

#[derive(Debug, Clone)]
pub struct Node {
	pub id: String,
	pub features: Vec<f64>,
}

#[derive(Debug, Clone)]
pub struct Edge {
	pub source: String,
	pub target: String,
}

#[derive(Debug, Clone)]
pub struct Graph {
	pub nodes: Vec<Node>,
	pub edges: Vec<Edge>,
	adj_list: HashMap<String, Vec<String>>,
	in_list: HashMap<String, Vec<String>>,
	node_idx: HashMap<String, usize>,
}

impl Graph {
	pub fn new() -> Self {
		Self {
			nodes: Vec::new(),
			edges: Vec::new(),
			adj_list: HashMap::new(),
			in_list: HashMap::new(),
			node_idx: HashMap::new(),
		}
	}

	pub fn add_node(&mut self, id: &str, features: Vec<f64>) -> Result<(), GraphError> {
		if self.node_idx.contains_key(id) {
			return Err(GraphError::DuplicateNode(id.to_owned()));
		}
		self.node_idx.insert(id.to_owned(), self.nodes.len());
		self.nodes.push(Node {
			id: id.to_owned(),
			features,
		});
		Ok(())
	}

	pub fn add_edge(&mut self, source: &str, target: &str) -> Result<(), GraphError> {
		if !self.node_idx.contains_key(source) {
			return Err(GraphError::NodeNotFound(source.to_owned()));
		}
		if !self.node_idx.contains_key(target) {
			return Err(GraphError::NodeNotFound(target.to_owned()));
		}
		self.edges.push(Edge {
			source: source.to_owned(),
			target: target.to_owned(),
		});
		self
			.adj_list
			.entry(source.to_owned())
			.or_default()
			.push(target.to_owned());
		self
			.in_list
			.entry(target.to_owned())
			.or_default()
			.push(source.to_owned());
		Ok(())
	}

	pub fn neighbors(&self, id: &str) -> &[String] {
		self.adj_list.get(id).map(|v| v.as_slice()).unwrap_or(&[])
	}

	pub fn num_nodes(&self) -> usize {
		self.nodes.len()
	}

	pub fn feature_matrix(&self) -> Tensor {
		if self.nodes.is_empty() {
			return Tensor::zeros(0, 0);
		}
		let dim = self.nodes[0].features.len();
		let n = self.nodes.len();
		let mut data = vec![0.0; n * dim];
		for (i, node) in self.nodes.iter().enumerate() {
			data[i * dim..(i + 1) * dim].copy_from_slice(&node.features);
		}
		Tensor {
			data,
			rows: n,
			cols: dim,
		}
	}

	fn adjacency_matrix(&self) -> Tensor {
		let n = self.nodes.len();
		let mut adj = Tensor::zeros(n, n);
		for e in &self.edges {
			let i = self.node_idx[&e.source];
			let j = self.node_idx[&e.target];
			adj.set(i, j, 1.0);
		}
		adj
	}

	pub fn add_self_loops(&mut self) {
		for node in &self.nodes {
			let has = self
				.adj_list
				.get(&node.id)
				.map(|v| v.contains(&node.id))
				.unwrap_or(false);
			if !has {
				let id = node.id.clone();
				self.edges.push(Edge {
					source: id.clone(),
					target: id.clone(),
				});
				self
					.adj_list
					.entry(id.clone())
					.or_default()
					.push(id.clone());
				self.in_list.entry(id.clone()).or_default().push(id);
			}
		}
	}

	pub fn normalized_adjacency(&self) -> Tensor {
		let n = self.nodes.len();
		let adj = self.adjacency_matrix();
		let deg: Vec<f64> = (0..n)
			.into_par_iter()
			.map(|i| {
				let mut d = 0.0;
				for j in 0..n {
					d += adj.at(i, j);
				}
				d
			})
			.collect();
		let adj_ref = &adj;
		let deg_ref = &deg;
		let data: Vec<f64> = (0..n)
			.into_par_iter()
			.flat_map_iter(|i| {
				let di = deg_ref[i];
				(0..n).map(move |j| {
					let a = adj_ref.at(i, j);
					if a != 0.0 && di > 0.0 && deg_ref[j] > 0.0 {
						a / (di.sqrt() * deg_ref[j].sqrt())
					} else {
						0.0
					}
				})
			})
			.collect();
		Tensor {
			data,
			rows: n,
			cols: n,
		}
	}

	/// The same matrix as [`Graph::normalized_adjacency`], stored as its nonzeros.
	///
	/// Bit-identical to the dense form by construction: degree is the row's
	/// distinct-target count, which the dense version reaches by summing that many
	/// exact `1.0`s, and the entry is the same `1.0 / (sqrt(di) * sqrt(dj))`
	/// expression. Entries the dense version zeroes — a target whose own degree is
	/// zero — are dropped rather than stored, which the dense product treats as the
	/// `+0.0` term it is.
	pub fn normalized_adjacency_sparse(&self) -> SparseMatrix {
		let n = self.nodes.len();
		let mut targets: Vec<Vec<usize>> = vec![Vec::new(); n];
		for e in &self.edges {
			targets[self.node_idx[&e.source]].push(self.node_idx[&e.target]);
		}
		for t in &mut targets {
			t.sort_unstable();
			t.dedup();
		}
		let deg: Vec<f64> = targets.iter().map(|t| t.len() as f64).collect();
		let per_row: Vec<Vec<(usize, f64)>> = targets
			.par_iter()
			.enumerate()
			.map(|(i, t)| {
				let di = deg[i];
				if di <= 0.0 {
					return Vec::new();
				}
				t.iter()
					.filter(|&&j| deg[j] > 0.0)
					.map(|&j| (j, 1.0 / (di.sqrt() * deg[j].sqrt())))
					.collect()
			})
			.collect();
		SparseMatrix::from_rows(n, n, per_row)
	}
}

impl Default for Graph {
	fn default() -> Self {
		Self::new()
	}
}

#[derive(Debug, thiserror::Error)]
pub enum GraphError {
	#[error("duplicate node: {0}")]
	DuplicateNode(String),
	#[error("node not found: {0}")]
	NodeNotFound(String),
}

#[cfg(test)]
#[path = "tests/gnn_graph_test.rs"]
mod gnn_graph_tests;
