# 0010. Produce scoped IDs through generated gate execution

A scoped ID records that a declared gate query returned a particular database ID. Child queries can require that proof instead of a raw URL ID. The type has one owner, even when gates and children belong to different generator owners. This is opt-in hardening of existing SQL authorization.

## Query declarations

A gate names its returned ID column and the shared Rust type:

```sql
-- func: get_order_confirmation
-- produces: id = curling_core::scoped_ids::ScopedOrderId
SELECT o.id, o.status FROM orders o
WHERE o.org_id = @org_id AND o.user_id = @user_id AND o.id = @order_id;
```

A child names the parameter that requires the proof:

```sql
-- func: list_order_join_links
-- consumes: order_id = curling_core::scoped_ids::ScopedOrderId
SELECT product_id FROM line_items WHERE order_id = @order_id;
```

Other parameters keep their ordinary types. A child may consume several scoped parameters by repeating `-- consumes:`. Duplicate parameter declarations, missing parameters, incompatible underlying types and malformed paths fail generation. A query produces at most one nonnullable INTEGER ID. Production requires a read-only gate; mutation, batch and unsupported cardinality combinations fail explicitly.

A deliberate cross-tenant query can record `-- unscoped: <reason>`. A missing or whitespace-only reason fails generation, and the directive cannot coexist with scope production or consumption. It does not exempt a query from server authorization or change SQL. Undeclared queries retain their existing raw types and byte-identical generated output. There is no tenant-query ratchet.

## One type owner and the cross-owner boundary

The consuming domain owns the shared type. For the pilot, `curling_core` owns `curling_core::scoped_ids::ScopedOrderId`; neither gate crate becomes a dependency of core. Its `source_root/scoped_ids.toml` declares the type and names each authorized producer using its existing companion path and function name:

```toml
module = "curling_core::scoped_ids"

[ids.ScopedOrderId]
gates = [
  "../../curling_public_cart/src/orders/id_/index.sql#get_order_confirmation",
  "../../curling_public_account_billing/src/account/orders/id_/index.sql#get_account_order_detail",
]
```

Each gate and consumer owner imports the home registry with its own source-root `scoped_ids.toml`:

```toml
imports = ["../../curling_core/src/scoped_ids.toml"]
```

Imports name home registries directly, without another alias family or transitive import graph. A home registry defines its module and IDs; imported registries cannot themselves import registries. Scope directives use the home registry's exact Rust path. Generation rejects an unknown type or a producer not registered at its current companion path and function name.

Paths resolve relative to the declaration's source root. The referenced `-- produces:` must name that exact type, and the ID result must exist and be a nonnullable INTEGER. Every registered gate has a distinct existing function name within its scoped type. Registering more than one gate for the same type is intentional: both gates must establish the authorization that the consuming queries require.

Marmot writes `scoped_ids.rs` in the type owner's configured shared output folder. The owner declares the module at the configured Rust path. The file contains the private ID representation and the generated execution functions for its registered gates. The companion SQL remains the only hand-authored query source. Other owners import the type using the absolute path in their directives; they don't emit copies of it.

The gate owner's generated binding keeps its parameter and result API and delegates execution to the shared type's generated function with the same query name. That function prepares and executes the canonical gate SQL, constructs the scoped ID from the returned ID column, and passes the row and proof to the binding's row mapper. A public helper is safe only because it executes the registered gate itself. There is no helper accepting a raw ID, an arbitrary SQL string or an arbitrary closure that manufactures an ID.

This delegation is the cross-crate visibility boundary. Rust cannot expose a constructor to a different crate's generated source while hiding it from that crate's handwritten source. A public raw constructor, an unsealed external trait, or a public `FromSql` implementation would let callers manufacture the proof without running the gate. None is generated. The representation and constructor stay private inside the shared generated module. Every public production path executes a declared gate. Compile-fail tests must cover struct construction, raw-ID conversion and passing an `i64` to a consumer. A cross-owner fixture proves two independently generated gate owners produce the same type consumed by a third owner.

## Connection, transaction and async lifetime

`ScopedOrderId<'a>` wraps the V3 target `orders.id` and borrows the connection that ran the gate. It carries no org ID: the SQL gate already proves org, user, order and status authorization. Child bindings check that the token belongs to the connection they receive before binding its raw ID. A token from another connection fails at that boundary.

The borrowed connection makes the proof live only inside the synchronous database operation described by [ADR 0005](0005-async-applications-use-a-blocking-database-boundary.md). It cannot escape a `Db::read` or `write_tx` closure as an owned result or cross an async task boundary. A transaction dereferences to its connection, so gates and children work within the same transaction without a second gate or query. The proof records a successful gate read; it doesn't promise that authorization cannot change later. Application code must keep the gated workflow in that same database operation, as the pilot already does.

The type may expose its raw value for existing separately scoped reads, response IDs and rendering. This is a read-only escape from the proof, with no conversion back. Generated consumer params accept the scoped type by value, including alongside ordinary params such as `user_id`. Gate rows containing the scoped ID carry the connection lifetime. No new database value, identifier mapping or importer behavior is introduced.

## Generation and removal

Each owner's generation inputs include its registry, imported home registries and the referenced gate SQL. Editing either invalidates generation receipts. Analysis, emission and freshness checks validate the registry and referenced declarations; a missing or mismatched producer fails before publication. Each generator owner can run independently against the shared database schema. The type owner generates the execution helpers; gate owners generate only delegates and row mapping; consumers generate typed params and connection checks.

Normal managed-output reconciliation removes the shared module when its declaration disappears. Its owner removes the Rust module declaration in the same adoption/removal change. [ADR 0001](0001-colocated-sql-files.md) lists this shared artifact. Owners with no scoped directives or registry retain their existing output, including the frozen no-directive fixture.

## Pilot boundary

The public order-confirmation gate and account order-detail gate produce the same type. Their org, user, order and status predicates remain unchanged. The two confirmation-only child queries and the two shared join-link/fee-continuation queries require it. The account page's own line-item query remains raw and outside this adoption.

`curling_core::domain::order_receipts::read` remains unchanged. Its SQL is separately scoped by org and order; the gate's early return still supplies the user check, as it does today. Widening this shared financial read model isn't required for the pilot.

The token wraps the V3 target ID. V2 imports retain `orders.source_v2_id` and assign the target `orders.id`; no stored value, importer, mapping or migration changes. Adoption must confirm that the importer calls none of the changed generated functions. Authorized responses remain equivalent, and same-org different-user and different-org requests must return 404 without order or child data.
