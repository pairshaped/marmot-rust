# Marmot Rust SQLite Benchmark Report

## Summary

This benchmark compares Marmot-generated Rust `rusqlite` calls against a few
useful reference points:

- raw Rust `rusqlite` as the low-level SQLite baseline
- Rust Marmot-generated `rusqlite`
- Rust SQLx SQLite variants
- Ruby ActiveRecord as a higher-level ORM reference

The workload is synthetic, but app-shaped. It uses dummy data and request shapes
based on ordinary server-side pages: many small indexed reads, a parent lookup,
and a short transactional update.

The dedicated benchmark host is:

- OS: Debian GNU/Linux 13 (trixie)
- CPU: AMD Ryzen 7 9700X, 8 cores / 16 threads
- Memory: 62 GiB
- Architecture: x86_64

See the [full benchmark report](https://github.com/pairshaped/gleam_sqlite_benchmarks/blob/master/REPORT.md)
for the complete cross-runtime comparison.

## Results

These are median rows from a five-run benchmark with 10,000 simulated requests
per run.

| Runner | Case | Median Time | Median us/item | Relative Notes |
| --- | --- | ---: | ---: | --- |
| Rust `rusqlite` | `rust_rusqlite/app_request/admin_item_edit` | 591,459us | 59 | low-level SQLite baseline |
| Rust `rusqlite` | `rust_rusqlite/app_request/admin_item_update` | 284,919us | 28 | low-level SQLite baseline |
| Rust Marmot | `rust_marmot/app_request/admin_item_edit` | 776,036us | 77 | generated `rusqlite` calls |
| Rust Marmot | `rust_marmot/app_request/admin_item_update` | 218,269us | 21 | generated `rusqlite` calls |
| Rust SQLx pool 5 | `rust_sqlx/app_request/admin_item_edit` | 1,826,036us | 182 | default SQLx pool-shaped path |
| Rust SQLx pool 5 | `rust_sqlx/app_request/admin_item_update` | 574,538us | 57 | default SQLx pool-shaped path |
| Rust SQLx pool 1 | `rust_sqlx_pool1/app_request/admin_item_edit` | 2,777,851us | 277 | single pooled connection capacity |
| Rust SQLx pool 1 | `rust_sqlx_pool1/app_request/admin_item_update` | 512,644us | 51 | single pooled connection capacity |
| Rust SQLx acquired connection | `rust_sqlx_conn/app_request/admin_item_edit` | 1,129,149us | 112 | one acquired connection held for the loop |
| Rust SQLx acquired connection | `rust_sqlx_conn/app_request/admin_item_update` | 499,218us | 49 | one acquired connection held for the loop |
| Rust SQLx direct connection | `rust_sqlx_direct/app_request/admin_item_edit` | 1,134,108us | 113 | direct `SqliteConnection` |
| Rust SQLx direct connection | `rust_sqlx_direct/app_request/admin_item_update` | 487,329us | 48 | direct `SqliteConnection` |
| Rust SQLx tuned direct connection | `rust_sqlx_direct_tuned/app_request/admin_item_edit` | 1,077,739us | 107 | direct connection with SQLx tuning knobs |
| Rust SQLx tuned direct connection | `rust_sqlx_direct_tuned/app_request/admin_item_update` | 504,356us | 50 | direct connection with SQLx tuning knobs |
| Rust SQLx manual transaction | `rust_sqlx_manual_tx/app_request/admin_item_update` | 475,469us | 47 | explicit transaction SQL |
| Ruby ActiveRecord SQLite | `active_record/app_request/admin_item_edit` | 12,732,861us | 1,273 | ORM reference |
| Ruby ActiveRecord SQLite | `active_record/app_request/admin_item_update` | 4,230,022us | 423 | ORM reference |

## Methodology

Each benchmark row prints:

```text
case,items,micros,us_per_item,check
```

The benchmark uses fixed dummy seed data. The row count controls how many
simulated requests run, not how many seed rows are created.

SQLite connections use:

```sql
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
PRAGMA busy_timeout=5000;
PRAGMA foreign_keys=ON;
```

### `app_request/admin_item_edit`

This represents a read-heavy admin edit page. It performs many small indexed
queries, including point selects, filtered counts, small lookup reads, and one
parent lookup.

### `app_request/admin_item_update`

This represents a short save request. It opens a transaction, performs a few
request-sized reads, updates one row, and commits.

## SQLx Variants

The SQLx rows are included to separate Marmot's generated `rusqlite` path from
the cost of different SQLx usage patterns:

- `rust_sqlx/*`: a normal `SqlitePool` with five max connections
- `rust_sqlx_pool1/*`: a `SqlitePool` constrained to one connection
- `rust_sqlx_conn/*`: one acquired pool connection held across the request loop
- `rust_sqlx_direct/*`: a direct `SqliteConnection`
- `rust_sqlx_direct_tuned/*`: direct connection with SQLx tuning knobs enabled
- `rust_sqlx_manual_tx/*`: explicit transaction SQL for the update request

For a local SQLite application, the main question is whether SQLx's async and
pooling model is worth the overhead. Marmot's Rust target is intentionally
closer to hand-written `rusqlite`: cached prepared statements, positional
parameter binds, and concrete row decoding.
