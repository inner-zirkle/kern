//! Tests extracted from gnn_graph.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;

	#[test]
	fn add_node_rejects_duplicate_ids() {
		let mut g = Graph::new();
		g.add_node("a", vec![1.0]).unwrap();
		assert!(matches!(g.add_node("a", vec![2.0]), Err(GraphError::DuplicateNode(id)) if id == "a"));
		assert_eq!(g.num_nodes(), 1, "the duplicate is not added");
	}

	#[test]
	fn add_edge_rejects_unknown_endpoints() {
		let mut g = Graph::new();
		g.add_node("a", vec![1.0]).unwrap();
		assert!(matches!(g.add_edge("a", "b"), Err(GraphError::NodeNotFound(id)) if id == "b"));
		assert!(matches!(g.add_edge("x", "a"), Err(GraphError::NodeNotFound(id)) if id == "x"));
		assert_eq!(g.edges.len(), 0);
	}

	#[test]
	fn add_self_loops_is_idempotent() {
		let mut g = Graph::new();
		g.add_node("a", vec![1.0]).unwrap();
		g.add_node("b", vec![1.0]).unwrap();
		g.add_edge("a", "b").unwrap();
		g.add_self_loops();
		let after_first = g.edges.len();
		g.add_self_loops();
		assert_eq!(
			g.edges.len(),
			after_first,
			"self-loops are not duplicated on re-run"
		);
		assert!(
			g.neighbors("a").contains(&"a".to_string()),
			"a has its self-loop"
		);
		assert!(
			g.neighbors("b").contains(&"b".to_string()),
			"b has its self-loop"
		);
	}

	#[test]
	fn normalized_adjacency_rows_sum_to_one_on_a_regular_graph() {
		let mut g = Graph::new();
		for id in ["a", "b", "c"] {
			g.add_node(id, vec![1.0]).unwrap();
		}
		for (s, t) in [
			("a", "b"),
			("b", "a"),
			("b", "c"),
			("c", "b"),
			("c", "a"),
			("a", "c"),
		] {
			g.add_edge(s, t).unwrap();
		}
		g.add_self_loops();

		let na = g.normalized_adjacency();
		let n = g.num_nodes();
		for i in 0..n {
			let row_sum: f64 = (0..n).map(|j| na.at(i, j)).sum();
			assert!(
				(row_sum - 1.0).abs() < 1e-9,
				"row {i} sums to {row_sum}, want 1.0"
			);
		}
	}
}
