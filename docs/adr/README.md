# Marmot Architecture Decisions

These records govern Marmot's current design. The [component README](../../README.md)
defines the public boundary and implementation entry points.

- [0001: Colocated SQL files are the source interface](0001-colocated-sql-files.md)
- [0002: Separate query analysis from language emitters](0002-query-model-and-emitters.md)
- [0003: Treat declared types as semantic input](0003-declared-types-are-semantic-input.md)
- [0004: Rust output targets rusqlite directly](0004-rust-output-targets-rusqlite.md)
- [0005: Async applications use a blocking database boundary](0005-async-applications-use-a-blocking-database-boundary.md)
- [0006: Use forward-only SQL file runners](0006-forward-only-sql-file-runners.md)
- [0007: Use Rust-neutral CLI configuration](0007-rust-cli-configuration.md)
- [0008: Treat SQLite views as declarative database code](0008-declarative-sqlite-views.md)

Numbering is unique and append-only. Update the owning decision when the design
changes. Beans own delivery status and do not belong in ADR status sections.
