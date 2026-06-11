---
# marmot-g6gy
title: Infer nullable placeholders in optional-filter predicates
status: todo
type: bug
priority: high
created_at: 2026-06-11T15:43:47Z
updated_at: 2026-06-11T15:43:47Z
---

Marmot currently infers a placeholder as required when the same placeholder is used in an optional-filter predicate such as:

```sql
where (?1 is null or line_items.item_id = ?1)
```

Observed while porting curling broadcast email filters. Because `line_items.item_id` is non-null/integer, the generated Rust function required `i64`, so callers could not pass `None` even though the SQL explicitly treats null as "no filter".

Expected behavior: if a placeholder is tested with `is null` or `is not null`, generated Rust should preserve nullability for that parameter, even if the placeholder also appears in a comparison against a non-null column.

Acceptance criteria:
- [ ] Add a failing fixture/query that uses `(? is null or column = ?)` with an integer column.
- [ ] Generate an `Option<i64>` parameter for the placeholder.
- [ ] Cover the same pattern for string filters such as `(? is null or status = ?)`.
- [ ] Document the inference rule or add it to existing generator expectations.
