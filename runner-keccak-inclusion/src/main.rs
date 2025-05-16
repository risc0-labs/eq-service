#![doc = include_str!("../README.md")]

use eq_common::KeccakInclusionToDataRootProofInput;
use eq_program_keccak_inclusion::EQ_PROGRAM_KECCAK_INCLUSION_GUEST_ELF as KECCAK_INCLUSION_ELF;
use risc0_zkvm::{default_executor, ExecutorEnv};
use std::fs;

fn main() {
    let input_json =
        fs::read_to_string("../blob-tool/proof_input.json").expect("Failed reading proof input");
    let input: KeccakInclusionToDataRootProofInput =
        serde_json::from_str(&input_json).expect("Failed deserializing proof input");

    let env = ExecutorEnv::builder()
        .write(&input)
        .unwrap()
        .build()
        .unwrap();
    // client
    //     .execute(KECCAK_INCLUSION_ELF, &stdin)
    //     .run()
    //     .expect("Failed executing program");

    /*let (pk, _vk) = client.setup(&KECCAK_INCLUSION_ELF);
    let proof = client
        .prove(&pk, &stdin)
        .groth16()
        .run()
        .expect("Failed proving");
    fs::write(
        "sample_groth16_proof.json",
        serde_json::to_string(&proof).expect("Failed to serialize proof"),
    )
    .expect("Failed to write proof to file");*/

    let exec = default_executor();
    let _r = exec
        .execute(env, &KECCAK_INCLUSION_ELF)
        .expect("Failed executing program");
    print!("✅ Proof seems OK! Execution completed without issue.\n");
}
