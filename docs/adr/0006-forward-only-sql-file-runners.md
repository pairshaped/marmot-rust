# 0006. Forward-Only SQL File Runners

## Status

Accepted

## Context

Marmot needs database setup workflows that are predictable, reviewable, and close to the generated-query workflow.

Migrations run once and seeds run in filename order every time. Projects may
also need more than one ordered seed directory, for example production-safe
bootstrap data followed by development fixtures.

## Decision

Rust Marmot will provide forward-only SQL file runners as library APIs.

Migration files live in `db/migrations` by default. `migrations::migrate_from` accepts an explicit directory for applications with another layout.

Seed files live in `db/seeds` by default. `seeds::seed_from` accepts an explicit
directory. Projects may separately configure an optional `bootstrap_dir` for
production-safe baseline data. `marmot bootstrap` runs only that directory;
`marmot seed` runs only fixtures; reset runs bootstrap before fixtures.

Migration filenames must match:

```text
NNN_description.sql
```

The `NNN` prefix is three digits. The description uses lowercase letters, digits, and underscores.

Migrations run in filename order. Each migration runs in a transaction. After a
migration succeeds, Marmot records its filename stem in the configured tracking
table. The default is `schema_migrations`; projects with an established table
can configure another safe SQLite identifier. Already-recorded versions are
skipped. Failed migrations are rolled back and are not recorded.

Seed filenames use lowercase letters, digits, and underscores with no required
numeric prefix. Seeds run in lexical filename order every time. Marmot disables
foreign-key enforcement while loading the complete ordered seed set in one
transaction, then runs `PRAGMA foreign_key_check` before committing. A failure
in any file or directory, or any reported foreign-key violation, rolls back the
complete set. Marmot restores the caller's original foreign-key enforcement
setting on both success and failure. Marmot does not create a seed tracking
table.

Reset deletes the configured SQLite database file and companion SQLite files
(`-wal`, `-shm`, and `-journal`), then runs migrations, reconciles declarative
views, and runs every configured seed directory. When a schema output is
configured, migrate and reset write a deterministic schema-only dump after the
lifecycle succeeds. Reset rejects a database path that is a directory.

The SQL-file runner is shared internally so migrations and seeds use the same ordering, filename validation, file reading, and transaction behavior.

## Consequences

Applications can use Marmot for setup without giving up plain SQL files.

The migration model is intentionally simple. There is no down migration support, checksum tracking, or migration editing workflow.

The configured tracking table is part of Marmot's runtime contract for
migrations.
