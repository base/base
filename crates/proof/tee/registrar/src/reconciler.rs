//! Signer reconciliation and unavailable-instance grace across discovery cycles.

use std::collections::{HashMap, HashSet};

use alloy_primitives::Address;
use tracing::warn;

use crate::{
    InstanceObservation, PendingRegistration, ProverInstance, RegisterableSigner, RegistrarMetrics,
};

/// Signers protected from orphan cleanup after every instance has been resolved.
///
/// Only the reconciler can authorize an orphan pass.
/// ```compile_fail
/// use base_proof_tee_registrar::ProtectedSigners;
/// use std::collections::HashSet;
/// let protected = ProtectedSigners { signers: HashSet::new() };
/// ```
#[derive(Debug)]
pub struct ProtectedSigners {
    signers: HashSet<Address>,
}

impl ProtectedSigners {
    /// Whether a signer must be retained by orphan cleanup.
    pub fn contains(&self, signer: &Address) -> bool {
        self.signers.contains(signer)
    }
}

/// Per-cycle signer decisions consumed by registration and orphan cleanup.
#[derive(Debug, Default)]
pub struct SignerReconciliation {
    /// Signers eligible for registration this cycle.
    pub registerable: Vec<RegisterableSigner>,
    /// Instance IDs whose resolution was inconclusive this cycle.
    pub unresolved_instance_ids: HashSet<String>,
    /// Protected signers, available only when no instance is unresolved.
    pub orphan_cleanup: Option<ProtectedSigners>,
}

/// Owns the unavailable-instance grace cache across completed discovery cycles.
#[derive(Debug)]
pub struct SignerReconciler {
    last_known_active: HashMap<String, CachedInstance>,
    unhealthy_instance_ids_with_grace: HashSet<String>,
    instance_cache_ttl_cycles: u32,
}

#[derive(Debug)]
struct CachedInstance {
    signers: Vec<Address>,
    ttl_cycles: u32,
}

impl SignerReconciler {
    /// Creates a reconciler with the configured grace period in cycles.
    pub fn new(instance_cache_ttl_cycles: u32) -> Self {
        Self {
            last_known_active: HashMap::new(),
            unhealthy_instance_ids_with_grace: HashSet::new(),
            instance_cache_ttl_cycles,
        }
    }

    /// Applies observations and pending registrations without endpoint I/O.
    ///
    /// Each step ages missing and unreachable entries once; discovery errors and cancellation skip it.
    /// Pending signers remain protected until reaped, including cancelled tasks.
    pub fn step(
        &mut self,
        observations: Vec<(ProverInstance, InstanceObservation)>,
        pending: &HashMap<Address, PendingRegistration>,
    ) -> SignerReconciliation {
        let mut resolution = SignerReconciliation::default();
        let mut active_signers = HashSet::new();
        let discovered_instance_ids = observations
            .iter()
            .map(|(instance, _)| instance.instance_id.clone())
            .collect::<HashSet<_>>();
        let mut unhealthy_instance_ids = HashSet::new();
        for (instance, observation) in observations {
            let (signers, registerable) = match observation {
                InstanceObservation::Unreachable => {
                    unhealthy_instance_ids.insert(instance.instance_id);
                    continue;
                }
                InstanceObservation::Failed => {
                    // Failed key resolution neither refreshes nor ages cached signers.
                    resolution.unresolved_instance_ids.insert(instance.instance_id);
                    continue;
                }
                InstanceObservation::AttestationsUnavailable { signers } => {
                    resolution.unresolved_instance_ids.insert(instance.instance_id.clone());
                    (signers, Vec::new())
                }
                InstanceObservation::Resolved { signers, registerable } => (signers, registerable),
            };
            let signers = signers.into_iter().collect::<HashSet<_>>();
            active_signers.extend(signers.iter().copied());
            if signers.is_empty() {
                self.last_known_active.remove(&instance.instance_id);
            } else {
                self.last_known_active.insert(
                    instance.instance_id,
                    CachedInstance { signers: signers.into_iter().collect(), ttl_cycles: 0 },
                );
            }
            resolution.registerable.extend(registerable);
        }

        self.unhealthy_instance_ids_with_grace
            .retain(|instance_id| unhealthy_instance_ids.contains(instance_id));
        for instance_id in &unhealthy_instance_ids {
            // A process restart has no signer addresses to cache. Keep an
            // empty cache entry for one unhealthy period so the global orphan
            // cleanup pass is deferred by the configured grace TTL.
            if self.unhealthy_instance_ids_with_grace.insert(instance_id.clone())
                && !self.last_known_active.contains_key(instance_id)
            {
                self.last_known_active.insert(
                    instance_id.clone(),
                    CachedInstance { signers: Vec::new(), ttl_cycles: 0 },
                );
            }
        }
        RegistrarMetrics::discovered_instances_count().set(discovered_instance_ids.len() as f64);

        self.last_known_active.retain(|instance_id, entry| {
            if discovered_instance_ids.contains(instance_id)
                && !unhealthy_instance_ids.contains(instance_id)
            {
                return true;
            }

            entry.ttl_cycles = entry.ttl_cycles.saturating_add(1);
            if entry.ttl_cycles <= self.instance_cache_ttl_cycles {
                warn!(
                    instance = %instance_id,
                    cached_signers = entry.signers.len(),
                    ttl_cycles = entry.ttl_cycles,
                    max_ttl_cycles = self.instance_cache_ttl_cycles,
                    "instance unavailable, preserving last-known active signers"
                );
                active_signers.extend(entry.signers.iter().copied());
                if entry.signers.is_empty() {
                    // We cannot identify signers to protect for an unhealthy
                    // instance first observed after a restart.
                    resolution.unresolved_instance_ids.insert(instance_id.clone());
                } else if !unhealthy_instance_ids.contains(instance_id) {
                    // A missing instance could have changed signers since its
                    // cached addresses were last refreshed.
                    resolution.unresolved_instance_ids.insert(instance_id.clone());
                }
                true
            } else {
                warn!(
                    instance = %instance_id,
                    ttl_cycles = entry.ttl_cycles,
                    max_ttl_cycles = self.instance_cache_ttl_cycles,
                    "last-known active signer cache expired for unavailable instance"
                );
                false
            }
        });

        RegistrarMetrics::active_signers_count().set(active_signers.len() as f64);
        RegistrarMetrics::registerable_signers_count().set(resolution.registerable.len() as f64);
        RegistrarMetrics::unresolved_instances_count()
            .set(resolution.unresolved_instance_ids.len() as f64);

        if resolution.unresolved_instance_ids.is_empty() {
            active_signers.extend(pending.keys().copied());
            resolution.orphan_cleanup = Some(ProtectedSigners { signers: active_signers });
        }
        resolution
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{EP1, healthy_prover_instance};

    const SIGNER: Address = Address::repeat_byte(1);

    #[tokio::test]
    async fn protects_active_and_cancelled_pending_signers() {
        let mut tasks = tokio::task::JoinSet::new();
        let handle = tasks.spawn(std::future::pending::<()>());
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let pending_signer = Address::repeat_byte(2);
        let pending = HashMap::from([(
            pending_signer,
            PendingRegistration { instance_id: "absent".into(), task_id: handle.id(), cancel },
        )]);
        let result = SignerReconciler::new(2).step(
            vec![(
                healthy_prover_instance(EP1),
                InstanceObservation::Resolved { signers: vec![SIGNER], registerable: vec![] },
            )],
            &pending,
        );
        let protected = result.orphan_cleanup.unwrap();
        assert!(protected.contains(&SIGNER));
        assert!(protected.contains(&pending_signer));
        assert!(!protected.contains(&Address::ZERO));
    }

    #[test]
    fn unavailable_attestations_block_cleanup() {
        let mut reconciler = SignerReconciler::new(2);
        let instance = healthy_prover_instance(EP1);
        let result = reconciler.step(
            vec![(
                instance.clone(),
                InstanceObservation::AttestationsUnavailable { signers: vec![SIGNER] },
            )],
            &HashMap::new(),
        );
        assert!(result.orphan_cleanup.is_none());
        assert_eq!(result.unresolved_instance_ids, HashSet::from([instance.instance_id]));
        assert!(result.registerable.is_empty());
    }

    #[test]
    fn empty_discovery_allows_cleanup() {
        let result = SignerReconciler::new(2).step(vec![], &HashMap::new());
        let protected = result.orphan_cleanup.unwrap();
        assert!(!protected.contains(&SIGNER));
        assert!(result.registerable.is_empty());
        assert!(result.unresolved_instance_ids.is_empty());
    }

    #[test]
    fn missing_signers_block_cleanup_until_grace_expires() {
        let mut reconciler = SignerReconciler::new(2);
        let instance = healthy_prover_instance(EP1);
        reconciler.step(
            vec![(
                instance.clone(),
                InstanceObservation::Resolved { signers: vec![SIGNER], registerable: vec![] },
            )],
            &HashMap::new(),
        );
        for _ in 0..2 {
            let result = reconciler.step(vec![], &HashMap::new());
            assert!(result.orphan_cleanup.is_none());
            assert!(result.registerable.is_empty());
            assert_eq!(
                result.unresolved_instance_ids,
                HashSet::from([instance.instance_id.clone()])
            );
        }
        let expired = reconciler.step(vec![], &HashMap::new());
        assert!(expired.unresolved_instance_ids.is_empty());
        assert!(!expired.orphan_cleanup.unwrap().contains(&SIGNER));
    }

    #[test]
    fn initially_unreachable_defers_exactly_one_grace_period() {
        let mut reconciler = SignerReconciler::new(2);
        let instance = healthy_prover_instance(EP1);
        for cycle in 1..=5 {
            let result = reconciler
                .step(vec![(instance.clone(), InstanceObservation::Unreachable)], &HashMap::new());
            assert!(result.registerable.is_empty());
            if cycle <= 2 {
                assert!(result.orphan_cleanup.is_none());
                assert_eq!(
                    result.unresolved_instance_ids,
                    HashSet::from([instance.instance_id.clone()])
                );
            } else {
                assert!(result.unresolved_instance_ids.is_empty());
                assert!(!result.orphan_cleanup.unwrap().contains(&SIGNER));
            }
        }
    }

    #[test]
    fn discovered_failure_blocks_cleanup_without_aging_cache() {
        let mut reconciler = SignerReconciler::new(2);
        let instance = healthy_prover_instance(EP1);
        reconciler.step(
            vec![(
                instance.clone(),
                InstanceObservation::Resolved { signers: vec![SIGNER], registerable: vec![] },
            )],
            &HashMap::new(),
        );
        reconciler.step(vec![], &HashMap::new());
        for _ in 0..3 {
            let healthy = healthy_prover_instance(crate::test_utils::EP2);
            let result = reconciler.step(
                vec![
                    (instance.clone(), InstanceObservation::Failed),
                    (
                        healthy.clone(),
                        InstanceObservation::Resolved {
                            signers: vec![Address::repeat_byte(2)],
                            registerable: vec![RegisterableSigner {
                                instance: healthy,
                                signer: Address::repeat_byte(2),
                                attestation: vec![1],
                            }],
                        },
                    ),
                ],
                &HashMap::new(),
            );
            assert!(result.orphan_cleanup.is_none());
            assert_eq!(
                result.unresolved_instance_ids,
                HashSet::from([instance.instance_id.clone()])
            );
            assert_eq!(result.registerable.len(), 1);
        }
        let result = reconciler.step(
            vec![
                (instance.clone(), InstanceObservation::Unreachable),
                (
                    healthy_prover_instance(crate::test_utils::EP2),
                    InstanceObservation::Resolved { signers: vec![], registerable: vec![] },
                ),
            ],
            &HashMap::new(),
        );
        assert!(result.orphan_cleanup.unwrap().contains(&SIGNER));
        let expired =
            reconciler.step(vec![(instance, InstanceObservation::Unreachable)], &HashMap::new());
        assert!(!expired.orphan_cleanup.unwrap().contains(&SIGNER));
    }

    #[test]
    fn cached_unreachable_signers_remain_protected_without_blocking_cleanup() {
        let mut reconciler = SignerReconciler::new(2);
        let instance = healthy_prover_instance(EP1);
        reconciler.step(
            vec![(
                instance.clone(),
                InstanceObservation::Resolved { signers: vec![SIGNER], registerable: vec![] },
            )],
            &HashMap::new(),
        );
        for _ in 0..2 {
            let result = reconciler
                .step(vec![(instance.clone(), InstanceObservation::Unreachable)], &HashMap::new());
            assert!(result.unresolved_instance_ids.is_empty());
            assert!(result.registerable.is_empty());
            assert!(result.orphan_cleanup.unwrap().contains(&SIGNER));
        }
        for _ in 0..2 {
            let expired = reconciler
                .step(vec![(instance.clone(), InstanceObservation::Unreachable)], &HashMap::new());
            assert!(!expired.orphan_cleanup.unwrap().contains(&SIGNER));
        }
    }

    #[test]
    fn refresh_resets_missing_grace_period() {
        let mut reconciler = SignerReconciler::new(2);
        let instance = healthy_prover_instance(EP1);
        for _ in 0..2 {
            let result = reconciler.step(
                vec![(
                    instance.clone(),
                    InstanceObservation::Resolved { signers: vec![SIGNER], registerable: vec![] },
                )],
                &HashMap::new(),
            );
            assert!(result.orphan_cleanup.unwrap().contains(&SIGNER));
            assert!(reconciler.step(vec![], &HashMap::new()).orphan_cleanup.is_none());
        }
        assert!(reconciler.step(vec![], &HashMap::new()).orphan_cleanup.is_none());
        assert!(
            !reconciler.step(vec![], &HashMap::new()).orphan_cleanup.unwrap().contains(&SIGNER)
        );
    }

    #[test]
    fn draining_instances_protect_all_signers_without_registration() {
        let signers = vec![SIGNER, Address::repeat_byte(2)];
        let result = SignerReconciler::new(2).step(
            vec![(
                crate::test_utils::prover_instance(EP1, crate::InstanceHealthStatus::Draining),
                InstanceObservation::Resolved { signers: signers.clone(), registerable: vec![] },
            )],
            &HashMap::new(),
        );
        assert!(result.registerable.is_empty());
        assert!(result.unresolved_instance_ids.is_empty());
        let protected = result.orphan_cleanup.unwrap();
        for signer in signers {
            assert!(protected.contains(&signer));
        }
    }

    #[cfg(feature = "metrics")]
    #[test]
    fn failed_discovered_instances_do_not_contribute_cached_active_signers() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            let mut reconciler = SignerReconciler::new(2);
            let instance = healthy_prover_instance(EP1);
            reconciler.step(
                vec![(
                    instance.clone(),
                    InstanceObservation::Resolved { signers: vec![SIGNER], registerable: vec![] },
                )],
                &HashMap::new(),
            );
            reconciler.step(vec![(instance, InstanceObservation::Failed)], &HashMap::new());
        });
        let snapshot = snapshotter.snapshot().into_vec();
        for (name, expected) in [
            ("base_registrar.active_signers_count", 0.0),
            ("base_registrar.registerable_signers_count", 0.0),
            ("base_registrar.unresolved_instances_count", 1.0),
            ("base_registrar.discovered_instances_count", 1.0),
        ] {
            let value = &snapshot.iter().find(|(key, _, _, _)| key.key().name() == name).unwrap().3;
            assert_eq!(value, &metrics_util::debugging::DebugValue::Gauge(expected.into()));
        }
    }

    #[test]
    fn attestation_failures_refresh_and_protect_resolved_signers() {
        let mut reconciler = SignerReconciler::new(2);
        let instance = healthy_prover_instance(EP1);
        reconciler.step(
            vec![(
                instance.clone(),
                InstanceObservation::Resolved {
                    signers: vec![Address::repeat_byte(2)],
                    registerable: vec![],
                },
            )],
            &HashMap::new(),
        );
        reconciler.step(vec![], &HashMap::new());
        let result = reconciler.step(
            vec![(
                instance.clone(),
                InstanceObservation::AttestationsUnavailable { signers: vec![SIGNER, SIGNER] },
            )],
            &HashMap::new(),
        );
        assert!(result.orphan_cleanup.is_none());
        assert!(result.registerable.is_empty());
        for _ in 0..2 {
            let result = reconciler
                .step(vec![(instance.clone(), InstanceObservation::Unreachable)], &HashMap::new());
            let protected = result.orphan_cleanup.unwrap();
            assert!(protected.contains(&SIGNER));
            assert!(!protected.contains(&Address::repeat_byte(2)));
        }
        let result =
            reconciler.step(vec![(instance, InstanceObservation::Unreachable)], &HashMap::new());
        assert!(!result.orphan_cleanup.unwrap().contains(&SIGNER));
    }

    #[cfg(feature = "metrics")]
    #[test]
    fn unresolved_cycles_count_cached_and_attested_active_signers() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let instance = healthy_prover_instance(EP1);
        let mut reconciler = SignerReconciler::new(2);
        for (observations, expected) in [
            (
                vec![(
                    instance.clone(),
                    InstanceObservation::Resolved {
                        signers: vec![SIGNER, SIGNER],
                        registerable: vec![RegisterableSigner {
                            instance: instance.clone(),
                            signer: SIGNER,
                            attestation: vec![1],
                        }],
                    },
                )],
                [1.0, 1.0, 0.0, 1.0],
            ),
            (vec![], [1.0, 0.0, 1.0, 0.0]),
            (
                vec![(
                    instance.clone(),
                    InstanceObservation::AttestationsUnavailable { signers: vec![SIGNER, SIGNER] },
                )],
                [1.0, 0.0, 1.0, 1.0],
            ),
            (vec![(instance, InstanceObservation::Unreachable)], [1.0, 0.0, 0.0, 1.0]),
            (vec![], [1.0, 0.0, 1.0, 0.0]),
            (vec![], [0.0, 0.0, 0.0, 0.0]),
        ] {
            metrics::with_local_recorder(&recorder, || {
                reconciler.step(observations, &HashMap::new());
            });
            let snapshot = snapshotter.snapshot().into_vec();
            for (name, expected) in [
                "base_registrar.active_signers_count",
                "base_registrar.registerable_signers_count",
                "base_registrar.unresolved_instances_count",
                "base_registrar.discovered_instances_count",
            ]
            .into_iter()
            .zip(expected)
            {
                let value =
                    &snapshot.iter().find(|(key, _, _, _)| key.key().name() == name).unwrap().3;
                assert_eq!(
                    value,
                    &metrics_util::debugging::DebugValue::Gauge(expected.into()),
                    "{name}"
                );
            }
        }
    }
}
