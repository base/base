# System test timings

Per-test durations from 3 CI run(s) of the `System Tests` job. Regenerate with
`etc/scripts/ci/system_test_timings.py` (see its docstring) when the configuration changes.

**These are wall-clock times under load, not the cost of a test on its own.** Tests share the
runner and the shared L1, so a test's time includes waiting on CPU, on the deployment lock and
on other tests' Docker work. Locally one `upgrade_signal` test took 18-30s alone but 100s+ at
six threads. To find what a test really costs, run it alone, and compare the total below
against it before and after a change, not individual rows.

Test phase (nextest summary): 313s, 304s, 310s.
Tests listed: 78 taking at least 1s; 53 faster tests, totalling 1s, are omitted.

| Test | Median (s) | Min (s) | Max (s) | Runs | Share of summed time |
| --- | ---: | ---: | ---: | ---: | ---: |
| `l1_reorg_recovery::sequencer_recovers_from_l1_outage_and_deep_reorg` | 155.3 | 155.2 | 155.7 | 3 | 7.0% |
| `upgrade_signal::test_upgrade_signal_runtime_admin_reapplies_live_schedule_change` | 77.6 | 76.7 | 77.7 | 3 | 3.5% |
| `upgrade_signal::test_upgrade_signal_startup_apply_sets_rollup_config` | 74.1 | 73.2 | 75.4 | 3 | 3.4% |
| `shadow_sequencer::shadow_builds_privately_then_reconciles_to_canonical` | 72.8 | 70.9 | 75.3 | 3 | 3.3% |
| `upgrade_signal::test_upgrade_signal_runtime_admin_applies_new_live_schedule` | 71.0 | 68.2 | 76.9 | 3 | 3.2% |
| `smoke::smoke_test_system_block_production_and_transactions` | 59.1 | 58.7 | 59.3 | 3 | 2.7% |
| `builder_cutover::zenith_keeps_native_builder_and_denim_cadence` | 56.9 | 56.6 | 60.5 | 3 | 2.6% |
| `builder_cutover::cuts_over_builder_and_block_time_at_denim` | 54.9 | 54.4 | 55.0 | 3 | 2.5% |
| `upgrade_signal::test_upgrade_signal_metrics_only_does_not_mutate_schedule` | 54.7 | 50.6 | 59.9 | 3 | 2.5% |
| `shadow_sequencer::late_shadow_catches_up_then_reconciles_private_blocks` | 53.2 | 53.0 | 55.3 | 3 | 2.4% |
| `upgrade_signal::test_upgrade_signal_execution_runtime_admin_reapplies_live_schedule_change` | 51.1 | 51.1 | 55.9 | 3 | 2.3% |
| `shadow_sequencer::shadow_reconciles_across_multiple_cycles` | 49.0 | 47.4 | 50.9 | 3 | 2.2% |
| `upgrade_signal::test_upgrade_signal_follow_mode_startup_apply_sets_rollup_config` | 48.6 | 45.4 | 49.8 | 3 | 2.2% |
| `smoke::smoke_test_builder_and_client_block_sync` | 48.3 | 46.3 | 48.9 | 3 | 2.2% |
| `smoke::smoke_test_client_pending_state_via_flashblocks` | 46.6 | 45.3 | 52.4 | 3 | 2.1% |
| `upgrade_signal::test_upgrade_signal_execution_startup_apply_sets_chain_spec` | 46.3 | 43.2 | 47.3 | 3 | 2.1% |
| `policy_registry::test_policy_registry_lifecycle_and_error_paths` | 44.3 | 43.6 | 44.8 | 3 | 2.0% |
| `shadow_indexer::shadow_indexer_persists_no_canonical_blocks` | 41.8 | 39.5 | 44.0 | 3 | 1.9% |
| `tx_forwarding::test_tx_forwarding_pipeline_system` | 41.1 | 40.0 | 42.9 | 3 | 1.9% |
| `tx_forwarding::test_tx_forwarding_pipeline_system_high_load` | 40.2 | 39.3 | 40.8 | 3 | 1.8% |
| `policy_transfer::test_allowlist_gates_transfer` | 39.0 | 38.7 | 40.2 | 3 | 1.8% |
| `b20_precompile::test_b20_asset_extension_via_rpc` | 37.2 | 35.3 | 37.2 | 3 | 1.7% |
| `policy_transfer::test_blocklist_gates_transfer` | 36.4 | 36.0 | 36.9 | 3 | 1.6% |
| `tx_forwarding::test_insert_validated_transaction_single` | 35.6 | 35.3 | 37.2 | 3 | 1.6% |
| `b20_precompile::test_b20_pause_and_unpause` | 35.6 | 33.6 | 38.3 | 3 | 1.6% |
| `b20_precompile::test_b20_mint_and_burn` | 33.5 | 32.3 | 34.4 | 3 | 1.5% |
| `activation_registry::test_activation_registry_admin_lifecycle` | 32.3 | 30.2 | 32.8 | 3 | 1.5% |
| `gossip_topic_retirement::retired_topics_preserve_connected_peers_and_unsafe_sync` | 30.5 | 27.0 | 32.5 | 3 | 1.4% |
| `b20_precompile::test_b20_metadata_updates` | 30.2 | 28.9 | 31.4 | 3 | 1.4% |
| `b20_precompile::test_b20_approve_and_transfer_from` | 29.5 | 27.4 | 29.7 | 3 | 1.3% |
| `b20_precompile::test_b20_supply_cap` | 28.9 | 27.1 | 29.4 | 3 | 1.3% |
| `policy_transfer::test_always_block_policy_blocks_transfer` | 28.1 | 26.2 | 28.6 | 3 | 1.3% |
| `policy_registry::test_policy_registry_deactivated_views_and_write_gate` | 27.3 | 27.2 | 28.5 | 3 | 1.2% |
| `fuzz_sync_parity::fuzz_sync_parity` | 26.2 | 25.2 | 29.6 | 3 | 1.2% |
| `activation_registry::test_activation_registry_check_activated_gate` | 26.1 | 24.3 | 27.0 | 3 | 1.2% |
| `activation_registry::test_activation_registry_cobalt_admin_rotation` | 25.8 | 25.7 | 27.5 | 3 | 1.2% |
| `b20_precompile::test_b20_transfer_with_memo` | 25.1 | 22.5 | 25.2 | 3 | 1.1% |
| `b20_precompile::test_b20_stablecoin_create_and_currency_via_rpc` | 24.8 | 23.8 | 25.1 | 3 | 1.1% |
| `b20_precompile::test_beryl_precompiles_do_not_execute_before_activation_block` | 24.3 | 23.7 | 24.6 | 3 | 1.1% |
| `b20_precompile::test_b20_create_token_duplicate_reverts` | 24.0 | 21.9 | 24.3 | 3 | 1.1% |
| `activation_registry::test_activation_registry_set_admin_reverts_before_cobalt` | 23.5 | 21.4 | 24.1 | 3 | 1.1% |
| `b20_precompile::test_b20_factory_create_and_transfer_via_rpc` | 23.2 | 23.1 | 24.5 | 3 | 1.1% |
| `activation_registry::test_activation_registry_unauthorized_activate_reverts` | 22.3 | 20.3 | 24.0 | 3 | 1.0% |
| `b20_precompile::test_b20_factory_predict_and_is_b20` | 22.0 | 21.1 | 22.2 | 3 | 1.0% |
| `b20_precompile::test_b20_token_metadata` | 21.9 | 20.3 | 22.3 | 3 | 1.0% |
| `policy_registry::test_policy_registry_create_policy_emits_events` | 21.8 | 19.9 | 22.1 | 3 | 1.0% |
| `b20_precompile::test_b20_stablecoin_variant_create_via_rpc` | 21.7 | 20.1 | 22.9 | 3 | 1.0% |
| `eip8130::eip8130_transaction_is_mined` | 21.3 | 21.0 | 21.7 | 3 | 1.0% |
| `policy_registry::test_policy_registry_policy_exists` | 21.0 | 20.0 | 21.1 | 3 | 1.0% |
| `activation_registry::test_activation_registry_admin` | 21.0 | 19.0 | 21.2 | 3 | 1.0% |
| `in_process_zk::in_process_prover_and_zk_host_start` | 20.7 | 19.8 | 20.8 | 3 | 0.9% |
| `tx_forwarding::test_validity_block_predicates_defer_and_expire_transactions` | 20.7 | 19.3 | 21.4 | 3 | 0.9% |
| `activation_registry::test_activation_registry_is_activated_default` | 19.4 | 18.0 | 20.0 | 3 | 0.9% |
| `tx_forwarding::test_invalid_validity_batches_are_rejected_at_mempool_ingress` | 19.3 | 17.0 | 21.7 | 3 | 0.9% |
| `tx_forwarding::test_matching_validity_predicates_are_forwarded_and_included` | 19.2 | 17.3 | 19.6 | 3 | 0.9% |
| `tx_forwarding::test_eip8130_validity_transaction_is_included_by_native_builder` | 18.6 | 17.2 | 20.9 | 3 | 0.8% |
| `tx_forwarding::test_validity_transaction_submitted_directly_to_builder_is_included` | 18.5 | 17.4 | 19.0 | 3 | 0.8% |
| `smoke::denim_and_zenith_activation_matches_el_and_cl_configs` | 15.9 | 14.0 | 16.7 | 3 | 0.7% |
| `shadow_retention::deletes_a_large_backlog_across_several_batches` | 5.6 | 5.5 | 6.2 | 3 | 0.3% |
| `shadow_blocks_reconciliation::a_canonical_ref_does_not_resolve_a_candidate_stored_after_it` | 5.2 | 5.1 | 5.6 | 3 | 0.2% |
| `shadow_retention::deletes_only_rows_older_than_the_retention_period` | 4.8 | 4.5 | 5.1 | 3 | 0.2% |
| `shadow_blocks_reconciliation::a_later_candidate_at_a_height_does_not_inherit_the_replaced_hash` | 3.3 | 2.3 | 4.6 | 3 | 0.2% |
| `shadow_blocks_reconciliation::the_backlog_index_survives_dropping_the_column_its_predicate_named` | 1.9 | 1.5 | 2.0 | 3 | 0.1% |
| `shadow_blocks_reconciliation::the_database_still_rejects_a_hash_that_is_not_lowercase_0x_hex` | 1.9 | 1.5 | 2.1 | 3 | 0.1% |
| `shadow_blocks_reconciliation::canonical_block_never_clears_an_established_hash` | 1.9 | 1.8 | 2.0 | 3 | 0.1% |
| `shadow_blocks_reconciliation::a_row_is_retrievable_by_the_hash_string_it_was_stored_under` | 1.9 | 1.6 | 2.1 | 3 | 0.1% |
| `shadow_blocks_reconciliation::list_recent_returns_resolved_rows_newest_first_and_pages_by_before` | 1.9 | 1.2 | 2.1 | 3 | 0.1% |
| `shadow_retention::keeps_every_row_inside_the_retention_period` | 1.8 | 1.3 | 2.1 | 3 | 0.1% |
| `shadow_blocks_reconciliation::a_replacement_candidate_does_not_inherit_the_previous_creation_time` | 1.8 | 1.6 | 3.8 | 3 | 0.1% |
| `shadow_blocks_reconciliation::an_unresolved_row_still_registers_in_the_backlog_after_the_contract` | 1.8 | 1.5 | 2.1 | 3 | 0.1% |
| `shadow_blocks_reconciliation::hashes_are_stored_as_text_the_etl_can_read` | 1.7 | 1.7 | 2.0 | 3 | 0.1% |
| `shadow_blocks_reconciliation::unresolved_backlog_counts_rows_awaiting_a_canonical_block` | 1.7 | 1.4 | 2.1 | 3 | 0.1% |
| `shadow_retention::yields_when_another_builder_holds_the_retention_lock` | 1.7 | 1.4 | 2.0 | 3 | 0.1% |
| `system_config::tests::snapshot_datadirs_must_not_alias` | 1.1 | 1.0 | 1.2 | 3 | 0.1% |
| `system_config::tests::resolves_builtin_sepolia_snapshot_chain` | 1.1 | 1.0 | 1.2 | 3 | 0.1% |
| `l2::snapshot_stack::tests::subsecond_cl_and_el_activate_denim_at_same_timestamp` | 1.0 | 1.0 | 1.1 | 3 | 0.0% |
| `system_config::tests::snapshot_is_l1_free` | 1.0 | 1.0 | 1.1 | 3 | 0.0% |
| `system_config::tests::snapshot_datadirs_must_be_distinct` | 1.0 | 1.0 | 1.1 | 3 | 0.0% |

Sum of medians: 2205s across 131 tests. Nextest runs several at once, so this
is larger than the test phase above.
