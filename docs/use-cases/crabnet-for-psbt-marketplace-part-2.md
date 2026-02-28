# Crabnet for PSBT Marketplace - Part 2

> Document version: v0.1
> Status: Draft
> Scope: Pricing and matching, federated relay coordination, and sponsor-paid MPC fee settlement

## Context and Goal

Part 1 defines lock-first custody flow and MPC-assisted settlement.
Part 2 defines how prices are discovered and matched without reverting to a single centralized matching authority.

The goal is to support:

- relay diversity for listing and quoting
- explicit fee responsibility (`buyer`, `sponsor`, `split`)
- neutral execution constraints while still enabling sponsor-driven growth incentives

## Why This Model

Centralized PSBT marketplaces usually centralize both pricing and execution authority.
Crabnet separates those concerns:

- `DHT/Gossip` for discovery and market visibility
- relay nodes for pricing and matching strategies
- MPC service for policy-bound settlement signing

This keeps matching competitive while keeping settlement deterministic.

## Actors

- `Seller`
  - chooses one or more relays to publish listing intents
- `Buyer`
  - requests quotes from one or more relays
  - selects executable quote route
- `Relay Matcher`
  - computes quotes and match proposals
  - may sponsor execution fees to boost fill rate
- `MPC Policy Signer`
  - validates settlement policy and fee payer rules
  - co-signs only compliant settlement PSBTs
- `Crabnet Network`
  - provides relay discovery and listing propagation over `DHT/Gossip`

## Pricing and Matching Model

Part 2 uses a federated RFQ/orderbook hybrid:

- sellers publish signed `ListingIntent` headers
- buyers request signed quotes (`RFQ`) from selected relays
- relays return signed `Quote` objects with validity windows
- relays may emit signed `MatchProposal` messages

All price-bearing payloads must include:

- `listing_id`
- `relay_id`
- `quote_id`
- `expiry`
- `nonce`
- `fee_payer`
- fee breakdown (`relay_fee`, `mpc_fee`, `network_fee_limit`)

## Federated Relay Model

Relay participation is permissionless at discovery layer and competitive at matching layer:

- buyers and sellers each define relay allowlists/denylists
- a route is valid only if it satisfies both counterparties' constraints
- if no overlap exists, state becomes `NO_ROUTE_MATCH`
- route changes require fresh signatures from counterparties

Recommended execution behavior:

- primary relay with failover relays
- deterministic route id for auditability
- no forced routing through untrusted relays

## Sponsor-Paid MPC Fee Model

`fee_payer` is explicit and signed before settlement:

- `buyer`: buyer covers execution fees
- `sponsor`: sponsor fee pool covers execution fees
- `split`: fee share is predefined by ratio or basis points

Sponsor mechanism:

- sponsor maintains dedicated BTC fee UTXO pool
- pool is restricted to fee usage only
- each settlement debits `mpc_fee` and/or network fee from fee pool under policy rules
- sponsor earns configured success commission only on `SETTLED`

Optional accounting:

- non-BTC sponsor accounting can exist off-chain
- on-chain commitment should include a compact receipt hash (`OP_RETURN`) for match/fee audit linking

## Primary Flow (Part 2)

1. Seller publishes signed listing intent via selected relays.
2. Buyer requests quotes from selected relays.
3. Relay returns signed quote and optional match proposal.
4. Buyer selects quote route and confirms signed terms.
5. Fee mode (`buyer`/`sponsor`/`split`) is fixed and signed.
6. Part 1 settlement flow executes with MPC validation.
7. Settlement finalizes and fees/commission are distributed per signed terms.

## State Model (Part 2)

- `LISTING_VISIBLE`
- `QUOTING`
- `MATCH_PROPOSED`
- `QUOTE_ACCEPTED`
- `SETTLEMENT_PENDING`
- `SETTLED`
- `EXPIRED`
- `NO_ROUTE_MATCH`

Terminal states:

- `SETTLED`
- `EXPIRED`
- `NO_ROUTE_MATCH` (for that quote cycle)

## Core Invariants

- all quote and route decisions are signed and replay-protected
- fee payer mode is immutable once quote is accepted
- relay matching authority does not imply MPC settlement authority
- sponsor fee pool cannot spend seller inventory UTXOs
- no settlement without explicit policy-conformant fee checks

## Crabnet Integration (Part 2)

- discovery plane: `DHT/Gossip` publishes relay capability and listing headers
- negotiation plane: private agent channels can refine bounded fields only
- execution plane: Part 1 lock-first + MPC co-sign flow

Required signed objects:

- `RelayAdvertise`
- `ListingIntent`
- `Quote`
- `MatchProposal`
- `SettlementReceiptCommitment`

## Scope

In scope:

- relay-level quote and matching contracts
- route selection and overlap rules
- fee payer modes and sponsor pool policy integration
- MPC checks for sponsored settlement

Out of scope:

- full auction mechanism optimization
- complex market maker inventory strategies
- cross-chain fee sponsorship settlement
- advanced anti-sybil reputation systems

## Risks and Open Questions

Risks:

- liquidity fragmentation across relays
- sponsor fee abuse if pool policy is weak
- hidden spread or fee opacity if quote schema is incomplete
- relay/MPC collusion risk without governance controls

Open questions:

- should relay and MPC be required to be independent legal entities
- minimum transparency requirements for quote components
- default failover route policy and timeout thresholds
- slashing model for sponsor or relay misbehavior

## Acceptance Checklist

- [ ] multi-relay quoting returns deterministic signed quote set
- [ ] route overlap enforcement rejects no-overlap counterparties
- [ ] fee payer mode is enforced by MPC at settlement time
- [ ] sponsor pool cannot be used outside whitelisted fee actions
- [ ] settlement receipt commitment can be verified against off-chain match logs
