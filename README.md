# Marmot

Experimental Rust port of Marmot.

See [BENCHMARK_REPORT.md](BENCHMARK_REPORT.md) for the Rust SQLite benchmark
summary.

The goal is to keep Marmot's source layout:

```text
src/items.rs
src/items.sql
src/orders.rs
src/orders.sql
```

and generate direct database code from those colocated SQL files.

The first target is Rust + `rusqlite`. The useful end state is a shared analyzer with multiple emitters:

```text
SQL files + SQLite schema
        |
        v
query model
        |
        +-- Rust / rusqlite
        +-- Gleam / sqlight
```

## Current Status

This is an active Rust port, not a complete replacement for Gleam Marmot yet.

It currently:

- finds module companion SQL files like `src/items.rs` plus `src/items.sql`
- supports multiple `-- func:` blocks per companion SQL file
- supports legacy `src/**/sql/*.sql` query files
- supports a configured SQL root with `--sql-dir`
- derives generated function names from `-- func:` blocks or legacy filenames
- derives generated module names from the owning module
- extracts named parameters like `@org_id`
- prepares each statement with SQLite and records result columns
- infers common parameter and result types from schema metadata, expressions, casts, joins, returning clauses, and insert/update positions
- emits typed direct `rusqlite` functions using `prepare_cached`
- lowers source-level SQL parameters to dense positional binds in generated Rust
- runs forward-only SQL migrations, seed files, and database resets through `marmot::migrations`, `marmot::seeds`, and `marmot::reset`
- supports named database references for multi-database projects

The remaining work is deeper SQL coverage and polish around diagnostics and edge-case inference.

## Usage

Inspect discovered queries:

```sh
cargo run -- inspect --database path/to/app.db --source-root path/to/src
```

Project defaults can live in `marmot.toml`:

```toml
[tools.marmot]
database = "path/to/app.db"
source_root = "path/to/src"
sql_dir = "path/to/src/sql"
output = "path/to/src/generated/sql"
migrations_dir = "db/migrations"
seeds_dir = "db/seeds"
```

Use another config path with `--config path/to/marmot.toml`. CLI flags override config values.

Multi-database projects can use named references:

```toml
[tools.marmot.databases.app]
path = "db/app.sqlite"
sql_dir = "src/sql/app"
output = "src/generated/sql/app"
migrations_dir = "db/migrations/app"
seeds_dir = "db/seeds/app"

[tools.marmot.databases.analytics]
path = "db/analytics.sqlite"
sql_dir = "src/sql/analytics"
output = "src/generated/sql/analytics"
migrations_dir = "db/migrations/analytics"
seeds_dir = "db/seeds/analytics"
```

Pass `--database-name app` to target one named database. Without it, `generate`, `migrate`, `seed`, and `reset` run every named database in sorted name order. Ambient `DATABASE_URL` does not replace named database paths in that mode. Named references can omit paths; Marmot derives `db/NAME.sqlite`, `src/sql/NAME`, `src/generated/sql/NAME`, `db/migrations/NAME`, and `db/seeds/NAME`.

Generate Rust files:

```sh
cargo run -- generate \
  --database path/to/app.db \
  --source-root path/to/src \
  --output path/to/src/generated/sql
```

A companion SQL file contains named blocks:

```sql
-- func: get_item_by_id
select id, name from items where id = @id;

-- func: list_items
select id, name from items order by name;
```

Generate from a configured SQL root:

```sh
cargo run -- generate \
  --database path/to/app.db \
  --source-root path/to/src \
  --sql-dir path/to/src/sql \
  --output path/to/src/generated/sql
```

Check generated files without writing:

```sh
cargo run -- generate \
  --database path/to/app.db \
  --source-root path/to/src \
  --output path/to/src/generated/sql \
  --check
```

Run migrations:

```sh
cargo run -- migrate \
  --database path/to/app.db \
  --migrations-dir db/migrations
```

Run seeds:

```sh
cargo run -- seed \
  --database path/to/app.db \
  --seeds-dir db/seeds
```

Reset a database, then run migrations and seeds:

```sh
cargo run -- reset \
  --database path/to/app.db \
  --migrations-dir db/migrations \
  --seeds-dir db/seeds
```

## Design Notes

The generator should emit boring concrete code. Runtime speed should come from staying close to hand-written `rusqlite`: cached prepared statements, positional parameter binds, positional row access, concrete row structs, and no dynamic mapper layer. SQL files can use named parameters for readability, but generated runtime SQL lowers them to dense positional slots so SQLite does not do a name lookup on every call.

SQLx's SQLite analyzer is useful reference material for closing inference gaps. In particular:

- declared SQLite type mapping
- statement description
- result-column nullability
- fallback inference from `EXPLAIN`
- offline metadata shape

Marmot should borrow ideas carefully, and only copy code when there is a clear reason and the license notice is preserved. SQLx should not be part of generated runtime code.
