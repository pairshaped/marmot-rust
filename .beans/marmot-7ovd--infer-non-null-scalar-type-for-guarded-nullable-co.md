---
# marmot-7ovd
title: Infer non-null scalar type for guarded nullable columns
status: todo
type: bug
priority: normal
created_at: 2026-06-25T14:34:18Z
updated_at: 2026-06-25T14:34:18Z
---

## Problem

While updating Curling, Marmot generated `Vec<Option<rusqlite::types::Value>>` for a scalar season query even though the SQL filtered out null seasons. The query was intended to return a simple list of season years.

```sql
-- func: list_account_event_seasons
select distinct cast(p.season as integer) as season
from products p
join product_types pt on pt.org_id = p.org_id
  and pt.slug = p.product_type
  and pt.enabled = 1
join product_type_capabilities event_capability on event_capability.product_type_id = pt.id
  and event_capability.capability_key = 'event'
  and event_capability.enabled = 1
join product_type_capabilities date_bounds_capability on date_bounds_capability.product_type_id = pt.id
  and date_bounds_capability.capability_key = 'date_bounds'
  and date_bounds_capability.enabled = 1
join line_items li on li.product_id = p.id
join orders o on o.id = li.order_id
join participants participant on participant.id = li.participant_id
  and participant.user_id = @user_id
where p.org_id = @org_id
  and p.enabled = 1
  and p.season is not null
  and li.kind = 'product'
  and li.status in ('submitted', 'paid')
  and o.status != 'pending'
order by season desc
```

Generated shape observed:

```rust
pub fn list_account_event_seasons(
    conn: &Connection,
    user_id: i64,
    org_id: i64,
) -> Result<Vec<Option<rusqlite::types::Value>>>
```

Expected shape, or at least the ideal inferred shape:

```rust
Result<Vec<i64>>
```

A variant using `select distinct coalesce(p.season, 0) as season` still inferred `Vec<Option<Value>>`. The Curling workaround was to select `p.season`, accept `Vec<Option<i64>>`, and flatten in Rust.

## Why this matters

For scalar queries, falling back to `rusqlite::types::Value` forces callers to do manual type handling or reshape SQL only to satisfy the generator. Marmot should ideally preserve the scalar integer type when the projection is a cast/coalesce or when `WHERE column is not null` makes the result non-null.

## Acceptance criteria

- [ ] Add a regression fixture for a nullable integer column selected with `WHERE column is not null`.
- [ ] Add a regression fixture for `cast(nullable_integer as integer)` with a non-null guard, or document why cast inference remains nullable.
- [ ] Generated scalar result should avoid `rusqlite::types::Value` when the expression type is clearly integer.
- [ ] Decide whether non-null guards should upgrade `Option<i64>` to `i64`; if not, document the intended nullability boundary.
