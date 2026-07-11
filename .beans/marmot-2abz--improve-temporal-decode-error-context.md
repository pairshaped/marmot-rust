---
# marmot-2abz
title: Improve temporal decode error context
status: completed
type: task
priority: normal
tags:
    - temporal
    - diagnostics
created_at: 2026-06-26T04:40:28Z
updated_at: 2026-07-11T23:53:15Z
---

Generated temporal boundary types fail loudly when SQLite returns malformed stored values, which is good, but the current rusqlite conversion error only reports the column index.

Current shape seen during Curling migration:

```text
Conversion error from type Text at index: 5, invalid temporal value "1700000000"; expected YYYY-MM-DD HH:MM:SS
```

That is technically readable, but it forces the caller to map index 5 back to the selected field. For generated code, Marmot knows the row field name and should be able to provide better context.

## Desired Error Shape

Prefer an error that names the result field when row decoding fails, for example:

```text
invalid temporal value for created_at: "1700000000"; expected YYYY-MM-DD HH:MM:SS
```

or, if practical:

```text
invalid temporal value for orders.created_at: "1700000000"; expected YYYY-MM-DD HH:MM:SS
```

## Acceptance Criteria

- Add a generated-code runtime test that stores malformed text in a temporal column and asserts the returned error includes the generated field name.
- Preserve the bad value and expected format in the error.
- Keep schema/type mismatch errors unchanged. Those already name table and column.
- Do not hide the original rusqlite error source if wrapping it.

## Result

Generated temporal row decoding now adds the generated field name while preserving the bad value, expected format, and original rusqlite conversion error in the source chain. Generated-code runtime coverage asserts all of those properties.
