# Migration ablation test version

**Goal:** Deliver the user's requested diagnostic version based on 781a14d, disabling selected seekdb-inspired methods and validating independently of the legacy AMD test methodology.

**Design / decision:** Use a default-on Cargo feature `migration-ablation` to select existing conservative execution paths. Disable in-memory staging, compact commit-block encoding (including text dictionaries), and the hot vector query path. Preserve the single-vector lossless codec, dense position indexes, LIMIT residency fixes, SQL grammar and durability gate. The implementation choice is the agent's, within the requested experimental scope. A broad rewrite or reverting to c919 would erase unrelated changes and reduce the diagnostic value. This group ablation cannot identify which individual method caused a platform result.

**AMD scope:** No HIP/ROCm/CAGRA runtime dependency exists in src, Cargo or the image. Historical AMD reuse belongs to the local generator/oracle/Recall toolkit. Leave historical assets intact; do not use that toolkit to accept this version. Use official public cases, fixed analytical vectors, and c919 differential checks. Do not claim that absent GPU code was removed.

**Stack:** Rust 1.99.1, existing locked dependencies, unchanged Dockerfile and JSONL interface. Fresh data directories for tests. Independent branch; original competition branch stays at 781a14d until explicitly selected for replacement.

## Execution

- [x] Add three red tests: staged bytes go to disk and remain readable; compressible commits use legacy frames and recover; an eligible vector table does not acquire a hot snapshot.
- [x] Gate the three existing paths. Test default ablation and original `--no-default-features`; retain optimization-specific tests in the latter and document any conditional assertions.
- [x] Run fmt, Clippy all targets/features, Rust tests, original Dockerfile build, public 20 cases, fresh analytical oracle, transaction/error/restart coverage and focused LIMIT comparisons.
- [x] Record performance impact on the identical diagnostic input; do not extrapolate to official E/5000 or claim root cause.
- [x] Update conversation with a compact truthful diagnostic note; retain original quotes and do not fabricate user messages. Verify UTF-8 JSON and <=131072 bytes.
- [x] Review implementation and commit exact owned source paths. Build the committed archive and compare its ELF with the tested image.
- Publication gate: push only the diagnostic branch, compare remote SHA/tree/material bytes and generate the final local archive and handoff.json. The post-push result is recorded in analysis/migration-ablation-20261115/handoff.json to avoid a self-referential commit SHA.

## Review focus

Disk staging must preserve reader offsets and failed-statement isolation. Legacy writes must acknowledge only after sync and must recover on restart. Generic exact search must retain filtering and primary-key tie order. Disabling optimizations must not depend on runtime environment variables. Protocol and submission materials must match the shipped commit.

## Ledger

- Base 781a14d; existing linked worktree clean. GitHub connector and gh both authenticate as faro-123. Original remote branch verified at base.
- Experimental disabling is authorized by the user's request; this is not a confirmed fix of the platform failure.
- Linux tests: default 80, original paths 78. Public cases 20, oracle 782 SQL/8 restarts, LIMIT differential 359 SQL per version. Windows fmt/Clippy pass. Fresh independent review found no blocker/major defect in the implementation diff.
- Single-round query timing is 926.718 ms versus 93.774 ms (781) and 151.015 ms (c919) for 200 requests: retain as diagnostic only. Full evidence and the corrected oracle int/float expectation are documented in docs/migration-ablation-20261115.md.
- User additionally requested historical migration analysis and persistent memory of all this chat's reports; write source-grounded history and a report archive/index under shared project memory.
- Tested source commit cf9b8c58fcbcde0e4fe73eb4d936e3aabeed31b9; committed-archive build exactly matches tested ELF da034ba3e0b03c47869dd2c6c802466699b9591b87ec56864210a61e3b4b25ca. Material is 130205 UTF-8 bytes with original conversations unchanged.
