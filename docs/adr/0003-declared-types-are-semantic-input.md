# 0003. Treat Declared Types as Semantic Input

## Status

Accepted

## Context

SQLite stores values using a small set of storage classes and ordinarily treats column types as affinities rather than rigid constraints. Declared types still carry application meaning that storage classes cannot express. A `BOOLEAN` column and an `INTEGER` column share integer storage, but they do not describe the same domain type to generated code.

SQLite `STRICT` tables accept only `INT`, `INTEGER`, `REAL`, `TEXT`, `BLOB`, and `ANY`. They reject declarations such as `BOOLEAN`. Replacing a Boolean declaration with `INTEGER` makes the table eligible for `STRICT`, but removes the schema fact Marmot uses to generate Boolean parameters and results.

Values, column names, defaults, and unnamed constraints are not reliable substitutes for an explicit declared type. An integer limited to zero and one may be a Boolean, a numeric flag, an index, or a two-value domain type. SQLite preserves named constraints in the table's `CREATE TABLE` SQL, so a named constraint can state the missing semantic fact explicitly.

## Decision

Marmot treats declared column types and recognized named constraints as semantic analyzer input. `BOOLEAN` and `BOOL` map to the language-neutral Boolean value type.

An `INT` or `INTEGER` column also maps to Boolean when it has this column-level constraint:

```sql
value INTEGER CONSTRAINT boolean CHECK (value IN (0, 1))
```

The constraint name is the semantic declaration. The check is the storage invariant required by that declaration. This convention applies to both strict and non-strict tables.

Marmot requires the canonical `CHECK (column IN (0, 1))` expression. It rejects a named `boolean` constraint attached to another constraint type, declared on another storage type, placed at table level, or using another check expression.

Unnamed 0/1 constraints remain integers. Marmot does not infer Boolean semantics from observed values, naming conventions, defaults, or constraint expressions alone.

## Consequences

Generated types follow facts stated explicitly by the schema rather than heuristics.

Schemas can use generated Boolean types in SQLite `STRICT` tables without pretending that every 0/1 integer is Boolean.

Omitting or misspelling the named constraint produces an integer generated type. Existing Boolean call sites therefore fail at compile time instead of silently accepting the schema change.
