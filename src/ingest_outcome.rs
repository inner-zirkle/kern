#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeStatus {
	Committed,
	Partial,
	Deduped,
	Failed,
}

impl OutcomeStatus {
	pub fn as_str(&self) -> &'static str {
		match self {
			Self::Committed => "committed",
			Self::Partial => "partial",
			Self::Deduped => "deduped",
			Self::Failed => "failed",
		}
	}
}

// class: "permanent" | "transient" (retryable); chunk_index 0 = document scope.
#[derive(Debug, Clone)]
pub struct FailureReport {
	pub scope: String,
	pub chunk_index: usize,
	pub class: String,
	pub error: String,
}

impl FailureReport {
	pub fn document_permanent(error: impl Into<String>) -> Self {
		Self {
			scope: "document".into(),
			chunk_index: 0,
			class: "permanent".into(),
			error: error.into(),
		}
	}
}

// INVARIANT: transient_failures + permanent_failures == failures.len().
#[derive(Debug, Clone)]
pub struct Outcome {
	pub status: OutcomeStatus,
	pub doc_id: String,
	pub total_chunks: usize,
	pub embedded_chunks: usize,
	pub failed_chunks: usize,
	pub transient_failures: usize,
	pub permanent_failures: usize,
	pub failures: Vec<FailureReport>,
	pub message: String,
}

impl Outcome {
	pub fn failed(message: impl Into<String>, failures: Vec<FailureReport>) -> Self {
		Self {
			status: OutcomeStatus::Failed,
			doc_id: String::new(),
			total_chunks: 0,
			embedded_chunks: 0,
			failed_chunks: 0,
			transient_failures: 0,
			permanent_failures: 0,
			failures,
			message: message.into(),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn status_as_str_is_the_lowercase_label() {
		assert_eq!(OutcomeStatus::Committed.as_str(), "committed");
		assert_eq!(OutcomeStatus::Partial.as_str(), "partial");
		assert_eq!(OutcomeStatus::Failed.as_str(), "failed");
		assert_eq!(OutcomeStatus::Deduped.as_str(), "deduped");
	}

	#[test]
	fn failed_zeroes_every_counter_and_carries_the_failures() {
		let f = FailureReport::document_permanent("boom");
		let o = Outcome::failed("nothing processed", vec![f]);
		assert_eq!(o.status, OutcomeStatus::Failed);
		assert!(o.doc_id.is_empty(), "no document committed");
		assert_eq!(
			(
				o.total_chunks,
				o.embedded_chunks,
				o.failed_chunks,
				o.transient_failures,
				o.permanent_failures
			),
			(0, 0, 0, 0, 0),
			"all counters zero on a wholly-failed outcome"
		);
		assert_eq!(o.failures.len(), 1, "failure detail preserved");
		assert_eq!(o.message, "nothing processed");
	}

	#[test]
	fn document_permanent_has_the_canonical_shape() {
		let f = FailureReport::document_permanent("lock poisoned");
		assert_eq!(f.scope, "document");
		assert_eq!(f.chunk_index, 0);
		assert_eq!(f.class, "permanent");
		assert_eq!(f.error, "lock poisoned");
	}
}
