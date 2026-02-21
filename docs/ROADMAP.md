# Crabnet Backend Hardening Roadmap

> Current baseline: 2026-02-21
> Goal: keep protocol compatibility while making node behavior more predictable, recoverable, and resilient to misuse.

## Milestone 0 (Highest Priority Fixes)

- [ ] Add robust persistence consistency and recovery for state load/save
  - Task: Add durability checks after atomic rename and validate saved file checksum.
  - Task: Implement fallback recovery to a backup copy (e.g. `.bak`) when load fails, and emit explicit monitor alerts.
  - Task: Add path/file locking strategy for `state.json` and monitor writes to prevent concurrent process corruption.

- [ ] Strengthen message deduplication and reject handling
  - Task: Replace unbounded `seen_messages` with bounded windowing + persistent pruning for TTL cleanup.
  - Task: Make message TTL configurable and log effective value on startup.
  - Task: Distinguish reject reasons in `apply_remote`: duplicate, expired, and malformed payload.

- [ ] Reduce high-risk execution paths
  - Task: Add command allowlist / command length / working directory restrictions in `runner`.
  - Task: On timeout, explicitly terminate task process and record termination metadata.
  - Task: Bound stdout/stderr capture size and truncate outputs to prevent log growth abuse.

## Milestone 1 (Security and Identity)

- [x] Add hybrid post-quantum-capable message transport for broadcast payloads.
  - Task: add `Envelope.crypto` recipient entries and hybrid key agreement (`x25519` ephemeral + `Kyber768` KEM) during broadcast.
  - Task: derive session keys via HKDF and encrypt payloads with `ChaCha20-Poly1305`.
  - Task: support local fallback for peers without identity/keys and for legacy plaintext messages.
  - Task: onboard peers through `NodeHello` containing static `x25519` and `Kyber768` public keys.

- [x] Add sender identity and signature verification
  - Task: define and store dual signature algorithm metadata on `Envelope`.
  - Task: add Ed25519 + Dilithium2 signatures for outbound envelopes.
  - Task: verify sender identity for all remote messages and reject unauthenticated senders.

- [ ] Add API auth model for monitor endpoints
  - Task: add optional token or header-based gate for `/api/*` endpoints.
  - Task: keep `/health` public, protect all other endpoints by default.

- [ ] Add network path rate limiting and abuse prevention
  - Task: cap send frequency for UDP and DHT broadcasts.
  - Task: add source-level fast-fail threshold, and drop/ignore after sustained misuse.

## Milestone 2 (Data Consistency)

- [ ] Define deterministic seed state convergence
  - Task: add version vector or update timestamp for seed updates.
  - Task: resolve `seed` update conflicts (remote settle vs local claim/result) with monotonic order rules.

- [ ] Handle expiry and timeouts
  - Task: background job to move stale bidable seeds to `Expired`.
  - Task: mark long-running executions as failed on timeout and emit monitoring alerts.

- [ ] Transaction-like behavior for run/settle
  - Task: keep `seed run` and `add_result` as a single business write path.
  - Task: add checkpoint + replayable audit for settlement, claim, and result updates.

## Milestone 3 (Operations Observability)

- [ ] Metrics and alerting
  - Task: emit counters/gauges for message ingest, reject, drop, and replay.
  - Task: add lag and backlog health signals for monitor file and sync queue.

- [ ] Hardening API behavior
  - Task: improve event pagination and filtering for `/api/events` to avoid full-file scans.
  - Task: unify invalid query handling into consistent API errors.

- [ ] Operator tools
  - Task: add `--verify-config` to precheck network addresses, directory permissions, and writable output paths.
  - Task: add `--dump-topology` CLI for faster incident investigation.

## Milestone 4 (Protocol and Platform)

- [ ] DHT production parameters
  - Task: separate publish/subscribe topics and version topics.
  - Task: add bootstrap and listener validation fallback to cached peers on repeated failures.

- [ ] Snapshot and event evolution
  - Task: introduce minimal delta exports to reduce full state broadcast churn.
  - Task: add snapshot version markers for cross-version compatibility.

## Milestone 5 (Acceptance Criteria)

- [ ] Full flow `publish -> bid -> claim -> run -> settle` is stable and consistent across at least two nodes over UDP and DHT.
- [ ] Known failure cases (missing signature, duplicate envelope, TTL expiration, concurrent writes) are all visible via monitor events and explicit rejection reason.
- [ ] State directory write failures stop operation with explicit errors; no silent corruption or data loss.
- [ ] All high-impact hardening knobs are configurable (signature requirement, API token, timeout, rate limits).

## Open Items

- Sandbox/isolated execution (containerized or jailed)
- Centralized identity trust store (key distribution and revocation)
- Multi-tenant namespace isolation
- End-to-end integrity chain for cross-region propagation
