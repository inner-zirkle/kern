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
#[path = "tests/gnn_tensor_test.rs"]
mod gnn_tensor_tests;
