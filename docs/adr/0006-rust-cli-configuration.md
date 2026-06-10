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

Missing `marmot.toml` is allowed. In that case Marmot uses built-in defaults for source, output, migrations, and seeds, while still requiring a database from `--database`, `DATABASE_URL`, or config.

## Consequences

Rust projects can run `generate`, `migrate`, `seed`, and `reset` with fewer flags.

The Rust CLI does not read `gleam.toml` by default. Projects that want to share config with Gleam Marmot can pass `--config gleam.toml` as long as the `[tools.marmot]` keys match the Rust CLI's supported subset.

Named database references remain future work.
