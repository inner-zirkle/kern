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
