//! Namespace shim for the hand-rolled GNN stack: re-exports every `gnn_*`
//! module under `crate::gnn::<name>` plus the shared [`GnnError`]. No graph
//! logic of its own.

pub use crate::gnn_activation::Activation;
pub use crate::gnn_backward::{BackwardGraphLayer, GraphLayer};

#[derive(Debug, thiserror::Error)]
pub enum GnnError {
	#[error("gnn: missing forward state ({0}); call forward_graph before backward/inference")]
	MissingForwardState(&'static str),

	#[error("gnn: tensor error: {0}")]
	Tensor(#[from] crate::gnn_tensor::TensorError),
}

pub use crate::gnn_gcn as gcn;
pub use crate::gnn_graph as graph;
pub use crate::gnn_layer as layer;
pub use crate::gnn_loss as loss;
pub use crate::gnn_model as model;
pub use crate::gnn_norm as norm;
pub use crate::gnn_optim as optim;
pub use crate::gnn_persist as persist;
pub use crate::gnn_propagate as propagate;
pub use crate::gnn_sparse as sparse;
pub use crate::gnn_tensor as tensor;
