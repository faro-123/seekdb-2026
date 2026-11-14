# LIMIT disk-index residency

The submitted 13bf04d still fails on the platform, according to the user. Local tests reproduce a distinct LIMIT cost regression, but the platform supplies no failing input or exit log. Repair the reproducible classes and verify the final candidate against c919d64 without claiming that the hidden platform cause is known.

## Evidence

Local evidence is under `C:/Users/faro/Documents/ChatGPT/MinivectorDB/analysis/limit-retest-20261115`. Two successful images have been fingerprinted against their commits. In a 100,000-row table, ordinary and covering projections alternate. The current ordinary compatibility index occupies 6 MiB, and the covering build retains two 6 MiB files. The 8 MiB/8-entry eviction policy retains only the last oversized index. Each change of projection rebuilds from WAL. Three mixed runs take 134.301/136.274/136.322 seconds on 13bf04d versus 10.829/10.618/10.518 seconds on c919d64. All finished responses agree. A strict 600-second long-loop reproduction is in progress.

The 300,000-row scale probe times out during CREATE VECTOR INDEX in both versions before LIMIT queries start. Retain that evidence separately. It is not evidence of a LIMIT-specific result defect. Investigate further only where it prevents required LIMIT validation.

## Approaches considered

1. Increase the byte/count thresholds. This only moves the failing boundary and cannot prevent rebuilds for larger tables or more tables.
2. Keep constructed compatibility indexes as ordinary derived disk indexes for the store lifetime, updating them at commit. File contents remain on disk and the kernel can reclaim file pages under the memory limit. Per-index heap state is bounded metadata. This removes size/count-triggered rebuilding and is the chosen approach.
3. Rewrite the primary position-index format to retain legacy ordering natively. This could avoid the extra derived indexes, but changes the core representation and the successful insertion/deletion ordering contract. It is broader than the measured failure warrants.

The choice is an implementation decision under the user's explicit repair goal. It is not attributed to user approval of this detailed design.

## Implementation contract

- Retain one lazily constructed index per queried `(table, covering)` for the store lifetime. Remove eviction based on disk-file bytes or number of indexes. Memory must not grow with the number of rows except reclaimable file pages. Metadata and file handles scale with queried tables, like the existing table/index catalog.
- Continue incremental update in original committed WAL operation order. BEGIN, failed statements, empty commits, and rollback do not discard committed indexes. A change to another table does not make a retained index stale.
- Preserve active-reader generations. An index referenced by an outstanding view must be detached before commit; only that exceptional snapshot path may rebuild its next generation.
- A covering index rebuild currently needs the primary scratch index to reconstruct vector-index insertion order. Once replay finishes and the vector index is selected, close and remove that unused primary file. Retain only the file actually queried. Never alter the order of operations or substitute live-key sorting for historical insertion order.
- First access after open/restart may rebuild once. Restart still removes derived scratch state and recovers from the authoritative WAL. No change to WAL format, persistence acknowledgment, error precedence, SQL grammar, Dockerfile, dependencies, or protocol.
- Builder-check findings adopted: retained compatibility files must not retain one open writer per table. Allow DiskIndex to close its writer and reopen lazily, without losing its path/capacity/length. Park indexes after building and at commit completion. During replay, retain writers for at most the current table and close them when switching tables. Read iterators own only their existing per-query handles.
- Replace the flat cache list with a BTreeMap keyed by table and two optional projection entries. Apply only to the affected table. Detach active generations when that table is mutated. Remove per-entry committed_end matching: retained entries advance synchronously with every successful apply, and unaffected tables need no invalidation or generation scan. No full-cache traversal per write or query.
- Old self-imposed cache-budget tests must be replaced with no-rebuild invariants. Keep exact output assertions, not only performance assertions.
- Review transaction staging, LIMIT 0, ordering, filtering, vector coverage, large K and malformed LIMIT for additional independently reproducible defects. StageIndex runs are already bounded at 4096 rows; do not rewrite unrelated mechanisms without a failing case.

## Acceptance

1. RED: demonstrate that ordinary/covering switching above the old byte threshold and repeated access to more than eight tables rebuild on the old implementation. Use deterministic in-process sequence/file checks plus the external timeout reproduction.
2. GREEN: retain stable cache generations and exact successful-version subsets through growth, updates, failed writes, transactions, index creation, deletion/reinsertion and restarts. Retest active readers.
3. Run full Rust suite, fmt and strict clippy. Build the candidate with the original Dockerfile from its committed source. Verify the ELF and revision label.
4. Execute actual Docker differential tests, public20, recovery/fault cases and wide/low-memory checks. Include many-table resident indexes under 96 MiB and file-descriptor/resource observations.
5. Rerun the same original and new 600-second LIMIT loops, repeat latency comparisons, and retain complete input/output and resource evidence. Limit claims to observed rows, schema, limits and environment.
6. Independent source review and evidence audit precede final version readiness. Update conversation from actual messages/results within 128 KiB. Keep the failed submitted version and all failed runs reproducible.

No absolute proof over unknown hidden cases is possible. Completion means the reproduced LIMIT failure classes and discovered correctness regressions are fixed and the specified evidence passes. The final report must state cold-path and platform-verification limits.
