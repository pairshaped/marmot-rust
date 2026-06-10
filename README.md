# Marmot

Experimental Rust port of Marmot.

The goal is to keep Marmot's source layout:

```text
src/items.rs
src/items/sql/get_item_by_id.sql
src/orders.rs
src/orders/sql/list_orders_by_account.sql
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

This is a scaffold, not a replacement for Gleam Marmot yet.

It currently:

- finds `src/**/sql/*.sql`
- derives query names from filenames
- derives generated module names from the owner directory
- extracts named parameters like `@org_id`
- prepares each statement with SQLite and records result column names
- emits direct `rusqlite` functions using `prepare_cached`

It does not yet port Marmot's real type/nullability inference. Generated row fields are currently `rusqlite::types::Value`, with a small integer heuristic for common scalar query shapes.

## Usage

Inspect discovered queries:

```sh
cargo run -- inspect --database path/to/app.db --source-root path/to/src
```

Generate Rust files:

```sh
cargo run -- generate \
  --database path/to/app.db \
  --source-root path/to/src \
  --output path/to/src/generated/sql
```

Check generated files without writing:

```sh
cargo run -- generate \
  --database path/to/app.db \
  --source-root path/to/src \
  --output path/to/src/generated/sql \
  --check
```

## Design Notes

The generator should emit boring concrete code. Runtime speed should come from staying close to hand-written `rusqlite`: cached prepared statements, positional row access, concrete row structs, and no dynamic mapper layer.

SQLx's SQLite analyzer is useful reference material for closing inference gaps. In particular:

- declared SQLite type mapping
- statement description
- result-column nullability
- fallback inference from `EXPLAIN`
- offline metadata shape

Marmot should borrow ideas carefully, and only copy code when there is a clear reason and the license notice is preserved. SQLx should not be part of generated runtime code.
