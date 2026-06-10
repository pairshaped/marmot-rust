# 0001. Colocated SQL Files Are the Source Interface

## Status

Accepted

## Context

Application code should keep SQL close to the workflow that owns it. The source layout is:

```text
src/items.rs
src/items/sql/get_item_by_id.sql
src/orders.rs
src/orders/sql/list_orders_by_account.sql
```

The `.sql` file is the reviewed source artifact. Generated code is mechanical output.

This matches the existing Marmot workflow in the Gleam application. That workflow has proven useful because the SQL files are easy to find, easy to review, and easy to move with the surrounding domain code.

The primary reason for this project is that ergonomics. Runtime speed matters, but the speedup over SQLx is a secondary benefit.

## Decision

Marmot will discover SQL files by walking source roots and finding directories named `sql`.

Applications may configure a specific SQL root when colocating under each domain directory is not the right shape:

```text
src/sql/get_settings.sql
src/sql/articles/get_articles.sql
src/sql/likes/get_likes.sql
```

When a SQL root is configured, Marmot recursively discovers `.sql` files under that root. Files directly under the configured root belong to the generated `sql` module. Files under child directories use the child directory name as the module stem:

```text
src/sql/get_settings.sql -> sql::get_settings
src/sql/articles/get_articles.sql -> articles_sql::get_articles
```

Each `*.sql` file defines one query. The filename becomes the generated function name.

The owning directory becomes the generated module name:

```text
src/items/sql/get_item_by_id.sql -> items_sql::get_item_by_id
```

Generated code belongs outside the hand-written domain module, initially under:

```text
src/generated/sql/
```

Hand-written application code maps generated rows into domain types. Generated rows are database boundary types, not the domain model.

## Consequences

SQL review stays local to the feature or page being changed.

The generator does not need a global query directory or a separate query naming system.

Moving a domain module should move its SQL files with it.

Generated modules are predictable, but they are still build output. Applications can decide whether to check them in.

SQL comments should not become benchmark-only code generation controls. Comment directives may be useful later for genuine cases the analyzer cannot infer, such as shared row naming, but the default path is inference from SQL and schema.
