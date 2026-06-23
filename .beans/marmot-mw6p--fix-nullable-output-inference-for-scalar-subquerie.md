---
# marmot-mw6p
title: Fix nullable output inference for scalar subqueries
status: completed
type: bug
priority: high
tags:
    - sql-generation
    - nullability
created_at: 2026-06-23T12:59:08Z
updated_at: 2026-06-23T13:02:38Z
---

Marmot handles nullable table columns and left-joined columns as `Option<T>`, but it inferred a nullable scalar subquery output column as a non-null `i64`.

Concrete failing shape from the Rust curling app's `src/pages/public/cart/index.sql`:

```sql
(
  select w.id
  from waivers w
  where ...
  order by w.id asc
  limit 1
) as first_required_waiver_id
```

SQLite returns `NULL` when no waiver row matches. Marmot generated:

```rust
pub first_required_waiver_id: i64
```

Runtime failure:

```text
sqlite error: Invalid column type Null at index: 19, name: first_required_waiver_id
```

Expected behavior:

- The generated row field should be `Option<i64>` for nullable scalar subqueries, or Marmot should support an explicit annotation/cast to declare the generated field nullable.
- User code should not need sentinel values like `coalesce((subquery), 0)` plus `0 -> None` mapping.

Acceptance criteria:

- Add a regression test with a scalar subquery that can return no rows and verify the generated Rust type is `Option<i64>`.
- If automatic inference is not reliable for SQLite expression columns, add a clear explicit nullable annotation mechanism and document it.
- Confirm this applies to output column inference, distinct from placeholder/parameter nullability inference.

## Summary of Changes

- Added scalar-subquery output inference so parenthesized `select` expressions keep the inner value type while marking the outer result nullable.
- Updated output nullability precedence so expression inference can override SQLite origin metadata for expression columns.
- Added analyzer and e2e regression coverage proving a no-row scalar subquery emits `Option<i64>` for the generated Rust field.
- Validated with `cargo test`.
