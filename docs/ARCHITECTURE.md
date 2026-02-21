# Crabnet Backend Architecture

> Document version: 2026-02-21

## Design Goals

The backend is currently bounded as a single binary node that:

- receives and applies local and remote events
- persists local state and task results
- broadcasts events over a pluggable transport (`udp` or `dht`)
- exposes a read-only monitoring HTTP API

The immediate goal is to harden runtime reliability and consistency first, then grow security and scalability.

## Component Boundaries

- `main` (`src/main.rs`)
  - CLI entrypoint and lifecycle control
  - initializes `Store`, `Monitor`, and `MeshClient`
  - routes command behavior (`listen`, `status`, `seed` commands)

- `store` (`src/store.rs`)
  - persistent domain state for seeds, bids, claims, results, and node identity
  - provides domain operations: publish, bid, claim, run, settle
  - applies remote messages with idempotent semantics

- `model` (`src/model.rs`)
  - domain objects: `Seed`, `Bid`, `Claim`, `TaskResult`
  - envelope types (`Envelope`, `MessageKind`) used for network propagation

- `network` (`src/network.rs`)
  - unified transport abstraction through `MeshClient`
  - `NetworkBackend::Udp`: UDP unicast/broadcast
  - `NetworkBackend::Dht`: libp2p gossipsub + mDNS with UDP fallback

- `runner` (`src/runner.rs`)
  - local task runner using shell invocation (`bash -lc` on Unix, `cmd /C` on Windows)
  - records result, duration, exit state, and output hash

- `monitor` (`src/monitor.rs`)
  - centralized event generation
  - async NDJSON event persistence to `events.ndjson`

- `web` (`src/web.rs`)
  - Axum API endpoints: `/health`, `/api/events`, `/api/topology`, `/api/overview`
  - static file serving and fallback frontend behavior

## Critical Flows

- Publish seed
  1. CLI creates `Seed`
  2. store writes local state
  3. monitor event is emitted
  4. optional broadcast with `Envelope::seed_created`

- Bid / claim
  1. local validations (status, bid window, minimum price, max bid limit)
  2. bid/claim is persisted and event is emitted
  3. broadcast is sent when requested

- Run / settle
  1. `seed claim` -> `seed run` -> generate `TaskResult`
  2. local result is broadcast
  3. `seed settle` updates status and optionally broadcasts settlement

- Sync / idempotency
  1. `listen` continuously receives envelopes
  2. `Store::apply_remote` validates TTL and deduplicates by `seen_messages`
  3. state deltas are applied and persisted

## Trust Boundary and Current Risks

- Envelope `signature` is currently always empty; no signature verification is implemented.
- UDP/DHT paths currently lack sender authentication, replay protection, and rate limiting.
- Task execution is direct shell execution; no sandboxing or privilege reduction.
- `Web` and stored state are not protected by authorization for now.
- Single-file persistence (`state.json`) is vulnerable to concurrent writer races, resolved mostly by last-writer-wins behavior.

## Reliability and Consistency (current)

- `Store::load` / `Store::save` use directory creation and temp-file rename.
- `state.json` can be raced by multiple instances sharing one directory.
- Monitor is append-only NDJSON without hard durability and retention policy guarantees.

## Backend Hardening Principles (for roadmap)

1. Preserve message semantics first: keep compatibility with existing `MessageKind` and payload shape.
2. Improve observability and consistency before adding protocol features.
3. Execution priority order:
   - trust and authentication controls
   - persistence durability and conflict recovery
   - protocol resilience (retry, reject, fallback)
   - state machine lifecycle correctness
   - operations visibility (alerts and monitoring)

## Key Extension Targets (without breaking behavior)

- Identity and signing: `Envelope.signature` is currently empty and requires verification support.
- Sync conflict policy: ordering rules for `seed`, `claim`, and `result` updates are not explicitly versioned.
- Attack surface: broadcasting paths need source validation and rate controls.
- Execution safety: no CPU/memory/disk isolation around `runner`.
- Observability loop: events stay local in NDJSON with no centralized alerting.
