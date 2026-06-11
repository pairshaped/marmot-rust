---
# marmot-t321
title: Strip -- returns annotations from generated Rust SQL constants
status: completed
type: bug
priority: normal
created_at: 2026-06-11T14:05:23Z
updated_at: 2026-06-11T14:09:42Z
---

Rust Marmot supports `-- returns:` annotations, but the generated Rust SQL constants currently include the annotation comment in the SQL sent to SQLite.

Example generated constant:
```rust
const COUNT_ADDONS_SQL: &str = r"-- returns: ValueRow
select count(*) as value from app_addons where addonable_id = ?1 and addonable_type = ?2";
```

Gleam Marmot strips this directive from generated SQL. Rust Marmot should do the same: `-- returns:` is generator metadata, not runtime SQL. SQLite accepts the comment, but generated code should not send Marmot directives to the database.

## Work Plan

- [x] Reproduce directive leaking into generated SQL
- [x] Add a regression test at the generator seam
- [x] Strip Marmot `-- returns:` directives from emitted SQL constants
- [x] Run formatter and tests
- [x] Update bean summary

## Summary of Changes

Added a regression that proves analyzed query SQL and generated Rust constants omit leading `-- returns:` directives. Moved directive stripping into `sqlite::annotation`, then applied it during analysis before nullability override stripping so `Query.sql` is runtime SQL. Validation passed with `cargo fmt && cargo test && cargo clippy --all-targets --all-features -- -D warnings`.
