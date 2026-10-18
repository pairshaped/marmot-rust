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

A home module can use `crate::value_sets` for local-only declarations, or its crate's absolute Rust path, such as `curling_core::value_sets`, for sharing. Imported homes must use a crate-named path, since `crate::` would name the importing crate. Generated parameters and results use the home's exact path. Shared home modules must be public to importing crates, and generated enums are public. A local-only home module may remain crate-local. Importers emit no enum copy. An owner can import sets and declare its own sets in the same registry; only its local declarations are emitted in its shared output folder.

For Curling, `curling_core/src/value_sets.toml` declares shared sets with `module = "curling_core::value_sets"`. Feature crates import that registry with the relative path above. The Root owner's configured source root is `apps/curling/platform/src`, so its `src/value_sets.toml` imports `"../crates/curling_core/src/value_sets.toml"`; `marmot.toml` needs no new option. Each owner generates independently against the same schema. Its analysis, output and freshness receipts include imported registry contents, so a vocabulary change invalidates every importing owner's generated bindings.

Imported columns receive the same literal validation and strict unknown-value reads as local columns. Importing a registry authorizes no stored-value rewrite and does not relax the adoption boundary below. Normal managed-output reconciliation removes an importer's obsolete local enum output when its last local declaration is replaced by an import.

## Rust attributes

A set may opt into Rust attributes on its generated enum, so an existing public
contract can use the home enum without keeping a handwritten duplicate:

```toml
[sets.ConversationState]
values = ["idle", "running"]
columns = ["assistant_conversations.state"]
rust_attributes = [
  '#[derive(serde::Serialize, serde::Deserialize, ts_rs::TS)]',
  '#[serde(rename_all = "snake_case")]',
  '#[cfg_attr(feature = "client-contract-generation", derive(schemars::JsonSchema))]',
]
```

Each string must parse as exactly one outer Rust attribute; malformed syntax,
inner attributes and trailing tokens fail generation with the registry path.
Attributes are emitted in declaration order on the enum, alongside its standard
traits. Rust checks macro resolution, feature selection and attribute meaning
when the owner compiles. The owner supplies any dependencies these attributes
require. Marmot adds no serialization, TypeScript or schema dependency to its
default output. Sets without attributes retain byte-identical output.

Attributes do not change SQL validation or database mappings. Adopting an
existing wire contract must prove that its serialized values, TypeScript type
and JSON schema still agree with that contract. Shared callers re-export the
home enum instead of retaining a second enum or string mapping.

## Parameters, results and reads

Known variants implement `rusqlite::ToSql` and `FromSql`, with an `as_str` method for existing text boundaries. Nullable params and results use `Option<Enum>`. There is no string constructor that accepts arbitrary values. `FromSql` rejects unknown stored text with an error that retains the value and type name. Strict rejection is the fixed read policy for every declared column. Supporting lossless historical reads later requires separate read and write types so an unknown read cannot become an accepted write by accident.

Adding a declared value adds a Rust variant. Exhaustive matches must then be updated by the caller; existing raw string comparisons outside generated bindings aren't compiler checked. Adoption replaces those comparisons and any handwritten enum and `as_str` mapping in the same change. There must be one canonical vocabulary and mapping for adopted callers.

## SQL validation through column identity

Validation uses resolved table columns, including table aliases and joins. It applies to equality and ordering comparisons, `IN` lists, INSERT values and UPDATE assignments. A bound parameter inherits the declared type from its column; conflicting column evidence for the same parameter is an error. Literals compared with a typed parameter receive that same vocabulary, even when another occurrence binds the parameter to its column. Unrelated quoted text remains ordinary text.

Simple `CASE column WHEN literal` validates each alternative against the column's vocabulary. Searched CASE validates its conditions normally. A CASE assigned to a declared column validates each result arm, including ELSE, against that column's set. A CASE combining a declared column and string result arms validates those strings against the declared vocabulary and returns that type. NULL remains valid only where the existing nullability analysis allows it.

Outside the explicit JSON serialization boundary below, when a declared column participates in an expression whose value identity cannot be preserved, generation fails with an actionable value-set diagnostic naming the query and expression. This includes conflicting sets, unresolved column identity and transformations such as concatenation or arbitrary functions. The caller must split the expression into supported column comparisons or return the raw column and perform the transformation in Rust. A cast or result alias doesn't disable validation. Marmot must never silently fall back to String for an expression carrying a declared value set. Tests cover accepted and rejected aliases, joins, parameters, IN and CASE expressions, including ambiguity.

## JSON snapshots, compounds and INSERT SELECT

`json_object` and `json_array` may pack declared values into a text result.
Marmot validates each argument, including comparisons, typed parameters and CASE
literals, then treats the JSON result as SQLite text, preserving its existing generated result representation. `CAST` of that JSON packing to
TEXT has the same boundary. JSON packaging cannot supply an enum result or an
assignment to a declared column. A cast of a declared column, concatenation and
arbitrary functions remain unsupported; packaging them in JSON does not bypass
validation.

Compound SELECT arms are validated independently with their own relation
bindings. Parameters retain one compatible type across every arm. If a result
position carries an enum, every arm must return the same declared set at that
position. Conflicting sets and mixed enum/unproven text results fail generation.
Declared-column compounds require explicit result projections. Text JSON snapshots can combine different sets inside different arms without
claiming that the snapshot itself is an enum. These rules apply inside CTEs and
derived tables too.

For a single SELECT source in INSERT SELECT, each projection assigned to a
declared column must be a valid literal, a named parameter bound to that set,
or a column of the same set. Marmot validates the source predicates and joins
in their own scope. An untyped text column, transformed value, CASE result or
scalar subquery is not proof for an enum assignment. Compound INSERT SELECT
sources assigning declared targets remain unsupported. Ordinary INSERT VALUES
keeps its existing CASE and nullability rules. These additions preserve SQL and
runtime behavior; they provide no general expression exemption or scan waiver.

## Import and adoption boundary

Generated types constrain generated bindings. They don't constrain raw SQL writers, external importers or other owners without the declaration. Each adoption traces all its writers before choosing the vocabulary.

Before cutover, the pilot column holds no V2-imported values, including defaults on imported rows. After cutover, adopting a V2-origin column requires reviewing its importer, target inserts, reconciliation counts and cutover reports in the same change. Historical values must be accounted for explicitly. Mapping V2 spellings is allowed only when the import design establishes equivalent meaning; the declaration alone authorizes no rewrite. A strict declaration cannot be adopted while required historical values would fail reads.

Scalar subqueries that return an ordinary value may filter on declared columns.
Their predicates receive the same literal and parameter validation as a top-level
SELECT, with local aliases shadowing outer aliases. They require explicit result
projections. A scalar subquery cannot return a declared enum or prove an enum
assignment; those uses remain rejected.
