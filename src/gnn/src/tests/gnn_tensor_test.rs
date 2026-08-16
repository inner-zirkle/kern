//! Tests extracted from gnn_tensor.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;

	#[test]
	fn matmul_small_path_is_correct() {
		let a = Tensor::new(2, 3, vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]).unwrap();
		let b = Tensor::new(3, 2, vec![7.0, 8.0, 9.0, 10.0, 11.0, 12.0]).unwrap();
		let c = a.matmul(&b).unwrap();
		assert_eq!(c.shape(), (2, 2));
		assert_eq!(c.data, vec![58.0, 64.0, 139.0, 154.0]);
	}

	#[test]
	fn matmul_parallel_and_serial_paths_agree_at_the_threshold() {
		let t = Tensor::MATMUL_PAR_THRESHOLD;
		for &m in &[t - 1, t] {
			let out = Tensor::ones(m, 2).matmul(&Tensor::ones(2, 2)).unwrap();
			assert_eq!(out.shape(), (m, 2));
			assert!(
				out.data.iter().all(|v| (*v - 2.0).abs() < 1e-12),
				"m={m} entries all 2.0"
			);
		}
	}

	#[test]
	fn matmul_inner_dimension_mismatch_errors() {
		let a = Tensor::zeros(2, 3);
		let b = Tensor::zeros(2, 2);
		assert!(matches!(
			a.matmul(&b),
			Err(TensorError::InnerMismatch { lhs: 3, rhs: 2 })
		));
	}

	#[test]
	fn transpose_swaps_axes_and_elements() {
		let a = Tensor::new(2, 3, vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]).unwrap();
		let t = a.transpose();
		assert_eq!(t.shape(), (3, 2));
		assert_eq!(t.at(0, 1), 4.0);
		assert_eq!(t.at(2, 0), 3.0);
		assert_eq!(t.data, vec![1.0, 4.0, 2.0, 5.0, 3.0, 6.0]);
	}

	#[test]
	fn add_row_vec_broadcasts_and_validates_width() {
		let m = Tensor::new(2, 2, vec![1.0, 2.0, 3.0, 4.0]).unwrap();
		let r = Tensor::new(1, 2, vec![10.0, 20.0]).unwrap();
		let out = m.add_row_vec(&r).unwrap();
		assert_eq!(out.data, vec![11.0, 22.0, 13.0, 24.0]);
		let bad = Tensor::new(1, 3, vec![0.0, 0.0, 0.0]).unwrap();
		assert!(matches!(
			m.add_row_vec(&bad),
			Err(TensorError::ShapeMismatch { .. })
		));
	}

	#[test]
	fn row_extracts_a_1xn_slice() {
		let a = Tensor::new(2, 3, vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]).unwrap();
		let r = a.row(1);
		assert_eq!(r.shape(), (1, 3));
		assert_eq!(r.data, vec![4.0, 5.0, 6.0]);
	}

	#[test]
	fn debug_truncates_large_data() {
		let big = Tensor::zeros(10, 10);
		let s = format!("{big:?}");
		assert!(s.contains("10x10"));
		assert!(s.contains("(100 total)"), "preview is truncated: {s}");
	}
}
mod sparse_tests {
	use super::*;
	use crate::gnn_graph::Graph;

	/// `==` on f64 says `0.0 == -0.0`, which is the one difference a dense/sparse
	/// swap can actually introduce, so equality here is over the bit patterns.
	fn assert_bit_identical(a: &Tensor, b: &Tensor, what: &str) {
		assert_eq!((a.rows, a.cols), (b.rows, b.cols), "{what}: shape");
		for (i, (x, y)) in a.data.iter().zip(&b.data).enumerate() {
			assert_eq!(
				x.to_bits(),
				y.to_bits(),
				"{what}: element {i} differs: dense {x:e} ({:#x}) vs sparse {y:e} ({:#x})",
				x.to_bits(),
				y.to_bits()
			);
		}
	}

	/// Degree-2 ring plus self-loops: the shape ingest actually produces, where
	/// `add_similarity_reason` gives each entity one similarity edge and
	/// `build_gnn_snapshot` adds the reverse.
	fn ring(n: usize) -> Graph {
		let mut g = Graph::new();
		for i in 0..n {
			g.add_node(&format!("n{i}"), vec![i as f64]).unwrap();
		}
		for i in 0..n {
			g.add_edge(&format!("n{i}"), &format!("n{}", (i + 1) % n))
				.unwrap();
			g.add_edge(&format!("n{}", (i + 1) % n), &format!("n{i}"))
				.unwrap();
		}
		g.add_self_loops();
		g
	}

	/// Every pair connected. The trap this closes: a graph dense enough that the
	/// two paths coincide would let a broken sparse path pass, so the equivalence
	/// is asserted at both ends of the density range.
	fn complete(n: usize) -> Graph {
		let mut g = Graph::new();
		for i in 0..n {
			g.add_node(&format!("n{i}"), vec![i as f64]).unwrap();
		}
		for i in 0..n {
			for j in 0..n {
				if i != j {
					g.add_edge(&format!("n{i}"), &format!("n{j}")).unwrap();
				}
			}
		}
		g.add_self_loops();
		g
	}

	/// No self-loops, so `n{n-1}` has in-edges and no out-edges: degree zero. The
	/// dense builder writes 0.0 into every column that lands on it, and the sparse
	/// builder has to drop exactly those and no others.
	fn with_a_sink(n: usize) -> Graph {
		let mut g = Graph::new();
		for i in 0..n {
			g.add_node(&format!("n{i}"), vec![i as f64]).unwrap();
		}
		for i in 0..n - 1 {
			g.add_edge(&format!("n{i}"), &format!("n{}", i + 1))
				.unwrap();
		}
		g
	}

	fn features(n: usize, d: usize) -> Tensor {
		let data = (0..n * d)
			.map(|k| ((k as f64) * 0.37).sin() * (k as f64 + 1.0).ln())
			.collect();
		Tensor::new(n, d, data).unwrap()
	}

	#[test]
	fn sparse_normalized_adjacency_is_bit_identical_to_dense() {
		for g in [
			ring(8),
			ring(96),
			complete(8),
			complete(96),
			with_a_sink(8),
			with_a_sink(96),
		] {
			let dense = g.normalized_adjacency();
			let sparse = g.normalized_adjacency_sparse();
			assert_eq!((sparse.rows, sparse.cols), (dense.rows, dense.cols));
			assert_bit_identical(&dense, &sparse.to_dense(), "normalized adjacency");
		}
	}

	#[test]
	fn sparse_storage_actually_skips_the_zeros() {
		let g = ring(96);
		let sparse = g.normalized_adjacency_sparse();
		assert_eq!(
			sparse.nnz(),
			96 * 3,
			"a degree-2 ring with self-loops stores 3 entries per row, not 96"
		);
		assert_eq!(
			complete(96).normalized_adjacency_sparse().nnz(),
			96 * 96,
			"a complete graph stores every entry, so the dense case is really covered"
		);
	}

	// Both `Tensor::matmul` branches are exercised: 8 rows takes the serial path,
	// 96 rows is over MATMUL_PAR_THRESHOLD and takes the rayon one.
	#[test]
	fn sparse_and_dense_products_are_bit_identical() {
		for g in [
			ring(8),
			ring(96),
			complete(8),
			complete(96),
			with_a_sink(8),
			with_a_sink(96),
		] {
			let n = g.num_nodes();
			let dense = g.normalized_adjacency();
			let sparse = g.normalized_adjacency_sparse();

			// 384 is the production embedding width; 1/5/17 are there so an
			// accidental dependence on a nice width would show.
			for d in [1usize, 5, 17, 384] {
				let x = features(n, d);
				assert_bit_identical(
					&dense.matmul(&x).unwrap(),
					&sparse.matmul(&x).unwrap(),
					"forward aggregation",
				);
				assert_bit_identical(
					&dense.transpose().matmul(&x).unwrap(),
					&sparse.transpose().matmul(&x).unwrap(),
					"backward aggregation",
				);
			}
		}
	}

	#[test]
	fn matmul_inner_dimension_mismatch_errors() {
		let s = SparseMatrix::from_rows(2, 3, vec![vec![(0, 1.0)], vec![(2, 1.0)]]);
		assert!(matches!(
			s.matmul(&Tensor::zeros(2, 2)),
			Err(TensorError::InnerMismatch { lhs: 3, rhs: 2 })
		));
	}
}
