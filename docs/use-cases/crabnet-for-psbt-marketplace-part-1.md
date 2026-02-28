# Crabnet for PSBT Marketplace - Part 1

> Document version: v0.1
> Status: Draft
> Scope: Lock-first listing and MPC-assisted settlement for Bitcoin/Ordinals PSBT swaps

## Context and Goal

This use case defines the first delivery slice for `crabnet` as a PSBT marketplace.
The primary goal is to remove the unsafe "pre-signed custody-like listing" pattern and replace it with a lock-first flow:

- seller locks inventory into a listing-specific commit UTXO
- timelock blocks early seller reclaim
- buyer confirms and signs a settlement PSBT
- MPC verifies funding conditions and co-signs unlock
- one listing can settle only once

In Part 1, we optimize for safety and protocol clarity, not matching sophistication.

## Why This Model

Many existing PSBT markets prioritize seller control and expose buyer-side asymmetry.
Part 1 moves to a symmetric, rule-driven execution model:

- no long-lived executable pre-signed sell order stored by the platform
- no platform custody of user private keys
- deterministic settlement path per listing
- explicit expiry and reclaim behavior

## Actors

- `Seller`
  - creates listing intent
  - funds listing commit UTXO
  - can reclaim only after timelock expiry
- `Buyer`
  - confirms quote and signs buyer-side PSBT
  - provides payment/funding proof as required by policy
- `MPC Policy Signer`
  - validates policy conditions
  - co-signs unlock path only when checks pass
- `Crabnet Network`
  - provides price discovery and listing propagation via `DHT/Gossip`
  - supports private negotiation channel between buyer/seller agents

## Primary Flow (Part 1)

1. Seller creates listing metadata and requests a per-listing commit address.
2. Seller transfers inventory UTXO to the commit address.
3. Listing enters `LOCKED` state after confirmation threshold.
4. Buyer accepts quote and signs settlement PSBT fields.
5. MPC validates:
   - listing is active and not expired
   - commit UTXO is unspent
   - payment/funding checks pass
   - PSBT outputs match listing terms, fee bounds, and routing rules
6. MPC co-signs unlock path and broadcast is triggered.
7. Listing transitions to `SETTLED` and is permanently consumed.

Fallback path:

- if not settled before expiry, listing moves to `EXPIRED`
- seller may execute timelock reclaim path
- reclaim transition is terminal for the current listing instance

## State Model (Part 1)

- `DRAFT` -> `LOCK_PENDING` -> `LOCKED` -> `MATCHED` -> `SETTLED`
- `LOCKED` -> `EXPIRED` -> `RECLAIMED`
- terminal states: `SETTLED`, `RECLAIMED`, `CANCELLED_BEFORE_LOCK`

Rules:

- exactly one commit UTXO per listing instance
- exactly one successful settlement spend per listing instance
- settlement and reclaim are mutually exclusive spend paths

## Core Invariants

- `single-spend`: a listing cannot settle twice
- `timelock safety`: seller reclaim is impossible before expiry
- `term binding`: signatures are bound to listing id, amount, destinations, fee limits, and expiry
- `auditability`: every MPC sign/reject decision is logged with reason codes

## Crabnet Integration (Part 1)

- discovery plane: listing and quote discovery over `DHT/Gossip`
- negotiation plane: private buyer/seller agent channel for bounded fields only
- execution plane: MPC-assisted PSBT validation and unlock

Negotiation is intentionally constrained in Part 1:

- allowed fields: price delta, fee side, expiry window
- disallowed: asset id change, destination rewrite, commit UTXO substitution

## Scope

In scope:

- listing commit address generation contract
- lock confirmation and listing activation
- buyer signature intake
- MPC verification and co-sign gate
- broadcast trigger and terminal state transitions
- expiry and reclaim mechanics

Out of scope:

- advanced auction engine design
- cross-chain settlement
- reputation and dispute arbitration
- full privacy hardening beyond baseline PSBT handling

## Risks and Open Questions

Risks:

- timelock misconfiguration can cause early reclaim or funds stuck
- MPC policy bugs can sign invalid settlements or block valid ones
- fee volatility can break prebuilt settlement assumptions
- operational MPC outage can increase expiry/reclaim events

Open questions:

- minimum confirmation policy per asset class
- private relay strategy for settlement broadcast
- fee bumping policy near expiry boundary
- recovery playbook for partial-signature failures

## Acceptance Checklist

- [ ] lock-to-settlement happy path passes end-to-end
- [ ] expiry-to-reclaim path proves no pre-expiry reclaim
- [ ] duplicate settlement attempt for one listing is rejected
- [ ] policy mismatch leads to deterministic MPC reject reason
- [ ] logs can reconstruct the full listing lifecycle
