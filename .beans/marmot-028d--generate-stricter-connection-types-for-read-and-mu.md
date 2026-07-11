---
# marmot-028d
title: Generate stricter connection types for read and mutation SQL
status: completed
type: task
priority: normal
created_at: 2026-06-30T15:46:35Z
updated_at: 2026-07-11T23:53:15Z
---

Marmot currently generates SQL functions that accept `&rusqlite::Connection` regardless of statement kind. That lets mutation functions be called anywhere a read-only connection is available, so applications have to enforce read/write separation entirely at their DB adapter layer.

Investigate whether Marmot can classify statement blocks by operation kind and generate stricter connection parameters:

- [x] Determine how reliably Marmot can classify `SELECT`/read statements versus `INSERT`, `UPDATE`, `DELETE`, DDL, and multi-statement blocks.
- [x] Decide the generated Rust signatures for read and mutation statements, probably `&Connection` for reads and `&mut Connection` or `&Transaction` for mutations.
- [x] Consider how this interacts with apps that intentionally call mutation statements inside a transaction.
- [x] Add tests proving read statements remain callable with immutable connections and mutation statements are rejected by the type system unless the caller has the stronger connection type.
- [x] Document the migration impact for existing generated query call sites.

Context: the Curling app uses separate SQLite read and write worker connections. It can set `PRAGMA query_only = ON` on read workers at runtime, but Marmot can still improve compile-time safety by making generated mutation functions require a stronger connection interface.

## Result

Marmot now uses SQLite prepared-statement read-only metadata for classification. Reads accept `&Connection`. Mutations and DDL accept the generated sealed `MutationConnection`, implemented for `&mut Connection` and `&Transaction`. Runtime tests cover both supported mutation paths, and a compile-fail check proves `&Connection` is rejected. Migration guidance is in the README.
