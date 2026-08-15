//! Raw-free default-off priority-economics fixture capture tests.

#![cfg(feature = "priority-economics-capture")]

use std::{cell::Cell, fs};

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use base_mev_trader::{
    AccountV1, AuditAccessKindV1, AuditObservedValueV1, AuditPhaseV1, AuditReadV1, ErrorV1,
    InputV1, OutcomeV1, PublicationGateV1, PublicationOutcomeV1, SelectedPartsV1,
    StateFixtureCaptureConfigV1, StorageV1, WriterV1,
};
use revm::primitives::KECCAK_EMPTY;

fn selected() -> SelectedPartsV1 {
    SelectedPartsV1::new(
        B256::repeat_byte(1),
        B256::repeat_byte(2),
        [Address::repeat_byte(3), Address::repeat_byte(4)],
        [Address::repeat_byte(5), Address::repeat_byte(6), Address::repeat_byte(7)],
        [Address::repeat_byte(8), Address::repeat_byte(9)],
        Address::repeat_byte(10),
        Address::repeat_byte(11),
        Address::repeat_byte(12),
        [B256::repeat_byte(13), B256::repeat_byte(14)],
        [500, 3_000],
        [true, false],
        U256::from(1_000u64),
        B256::repeat_byte(15),
        B256::repeat_byte(16),
        B256::repeat_byte(17),
        B256::repeat_byte(18),
    )
    .unwrap()
}

fn storage_rows(count: usize) -> Vec<StorageV1> {
    (0..count)
        .map(|slot| {
            let slot = u64::try_from(slot).unwrap();
            StorageV1::new(U256::from(slot), U256::from(slot + 1), 1, slot, slot, 1).unwrap()
        })
        .collect()
}

fn account(address: u8, code: Vec<u8>, storage: Vec<StorageV1>) -> AccountV1 {
    let code = Bytes::from(code);
    AccountV1::new(
        Address::repeat_byte(address),
        true,
        U256::from(5),
        6,
        keccak256(code.as_ref()),
        code,
        storage,
        1,
    )
    .unwrap()
}

fn storage_audit_read(
    phase: AuditPhaseV1,
    address: Address,
    slot: U256,
    value: U256,
    ordinal: u64,
) -> AuditReadV1 {
    AuditReadV1::new(
        phase,
        AuditAccessKindV1::Storage,
        Some(address),
        None,
        Some(slot),
        None,
        None,
        AuditObservedValueV1::Storage(value),
        ordinal,
        ordinal,
        1,
    )
    .unwrap()
}

fn audit_reads() -> Vec<AuditReadV1> {
    vec![
        storage_audit_read(
            AuditPhaseV1::PreWeth,
            Address::repeat_byte(12),
            U256::from(3),
            U256::from(100),
            2,
        ),
        AuditReadV1::new(
            AuditPhaseV1::Candidate,
            AuditAccessKindV1::Basic,
            Some(Address::repeat_byte(30)),
            None,
            None,
            None,
            None,
            AuditObservedValueV1::AbsentAccount,
            3,
            3,
            1,
        )
        .unwrap(),
        storage_audit_read(
            AuditPhaseV1::PostWeth,
            Address::repeat_byte(12),
            U256::from(3),
            U256::from(140),
            9,
        ),
        AuditReadV1::new(
            AuditPhaseV1::L1Fetch,
            AuditAccessKindV1::BlockHash,
            None,
            None,
            None,
            None,
            Some(36_000_000),
            AuditObservedValueV1::BlockHash(B256::repeat_byte(31)),
            10,
            10,
            1,
        )
        .unwrap(),
    ]
}

fn input_with_accounts_and_audit(
    accounts: Vec<AccountV1>,
    audit_reads: Vec<AuditReadV1>,
) -> Result<InputV1, ErrorV1> {
    InputV1::seal(
        8_453,
        36_000_000,
        B256::repeat_byte(20),
        B256::repeat_byte(21),
        1_800_000_000,
        30_000_000,
        Address::repeat_byte(22),
        5_000_000,
        B256::repeat_byte(23),
        Some(1),
        Address::repeat_byte(12),
        Address::repeat_byte(12),
        U256::from(3),
        U256::from(100),
        U256::from(140),
        B256::repeat_byte(27),
        U256::from(28),
        B256::repeat_byte(24),
        selected(),
        accounts,
        audit_reads,
        B256::repeat_byte(25),
        B256::repeat_byte(26),
    )
}

fn input_with_accounts(accounts: Vec<AccountV1>) -> Result<InputV1, ErrorV1> {
    input_with_accounts_and_audit(accounts, audit_reads())
}

fn input() -> InputV1 {
    input_with_accounts(vec![account(1, Vec::new(), storage_rows(1))]).unwrap()
}

#[test]
fn capture_dto_is_owned_raw_free_and_preserves_execution_evidence() {
    let captured = input();
    assert_eq!(captured.economics_evidence_digest(), B256::repeat_byte(26));
    let encoded = serde_json::to_value(&captured).unwrap();
    assert_eq!(encoded["recipientWethPre"], "0x64");
    assert_eq!(encoded["recipientWethAddress"], format!("{:#x}", Address::repeat_byte(12)));
    assert_eq!(encoded["recipientWethSlot"], "0x3");
    assert_eq!(encoded["recipientWethRecipient"], format!("{:#x}", Address::repeat_byte(12)));
    assert_eq!(encoded["recipientWethPost"], "0x8c");
    assert_eq!(encoded["canonicalL1Digest"], format!("{:#x}", B256::repeat_byte(27)));
    assert_eq!(encoded["auditReads"][0]["phase"], "preWeth");
    assert_eq!(encoded["auditReads"][0]["accessKind"], "storage");
    assert_eq!(encoded["auditReads"][0]["address"], format!("{:#x}", Address::repeat_byte(12)));
    assert_eq!(encoded["auditReads"][0]["slot"], "0x3");
    assert_eq!(encoded["auditReads"][0]["observedValue"]["storage"], "0x64");
    assert_eq!(encoded["auditReads"][0]["firstOrdinal"], 2);
    assert_eq!(encoded["auditReads"][0]["lastOrdinal"], 2);
    assert_eq!(encoded["auditReads"][0]["occurrences"], 1);
    assert_eq!(encoded["auditReads"][1]["phase"], "candidate");
    assert_eq!(encoded["auditReads"][2]["phase"], "postWeth");
    assert_eq!(encoded["auditReads"][2]["address"], format!("{:#x}", Address::repeat_byte(12)));
    assert_eq!(encoded["auditReads"][2]["slot"], "0x3");
    assert_eq!(encoded["auditReads"][2]["observedValue"]["storage"], "0x8c");
    assert_eq!(encoded["auditReads"][2]["firstOrdinal"], 9);
    assert_eq!(encoded["auditReads"][2]["lastOrdinal"], 9);
    assert_eq!(encoded["auditReads"][2]["occurrences"], 1);
    assert_eq!(encoded["auditReads"][3]["phase"], "l1Fetch");
    let bytes = serde_json::to_vec(&captured).unwrap();
    for forbidden in [b"rawTx".as_slice(), b"envelope", b"signature"] {
        assert!(!bytes.windows(forbidden.len()).any(|window| window == forbidden));
    }

    assert!(
        input_with_accounts(vec![
            account(1, Vec::new(), storage_rows(8_191)),
            account(2, Vec::new(), storage_rows(1)),
        ])
        .is_ok()
    );
    assert_eq!(
        input_with_accounts(vec![
            account(1, Vec::new(), storage_rows(8_191)),
            account(2, Vec::new(), storage_rows(2)),
        ]),
        Err(ErrorV1::InvalidInput)
    );

    assert!(
        input_with_accounts(vec![
            account(1, vec![1; 2 * 1024 * 1024], Vec::new()),
            account(2, vec![2; 2 * 1024 * 1024], Vec::new()),
        ])
        .is_ok()
    );
    assert_eq!(
        input_with_accounts(vec![
            account(1, vec![1; 2 * 1024 * 1024], Vec::new()),
            account(2, vec![2; 2 * 1024 * 1024 + 1], Vec::new()),
        ]),
        Err(ErrorV1::InvalidInput)
    );

    let encoded_boundary_code_bytes =
        (10 * 1024 * 1024 - 8 * 1024 - 512 - 8_192 * 320 - 4 * 384) / 2;
    assert!(
        input_with_accounts(vec![account(
            1,
            vec![1; encoded_boundary_code_bytes],
            storage_rows(8_192),
        )])
        .is_ok()
    );
    assert_eq!(
        input_with_accounts(vec![account(
            1,
            vec![1; encoded_boundary_code_bytes + 1],
            storage_rows(8_192),
        )]),
        Err(ErrorV1::InvalidInput)
    );
}

#[test]
fn capture_rejects_noncanonical_storage_order() {
    let high = StorageV1::new(U256::from(2), U256::ZERO, 1, 0, 0, 1).unwrap();
    let low = StorageV1::new(U256::from(1), U256::ZERO, 1, 1, 1, 1).unwrap();
    assert!(matches!(
        AccountV1::new(
            Address::repeat_byte(1),
            true,
            U256::ZERO,
            0,
            KECCAK_EMPTY,
            Bytes::new(),
            vec![high, low],
            1,
        ),
        Err(ErrorV1::NonCanonicalOrdering)
    ));
    let mut noncanonical_audit = audit_reads();
    noncanonical_audit.reverse();
    assert_eq!(
        input_with_accounts_and_audit(
            vec![account(1, Vec::new(), storage_rows(1))],
            noncanonical_audit,
        ),
        Err(ErrorV1::NonCanonicalOrdering)
    );
    let assert_invalid = |reads: Vec<AuditReadV1>| {
        assert_eq!(
            input_with_accounts_and_audit(vec![account(1, Vec::new(), storage_rows(1))], reads,),
            Err(ErrorV1::InvalidInput)
        );
    };
    for missing_index in 0..4 {
        let mut missing_phase = audit_reads();
        missing_phase.remove(missing_index);
        assert_invalid(missing_phase);
    }

    let mut wrong_pre_address = audit_reads();
    wrong_pre_address[0] = storage_audit_read(
        AuditPhaseV1::PreWeth,
        Address::repeat_byte(13),
        U256::from(3),
        U256::from(100),
        2,
    );
    assert_invalid(wrong_pre_address);
    let mut wrong_post_slot = audit_reads();
    wrong_post_slot[2] = storage_audit_read(
        AuditPhaseV1::PostWeth,
        Address::repeat_byte(12),
        U256::from(4),
        U256::from(140),
        9,
    );
    assert_invalid(wrong_post_slot);
    let mut wrong_pre_value = audit_reads();
    wrong_pre_value[0] = storage_audit_read(
        AuditPhaseV1::PreWeth,
        Address::repeat_byte(12),
        U256::from(3),
        U256::from(99),
        2,
    );
    assert_invalid(wrong_pre_value);
    let mut wrong_post_value = audit_reads();
    wrong_post_value[2] = storage_audit_read(
        AuditPhaseV1::PostWeth,
        Address::repeat_byte(12),
        U256::from(3),
        U256::from(141),
        9,
    );
    assert_invalid(wrong_post_value);

    for (phase, value, ordinal) in
        [(AuditPhaseV1::PreWeth, U256::from(100), 1), (AuditPhaseV1::PostWeth, U256::from(140), 10)]
    {
        let mut duplicate = audit_reads();
        duplicate.push(storage_audit_read(
            phase,
            Address::repeat_byte(12),
            U256::from(3),
            value,
            ordinal,
        ));
        duplicate.sort();
        assert_invalid(duplicate);
    }

    let mut sealed_read = audit_reads();
    sealed_read.push(
        AuditReadV1::new(
            AuditPhaseV1::Sealed,
            AuditAccessKindV1::Basic,
            Some(Address::repeat_byte(30)),
            None,
            None,
            None,
            None,
            AuditObservedValueV1::AbsentAccount,
            11,
            11,
            1,
        )
        .unwrap(),
    );
    assert_invalid(sealed_read);

    let mut repeated_pre = audit_reads();
    repeated_pre[0] = AuditReadV1::new(
        AuditPhaseV1::PreWeth,
        AuditAccessKindV1::Storage,
        Some(Address::repeat_byte(12)),
        None,
        Some(U256::from(3)),
        None,
        None,
        AuditObservedValueV1::Storage(U256::from(100)),
        2,
        3,
        2,
    )
    .unwrap();
    assert_invalid(repeated_pre);
    assert_eq!(
        AuditReadV1::new(
            AuditPhaseV1::PreWeth,
            AuditAccessKindV1::Storage,
            Some(Address::repeat_byte(12)),
            None,
            None,
            None,
            None,
            AuditObservedValueV1::Storage(U256::from(100)),
            2,
            2,
            1,
        ),
        Err(ErrorV1::InvalidAuditRead)
    );
    assert_eq!(
        StorageV1::new(U256::from(1), U256::ZERO, 0b1_0000, 0, 0, 1),
        Err(ErrorV1::InvalidProvenance)
    );
    let nonempty = Bytes::from(vec![0x60, 0]);
    assert_eq!(
        AccountV1::new(
            Address::repeat_byte(1),
            true,
            U256::ZERO,
            0,
            KECCAK_EMPTY,
            nonempty,
            Vec::new(),
            1,
        ),
        Err(ErrorV1::InvalidAccount)
    );
    assert_eq!(
        AccountV1::new(
            Address::repeat_byte(1),
            true,
            U256::ZERO,
            0,
            B256::repeat_byte(1),
            Bytes::new(),
            Vec::new(),
            1,
        ),
        Err(ErrorV1::InvalidAccount)
    );
    assert_eq!(
        AccountV1::new(
            Address::repeat_byte(1),
            false,
            U256::from(1),
            0,
            KECCAK_EMPTY,
            Bytes::new(),
            Vec::new(),
            1,
        ),
        Err(ErrorV1::InvalidAccount)
    );
    assert_eq!(
        AccountV1::new(
            Address::repeat_byte(1),
            false,
            U256::ZERO,
            1,
            KECCAK_EMPTY,
            Bytes::new(),
            Vec::new(),
            1,
        ),
        Err(ErrorV1::InvalidAccount)
    );
    assert_eq!(
        AccountV1::new(
            Address::repeat_byte(1),
            false,
            U256::ZERO,
            0,
            KECCAK_EMPTY,
            Bytes::new(),
            storage_rows(1),
            1,
        ),
        Err(ErrorV1::InvalidAccount)
    );
    let absent_code = Bytes::from(vec![1]);
    assert_eq!(
        AccountV1::new(
            Address::repeat_byte(1),
            false,
            U256::ZERO,
            0,
            keccak256(absent_code.as_ref()),
            absent_code,
            Vec::new(),
            1,
        ),
        Err(ErrorV1::InvalidAccount)
    );
    assert_eq!(
        AccountV1::new(
            Address::repeat_byte(1),
            false,
            U256::ZERO,
            0,
            KECCAK_EMPTY,
            Bytes::new(),
            Vec::new(),
            1,
        ),
        Ok(AccountV1::new(
            Address::repeat_byte(1),
            false,
            U256::ZERO,
            0,
            KECCAK_EMPTY,
            Bytes::new(),
            Vec::new(),
            1,
        )
        .unwrap())
    );
}

#[test]
fn disabled_capture_has_zero_writer_and_file_counters() {
    let root = tempfile::tempdir().unwrap();
    let config = StateFixtureCaptureConfigV1::new(
        false,
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .unwrap();
    let receipt = WriterV1::new(config).write(input());
    assert_eq!(receipt.outcome(), OutcomeV1::Disabled);
    assert_eq!(receipt.counters().writer_attempted(), 0);
    assert_eq!(receipt.counters().files_created(), 0);
}

#[test]
fn capture_writer_is_create_new_and_economics_byte_stable_on_failure() {
    let root = tempfile::tempdir().unwrap();
    let config = StateFixtureCaptureConfigV1::new(
        true,
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .unwrap();
    let expected = input().economics_evidence_digest();
    let first = WriterV1::new(config).write(input());
    assert_eq!(first.outcome(), OutcomeV1::Written);
    assert_eq!(first.economics_evidence_digest(), expected);
    let capture_paths =
        fs::read_dir(root.path()).unwrap().map(|entry| entry.unwrap().path()).collect::<Vec<_>>();
    assert_eq!(capture_paths.len(), 1);
    let capture_path = capture_paths[0].clone();
    assert!(!capture_path.file_name().unwrap().to_string_lossy().starts_with('.'));
    let committed_bytes = fs::read(&capture_path).unwrap();
    fs::write(root.path().join("unrelated"), b"retained").unwrap();

    let config = StateFixtureCaptureConfigV1::new(
        true,
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .unwrap();
    let second = WriterV1::new(config).write(input());
    assert_eq!(second.outcome(), OutcomeV1::Failed(ErrorV1::AlreadyExists));
    assert_eq!(second.economics_evidence_digest(), expected);
    assert_eq!(second.counters().writer_attempted(), 1);
    assert_eq!(second.counters().files_created(), 0);
    assert_eq!(fs::read(&capture_path).unwrap(), committed_bytes);
    assert_eq!(fs::read(root.path().join("unrelated")).unwrap(), b"retained");

    let failure_base = tempfile::tempdir().unwrap();
    let failure_root = failure_base.path().join("capture");
    let moved_root = failure_base.path().join("capture-moved");
    fs::create_dir(&failure_root).unwrap();
    fs::write(failure_root.join("unrelated"), b"retained").unwrap();
    let config =
        StateFixtureCaptureConfigV1::new(true, failure_root.clone(), failure_root.clone()).unwrap();
    fs::rename(&failure_root, &moved_root).unwrap();
    fs::write(&failure_root, b"not a directory").unwrap();
    let failed = WriterV1::new(config).write(input());
    assert_eq!(failed.outcome(), OutcomeV1::Failed(ErrorV1::Io));
    assert_eq!(failed.economics_evidence_digest(), expected);
    assert_eq!(failed.counters().writer_attempted(), 1);
    assert_eq!(failed.counters().files_created(), 0);
    assert_eq!(fs::read(moved_root.join("unrelated")).unwrap(), b"retained");
    assert_eq!(fs::read(&failure_root).unwrap(), b"not a directory");

    let writer_source = include_str!("../../mev-trader/src/state_fixture_capture.rs");
    assert!(writer_source.contains("fs::hard_link(&temporary_path, &path)"));
    assert!(!writer_source.contains("let _ = fs::remove_file"));
}

#[test]
fn finalize_success_precedes_optional_writer_and_finalize_failure_skips_it() {
    let root = tempfile::tempdir().unwrap();
    let builder_calls = Cell::new(0);
    let config = StateFixtureCaptureConfigV1::new(
        true,
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .unwrap();
    let rejected = PublicationGateV1::publish(
        Result::<(), _>::Err("finalize rejected"),
        |&()| -> Result<InputV1, &str> {
            builder_calls.set(builder_calls.get() + 1);
            Ok(input())
        },
        WriterV1::new(config),
    );
    assert_eq!(rejected.builder_attempted(), 0);
    assert_eq!(rejected.writer_attempted(), 0);
    assert_eq!(rejected.files_created(), 0);
    assert!(matches!(&rejected, PublicationOutcomeV1::FinalizeRejected("finalize rejected")));
    assert_eq!(rejected.into_finalized(), Err("finalize rejected"));
    assert_eq!(builder_calls.get(), 0);
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);

    let config = StateFixtureCaptureConfigV1::new(
        true,
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .unwrap();
    let published = PublicationGateV1::publish(
        Ok::<_, &str>(()),
        |&()| -> Result<InputV1, &str> {
            builder_calls.set(builder_calls.get() + 1);
            Ok(input())
        },
        WriterV1::new(config),
    );
    assert_eq!(published.builder_attempted(), 1);
    assert_eq!(published.writer_attempted(), 1);
    assert_eq!(published.files_created(), 1);
    assert!(matches!(
        &published,
        PublicationOutcomeV1::WriterOutcome { receipt, .. }
            if receipt.outcome() == OutcomeV1::Written
    ));
    assert_eq!(published.into_finalized(), Ok(()));
    assert_eq!(builder_calls.get(), 1);
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);

    let production = include_str!("../src/mev_trader.rs");
    assert!(production.contains("fn priority_economics_capture_from_environment("));
    assert!(production.contains("StateFixtureCaptureConfigV1::new("));
    assert!(production.contains("capture.map(WriterV1::new)"));
    assert!(production.contains("let mut priority_economics_capture ="));
    assert!(production.contains("config.take_priority_economics_capture_writer()?"));
    assert!(production.contains("priority_economics_capture.take()"));

    for observer_symbol in ["t4b_shadow::observer(", "t4d_shadow::observer("] {
        for (offset, _) in production.match_indices(observer_symbol) {
            let construction = &production[offset..];
            let construction = &construction[..construction.find(")?;").unwrap()];
            assert!(construction.contains("priority_economics_capture"));
            assert!(!construction.contains("None"));
        }
    }
}
