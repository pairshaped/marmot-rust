# 0001. Colocated SQL Files Are the Source Interface

## Status

Accepted

## Context

Application code should keep SQL close to the workflow that owns it. The source layout is:

```text
src/items.rs
src/items.sql
src/registrations/index.rs
src/registrations/index.sql
src/orders.rs
src/orders.sql
```

The `.sql` file is the reviewed source artifact. Generated code is mechanical output.

The primary reason for this project is that ergonomics. Runtime speed matters, but the speedup over SQLx is a secondary benefit.

## Decision

Marmot will discover module companion SQL files by walking source roots. A
companion SQL file has the same stem as a Rust module:

```text
src/items.rs
src/items.sql
src/registrations/index.rs
src/registrations/index.sql
```

Each companion SQL file contains one or more `-- func:` blocks. The block name
becomes the generated Rust function name:

```sql
-- func: get_item_by_id
select id, name from items where id = @id;

-- func: list_items
select id, name from items order by name;
```

Marmot writes each companion file's bindings beside it, as `NAME.generated.rs`
with an `@generated` header:

```text
src/items.sql -> src/items.generated.rs
src/registrations/index.sql -> src/registrations/index.generated.rs
src/registrations/form.sql -> src/registrations/form.generated.rs
```

The module that owns the SQL declares its bindings with
`#[path = "items.generated.rs"] mod sql;`. The declaration is private by
default and widens to `pub(crate)` only where a caller outside the module needs
it. Marmot's check requires exactly one declaration per discovered query,
pointing at an existing file, because the compiler never loads an undeclared
generated file. There is no central query tree, no generated `mod.rs` tree and
no per-query output override.

`mod.sql` is rejected. SQL files under directories named `sql` are rejected.
Query names come from `-- func:` blocks, not filenames.

Generated bindings sit beside the SQL that owns them, so a feature owns its SQL
and its bindings in one place. Shared generated files are the configured temporal module (with a fixed
`mod.rs` declaring it), the value-set module defined by
[ADR 0009](0009-declared-column-value-sets.md), and the aggregate `views.sql`,
under the configured shared output folder, for example:

```text
src/generated/sql/
```

Hand-written application code maps generated rows into domain types. Generated rows are database boundary types, not the domain model.

## Consequences

SQL review stays local to the feature or page being changed.

The generator does not need a global query directory or per-query SQL files.
`-- func:` names partition multi-query companion files without requiring shared
row names.

Moving a domain module should move its SQL files with it.

Generated modules are predictable, but they are still build output. Applications can decide whether to check them in.

SQL comments should not become benchmark-only code generation controls.
`-- func:` is the source boundary between statements. Other directives should
be added only for genuine cases the analyzer cannot infer from SQL and schema.
