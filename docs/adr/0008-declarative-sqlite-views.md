# 0008. Treat SQLite Views as Declarative Database Code

## Status

Accepted

## Context

SQLite stores a normal view as the SQL text of a named `SELECT`. The view has no rows, materialized result, independent index, or persistent query plan. A consuming statement expands and plans the view when that statement is prepared.

Forward-only migrations can create views, but migration history is a poor source interface for finding and reviewing the current definition. Views are also used as reusable relations that compose into queries owned by multiple application workflows.

SQLite permanent views have no page or Rust-module scope. Tables and views share the `main` schema relation namespace. Source paths do not qualify installed object names.

Triggers are also stored as schema SQL, but they are not equivalent to read-only views. Trigger bodies introduce implicit writes without a Marmot-generated typed function boundary or an explicit Rust call site. Their validation and deployment risks are different.

## Decision

Marmot treats permanent, read-only, parameter-free SQLite views as declarative database code owned by the current application revision.

Each view has one hand-authored source file under the database target's source root:

```text
src/db_views/
  view_active_purchase_line_items.sql
  view_capacity_holding_line_items.sql
  view_membership_qualifying_line_items.sql
```

Each file contains one native `CREATE VIEW` statement. Physical view names use
lowercase `view_` prefixes. The filename stem must equal the physical view name.
The statement carries the explicit, stable output-column contract:

```sql
CREATE VIEW view_active_purchase_line_items (line_item_id, participant_id) AS
SELECT id, participant_id
FROM line_items
WHERE status IN ('submitted', 'paid');
```

The configured directory and matching filename already classify the file as a
reusable declarative view. Marmot annotations are reserved for cases where
ordinary SQL and source conventions cannot express the required ownership or
reuse semantics. Parameters are not supported.

Marmot generates the aggregate installation SQL under its configured generated output:

```text
src/generated/sql/views.sql
```

Generated output is disposable. The files under `src/db_views` are the canonical definitions.

Marmot reconciles declared views after ordinary forward-only migrations. Reconciliation runs in a transaction and implements replacement with `DROP VIEW IF EXISTS` followed by `CREATE VIEW`, because SQLite has no `CREATE OR REPLACE VIEW` statement. Parent-first creation order is not required.

Reconciliation unconditionally replaces the complete declared set. Before it commits, Marmot prepares a zero-row query against every declared view. This forces SQLite to resolve the final dependency and output-column graph and catches missing relations, circular definitions, and incompatible dependent columns. Marmot may build a dependency graph for better diagnostics, but DDL ordering does not depend on one.

Marmot makes declared views available to its analysis connection before analyzing executable queries that consume them. View support must not silently bypass Marmot's existing parameter, result-column, type, or nullability checks. Cases SQLite cannot describe require analyzer inference, an explicit override, or a clear diagnostic.

Removing a source declaration does not authorize Marmot to drop the installed view. Removal requires an explicit forward migration containing `DROP VIEW IF EXISTS`. Reconciliation never turns source absence into a destructive action.

Marmot audits permanent `main.sqlite_schema` views whose case-sensitive names match `view_*`:

- an installed `view_*` without a source declaration produces an actionable warning;
- strict CI and deployment checks treat that warning as a failure;
- a declaration that was not installed is an error;
- a declaration or dependent view that cannot be prepared is an error.

The `audit-views` command performs this audit. `--deny-warnings` makes database-only views fail the command. `generate --check`, `migrate --deny-view-warnings`, and `reset --deny-view-warnings` provide strict build and deployment integration.

The database-only diagnostic shows copyable, safely quoted `DROP VIEW IF EXISTS` migration SQL. It also explains that restoring the source declaration is the correct action when the view is still intentional. Marmot never executes the suggested removal automatically.

Reconciliation belongs in controlled migration, reset, build verification, and deployment workflows. It does not run independently from every application process at startup. Deployments that overlap application revisions use expand-and-contract changes for shared view output contracts.

SQLite preserves declared types through direct view columns and explicit casts, but does not preserve useful `NOT NULL` metadata and may expose no declared type for an uncast expression. Marmot treats view columns as nullable. An untyped expression uses the dynamic SQLite value type unless the view definition supplies a `CAST`.

Marmot does not generalize `src/db_views` into a database-program source hierarchy as part of this decision. Trigger support is separate work if it is ever justified.

Application write behavior follows these defaults:

- use foreign-key actions for simple referential cascades modeled directly by SQLite;
- use typed Rust transactions and Marmot-generated query functions for application cascades, derived records, and domain workflows;
- consider triggers only for invariants that must apply to every database writer and cannot be expressed with ordinary constraints.

Indexes, tables, constraints, generated columns, and virtual tables remain migration-owned because they carry persistent data or expensive physical state. SQL functions and collations remain part of connection setup because SQLite does not persist them as schema definitions.

## Consequences

Current view definitions are first-class source instead of facts recovered from migration history.

Views can provide stable, reusable names for domain relations without requiring Marmot to become an Arel-style runtime query builder.

Changing a view definition does not require a migration. Removing a view remains explicit and reviewable.

The `view_` prefix provides a narrow audit and ownership boundary without a registry table. SQLite-reserved objects, temporary views, attached-database views, and unrelated unprefixed objects remain outside that boundary.

Generation and deployment gain a view reconciliation and validation phase. A schema change may cause existing prepared statements to recompile, but it does not discard base-table indexes, `ANALYZE` statistics, or materialized view data.

Base-table migrations may need to drop an existing view inside their own transaction when SQLite refuses a table or column change that the view references. Marmot recreates the current declaration after migrations finish.

Database cascades remain visible through constraints or typed Rust workflows. Marmot does not trade compiler-visible application behavior for implicit trigger execution merely to share a source directory.
