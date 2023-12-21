/// This module defines structs and methods to initialize the gas schedule, which dictates how much
/// it costs to execute Move on the network.
module aptos_framework::gas_schedule {
    use std::config_for_next_epoch;
    use std::error;
    use std::string::String;
    use std::vector;

    use aptos_framework::reconfiguration;
    use aptos_framework::system_addresses;
    use aptos_framework::util::from_bytes;
    use aptos_framework::storage_gas::StorageGasConfig;
    use aptos_framework::storage_gas;

    friend aptos_framework::genesis;
    friend aptos_framework::reconfiguration_with_dkg;

    /// The provided gas schedule bytes are empty or invalid
    const EINVALID_GAS_SCHEDULE: u64 = 1;
    const EINVALID_GAS_FEATURE_VERSION: u64 = 2;
    const EAPI_DISABLED: u64 = 3;

    struct GasEntry has store, copy, drop {
        key: String,
        val: u64,
    }

    struct GasSchedule has key, copy, drop {
        entries: vector<GasEntry>
    }

    struct GasScheduleV2 has key, copy, drop, store {
        feature_version: u64,
        entries: vector<GasEntry>,
    }

    /// Only called during genesis.
    public(friend) fun initialize(aptos_framework: &signer, gas_schedule_blob: vector<u8>) {
        system_addresses::assert_aptos_framework(aptos_framework);
        assert!(!vector::is_empty(&gas_schedule_blob), error::invalid_argument(EINVALID_GAS_SCHEDULE));

        // TODO(Gas): check if gas schedule is consistent
        let gas_schedule: GasScheduleV2 = from_bytes(gas_schedule_blob);
        move_to<GasScheduleV2>(aptos_framework, gas_schedule);
    }

    /// This can be called by on-chain governance to update the gas schedule.
    public fun set_gas_schedule(aptos_framework: &signer, gas_schedule_blob: vector<u8>) acquires GasSchedule, GasScheduleV2 {
        assert!(!std::features::reconfigure_with_dkg_enabled(), error::invalid_state(EAPI_DISABLED));
        system_addresses::assert_aptos_framework(aptos_framework);
        assert!(!vector::is_empty(&gas_schedule_blob), error::invalid_argument(EINVALID_GAS_SCHEDULE));

        if (exists<GasScheduleV2>(@aptos_framework)) {
            let gas_schedule = borrow_global_mut<GasScheduleV2>(@aptos_framework);
            let new_gas_schedule: GasScheduleV2 = from_bytes(gas_schedule_blob);
            assert!(new_gas_schedule.feature_version >= gas_schedule.feature_version,
                error::invalid_argument(EINVALID_GAS_FEATURE_VERSION));
            // TODO(Gas): check if gas schedule is consistent
            *gas_schedule = new_gas_schedule;
        }
        else {
            if (exists<GasSchedule>(@aptos_framework)) {
                _ = move_from<GasSchedule>(@aptos_framework);
            };
            let new_gas_schedule: GasScheduleV2 = from_bytes(gas_schedule_blob);
            // TODO(Gas): check if gas schedule is consistent
            move_to<GasScheduleV2>(aptos_framework, new_gas_schedule);
        };

        // Need to trigger reconfiguration so validator nodes can sync on the updated gas schedule.
        reconfiguration::reconfigure();
    }

    /// Set the gas schedule for the next epoch, typically called by on-chain governance.
    /// Unlike `set_gas_schedule()`, the change will not take effect until a reconfiguration with DKG is done.
    public fun set_for_next_epoch(aptos_framework: &signer, gas_schedule_blob: vector<u8>) {
        assert!(std::features::reconfigure_with_dkg_enabled(), error::invalid_state(EAPI_DISABLED));
        system_addresses::assert_aptos_framework(aptos_framework);
        assert!(!vector::is_empty(&gas_schedule_blob), error::invalid_argument(EINVALID_GAS_SCHEDULE));
        let new_gas_schedule: GasScheduleV2 = from_bytes(gas_schedule_blob);
        config_for_next_epoch::upsert(aptos_framework, new_gas_schedule);
    }

    /// Apply the pending gas schedule changes, typically called in `block_prologue_ext()`.
    public(friend) fun on_new_epoch(account: &signer) acquires GasScheduleV2 {
        assert!(std::features::reconfigure_with_dkg_enabled(), error::invalid_state(EAPI_DISABLED));
        if (config_for_next_epoch::does_exist<GasScheduleV2>()) {
            let new_gas_schedule: GasScheduleV2 = config_for_next_epoch::extract<GasScheduleV2>(account);
            let gas_schedule = borrow_global_mut<GasScheduleV2>(@aptos_framework);
            *gas_schedule = new_gas_schedule;
        }
    }

    public fun set_storage_gas_config(aptos_framework: &signer, config: StorageGasConfig) {
        storage_gas::set_config(aptos_framework, config);
        if (!std::features::reconfigure_with_dkg_enabled()) {
            // Need to trigger reconfiguration so the VM is guaranteed to load the new gas fee starting from the next
            // transaction.
            reconfiguration::reconfigure();
        }
    }
}
