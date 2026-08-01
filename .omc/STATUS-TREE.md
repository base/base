# base-mev 작업 상태 트리 (single source of truth)

> 문제/작업 목록 canonical 보드. 진행마다 Claude가 갱신.
> **마지막 갱신: 2026-08-01(15차)** — ★★★오너 결정 2건을 전 트리에 반영했다. ① issue-76 gross 낙관 검증 T5는 cost/net의 선행 차단이 아니라 **병렬 report-only 진단**이며, 실제 상태 캡처와 독립 리뷰 전 모든 positive disposition은 `grossOptimismUnverified: true`를 유지한다. ② PR-1 소스에는 candidate-specific execution gas·same-block Base fee·OP L1 data fee authority와 새 net-profit disposition/원장이 구현됐다. 단 **이 PR은 소스 recipe만** `jemalloc,base-execution-cli/t4b-shadow`로 바꾸며 maxperf를 빌드·배포하지 않았다. 현재 배포 산출물의 recipe는 MEV cargo feature를 0개 선택하고, 라이브 MEV는 env-enabled `mev-emitter`뿐이다. 반영에는 별도 오너 승인 rebuild/redeploy/restart가 필요하다.
> **입력 우선 상태:** 활성화 점검표 B·D GREEN / A·C·E·F RED. 전 항목 GREEN + 별도 오너 GO 전 활성화 금지.
> **병행 관측:** 72h base-arb 샘플러는 24풀·양성 대조군과 함께 진행 중이다. `.omc/state/edge-scan-72h/`와 프로세스는 읽기 전용.
> **구트랙:** TypeScript `packages/graph-arb`/`scripts/scan.ts`는 실재하지만 현재 Rust 트랙과 별개이며 이번 T1 수정은 취소됐다.
> **안전선:** shadow-mode는 서명 가능한 측정 바이트와 제출을 동일시하지 않는다. `arm-live-egress` OFF에서 호스트 밖 전송은 도달 불가여야 한다.
>
> ⚠️**갱신 규칙(오너 지적 2026-07-25)**: 헤더만 고치고 본문 섹션을 흘려보내면 안 된다. 매 갱신마다 **최소 이 4곳을 함께 점검**한다 — ①§전체 트리의 파이프라인이 현재 결정과 일치하는가 ②§1 표의 해당 행 **꼬리**가 오래된 "현재=…"를 들고 있지 않은가 ③새 발견이 §2 게이트 순서나 §4 verdict 해석 전제를 바꾸는가 ④§관련 문서에 새 스펙·verdict가 올라갔는가. **결정이 파이프라인을 바꾸면 헤더가 아니라 §전체 트리부터 고친다.**

## ⛔ 재오픈 금지 — 오너가 이미 닫은 게이트
> `.omc/specs/t4e-arm-activation-claude-design-v4.md:165`가 **"G3/G5/G6 OPEN시 … G4=DONE"** 이라고 써서 G4만 추적하고 나머지 셋을 OPEN 가능성으로 나열한다. **이 문장 때문에 감사가 이미 두 번 오류를 냈다**(07-25 G3 법무, 같은 날 G7).
> **G3 법무 ✅CLOSED(07-20) · G5-static ✅CLOSED(07-20) · G6 P2 HARD GO ✅GIVEN(07-20) · G4 arm attestation ✅완료(07-22) · G7 계약 ✅BD답변 수령(07-21)**.
> **스펙의 게이트 체인 문구는 이름 나열일 뿐 상태 근거가 아니다. 오너 결정의 authority는 오너 결정 기록(memory/runbook)이다.**

---

## 라벨 범례

- ✅ 완료 · ⏳ 진행 중 · ⬜ 미착수 · ⏸ PAUSE · 🔒 오너 게이트 · ⛔ 금지/차단
- **T4a~T4e** = Rust in-node 측정/시뮬레이션 스택. **issue-76 T5** = 병렬 report-only gross 진단. **T6~T8** = post-verdict 라이브 제출 래더. 세 트랙은 같은 사다리가 아니다.
- **OA** = 서명·자금·온체인 실행처럼 에이전트가 대신할 수 없는 오너 act.

## 전체 트리

```text
[현재 핵심] Rust in-node pairwise 2-hop shadow-mode 시뮬레이션
  현재 배포 /data/base-src @ 82de658e339227c003fe4a57baa44a289c99fdd5
  현재 배포 recipe: jemalloc only → MEV cargo feature 0개
  라이브 MEV: env-enabled mev-emitter only
                    │
                    ├─ 기존 Rust source track(현재 배포에 미선택)
                    │    victim frame → pinned snapshot/state 검증
                    │                 → pairwise 2-hop 발견·선택 → unsigned measurement evidence
                    │                 → 제출/forwarding/outbound transport 없음
                    │
                    ├─ PR-1 source-only 신규 노드
                    │    route/victim 원장 → candidate-specific execution gas + same-block Base fee
                    │                      → OP L1 data fee → retained value/total cost/net-profit disposition
                    │    source maxperf recipe: jemalloc,base-execution-cli/t4b-shadow
                    │    ⚠ build/deploy 아님; 별도 오너 rebuild/redeploy/restart 전 live 변화 0
                    │
                    ├─ 입력 우선 점검: B·D GREEN / A·C·E·F RED / G 활성화 시 확인
                    │    A 배포 artifact에 MEV feature 0개 · C pool universe 설치 🔒
                    │    E Blink credential 배선 🔒 · F exporter env ⛔
                    │    전 항목 GREEN + 별도 오너 GO 전 활성화 금지
                    │
                    └─ 측정값으로 다음 범위 판단; shadow-mode 유지, 거래 제출 금지

[병렬 report-only] issue-76 gross 낙관 진단 T5
  #76 과거 관측은 프루닝돼 현재 재실행 불가
  → tip-near 자립 state fixture 캡처🔒 → 독립 리뷰🔒
  → 그 전 positive disposition grossOptimismUnverified=true
  ※ cost/net 및 PR-1의 선행·차단 노드가 아니며 새 캡처에는 31일 deadline이 없다.

[병행 관측] 72h base-arb 샘플러 ⏳
  24풀 / negCycleFromWETH=true·negCycleFromUSDC=true 양성 대조군 / read-only
  ※ scripts/scan.ts를 쓰지만 현재 Rust 구현 트랙은 아니며 결과도 제출 권한이 아니다.

[post-verdict 라이브] G7 계약✅·OA-3 서명⬜ ∥ T6 kill-reset✅
                       → T7 live-run🔒 → T8 첫 제출(PONR)🔒
```

**현재 판정:** PR-1은 net-profit authority와 build recipe의 **소스만** 바꾼다. maxperf build·rebuild/redeploy/restart·활성화는 각각 별도 오너 게이트이며 현재 live disposition은 변하지 않는다. Rust shadow 관측과 TS 72h 샘플러 어느 것도 거래 제출 승인이 아니다.

---

## 1. 현재 핵심 — Rust in-node shadow-mode

| 항목 | 상태 | 직접 확인 근거 |
|---|---|---|
| 구현 위치·역할 | ✅ | `/data/base-src/crates/execution/mev-trader/README.md:3-9` — read-only Phase A in-node measurement, transaction 대신 measurement data, transport/signer/submission/txpool 없음 |
| pairwise 2-hop | ✅ | 같은 README `:13-20,25-27`; `src/pairwise.rs:1,26-29`; `src/edge_measurement.rs:3203-3207`의 정확히 2개 execution hop |
| shadow gating | ✅ 기본-off | `Cargo.toml:53-55`의 `t4a-shadow`/`t4b-shadow`; `src/runtime.rs:107-113,151-181,208-210` |
| 제출 금지 | ⛔ | README `:18-20` — `BackrunPlan`은 unsigned measurement evidence이며 envelope conversion/signing/submission/forwarding/outbound transport 없음 |
| 배포 소스 | ✅ 확인 | `/data/base-src` HEAD `82de658e339227c003fe4a57baa44a289c99fdd5`; 점검표 `CHECKLIST-t4a-activation-preconditions-2026-07-31.md:28-40` |
| Phase A exporter | ✅ MERGED 기록 | GitHub REST `repos/simjaemun2/base/pulls/63` 직접 확인: `merged_at=2026-07-31T23:18:26Z`, `merge_commit_sha=15ebc01d64d25b37a5c83226e0ca47a3267ef6d8`, `head.sha=f68650c18d6fd11fc1429bb002f360bd853d47fa`; R3 APPROVE `critic-review-pr63-r3-claude.md:1-9,114-124` |

### ★★★수익 판정 능력 — 15차 source authority와 live 경계

**질문**: 이 시뮬레이터가 수익 경로를 찾고 **실제 수익을 보장**하는가.
**답**: 기존 Rust source path는 경로와 gross를 계산하지만 현재 배포 recipe는 그 MEV cargo feature를 선택하지 않는다. PR-1 소스는 비용 authority와 net-profit disposition을 구현하지만, 빌드·배포·실제 상태 검증 전에는 live 수익이나 live-positive를 주장하지 않는다.

| 상태 | 근거 |
|---|---|
| ✅ 기존 경로 발견·자료 | `pairwise.rs`의 2-hop discover/size optimization과 `BackrunPlan`의 route·amount·gross·victim 결속은 유지된다. |
| ✅ 기존 gross 기록 | runtime → `AdmissionTerminalV1.bestModeledGrossProfitWeiSigned` 경로가 gross를 기록한다. |
| ✅ **PR-1 소스 authority 구현** | `mev-trader-submit/src/economics.rs`의 `PriorityEconomicsAuthority`가 candidate-specific simulated execution gas, same-block Base fee, OP-stack L1 data fee를 결속하고 retained value·total cost·expected EV를 계산한다. `tx_authority.rs`는 authority 부재·block/base-fee 불일치를 fail-closed 거부한다. |
| ✅ **PR-1 소스 원장/disposition 구현** | selected route/victim과 signed gross, retained value, total cost, net-profit disposition을 terminal wire/ledger에 보존한다. 구조상 positive라도 `grossOptimismUnverified: true`를 유지한다. |
| ⚠️ **PR-1 이전 structural live-positive=0** | PR-1 이전 source의 `crates/execution/cli/src/mev_trader.rs:9921-9930` `priority_economics`는 무조건 `Err(TxAuthorityNodeError::Unavailable)`이었다. 따라서 t4b authority 경로의 positive 0은 기회 부재 증거가 아니라 authority 도달 불가가 만든 구조적 0이었다. 현재 배포는 애초에 MEV cargo feature 0개라 이 수치를 실측 live-positive로 승격할 수도 없다. |
| ⚠️ **gross 낙관은 미검증** | `ISSUE76_ENGINE_QUOTE=1,229,736`과 기록된 과거 관측 `1,216,314`의 1.10% 격차는 현재 authority가 아니다. 앵커 47,697,819는 관측 tip 49,384,110에서 1,686,291블록 뒤라 `eth_getCode`/`debug_traceCall` 모두 pruned였고, `latest` 양성 대조군만 정상이다. |
| 🔒 **live 변화 0** | 이 PR은 `etc/just/build.just`의 source maxperf recipe를 `jemalloc,base-execution-cli/t4b-shadow`로 바꿀 뿐 maxperf를 실행하지 않았다. 현재 배포 산출물은 MEV cargo feature 0개이고 live MEV는 env-enabled `mev-emitter`다. 별도 오너 승인 rebuild/redeploy/restart 전 새 authority는 live disposition을 바꾸지 않는다. |

★★**정정 유지**: `tests/pairwise_parity.rs`는 const를 같은 리터럴과 비교할 뿐 TS 파일 blob을 핀하지 않는다. #76 golden 폐포를 건드리지 않는 것은 규율이지 기계 강제가 아니다.

**병렬 T5 판정:** issue-76 진단은 report-only이며 cost/net 구현을 막지 않는다. `grossOptimismUnverified`를 false로 바꾸려면 별도 오너 게이트로 tip-near 실제 2-hop의 code+storage+header 자립 fixture를 캡처하고 독립 리뷰해야 한다. 새 fixture는 캡처 즉시 자립하므로 **31일 deadline이 없고**, prune urgency도 이 오너 게이트를 우회하지 않는다.

### 입력 우선 활성화 점검표

| # | 선행조건 | 현재 | 다음 경계 |
|---|---|---|---|
| A | t4b-shadow 포함 배포 산출물 | ⛔ RED | PR-1은 source recipe만 변경; 별도 오너 승인 rebuild/redeploy/restart 전 현재 배포의 MEV cargo feature는 0개 |
| B | T4e 상태 파일 5종 | ✅ GREEN | 불변 취급 |
| C | `t4a-pool-universe-v1.json` | ⛔ RED | 설치는 오너 act; exact JSON·0600·service uid·digest 검증 |
| D | 핫월렛 정확히 66B | ✅ GREEN | 키 내용 재노출·재편집 금지 |
| E | `MEV_TRADER_BLINK_CREDENTIAL_FILE` | ⛔ RED | 없으면 `DisabledNoConnect`, victim frame 0 |
| F | admission exporter env 3종 | ⛔ RED | 전부 설정 또는 전부 비움; 부분 설정은 부팅 실패 |
| G | T4b/T4d exact opt-in | 활성화 시 확인 | 조합 불일치 시 부팅 실패 |

상태 권위는 최신 오너 정정과 직접 검증 기록이다. 점검표 최초 스냅샷(`CHECKLIST-t4a-activation-preconditions-2026-07-31.md:12-24`)의 D=67B는 후속 독립 검증에서 66B·RED→GREEN으로 교정됐다(`critic-review-preconditions-2026-07-31-claude.md:9-23`). 15차는 pre-PR recipe 기록(`critic-review-rust-net-profit-plan-r3-claude.md:23-24`)과 `crates/execution/cli/Cargo.toml`의 `default=[]`를 근거로 현재 배포의 MEV cargo feature 0개를 확인하여 A를 RED로 교정했다. 이 PR의 `etc/just/build.just:36-38` 변경은 source-only라 현재 배포 사실을 바꾸지 않는다. C·E·F RED는 유지된다. `memory/input-first-decision-2026-07-31.md:51-70`의 새 순서에 따라 **입력 연결·측정 → 실측으로 후속 범위 결정**이며, 옛 A→B→C→D 고정 순서는 대체됐다.

---

## 2. 진행 중 72h base-arb 샘플러 — 병행 관측, 핵심 구현과 분리

- ⏳ **run-2 가동 중** — 시작 `2026-08-01T01:39:30Z` → 종료 예정 `2026-08-04T01:39:30Z`. 24풀은 `.omc/state/edge-scan-72h/pool-set-provenance.txt:1-24`에 고정.
- ★**run-2 부터 내구 spool 활성**(PR#313 `85e6fdaf`) — `SCAN_DAEMON_SPOOL=1`, `campaign_id=base-arb-sampler-20260801`(기존 `in-node-dryrun` 385,877행과 분리). 후보는 `arb_dryrun_observations` 에 `spoolArbDryrunObservation`(ON CONFLICT DO NOTHING)로 insert-once 되고 `observed == spooled` 회계가 불일치 시 종료코드 비-0. run-1(NDJSON 전용·128분·후보 0)은 `run-1-ndjson-only/`에 보존.
- ✅ 양성 대조군: `daemon.sh:2-6`이 `negCycleFromWETH=true`와 `negCycleFromUSDC=true`를 기록한다. 따라서 구조적으로 후보 불가능한 factory-star 0과 구별한다.
- 🔒 `.omc/state/edge-scan-72h/`와 실행 프로세스는 읽기 전용. 노드/RPC/DB 조작·재시작·종료 금지.
- ⚠ 이 샘플러는 TypeScript `scripts/scan.ts` 구트랙을 관측 도구로 재사용한다. Rust in-node shadow-mode의 구현·admission·제출 경계와 합치지 않는다.

---

## 3. 라이브 래더 · OA 원장 · 온체인 자산

### issue-76 T5(report-only)와 T6~T8

| 단계 | 상태 | 현재 사실 |
|---|---|---|
| issue-76 T5 | **병렬 report-only** | cost/net 비차단; `grossOptimismUnverified=true`, tip-near fixture 캡처·독립 리뷰는 별도 오너 게이트 |
| G7 | ✅ 계약 / ⬜ OA-3 서명 | Blink BD 답변 수령 완료; 남은 것은 closure attestation 서명 1건 |
| T6 | ✅ | kill-reset 완료, anchor `clear`(2026-07-29) |
| T7 | 🔒 | live-run GO/OA-5 필요 |
| T8 | 🔒 | 첫 제출·실금전 PONR; shadow 측정과 별도 승인 |
| Phase C | ⬜ | 라이브 48h×2 GO/NO-GO; 인클루전은 제출 뒤에만 측정 가능 |

### OA

| OA | 상태 | 비고 |
|---|---|---|
| OA-1 G4 arm attestation | ✅ | 07-22 완료 |
| OA-2 kill-reset | ✅ | 07-29 완료, `d615e733…65191c` |
| OA-3 G7 closure attestation | ⬜ | 계약 답변은 닫힘; 서명 1건 |
| OA-4 deployment / OA-5 live-run | 🔒 | digest·readiness·별도 오너 GO 필요 |
| OA-6 executor unpause | ⬜ | T7/T8 readiness 항목 |
| OA-7 principal WETH→executor | ⬜ 미배정 | 거래 EOA와 executor custody 불일치 해소 필요 |
| OA-8 rollback 운영금지 승인 | ⬜ | 라이브 전 결정 |

### 온체인 자산 — 마지막 read-only 확인값(재조회하지 않음)

| 자산 | 확인값 | 의미 |
|---|---|---|
| `BlinkAtomicExecutor` `0x1810cbFA…` | paused=true · WETH 0 | OA-6·OA-7 전 첫 제출 불가 |
| 거래 hot wallet `0x98e1e2A8…` | 0.000986 ETH + 0.00126 WETH | per-tx cap×2의 blast-radius 분할 |
| L1-fee/백업 `0x6d568C8f…` | 0.14170 ETH + 0.99874 WETH | 두 지갑 WETH 합 1.00000 WETH |
| 어댑터 | UniV2 `0x17314D6F…` · UniV3 `0xD73a2ACb…` · Aero `0x6a2242f5…` | 이미 배포된 비가역 자산 |

※ Rust shadow-mode는 이 자산의 제출 권한을 열지 않는다. `merged ≠ deployed`, simulation ≠ live submission이다.

---

## 4. 판정 전제 — 나란히 보는 세 개의 공허한 0

| 공허한 0 | 왜 0인가 | 왜 엣지 없음과 구별 불가인가 | 근거 |
|---|---|---|---|
| **T4a DeltaGuard admission=0** | credential 부재로 victim frame 자체가 0이고, 발신자 코호트/pool universe가 없으면 DeltaGuard까지 도달해도 admission이 막힌다 | 출력 0행이 기회 부재인지 파이프라인 미가동인지 나타내지 않는다 | `memory/t4a-deltaguard-admission-zero-2026-07-31.md:17-29,31-51,61-76` |
| **scan.ts factory universe=0** | 500풀이 WETH+서로 다른 500 토큰의 star를 만들어 공유 상대토큰·사이클이 0이다 | 파이프라인은 정상이어도 입력 위상이 nonzero 답을 구조적으로 배제한다 | `memory/scan-universe-topology-trap-2026-07-31.md:19-30,32-49` |
| **PR-1 이전 structural live-positive=0** | t4b source의 `priority_economics`가 무조건 `Unavailable`이었고 현재 배포 recipe도 MEV cargo feature 0개라 authority가 positive disposition까지 도달할 수 없었다 | 0이 무수익인지 미배선 authority인지 구별하지 못한다 | PR-1 이전 `crates/execution/cli/src/mev_trader.rs:9921-9930`; pre-PR recipe 기록 `critic-review-rust-net-profit-plan-r3-claude.md:23-24`; `crates/execution/cli/Cargo.toml` `default=[]` |

**공통 결론:** 셋 다 **“엣지/순수익 없음”과 구별되지 않는 0**이었다. 측정 전 입력 위상·feature 도달 가능성·authority와 양성 대조군을 먼저 확인한다. 현재 24풀 sampler의 양성 대조군은 두 번째 함정을 피하지만, history-circular DB 풀 편향(`scan-universe…:63-68`)은 결과 해석에 반드시 붙인다. PR-1의 synthetic positive는 source capability 증거일 뿐 실제 상태나 live-positive 증거가 아니다.

그 밖의 살아있는 해석 전제:
- 인클루전 레이스는 제출 없이 원리적으로 측정 불가하며 post-verdict 라이브에서만 잰다.
- 2-hop은 오너가 먼저 프로덕션까지 진행하기로 정한 범위이고 3-hop은 유보다.
- kickback 75% 적용 winner-fixture 생존은 34/40이지만 razor-thin이며, 서로 다른 모집단 수치를 혼합하지 않는다.
- S3 독립 검증은 파생 산술을 덮지만 원문 victim→backrun 구성 Layer M을 재검증하지 않는다(`DECISION-s3-authority-boundary-2026-07-28.md`).

---

## 5. 별도 구트랙 — TypeScript `graph-arb` / `scan.ts`

- `packages/graph-arb`와 `scripts/scan.ts`는 실재하는 WETH closed-loop 스캐너/그래프 엔진이다(`CLAUDE.md:60-76`).
- **현재 핵심 트랙이 아니다.** 현재 트랙은 §1의 Rust in-node pairwise 2-hop shadow-mode다.
- 디스패치 T1의 Set ceiling·`SCAN_POOL_LIMIT` 결함 진단은 기록으로 남기되, 최신 오너 정정으로 **이번 수정은 취소**됐다. 살아있는 현재 엔진 결함/작업으로 올리지 않는다.
- 72h sampler가 이 코드를 관측 도구로 쓰는 사실은 구현 트랙 소유권을 바꾸지 않는다.

---

## 6. 살아있는 후속/검증 backlog

현재 핵심·입력 점검표·라이브 래더/OA에 이미 표시된 항목은 그 절이 상태 권위다. 아래는 12차 원본에서 종료 근거 없이 남았지만 축약 과정에서 빠졌거나, 현재 절의 실행 전 검증으로 계속 살아 있는 항목이다. W2~W5/EG-a~d 종료 이력과 섞지 않는다.

| 항목 | 상태 | 근거·종료 조건 |
|---|---|---|
| issue-76 gross 낙관 실상태 캡처·리뷰 | **병렬 report-only · 별도 오너 게이트** | cost/net 구현과 PR-1을 차단하지 않는다. `grossOptimismUnverified`는 tip-near 실제 2-hop의 code+storage+header 자립 fixture 캡처와 독립 리뷰 전까지 모든 positive disposition에서 true다. 새 캡처는 자립 fixture로 동결되므로 31일 deadline이 없고 prune urgency는 이 게이트를 우회하지 않는다. |
| BP-5 clock ordinal | **OPEN · 재판정 필요** | 12차 §1은 옵션3 §7.2 범위 밖의 별도 레인이며 EG-c 선행 필수라고 기록했다(`critic-review-edge-n1-pr42-r5-claude.md`, 72h 4,321 anchor·60s cadence). 통합 경로 결정에 명시 종료 근거가 없으므로 삭제하지 않는다. 현 Rust in-node 입력 우선 순서에서 적용 지점·필요성을 재판정하고, 닫으려면 별도 권위 결정 또는 검증 영수증이 필요하다. |
| C2 배포 매니페스트 drift 2건 | **OPEN · 즉시 수정 가능, 현재 게이트 아님** | 12차 §10 C2: UniV3Adapter `0x558aba7a…→0xdd184f8b…`, BlinkAtomicExecutor `0x3b93ab9d…→0xcc7e119d…`; 온체인 immutable 베이킹 결과와 base-mev 아티팩트 2개 엔트리의 불일치다. Rust `tx_authority.rs:128` 핀은 이미 올바르며, 두 엔트리 repin·검증으로 종료한다. |
| Phase C 프로덕션 runner 및 Rust→원장 경로 | **OPEN · T7/Phase C 전 차단** | 12차 §8: TS `runP2Integration`/`spoolP2Submission` 스택은 머지됐지만 프로덕션 호출자 0, 실행 스크립트 없음, Rust→원장 경로 없음. 현재 Rust shadow 원장과 TS 구트랙을 합쳤다고 간주하지 말고, 프로덕션 호출·원장 lineage가 독립 검증될 때 종료한다. |
| drawdown floor 형식 정합성 | **OPEN · Phase C 전** | 12차 §6: 0.05 WETH floor와 0.00126 WETH 거래지갑의 39.7배 격차로 floor가 사실상 vestigial이다. 지갑 잔액 cap이 더 강한 보호라는 안전 판정은 유지하되, Phase C 전에 floor를 새 지갑 규모로 재산정하거나 “지갑 잔액 cap이 floor를 대체”한다고 prereg에 명시해야 한다. |
| Phase C per-submission admission floor | **OPEN · Phase C prereg 전** | 12차 §4-B: per-tx cap·누적 drawdown floor·2σ median GO 임계는 어느 것도 제출별 admission floor가 아니다. 제출 전 허용 손익 임계와 fail-closed 집행 위치를 등록·검증해야 한다. |
| Phase C live economics 확인 | **OPEN · Phase C에서 판정** | 12차 §4-B의 “유의미 흑자 N”은 라이브 priority fee가 부호를 지배해 아직 OPEN이다. Blink가 배포 executor의 `actualFinal−amountIn` kickback basis를 운영상 인정하는지도 함께 확인하며, 34/40 winner-fixture와 다른 모집단 수치를 섞지 않는다. |
| dedicated inclusion endpoint 및 상대 지연 검증 | **OPEN · 라이브 준비 후** | 12차 §8의 Blink 계약·일반 제출 endpoint/method/schema는 CLOSED이나 최저-latency dedicated sequencer inclusion endpoint는 미종결이다. §4-A의 승자 대비 상대 지연도 A2 하네스 머지 후 EG-c 미실행으로 미측정이며, 라이브 준비 단계에서 endpoint 권위와 latency 실측으로 닫는다. |
| nightly live-quote-validation | **OPEN · 복구 필요** | 12차 §11: 도입 뒤 33회 중 성공 0회(cancelled 32, queued 1), self-hosted runner 0, 24h queue expiry와 `ALERT_WEBHOOK_URL` 부재. #76의 V3 quote 낙관 오류 계열을 잡는 유일 자동 게이트이므로 실제 성공 run과 알림 수신 영수증이 필요하다. |
| **base node v1.2.0 통합** | **보류 · 오너 결정 2026-08-01** | ★**정정(2026-08-01)**: 이전 기재의 *"⛔퇴로가 없다"* 는 **리뷰어 오류**였다. 릴리스 요약의 *"Database migration mandatory"* 를 바이너리가 V1을 거부하는지 확인하지 않고 게이트로 옮긴 것이다. **실측: reth v2.3.0은 V1 DB에서 정상 기동한다** — `init.rs:221-231`이 `storage_settings().unwrap_or_else(StorageSettings::v1)` 후 불일치 시 `warn!`만 내고 기존 설정으로 진행하며, reth 전체에 `StorageSettingsMismatch`/`RequiresV2`/`MustMigrate` **0건**. ⇒ **v1.2.0 업그레이드는 코드 리베이스만**이고 스토리지 작업이 선행조건이 아니다. 델타 실측 = `base/base` `01e732cd`→`8e28af24` 385커밋·1,028파일이나 포크와 겹치는 것은 10파일, 실제 3-way 충돌 **7파일·17블록**, 의미론 작업은 `forkchoice.rs`·`standard_node.rs` 2개. `crates/execution/mev-trader`와 deadlock 패치(`processor.rs`, `drop(live_state)` **4개**)는 upstream 변경 0. ⚠️**우리 DB가 V1인 것은 사실**(권위 = MDBX `Metadata["storage_settings"].storage_v2`, 관측 `No storage settings found.`). 그러나 그건 업그레이드 게이트가 아니라 **복구 항목**이다 — *"V1 Snapshots will be decommissioned over the next few weeks"* 이므로 DB 손상 시 되살릴 스냅샷이 사라진다. **오너 결정 2026-08-01: 복구/스토리지 작업은 보류**하고 최우선을 Rust 시뮬레이터의 수익 판정 능력 규명에 둔다. 판정서 `critic-review-pr314-node-v120-delta-claude.md`, 보고서 `REPORT-gjc-node-v120-delta-2026-08-01.md`(관측·V2 판정은 유효, 결론만 과했음). |
| per-PR CI 재활성화 | **OPEN · overdue** | 2026-06-25 Actions 분 소진으로 의도적으로 비활성화했으나 재활성화 트리거(2026-07-01 reset)가 지났다(12차 §11, `ci.yml:13-17`). 복구 후 대표 PR run 성공으로 종료한다. |

---

## 7. 종료 · 대체

| 항목 | 상태 | 보존 근거 |
|---|---|---|
| 오프라인 counterfactual W2~W5·EG-a~d 캠페인 | **2026-07-29 종료/통합 경로로 대체** | `.omc/plans/DECISION-unified-path-2026-07-29.md:3-19,23-29` |
| 고정 순서 A→B→C→D | **2026-07-31 대체** | `memory/input-first-decision-2026-07-31.md:51-59` — A→입력 연결·측정→B 필요성 판단→C→D |
| Phase A admission exporter PR#63 | **MERGED** | GitHub REST `repos/simjaemun2/base/pulls/63` 직접 확인: `merged_at=2026-07-31T23:18:26Z`, `merge_commit_sha=15ebc01d64d25b37a5c83226e0ca47a3267ef6d8`, `head.sha=f68650c18d6fd11fc1429bb002f360bd853d47fa`; R3 APPROVE `critic-review-pr63-r3-claude.md:1-9,114-124` |
| S1~S4/N1 옵션3 구현 스택 | **완료·현 트랙 아님** | PR#43 `a0078718` → S4 `892688a` → S2 `73b1bd5` → S3 `677bece`; 상세 각 critic 문서 |
| W2~W5 successor·옛 EG-a~d | **종료** | `DECISION-unified-path…:13-19`; 옛 비용·digest·supersede 상세는 S4 successor plan/critic에 보존 |
| 구 T4a~T4d offline 단계와 T4e 구현/프로비저닝 | **완료, Rust 현 트랙의 기반으로 흡수** | PR#58 및 PR#61 critic, `/data/base-src` HEAD `82de658e3` |
| 옛 “T4e=broadcastability 0→1” 표기 | **폐기** | 2026-07-30 red-line 이동; 아래 현행 red-line이 권위 |
| 구 `S2-fix`, `N1-freeze`, `S4마감`, `N2` 독립 레인 | **흡수/대체** | 옵션3 스펙 §7.1/§7.6 및 관련 critic |
| T1 TypeScript 엔진 수정 | **취소** | 최신 오너 정정; 진단은 디스패치 T1·§5에 기록 보존 |

---

## red-line (2026-07-30 오너 결정으로 **이동**)
⚠️**옛 문장은 폐기됐다.** 이전 판은 *"서명 유효 envelope을 생성할 수 있는 코드경로가 있으면 위반"* 이었고, 근거로 "`arm`/`arm-live-egress`를 켜는 크레이트 0건"·"실 signer는 dormant"를 들었다. **셋 다 더는 사실이 아니다** — `bin/node`의 `arm-sim`이 `arm`을 켜고, PR#58이 `load_and_sign_detailed`에 프로덕션 호출자를 만들었다(base 0 → head 1).

**현행 선**:
> `arm-live-egress`가 꺼진 어떤 빌드에서도 **호스트 밖으로 바이트를 전송하는 호출이 도달 불가능**하다. **서명은 이 선 아래에서 허용, 전송은 금지.**

- 무엇이 바뀌었나: 비기본·런타임 게이트된 `arm-sim` 빌드에서 워커가 후보마다 실거래 키(`~/.config/mev-trading-hotwallet` = `0x98e1e2A8…`)를 읽어 **서명 유효한 `raw_tx`를 만든다**. 그 바이트는 `RuntimeBackend::simulated`로만 가고 어디에도 송신되지 않는다. 기본 산출물에서는 도달 불가.
- 무엇이 안 바뀌었나: **제출 코드·릴레이·egress 없음**. S1-b 4중 잠금 그대로. **"이제 라이브 거래"가 아니다.**
- 계측: `cargo tree`는 **호출을 못 본다**. 그래서 두 검사가 커밋돼 있다 — **Check A**(inverse feature tree·`arm-live-egress`로 반드시 FAIL하며 사유 문자열까지 고정) · **Check B**(crate 전체 재귀 walk·파일명 제외 0). 둘 다 `s2_capability_seal`.
- ★**규칙**: 실패하는 경우를 보여주지 못한 통과 검사는 증거가 아니다. capability 검사에는 **음성 대조군**을 붙인다.
- ★**merged ≠ deployed**: 당시 기록의 반영 경계는 pull+재빌드+재시작과 별도 오너 GO였다. 15차 PR-1도 동일하다. source maxperf recipe에 `base-execution-cli/t4b-shadow`를 추가했지만 maxperf를 빌드·배포하지 않았으며, 현재 배포 산출물은 여전히 MEV cargo feature 0개다.

※ 위 절은 오너 요구에 따라 2026-07-30 문구를 그대로 보존했다. 그중 `/data/base-src`의 당시 배포 SHA `2d57e275b`는 이후 배포로 대체됐으며, 현재 직접 확인 HEAD는 §1의 `82de658e339227c003fe4a57baa44a289c99fdd5`다.

---

## 이력

- **15차(2026-08-01)** — net-profit PR-1 source authority와 owner-gated live 경계를 전 트리에 반영했다. issue-76 T5는 선행 차단에서 병렬 report-only로 강등하고 `grossOptimismUnverified: true`를 유지했다. candidate gas/Base fee/OP L1 fee authority·route/경제 원장·net-profit disposition과 §5-A maxperf source recipe를 기록하되 build/deploy/live 변화는 주장하지 않는다. 계획/critic 수렴은 R1 MAJOR 6(`critic-review-rust-net-profit-plan-claude.md`) → R2 MAJOR 4(`critic-review-rust-net-profit-plan-r2-claude.md`) → R3 APPROVE+필수 1(`critic-review-rust-net-profit-plan-r3-claude.md`) → R4 BLOCK·mutant #4 오배치(`critic-review-rust-net-profit-plan-r4-claude.md`) → R5 APPROVE(`critic-review-rust-net-profit-plan-r5-claude.md`, run `019fbd57`)다.
- **14차(2026-08-01)** — Rust simulator가 경로·gross를 만들지만 route 원장, 비용 authority, net 판정이 없고 PR-1 이전 CLI `priority_economics`가 unconditional `Unavailable`인 사실을 확인했다. issue-76 1.10%는 기록된 과거 관측이며 현재 재실행 authority가 아님을 후속 critic에서 교정했다. 근거: `DISPATCH-gjc-rust-net-profit-2026-08-01.md`, `critic-review-rust-net-profit-plan-claude.md`.
- **13차(2026-07-31)** — 오너 정정 반영: Rust in-node pairwise 2-hop shadow-mode를 현재 핵심으로 재구성, TS 엔진은 구트랙, W2~W5/EG-a~d는 종료 절로 이동. 근거: 이 문서 §1·§5·§7, `/data/base-src/crates/execution/mev-trader/README.md`.
- **12차(2026-07-31)** — Phase B 보류·입력 우선 측정. credential 미배선은 victim frame 0의 앞단 원인; A·B·D GREEN/C·E·F RED로 교정. 근거: `memory/input-first-decision-2026-07-31.md`, `critic-review-preconditions-2026-07-31-claude.md:9-23`, `CHECKLIST-t4a-activation-preconditions-2026-07-31.md`.
- **11차(2026-07-31)** — Phase A exporter PR#63 R1 ITERATE에서 R3 APPROVE·MERGED로 종결; 34종 terminal accounting·`universe_absent`·writer 계약 보강. 근거: `.omc/plans/critic-review-pr63-{phase-a,r2,r3}-claude.md`.
- **10차(2026-07-31)** — 노드 `82de658e3` 배포와 T4e 프로비저닝 예식 완료; arm-sim 활성화는 선행조건 누락으로 롤백. 근거: `CHECKLIST-t4a-activation-preconditions-2026-07-31.md`, PR#61 critic 묶음.
- **9차(2026-07-30)** — T4e PR#58 MERGED, red-line을 “서명 금지”에서 “egress 도달 불가”로 이동. 근거: `memory/redline-moved-to-egress-2026-07-30.md`, `.omc/plans/critic-review-pr58-t4e-impl-claude.md`.
- **8차(2026-07-29)** — 포크 PR#47~#56와 base-mev #295~#297 머지, T6 kill-reset 완료; T4e 측정과 라이브 래더 분리. 근거: `memory/RESUME-unified-path-2026-07-29.md`.
- **7차(2026-07-29)** — kill anchor `engaged→clear`, broadcastability 0 유지; suppression writer 배선 부재를 S1 항목으로 재분류. 근거: PR#47 critic/kill-reset 기록.
- **6차(2026-07-29)** — 오프라인 counterfactual 캠페인 종료, 제출·시뮬레이션 통합 결정. 근거: `.omc/plans/DECISION-unified-path-2026-07-29.md`.
- **5차(2026-07-29)** — deployed-identity binding PR#289 MERGED; placeholder 10개와 producer-binary 결속 문제 확인. 근거: `.omc/plans/critic-review-deployed-identity-binding-pr289-claude.md`, `memory/identity-binding-review-chain-2026-07-29.md`.
- **4차(2026-07-28)** — S3 gas-price authority PR#288 MERGED. 근거: `.omc/plans/critic-review-s3-gasprice-authority-code-pr288-claude.md`.
- **3차(2026-07-28)** — S3 PR#287 R2 APPROVE·MERGED, 옵션3 4-PR 스택 완성. 근거: `.omc/plans/critic-review-s3-checkpoint-gate-code-pr287-claude.md`.
- **2차(2026-07-28)** — S3 R1에서 복붙 검사 범위와 measurement/derived 선언-대-사용 결함 확인. 근거: 같은 S3 code critic R1.
- **1차(2026-07-28)** — S3 권위 경계를 Layer M/Layer D로 재정의. 근거: `.omc/plans/DECISION-s3-authority-boundary-2026-07-28.md`.
- **이전(2026-07-25~27)** — N1 옵션3 디스코프·스펙 v9 APPROVE·N1/S4/S2/S3 구현 및 반복 critic 이력. 근거: `.omc/specs/edge-n1-option3-consumer-finalization-spec.md`와 각 `critic-review-{edge-n1,s4-policy,s2-descope,s3-checkpoint}*`.

## 근거 문서

- 현재 Rust 권위: `/data/base-src/crates/execution/mev-trader/{README.md,Cargo.toml,src/pairwise.rs,src/runtime.rs,src/edge_measurement.rs}` @ `82de658e339227c003fe4a57baa44a289c99fdd5`.
- 입력/0 판정: `memory/input-first-decision-2026-07-31.md`, `memory/t4a-deltaguard-admission-zero-2026-07-31.md`, `memory/scan-universe-topology-trap-2026-07-31.md`.
- 전환/점검: `.omc/plans/DECISION-unified-path-2026-07-29.md`, `.omc/plans/CHECKLIST-t4a-activation-preconditions-2026-07-31.md`, `.omc/plans/DISPATCH-gjc-tree-and-engine-defect-2026-07-31.md`.
- Rust net-profit 계획/결정: `.omc/plans/DISPATCH-gjc-rust-net-profit-2026-08-01.md`, `.omc/plans/DISPATCH-gjc-rust-net-profit-plan-revision-2026-08-01.md`, `.omc/plans/DISPATCH-gjc-rust-net-profit-r2-fixes-2026-08-01.md`.
- 정확한 R1–R5 critic provenance: `.omc/plans/critic-review-rust-net-profit-plan-claude.md`, `.omc/plans/critic-review-rust-net-profit-plan-r2-claude.md`, `.omc/plans/critic-review-rust-net-profit-plan-r3-claude.md`, `.omc/plans/critic-review-rust-net-profit-plan-r4-claude.md`, `.omc/plans/critic-review-rust-net-profit-plan-r5-claude.md`.
- ★**살아있는 규범**: `.omc/specs/t4a-sender-cohort-operating-model-2026-07-31.md`(base-mev **#307 MERGED**·3라운드 APPROVE·456행). §10.1이 캠페인 go/no-go **사전등록 임계치**(`A≥10,000`·`D≥1,000`·`D/A≥10%`·`M/D≥95%`·`C≥20`·`R/C≥80%`·`SCOPED_MODEL_STOP = P==0`·시간 균일성)를 고정하고, §9가 5분 bucket 게이트를, §3.2가 Phase B 증명의무 6종을 정한다. Phase A/B 작업은 이 문서가 규범이다. 판정 이력 = `critic-review-pr307-{cohort-design,r2,r3}-claude.md`.
- 💰**자금·키·서명 상태는 이 보드에서 인용하지 않는다 — 매번 실측**(과거 인용을 믿고 2회 오보한 전례). 권위 맵 = `memory/wallet-custody-map-2026-07-29.md`. §3의 온체인 표는 마지막 read-only 확인값이지 현재값 보증이 아니다.
- 현행 보드 해석은 최신 오너 정정이 우선한다. `CLAUDE.md:12,60-76,79-87`의 “offline analyzer/TS engine” 설명은 구트랙의 존재 근거이지 현재 Rust 트랙 소유권의 근거가 아니다.

---

## 삭제 감사 — 12차 원본 전수 대조

분류: **(a)** 종료·이력으로 이동, **(b)** 권위 근거 문서에 장문 보존, **(live)** 현재/후속 절에 유지, **(c)** 소실. 라운드별 수치와 변이 상세는 위 critic 묶음이 권위이며 현재 보드에는 판정·살아있는 꼬리만 둔다.

| 12차 원본 섹션/사실군 | 분류 | 현 위치 또는 보존 근거 |
|---|---|---|
| 헤더·전체 트리·라벨 | **live** | 현 헤더·§전체 트리·범례: Rust in-node shadow와 PR-1 source net-profit authority가 현재, issue-76 T5는 병렬 report-only, TS는 구트랙, 라이브 래더·owner-gated rebuild/redeploy는 별도 |
| §1 N1/S4/S2/S3 옵션3 및 라운드별 결함·수치 | **a+b** | §7 종료·대체 및 §이력; `critic-review-{edge-n1,s4-policy,s2-descope,s3-checkpoint}*`, 옵션3 v9 스펙 |
| §1 BP-5 | **live** | §6 backlog — 명시 종료 근거가 없어 OPEN/재판정 |
| §1 S4 successor W1~W5 | **a+b** | §7의 W2~W5/EG-a~d 종료; `DECISION-unified-path-2026-07-29.md`와 successor plan/critic |
| §2 EG-a~d·supersede·구 EG-c 배포 순서 | **a+b** | §7 종료·대체; 결정서와 옵션3/successor critic에 수치·순서 보존. 닫힌 게이트 재오픈 근거로 사용 금지 |
| §3 비기술 오너 결정 G3/G6/P0 | **live+b** | 상단 재오픈 금지 byte-exact 유지; P0 중 남은 drawdown 형식 꼬리는 §6 |
| §4-A 인클루전·latency | **live+b** | §3 Phase C, §4 해석 전제, §6 dedicated endpoint/상대 지연; 장문 수치는 owner-lock/옵션3 critic |
| §4-B kickback·흑자·admission floor | **live+b** | §4의 34/40 해석, §3 Phase C, §6 admission floor; PR#272/Phase C prereg에 상세 보존 |
| §4-C universe·hop·민감도 | **live+b** | §1 Rust 2-hop, §2 sampler, §4 해석 전제; `edge-economic-prereg`와 옵션3 critic에 라운드별 수치 보존 |
| §4-D S3 권위 경계 | **live+b** | §4 해석 전제; `DECISION-s3-authority-boundary-2026-07-28.md` |
| §4-E admission/net 0·입력 단절 | **live+b** | §1 입력 점검표와 §4의 세 공허한 0; 관련 memory·PR#63 critic 및 net-profit R1–R5 critic |
| §5 T4a-shadow-op·활성화 점검 | **live+b** | §1 A~G 및 입력 우선 순서; activation checklist |
| §6 인터록 | **live/a** | kill-reset은 §3/§7 종료, drawdown 형식 정합성은 §6 backlog |
| §7 OA 원장 | **live** | §3 OA 표 |
| §8 issue-76 T5 및 T6~T8·Phase C | **live+b** | T5는 §1/§6의 병렬 report-only 진단; T6~T8 라이브 래더와 Phase C runner/admission/endpoint는 §3/§6 |
| §9 온체인 자산·커스터디 | **live + b** | 잔액·executor 표는 §3에 live. ★단 **권위 주소 2개**(`0x581F5c5E…` 오너 attest·`0xe01de0dB…` custody)는 보드에서 의도적으로 내렸다 — 자금·키는 인용 금지·매번 실측이 규칙이고 권위 맵은 `memory/wallet-custody-map-2026-07-29.md`(리뷰어 지적으로 2026-08-01 분류 정정) |
| §10 identity·provisioning·C2 | **a+live+b** | 프로비저닝은 §7 종료, C2 drift는 §6 backlog; PR#58/#61 critic |
| §11 검증 인프라 | **live** | §6 nightly 0/33 및 per-PR CI overdue |
| red-line | **live** | 위 보존문과 15차 source-recipe/current-deployment 경계: maxperf는 미실행, MEV feature 0개인 현재 배포와 별도 오너 rebuild/redeploy |
| 관련 문서·부록 감사 | **b** | §근거 문서의 net-profit dispatch와 정확한 R1–R5 critic provenance를 포함한 각 decision/spec/critic; 재오픈 방지 규칙은 상단 byte-exact 블록에 유지 |

**감사 합계: (c) 소실 = 0.** 원본의 모든 섹션/사실군은 (a), (b), (live) 중 하나 이상으로 추적되며, 살아있는 항목은 현재 절 또는 §6 backlog에 있다.
