---
# marmot-8xeb
title: Improve temporal inference for standalone SQL parameters
status: todo
type: task
priority: normal
tags:
    - temporal
    - codegen
created_at: 2026-06-26T02:25:10Z
updated_at: 2026-06-26T02:25:10Z
---

Curling's temporal migration exposed a narrow inference gap in Marmot.

## Problem

Temporal result columns and direct column-bound params work well, but standalone parameters do not always get the intended temporal type when the parameter is only shaped by a suffix, a cast, or a SQLite datetime wrapper.

Examples seen while integrating Curling:

- `datetime(@season_start, 'unixepoch')` kept `@season_start` as a string in some queries, even though the caller had Unix seconds.
- Parameters named like `@now_at` or aliases like `cast(@now_at as text) as now_at` did not reliably infer `DbDateTime` unless the parameter was compared directly to a temporal column.
- Mixed date/datetime report filters needed explicit split params (`from_at`, `from_on`) because a single placeholder was compared to both `_at` and `_on` columns. That one should stay rejected or require explicit typing, but the error path should be clear.

## Desired Design

Marmot should have a deliberate rule for temporal parameter inference:

- A parameter compared directly to a configured temporal column uses that column's temporal type.
- A parameter name ending in a configured temporal suffix can infer the matching temporal type when no stronger schema context exists.
- Casts or SQLite wrappers should not accidentally erase a known temporal type.
- Ambiguous params used as both date and datetime should produce a helpful diagnostic instead of falling back to an arbitrary scalar.

## Acceptance Criteria

- Add fixture SQL covering standalone `@created_at` / `@starts_on` style params.
- Add fixture SQL covering temporal params through CTE aliases and casts.
- Add a test that a param used against both `_at` and `_on` columns reports an explicit conflict.
- Update generated Rust signatures to match the selected temporal type.
