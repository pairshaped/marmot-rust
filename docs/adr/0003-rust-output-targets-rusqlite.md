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
- named parameters for source SQL that uses `@name`
- `params!` for anonymous placeholders and dense positional slots such as `?1, ?2`
- `named_params!` for sparse numbered slots such as `?2` without `?1`
- positional column reads by index
- concrete row structs
- `execute` for statements without result columns
- `query_map` or `query_row` for result statements

Parameter inference follows SQLite bind slots. Named parameters occupy slots, anonymous `?` placeholders take the next slot after the highest prior slot, numbered `?NNN` placeholders use slot `NNN`, and repeated references to the same slot share one generated argument. If a numbered placeholder refers to a named slot, the generated function binds that slot once using the named placeholder. Dense positional slots bind with `params!`; sparse numbered slots bind by SQLite parameter name so generated functions do not need unused dummy arguments.

The emitter should avoid:

- runtime row mappers
- string-keyed column reads in hot paths
- dynamic type registries
- async wrappers in generated query functions
- generic abstractions that make the generated code harder to read

Generated Rust should look close to hand-written `rusqlite`.

## Consequences

The generator should land near hand-written `rusqlite` performance.

The generated code will be synchronous because `rusqlite` is synchronous.

Async web applications need an explicit blocking boundary around database calls. That boundary belongs in the application or a small runtime adapter, not inside every generated query function.

The first generated rows may be conservative while inference is ported, but the intended output is typed concrete Rust fields.
