//! Weight (de)serialization, gated by `WEIGHT_FILE_VERSION`: a mismatched or
//! torn weight file is rejected and training restarts from init — weights are
//! a cache, never migrated (alpha policy).

use crate::gnn_model::Model;
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
mod tests {
	use super::*;
	use crate::gnn_gcn::GCNLayer;
	use rand::rngs::StdRng;
	use rand::SeedableRng;

	fn small_model(seed: u64) -> Model {
		let mut rng = StdRng::seed_from_u64(seed);
		Model::new(vec![GCNLayer::with_rng(4, 3, None, false, &mut rng)], None)
	}

	#[test]
	fn marshal_unmarshal_round_trips_every_param_value_and_shape() {
		let src = small_model(1);
		let bytes = marshal_weights(&src).expect("marshal");

		let mut dst = small_model(999);
		unmarshal_weights(&mut dst, &bytes).expect("unmarshal");

		let sp = src.parameters();
		let dp = dst.parameters();
		assert_eq!(sp.len(), dp.len(), "parameter count preserved");
		assert!(
			!sp.is_empty(),
			"the model actually has parameters to compare"
		);
		for (a, b) in sp.iter().zip(&dp) {
			assert_eq!((a.rows, a.cols), (b.rows, b.cols), "shape preserved");
			assert_eq!(
				a.data, b.data,
				"every value is byte-identical after the round trip"
			);
		}
	}

	#[test]
	fn unmarshal_rejects_a_future_version_before_checking_params() {
		let wf = WeightFile {
			version: WEIGHT_FILE_VERSION + 1,
			params: Vec::new(),
		};
		let bytes = bincode::serde::encode_to_vec(&wf, bincode_cfg()).unwrap();
		let mut model = small_model(1);
		let err = unmarshal_weights(&mut model, &bytes).unwrap_err();
		assert!(
			matches!(err, PersistError::VersionMismatch { found, expected }
				if found == WEIGHT_FILE_VERSION + 1 && expected == WEIGHT_FILE_VERSION),
			"got {err:?}",
		);
	}

	#[test]
	fn unmarshal_rejects_a_corrupt_data_length_without_panicking() {
		let model = small_model(1);
		let records: Vec<TensorRecord> = model
			.parameters()
			.iter()
			.enumerate()
			.map(|(i, p)| TensorRecord {
				rows: p.rows,
				cols: p.cols,
				data: if i == 0 {
					p.data[..p.data.len() - 1].to_vec()
				} else {
					p.data.clone()
				},
			})
			.collect();
		let wf = WeightFile {
			version: WEIGHT_FILE_VERSION,
			params: records,
		};
		let bytes = bincode::serde::encode_to_vec(&wf, bincode_cfg()).unwrap();

		let mut dst = small_model(2);
		let err = unmarshal_weights(&mut dst, &bytes).unwrap_err();
		assert!(
			matches!(err, PersistError::DataLenMismatch { idx: 0, .. }),
			"corrupt data length must be a clean error, not a panic; got {err:?}"
		);
	}

	#[test]
	fn unmarshal_rejects_a_param_count_mismatch() {
		let wf = WeightFile {
			version: WEIGHT_FILE_VERSION,
			params: Vec::new(),
		};
		let bytes = bincode::serde::encode_to_vec(&wf, bincode_cfg()).unwrap();
		let mut model = small_model(1);
		let err = unmarshal_weights(&mut model, &bytes).unwrap_err();
		assert!(
			matches!(err, PersistError::CountMismatch { .. }),
			"got {err:?}"
		);
	}
}
