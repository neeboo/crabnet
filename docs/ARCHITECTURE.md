# Crabnet Backend Architecture

> Document version: 2026-02-21

## Design Goals

Crabnet currently runs as a single binary node that:

- executes local CLI actions for `publish -> bid -> claim -> run -> settle`
- syncs state across peers over `udp` or `dht`
- stores local state and monitor events on disk
- exposes read-only monitor APIs for operators

Hardening focus in this branch is concrete runtime safety for remote sync: authenticated messages, encrypted payload fan-out, deterministic reject/noop paths, and stronger transport fallback behavior.

## Runtime Components

- `main` (`src/main.rs`)
  - CLI entrypoint and lifecycle control
  - initializes `Store`, `MeshClient`, and `MonitorHandle`
  - `listen` mode starts message loop + web monitor server
  - sends signed `NodeHello` (3 attempts) on listener startup

- `store` (`src/store.rs`)
  - local persistence (`state.json`) and domain state transitions
  - remote apply pipeline with explicit noop reasons (`ApplyRemoteNoopReason`)
  - peer identity table + signature verification policy
  - per-recipient payload encryption/decryption helpers

- `network` (`src/network.rs`)
  - `udp` transport for direct broadcast/unicast
  - `dht` transport (libp2p gossipsub + mDNS) with UDP fallback
  - UDP payload fragmentation/reassembly for large envelopes

- `runner` (`src/runner.rs`)
  - shell task execution (`bash -lc` / `cmd /C`)
  - timeout-wrapped process execution + output hashing

- `monitor` (`src/monitor.rs`)
  - unbounded async event channel
  - append-only NDJSON sink (`events.ndjson`)

- `web` (`src/web.rs`)
  - API routes: `/health`, `/api/events`, `/api/topology`, `/api/overview`
  - serves static monitor UI when frontend dist is available

## Hardened Remote Apply Flow (implemented)

Remote processing in `Store::apply_remote_with_reason` follows this order:

1. TTL gate (`MSG_TTL_SECONDS`) before mutation.
2. Signature verification before any state mutation.
3. Optional envelope decryption for addressed recipient.
4. Dedup insertion (`seen_messages`) before payload mutation.
5. Typed mutation by `MessageKind` with duplicate/noop reasons.

This gives concrete rejection/noop outcomes for:

- expired message
- duplicate message ids
- duplicate payload/state transitions
- not-addressed encrypted envelopes
- malformed payload shape (hard error)
- invalid signatures / unknown peer identity (hard error)

`main` maps these outcomes to monitor events (`store_sync`, `store_sync_noop`, `store_sync_rejected`) with structured payload fields.

## Message Security Hardening (implemented)

- Identity onboarding via signed `NodeHello`.
- Envelope dual signatures for remote mutation:
  - Ed25519 (`ed25519_signature`)
  - Dilithium2 (`dilithium_signature`)
- Hybrid recipient encryption path:
  - X25519 ephemeral DH shared secret
  - Kyber768 encapsulated shared secret
  - HKDF-SHA256 expansion
  - ChaCha20-Poly1305 payload encryption

Practical behavior:

- Non-`NodeHello` remote messages require known peer identity (or valid attached sender identity).
- `NodeHello` must match declared identity fields and valid dual signatures.
- Peer table mutation is apply-time only (not verify-time), covered by store tests.

## Transport Resilience Hardening (implemented)

- `dht` publish retries inside DHT worker loop with monitor events for failures.
- UDP fallback send path for DHT publish failures.
- Large UDP payload handling:
  - fragment at fixed chunk size
  - resend fragments for multiple attempts
  - reassemble by envelope id
  - fragment TTL cleanup with drop events

This reduces message loss risk when payload size exceeds single datagram limits.

## Persistence and Observability Hardening (implemented)

- `Store::save` writes `state.json` via:
  - unique temp file
  - explicit `sync_all`
  - atomic rename to target path
- Web monitor APIs support filtering/limiting (`limit`, `kind`, `source`) and derived topology/overview responses from NDJSON event history.
- Integration coverage includes two-node UDP and two-node DHT CLI flows (`tests/cli_e2e.rs`).

## Current Hardening Gaps (still open)

- Monitor NDJSON writer has no file lock to prevent concurrent writers (`events.ndjson`).
- No retention policy on monitor NDJSON file; append-only without size/age limits.
- No centralized alerting or external notification for hardening events.
- No deterministic conflict resolution for concurrent seed updates across nodes.
- No lifecycle expiry enforcement (background sweeper for timed-out `Open` seeds).

## Implemented Hardening (completed)

- API auth gate on `/api/*` with public `/health` (token via `Authorization: Bearer` or `X-Api-Token`).
- Transport/source rate limiting on UDP and DHT paths (broadcast rate limiter + per-source fast-fail).
- `seen_messages` is bounded and TTL value is runtime-configurable (`CRABNET_MESSAGE_TTL_SECONDS`).
- Task runner enforces allowlist, workdir restriction, output-size caps, and force-kill on timeout.
- `state.json` writes use file lock + checksum + backup recovery for safe multi-process operation.
- Operator preflight and inspection commands: `--verify-config`, `--dump-topology`.
