# LIMIT Residency Implementation Plan

> **For agentic workers:** Use superpowers:executing-plans to implement this plan task by task. Reviewers are read-only.

**Goal:** Remove repeated WAL reconstruction from ordinary/covering and many-table LIMIT access while preserving the successful version's results.

**Architecture:** Keep queried compatibility indexes resident on disk for the Store lifetime and maintain them in commit order. Remove the obsolete file-byte/count eviction policy. Remove the temporary primary file after a covering build has used it.

**Tech Stack:** Existing Rust, std disk I/O, JSONL protocol, Docker Linux.

**Spec:** `docs/superpowers/specs/2026-11-15-limit-residency-design.md`.

## Task 1: Regression evidence and bounded change

- [ ] Finish immutable 13bf04d/c919d64 measurements, retain failures, audit trace and resource data.
- [ ] Replace the old eviction test with a more-than-eight-tables no-rebuild assertion and add oversized ordinary/covering alternation coverage in src/storage.rs.
- [ ] Run those tests against unchanged product code and retain the expected failures.
- [ ] Remove size/count eviction and unused per-entry byte accounting in src/storage.rs. Keep lifetime/snapshot and incremental commit rules.
- [ ] Address builder review: close idle DiskIndex writers without deleting files, bound transient writers to the current replay table, and locate indexes by table instead of scanning all retained entries. Test close/reopen/grow, many-table descriptor counts and active snapshots.
- [ ] Remove the primary build file when a covering build returns its vector index, with explicit close before removal for Windows.
- [ ] Run focused tests, then full Rust tests, fmt and clippy.

## Task 2: Class-wide validation

- [ ] Commit only owned source/tests/design paths after staged-diff review. Build the original Dockerfile from the committed source and record image/ELF hashes.
- [ ] Reuse immutable input fixtures for ordinary, covering, mixed projections, many tables, transaction boundaries, large K, restart and low memory. Compare exact normalized responses against c919d64.
- [ ] Rerun public20 and relevant independent truth, persistence/recovery/fault tests.
- [ ] Require repeated mixed LIMIT and write/transaction loops to complete under 600 seconds, with no OOM and no per-query index reconstruction after first use.
- [ ] Obtain read-only code review and resolve substantive findings with new evidence.

## Task 3: Reviewable delivery

- [ ] Update the concise report and truthful conversation materials, validate strict JSON, quotes, source identity and byte limit.
- [ ] Commit final materials, build/test the final SHA, compare source and binary identity, make durable remote branch state and verify full remote SHA.
- [ ] Deliver the successful-version/submitted-version/fixed-version comparison and exact submission fields. Mark the goal complete only when required local work is done, distinguishing unobservable platform hidden results.
