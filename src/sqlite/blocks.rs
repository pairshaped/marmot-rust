use crate::model::sanitize_identifier;
use crate::sql_text::strip_comments;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlBlock {
    pub function_name: String,
    pub emit_batch: bool,
    pub sql: String,
    pub column_substitution: Option<ColumnSubstitution>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSubstitution {
    pub choices: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlBlockError {
    MissingFunc,
    SqlBeforeFirstFunc,
    EmptyFuncName,
    InvalidFuncName { name: String },
    EmptyBlock { name: String },
    DuplicateFunc { name: String },
    ColumnBeforeFunc,
    InvalidBatch { reason: String },
    InvalidColumn { reason: String },
}

impl std::fmt::Display for SqlBlockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
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

    for line in sql.lines() {
        if let Some(name) = func_name_from_line(line)? {
            finish_block(
                &mut blocks,
                current_name.take(),
                &mut current_sql,
                current_column.take(),
                std::mem::take(&mut current_batch),
            )?;
            current_name = Some(name);
            continue;
        }

        let comment = line.trim().strip_prefix("--").map(str::trim);
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
        sql: trimmed.to_string(),
        column_substitution,
    });
    sql.clear();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
                    sql: "select id, name from users;".to_string(),
                    column_substitution: None,
                },
                SqlBlock {
                    function_name: "delete_user".to_string(),
                    emit_batch: false,
                    sql: "delete from users where id = @id;".to_string(),
                    column_substitution: None,
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
                sql: "update users set {{column}} = @value where id = @id;".to_string(),
                column_substitution: Some(ColumnSubstitution {
                    choices: vec!["name".to_string(), "nickname".to_string()],
                }),
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
