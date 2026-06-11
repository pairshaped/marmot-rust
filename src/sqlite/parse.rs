use std::collections::BTreeMap;

use crate::sqlite::tokenize::Token;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub schema: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableBinding {
    pub table: TableRef,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FromItem {
    pub binding: TableBinding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasError {
    Collision(String),
}

pub fn from_items(tokens: &[Token]) -> Vec<FromItem> {
    let mut items = Vec::new();
    let mut depth = 0usize;
    let mut index = 0usize;

    while index < tokens.len() {
        match &tokens[index] {
            Token::OpenParen => {
                depth += 1;
                index += 1;
            }
            Token::CloseParen => {
                depth = depth.saturating_sub(1);
                index += 1;
            }
            token if depth == 0 && token_is_word(token, "FROM") => {
                index = parse_from_sequence(tokens, index + 1, &mut items);
            }
            token if depth == 0 && token_is_word(token, "JOIN") => {
                if let Some((binding, after_binding)) = parse_table_binding(tokens, index + 1) {
                    items.push(FromItem { binding });
                    index = after_binding;
                } else {
                    index += 1;
                }
            }
            token if depth == 0 && compound_keyword(token) => break,
            _ => index += 1,
        }
    }

    items
}

pub fn build_alias_map(bindings: &[TableBinding]) -> Result<BTreeMap<String, String>, AliasError> {
    let mut map = BTreeMap::new();

    for binding in bindings {
        let key = binding
            .alias
            .as_deref()
            .unwrap_or(&binding.table.name)
            .to_ascii_lowercase();
        let table = binding.table.name.to_ascii_lowercase();
        if map.insert(key.clone(), table).is_some() {
            return Err(AliasError::Collision(key));
        }
    }

    Ok(map)
}

pub fn table_references(tokens: &[Token]) -> BTreeMap<String, String> {
    let mut refs = BTreeMap::new();
    for item in all_from_items(tokens) {
        let key = item
            .binding
            .alias
            .as_deref()
            .unwrap_or(&item.binding.table.name)
            .to_ascii_lowercase();
        refs.insert(key, item.binding.table.name.to_ascii_lowercase());
    }
    refs
}

fn all_from_items(tokens: &[Token]) -> Vec<FromItem> {
    let mut items = Vec::new();
    let mut index = 0usize;

    while index < tokens.len() {
        if tokens
            .get(index)
            .is_some_and(|token| token_is_word(token, "FROM"))
        {
            index = parse_from_sequence(tokens, index + 1, &mut items);
            continue;
        }
        if tokens
            .get(index)
            .is_some_and(|token| token_is_word(token, "JOIN"))
        {
            if let Some((binding, after_binding)) = parse_table_binding(tokens, index + 1) {
                items.push(FromItem { binding });
                index = after_binding;
                continue;
            }
        }
        index += 1;
    }

    items
}

fn parse_from_sequence(tokens: &[Token], mut index: usize, items: &mut Vec<FromItem>) -> usize {
    while index < tokens.len() {
        if matches!(tokens.get(index), Some(Token::Comma)) {
            index += 1;
            continue;
        }
        if tokens.get(index).is_some_and(from_sequence_boundary) {
            break;
        }

        let Some((binding, after_binding)) = parse_table_binding(tokens, index) else {
            break;
        };
        items.push(FromItem { binding });
        index = after_binding;

        while index < tokens.len()
            && !matches!(tokens.get(index), Some(Token::Comma))
            && !tokens.get(index).is_some_and(from_sequence_boundary)
            && !tokens.get(index).is_some_and(join_keyword)
        {
            index += 1;
        }

        if tokens.get(index).is_some_and(join_keyword) {
            break;
        }
    }

    index
}

fn parse_table_binding(tokens: &[Token], index: usize) -> Option<(TableBinding, usize)> {
    let (table, mut index) = parse_table_ref(tokens, index)?;
    let mut alias = None;

    if tokens
        .get(index)
        .is_some_and(|token| token_is_word(token, "AS"))
    {
        if let Some(name) = tokens.get(index + 1).and_then(identifier_from_token)
            && !alias_stop_word(name)
        {
            alias = Some(name.to_ascii_lowercase());
            index += 2;
        }
    } else if let Some(name) = tokens.get(index).and_then(identifier_from_token)
        && !alias_stop_word(name)
    {
        alias = Some(name.to_ascii_lowercase());
        index += 1;
    }

    Some((TableBinding { table, alias }, index))
}

fn parse_table_ref(tokens: &[Token], index: usize) -> Option<(TableRef, usize)> {
    let first = identifier_from_token(tokens.get(index)?)?;
    if matches!(tokens.get(index + 1), Some(Token::Dot)) {
        let name = identifier_from_token(tokens.get(index + 2)?)?;
        return Some((
            TableRef {
                schema: Some(first.to_ascii_lowercase()),
                name: name.to_ascii_lowercase(),
            },
            index + 3,
        ));
    }

    Some((
        TableRef {
            schema: None,
            name: first.to_ascii_lowercase(),
        },
        index + 1,
    ))
}

fn identifier_from_token(token: &Token) -> Option<&str> {
    match token {
        Token::Word(word) | Token::QuotedId(word) => Some(word),
        _ => None,
    }
}

fn token_is_word(token: &Token, expected: &str) -> bool {
    matches!(token, Token::Word(word) if word.eq_ignore_ascii_case(expected))
}

fn from_sequence_boundary(token: &Token) -> bool {
    matches!(token, Token::Semicolon)
        || matches!(
            identifier_from_token(token).map(|word| word.to_ascii_uppercase()),
            Some(word)
                if matches!(
                    word.as_str(),
                    "WHERE"
                        | "GROUP"
                        | "HAVING"
                        | "ORDER"
                        | "LIMIT"
                        | "OFFSET"
                        | "UNION"
                        | "INTERSECT"
                        | "EXCEPT"
                )
        )
}

fn join_keyword(token: &Token) -> bool {
    matches!(
        identifier_from_token(token).map(|word| word.to_ascii_uppercase()),
        Some(word)
            if matches!(
                word.as_str(),
                "JOIN" | "LEFT" | "RIGHT" | "INNER" | "OUTER" | "CROSS" | "NATURAL" | "FULL"
            )
    )
}

fn compound_keyword(token: &Token) -> bool {
    matches!(
        identifier_from_token(token).map(|word| word.to_ascii_uppercase()),
        Some(word) if matches!(word.as_str(), "UNION" | "INTERSECT" | "EXCEPT")
    )
}

fn alias_stop_word(word: &str) -> bool {
    matches!(
        word.to_ascii_uppercase().as_str(),
        "ON" | "USING"
            | "WHERE"
            | "GROUP"
            | "HAVING"
            | "ORDER"
            | "LIMIT"
            | "OFFSET"
            | "UNION"
            | "INTERSECT"
            | "EXCEPT"
            | "JOIN"
            | "LEFT"
            | "RIGHT"
            | "INNER"
            | "OUTER"
            | "CROSS"
            | "NATURAL"
            | "FULL"
            | "INDEXED"
            | "NOT"
            | "RETURNING"
            | "VALUES"
            | "SET"
            | "FROM"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::tokenize::tokenize;

    fn parse_from(sql: &str) -> Vec<FromItem> {
        from_items(&tokenize(sql))
    }

    #[test]
    fn parses_single_table_from_item() {
        assert_eq!(
            parse_from("select * from users"),
            [FromItem {
                binding: TableBinding {
                    table: TableRef {
                        schema: None,
                        name: "users".to_string(),
                    },
                    alias: None,
                },
            }]
        );
    }

    #[test]
    fn parses_explicit_and_implicit_aliases() {
        assert_eq!(
            parse_from("select * from users as u join orders o on o.user_id = u.id"),
            [
                FromItem {
                    binding: TableBinding {
                        table: TableRef {
                            schema: None,
                            name: "users".to_string(),
                        },
                        alias: Some("u".to_string()),
                    },
                },
                FromItem {
                    binding: TableBinding {
                        table: TableRef {
                            schema: None,
                            name: "orders".to_string(),
                        },
                        alias: Some("o".to_string()),
                    },
                },
            ]
        );
    }

    #[test]
    fn parses_schema_qualified_and_quoted_tables() {
        assert_eq!(
            parse_from(r#"select * from main."user table""#),
            [FromItem {
                binding: TableBinding {
                    table: TableRef {
                        schema: Some("main".to_string()),
                        name: "user table".to_string(),
                    },
                    alias: None,
                },
            }]
        );
    }

    #[test]
    fn parses_comma_separated_from_items() {
        assert_eq!(
            parse_from("select * from users u, orders o"),
            [
                FromItem {
                    binding: TableBinding {
                        table: TableRef {
                            schema: None,
                            name: "users".to_string(),
                        },
                        alias: Some("u".to_string()),
                    },
                },
                FromItem {
                    binding: TableBinding {
                        table: TableRef {
                            schema: None,
                            name: "orders".to_string(),
                        },
                        alias: Some("o".to_string()),
                    },
                },
            ]
        );
    }

    #[test]
    fn parses_self_join_aliases() {
        assert_eq!(
            parse_from("select * from users u join users manager on manager.id = u.manager_id"),
            [
                FromItem {
                    binding: TableBinding {
                        table: TableRef {
                            schema: None,
                            name: "users".to_string(),
                        },
                        alias: Some("u".to_string()),
                    },
                },
                FromItem {
                    binding: TableBinding {
                        table: TableRef {
                            schema: None,
                            name: "users".to_string(),
                        },
                        alias: Some("manager".to_string()),
                    },
                },
            ]
        );
    }

    #[test]
    fn does_not_treat_clause_keywords_as_aliases() {
        assert_eq!(
            parse_from("select a from t1 union select b from t2"),
            [FromItem {
                binding: TableBinding {
                    table: TableRef {
                        schema: None,
                        name: "t1".to_string(),
                    },
                    alias: None,
                },
            }]
        );
        assert_eq!(
            parse_from("select * from into"),
            [FromItem {
                binding: TableBinding {
                    table: TableRef {
                        schema: None,
                        name: "into".to_string(),
                    },
                    alias: None,
                },
            }]
        );
        assert_eq!(
            parse_from("select * from returning"),
            [FromItem {
                binding: TableBinding {
                    table: TableRef {
                        schema: None,
                        name: "returning".to_string(),
                    },
                    alias: None,
                },
            }]
        );
    }

    #[test]
    fn alias_map_uses_aliases_when_present_and_table_names_otherwise() {
        let bindings = [
            TableBinding {
                table: TableRef {
                    schema: None,
                    name: "users".to_string(),
                },
                alias: Some("u".to_string()),
            },
            TableBinding {
                table: TableRef {
                    schema: None,
                    name: "orders".to_string(),
                },
                alias: None,
            },
        ];

        let map = build_alias_map(&bindings).unwrap();

        assert_eq!(map.get("u").map(String::as_str), Some("users"));
        assert_eq!(map.get("orders").map(String::as_str), Some("orders"));
        assert!(!map.contains_key("users"));
    }

    #[test]
    fn alias_map_allows_self_joins_with_distinct_aliases() {
        let bindings = [
            TableBinding {
                table: TableRef {
                    schema: None,
                    name: "users".to_string(),
                },
                alias: Some("u".to_string()),
            },
            TableBinding {
                table: TableRef {
                    schema: None,
                    name: "users".to_string(),
                },
                alias: Some("manager".to_string()),
            },
        ];

        let map = build_alias_map(&bindings).unwrap();

        assert_eq!(map.get("u").map(String::as_str), Some("users"));
        assert_eq!(map.get("manager").map(String::as_str), Some("users"));
        assert!(!map.contains_key("users"));
    }

    #[test]
    fn alias_map_rejects_collisions() {
        let bindings = [
            TableBinding {
                table: TableRef {
                    schema: None,
                    name: "users".to_string(),
                },
                alias: Some("x".to_string()),
            },
            TableBinding {
                table: TableRef {
                    schema: None,
                    name: "orders".to_string(),
                },
                alias: Some("x".to_string()),
            },
        ];

        assert_eq!(
            build_alias_map(&bindings),
            Err(AliasError::Collision("x".to_string()))
        );
    }

    #[test]
    fn table_references_include_nested_query_bindings_for_analyzer_inference() {
        let refs = table_references(&tokenize(
            "select id from users where exists (
                select 1 from audit_logs u where u.actor_id = @actor_id
            )",
        ));

        assert_eq!(refs.get("users").map(String::as_str), Some("users"));
        assert_eq!(refs.get("u").map(String::as_str), Some("audit_logs"));
    }
}
