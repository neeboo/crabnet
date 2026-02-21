# AGENTS.md - crabnet

This document defines working constraints and execution rules for Claude Code and Codex in this repository.

---

## Global Requirements

### 1. Use Agent-Team Working Mode
Whenever interacting as Claude Code in this repository, tasks must be split into subtasks and mapped to separate agents.

- Always decompose tasks into multiple subtasks.
- Each agent should focus on one responsibility.
- Process independent modules in parallel.
- Consolidate results before finalizing.

Example:
- Agent A: implement module X
- Agent B: implement module Y
- Agent C: add/update tests
- Then aggregate and merge outputs.

### 2. Codex Command

```bash
codex -m gpt-5.3-codex \
  --config model_reasoning_effort="high" \
  --full-auto
```

Supported options:

| Option                                      | Meaning                         |
| ------------------------------------------- | ------------------------------- |
| `-m gpt-5.3-codex-spark`                   | Use the Codex-specialized model |
| `--config model_reasoning_effort="high"`    | Enable high reasoning depth      |
| `--full-auto`                               | Fully automatic execution mode   |

This enables:
- high quality code generation
- strict type checking
- strongest practical best-practice adherence
- code review support
- deeper reasoning

### 3. Branch to PR Workflow (Blocking)
All changes must follow this sequence:

1. Create a branch from the latest `main`.
2. Implement and validate changes on that branch.
3. Run relevant tests before opening a PR.
4. Open a PR targeting `main`.
5. Request review from `@codex`.
6. Post a PR comment with `@codex review this`.
7. Address all review feedback and rerun review until it passes.
8. Merge only after `@codex` approval and required checks pass.

Rules:
- Direct commits to `main` are not allowed.
- Merging without `@codex` review pass is not allowed.

---

## Execution Checklist

Each task should include:

| Item             | Requirement                            |
| ---------------- | -------------------------------------- |
| Implementation   | Make the required code changes          |
| Code Review      | Run `codex -m gpt-5.3-codex --config model_reasoning_effort="high" --full-auto` and follow review notes |
| Unit Testing     | Cover critical logic paths              |
| E2E Testing      | Run integration tests when relevant      |
| Documentation    | Update related docs and behavior notes  |

### Commit format

```
<type>(<scope>): <description>

Types: feat | fix | docs | test | refactor | chore
Scope: bus | router | daemon | adaptor | cli
```

Examples:

- `feat(adaptor): add claude adaptor implementation`
- `fix(router): correct topic routing for system messages`
- `docs(bus): add protocol specification`

---

## Core Principles

### 1. Local-First
- Review local roadmaps before running major changes.
- Prefer local execution over external dependencies.
- Keep data local-first.
- Ensure offline usability where practical.

### 2. Modular Design
- Keep each component independently testable.
- Maintain clear interface boundaries.
- Limit coupling and dependencies.

### 3. Test-Driven Delivery
- Prefer test-first implementation.
- Target high test confidence for changed logic.
- Keep CI and local checks passing.

### 4. Documentation Sync
- Update documentation with each behavior change.
- Prefer Rust doc comments for public API updates.
- Record architecture changes in `ARCHITECTURE.md`.
- Update `ROADMAP.md` after each completed run.

---

## Key Documents

| Document            | Purpose                    |
| ------------------- | -------------------------- |
| `docs/ROADMAP.md`   | Mandatory roadmap reference |
| `docs/ARCHITECTURE.md` | Architecture reference      |
