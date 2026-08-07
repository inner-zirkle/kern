//! Link-prediction loss and gradient: positive pairs are connected nodes,
//! negatives are sampled non-edges, scored by sigmoid of the dot product.

use crate::gnn_activation::sigmoid;
use crate::gnn_tensor::Tensor;

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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn link_prediction_empty_edges_is_zero_loss_and_grad() {
		let emb = Tensor::new(3, 2, vec![1.0, 0.0, 0.0, 1.0, 1.0, 1.0]).unwrap();
		assert_eq!(link_prediction_loss(&emb, &[], &[]), 0.0);
		let g = link_prediction_grad(&emb, &[], &[]);
		assert_eq!((g.rows, g.cols), (3, 2));
		assert!(g.data.iter().all(|&v| v == 0.0));
	}

	#[test]
	fn link_prediction_aligned_positive_edge_has_lower_loss_than_opposed() {
		let aligned = Tensor::new(2, 2, vec![3.0, 0.0, 3.0, 0.0]).unwrap();
		let opposed = Tensor::new(2, 2, vec![3.0, 0.0, -3.0, 0.0]).unwrap();
		let pos = [[0usize, 1usize]];
		assert!(
			link_prediction_loss(&aligned, &pos, &[]) < link_prediction_loss(&opposed, &pos, &[]),
			"a positive edge between aligned embeddings is cheaper than between opposed ones"
		);
	}

	#[test]
	fn link_prediction_grad_matches_numerical_gradient() {
		let emb = Tensor::new(3, 2, vec![0.5, -0.2, 0.1, 0.3, -0.4, 0.6]).unwrap();
		let pos = [[0usize, 1usize], [1, 2]];
		let neg = [[0usize, 2usize]];
		let analytic = link_prediction_grad(&emb, &pos, &neg);
		const H: f64 = 1e-6;
		for idx in 0..emb.data.len() {
			let mut ep = emb.clone();
			ep.data[idx] += H;
			let mut em = emb.clone();
			em.data[idx] -= H;
			let num =
				(link_prediction_loss(&ep, &pos, &neg) - link_prediction_loss(&em, &pos, &neg)) / (2.0 * H);
			let den = 1.0_f64.max(analytic.data[idx].abs()).max(num.abs());
			assert!(
				(analytic.data[idx] - num).abs() / den < 1e-4,
				"grad[{idx}]: analytic {} vs numeric {num}",
				analytic.data[idx]
			);
		}
	}
}
