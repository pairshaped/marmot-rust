# 0009. Declare column value sets

Marmot accepts explicit value sets for TEXT columns, extending [ADR 0003](0003-declared-types-are-semantic-input.md). A declaration binds a finite vocabulary to exact table columns. Generated parameters and results use that vocabulary's Rust enum, and generation rejects SQL literals outside it. SQLite storage and migrations stay unchanged.

## Declaration and ownership

An owner may place `value_sets.toml` directly in its configured `source_root`:

```toml
module = "crate::value_sets"

[sets.NoticeStatus]
values = ["draft", "published"]
columns = ["notices.status"]
```

The file is schema metadata scoped to that generator owner. It is separate from companion query SQL because several companions can use the same column. Keeping it outside create migrations avoids turning a vocabulary change into a schema migration. A missing file means no declarations and byte-identical existing output.

Marmot emits `value_sets.rs` under the configured shared output folder. The owner declares that module at the configured absolute Rust path. It is a shared generated artifact alongside temporal types and view installation SQL. Normal managed-output reconciliation removes it when the declaration disappears; the owner removes its module declaration in the same change. The set name is the Rust enum name; values become PascalCase variants. Invalid Rust names, duplicate values, variant collisions, duplicate column bindings, unknown columns, and non-TEXT columns fail generation. Multiple columns share a type only by appearing in the same set's `columns` list. Columns with the same name or vocabulary don't implicitly share a type. Declaration and SQL identifiers follow SQLite's case-insensitive identity rules.

One home owner declares each generated enum and its database mappings. Declarations never inspect stored rows to guess a vocabulary.

## Cross-owner imports

Other owners import home registries directly from their own `source_root/value_sets.toml`, following [ADR 0010](0010-scoped-tenant-ids.md):

```toml
imports = ["../../curling_core/src/value_sets.toml"]
```

Paths resolve relative to the importing source root. An imported home registry must declare sets and must not itself import another registry. Imports expose every set and column binding in that registry; they are not transitive. Duplicate set names or column bindings across local and imported declarations fail generation, as do missing registries. A query cannot refer to an undeclared set. Diagnostics identify the registry or companion SQL responsible.

A home module can use `crate::value_sets` for local-only declarations, or its crate's absolute Rust path, such as `curling_core::value_sets`, for sharing. Imported homes must use a crate-named path, since `crate::` would name the importing crate. Generated parameters and results use the home's exact path. Importers emit no enum copy. An owner can import sets and declare its own sets in the same registry; only its local declarations are emitted in its shared output folder.

For Curling, `curling_core/src/value_sets.toml` declares shared sets with `module = "curling_core::value_sets"`. Feature crates import that registry with the relative path above. The Root owner's configured source root is `apps/curling/platform/src`, so its `src/value_sets.toml` imports `"../crates/curling_core/src/value_sets.toml"`; `marmot.toml` needs no new option. Each owner generates independently against the same schema. Its analysis, output and freshness receipts include imported registry contents, so a vocabulary change invalidates every importing owner's generated bindings.

Imported columns receive the same literal validation and strict unknown-value reads as local columns. Importing a registry authorizes no stored-value rewrite and does not relax the adoption boundary below. Normal managed-output reconciliation removes an importer's obsolete local enum output when its last local declaration is replaced by an import.

## Parameters, results and reads

Known variants implement `rusqlite::ToSql` and `FromSql`, with an `as_str` method for existing text boundaries. Nullable params and results use `Option<Enum>`. There is no string constructor that accepts arbitrary values. `FromSql` rejects unknown stored text with an error that retains the value and type name. Strict rejection is the fixed read policy for every declared column. Supporting lossless historical reads later requires separate read and write types so an unknown read cannot become an accepted write by accident.

Adding a declared value adds a Rust variant. Exhaustive matches must then be updated by the caller; existing raw string comparisons outside generated bindings aren't compiler checked. Adoption replaces those comparisons and any handwritten enum and `as_str` mapping in the same change. There must be one canonical vocabulary and mapping for adopted callers.

## SQL validation through column identity

Validation uses resolved table columns, including table aliases and joins. It applies to equality and ordering comparisons, `IN` lists, INSERT values and UPDATE assignments. A bound parameter inherits the declared type from its column; conflicting column evidence for the same parameter is an error. Literals compared with a typed parameter receive that same vocabulary, even when another occurrence binds the parameter to its column. Unrelated quoted text remains ordinary text.

Simple `CASE column WHEN literal` validates each alternative against the column's vocabulary. Searched CASE validates its conditions normally. A CASE assigned to a declared column validates each result arm, including ELSE, against that column's set. A CASE combining a declared column and string result arms validates those strings against the declared vocabulary and returns that type. NULL remains valid only where the existing nullability analysis allows it.

When a declared column participates in an expression whose value identity cannot be preserved, generation fails with an actionable value-set diagnostic naming the query and expression. This includes conflicting sets, unresolved column identity and transformations such as concatenation or arbitrary functions. The caller must split the expression into supported column comparisons or return the raw column and perform the transformation in Rust. A cast or result alias doesn't disable validation. Marmot must never silently fall back to String for an expression carrying a declared value set. Tests cover accepted and rejected aliases, joins, parameters, IN and CASE expressions, including ambiguity.

## Import and adoption boundary

Generated types constrain generated bindings. They don't constrain raw SQL writers, external importers or other owners without the declaration. Each adoption traces all its writers before choosing the vocabulary.

Before cutover, the pilot column holds no V2-imported values, including defaults on imported rows. After cutover, adopting a V2-origin column requires reviewing its importer, target inserts, reconciliation counts and cutover reports in the same change. Historical values must be accounted for explicitly. Mapping V2 spellings is allowed only when the import design establishes equivalent meaning; the declaration alone authorizes no rewrite. A strict declaration cannot be adopted while required historical values would fail reads.
