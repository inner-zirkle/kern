//! A minimal row-major 2-D tensor with the shape-checked ops the GNN needs;
//! errors, not panics, on dimension mismatch.

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TensorError {
	#[error("shape mismatch: expected ({er},{ec}), got ({ar},{ac})", er = .expected.0, ec = .expected.1, ar = .actual.0, ac = .actual.1)]
	ShapeMismatch {
		expected: (usize, usize),
		actual: (usize, usize),
	},
	#[error("inner dimension mismatch: {lhs} vs {rhs}")]
	InnerMismatch { lhs: usize, rhs: usize },
	#[error("data length {len} does not match shape ({rows}, {cols})")]
	DataLength {
		len: usize,
		rows: usize,
		cols: usize,
	},
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Tensor {
	pub data: Vec<f64>,
	pub rows: usize,
	pub cols: usize,
}

/// Manual `Debug` (not derived): print the shape and only a short data preview
/// so logging a large weight tensor doesn't dump thousands of floats.
impl std::fmt::Debug for Tensor {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		const PREVIEW: usize = 8;
		write!(f, "Tensor {{ {}x{}, data: [", self.rows, self.cols)?;
		for (i, v) in self.data.iter().take(PREVIEW).enumerate() {
			if i > 0 {
				write!(f, ", ")?;
			}
			write!(f, "{v}")?;
		}
		if self.data.len() > PREVIEW {
			write!(f, ", … ({} total)", self.data.len())?;
		}
		write!(f, "] }}")
	}
}

impl Tensor {
	pub fn new(rows: usize, cols: usize, data: Vec<f64>) -> Result<Self, TensorError> {
		if data.len() != rows * cols {
			return Err(TensorError::DataLength {
				len: data.len(),
				rows,
				cols,
			});
		}
		Ok(Self { data, rows, cols })
	}

	pub fn zeros(rows: usize, cols: usize) -> Self {
		Self {
			data: vec![0.0; rows * cols],
			rows,
			cols,
		}
	}

	pub fn fill(&mut self, v: f64) {
		self.data.iter_mut().for_each(|x| *x = v);
	}

	pub fn ones(rows: usize, cols: usize) -> Self {
		Self {
			data: vec![1.0; rows * cols],
			rows,
			cols,
		}
	}

	pub fn rand_with<R: rand::Rng>(rows: usize, cols: usize, scale: f64, rng: &mut R) -> Self {
		use rand::RngExt;
		let data: Vec<f64> = (0..rows * cols)
			.map(|_| {
				let u1: f64 = rng.random_range(1e-10..1.0);
				let u2: f64 = rng.random_range(0.0..std::f64::consts::TAU);
				(-2.0 * u1.ln()).sqrt() * u2.cos() * scale
			})
			.collect();
		Self { data, rows, cols }
	}

	#[inline]
	pub fn at(&self, row: usize, col: usize) -> f64 {
		self.data[row * self.cols + col]
	}

	#[inline]
	pub fn set(&mut self, row: usize, col: usize, val: f64) {
		self.data[row * self.cols + col] = val;
	}

	#[inline]
	pub fn shape(&self) -> (usize, usize) {
		(self.rows, self.cols)
	}

	const MATMUL_PAR_THRESHOLD: usize = 64;

	pub fn matmul(&self, other: &Tensor) -> Result<Tensor, TensorError> {
		if self.cols != other.rows {
			return Err(TensorError::InnerMismatch {
				lhs: self.cols,
				rhs: other.rows,
			});
		}
		let (m, k, n) = (self.rows, self.cols, other.cols);
		let mut out = vec![0.0; m * n];
		let a = &self.data;
		let b = &other.data;

		if m >= Self::MATMUL_PAR_THRESHOLD {
			out.par_chunks_mut(n).enumerate().for_each(|(i, row)| {
				for p in 0..k {
					let a_ip = a[i * k + p];
					let b_row = p * n;
					for j in 0..n {
						row[j] += a_ip * b[b_row + j];
					}
				}
			});
		} else {
			for i in 0..m {
				for p in 0..k {
					let a_ip = a[i * k + p];
					let out_row = i * n;
					let b_row = p * n;
					for j in 0..n {
						out[out_row + j] += a_ip * b[b_row + j];
					}
				}
			}
		}

		Ok(Tensor {
			data: out,
			rows: m,
			cols: n,
		})
	}

	pub fn transpose(&self) -> Tensor {
		let mut out = Tensor::zeros(self.cols, self.rows);
		for i in 0..self.rows {
			for j in 0..self.cols {
				out.data[j * self.rows + i] = self.data[i * self.cols + j];
			}
		}
		out
	}

	pub fn apply(&self, f: impl Fn(f64) -> f64) -> Tensor {
		Tensor {
			data: self.data.iter().map(|v| f(*v)).collect(),
			rows: self.rows,
			cols: self.cols,
		}
	}

	pub fn add_row_vec(&self, vec: &Tensor) -> Result<Tensor, TensorError> {
		if vec.rows != 1 || vec.cols != self.cols {
			return Err(TensorError::ShapeMismatch {
				expected: (1, self.cols),
				actual: (vec.rows, vec.cols),
			});
		}
		let mut out = self.clone();
		for i in 0..self.rows {
			for j in 0..self.cols {
				out.data[i * self.cols + j] += vec.data[j];
			}
		}
		Ok(out)
	}

	pub fn row(&self, i: usize) -> Tensor {
		let start = i * self.cols;
		Tensor {
			data: self.data[start..start + self.cols].to_vec(),
			rows: 1,
			cols: self.cols,
		}
	}

	pub fn sum_all(&self) -> f64 {
		self.data.iter().sum()
	}

	pub fn add_inplace(&mut self, other: &Tensor) -> Result<(), TensorError> {
		self.check_shape(other)?;
		for (a, b) in self.data.iter_mut().zip(&other.data) {
			*a += *b;
		}
		Ok(())
	}

	fn check_shape(&self, other: &Tensor) -> Result<(), TensorError> {
		if self.rows != other.rows || self.cols != other.cols {
			return Err(TensorError::ShapeMismatch {
				expected: (self.rows, self.cols),
				actual: (other.rows, other.cols),
			});
		}
		Ok(())
	}
}

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

#[cfg(test)]
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
