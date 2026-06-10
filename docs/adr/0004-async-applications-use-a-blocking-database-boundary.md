# 0004. Async Applications Use a Blocking Database Boundary

## Status

Accepted

## Context

`rusqlite` is synchronous. The target Rust web application uses Axum and Tokio.

Calling synchronous SQLite code directly inside async handlers can block Tokio worker threads. That would trade one kind of database overhead for a runtime scheduling problem.

## Decision

Generated query functions accept a `rusqlite::Connection` and stay synchronous.

Async applications call generated functions through an application-level database wrapper. The wrapper is responsible for connection ownership and moving blocking work off async worker threads.

A typical call shape should look like:

```rust
state
    .db
    .run(|conn| items_sql::get_item_by_id(conn, &id, &org_id))
    .await
```

The wrapper may use a dedicated SQLite worker, a small blocking thread pool, or a pooled connection strategy. That choice belongs to the application runtime layer.

## Consequences

Generated code stays simple and fast.

The application has one place to reason about blocking behavior, pooling, transactions, busy timeouts, WAL mode, and foreign keys.

Transaction support needs to fit the same boundary. A transaction should run as one blocking unit rather than as many async calls that repeatedly cross the boundary.
