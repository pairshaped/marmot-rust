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
- requires immutable connections for reads and stronger mutation-capable connections for writes
- lowers source-level SQL parameters to dense positional binds in generated Rust
- runs forward-only SQL migrations, seed files, and database resets through `marmot::migrations`, `marmot::seeds`, and `marmot::reset`
- owns declarative SQLite views under `src/db_views` and reconciles them after migrations
- supports named database references for multi-database projects
- can enforce configured temporal suffixes and generate checked Rust date/datetime boundary types for those columns

The remaining work is deeper SQL coverage and polish around diagnostics and edge-case inference.

### Declared types are semantic input

Marmot uses SQLite declared types to build its language-neutral query model. In particular, `BOOLEAN` and `BOOL` generate Boolean parameters and result fields even though SQLite stores those values as integers.

SQLite `STRICT` tables reject `BOOLEAN` and `BOOL`. To preserve explicit Boolean semantics with an allowed integer storage type, name the column's canonical 0/1 check constraint `boolean`:

```sql
CREATE TABLE settings (
  enabled INTEGER NOT NULL
    CONSTRAINT boolean CHECK (enabled IN (0, 1))
) STRICT;
```

Marmot generates `enabled` as `bool`. The same named constraint works on ordinary non-strict tables. The marker must be a column-level constraint on an `INT` or `INTEGER` column, and its check must use the exact shape `CHECK (column IN (0, 1))`. Marmot rejects malformed markers during analysis.

An unnamed 0/1 check remains an integer because that shape can represent an index, numeric flag, or two-value domain type. Marmot does not infer Boolean semantics from values, column names, defaults, or unnamed constraints. See [ADR 0003](docs/adr/0003-declared-types-are-semantic-input.md).

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
bootstrap_dir = "db/bootstrap"
seeds_dir = "db/seeds"
migration_table = "schema_versions"
schema_output = "db/schema.sql"
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

### Temporal Suffixes

Temporal suffix enforcement is opt-in:

```toml
[tools.marmot.temporal]
strict_suffixes = true
datetime_suffixes = ["_at"]
date_suffixes = ["_on"]
datetime_storage = "text_second_utc"
date_storage = "text_ymd"
```

With `strict_suffixes = true`, columns ending in a configured datetime suffix
must be declared as `TEXT` and generate `temporal::DbDateTime`. Columns ending
in a configured date suffix must be declared as `TEXT` and generate
`temporal::DbDate`.

If stored text does not match the configured temporal format, generated row
decoding includes the generated field name, bad value, and expected format in
the error. The original `rusqlite` conversion error remains in the error source
chain.

The storage keys are explicit because they are part of the project contract,
but they are not a menu of equally supported backends yet. The only supported
values today are:

- `datetime_storage = "text_second_utc"` for `YYYY-MM-DD HH:MM:SS` UTC datetime text
- `date_storage = "text_ymd"` for `YYYY-MM-DD` date text

Unknown storage values are rejected.

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

### Allowlisted update columns

SQLite parameters can bind values, but they cannot bind column names. For the
rare update whose target column is selected at runtime, declare the complete
allowlist in the function block:

```sql
-- func: update_user_field
-- columns: display_name, email
update users
set {{column}} = @value,
    updated_at = @now
where id = @id;
```

Marmot verifies that every choice is a real column on the updated table, quotes
each identifier, prepares every resulting statement during analysis, and
generates a closed `UpdateUserFieldColumn` Rust enum. The marker must be the
complete left side of a direct `{{column}} = @value` assignment. Only one
column allowlist and marker are allowed per function. A long allowlist can
continue on another `-- columns:` directive:

```sql
-- columns: first_name, last_name, email
-- columns: phone, city, country, postal_code
```

`@column` is reserved because generated parameter structs use `column` for the
selector enum. Marmot rejects that collision during analysis. Reusing another
named parameter is allowed and follows SQLite's normal single-binding behavior.

When every allowed column has a compatible type, `@value` keeps that inferred
type. If the columns have incompatible types, only `@value` falls back to
`rusqlite::types::Value`; the column remains a closed enum. This feature does
not substitute table names, clauses, expressions, ordering, or arbitrary SQL
fragments.

### Read and mutation connections

Marmot asks SQLite whether each prepared statement is read-only. Read statements
generate functions that accept `&Connection`. Anything SQLite reports as a
write, including `INSERT`, `UPDATE`, `DELETE`, write statements with `RETURNING`,
and DDL, generates a function that accepts `impl MutationConnection`.

`MutationConnection` is a sealed generated trait implemented for
`&mut Connection` and `&Transaction`. This means an immutable `&Connection`
cannot call generated mutation SQL:

```rust
let mut conn = rusqlite::Connection::open("app.sqlite")?;
queries::create_item(&mut conn, "Stone")?;

let tx = conn.transaction()?;
queries::create_item(&tx, "Broom")?;
tx.commit()?;
```

Existing callers must make standalone write connections mutable and pass
`&mut conn`. Callers already inside a transaction should pass `&transaction`.
Read calls stay unchanged. Marmot rejects multi-statement query blocks, so each
generated function has one SQLite access classification.

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

### Declarative views

Put reusable, permanent SQLite views in `src/db_views`. Each view has one file,
and its filename must match the physical view name:

```sql
-- src/db_views/view_active_memberships.sql
CREATE VIEW view_active_memberships (participant_id, season_started_in) AS
SELECT participant_id, season_started_in
FROM line_items
WHERE status IN ('submitted', 'paid');
```

View names use lowercase `view_` prefixes. The declaration requires an explicit
output column list because those names are the view's public contract. A file
contains one native `CREATE VIEW` statement, and the SQL view name must match
the filename. The directory and filename convention identifies declarative
view code, so no Marmot annotation is required. Parameters are not supported.

`inspect` and `generate` install the declarations before Marmot analyzes queries
that consume them. `generate` also writes the disposable aggregate
`src/generated/sql/views.sql`. `generate --check` verifies that aggregate and
fails if the database contains a managed `view_*` without a declaration.

`migrate` reconciles declared views after forward migrations. `reset` reconciles
them after migrations and before seeds, so seed SQL can read declared views.
Both commands accept `--source-root`; without it they use `marmot.toml` or
`src`. Pass `--deny-view-warnings` in deployment checks to reject stale managed
views. When `schema_output` is configured, both commands write the deterministic
schema dump after the database lifecycle succeeds.

Marmot replaces the complete declared view set transactionally. It drops every
declared view, creates every current definition, then prepares a zero-row query
against each one. That final preparation catches missing dependencies, cycles,
and incompatible output contracts. A failure rolls back to the previous view
set.

Removing a source file does not drop the installed view. Add an explicit
forward migration with `DROP VIEW IF EXISTS`, then run the audit:

```sh
cargo run -- audit-views --database path/to/app.db --source-root path/to/src
cargo run -- audit-views --database path/to/app.db --source-root path/to/src --deny-warnings
```

The audit prints copyable migration SQL for database-only `view_*` objects. It
never executes that removal itself.

SQLite exposes declared types for direct view columns and explicit casts, so
Marmot can retain those generated types. SQLite does not preserve useful
`NOT NULL` metadata through a view, and an uncast expression may have no declared
type. Marmot handles those cases conservatively: view results are nullable, and
untyped expressions generate `rusqlite::types::Value`. Use `CAST` in the view
definition when an expression needs a stable generated type.

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

Production-safe bootstrap data is separate from development and test fixtures:

```sh
cargo run -- bootstrap \
  --database path/to/app.db \
  --bootstrap-dir db/bootstrap
```

`bootstrap_dir` is optional. Reset runs it before `seeds_dir` when configured.
Seed and bootstrap filenames use lowercase letters, digits, and underscores.
Unlike migrations, they do not need a numeric prefix because Marmot reruns
every file in lexical filename order.

Reset a database, then run migrations and seeds:

```sh
cargo run -- reset \
  --database path/to/app.db \
  --migrations-dir db/migrations \
  --seeds-dir db/seeds
```

Write the database schema in deterministic object order, or verify that a
committed dump is current:

```sh
cargo run -- dump-schema --database path/to/app.db --output db/schema.sql
cargo run -- dump-schema --database path/to/app.db --output db/schema.sql --check
```

The dump contains schema only. It has no timestamps or migration data, so a
reset does not manufacture history-only diffs.

## Design Notes

The generator should emit boring concrete code. Runtime speed should come from staying close to hand-written `rusqlite`: cached prepared statements, positional parameter binds, positional row access, concrete row structs, and no dynamic mapper layer. SQL files can use named parameters for readability, but generated runtime SQL lowers them to dense positional slots so SQLite does not do a name lookup on every call.

SQLx's SQLite analyzer is useful reference material for closing inference gaps. In particular:

- declared SQLite type mapping
- statement description
- result-column nullability
- fallback inference from `EXPLAIN`
- offline metadata shape

Marmot should borrow ideas carefully, and only copy code when there is a clear reason and the license notice is preserved. SQLx should not be part of generated runtime code.
