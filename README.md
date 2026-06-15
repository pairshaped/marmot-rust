# Marmot

Experimental Rust port of Marmot.

See [BENCHMARK_REPORT.md](BENCHMARK_REPORT.md) for the Rust SQLite benchmark
summary.

The goal is to keep Marmot's source layout:

```text
src/items.rs
src/items.sql
src/registrations/index.rs
src/registrations/index.sql
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
- derives generated function names from `-- func:` blocks
- derives generated module paths from the companion SQL path
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
output = "path/to/src/generated/sql"
init_sql = "db/marmot_init.sql"
migrations_dir = "db/migrations"
seeds_dir = "db/seeds"
```

Use another config path with `--config path/to/marmot.toml`. CLI flags override config values.

Multi-database projects can use named references:

```toml
[tools.marmot.databases.app]
path = "db/app.sqlite"
source_root = "src/app"
output = "src/app/generated/sql"
init_sql = "db/app_marmot_init.sql"
migrations_dir = "db/migrations/app"
seeds_dir = "db/seeds/app"

[tools.marmot.databases.analytics]
path = "db/analytics.sqlite"
source_root = "src/analytics"
output = "src/analytics/generated/sql"
migrations_dir = "db/migrations/analytics"
seeds_dir = "db/seeds/analytics"
```

Pass `--database-name app` to target one named database. Without it, `generate`, `migrate`, `seed`, and `reset` run every named database in sorted name order. Ambient `DATABASE_URL` does not replace named database paths in that mode. Named references can omit paths; Marmot derives `db/NAME.sqlite`, `src/NAME`, `src/NAME/generated/sql`, `db/migrations/NAME`, and `db/seeds/NAME`.

`init_sql` is optional setup for Marmot's analysis connection. Marmot runs the
file after opening SQLite and before reading schema metadata or preparing query
blocks. Named databases inherit `[tools.marmot].init_sql` unless they set their
own `init_sql`.

Warning: `init_sql` is an escape hatch. Marmot does not sandbox it, roll it
back, or check whether it mutates schema or data. It is not a migration system,
and it only runs during `inspect` and `generate`, not when your application
starts. Use it for setup the analyzer needs, such as `ATTACH`, temporary tables,
PRAGMAs, or native SQLite extension loading when your SQLite build and driver
support it.

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

The generated module mirrors the companion SQL file path under the generated
SQL namespace:

```text
src/items.sql                       -> src/generated/sql/items.rs
src/registrations/index.sql         -> src/generated/sql/registrations/index.rs
src/registrations/form.sql          -> src/generated/sql/registrations/form.rs
```

Each `-- func:` block in one companion file becomes a function in the generated
module for that file.

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
