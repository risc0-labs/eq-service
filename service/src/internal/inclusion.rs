use crate::internal::job::{BoundlessJobId, BoundlessProofResult};
use crate::internal::offer::offer_from_env;
use crate::internal::prom_metrics::PromMetrics;
use crate::{Job, JobStatus};

use boundless_market::alloy::signers::local::PrivateKeySigner;
use boundless_market::alloy::transports::http::reqwest::Url;
use boundless_market::client::Client as BoundlessClient;
use boundless_market::storage::storage_provider_from_env;
use boundless_market::GuestEnv;
use celestia_rpc::{BlobClient, Client as CelestiaJSONClient, HeaderClient, ShareClient};
use eq_common::{ErrorLabels, InclusionServiceError, ZKStackEqProofInput};
use eq_program_keccak_inclusion_builder::EQ_PROGRAM_KECCAK_INCLUSION_ELF;
use jsonrpsee::core::ClientError as JsonRpcError;
use log::{debug, error, info};
use sha3::Digest;
use sha3::Keccak256;
use sled::{Transactional, Tree as SledTree};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, OnceCell};

/// The main service, depends on external DA and ZK clients internally!
pub struct InclusionService {
    pub config: InclusionServiceConfig,
    da_client_handle: OnceCell<Arc<CelestiaJSONClient>>,
    zk_client_handle: OnceCell<Arc<BoundlessClient>>,
    pub metrics: Arc<PromMetrics>,
    pub config_db: SledTree,
    pub queue_db: SledTree,
    pub finished_db: SledTree,
    pub job_sender: mpsc::UnboundedSender<Option<Job>>,
}

impl InclusionService {
    pub fn new(
        config: InclusionServiceConfig,
        da_client_handle: OnceCell<Arc<CelestiaJSONClient>>,
        zk_client_handle: OnceCell<Arc<BoundlessClient>>,
        metrics: Arc<PromMetrics>,
        config_db: SledTree,
        queue_db: SledTree,
        finished_db: SledTree,
        job_sender: mpsc::UnboundedSender<Option<Job>>,
    ) -> Self {
        InclusionService {
            config,
            da_client_handle,
            zk_client_handle,
            metrics,
            config_db,
            queue_db,
            finished_db,
            job_sender,
        }
    }
}

pub struct InclusionServiceConfig {
    pub da_node_token: Option<String>,
    pub da_node_http: String,
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
            let metrics = self.metrics.clone();
            tokio::spawn(async move {
                debug!("Job worker received {job:?}",);
                let _ = service.prove(job).await.map_err(|e| {
                    debug!("COUNTED ERROR METRIC ---{e:?}");
                    count_error(&metrics, e)
                }); //Don't return with "?", we run keep looping
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
                    match self.request_zk_proof(&proof_input, &job, &job_key).await {
                        Ok(zk_job_id) => {
                            debug!("Proof request {zk_job_id:?} started");
                            job_status = JobStatus::ZkProofPending(zk_job_id);
                            self.send_job_with_new_status(job_key, job_status, job)?;
                        }
                        Err(e) => {
                            error!("{job:?} failed progressing DataAvailable: {e}");
                            // NOTE: we internally finalize the job in `handle_da_client_error`
                        }
                    };
                }
                JobStatus::ZkProofPending(zk_request_id) => {
                    debug!("ZK request waiting");
                    match self.wait_for_zk_proof(&job, &job_key, zk_request_id).await {
                        Ok(zk_proof) => {
                            info!("🎉 {job:?} Finished!");
                            job_status = JobStatus::ZkProofFinished(zk_proof);
                            self.finalize_job(&job_key, job_status)?;
                            self.metrics.jobs_finished.inc();
                        }
                        Err(e) => {
                            error!("{job:?} failed progressing ZkProofPending: {e}");
                            // NOTE: we internally finalize the job in `handle_zk_client_error`
                        }
                    }
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
        let proof_input = ZKStackEqProofInput {
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
            batch_number: job.batch_number,
            chain_id: job.l2_chain_id,
            author: blob.signer,
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

    /// Start a proof request from Boundless prover network
    pub async fn request_zk_proof(
        &self,
        proof_input: &ZKStackEqProofInput,
        job: &Job,
        _job_key: &[u8],
    ) -> Result<BoundlessJobId, InclusionServiceError> {
        debug!("Preparing prover network request and starting proving");
        let client = self.get_zk_client_remote().await;

        let env = GuestEnv::builder()
            .write(proof_input)
            .map_err(|_| {
                InclusionServiceError::ZkClientError(
                    "Failed to serialize ZK proof input".to_string(),
                )
            })?
            .build_env();

        let request = client
            .new_request()
            .with_program(EQ_PROGRAM_KECCAK_INCLUSION_ELF)
            .with_env(env)
            .with_offer(offer_from_env());

        // Submit the request directly
        let (id, expires_at) = client.submit_onchain(request).await.map_err(|e| {
            InclusionServiceError::ZkClientError(format!(
                "Error submitting proof for job {job} {e}"
            ))
        })?;

        info!("Proof request submitted 🚀 https://explorer.beboundless.xyz/orders/{id:x}");

        Ok(BoundlessJobId { id, expires_at })
    }

    /// Await a proof request from Boundless prover network
    async fn wait_for_zk_proof(
        &self,
        job: &Job,
        _job_key: &[u8],
        request_id: BoundlessJobId,
    ) -> Result<BoundlessProofResult, InclusionServiceError> {
        debug!("Waiting for proof from prover network");
        let start_time = Instant::now();
        let client = self.get_zk_client_remote().await;

        let (journal, seal) = client
            .wait_for_request_fulfillment(
                request_id.id,
                Duration::from_secs(5), // check every 5 seconds
                request_id.expires_at,
            )
            .await
            .map_err(|e| {
                InclusionServiceError::ZkClientError(format!(
                    "Error retrieving proof result for job {job} {e}"
                ))
            })?;

        // Record the time taken to wait for the ZK proof
        let duration = start_time.elapsed();
        self.metrics
            .zk_proof_wait_time
            .observe(duration.as_secs_f64().round());

        Ok(BoundlessProofResult { journal, seal })
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
                    self.config.da_node_http.as_str(),
                    self.config.da_node_token.as_deref(),
                )
                .await
                .map_err(|e| InclusionServiceError::DaClientError(e.to_string()))?;
                Ok(Arc::new(client))
            })
            .await
            .map_err(|e: InclusionServiceError| e)?;
        Ok(handle.clone())
    }

    pub async fn get_zk_client_remote(&self) -> Arc<BoundlessClient> {
        self.zk_client_handle
            .get_or_init(|| async {
                debug!("Building ZK client");

                let rpc_url = std::env::var("RPC_URL")
                    .expect("RPC_URL required to interact with Boundless market");

                let private_key: PrivateKeySigner = std::env::var("PRIVATE_KEY")
                    .expect("PRIVATE_KEY must be set to interact with Boundless market")
                    .parse()
                    .expect("PRIVATE_KEY must be a valid private key");

                let client = BoundlessClient::builder()
                    .with_rpc_url(Url::parse(&rpc_url).unwrap())
                    .with_private_key(private_key)
                    .with_storage_provider(Some(
                        storage_provider_from_env()
                            .expect("Could not build Boundless storage provider from env. Check PINATA_* or AWS_* vars"),
                    ))
                    .build()
                    .await
                    .expect("Failed to create Boundless client");

                Arc::new(client)
            })
            .await
            .clone()
    }

    pub fn shutdown(&self) {
        info!("Terminating worker, finishing preexisting jobs");
        let _ = self.job_sender.send(None); // Break loop in `job_worker`
    }
}

/// Helper to count/log the error for Prometheus metrics
fn count_error(metrics: &PromMetrics, e: InclusionServiceError) {
    let _ = metrics
        .jobs_errors
        .get_or_create(&ErrorLabels { error_type: e })
        .inc();
}
