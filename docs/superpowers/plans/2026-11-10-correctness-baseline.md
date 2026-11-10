# MiniVectorDB Correctness Baseline Implementation Plan

> **For agentic workers:** Use superpowers:executing-plans. The user explicitly requests implementation in this checkout this round; proceed inline, preserving existing tools and conversation.json.

**Goal:** Build a real persistent SQL database and validate it through the official JSONL driver.

**Architecture:** A hand-written SQL lexer/parser and typed executor feed a streaming transaction store. A checksummed append-only commit log is the authoritative data file; rebuildable on-disk hash indexes locate current rows. Statement/transaction staging stays on disk. Exact filtered scans and bounded external sorting provide a correctness baseline.

**Tech Stack:** Rust 1.99.1, std, serde/serde_json only; existing Docker toolchain; Python stdlib verification.

**Spec:** Current STDIO_PROTOCOL.md, mysqltest/README.md and the local statement mirror at 220842bb. User's detailed request is the execution scope.

## Global Constraints

- Self-implement storage, execution, transactions and vector search; no database/index engine dependencies.
- Preserve signed i64; store finite f32 before computing distances; filter before TopK; tie by primary key ascending.
- Failed statements leave no partial change; earlier transaction statements remain committable.
- Acknowledge commits only after sync; shutdown discards uncommitted staging.
- All database, recovery and scratch files live in the selected data directory (default /data).
- Never modify expected public/legacy results or conversation.json.

## Design choices

The log is both redo history and row storage. Fixed-size disk hash slots and streaming replay bound row and index memory. A secondary flat index points to the committed vector bytes. Recovery rebuilds both indexes and retains the commit history. The two requested modes use the same exact distance order. Sort runs hold keys and row positions so wide projections are read only for output.

## Review Focus

- Invalid query/assignment types must fail even against empty tables and LIMIT 0.
- Partial log tails must never hide later acknowledged commits; recovery truncates before accepting writes.
- Unicode limits, SQL escaping, BIGINT extrema and f32 overflow need explicit checks.
- Index creation and changes must use the same transaction view as regular scans.
- Unbounded sorting, large transactions and index growth must avoid whole-table memory copies.

## Tasks

### 1. Protocol, SQL and typed semantics
- [x] Create black-box tests in tools/verify_database.py and observe failure against the protocol-only skeleton.
- [x] Implement src/sql.rs, src/types.rs, src/protocol.rs and src/engine.rs for lexical boundaries, types, projections, predicates and transaction control.
- [x] Test actual JSONL, precise IDs/BIGINT, unsupported syntax, Unicode and vector validation.

### 2. Durable streaming store
- [x] Implement src/storage.rs and src/disk_index.rs. Commit frames contain byte length, checksum and complete trailer. Sync before acknowledgement and truncate incomplete tails after replay.
- [x] Stage each statement independently and merge into a transaction only after validation. Rebuild disk indexes using bounded buffers.
- [x] Test normal restart, explicit rollback, failed multi-row statements and forced kills including recovery interruption.

### 3. Exact search and bounded results
- [x] Implement filtered f32 distance scans and sorting in src/query.rs and src/distance.rs. Keep projection payloads out of sort runs and write results to a response spool.
- [x] Run official public and legacy packages through unchanged protocol_check/run.py.
- [x] Generate small datasets with the existing testkit and compare actual SQL results against its independent oracle.

### 4. Reproducible artifact and supplementary evidence
- [x] Build locked release binary using devtools/docker and add a source-building Dockerfile with a minimal build context.
- [x] Verify the default image with the official driver, no network and read-only root.
- [x] Run larger-than-memory and kill/recovery tests with fresh volumes. Retain input hashes, commands, stdout/stderr and exit status.
- [x] Run cargo fmt, cargo test and cargo clippy. Address review findings and save the baseline with source/image fingerprints and a factual report.

## Execution ledger

- Initial state: f4d4e266, branch 2026-competition-round2; Dockerfile/conversation.json empty; only pre-existing untracked tools/minivectordb-legacy-testkit. Remote branch and statement mirror unchanged on recheck. No canonical instruction.ctf.md found at this project or searched parent/global locations; normal mode retained.
- The protocol-only executable returned not_implemented for CREATE TABLE. After implementation the official driver passed public 20/20 and migration 3/3 against the container image.
- Regression failures reproduced double rounding to f32, f64 distance rank collapse, nonnumeric vector input, object request id conversion, quoted keywords and concurrent opens. The corresponding checks passed after fixes.
- The wide projection test reproduced a cgroup OOM. Sorting row locations and reading projections after ordering passed the same input. Under 96 MiB, the final image also handled a 192 MiB transaction and two interruptions before recovery ready.
- Evidence paths and machine-readable results are in the workspace's analysis/implementation-20261110 directory.
