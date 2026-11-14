# LIMIT admission candidate — 2026-11-14

Baseline: `da2999690a089faf7bd91949902f0558ea47cad7`.
Engine change: `4b844c7d910ca9bea24d803676056f4d54a7eb6b`.
Successful behavior control: `c919d6436d4914a31ffb958a4d225dc6f640336d`.

Preparing a transaction or a failed statement previously discarded the legacy
LIMIT index. Even an acknowledged write discarded it, forcing the next unordered
LIMIT to scan the complete WAL. The change keeps these derived indexes and applies
new committed operations in WAL order. Active readers retain their old generation;
cache growth still obeys LRU byte and entry limits. Initial, recovered and evicted
indexes still rebuild lazily. No storage format, dependency, Dockerfile or public
protocol change is included.

Local acceptance of the original-Dockerfile Linux/amd64 image:

- Rust: 47 unit and 28 integration tests; fmt and strict clippy passed.
- Public cases: 20/20; 10 resource/recovery groups passed, including 3 migrated
  cases, independent vector oracles, 96 MiB wide rows, 192 MiB recovery and kills.
- Successful-version differential: 19,485 SQL per version, zero differences and
  21 paired restarts. Of these, 16,534 SQL ran as native ELF semantic checks and
  2,951 ran in Docker with the official request/resource settings.
- Identical 100,000-row / 4,000-read-transaction fixture: baseline exhausted the
  original 600 s case budget; candidate completed 12,202 SQL in 13.689 s.
- 4,000 successful UPDATE/LIMIT pairs: 8,202 SQL in 249.950 s, below 600 s.
  Warm post-write LIMIT median was 0.469 ms; cold build was 237.613 ms.
- Incremental multi-table growth crossed the 8 MiB cache budget and evicted
  correctly. Legacy and compact frames, active readers, tombstones, vector
  projection and restart results were checked. Warm-cache WAL short-write/fsync
  failure injection passed without acknowledging failed commits.

ELF SHA-256: `0c67eb3f332d8133e8c5653e184652552c8dfd4e79ad7c922c221efbed96af9a`.
Public package matched upstream `f4d4e26653af823a7601668802f80e374ad6643c`.
The existing regression programs are under `tools/`; local acceptance traces and
the deterministic comparison harness are archived in the project workspace at
`analysis/limit-admission-repair-20261114/`.

Per the user's explicit historical correction, the November 11
success/failure/success sequence concerned conversation JSON formatting and is
excluded from source-failure attribution. Current platform-private cases have
not been run locally; this is a candidate for manual platform submission, not a
claim that the historical platform failure is conclusively explained.
