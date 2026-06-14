use crate::model::sanitize_identifier;
use crate::sql_text::strip_comments;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlBlock {
    pub function_name: String,
    pub sql: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlBlockError {
    MissingFunc,
    SqlBeforeFirstFunc,
    EmptyFuncName,
    InvalidFuncName { name: String },
    EmptyBlock { name: String },
    DuplicateFunc { name: String },
}

impl std::fmt::Display for SqlBlockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
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
        }
    }
}

pub fn parse_sql_blocks(sql: &str) -> std::result::Result<Vec<SqlBlock>, SqlBlockError> {
    let mut blocks = Vec::new();
    let mut current_name: Option<String> = None;
    let mut current_sql = String::new();

    for line in sql.lines() {
        if let Some(name) = func_name_from_line(line)? {
            finish_block(&mut blocks, current_name.take(), &mut current_sql)?;
            current_name = Some(name);
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

    finish_block(&mut blocks, current_name, &mut current_sql)?;

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

fn finish_block(
    blocks: &mut Vec<SqlBlock>,
    name: Option<String>,
    sql: &mut String,
) -> std::result::Result<(), SqlBlockError> {
    let Some(name) = name else {
        return Ok(());
    };
    let trimmed = sql.trim();
    if strip_comments(trimmed).trim().is_empty() {
        return Err(SqlBlockError::EmptyBlock { name });
    }
    blocks.push(SqlBlock {
        function_name: name,
        sql: trimmed.to_string(),
    });
    sql.clear();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
                    sql: "select id, name from users;".to_string(),
                },
                SqlBlock {
                    function_name: "delete_user".to_string(),
                    sql: "delete from users where id = @id;".to_string(),
                },
            ]
        );
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
