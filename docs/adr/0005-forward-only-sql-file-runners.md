# 0005. Forward-Only SQL File Runners

## Status

Accepted

## Context

Marmot needs database setup workflows that are predictable, reviewable, and close to the generated-query workflow.

The Gleam implementation has two related behaviors:

- migrations run once and are recorded in `schema_migrations`
- seeds run in filename order every time

Both workflows use ordered SQL files with names like `001_create_users.sql`.

## Decision

Rust Marmot will provide forward-only SQL file runners as library APIs.

Migration files live in `db/migrations` by default. `migrations::migrate_from` accepts an explicit directory for applications with another layout.

Seed files live in `db/seeds` by default. `seeds::seed_from` accepts an explicit directory for applications with another layout.

Migration filenames must match:

```text
NNN_description.sql
```

The `NNN` prefix is three digits. The description uses lowercase letters, digits, and underscores.

Migrations run in filename order. Each migration runs in a transaction. After a migration succeeds, Marmot records its filename stem in `schema_migrations.version`. Already-recorded versions are skipped. Failed migrations are rolled back and are not recorded.

Seeds run in filename order every time. Marmot does not create a seed tracking table.

The SQL-file runner is shared internally so migrations and seeds use the same ordering, filename validation, file reading, and transaction behavior.

## Consequences

Applications can use Marmot for setup without giving up plain SQL files.

The migration model is intentionally simple. There is no down migration support, checksum tracking, or migration editing workflow.

The tracking table is part of Marmot's runtime contract for migrations.
