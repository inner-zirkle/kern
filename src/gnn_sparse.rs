//! CSR sparse matrix with rayon-parallel dense multiply — the adjacency
//! representation the GCN forward/backward passes run through.

use rayon::prelude::*;

use crate::gnn_tensor::{Tensor, TensorError};

/// Compressed sparse rows: `row_start[i]..row_start[i+1]` indexes `col`/`val`.
///
/// Columns ascend within a row, and that is load-bearing rather than tidiness.
/// [`SparseMatrix::matmul`] accumulates an output row by visiting its stored
/// columns in order, which is the order `Tensor::matmul` visits the same
/// nonzeros in; the terms the dense loop visits in between are exactly the
/// stored zeros, and adding `a * b` with `a == 0.0` leaves a `+0.0`-seeded
/// accumulator bit-unchanged. So the two products agree bit for bit, which is
/// what `sparse_and_dense_products_are_bit_identical` asserts.
pub struct SparseMatrix {
	pub rows: usize,
	pub cols: usize,
	row_start: Vec<usize>,
	col: Vec<usize>,
	val: Vec<f64>,
}

impl SparseMatrix {
	/// `per_row[i]` are the nonzeros of row `i`; each row is sorted here so no
	/// caller can hand over an ordering the bit-identity argument does not hold for.
	pub fn from_rows(rows: usize, cols: usize, mut per_row: Vec<Vec<(usize, f64)>>) -> Self {
		per_row.resize_with(rows, Vec::new);
		let nnz: usize = per_row.iter().map(|r| r.len()).sum();
		let mut row_start = Vec::with_capacity(rows + 1);
		let mut col = Vec::with_capacity(nnz);
		let mut val = Vec::with_capacity(nnz);
		row_start.push(0);
		for r in &mut per_row {
			r.sort_unstable_by_key(|&(j, _)| j);
			for &(j, v) in r.iter() {
				col.push(j);
				val.push(v);
			}
			row_start.push(col.len());
		}
		Self {
			rows,
			cols,
			row_start,
			col,
			val,
		}
	}

	pub fn nnz(&self) -> usize {
		self.val.len()
	}

	pub fn matmul(&self, other: &Tensor) -> Result<Tensor, TensorError> {
		if self.cols != other.rows {
			return Err(TensorError::InnerMismatch {
				lhs: self.cols,
				rhs: other.rows,
			});
		}
		let n = other.cols;
		let mut out = vec![0.0; self.rows * n];
		let starts = &self.row_start;
		out
			.par_chunks_mut(n.max(1))
			.enumerate()
			.for_each(|(i, row)| {
				for k in starts[i]..starts[i + 1] {
					let a = self.val[k];
					let b = &other.data[self.col[k] * n..(self.col[k] + 1) * n];
					for (o, bv) in row.iter_mut().zip(b) {
						*o += a * bv;
					}
				}
			});
		Ok(Tensor {
			data: out,
			rows: self.rows,
			cols: n,
		})
	}

	/// Counting sort by column, filling each transposed row in ascending source-row
	/// order — so the transpose keeps the ascending-column invariant for free.
	pub fn transpose(&self) -> SparseMatrix {
		let mut row_start = vec![0usize; self.cols + 1];
		for &j in &self.col {
			row_start[j + 1] += 1;
		}
		for k in 0..self.cols {
			row_start[k + 1] += row_start[k];
		}
		let mut fill = row_start.clone();
		let mut col = vec![0usize; self.col.len()];
		let mut val = vec![0.0; self.val.len()];
		for (i, w) in self.row_start.windows(2).enumerate() {
			for k in w[0]..w[1] {
				let dst = fill[self.col[k]];
				col[dst] = i;
				val[dst] = self.val[k];
				fill[self.col[k]] += 1;
			}
		}
		SparseMatrix {
			rows: self.cols,
			cols: self.rows,
			row_start,
			col,
			val,
		}
	}

	pub fn to_dense(&self) -> Tensor {
		let mut t = Tensor::zeros(self.rows, self.cols);
		for (i, w) in self.row_start.windows(2).enumerate() {
			for k in w[0]..w[1] {
				t.set(i, self.col[k], self.val[k]);
			}
		}
		t
	}
}

#[cfg(test)]
mod tests {
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
