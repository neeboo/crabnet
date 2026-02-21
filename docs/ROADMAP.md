# Crabnet Backend Hardening Roadmap

> Baseline: 2026-02-21
> Rule: keep unfinished work first; completed work moves to evidence section.

## Top Priority TODO (Unfinished First)

### P0 - State Safety and Execution Containment

- [ ] Add state corruption recovery on load
  - Task: persist checksum metadata for `state.json` writes.
  - Task: add `.bak` fallback recovery path when primary state fails to parse.
  - Task: emit explicit monitor event when fallback recovery is used.

- [ ] Add multi-process write coordination for local files
  - Task: add file locking around `state.json` writes.
  - Task: prevent concurrent monitor writer corruption for `events.ndjson`.
  - Task: fail fast with explicit error when lock cannot be acquired.

- [ ] Bound remote dedupe memory growth
  - Task: replace unbounded `seen_messages` with bounded window/pruning.
  - Task: persist pruning metadata compatible with existing state format.
  - Task: expose effective dedupe/TTL config in startup logs.

- [ ] Harden runner execution guardrails
  - Task: add command allowlist + max command length + working directory restriction.
  - Task: enforce hard process termination on timeout and persist termination reason.
  - Task: cap stdout/stderr size and mark truncation in `TaskResult`.

### P1 - Abuse Resistance and Auth

- [ ] Protect monitor APIs
  - Task: keep `/health` public.
  - Task: add auth gate for `/api/events`, `/api/topology`, `/api/overview`.

- [ ] Add network path abuse controls
  - Task: rate-limit inbound/outbound UDP and DHT message paths.
  - Task: add per-source drop thresholds and monitor counters.

### P2 - Deterministic State Convergence

- [ ] Define seed conflict convergence rules
  - Task: add explicit ordering/versioning for remote seed updates.
  - Task: resolve settle/result/claim cross-node conflicts deterministically.

- [ ] Add lifecycle timeout/expiry enforcement
  - Task: background sweeper for expired `Open` seeds.
  - Task: promote timed-out running seeds to terminal status with monitor alert.

- [ ] Add transaction-like run/settle write discipline
  - Task: define atomic transition boundary for `Running -> Done -> Settled`.
  - Task: add replayable audit checkpoints for claim/result/settle transitions.

### P3 - Operator Visibility and Tooling

- [ ] Add hardening metrics
  - Task: counters for reject/noop/drop/replay outcomes.
  - Task: monitor lag/backlog indicators for event pipeline.

- [ ] Improve event query scalability and API consistency
  - Task: avoid full-file scans for common `/api/events` queries.
  - Task: standardize invalid-query error responses.

- [ ] Add operator preflight/inspection commands
  - Task: `--verify-config` for path/addr/writeability checks.
  - Task: `--dump-topology` CLI output for incident triage.

### P4 - Protocol Evolution (Compatibility-Preserving)

- [ ] DHT hardening parameters
  - Task: split topic/version strategy for forward compatibility.
  - Task: add bootstrap fallback strategy using cached peers.

- [ ] Snapshot/event evolution
  - Task: add minimal delta exports to reduce full payload churn.
  - Task: version snapshot schema for cross-version replay safety.

### Acceptance Criteria (Open)

- [ ] Signature failures, duplicate messages, and TTL-expired messages are surfaced with explicit operator-facing diagnostics.
- [ ] Multi-process state write scenarios fail safely without silent corruption.
- [ ] All high-impact hardening knobs are configurable and observable at startup.

## Completed in This Branch (Evidence)

- [x] Hybrid post-quantum-capable payload protection is implemented.
  - Evidence: `src/store.rs` (`encrypt_for_peers`, `build_recipient_envelope`, `decrypt_payload`, `derive_session_key`).

- [x] Dual-signature envelope authenticity and sender verification are implemented.
  - Evidence: `src/store.rs` (`sign_envelope`, `verify_envelope`, `verify_with_identity`, `verify_ed25519_signature`, `verify_dilithium2_signature`).

- [x] Signed `NodeHello` identity onboarding is implemented.
  - Evidence: `src/main.rs` listener startup sends `Envelope::node_hello`; `src/store.rs` `MessageKind::NodeHello` apply path and identity checks.

- [x] Remote apply noop/reject categorization is implemented.
  - Evidence: `src/store.rs` `ApplyRemoteNoopReason`; `src/main.rs` monitor events `store_sync_noop` and `store_sync_rejected`.

- [x] UDP fragmentation/reassembly and DHT UDP fallback path are implemented.
  - Evidence: `src/network.rs` (`send_udp_payload`, `maybe_reassemble_fragment`, `fallback_send`).

- [x] Two-node full flow stability is covered for both UDP and DHT.
  - Evidence: `tests/cli_e2e.rs` (`cli_e2e_dual_node_sync_publish_bid_claim_run_settle`, `cli_e2e_dual_node_dht_sync_publish_bid_claim_run_settle`).

- [x] Atomic state write path with parseable concurrent-save regression coverage is implemented.
  - Evidence: `src/store.rs` (`save` temp-file + `sync_all` + rename), store tests `concurrent_saves_keep_state_json_parsable` and `save_overwrites_and_loads_without_corruption`.
