// Copyright © Aptos Foundation

use crate::{
    changing_working_quorum_test_helper, optimize_for_maximum_throughput,
    optimize_state_sync_for_throughput, realistic_network_tuned_for_throughput_test,
    wrap_with_realistic_env, TestCommand,
};
use aptos_forge::{
    success_criteria::{LatencyType, StateProgressThreshold, SuccessCriteria},
    EmitJobMode, EmitJobRequest, ForgeConfig, NodeResourceOverride,
};
use aptos_sdk::types::on_chain_config::{
    BlockGasLimitType, ConsensusAlgorithmConfig, DagConsensusConfigV1, OnChainConsensusConfig,
    OnChainExecutionConfig, TransactionShufflerType, ValidatorTxnConfig,
};
use aptos_testcases::{
    consensus_reliability_tests::ChangingWorkingQuorumTest,
    dag_onchain_enable_test::DagOnChainEnableTest,
    multi_region_network_test::MultiRegionNetworkEmulationTest, two_traffics_test::TwoTrafficsTest,
};
use std::{num::NonZeroUsize, sync::Arc, time::Duration};

pub fn get_dag_test(
    test_name: &str,
    duration: Duration,
    test_cmd: &TestCommand,
) -> Option<ForgeConfig> {
    get_dag_on_realistic_env_test(test_name, duration, test_cmd)
}

/// Attempts to match the test name to a dag-realistic-env test
fn get_dag_on_realistic_env_test(
    test_name: &str,
    duration: Duration,
    test_cmd: &TestCommand,
) -> Option<ForgeConfig> {
    let test = match test_name {
        "dag_realistic_env_max_load" => dag_realistic_env_max_load_test(duration, test_cmd, 100, 0),
        "dag_changing_working_quorum_test" => dag_changing_working_quorum_test(),
        "dag_reconfig_enable_test" => dag_reconfig_enable_test(),
        "dag_realistic_network_tuned_for_throughput_test" => {
            dag_realistic_network_tuned_for_throughput_test()
        },
        _ => return None, // The test name does not match a dag realistic-env test
    };
    Some(test)
}

fn dag_realistic_env_max_load_test(
    duration: Duration,
    test_cmd: &TestCommand,
    num_validators: usize,
    num_fullnodes: usize,
) -> ForgeConfig {
    // Check if HAProxy is enabled
    let ha_proxy = if let TestCommand::K8sSwarm(k8s) = test_cmd {
        k8s.enable_haproxy
    } else {
        false
    };

    // Determine if this is a long running test
    let duration_secs = duration.as_secs();
    let long_running = duration_secs >= 2400;

    // Create the test
    ForgeConfig::default()
        .with_initial_validator_count(NonZeroUsize::new(num_validators).unwrap())
        .with_initial_fullnode_count(num_fullnodes)
        .add_network_test(wrap_with_realistic_env(TwoTrafficsTest {
            inner_traffic: EmitJobRequest::default()
                .mode(EmitJobMode::MaxLoad {
                    mempool_backlog: 400_000,
                })
                .init_gas_price_multiplier(20),
            inner_success_criteria: SuccessCriteria::new(
                if ha_proxy {
                    2000
                } else if long_running {
                    // This is for forge stable
                    2500
                } else {
                    // During land time we want to be less strict, otherwise we flaky fail
                    2800
                },
            ),
        }))
        .with_validator_override_node_config_fn(Arc::new(|config, _| {
            config.consensus.max_sending_block_txns = 4000;
            config.consensus.max_sending_block_bytes = 6 * 1024 * 1024;
            config.consensus.max_receiving_block_txns = 10000;
            config.consensus.max_receiving_block_bytes = 7 * 1024 * 1024;
        }))
        .with_genesis_helm_config_fn(Arc::new(move |helm_values| {
            // Have single epoch change in land blocking, and a few on long-running
            helm_values["chain"]["epoch_duration_secs"] =
                (if long_running { 600 } else { 300 }).into();

            let onchain_consensus_config = OnChainConsensusConfig::V3 {
                alg: ConsensusAlgorithmConfig::DAG(DagConsensusConfigV1::default()),
                vtxn: ValidatorTxnConfig::default_for_genesis(),
            };

            helm_values["chain"]["on_chain_consensus_config"] =
                serde_yaml::to_value(onchain_consensus_config).expect("must serialize");

            let mut on_chain_execution_config = OnChainExecutionConfig::default_for_genesis();
            // Need to update if the default changes
            match &mut on_chain_execution_config {
                OnChainExecutionConfig::Missing
                | OnChainExecutionConfig::V1(_)
                | OnChainExecutionConfig::V2(_)
                | OnChainExecutionConfig::V3(_) => {
                    unreachable!("Unexpected on-chain execution config type, if OnChainExecutionConfig::default_for_genesis() has been updated, this test must be updated too.")
                }
                OnChainExecutionConfig::V4(config_v4) => {
                    config_v4.block_gas_limit_type = BlockGasLimitType::NoLimit;
                    config_v4.transaction_shuffler_type = TransactionShufflerType::Fairness {
                        sender_conflict_window_size: 256,
                        module_conflict_window_size: 2,
                        entry_fun_conflict_window_size: 3,
                    };
                }
            }
            helm_values["chain"]["on_chain_execution_config"] =
                serde_yaml::to_value(on_chain_execution_config).expect("must serialize");
        }))
        // First start higher gas-fee traffic, to not cause issues with TxnEmitter setup - account creation
        .with_emit_job(
            EmitJobRequest::default()
                .mode(EmitJobMode::ConstTps { tps: 100 })
                .gas_price(5 * aptos_global_constants::GAS_UNIT_PRICE)
                .latency_polling_interval(Duration::from_millis(100)),
        )
        .with_success_criteria(
            SuccessCriteria::new(95)
                .add_no_restarts()
                .add_wait_for_catchup_s(
                    // Give at least 60s for catchup, give 10% of the run for longer durations.
                    (duration.as_secs() / 10).max(60),
                )
                .add_latency_threshold(4.0, LatencyType::P50)
                .add_chain_progress(StateProgressThreshold {
                    max_no_progress_secs: 15.0,
                    max_round_gap: 8,
                }),
        )
}

fn dag_changing_working_quorum_test() -> ForgeConfig {
    let epoch_duration = 120;
    let num_large_validators = 0;
    let base_config = changing_working_quorum_test_helper(
        16,
        epoch_duration,
        100,
        70,
        true,
        true,
        ChangingWorkingQuorumTest {
            min_tps: 15,
            always_healthy_nodes: 0,
            max_down_nodes: 16,
            num_large_validators,
            add_execution_delay: false,
            // Use longer check duration, as we are bringing enough nodes
            // to require state-sync to catch up to have consensus.
            check_period_s: 53,
        },
    );

    base_config
        .with_validator_override_node_config_fn(Arc::new(|config, _| {
            config.consensus.max_sending_block_txns = 4000;
            config.consensus.max_sending_block_bytes = 6 * 1024 * 1024;
            config.consensus.max_receiving_block_txns = 10000;
            config.consensus.max_receiving_block_bytes = 7 * 1024 * 1024;
        }))
        .with_genesis_helm_config_fn(Arc::new(move |helm_values| {
            helm_values["chain"]["epoch_duration_secs"] = epoch_duration.into();
            helm_values["genesis"]["validator"]["num_validators_with_larger_stake"] =
                num_large_validators.into();

            let onchain_consensus_config = OnChainConsensusConfig::V3 {
                alg: ConsensusAlgorithmConfig::DAG(DagConsensusConfigV1::default()),
                vtxn: ValidatorTxnConfig::default_for_genesis(),
            };

            helm_values["chain"]["on_chain_consensus_config"] =
                serde_yaml::to_value(onchain_consensus_config).expect("must serialize");
            helm_values["chain"]["on_chain_execution_config"] =
                serde_yaml::to_value(OnChainExecutionConfig::default_for_genesis())
                    .expect("must serialize");
        }))
}

fn dag_reconfig_enable_test() -> ForgeConfig {
    ForgeConfig::default()
        .with_initial_validator_count(NonZeroUsize::new(20).unwrap())
        .with_initial_fullnode_count(20)
        .add_network_test(DagOnChainEnableTest {})
        .with_validator_override_node_config_fn(Arc::new(|config, _| {
            config.consensus.max_sending_block_txns = 4000;
            config.consensus.max_sending_block_bytes = 6 * 1024 * 1024;
            config.consensus.max_receiving_block_txns = 10000;
            config.consensus.max_receiving_block_bytes = 7 * 1024 * 1024;
        }))
        .with_genesis_helm_config_fn(Arc::new(move |helm_values| {
            let mut on_chain_execution_config = OnChainExecutionConfig::default_for_genesis();
            // Need to update if the default changes
            match &mut on_chain_execution_config {
                    OnChainExecutionConfig::Missing
                    | OnChainExecutionConfig::V1(_)
                    | OnChainExecutionConfig::V2(_)
                    | OnChainExecutionConfig::V3(_) => {
                        unreachable!("Unexpected on-chain execution config type, if OnChainExecutionConfig::default_for_genesis() has been updated, this test must be updated too.")
                    }
                    OnChainExecutionConfig::V4(config_v4) => {
                        config_v4.block_gas_limit_type = BlockGasLimitType::NoLimit;
                        config_v4.transaction_shuffler_type = TransactionShufflerType::Fairness {
                            sender_conflict_window_size: 256,
                            module_conflict_window_size: 2,
                            entry_fun_conflict_window_size: 3,
                        };
                    }
            }
            helm_values["chain"]["on_chain_execution_config"] =
                serde_yaml::to_value(on_chain_execution_config).expect("must serialize");
        }))
        .with_success_criteria(
            SuccessCriteria::new(1000)
                .add_no_restarts()
                .add_wait_for_catchup_s(240)
                .add_chain_progress(StateProgressThreshold {
                    max_no_progress_secs: 20.0,
                    max_round_gap: 20,
                }),
        )
}

fn dag_realistic_network_tuned_for_throughput_test() -> ForgeConfig {
    // THE MOST COMMONLY USED TUNE-ABLES:
    const USE_CRAZY_MACHINES: bool = false;
    const ENABLE_VFNS: bool = true;
    const VALIDATOR_COUNT: usize = 100;

    // Config is based on these values. The target TPS should be a slight overestimate of
    // the actual throughput to be able to have reasonable queueing but also so throughput
    // will improve as performance improves.
    // Overestimate: causes mempool and/or batch queueing. Underestimate: not enough txns in blocks.
    const TARGET_TPS: usize = 15_000;
    // Overestimate: causes blocks to be too small. Underestimate: causes blocks that are too large.
    // Ideally, want the block size to take 200-250ms of execution time to match broadcast RTT.
    const MAX_TXNS_PER_BLOCK: usize = 20_000;
    // Overestimate: causes batch queueing. Underestimate: not enough txns in quorum store.
    // This is validator latency, minus mempool queueing time.
    const VN_LATENCY_S: f64 = 2.5;
    // Overestimate: causes mempool queueing. Underestimate: not enough txns incoming.
    const VFN_LATENCY_S: f64 = 4.0;

    let mut forge_config = ForgeConfig::default()
        .with_initial_validator_count(NonZeroUsize::new(VALIDATOR_COUNT).unwrap())
        .add_network_test(MultiRegionNetworkEmulationTest::default())
        .with_emit_job(EmitJobRequest::default().mode(EmitJobMode::MaxLoad {
            mempool_backlog: 100,
        }).txn_expiration_time_secs(600))
        .with_validator_override_node_config_fn(Arc::new(|config, _| {
            // Increase the state sync chunk sizes (consensus blocks are much larger than 1k)
            optimize_state_sync_for_throughput(config);

            optimize_for_maximum_throughput(config, TARGET_TPS, MAX_TXNS_PER_BLOCK, VN_LATENCY_S);

            // Other consensus / Quroum store configs
            config.consensus.quorum_store_pull_timeout_ms = 200;

            // Experimental storage optimizations
            config.storage.rocksdb_configs.enable_storage_sharding = true;

            // Increase the concurrency level
            if USE_CRAZY_MACHINES {
                config.execution.concurrency_level = 48;
            }
        }))
        .with_genesis_helm_config_fn(Arc::new(move |helm_values| {
            let onchain_consensus_config = OnChainConsensusConfig::V3 {
                alg: ConsensusAlgorithmConfig::DAG(DagConsensusConfigV1::default()),
                vtxn: ValidatorTxnConfig::default_for_genesis(),
            };

            helm_values["chain"]["on_chain_consensus_config"] =
                serde_yaml::to_value(onchain_consensus_config).expect("must serialize");

            let mut on_chain_execution_config = OnChainExecutionConfig::default_for_genesis();
            // Need to update if the default changes
            match &mut on_chain_execution_config {
                OnChainExecutionConfig::Missing
                | OnChainExecutionConfig::V1(_)
                | OnChainExecutionConfig::V2(_)
                | OnChainExecutionConfig::V3(_) => {
                    unreachable!("Unexpected on-chain execution config type, if OnChainExecutionConfig::default_for_genesis() has been updated, this test must be updated too.")
                }
                OnChainExecutionConfig::V4(config_v4) => {
                    config_v4.block_gas_limit_type = BlockGasLimitType::NoLimit;
                    config_v4.transaction_shuffler_type = TransactionShufflerType::Fairness {
                        sender_conflict_window_size: 256,
                        module_conflict_window_size: 2,
                        entry_fun_conflict_window_size: 3,
                    };
                }
            }
            helm_values["chain"]["on_chain_execution_config"] =
                serde_yaml::to_value(on_chain_execution_config).expect("must serialize");
        }));

    if ENABLE_VFNS {
        forge_config = forge_config
            .with_initial_fullnode_count(5)
            .with_fullnode_override_node_config_fn(Arc::new(|config, _| {
                // Increase the state sync chunk sizes (consensus blocks are much larger than 1k)
                optimize_state_sync_for_throughput(config);

                // Experimental storage optimizations
                config.storage.rocksdb_configs.enable_storage_sharding = true;

                // Increase the concurrency level
                if USE_CRAZY_MACHINES {
                    config.execution.concurrency_level = 48;
                }
            }));
    }

    if USE_CRAZY_MACHINES {
        forge_config = forge_config
            .with_validator_resource_override(NodeResourceOverride {
                cpu_cores: Some(58),
                memory_gib: Some(200),
            })
            .with_fullnode_resource_override(NodeResourceOverride {
                cpu_cores: Some(58),
                memory_gib: Some(200),
            })
            .with_success_criteria(
                SuccessCriteria::new(25000)
                    .add_no_restarts()
                    /* This test runs at high load, so we need more catchup time */
                    .add_wait_for_catchup_s(120),
                /* Doesn't work without event indices
                .add_chain_progress(StateProgressThreshold {
                    max_no_progress_secs: 10.0,
                    max_round_gap: 4,
                }),
                 */
            );
    } else {
        forge_config = forge_config.with_success_criteria(
            SuccessCriteria::new(12000)
                .add_no_restarts()
                /* This test runs at high load, so we need more catchup time */
                .add_wait_for_catchup_s(120),
            /* Doesn't work without event indices
                .add_chain_progress(StateProgressThreshold {
                     max_no_progress_secs: 10.0,
                     max_round_gap: 4,
                 }),
            */
        );
    }

    forge_config
}
