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

These are median results from five-run benchmarks. Each timing value is the
average request time from a 10,000-request benchmark run.

### `app_request/admin_item_edit`

| Runner | Time (us/item) | vs `rusqlite` | req/sec |
| --- | ---: | ---: | ---: |
| `rusqlite` | 59 | 1.0x | 16,907 |
| Rust Marmot | 77 | 1.3x | 12,886 |
| Rust SQLx pool 5 | 182 | 3.1x | 5,476 |
| Rust SQLx pool 1 | 277 | 4.7x | 3,600 |
| Rust SQLx acquired connection | 112 | 1.9x | 8,856 |
| Rust SQLx direct connection | 113 | 1.9x | 8,818 |
| Rust SQLx tuned direct connection | 107 | 1.8x | 9,279 |
| Ruby ActiveRecord SQLite | 1,273 | 21.5x | 785 |

### `app_request/admin_item_update`

| Runner | Time (us/item) | vs `rusqlite` | req/sec |
| --- | ---: | ---: | ---: |
| `rusqlite` | 28 | 1.0x | 35,098 |
| Rust Marmot | 21 | 0.8x | 45,815 |
| Rust SQLx pool 5 | 57 | 2.0x | 17,405 |
| Rust SQLx pool 1 | 51 | 1.8x | 19,507 |
| Rust SQLx acquired connection | 49 | 1.8x | 20,031 |
| Rust SQLx direct connection | 48 | 1.7x | 20,520 |
| Rust SQLx tuned direct connection | 50 | 1.8x | 19,827 |
| Rust SQLx manual transaction | 47 | 1.7x | 21,032 |
| Ruby ActiveRecord SQLite | 423 | 14.8x | 2,364 |

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
