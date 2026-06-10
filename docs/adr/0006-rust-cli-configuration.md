# 0006. Rust CLI Configuration

## Status

Accepted

## Context

The Rust CLI needs repeatable project defaults without requiring every command to pass long path flags. The Gleam implementation reads `[tools.marmot]` from `gleam.toml` because it runs inside Gleam projects. The Rust package is not tied to Gleam, so its default config file should be Rust-tool neutral.

## Decision

The Rust CLI reads `marmot.toml` by default. A different config file can be selected with `--config`.

The supported table is:

```toml
[tools.marmot]
database = "app.sqlite3"
source_root = "src"
sql_dir = "src/sql"
output = "src/generated/sql"
migrations_dir = "db/migrations"
seeds_dir = "db/seeds"
```

CLI flags override config values. `DATABASE_URL` may provide the database path when no `--database` flag is present.

Named database references live under `[tools.marmot.databases.NAME]`:

```toml
[tools.marmot.databases.app]
path = "db/app.sqlite"
sql_dir = "src/sql/app"
output = "src/generated/sql/app"
migrations_dir = "db/migrations/app"
seeds_dir = "db/seeds/app"
```

They can also be written as array entries:

```toml
[[tools.marmot.databases]]
name = "app"
path = "db/app.sqlite"
```

When named databases are configured, `--database-name NAME` selects one reference. Without `--database-name`, `generate`, `migrate`, `seed`, and `reset` run every named reference in sorted name order.

Named references can omit paths. Marmot derives defaults from the database name:

```text
database       db/NAME.sqlite
sql_dir        src/sql/NAME
output         src/generated/sql/NAME
migrations_dir db/migrations/NAME
seeds_dir      db/seeds/NAME
```

If a global `sql_dir`, `output`, `migrations_dir`, or `seeds_dir` is configured, named references without their own value append the database name to that global path. If the global path already ends with the database name, it is used as-is.

`[tools.marmot].database` cannot be combined with named database references. Use a single top-level database config or named references, not both.

Generated output must be under `source_root` after lexical path normalization. This keeps generated Rust in the source tree that owns the SQL files and prevents accidental writes to unrelated directories.

`generate` checks generated output paths across all selected database targets before writing files. If two targets would write the same Rust module or `mod.rs`, generation fails instead of allowing a later target to overwrite an earlier one.

Missing `marmot.toml` is allowed. In that case Marmot uses built-in defaults for source, output, migrations, and seeds, while still requiring a database from `--database`, `DATABASE_URL`, or config.

## Consequences

Rust projects can run `generate`, `migrate`, `seed`, and `reset` with fewer flags.

The Rust CLI does not read `gleam.toml` by default. Projects that want to share config with Gleam Marmot can pass `--config gleam.toml` as long as the `[tools.marmot]` keys match the Rust CLI's supported subset.
