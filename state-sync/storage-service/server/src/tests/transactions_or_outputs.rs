// Copyright (c) Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::tests::{mock, mock::MockClient, utils};
use aptos_config::config::StorageServiceConfig;
use aptos_storage_service_types::{
    requests::{DataRequest, TransactionsOrOutputsWithProofRequest},
    responses::{DataResponse, StorageServiceResponse},
    StorageServiceError,
};
use aptos_types::transaction::{TransactionListWithProof, TransactionOutputListWithProof};
use claims::assert_matches;
use mockall::{predicate::eq, Sequence};

#[tokio::test]
async fn test_get_transactions_or_outputs_with_proof() {
    // Test small and large chunk requests
    let max_output_chunk_size = StorageServiceConfig::default().max_transaction_output_chunk_size;
    for chunk_size in [1, 100, max_output_chunk_size] {
        // Test fallback to transaction syncing
        for fallback_to_transactions in [false, true] {
            // Create test data
            let start_version = 0;
            let end_version = start_version + chunk_size - 1;
            let proof_version = end_version;
            let output_list_with_proof =
                utils::create_output_list_with_proof(start_version, end_version, proof_version);
            let transaction_list_with_proof = utils::create_transaction_list_with_proof(
                start_version,
                start_version,
                proof_version,
                false,
            ); // Creates a small transaction list

            // Create the mock db reader
            let max_num_output_reductions = 5;
            let mut db_reader = mock::create_mock_db_reader();
            for i in 0..=max_num_output_reductions {
                utils::expect_get_transaction_outputs(
                    &mut db_reader,
                    start_version,
                    (chunk_size as u32 / (u32::pow(2, i as u32))) as u64,
                    proof_version,
                    output_list_with_proof.clone(),
                );
            }
            if fallback_to_transactions {
                utils::expect_get_transactions(
                    &mut db_reader,
                    start_version,
                    chunk_size,
                    proof_version,
                    false,
                    transaction_list_with_proof.clone(),
                );
            }

            // Create the storage client and server
            let storage_config = utils::configure_network_chunk_limit(
                fallback_to_transactions,
                &output_list_with_proof,
                &transaction_list_with_proof,
            );
            let (mut mock_client, mut service, _, _, _) =
                MockClient::new(Some(db_reader), Some(storage_config));
            utils::update_storage_server_summary(&mut service, proof_version + 100, 10);
            tokio::spawn(service.start());

            // Create a request to fetch transactions or outputs with a proof
            let response = get_transactions_or_outputs_with_proof(
                &mut mock_client,
                start_version,
                end_version,
                end_version,
                false,
                max_num_output_reductions,
                true,
            )
            .await
            .unwrap();

            // Verify the response is correct
            verify_transactions_or_output_response(
                fallback_to_transactions,
                &output_list_with_proof,
                &transaction_list_with_proof,
                &response,
            );
        }
    }
}

#[tokio::test]
async fn test_get_transactions_or_outputs_with_proof_chunk_limit() {
    // Test fallback to transaction syncing
    for fallback_to_transactions in [false, true] {
        // Create test data
        let max_output_chunk_size =
            StorageServiceConfig::default().max_transaction_output_chunk_size;
        let max_transaction_chunk_size = StorageServiceConfig::default().max_transaction_chunk_size;
        let chunk_size = max_output_chunk_size * 10; // Set a chunk request larger than the max
        let start_version = 0;
        let end_version = start_version + max_output_chunk_size - 1;
        let proof_version = end_version;
        let output_list_with_proof =
            utils::create_output_list_with_proof(start_version, end_version, proof_version);
        let transaction_list_with_proof = utils::create_transaction_list_with_proof(
            start_version,
            start_version,
            proof_version,
            false,
        ); // Creates a small transaction list

        // Create the mock db reader
        let mut db_reader = mock::create_mock_db_reader();
        utils::expect_get_transaction_outputs(
            &mut db_reader,
            start_version,
            max_output_chunk_size,
            proof_version,
            output_list_with_proof.clone(),
        );
        if fallback_to_transactions {
            utils::expect_get_transactions(
                &mut db_reader,
                start_version,
                max_transaction_chunk_size,
                proof_version,
                false,
                transaction_list_with_proof.clone(),
            );
        }

        // Create the storage client and server
        let storage_config = utils::configure_network_chunk_limit(
            fallback_to_transactions,
            &output_list_with_proof,
            &transaction_list_with_proof,
        );
        let (mut mock_client, mut service, _, _, _) =
            MockClient::new(Some(db_reader), Some(storage_config));
        utils::update_storage_server_summary(&mut service, proof_version + chunk_size, 10);
        tokio::spawn(service.start());

        // Create a request to fetch transactions outputs with a proof
        let response = get_transactions_or_outputs_with_proof(
            &mut mock_client,
            start_version,
            start_version + chunk_size - 1,
            end_version,
            false,
            0,
            false,
        )
        .await
        .unwrap();

        // Verify the response is correct
        verify_transactions_or_output_response(
            fallback_to_transactions,
            &output_list_with_proof,
            &transaction_list_with_proof,
            &response,
        );
    }
}

#[tokio::test]
async fn test_get_transactions_or_outputs_with_proof_invalid() {
    // Create the storage client and server
    let (mut mock_client, service, _, _, _) = MockClient::new(None, None);
    tokio::spawn(service.start());

    // Test invalid ranges
    let start_version = 1000;
    for end_version in [0, 999] {
        let response = get_transactions_or_outputs_with_proof(
            &mut mock_client,
            start_version,
            end_version,
            end_version,
            false,
            3,
            true,
        )
        .await
        .unwrap_err();
        assert_matches!(response, StorageServiceError::InvalidRequest(_));
    }
}

#[tokio::test]
async fn test_get_transactions_or_outputs_with_proof_network_limit() {
    // Test different byte limits
    for network_limit_bytes in [1, 2 * 1024, 10 * 1024, 30 * 1024] {
        get_transactions_or_outputs_with_proof_network_limit(network_limit_bytes).await;
    }
}

#[tokio::test]
async fn test_get_transactions_or_outputs_with_proof_not_serviceable() {
    // Test small and large chunk requests
    let max_output_chunk_size = StorageServiceConfig::default().max_transaction_output_chunk_size;
    for chunk_size in [2, 100, max_output_chunk_size] {
        // Create test data
        let start_version = 0;
        let end_version = start_version + chunk_size - 1;
        let proof_version = end_version;

        // Create the storage client and server (that cannot service the request)
        let (mut mock_client, mut service, _, _, _) = MockClient::new(None, None);
        utils::update_storage_server_summary(&mut service, proof_version - 1, 10);
        tokio::spawn(service.start());

        // Create a request to fetch transactions or outputs with a proof
        let response = get_transactions_or_outputs_with_proof(
            &mut mock_client,
            start_version,
            end_version,
            end_version,
            false,
            5,
            true,
        )
        .await
        .unwrap_err();

        // Verify the request is not serviceable
        assert_matches!(response, StorageServiceError::InvalidRequest(_));
    }
}

/// Sends a transaction or outputs with proof request and processes the response
async fn get_transactions_or_outputs_with_proof(
    mock_client: &mut MockClient,
    start_version: u64,
    end_version: u64,
    proof_version: u64,
    include_events: bool,
    max_num_output_reductions: u64,
    use_compression: bool,
) -> Result<StorageServiceResponse, StorageServiceError> {
    let data_request =
        DataRequest::GetTransactionsOrOutputsWithProof(TransactionsOrOutputsWithProofRequest {
            proof_version,
            start_version,
            end_version,
            include_events,
            max_num_output_reductions,
        });
    utils::send_storage_request(mock_client, use_compression, data_request).await
}

/// A helper method to request transactions or outputs with proof using the
/// the specified network limit.
async fn get_transactions_or_outputs_with_proof_network_limit(network_limit_bytes: u64) {
    for use_compression in [true, false] {
        for include_events in [true, false] {
            // Create test data
            let min_bytes_per_output = 2500; // 2.5 KB
            let min_bytes_per_transaction = 499; // 0.5 KB
            let start_version = 455;
            let proof_version = 1000000;
            let max_output_size = StorageServiceConfig::default().max_transaction_output_chunk_size;
            let max_transaction_size = StorageServiceConfig::default().max_transaction_chunk_size;

            // Create the mock db reader
            let mut db_reader = mock::create_mock_db_reader();
            let mut expectation_sequence = Sequence::new();
            let mut chunk_size = max_output_size;
            let mut max_num_output_reductions = 0;
            while chunk_size >= 1 {
                let output_list_with_proof = utils::create_output_list_using_sizes(
                    start_version,
                    chunk_size,
                    min_bytes_per_output,
                );
                db_reader
                    .expect_get_transaction_outputs()
                    .times(1)
                    .with(eq(start_version), eq(chunk_size), eq(proof_version))
                    .in_sequence(&mut expectation_sequence)
                    .returning(move |_, _, _| Ok(output_list_with_proof.clone()));
                chunk_size /= 2;
                max_num_output_reductions += 1;
            }
            let mut chunk_size = max_transaction_size;
            while chunk_size >= 1 {
                let transaction_list_with_proof = utils::create_transaction_list_using_sizes(
                    start_version,
                    chunk_size,
                    min_bytes_per_transaction,
                    include_events,
                );
                db_reader
                    .expect_get_transactions()
                    .times(1)
                    .with(
                        eq(start_version),
                        eq(chunk_size),
                        eq(proof_version),
                        eq(include_events),
                    )
                    .in_sequence(&mut expectation_sequence)
                    .returning(move |_, _, _, _| Ok(transaction_list_with_proof.clone()));
                chunk_size /= 2;
            }

            // Create the storage client and server
            let storage_config = StorageServiceConfig {
                max_network_chunk_bytes: network_limit_bytes,
                ..Default::default()
            };
            let (mut mock_client, mut service, _, _, _) =
                MockClient::new(Some(db_reader), Some(storage_config));
            utils::update_storage_server_summary(&mut service, proof_version + 100, 10);
            tokio::spawn(service.start());

            // Process a request to fetch transactions or outputs with a proof
            let response = get_transactions_or_outputs_with_proof(
                &mut mock_client,
                start_version,
                start_version + (max_output_size * 10), // Request more than the max chunk
                proof_version,
                include_events,
                max_num_output_reductions,
                use_compression,
            )
            .await
            .unwrap();

            // Verify the response is correct
            match response.get_data_response().unwrap() {
                DataResponse::TransactionsOrOutputsWithProof(
                    transactions_or_outputs_with_proof,
                ) => {
                    let (transactions_with_proof, outputs_with_proof) =
                        transactions_or_outputs_with_proof;

                    if let Some(transactions_with_proof) = transactions_with_proof {
                        let num_response_bytes =
                            bcs::serialized_size(&transactions_with_proof).unwrap() as u64;
                        let num_transactions = transactions_with_proof.transactions.len() as u64;
                        if num_response_bytes > network_limit_bytes {
                            assert_eq!(num_transactions, 1); // Data cannot be reduced more than a single item
                        } else {
                            let max_transactions = network_limit_bytes / min_bytes_per_transaction;
                            assert!(num_transactions <= max_transactions);
                        }
                    } else if let Some(outputs_with_proof) = outputs_with_proof {
                        let num_response_bytes =
                            bcs::serialized_size(&outputs_with_proof).unwrap() as u64;
                        let num_outputs = outputs_with_proof.transactions_and_outputs.len() as u64;
                        if num_response_bytes > network_limit_bytes {
                            assert_eq!(num_outputs, 1); // Data cannot be reduced more than a single item
                        } else {
                            let max_outputs = network_limit_bytes / min_bytes_per_output;
                            assert!(num_outputs <= max_outputs);
                        }
                    } else {
                        panic!("No transactions or outputs were returned!");
                    }
                },
                _ => panic!(
                    "Expected transactions or outputs with proof but got: {:?}",
                    response
                ),
            };
        }
    }
}

/// Verifies that a transactions or outputs with proof response is received
/// and that the response contains the correct data.
fn verify_transactions_or_output_response(
    fallback_to_transactions: bool,
    output_list_with_proof: &TransactionOutputListWithProof,
    transaction_list_with_proof: &TransactionListWithProof,
    response: &StorageServiceResponse,
) {
    match response.get_data_response().unwrap() {
        DataResponse::TransactionsOrOutputsWithProof(transactions_or_outputs_with_proof) => {
            let (transactions_with_proof, outputs_with_proof) = transactions_or_outputs_with_proof;
            if fallback_to_transactions {
                assert_eq!(
                    transactions_with_proof.unwrap(),
                    transaction_list_with_proof.clone()
                );
            } else {
                assert_eq!(outputs_with_proof.unwrap(), output_list_with_proof.clone());
            }
        },
        _ => panic!(
            "Expected transactions or outputs with proof but got: {:?}",
            response
        ),
    };
}
