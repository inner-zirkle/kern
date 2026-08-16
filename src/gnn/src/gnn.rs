//! The hand-rolled GNN stack in one place: activations, dense and graph
//! layers with their backward passes, layer norm, link-prediction loss, Adam,
//! the assembled model, and versioned weight persistence. Tensors live in
//! `gnn_tensor`, the training graph in `gnn_graph`, propagation in
//! `gnn_propagate`.

#[derive(Debug, thiserror::Error)]
pub enum GnnError {
	#[error("gnn: missing forward state ({0}); call forward_graph before backward/inference")]
	MissingForwardState(&'static str),

	#[error("gnn: tensor error: {0}")]
	Tensor(#[from] crate::gnn_tensor::TensorError),
}

pub use crate::gnn_graph as graph;
pub use crate::gnn_propagate as propagate;
pub use crate::gnn_tensor as tensor;

// ==== [activation] ====

#[inline]
pub fn relu(x: f64) -> f64 {
	x.max(0.0)
}

#[inline]
pub fn relu_deriv(x: f64) -> f64 {
	if x > 0.0 {
		1.0
	} else {
		0.0
	}
}

#[inline]
pub fn sigmoid(x: f64) -> f64 {
	1.0 / (1.0 + (-x).exp())
}

#[inline]
pub fn sigmoid_deriv(x: f64) -> f64 {
	let s = sigmoid(x);
	s * (1.0 - s)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Activation {
	Relu,
	Sigmoid,
}

impl Activation {
	#[inline]
	pub fn forward(self, x: f64) -> f64 {
		match self {
			Activation::Relu => relu(x),
			Activation::Sigmoid => sigmoid(x),
		}
	}

	#[inline]
	pub fn deriv(self, x: f64) -> f64 {
		match self {
			Activation::Relu => relu_deriv(x),
			Activation::Sigmoid => sigmoid_deriv(x),
		}
	}
}

// ==== [layer] ====

use crate::gnn_tensor::Tensor;

pub trait Layer {
	fn forward(&mut self, input: &Tensor) -> Tensor;
	fn parameters(&self) -> Vec<&Tensor>;
	fn parameters_mut(&mut self) -> Vec<&mut Tensor>;
}

pub trait Backward {
	fn backward(&mut self, d_out: &Tensor) -> Tensor;
	fn param_grads(&self) -> Vec<&Tensor>;
	fn param_grads_mut(&mut self) -> Vec<&mut Tensor>;
	fn zero_grads(&mut self);
}

pub struct LinearLayer {
	pub weight: Tensor, // (in_features, out_features)
	pub bias: Tensor,   // (1, out_features)
	last_input: Option<Tensor>,
	d_weight: Tensor,
	d_bias: Tensor,
}

impl LinearLayer {
	pub fn new(in_features: usize, out_features: usize) -> Self {
		let mut rng = rand::rng();
		Self::with_rng(in_features, out_features, &mut rng)
	}

	pub fn with_rng<R: rand::Rng>(in_features: usize, out_features: usize, rng: &mut R) -> Self {
		let scale = (2.0 / (in_features + out_features) as f64).sqrt();
		let weight = Tensor::rand_with(in_features, out_features, scale, rng);
		let bias = Tensor::zeros(1, out_features);
		let d_weight = Tensor::zeros(in_features, out_features);
		let d_bias = Tensor::zeros(1, out_features);
		Self {
			weight,
			bias,
			last_input: None,
			d_weight,
			d_bias,
		}
	}

	pub fn try_forward(&mut self, input: &Tensor) -> Result<Tensor, GnnError> {
		let out = input.matmul(&self.weight)?.add_row_vec(&self.bias)?;
		self.last_input = Some(input.clone());
		Ok(out)
	}

	pub fn try_backward(&mut self, d_out: &Tensor) -> Result<Tensor, GnnError> {
		let input = self
			.last_input
			.as_ref()
			.ok_or(GnnError::MissingForwardState("linear::last_input"))?;
		let dw = input.transpose().matmul(d_out)?;
		self.d_weight.add_inplace(&dw)?;
		for i in 0..d_out.rows {
			for j in 0..d_out.cols {
				self.d_bias.data[j] += d_out.at(i, j);
			}
		}
		Ok(d_out.matmul(&self.weight.transpose())?)
	}
}

impl Layer for LinearLayer {
	fn forward(&mut self, input: &Tensor) -> Tensor {
		match self.try_forward(input) {
			Ok(t) => t,
			Err(e) => {
				tracing::debug!(error = %e, "LinearLayer forward failed; returning zero activations");
				self.last_input = None;
				Tensor::zeros(input.rows, self.weight.cols)
			}
		}
	}

	fn parameters(&self) -> Vec<&Tensor> {
		vec![&self.weight, &self.bias]
	}

	fn parameters_mut(&mut self) -> Vec<&mut Tensor> {
		vec![&mut self.weight, &mut self.bias]
	}
}

impl Backward for LinearLayer {
	fn backward(&mut self, d_out: &Tensor) -> Tensor {
		match self.try_backward(d_out) {
			Ok(t) => t,
			Err(e) => {
				tracing::debug!(error = %e, "LinearLayer backward failed; returning zero gradient");
				// dInput is (n_samples, in_features); in_features == weight.rows.
				Tensor::zeros(d_out.rows, self.weight.rows)
			}
		}
	}

	fn param_grads(&self) -> Vec<&Tensor> {
		vec![&self.d_weight, &self.d_bias]
	}

	fn param_grads_mut(&mut self) -> Vec<&mut Tensor> {
		vec![&mut self.d_weight, &mut self.d_bias]
	}

	fn zero_grads(&mut self) {
		self.d_weight.fill(0.0);
		self.d_bias.fill(0.0);
	}
}

// ==== [backward] ====

use crate::gnn_graph::Graph;

pub fn act_deriv_mul(act: Activation, d_out: &Tensor, pre_act: &Tensor) -> Tensor {
	let mut out = Tensor::zeros(d_out.rows, d_out.cols);
	for (i, &x) in pre_act.data.iter().enumerate() {
		out.data[i] = d_out.data[i] * act.deriv(x);
	}
	out
}

pub trait GraphLayer {
	fn forward_graph(&mut self, g: &Graph, features: &Tensor) -> Tensor;
	fn parameters(&self) -> Vec<&Tensor>;
	fn parameters_mut(&mut self) -> Vec<&mut Tensor>;
}

pub trait BackwardGraphLayer: GraphLayer {
	fn backward_graph(&mut self, g: &Graph, d_out: &Tensor) -> Tensor;
	fn param_grads(&self) -> Vec<&Tensor>;
	fn param_grads_mut(&mut self) -> Vec<&mut Tensor>;
	fn zero_grads(&mut self);
}

// ==== [gcn] ====

use crate::gnn_tensor::SparseMatrix;

pub struct GCNLayer {
	pub linear: LinearLayer,
	pub norm: Option<LayerNorm>,
	pub act: Option<Activation>,
	last_norm_adj: Option<SparseMatrix>,
	last_pre_act: Option<Tensor>,
}

impl GCNLayer {
	pub fn new(in_features: usize, out_features: usize, act: Option<Activation>, norm: bool) -> Self {
		let mut rng = rand::rng();
		Self::with_rng(in_features, out_features, act, norm, &mut rng)
	}

	pub fn with_rng<R: rand::Rng>(
		in_features: usize,
		out_features: usize,
		act: Option<Activation>,
		norm: bool,
		rng: &mut R,
	) -> Self {
		Self {
			linear: LinearLayer::with_rng(in_features, out_features, rng),
			norm: if norm {
				Some(LayerNorm::new(out_features))
			} else {
				None
			},
			act,
			last_norm_adj: None,
			last_pre_act: None,
		}
	}

	pub fn try_forward_graph(&mut self, g: &Graph, features: &Tensor) -> Result<Tensor, GnnError> {
		let norm_adj = g.normalized_adjacency_sparse();
		let agg = norm_adj.matmul(features)?;
		self.last_norm_adj = Some(norm_adj);

		let mut h = self.linear.try_forward(&agg)?;
		if let Some(ref mut n) = self.norm {
			h = n.forward(&h);
		}
		self.last_pre_act = Some(h.clone());
		if let Some(a) = self.act {
			h = h.apply(|x| a.forward(x));
		}
		Ok(h)
	}

	pub fn try_backward_graph(&mut self, _g: &Graph, d_out: &Tensor) -> Result<Tensor, GnnError> {
		let norm_adj = self
			.last_norm_adj
			.as_ref()
			.ok_or(GnnError::MissingForwardState("gcn::last_norm_adj"))?
			.transpose();
		let mut grad = d_out.clone();
		if let Some(a) = self.act {
			let pre_act = self
				.last_pre_act
				.as_ref()
				.ok_or(GnnError::MissingForwardState("gcn::last_pre_act"))?;
			grad = act_deriv_mul(a, &grad, pre_act);
		}
		if let Some(ref mut n) = self.norm {
			grad = n.try_backward(&grad)?;
		}
		let d_agg = self.linear.try_backward(&grad)?;
		Ok(norm_adj.matmul(&d_agg)?)
	}
}

impl GraphLayer for GCNLayer {
	fn forward_graph(&mut self, g: &Graph, features: &Tensor) -> Tensor {
		match self.try_forward_graph(g, features) {
			Ok(t) => t,
			Err(e) => {
				tracing::debug!(error = %e, "GCNLayer forward_graph failed; returning zero activations");
				// Drop any stale cache so a later backward takes the MissingForwardState
				// path instead of multiplying against a shape this forward never produced.
				self.last_norm_adj = None;
				self.last_pre_act = None;
				Tensor::zeros(g.num_nodes(), self.linear.weight.cols)
			}
		}
	}

	fn parameters(&self) -> Vec<&Tensor> {
		let mut p = self.linear.parameters();
		if let Some(ref n) = self.norm {
			p.extend(Layer::parameters(n));
		}
		p
	}

	fn parameters_mut(&mut self) -> Vec<&mut Tensor> {
		let mut p = self.linear.parameters_mut();
		if let Some(ref mut n) = self.norm {
			p.extend(Layer::parameters_mut(n));
		}
		p
	}
}

impl BackwardGraphLayer for GCNLayer {
	fn backward_graph(&mut self, g: &Graph, d_out: &Tensor) -> Tensor {
		match self.try_backward_graph(g, d_out) {
			Ok(t) => t,
			Err(e) => {
				tracing::debug!(error = %e, "GCNLayer backward_graph failed; returning zero gradient");
				// dInput is (num_nodes, in_features); in_features == linear.weight.rows.
				Tensor::zeros(g.num_nodes(), self.linear.weight.rows)
			}
		}
	}

	fn param_grads(&self) -> Vec<&Tensor> {
		let mut g = self.linear.param_grads();
		if let Some(ref n) = self.norm {
			g.extend(Backward::param_grads(n));
		}
		g
	}

	fn param_grads_mut(&mut self) -> Vec<&mut Tensor> {
		let mut g = self.linear.param_grads_mut();
		if let Some(ref mut n) = self.norm {
			g.extend(Backward::param_grads_mut(n));
		}
		g
	}

	fn zero_grads(&mut self) {
		self.linear.zero_grads();
		if let Some(ref mut n) = self.norm {
			Backward::zero_grads(n);
		}
	}
}

// ==== [norm] ====

pub struct LayerNorm {
	pub gamma: Tensor, // 1×D
	pub beta: Tensor,  // 1×D
	pub epsilon: f64,
	pub dim: usize,
	pub last_x_hat: Option<Tensor>,
	last_inv_std: Vec<f64>,
	d_gamma: Tensor,
	d_beta: Tensor,
}

impl LayerNorm {
	pub fn new(dim: usize) -> Self {
		Self {
			gamma: Tensor::ones(1, dim),
			beta: Tensor::zeros(1, dim),
			epsilon: 1e-5,
			dim,
			last_x_hat: None,
			last_inv_std: Vec::new(),
			d_gamma: Tensor::zeros(1, dim),
			d_beta: Tensor::zeros(1, dim),
		}
	}

	pub fn try_backward(&mut self, d_out: &Tensor) -> Result<Tensor, GnnError> {
		let x_hat = self
			.last_x_hat
			.as_ref()
			.ok_or(GnnError::MissingForwardState("layernorm::last_x_hat"))?;
		let (n, d) = (d_out.rows, d_out.cols);
		let mut d_input = Tensor::zeros(n, d);

		for i in 0..n {
			let mut d_x_hat = vec![0.0; d];
			for (j, slot) in d_x_hat.iter_mut().enumerate().take(d) {
				*slot = d_out.at(i, j) * self.gamma.at(0, j);
				self.d_gamma.data[j] += d_out.at(i, j) * x_hat.at(i, j);
				self.d_beta.data[j] += d_out.at(i, j);
			}

			let mut sum_dx = 0.0;
			let mut sum_dx_xh = 0.0;
			for (j, &dxh) in d_x_hat.iter().enumerate().take(d) {
				sum_dx += dxh;
				sum_dx_xh += dxh * x_hat.at(i, j);
			}

			let scale = self.last_inv_std[i] / d as f64;
			for (j, &dxh) in d_x_hat.iter().enumerate().take(d) {
				d_input.set(
					i,
					j,
					scale * (d as f64 * dxh - sum_dx - x_hat.at(i, j) * sum_dx_xh),
				);
			}
		}
		Ok(d_input)
	}
}

impl Layer for LayerNorm {
	fn forward(&mut self, input: &Tensor) -> Tensor {
		let (n, d) = (input.rows, input.cols);
		let mut out = Tensor::zeros(n, d);
		let mut x_hat = Tensor::zeros(n, d);
		let mut inv_stds = vec![0.0; n];

		for (i, inv_std_slot) in inv_stds.iter_mut().enumerate().take(n) {
			let mut mean = 0.0;
			for j in 0..d {
				mean += input.at(i, j);
			}
			mean /= d as f64;

			let mut var = 0.0;
			for j in 0..d {
				let diff = input.at(i, j) - mean;
				var += diff * diff;
			}
			var /= d as f64;

			let inv_std = 1.0 / (var + self.epsilon).sqrt();
			*inv_std_slot = inv_std;

			for j in 0..d {
				let x = (input.at(i, j) - mean) * inv_std;
				x_hat.set(i, j, x);
				out.set(i, j, x * self.gamma.at(0, j) + self.beta.at(0, j));
			}
		}
		self.last_x_hat = Some(x_hat);
		self.last_inv_std = inv_stds;
		out
	}

	fn parameters(&self) -> Vec<&Tensor> {
		vec![&self.gamma, &self.beta]
	}

	fn parameters_mut(&mut self) -> Vec<&mut Tensor> {
		vec![&mut self.gamma, &mut self.beta]
	}
}

impl Backward for LayerNorm {
	fn backward(&mut self, d_out: &Tensor) -> Tensor {
		match self.try_backward(d_out) {
			Ok(t) => t,
			Err(e) => {
				tracing::error!(error = %e, "LayerNorm backward failed; returning zero gradient");
				Tensor::zeros(d_out.rows, d_out.cols)
			}
		}
	}

	fn param_grads(&self) -> Vec<&Tensor> {
		vec![&self.d_gamma, &self.d_beta]
	}

	fn param_grads_mut(&mut self) -> Vec<&mut Tensor> {
		vec![&mut self.d_gamma, &mut self.d_beta]
	}

	fn zero_grads(&mut self) {
		self.d_gamma = Tensor::zeros(1, self.dim);
		self.d_beta = Tensor::zeros(1, self.dim);
	}
}

// ==== [loss] ====

fn row_dot(t: &Tensor, i: usize, j: usize) -> f64 {
	let d = t.cols;
	let mut sum = 0.0;
	for k in 0..d {
		sum += t.at(i, k) * t.at(j, k);
	}
	sum
}

pub fn link_prediction_loss(
	embeddings: &Tensor,
	pos_edges: &[[usize; 2]],
	neg_edges: &[[usize; 2]],
) -> f64 {
	let total = pos_edges.len() + neg_edges.len();
	if total == 0 {
		return 0.0;
	}
	let mut loss = 0.0;
	for e in pos_edges {
		let dot = row_dot(embeddings, e[0], e[1]);
		loss -= (sigmoid(dot) + 1e-10).ln();
	}
	for e in neg_edges {
		let dot = row_dot(embeddings, e[0], e[1]);
		loss -= (1.0 - sigmoid(dot) + 1e-10).ln();
	}
	loss / total as f64
}

pub fn link_prediction_grad(
	embeddings: &Tensor,
	pos_edges: &[[usize; 2]],
	neg_edges: &[[usize; 2]],
) -> Tensor {
	let (n, d) = (embeddings.rows, embeddings.cols);
	let total = pos_edges.len() + neg_edges.len();
	if total == 0 {
		return Tensor::zeros(n, d);
	}
	let scale = 1.0 / total as f64;
	let mut grad = Tensor::zeros(n, d);

	for e in pos_edges {
		let (u, v) = (e[0], e[1]);
		let dot = row_dot(embeddings, u, v);
		let s = sigmoid(dot) - 1.0;
		for j in 0..d {
			grad.data[u * d + j] += scale * s * embeddings.at(v, j);
			grad.data[v * d + j] += scale * s * embeddings.at(u, j);
		}
	}
	for e in neg_edges {
		let (u, v) = (e[0], e[1]);
		let dot = row_dot(embeddings, u, v);
		let s = sigmoid(dot);
		for j in 0..d {
			grad.data[u * d + j] += scale * s * embeddings.at(v, j);
			grad.data[v * d + j] += scale * s * embeddings.at(u, j);
		}
	}
	grad
}

// ==== [optim] ====

pub trait Optimizer {
	fn step(&mut self, params: &mut [&mut Tensor], grads: &[&Tensor]);
	fn zero_grad(&self, grads: &mut [Tensor]) {
		for g in grads.iter_mut() {
			for v in &mut g.data {
				*v = 0.0;
			}
		}
	}
}

pub struct Adam {
	pub lr: f64,
	pub beta1: f64,
	pub beta2: f64,
	pub epsilon: f64,
	step_count: usize,
	m: Vec<Tensor>,
	v: Vec<Tensor>,
}

impl Adam {
	pub fn new(lr: f64) -> Self {
		Self {
			lr,
			beta1: 0.9,
			beta2: 0.999,
			epsilon: 1e-8,
			step_count: 0,
			m: Vec::new(),
			v: Vec::new(),
		}
	}
}

impl Optimizer for Adam {
	fn step(&mut self, params: &mut [&mut Tensor], grads: &[&Tensor]) {
		if self.m.is_empty() {
			self.m = params
				.iter()
				.map(|p| Tensor::zeros(p.rows, p.cols))
				.collect();
			self.v = params
				.iter()
				.map(|p| Tensor::zeros(p.rows, p.cols))
				.collect();
		}
		self.step_count += 1;
		let t = self.step_count as f64;
		let bias_c1 = 1.0 - self.beta1.powf(t);
		let bias_c2 = 1.0 - self.beta2.powf(t);

		for (i, (param, grad)) in params.iter_mut().zip(grads.iter()).enumerate() {
			for j in 0..param.data.len() {
				let g = grad.data[j];
				self.m[i].data[j] = self.beta1 * self.m[i].data[j] + (1.0 - self.beta1) * g;
				self.v[i].data[j] = self.beta2 * self.v[i].data[j] + (1.0 - self.beta2) * g * g;

				let m_hat = self.m[i].data[j] / bias_c1;
				let v_hat = self.v[i].data[j] / bias_c2;

				param.data[j] -= self.lr * m_hat / (v_hat.sqrt() + self.epsilon);
			}
		}
	}
}

// ==== [model] ====

// A `forward` must precede its `backward`; call `zero_grads` before each backward.
pub struct Model {
	pub layers: Vec<GCNLayer>,
	pub out_layer: Option<LinearLayer>,
}

impl Model {
	pub fn new(layers: Vec<GCNLayer>, out_layer: Option<LinearLayer>) -> Self {
		Self { layers, out_layer }
	}

	pub fn forward(&mut self, g: &Graph, features: &Tensor) -> Result<Tensor, GnnError> {
		let mut h = features.clone();
		for layer in &mut self.layers {
			h = layer.try_forward_graph(g, &h)?;
		}
		if let Some(ref mut ol) = self.out_layer {
			h = ol.try_forward(&h)?;
		}
		Ok(h)
	}

	pub fn backward(&mut self, g: &Graph, d_out: &Tensor) -> Result<(), GnnError> {
		let mut grad = d_out.clone();
		if let Some(ref mut ol) = self.out_layer {
			grad = ol.try_backward(&grad)?;
		}
		for layer in self.layers.iter_mut().rev() {
			grad = layer.try_backward_graph(g, &grad)?;
		}
		Ok(())
	}

	pub fn parameters(&self) -> Vec<&Tensor> {
		let mut p = Vec::new();
		for layer in &self.layers {
			p.extend(GraphLayer::parameters(layer));
		}
		if let Some(ref ol) = self.out_layer {
			p.extend(Layer::parameters(ol));
		}
		p
	}

	pub fn parameters_mut(&mut self) -> Vec<&mut Tensor> {
		let mut p = Vec::new();
		for layer in &mut self.layers {
			p.extend(GraphLayer::parameters_mut(layer));
		}
		if let Some(ref mut ol) = self.out_layer {
			p.extend(Layer::parameters_mut(ol));
		}
		p
	}

	pub fn param_grads(&self) -> Vec<&Tensor> {
		let mut g = Vec::new();
		for layer in &self.layers {
			g.extend(layer.param_grads());
		}
		if let Some(ref ol) = self.out_layer {
			g.extend(Backward::param_grads(ol));
		}
		g
	}

	pub fn param_grads_mut(&mut self) -> Vec<&mut Tensor> {
		let mut g = Vec::new();
		for layer in &mut self.layers {
			g.extend(layer.param_grads_mut());
		}
		if let Some(ref mut ol) = self.out_layer {
			g.extend(Backward::param_grads_mut(ol));
		}
		g
	}

	pub fn zero_grads(&mut self) {
		for layer in &mut self.layers {
			layer.zero_grads();
		}
		if let Some(ref mut ol) = self.out_layer {
			Backward::zero_grads(ol);
		}
	}
}

// ==== [persist] ====

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// On-disk weight format version. Bump on any `WeightFile`/`TensorRecord` shape
/// change — old files are rejected, not mis-decoded and not migrated.
pub const WEIGHT_FILE_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum PersistError {
	#[error("unsupported weight file version {found}, expected {expected}")]
	VersionMismatch { found: u32, expected: u32 },
	#[error("parameter count mismatch: model {model}, file {file}")]
	CountMismatch { model: usize, file: usize },
	#[error("param {idx} shape mismatch: model ({mr},{mc}), file ({fr},{fc})", mr = .model.0, mc = .model.1, fr = .file.0, fc = .file.1)]
	ShapeMismatch {
		idx: usize,
		model: (usize, usize),
		file: (usize, usize),
	},
	#[error("param {idx} data length {found} does not match shape {expected} (corrupt weight file)")]
	DataLenMismatch {
		idx: usize,
		expected: usize,
		found: usize,
	},
	#[error("bincode encode: {0}")]
	BincodeEncode(#[from] bincode::error::EncodeError),
	#[error("bincode decode: {0}")]
	BincodeDecode(#[from] bincode::error::DecodeError),
	#[error("io: {0}")]
	Io(#[from] std::io::Error),
}

fn bincode_cfg() -> bincode::config::Configuration {
	bincode::config::standard()
}

#[derive(Serialize, Deserialize)]
struct WeightFile {
	version: u32,
	params: Vec<TensorRecord>,
}

#[derive(Serialize, Deserialize)]
struct TensorRecord {
	rows: usize,
	cols: usize,
	data: Vec<f64>,
}

pub fn marshal_weights(model: &Model) -> Result<Vec<u8>, PersistError> {
	let params = model.parameters();
	let records: Vec<TensorRecord> = params
		.iter()
		.map(|p| TensorRecord {
			rows: p.rows,
			cols: p.cols,
			data: p.data.clone(),
		})
		.collect();
	let wf = WeightFile {
		version: WEIGHT_FILE_VERSION,
		params: records,
	};
	Ok(bincode::serde::encode_to_vec(&wf, bincode_cfg())?)
}

pub fn unmarshal_weights(model: &mut Model, data: &[u8]) -> Result<(), PersistError> {
	let (wf, _): (WeightFile, _) = bincode::serde::decode_from_slice(data, bincode_cfg())?;
	if wf.version != WEIGHT_FILE_VERSION {
		return Err(PersistError::VersionMismatch {
			found: wf.version,
			expected: WEIGHT_FILE_VERSION,
		});
	}
	let params = model.parameters_mut();
	if params.len() != wf.params.len() {
		return Err(PersistError::CountMismatch {
			model: params.len(),
			file: wf.params.len(),
		});
	}
	for (i, (param, rec)) in params.into_iter().zip(&wf.params).enumerate() {
		if param.rows != rec.rows || param.cols != rec.cols {
			return Err(PersistError::ShapeMismatch {
				idx: i,
				model: (param.rows, param.cols),
				file: (rec.rows, rec.cols),
			});
		}
		// Shape and data are independent fields: a corrupt file can match shape yet
		// carry a wrong-length data vec, and `copy_from_slice` PANICS on that.
		if rec.data.len() != param.data.len() {
			return Err(PersistError::DataLenMismatch {
				idx: i,
				expected: param.data.len(),
				found: rec.data.len(),
			});
		}
		param.data.copy_from_slice(&rec.data);
	}
	Ok(())
}

#[cfg(test)]
#[path = "tests/gnn_test.rs"]
mod gnn_tests;
