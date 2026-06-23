---
# marmot-m36e
title: Preserve nullability through cast output expressions
status: completed
type: bug
priority: high
created_at: 2026-06-23T13:14:21Z
updated_at: 2026-06-23T13:18:06Z
---

Casting a nullable output expression currently makes Marmot infer a non-null generated Rust type. SQLite returns NULL for casts of NULL, including nullable columns and no-row scalar subqueries.

Acceptance criteria:

- [x] Add regression coverage for casting a nullable table column and verify the output is nullable.
- [x] Add regression coverage for casting a scalar subquery that can return no rows and verify the generated Rust field is nullable.
- [x] Preserve existing non-null cast inference for non-null expressions like count/coalesce/literals/non-null columns.
- [x] Run cargo fmt and cargo test.

## Summary of Changes

- Changed cast output inference to keep the target Rust value type while preserving the nullability of the expression being cast.
- Added scalar subquery output inference so no-row scalar subqueries generate nullable fields.
- Added derived-table column inference for output expressions so casts over `from (select ...) alias` keep known column metadata.
- Added analyzer and e2e regression coverage for nullable scalar subqueries, nullable casts, and casted scalar subqueries.
- Validated with `cargo fmt` and `cargo test`.
