# Changelog

Every notable change to tapas lands here. Versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.0.0](https://github.com/xjunior/tapas/compare/v0.3.0...v1.0.0) (2026-11-28)


### ⚠ BREAKING CHANGES

* a config that relied on its declared `deps` alone now also follows the source's foreign keys, which can bring more rows. Add `foreign_keys: false` to the config, or pass `--disable-foreign-keys`, to keep the old behaviour.
* make limit say how far a sampler reaches ([#14](https://github.com/xjunior/tapas/issues/14))

### Features

* add --debug to show the values errors leave out ([#12](https://github.com/xjunior/tapas/issues/12)) ([bc017d3](https://github.com/xjunior/tapas/commit/bc017d3af970295127e4601e8400f7973d9c3d2e))
* follow the source's foreign keys as deps ([#16](https://github.com/xjunior/tapas/issues/16)) ([6e705e6](https://github.com/xjunior/tapas/commit/6e705e6e93a4cb3428c5158ea4a08be7c4ec8b06))
* let of and deps follow several columns ([#15](https://github.com/xjunior/tapas/issues/15)) ([56c9757](https://github.com/xjunior/tapas/commit/56c9757b5651b6792680fa8fbabc42712d1bafd2))
* make limit say how far a sampler reaches ([#14](https://github.com/xjunior/tapas/issues/14)) ([961e170](https://github.com/xjunior/tapas/commit/961e1700113ef3b70fbb0d8b88e59bba10204a85))

## [0.3.0](https://github.com/xjunior/tapas/compare/v0.2.0...v0.3.0) (2026-11-27)


### Features

* read from and write to Postgres ([#9](https://github.com/xjunior/tapas/issues/9)) ([cc9e2a2](https://github.com/xjunior/tapas/commit/cc9e2a22d81dddf168aa501a866969644c06d657))


### Bug Fixes

* say why the target refused a statement ([#11](https://github.com/xjunior/tapas/issues/11)) ([43003a5](https://github.com/xjunior/tapas/commit/43003a59704cf887ecda65bf9c3c652cc751aa6d))

## [0.2.0](https://github.com/xjunior/tapas/compare/v0.1.0...v0.2.0) (2026-11-27)


### Features

* gzip the output when its path ends in .gz ([6c198d6](https://github.com/xjunior/tapas/commit/6c198d6d95a3b1ab538eeac0d81df9de1f53f8fd))
* read sanitize rules from the config ([ed3a086](https://github.com/xjunior/tapas/commit/ed3a086223b345f56400807d3e73385938f1cc6b))
* sanitize values on their way into the sample ([f2b7710](https://github.com/xjunior/tapas/commit/f2b7710e735424c071a48023aa27b4f656e31e73))
* write the sample straight into a target database ([8fab89d](https://github.com/xjunior/tapas/commit/8fab89da73e858286f5f2da589cd088e3a6c2a6b))


### Bug Fixes

* require Rust 1.89, the oldest that builds the dependencies ([c3c2260](https://github.com/xjunior/tapas/commit/c3c226030371c1cd05b5abbdec33ba72f3293bdf))


### Performance Improvements

* cut allocations on the per-row paths ([35e25f0](https://github.com/xjunior/tapas/commit/35e25f092dae7d3715c156f7d5b672384c6d27c0))

## [0.1.0] - 2026-11-27

The first version: a sample of a MySQL database, written to a SQL file, with every reference intact.

### Added

- **Samplers** in YAML, each picking rows from one table with `where`, `order_by` and `limit`. A table can have as many
  samplers as you like; its rows are each written once.
- **`of`**, to take only the rows that belong to rows already in the sample.
- **`deps`**, to bring along every row a table's rows point at, including rows that were themselves brought along.
- **`name`**, so an `of` can follow one sampler's rows instead of its whole table's.
- **Cycles.** Tables that lead to each other, or to themselves, are extracted in rounds until nothing new turns up.
- **`time_column` and `--since`**, so one config gives each environment its own size: `--since '2 weeks'`,
  `--since '3 months'`, or `--since ever`. `--since` is required once any sampler has a `time_column`.
- **`--plan`**, which prints every table in run order with the queries its samplers will send, without connecting.
- **Several config files at once**, with glob patterns tapas expands itself, `**` included.
- **Parallel extraction.** Independent tables, samplers and `IN (...)` chunks all run at once, up to `--connections`.
- **A SQL file you can trust.** It's written to `<file>.partial` and only renamed once the whole sample worked, with
  foreign-key checks off so it loads in any order.
- **Read-only connections** to the source, set up from `--source` or `TAPAS_SOURCE_URL`.
- **Clear errors.** Unknown keys, missing parents, name clashes and conflicting `deps` are caught before anything runs,
  and every error names the file and sampler it came from.

[0.1.0]: https://github.com/xjunior/tapas/releases/tag/v0.1.0
