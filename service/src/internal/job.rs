use boundless_market::alloy::primitives::{Bytes, U256};
use eq_common::{InclusionServiceError, ZKStackEqProofInput};
use eq_sdk::types::BlobId;
use serde::{Deserialize, Serialize};

/// A job for the service, mapped to a [BlobId]
pub type Job = BlobId;

/// A job ID for the Boundless prover network, used to track ZK proof requests
#[derive(Debug, Serialize, Deserialize)]
pub struct BoundlessJobId {
    pub id: U256,
    pub expires_at: u64,
}

/// The public values and data needed to verify a ZK proof from Boundless
#[derive(Debug, Serialize, Deserialize)]
pub struct BoundlessProofResult {
    pub journal: Bytes,
    pub seal: Bytes,
}

/// Used as a [Job] state machine for the eq-service.
///
/// Should map 1to1 with [ResponseStatus](eq_common::eqs::get_keccak_inclusion_response::ResponseValue)
/// for consistency in internal state and what is reported by the RPC.
#[derive(Serialize, Deserialize)]
pub enum JobStatus {
    /// DA inclusion proof data is being collected
    DataAvailabilityPending,
    /// DA inclusion is processed and ready to send to the ZK prover
    DataAvailable(ZKStackEqProofInput),
    /// A ZK prover job had been requested, awaiting response
    ZkProofPending(BoundlessJobId),
    /// A ZK proof is ready, and the [Job] is complete
    ZkProofFinished(BoundlessProofResult),
    /// A wrapper for any [InclusionServiceError], with:
    /// - Option = None                        --> Permanent failure
    /// - Option = Some(\<retry-able status\>) --> Retry is possible, with a JobStatus state to retry with
    Failed(InclusionServiceError, Option<Box<JobStatus>>),
}

impl std::fmt::Debug for JobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobStatus::DataAvailabilityPending => write!(f, "DataAvailabilityPending"),
            JobStatus::DataAvailable(_) => write!(f, "DataAvailable"),
            JobStatus::ZkProofPending(_) => write!(f, "ZkProofPending"),
            JobStatus::ZkProofFinished(_) => write!(f, "ZkProofFinished"),
            JobStatus::Failed(_, _) => write!(f, "Failed"),
        }
    }
}
