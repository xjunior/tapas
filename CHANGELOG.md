# Changelog

Every notable change to tapas lands here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0](https://github.com/xjunior/tapas/compare/v0.1.0...v0.2.0) (2026-11-27)


### Features

* gzip the output when its path ends in .gz ([fdad529](https://github.com/xjunior/tapas/commit/fdad529b10fa35c3fd4ff07da639fe11760d563f))
* gzip the output when its path ends in .gz ([6c198d6](https://github.com/xjunior/tapas/commit/6c198d6d95a3b1ab538eeac0d81df9de1f53f8fd))
* read sanitize rules from the config ([ed3a086](https://github.com/xjunior/tapas/commit/ed3a086223b345f56400807d3e73385938f1cc6b))
* sanitize sensitive values with fakes that stay consistent ([0737941](https://github.com/xjunior/tapas/commit/0737941c83b5e69a64bd0f00e94c04f5303ac0aa))
* sanitize values on their way into the sample ([f2b7710](https://github.com/xjunior/tapas/commit/f2b7710e735424c071a48023aa27b4f656e31e73))
* write the sample straight into a target database ([cccfab7](https://github.com/xjunior/tapas/commit/cccfab7cf19e62dd7630b95ec7d5f3e890e053b0))
* write the sample straight into a target database ([8fab89d](https://github.com/xjunior/tapas/commit/8fab89da73e858286f5f2da589cd088e3a6c2a6b))


### Bug Fixes

* require Rust 1.89, the oldest that builds the dependencies ([c3c2260](https://github.com/xjunior/tapas/commit/c3c226030371c1cd05b5abbdec33ba72f3293bdf))


### Performance Improvements

* cut allocations on the per-row paths ([35e25f0](https://github.com/xjunior/tapas/commit/35e25f092dae7d3715c156f7d5b672384c6d27c0))

## [Unreleased]

### Added

- **`--target`**, to write the sample straight into another database instead of a file, in one transaction that's
  only committed once the whole sample worked. `TAPAS_TARGET_URL` works too.

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

[Unreleased]: https://github.com/xjunior/tapas/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/xjunior/tapas/releases/tag/v0.1.0
