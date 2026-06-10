#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlValidationError {
    Empty,
    MultipleStatements,
}

impl std::fmt::Display for SqlValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("SQL file is empty"),
            Self::MultipleStatements => f.write_str("SQL file contains multiple statements"),
        }
    }
}

pub fn validate_sql(sql: &str) -> std::result::Result<String, SqlValidationError> {
    let trimmed = sql.trim();
    if strip_comments(trimmed).trim().is_empty() {
        return Err(SqlValidationError::Empty);
    }

    let Some(statement_end) = first_semicolon_outside_strings(trimmed) else {
        return Ok(trimmed.to_string());
    };

    let before_semicolon = trimmed[..statement_end].trim_end();
    let after_semicolon = trimmed[statement_end + 1..].trim();
    if strip_comments(after_semicolon).trim().is_empty() {
        return Ok(before_semicolon.to_string());
    }

    Err(SqlValidationError::MultipleStatements)
}

pub fn contains_semicolon_outside_strings(sql: &str) -> bool {
    first_semicolon_outside_strings(sql).is_some()
}

pub fn strip_comments(sql: &str) -> String {
    let mut output = String::new();
    let mut chars = sql.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;

    while let Some(c) = chars.next() {
        if in_single {
            output.push(c);
            if c == '\'' {
                if chars.next_if_eq(&'\'').is_some() {
                    output.push('\'');
                } else {
                    in_single = false;
                }
            }
            continue;
        }

        if in_double {
            output.push(c);
            if c == '"' {
                in_double = false;
            }
            continue;
        }

        match c {
            '\'' => {
                in_single = true;
                output.push(c);
            }
            '"' => {
                in_double = true;
                output.push(c);
            }
            '-' if chars.peek() == Some(&'-') => {
                chars.next();
                for comment_char in chars.by_ref() {
                    if comment_char == '\n' {
                        output.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut previous = '\0';
                for comment_char in chars.by_ref() {
                    if previous == '*' && comment_char == '/' {
                        output.push(' ');
                        break;
                    }
                    previous = comment_char;
                }
            }
            _ => output.push(c),
        }
    }

    output
}

fn first_semicolon_outside_strings(sql: &str) -> Option<usize> {
    let mut chars = sql.char_indices().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut previous = '\0';

    while let Some((index, c)) = chars.next() {
        if in_line_comment {
            if c == '\n' {
                in_line_comment = false;
            }
            continue;
        }

        if in_block_comment {
            if previous == '*' && c == '/' {
                in_block_comment = false;
                previous = '\0';
            } else {
                previous = c;
            }
            continue;
        }

        if in_single {
            if c == '\'' {
                if chars.next_if(|(_, next)| *next == '\'').is_some() {
                    continue;
                }
                in_single = false;
            }
            continue;
        }

        if in_double {
            if c == '"' {
                in_double = false;
            }
            continue;
        }

        match c {
            ';' => return Some(index),
            '\'' => in_single = true,
            '"' => in_double = true,
            '-' if chars.peek().is_some_and(|(_, next)| *next == '-') => {
                chars.next();
                in_line_comment = true;
            }
            '/' if chars.peek().is_some_and(|(_, next)| *next == '*') => {
                chars.next();
                in_block_comment = true;
                previous = '\0';
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_single_statement_sql() {
        assert_eq!(validate_sql("SELECT 1").unwrap(), "SELECT 1");
        assert_eq!(validate_sql("SELECT 1;").unwrap(), "SELECT 1");
        assert_eq!(validate_sql("SELECT ';';").unwrap(), "SELECT ';'");
    }

    #[test]
    fn rejects_empty_comment_only_and_multiple_statement_sql() {
        assert_eq!(validate_sql("").unwrap_err(), SqlValidationError::Empty);
        assert_eq!(
            validate_sql("-- only comment").unwrap_err(),
            SqlValidationError::Empty
        );
        assert_eq!(
            validate_sql("/* only comment */").unwrap_err(),
            SqlValidationError::Empty
        );
        assert_eq!(
            validate_sql("SELECT 1; SELECT 2").unwrap_err(),
            SqlValidationError::MultipleStatements
        );
    }

    #[test]
    fn ignores_semicolons_inside_strings_identifiers_and_comments() {
        assert!(!contains_semicolon_outside_strings("SELECT 1"));
        assert!(contains_semicolon_outside_strings("SELECT 1; SELECT 2"));
        assert!(!contains_semicolon_outside_strings("SELECT 'hello;world'"));
        assert!(!contains_semicolon_outside_strings(
            "SELECT \"hello;world\""
        ));
        assert!(!contains_semicolon_outside_strings(
            "SELECT 1 -- a; comment"
        ));
        assert!(!contains_semicolon_outside_strings(
            "SELECT /* a; comment */ 1"
        ));
    }

    #[test]
    fn removes_trailing_statement_comments_after_semicolon() {
        assert_eq!(validate_sql("SELECT 1; -- comment").unwrap(), "SELECT 1");
        assert_eq!(validate_sql("SELECT 1; /* comment */").unwrap(), "SELECT 1");
        assert_eq!(
            validate_sql("-- returns: Foo\nSELECT 1; -- comment").unwrap(),
            "-- returns: Foo\nSELECT 1"
        );
        assert_eq!(
            validate_sql("-- returns: Foo\nSELECT 1;\n-- comment").unwrap(),
            "-- returns: Foo\nSELECT 1"
        );
    }
}
