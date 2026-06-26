---
# marmot-e905
title: Enforce temporal suffix types for Rust codegen
status: completed
type: feature
priority: high
tags:
    - temporal
    - codegen
created_at: 2026-06-25T22:47:59Z
updated_at: 2026-06-25T23:58:04Z
---

Curling has adopted hard temporal storage rules and wants Marmot to own the database boundary conversions.

## Rules Marmot Should Support

- `*_on` columns are date-only calendar values.
- `*_on` columns must be declared as `TEXT` and stored as `YYYY-MM-DD`.
- `*_at` columns are UTC datetime instants.
- `*_at` columns must be declared as `TEXT` and stored as `YYYY-MM-DD HH:MM:SS`.
- Both are second precision only.
- No timezone offset or fractional seconds are stored in the database.
- Marmot should fail generation when a configured temporal suffix is used with a non-`TEXT` type.

## Codegen Goals

- Generate a Rust DB date type for `*_on TEXT` results and params.
- Generate a Rust DB datetime type for `*_at TEXT` results and params.
- Support nullable columns as `Option<DbDate>` / `Option<DbDateTime>`.
- Implement SQLite boundary conversion/validation for these types.
- Propagate the semantic type through inserts, updates, comparisons, and selected result columns.
- Keep suffix matching exact: `_at` and `_on` suffixes only. Do not infer from names that merely contain `at`, `on`, `date`, or `time`.

## Suggested Config Shape

```toml
[temporal]
strict_suffixes = true
datetime_suffixes = ["_at"]
date_suffixes = ["_on"]
datetime_storage = "text_second_utc"
date_storage = "text_ymd"
```

The exact config names can change, but Curling needs to opt into hard validation.

## Acceptance Criteria

- Marmot tests cover result rows, insert params, update params, comparison params, nullable temporal columns, and validation failures for bad suffix/type combinations.
- Invalid stored date/datetime strings fail at decode rather than silently passing through as arbitrary strings.
- Generated code exposes typed temporal values, not raw `String`, for configured temporal suffix columns.
- The implementation is documented enough for Curling to enable it and migrate its schema.

Related Curling coordination bean: `curling-xk9l` in `/Users/daverapin/projects/rust/curling`.


## Implementation Complete

Rust Marmot now supports opt-in temporal suffix enforcement through `[tools.marmot.temporal]`:

- `*_at` columns in strict mode must be `TEXT` and generate `temporal::DbDateTime`.
- `*_on` columns in strict mode must be `TEXT` and generate `temporal::DbDate`.
- Temporal types propagate through result columns, inserts, updates, comparisons, `IN`, `BETWEEN`, `CASE`, CTEs, and nullable parameters.
- Generated `temporal.rs` validates `YYYY-MM-DD` and `YYYY-MM-DD HH:MM:SS` at construction and SQLite decode time.
- Generated code rejects a root SQL module named `temporal` when temporal support needs the shared generated module.

Verification: `cargo fmt && cargo test` passes in `/Users/daverapin/projects/rust/marmot`.
