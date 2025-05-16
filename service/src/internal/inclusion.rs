use crate::{Job, JobStatus};

use bonsai_sdk::non_blocking::{Client as BonsaiClient, SessionId};
use celestia_rpc::{BlobClient, Client as CelestiaJSONClient, HeaderClient, ShareClient};
use eq_common::{InclusionServiceError, KeccakInclusionToDataRootProofInput};
use jsonrpsee::core::ClientError as JsonRpcError;
use log::{debug, error, info};
use risc0_zkvm::serde::to_vec;
use risc0_zkvm::{compute_image_id, Receipt};
use sha3::Digest;
use sha3::Keccak256;
use sled::{Transactional, Tree as SledTree};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, OnceCell};

use eq_program_keccak_inclusion::{
    EQ_PROGRAM_KECCAK_INCLUSION_GUEST_ELF as KECCAK_INCLUSION_ELF,
    EQ_PROGRAM_KECCAK_INCLUSION_GUEST_ID as KECCAK_INCLUSION_ID,
};

/// The main service, depends on external DA and ZK clients internally!
pub struct InclusionService {
    pub config: InclusionServiceConfig,
    da_client_handle: OnceCell<Arc<CelestiaJSONClient>>,
    zk_client_handle: OnceCell<Arc<BonsaiClient>>,
    pub config_db: SledTree,
    pub queue_db: SledTree,
    pub finished_db: SledTree,
    pub job_sender: mpsc::UnboundedSender<Option<Job>>,
}

impl InclusionService {
    pub fn new(
        config: InclusionServiceConfig,
        da_client_handle: OnceCell<Arc<CelestiaJSONClient>>,
        zk_client_handle: OnceCell<Arc<BonsaiClient>>,
        config_db: SledTree,
        queue_db: SledTree,
        finished_db: SledTree,
        job_sender: mpsc::UnboundedSender<Option<Job>>,
    ) -> Self {
        InclusionService {
            config,
            da_client_handle,
            zk_client_handle,
            config_db,
            queue_db,
            finished_db,
            job_sender,
        }
    }
}

pub struct InclusionServiceConfig {
    pub da_node_token: String,
    pub da_node_ws: String,
}

impl InclusionService {
    /// A worker that receives [Job]s by a channel and drives them to completion.
    ///
    /// `Job`s are complete stepwise with a [JobStatus] that is recorded in a
    /// queue database, with work to progress each status to towards completion async.
    /// Once a step is completed, `JobStatus` is recorded into the queue database that
    /// recursively, driving to `Job` completion.
    ///
    /// When a successful or failed state is arrived at,
    /// the job is atomically removed from the queue and added to a results database.
    pub async fn job_worker(
        self: Arc<Self>,
        mut job_receiver: mpsc::UnboundedReceiver<Option<Job>>,
    ) {
        debug!("Job worker started");
        while let Some(Some(job)) = job_receiver.recv().await {
            let service = self.clone();
            tokio::spawn(async move {
                debug!("Job worker received {job:?}",);
                let _ = service.prove(job).await; //Don't return with "?", we run keep looping
            });
        }

        info!("Shutting down");
        let _ = self.queue_db.flush();
        let _ = self.finished_db.flush();
        info!("Cleanup complete");

        std::process::exit(0);
    }

    /// The main service task: produce a proof based on a [Job] requested.
    pub async fn prove(&self, job: Job) -> Result<(), InclusionServiceError> {
        let job_key = bincode::serialize(&job)
            .map_err(|e| InclusionServiceError::InternalError(e.to_string()))?;
        if let Some(queue_data) = self
            .queue_db
            .get(&job_key)
            .map_err(|e| InclusionServiceError::InternalError(e.to_string()))?
        {
            let mut job_status: JobStatus = bincode::deserialize(&queue_data)
                .map_err(|e| InclusionServiceError::InternalError(e.to_string()))?;
            debug!("Job worker processing with starting status: {job_status:?}");
            match job_status {
                JobStatus::DataAvailabilityPending => {
                    let da_client_handle = self.get_da_client().await.clone();
                    self.get_zk_proof_input_from_da(&job, &job_key, da_client_handle?)
                        .await?;
                    debug!("DA data -> zk input ready");
                }
                JobStatus::DataAvailable(proof_input) => {
                    // TODO handle non-hardcoded ZK programs
                    match self.request_zk_proof(&proof_input).await {
                        Ok(zk_job_id) => {
                            job_status = JobStatus::ZkProofPending(zk_job_id);
                            self.send_job_with_new_status(job_key, job_status, job)?;
                        }
                        Err(e) => {
                            error!("{job:?} failed progressing DataAvailable: {e}");
                            job_status = JobStatus::Failed(
                                InclusionServiceError::ZkClientError(e.to_string()),
                                Some(JobStatus::DataAvailable(proof_input).into()),
                            );
                            self.finalize_job(&job_key, job_status)?;
                        }
                    };
                    debug!("ZK request sent");
                }
                JobStatus::ZkProofPending(zk_request_id) => {
                    debug!("ZK request waiting");
                    match self.wait_for_zk_proof(zk_request_id.clone()).await {
                        Ok(zk_proof) => {
                            info!("🎉 {job:?} Finished!");
                            job_status = JobStatus::ZkProofFinished(zk_proof);
                            self.finalize_job(&job_key, job_status)?;
                        }
                        Err(e) => {
                            error!("{job:?} failed progressing ZkProofPending: {e}");
                            job_status = JobStatus::Failed(
                                InclusionServiceError::ZkClientError(e.to_string()),
                                Some(JobStatus::ZkProofPending(zk_request_id).into()),
                            );
                            self.finalize_job(&job_key, job_status)?;
                        }
                    }
                    debug!("ZK request fulfilled");
                }
                _ => error!("Queue has INVALID status! Finished jobs stuck in queue!"),
            }
        };
        Ok(())
    }

    /// Connects to a [CelestiaJSONClient] and attempts to get a inclusion proof for a [Job].
    /// On `Ok(())`, the queue DB contains valid ZKP input inside a new [JobStatus::DataAvailable] on the queue.
    async fn get_zk_proof_input_from_da(
        &self,
        job: &Job,
        job_key: &[u8],
        client: Arc<CelestiaJSONClient>,
    ) -> Result<(), InclusionServiceError> {
        debug!("Preparing request to Celestia");

        let header = client
            .header_get_by_height(job.height.into())
            .await
            .map_err(|e| self.handle_da_client_error(e, job, job_key))?;

        let eds_row_roots = header.dah.row_roots();
        let eds_size: u64 = eds_row_roots.len().try_into().map_err(|_| {
            InclusionServiceError::InternalError(
                "Failed to convert eds_row_roots.len() to u64".to_string(),
            )
        })?;
        let ods_size: u64 = eds_size / 2;

        let blob = client
            .blob_get(job.height.into(), job.namespace, job.commitment)
            .await
            .map_err(|e| self.handle_da_client_error(e, job, job_key))?;

        let blob_index = blob
            .index
            .ok_or_else(|| InclusionServiceError::MissingBlobIndex)?;

        // https://github.com/celestiaorg/eq-service/issues/65
        //let first_row_index: u64 = blob_index.div_ceil(eds_size) - 1;
        let first_row_index: u64 =
            blob.index.ok_or(InclusionServiceError::MissingBlobIndex)? / eds_size;
        let ods_index = blob_index - (first_row_index * ods_size);

        let range_response = client
            .share_get_range(&header, ods_index, ods_index + blob.shares_len() as u64)
            .await
            .map_err(|e| self.handle_da_client_error(e, job, job_key))?;

        range_response
            .proof
            .verify(header.dah.hash())
            .map_err(|_| InclusionServiceError::FailedShareRangeProofSanityCheck)?;

        let keccak_hash: [u8; 32] = Keccak256::new().chain_update(&blob.data).finalize().into();

        debug!("Creating ZK Proof input from Celestia Data");
        let proof_input = KeccakInclusionToDataRootProofInput {
            data: blob.data,
            namespace_id: job.namespace,
            share_proofs: range_response.proof.share_proofs,
            row_proof: range_response.proof.row_proof,
            data_root: header.dah.hash().as_bytes().try_into().map_err(|_| {
                InclusionServiceError::InternalError(
                    "Failed to convert header.dah.hash().as_bytes() to [u8; 32]".to_string(),
                )
            })?,
            keccak_hash,
        };

        self.send_job_with_new_status(
            job_key.to_vec(),
            JobStatus::DataAvailable(proof_input),
            job.clone(),
        )
    }

    /// Helper function to handle error from a [jsonrpsee] based DA client.
    /// Will finalize the job in an [JobStatus::Failed] state,
    /// that may be retryable.
    fn handle_da_client_error(
        &self,
        da_client_error: JsonRpcError,
        job: &Job,
        job_key: &[u8],
    ) -> InclusionServiceError {
        error!("Celestia Client error: {da_client_error}");
        let (e, job_status);
        let call_err = "DA Call Error: ".to_string() + &da_client_error.to_string();
        match da_client_error {
            JsonRpcError::Call(error_object) => {
                // TODO: make this handle errors much better! JSON stringiness is a problem!
                if error_object.message().starts_with("header: not found") {
                    e = InclusionServiceError::DaClientError(format!(
                        "{call_err} - Likely DA Node is not properly synced, and blob does exists on the network. PLEASE REPORT!"
                    ));
                    job_status = JobStatus::Failed(
                        e.clone(),
                        Some(JobStatus::DataAvailabilityPending.into()),
                    );
                } else if error_object
                    .message()
                    .starts_with("header: given height is from the future")
                {
                    e = InclusionServiceError::DaClientError(format!("{call_err}"));
                    job_status = JobStatus::Failed(e.clone(), None);
                } else if error_object
                    .message()
                    .starts_with("header: syncing in progress")
                {
                    e = InclusionServiceError::DaClientError(format!(
                        "{call_err} - Blob *may* exist on the network."
                    ));
                    job_status = JobStatus::Failed(
                        e.clone(),
                        Some(JobStatus::DataAvailabilityPending.into()),
                    );
                } else if error_object.message().starts_with("blob: not found") {
                    e = InclusionServiceError::DaClientError(format!(
                        "{call_err} - Likely incorrect request inputs."
                    ));
                    job_status = JobStatus::Failed(e.clone(), None);
                } else {
                    e = InclusionServiceError::DaClientError(format!(
                        "{call_err} - UNKNOWN DA client error. PLEASE REPORT!"
                    ));
                    job_status = JobStatus::Failed(e.clone(), None);
                }
            }
            JsonRpcError::RequestTimeout
            | JsonRpcError::Transport(_)
            | JsonRpcError::RestartNeeded(_) => {
                e = InclusionServiceError::DaClientError(format!("{da_client_error}"));
                job_status =
                    JobStatus::Failed(e.clone(), Some(JobStatus::DataAvailabilityPending.into()));
            }
            // TODO: handle other Celestia JSON RPC errors
            _ => {
                e = InclusionServiceError::DaClientError(
                    "Unhandled Celestia SDK error. PLEASE REPORT!".to_string(),
                );
                error!("{job:?} failed, not recoverable: {e}");
                job_status = JobStatus::Failed(e.clone(), None);
            }
        };
        match self.finalize_job(job_key, job_status) {
            Ok(_) => e,
            Err(internal_err) => internal_err,
        }
    }

    /// Start a proof request from Succinct's prover network
    pub async fn request_zk_proof(
        &self,
        proof_input: &KeccakInclusionToDataRootProofInput,
    ) -> Result<SessionId, anyhow::Error> {
        debug!("Preparing prover network request and starting proving");
        let zk_client_handle = self.get_zk_client_remote().await;

        // Compute the image_id, then upload the ELF with the image_id as its key.
        let image_id = hex::encode(compute_image_id(KECCAK_INCLUSION_ELF)?);
        zk_client_handle
            .upload_img(&image_id, KECCAK_INCLUSION_ELF.to_vec())
            .await?;

        // Prepare input data and upload it.
        let input_data = to_vec(&proof_input).unwrap();
        let input_data = bytemuck::cast_slice(&input_data).to_vec();
        let input_id = zk_client_handle.upload_input(input_data).await?;

        // Start a session running the prover
        let session = zk_client_handle
            .create_session(image_id, input_id, Vec::new(), false)
            .await?;

        Ok(session)
    }

    /// Await a proof request from Succinct's prover network
    async fn wait_for_zk_proof(&self, request_id: SessionId) -> Result<Receipt, anyhow::Error> {
        debug!("Waiting for proof from prover network");
        let zk_client_handle = self.get_zk_client_remote().await;

        loop {
            let res = request_id.status(&zk_client_handle).await?;
            if res.status == "RUNNING" {
                eprintln!(
                    "Current status: {} - state: {} - continue polling...",
                    res.status,
                    res.state.unwrap_or_default()
                );
                std::thread::sleep(Duration::from_secs(15));
                continue;
            }
            if res.status == "SUCCEEDED" {
                // Download the receipt, containing the output
                let receipt_url = res
                    .receipt_url
                    .expect("API error, missing receipt on completed session");

                let receipt_buf = zk_client_handle.download(&receipt_url).await?;
                let receipt: Receipt = bincode::deserialize(&receipt_buf)?;
                receipt
                    .verify(KECCAK_INCLUSION_ID)
                    .expect("Receipt verification failed");

                return Ok(receipt);
            } else {
                panic!(
                    "Workflow exited: {} - | err: {}",
                    res.status,
                    res.error_msg.unwrap_or_default()
                );
            }
        }
    }

    /// Atomically move a job from the database queue tree to the proof tree.
    /// This removes the job from any further processing by workers.
    /// The [JobStatus] should be success or failure only
    /// (but this is not enforced or checked at this time)
    fn finalize_job(
        &self,
        job_key: &[u8],
        job_status: JobStatus,
    ) -> Result<(), InclusionServiceError> {
        // TODO: do we want to do a status check here? To prevent accidentally getting into a DB invalid state
        (&self.queue_db, &self.finished_db)
            .transaction(|(queue_tx, finished_tx)| {
                queue_tx.remove(job_key)?;
                finished_tx.insert(
                    job_key,
                    bincode::serialize(&job_status).expect("Always given serializable job status"),
                )?;
                Ok::<(), sled::transaction::ConflictableTransactionError<InclusionServiceError>>(())
            })
            .map_err(|e| InclusionServiceError::InternalError(e.to_string()))?;
        Ok(())
    }

    /// Insert a [JobStatus] into a [SledTree] database
    /// AND `send()` this job back to the `self.job_sender` to schedule more progress.
    /// You likely want to pass `self.some_sled_tree` into `data_base` as input.
    pub fn send_job_with_new_status(
        &self,
        job_key: Vec<u8>,
        update_status: JobStatus,
        job: Job,
    ) -> Result<(), InclusionServiceError> {
        debug!("Sending {job:?} back with updated status: {update_status:?}");
        (&self.queue_db, &self.finished_db)
            .transaction(|(queue_tx, finished_tx)| {
                finished_tx.remove(job_key.clone())?;
                queue_tx.insert(
                    job_key.clone(),
                    bincode::serialize(&update_status)
                        .expect("Always given serializable job status"),
                )?;
                Ok::<(), sled::transaction::ConflictableTransactionError<InclusionServiceError>>(())
            })
            .map_err(|e| InclusionServiceError::InternalError(e.to_string()))?;
        self.job_sender
            .send(Some(job))
            .map_err(|e| InclusionServiceError::InternalError(e.to_string()))
    }

    pub async fn get_da_client(&self) -> Result<Arc<CelestiaJSONClient>, InclusionServiceError> {
        let handle = self
            .da_client_handle
            .get_or_try_init(|| async {
                debug!("Building DA client");
                let client = CelestiaJSONClient::new(
                    self.config.da_node_ws.as_str(),
                    self.config.da_node_token.as_str().into(),
                )
                .await
                .map_err(|e| InclusionServiceError::DaClientError(e.to_string()))?;
                Ok(Arc::new(client))
            })
            .await
            .map_err(|e: InclusionServiceError| e)?;
        Ok(handle.clone())
    }

    pub async fn get_zk_client_remote(&self) -> Arc<BonsaiClient> {
        self.zk_client_handle
            .get_or_init(|| async {
                debug!("Building ZK client");
                let client = BonsaiClient::from_env(risc0_zkvm::VERSION)
                    .expect("Failed to create Bonsai client from env vars. Ensure the BONSAI_URL and BONSAI_TOKEN environment variables are set.");
                Arc::new(client)
            })
            .await
            .clone()
    }

    pub fn shutdown(&self) {
        info!("Terminating worker,finishing preexisting jobs");
        let _ = self.job_sender.send(None); // Break loop in `job_worker`
    }
}
