# Changelog

All notable changes to this project are documented in this file.
The project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.27.0] – 2026-10-17

First public release under the NHyDB / dNHyDB Experimental Software License 1.0
(source-available, non-production use only; see `LICENSE.md`).

* Native Rust storage engine (`nhydb-core`) with cell identity
  `(R, u, v, lane)`; `N` is computed arithmetically, never stored as a key.
* Dense BASE records, sparse cells, two lanes, layouts CLASSIC / COLUMN /
  PACKED / CUSTOM, mixed-realm routing, layout evolution, geometric GET.
* Append-only container (file identifier `NHYDB016`) with commit boundary,
  checkpoints, verify, health, stats, repair and compaction; durability
  `fsync` / `full`; portable write lock; database size limit.
* System space R0,u1 and an encrypted external layout plan (AES-256-GCM +
  HMAC, two-generation commit), fail-closed opening.
* Secondary indexes VALUE / TOKEN / TRIGRAM / FULLTEXT (positions, BM25).
* MATH_MAP expression language with resource limits, realm planner.
* QueryEngine (cost-based planner, aggregation, BM25 search) and NHySQL
  (read-only `SELECT`).
* Python package `nhydb` (PyO3) with single-operation API and NumPy batch
  API; command line `nhydb`.
* Platforms: macOS, Linux, Windows.
