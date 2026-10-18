use crate::model::{ScalarCardinality, sanitize_identifier};
use crate::sql_text::strip_comments;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlBlock {
    pub function_name: String,
    pub emit_batch: bool,
    pub scalar_cardinality: Option<ScalarCardinality>,
    pub sql: String,
    pub column_substitution: Option<ColumnSubstitution>,
    pub scope: ScopeDirectives,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopeDirectives {
    pub produces: Option<ScopeBinding>,
    pub consumes: Vec<ScopeBinding>,
    pub unscoped: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeBinding {
    pub name: String,
    pub rust_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSubstitution {
    pub choices: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlBlockError {
    InvalidScope { reason: String },
    MissingFunc,
    SqlBeforeFirstFunc,
    EmptyFuncName,
    InvalidFuncName { name: String },
    EmptyBlock { name: String },
    DuplicateFunc { name: String },
    ColumnBeforeFunc,
    InvalidBatch { reason: String },
    InvalidCardinality { reason: String },
    InvalidColumn { reason: String },
}

impl std::fmt::Display for SqlBlockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidScope { reason } => write!(f, "invalid scoped-id directive: {reason}"),
            Self::InvalidCardinality { reason } => {
                write!(f, "invalid `-- cardinality:` directive: {reason}")
            }
            Self::InvalidBatch { reason } => write!(f, "invalid `-- batch` directive: {reason}"),
            Self::MissingFunc => f.write_str("SQL file must contain at least one `-- func:` block"),
            Self::SqlBeforeFirstFunc => {
                f.write_str("SQL appears before the first `-- func:` block")
            }
            Self::EmptyFuncName => f.write_str("`-- func:` name is empty"),
            Self::InvalidFuncName { name } => {
                write!(f, "`-- func: {name}` is not a valid Rust function name")
            }
            Self::EmptyBlock { name } => write!(f, "`-- func: {name}` has no SQL statement"),
            Self::DuplicateFunc { name } => write!(f, "duplicate `-- func: {name}` block"),
            Self::ColumnBeforeFunc => {
                f.write_str("`-- columns:` must appear inside a `-- func:` block")
            }
            Self::InvalidColumn { reason } => {
                write!(f, "invalid `-- columns:` directive: {reason}")
            }
        }
    }
}

pub fn parse_sql_blocks(sql: &str) -> std::result::Result<Vec<SqlBlock>, SqlBlockError> {
    let mut blocks = Vec::new();
    let mut current_name: Option<String> = None;
    let mut current_sql = String::new();
    let mut current_column = None;
    let mut current_batch = false;
    let mut current_cardinality = None;
    let mut current_scope = ScopeDirectives::default();

    for line in sql.lines() {
        if let Some(name) = func_name_from_line(line)? {
            finish_block(
                &mut blocks,
                current_name.take(),
                &mut current_sql,
                current_column.take(),
                std::mem::take(&mut current_batch),
                current_cardinality.take(),
                std::mem::take(&mut current_scope),
            )?;
            current_name = Some(name);
            continue;
        }

        let comment = line.trim().strip_prefix("--").map(str::trim);
        if let Some(body) = comment
            && parse_scope_directive(body, current_name.is_some(), &mut current_scope)?
        {
            continue;
        }
        if comment == Some("batch") {
            if current_name.is_none() || current_batch {
                return Err(SqlBlockError::InvalidBatch {
                    reason: "must appear once inside a `-- func:` block".into(),
                });
            }
            current_batch = true;
            continue;
        }
        if comment.is_some_and(|body| {
            body.strip_prefix("batch").is_some_and(|value| {
                value
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_whitespace() || matches!(ch, ':' | '='))
            })
        }) {
            return Err(SqlBlockError::InvalidBatch {
                reason: "use `-- batch` without a value".into(),
            });
        }

        if let Some(value) = comment.and_then(|body| body.strip_prefix("cardinality:")) {
            if current_name.is_none() || current_cardinality.is_some() {
                return Err(SqlBlockError::InvalidCardinality {
                    reason: "must appear once inside a `-- func:` block".into(),
                });
            }
            current_cardinality = Some(match value.trim() {
                "many" => ScalarCardinality::Many,
                "one" => ScalarCardinality::One,
                "both" => ScalarCardinality::Both,
                _ => {
                    return Err(SqlBlockError::InvalidCardinality {
                        reason: "expected many, one or both".into(),
                    });
                }
            });
            continue;
        }
        if comment.is_some_and(|body| {
            body.strip_prefix("cardinality").is_some_and(|value| {
                value.is_empty()
                    || value
                        .chars()
                        .next()
                        .is_some_and(|ch| ch.is_whitespace() || ch == '=')
            })
        }) {
            return Err(SqlBlockError::InvalidCardinality {
                reason: "use `-- cardinality: many`, `one` or `both`".into(),
            });
        }

        if let Some(column) = column_from_line(line)? {
            if current_name.is_none() {
                return Err(SqlBlockError::ColumnBeforeFunc);
            }
            if let Some(current) = &mut current_column {
                current.choices.extend(column.choices);
            } else {
                current_column = Some(column);
            }
            continue;
        }

        if current_name.is_none() {
            if strip_comments(line).trim().is_empty() {
                continue;
            }
            return Err(SqlBlockError::SqlBeforeFirstFunc);
        }

        current_sql.push_str(line);
        current_sql.push('\n');
    }

    finish_block(
        &mut blocks,
        current_name,
        &mut current_sql,
        current_column,
        current_batch,
        current_cardinality,
        current_scope,
    )?;

    if blocks.is_empty() {
        return Err(SqlBlockError::MissingFunc);
    }

    let mut names = std::collections::BTreeSet::new();
    for block in &blocks {
        if !names.insert(block.function_name.clone()) {
            return Err(SqlBlockError::DuplicateFunc {
                name: block.function_name.clone(),
            });
        }
    }

    Ok(blocks)
}

fn func_name_from_line(line: &str) -> std::result::Result<Option<String>, SqlBlockError> {
    let trimmed = line.trim();
    if !trimmed.starts_with("--") {
        return Ok(None);
    }

    let body = trimmed.trim_start_matches("--").trim();
    let Some(name) = body.strip_prefix("func:") else {
        return Ok(None);
    };
    let name = name.trim();
    if name.is_empty() {
        return Err(SqlBlockError::EmptyFuncName);
    }
    if sanitize_identifier(name) != name {
        return Err(SqlBlockError::InvalidFuncName {
            name: name.to_string(),
        });
    }

    Ok(Some(name.to_string()))
}

fn column_from_line(line: &str) -> std::result::Result<Option<ColumnSubstitution>, SqlBlockError> {
    let trimmed = line.trim();
    if !trimmed.starts_with("--") {
        return Ok(None);
    }

    let body = trimmed.trim_start_matches("--").trim();
    let Some(directive) = body.strip_prefix("columns:") else {
        return Ok(None);
    };
    let choices = directive
        .split(',')
        .map(str::trim)
        .map(str::to_string)
        .collect::<Vec<_>>();
    if choices
        .iter()
        .any(|choice| choice.is_empty() || sanitize_identifier(choice) != *choice)
    {
        return Err(SqlBlockError::InvalidColumn {
            reason: "allowed columns must be lowercase Rust-compatible SQL identifiers".to_string(),
        });
    }
    let distinct = choices.iter().collect::<std::collections::BTreeSet<_>>();
    if distinct.len() != choices.len() {
        return Err(SqlBlockError::InvalidColumn {
            reason: "allowed columns must be unique".to_string(),
        });
    }

    Ok(Some(ColumnSubstitution { choices }))
}

fn finish_block(
    blocks: &mut Vec<SqlBlock>,
    name: Option<String>,
    sql: &mut String,
    column_substitution: Option<ColumnSubstitution>,
    emit_batch: bool,
    scalar_cardinality: Option<ScalarCardinality>,
    scope: ScopeDirectives,
) -> std::result::Result<(), SqlBlockError> {
    let Some(name) = name else {
        return Ok(());
    };
    let trimmed = sql.trim();
    if strip_comments(trimmed).trim().is_empty() {
        return Err(SqlBlockError::EmptyBlock { name });
    }
    if let Some(column) = &column_substitution {
        if column.choices.len() < 2 {
            return Err(SqlBlockError::InvalidColumn {
                reason: "at least two allowed columns are required".to_string(),
            });
        }
        let distinct = column
            .choices
            .iter()
            .collect::<std::collections::BTreeSet<_>>();
        if distinct.len() != column.choices.len() {
            return Err(SqlBlockError::InvalidColumn {
                reason: "allowed columns must be unique".to_string(),
            });
        }
        let marker = "{{column}}";
        let marker_count = trimmed.matches(&marker).count();
        if marker_count != 1 {
            return Err(SqlBlockError::InvalidColumn {
                reason: format!(
                    "`-- columns:` requires exactly one `{marker}` marker, found {marker_count}"
                ),
            });
        }
    } else if trimmed.contains("{{") || trimmed.contains("}}") {
        return Err(SqlBlockError::InvalidColumn {
            reason: "column markers require a matching `-- columns:` directive".to_string(),
        });
    }
    blocks.push(SqlBlock {
        function_name: name,
        emit_batch,
        scalar_cardinality,
        sql: trimmed.to_string(),
        column_substitution,
        scope,
    });
    sql.clear();
    Ok(())
}

fn parse_scope_directive(
    body: &str,
    inside: bool,
    scope: &mut ScopeDirectives,
) -> std::result::Result<bool, SqlBlockError> {
    let Some(keyword) = ["produces", "consumes", "unscoped"]
        .into_iter()
        .find(|keyword| {
            body.strip_prefix(keyword).is_some_and(|rest| {
                rest.is_empty()
                    || rest.starts_with(':')
                    || rest.starts_with('=')
                    || rest.starts_with(char::is_whitespace)
            })
        })
    else {
        return Ok(false);
    };
    let invalid = |reason: &str| SqlBlockError::InvalidScope {
        reason: format!("{keyword}: {reason}"),
    };
    if !inside {
        return Err(invalid("must appear inside a -- func: block"));
    }
    let value = body
        .strip_prefix(keyword)
        .unwrap()
        .strip_prefix(':')
        .ok_or_else(|| invalid("requires a colon"))?
        .trim();
    if keyword == "unscoped" {
        if value.is_empty()
            || scope.unscoped.is_some()
            || scope.produces.is_some()
            || !scope.consumes.is_empty()
        {
            return Err(invalid(
                "requires a nonempty reason and cannot coexist with another scope directive",
            ));
        }
        scope.unscoped = Some(value.to_string());
        return Ok(true);
    }
    if scope.unscoped.is_some() {
        return Err(invalid("cannot coexist with unscoped"));
    }
    let (name, path) = value
        .split_once('=')
        .ok_or_else(|| invalid("expected name = absolute::Type"))?;
    let name = name.trim();
    let path = path.trim();
    syn::parse_str::<syn::Ident>(name).map_err(|_| invalid("invalid column or parameter name"))?;
    let parsed =
        syn::parse_str::<syn::Path>(path).map_err(|_| invalid("invalid Rust type path"))?;
    if parsed.segments.len() < 2
        || parsed.leading_colon.is_some()
        || parsed
            .segments
            .iter()
            .any(|segment| !matches!(segment.arguments, syn::PathArguments::None))
        || matches!(
            parsed.segments.first().unwrap().ident.to_string().as_str(),
            "self" | "super" | "crate"
        )
    {
        return Err(invalid("use the home registry's absolute crate path"));
    }
    let binding = ScopeBinding {
        name: name.to_string(),
        rust_path: path.to_string(),
    };
    if keyword == "produces" {
        if scope.produces.replace(binding).is_some() {
            return Err(invalid("only one produced ID is supported"));
        }
    } else {
        if scope
            .consumes
            .iter()
            .any(|prior| prior.name.eq_ignore_ascii_case(name))
        {
            return Err(invalid("duplicate consumed parameter"));
        }
        scope.consumes.push(binding);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_cardinality_is_scoped_and_rejects_malformed_values() {
        for (value, expected) in [
            ("many", ScalarCardinality::Many),
            ("one", ScalarCardinality::One),
            ("both", ScalarCardinality::Both),
        ] {
            let source = format!(
                "-- func: find\n-- cardinality: {value}\nselect 1;\n-- func: other\nselect 2"
            );
            let blocks = parse_sql_blocks(&source).unwrap();
            assert_eq!(blocks[0].scalar_cardinality, Some(expected));
            assert_eq!(blocks[1].scalar_cardinality, None);
            assert!(!blocks[0].sql.contains("cardinality"));
        }
        for source in [
            "-- cardinality: one\n-- func: find\nselect 1",
            "-- func: find\n-- cardinality: one\n-- cardinality: both\nselect 1",
            "-- func: find\n-- cardinality: single\nselect 1",
            "-- func: find\n-- cardinality one\nselect 1",
            "-- func: find\n-- cardinality=one\nselect 1",
        ] {
            assert!(
                matches!(
                    parse_sql_blocks(source),
                    Err(SqlBlockError::InvalidCardinality { .. })
                ),
                "{source}"
            );
        }
    }

    #[test]
    fn batch_opt_in_is_scoped_and_validated() {
        let blocks = parse_sql_blocks(
            "-- func: insert
-- batch
insert into items values (@id);
-- func: remove
delete from items where id = @id;",
        )
        .unwrap();
        assert!(blocks[0].emit_batch);
        assert!(!blocks[1].emit_batch);
        assert!(!blocks[0].sql.contains("-- batch"));
        for source in [
            "-- batch
-- func: insert
select 1",
            "-- func: insert
-- batch
-- batch
select 1",
            "-- func: insert
-- batch: true
select 1",
            "-- func: insert\n-- batch true\nselect 1",
            "-- func: insert\n-- batch=true\nselect 1",
        ] {
            assert!(matches!(
                parse_sql_blocks(source),
                Err(SqlBlockError::InvalidBatch { .. })
            ));
        }
    }

    #[test]
    fn parses_named_sql_blocks() {
        let blocks = parse_sql_blocks(
            "
            -- file comment
            -- func: list_users
            select id, name from users;

            -- func: delete_user
            delete from users where id = @id;
            ",
        )
        .unwrap();

        assert_eq!(
            blocks,
            vec![
                SqlBlock {
                    function_name: "list_users".to_string(),
                    emit_batch: false,
                    scalar_cardinality: None,
                    sql: "select id, name from users;".to_string(),
                    column_substitution: None,
                    scope: ScopeDirectives::default(),
                },
                SqlBlock {
                    function_name: "delete_user".to_string(),
                    emit_batch: false,
                    scalar_cardinality: None,
                    sql: "delete from users where id = @id;".to_string(),
                    column_substitution: None,
                    scope: ScopeDirectives::default(),
                },
            ]
        );
    }

    #[test]
    fn parses_a_single_allowlisted_column_substitution() {
        let blocks = parse_sql_blocks(
            "
            -- func: update_user_field
            -- columns: name, nickname
            update users set {{column}} = @value where id = @id;
            ",
        )
        .unwrap();

        assert_eq!(
            blocks,
            vec![SqlBlock {
                function_name: "update_user_field".to_string(),
                emit_batch: false,
                scalar_cardinality: None,
                sql: "update users set {{column}} = @value where id = @id;".to_string(),
                column_substitution: Some(ColumnSubstitution {
                    choices: vec!["name".to_string(), "nickname".to_string()],
                }),
                scope: ScopeDirectives::default(),
            }]
        );
    }

    #[test]
    fn continues_one_column_allowlist_across_comment_lines() {
        let blocks = parse_sql_blocks(
            "
            -- func: update_user_field
            -- columns: name, nickname
            -- columns: active
            update users set {{column}} = @value where id = @id;
            ",
        )
        .unwrap();

        assert_eq!(
            blocks[0].column_substitution,
            Some(ColumnSubstitution {
                choices: vec![
                    "name".to_string(),
                    "nickname".to_string(),
                    "active".to_string(),
                ],
            })
        );
    }

    #[test]
    fn rejects_unsafe_or_ambiguous_column_substitutions() {
        for sql in [
            "-- columns: name, nickname\n-- func: update_user\nupdate users set {{column}} = @value",
            "-- func: update_user\n-- columns: name\nupdate users set {{column}} = @value",
            "-- func: update_user\n-- columns: name, name\nupdate users set {{column}} = @value",
            "-- func: update_user\n-- columns: name, users.nickname\nupdate users set {{column}} = @value",
            "-- func: update_user\n-- columns: name, nickname\nupdate users set name = @value",
            "-- func: update_user\nupdate users set {{column}} = @value",
        ] {
            assert!(parse_sql_blocks(sql).is_err(), "{sql}");
        }
    }

    #[test]
    fn rejects_sql_without_a_func_block() {
        assert_eq!(
            parse_sql_blocks("select 1").unwrap_err(),
            SqlBlockError::SqlBeforeFirstFunc
        );
        assert_eq!(
            parse_sql_blocks("-- comment").unwrap_err(),
            SqlBlockError::MissingFunc
        );
    }

    #[test]
    fn rejects_invalid_func_blocks() {
        assert_eq!(
            parse_sql_blocks("-- func:\nselect 1").unwrap_err(),
            SqlBlockError::EmptyFuncName
        );
        assert_eq!(
            parse_sql_blocks("-- func: list-users\nselect 1").unwrap_err(),
            SqlBlockError::InvalidFuncName {
                name: "list-users".to_string(),
            }
        );
        assert_eq!(
            parse_sql_blocks("-- func: type\nselect 1").unwrap_err(),
            SqlBlockError::InvalidFuncName {
                name: "type".to_string(),
            }
        );
        assert_eq!(
            parse_sql_blocks("-- func: list_users\n-- only comment").unwrap_err(),
            SqlBlockError::EmptyBlock {
                name: "list_users".to_string(),
            }
        );
        assert_eq!(
            parse_sql_blocks("-- func: list_users\nselect 1\n-- func: list_users\nselect 2")
                .unwrap_err(),
            SqlBlockError::DuplicateFunc {
                name: "list_users".to_string(),
            }
        );
    }
}
