# 0002. Separate Query Analysis From Language Emitters

## Status

Accepted

## Context

Marmot's useful core is not the text it emits. The useful core is the query model built from SQLite schema metadata, query preparation, result columns, parameter discovery, nullability, and Marmot directives.

The Rust project should preserve that split:

```text
SQL files + SQLite schema
        |
        v
Query model
        |
        +-- Rust emitter
        +-- Gleam emitter
```

The first practical emitter is Rust using `rusqlite`. A Gleam emitter remains a design goal so this project can eventually replace or share behavior with the current Gleam Marmot implementation.

SQLx's SQLite implementation is useful reference material for the analyzer. Its SQLite driver already handles statement description, declared type mapping, nullable column metadata, and `EXPLAIN`-based fallback inference. SQLx is licensed `MIT OR Apache-2.0`, so code can be studied and adapted when that is the clearest path.

## Decision

The analyzer owns source discovery, SQL loading, SQLite introspection, named parameter discovery, result column discovery, nullability inference, and type inference.

Emitters consume a language-neutral query model. They should not inspect SQLite directly.

The analyzer should use direct SQLite introspection as its foundation. SQLx may be used as a reference implementation, test oracle, or optional verification backend, but Marmot should not depend on SQLx for its core runtime output.

The query model should describe database-facing facts:

- query name
- source path
- module name
- SQL text
- parameters
- result columns
- column types
- nullability
- shared return row name

Language-specific concepts belong in emitters.

## Consequences

Rust can be the first usable target without baking Rust assumptions into the analyzer.

Gleam output can reuse the same query facts later.

The analyzer will be the hardest part of the project. Porting Marmot's inference behavior matters more than making the first emitter clever.

SQLx's SQLite code should be consulted for hard inference cases, especially:

- mapping SQLite declared types to Rust types
- deciding when a result column is nullable
- using `EXPLAIN` output when declared column metadata is missing
- representing query metadata for offline checks

Emitter tests should mostly cover known query models, plus generated-code runtime tests for the Rust target. Those runtime tests should generate a small crate, compile it, and run the generated functions against SQLite so parameter binding, row decoding, scalar helpers, returning rows, and execute counts are verified through the emitted public API.

Analyzer tests should use real SQLite schemas and real SQL files.
