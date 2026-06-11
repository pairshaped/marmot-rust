# 0003. Rust Output Targets rusqlite Directly

## Status

Accepted

## Context

The Rust application wants typed SQL without giving up SQLite performance.

Benchmarking showed a meaningful gap between direct `rusqlite` and SQLx for small, frequent SQLite queries. SQLx still has good qualities: async integration, mature macros, metadata checking, and nice transaction ergonomics. Those qualities do not outweigh the cost for this use case when the application already has real SQL files and wants SQLite-first behavior.

## Decision

The Rust emitter will generate direct `rusqlite` code.

Generated query functions should use:

- `Connection::prepare_cached`
- generated SQL with dense positional placeholders such as `?1, ?2`
- `params!` for runtime parameter binding
- positional column reads by index
- concrete row structs
- `execute` for statements without result columns
- `query_map` or `query_row` for result statements

Source SQL may use named parameters such as `@club_id`, anonymous `?` placeholders, or numbered placeholders such as `?2`. Those forms are source-level ergonomics and analyzer input, not a runtime binding requirement.

Parameter inference follows SQLite bind slots. Named parameters occupy slots, anonymous `?` placeholders take the next slot after the highest prior slot, numbered `?NNN` placeholders use slot `NNN`, and repeated references to the same slot share one generated argument.

The Rust emitter lowers all generated SQL to dense positional placeholders and binds with `params!`. Sparse numbered slots are compacted into generated slot order instead of forcing unused dummy arguments. Repeated source references to the same bind slot become repeated references to the same generated positional slot. If distinct source slots infer the same logical argument, generated code may bind the same function argument into multiple positional slots.

Generated runtime code should avoid SQLite parameter-name lookups in hot paths.

The emitter should avoid:

- runtime row mappers
- string-keyed column reads in hot paths
- runtime named parameter binding
- dynamic type registries
- async wrappers in generated query functions
- generic abstractions that make the generated code harder to read

Generated Rust should look close to hand-written `rusqlite`.

## Consequences

The generator should land near hand-written `rusqlite` performance.

The generated code will be synchronous because `rusqlite` is synchronous.

Async web applications need an explicit blocking boundary around database calls. That boundary belongs in the application or a small runtime adapter, not inside every generated query function.

The first generated rows may be conservative while inference is ported, but the intended output is typed concrete Rust fields.
