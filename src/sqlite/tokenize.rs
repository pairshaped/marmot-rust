#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    Word(String),
    StringLit(String),
    QuotedId(String),
    Number(String),
    OpenParen,
    CloseParen,
    Comma,
    Semicolon,
    Dot,
    ParamAnon,
    ParamNamed { prefix: char, name: String },
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    Plus,
    Minus,
    Star,
    Slash,
    NullOverride,
    NullableOverride,
    Concat,
    Percent,
    ShiftLeft,
    ShiftRight,
    BitAnd,
    BitOr,
    Unknown(char),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpannedToken {
    pub token: Token,
    pub start: usize,
    pub end: usize,
}

pub fn tokenize(sql: &str) -> Vec<Token> {
    tokenize_spans(sql)
        .into_iter()
        .map(|spanned| spanned.token)
        .collect()
}

pub fn tokenize_spans(sql: &str) -> Vec<SpannedToken> {
    let byte_offsets = sql
        .char_indices()
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let chars = sql.chars().collect::<Vec<_>>();
    let mut tokens = Vec::new();
    let mut index = 0;

    while index < chars.len() {
        let c = chars[index];
        match c {
            c if c.is_whitespace() => index += 1,
            '-' if chars.get(index + 1) == Some(&'-') => {
                index += 2;
                while index < chars.len() && chars[index] != '\n' {
                    index += 1;
                }
            }
            '/' if chars.get(index + 1) == Some(&'*') => {
                index += 2;
                while index + 1 < chars.len() {
                    if chars[index] == '*' && chars[index + 1] == '/' {
                        index += 2;
                        break;
                    }
                    index += 1;
                }
            }
            '\'' => {
                let start = index;
                let (text, next) = consume_single_quoted(&chars, index + 1);
                push_token(
                    &mut tokens,
                    Token::StringLit(text),
                    start,
                    next,
                    &byte_offsets,
                    sql.len(),
                );
                index = next;
            }
            '"' => {
                let start = index;
                let (text, next) = consume_quoted_identifier(&chars, index + 1, '"');
                push_token(
                    &mut tokens,
                    Token::QuotedId(text),
                    start,
                    next,
                    &byte_offsets,
                    sql.len(),
                );
                index = next;
            }
            '`' => {
                let start = index;
                let (text, next) = consume_quoted_identifier(&chars, index + 1, '`');
                push_token(
                    &mut tokens,
                    Token::QuotedId(text),
                    start,
                    next,
                    &byte_offsets,
                    sql.len(),
                );
                index = next;
            }
            '|' if chars.get(index + 1) == Some(&'|') => {
                push_token(
                    &mut tokens,
                    Token::Concat,
                    index,
                    index + 2,
                    &byte_offsets,
                    sql.len(),
                );
                index += 2;
            }
            '<' if chars.get(index + 1) == Some(&'<') => {
                push_token(
                    &mut tokens,
                    Token::ShiftLeft,
                    index,
                    index + 2,
                    &byte_offsets,
                    sql.len(),
                );
                index += 2;
            }
            '>' if chars.get(index + 1) == Some(&'>') => {
                push_token(
                    &mut tokens,
                    Token::ShiftRight,
                    index,
                    index + 2,
                    &byte_offsets,
                    sql.len(),
                );
                index += 2;
            }
            '<' if chars.get(index + 1) == Some(&'=') => {
                push_token(
                    &mut tokens,
                    Token::Le,
                    index,
                    index + 2,
                    &byte_offsets,
                    sql.len(),
                );
                index += 2;
            }
            '>' if chars.get(index + 1) == Some(&'=') => {
                push_token(
                    &mut tokens,
                    Token::Ge,
                    index,
                    index + 2,
                    &byte_offsets,
                    sql.len(),
                );
                index += 2;
            }
            '<' if chars.get(index + 1) == Some(&'>') => {
                push_token(
                    &mut tokens,
                    Token::Ne,
                    index,
                    index + 2,
                    &byte_offsets,
                    sql.len(),
                );
                index += 2;
            }
            '!' if chars.get(index + 1) == Some(&'=') => {
                push_token(
                    &mut tokens,
                    Token::Ne,
                    index,
                    index + 2,
                    &byte_offsets,
                    sql.len(),
                );
                index += 2;
            }
            '<' => {
                push_token(
                    &mut tokens,
                    Token::Lt,
                    index,
                    index + 1,
                    &byte_offsets,
                    sql.len(),
                );
                index += 1;
            }
            '>' => {
                push_token(
                    &mut tokens,
                    Token::Gt,
                    index,
                    index + 1,
                    &byte_offsets,
                    sql.len(),
                );
                index += 1;
            }
            '=' => {
                push_token(
                    &mut tokens,
                    Token::Eq,
                    index,
                    index + 1,
                    &byte_offsets,
                    sql.len(),
                );
                index += 1;
            }
            '+' => {
                push_token(
                    &mut tokens,
                    Token::Plus,
                    index,
                    index + 1,
                    &byte_offsets,
                    sql.len(),
                );
                index += 1;
            }
            '-' => {
                push_token(
                    &mut tokens,
                    Token::Minus,
                    index,
                    index + 1,
                    &byte_offsets,
                    sql.len(),
                );
                index += 1;
            }
            '*' => {
                push_token(
                    &mut tokens,
                    Token::Star,
                    index,
                    index + 1,
                    &byte_offsets,
                    sql.len(),
                );
                index += 1;
            }
            '/' => {
                push_token(
                    &mut tokens,
                    Token::Slash,
                    index,
                    index + 1,
                    &byte_offsets,
                    sql.len(),
                );
                index += 1;
            }
            '%' => {
                push_token(
                    &mut tokens,
                    Token::Percent,
                    index,
                    index + 1,
                    &byte_offsets,
                    sql.len(),
                );
                index += 1;
            }
            '&' => {
                push_token(
                    &mut tokens,
                    Token::BitAnd,
                    index,
                    index + 1,
                    &byte_offsets,
                    sql.len(),
                );
                index += 1;
            }
            '|' => {
                push_token(
                    &mut tokens,
                    Token::BitOr,
                    index,
                    index + 1,
                    &byte_offsets,
                    sql.len(),
                );
                index += 1;
            }
            '(' => {
                push_token(
                    &mut tokens,
                    Token::OpenParen,
                    index,
                    index + 1,
                    &byte_offsets,
                    sql.len(),
                );
                index += 1;
            }
            ')' => {
                push_token(
                    &mut tokens,
                    Token::CloseParen,
                    index,
                    index + 1,
                    &byte_offsets,
                    sql.len(),
                );
                index += 1;
            }
            ',' => {
                push_token(
                    &mut tokens,
                    Token::Comma,
                    index,
                    index + 1,
                    &byte_offsets,
                    sql.len(),
                );
                index += 1;
            }
            ';' => {
                push_token(
                    &mut tokens,
                    Token::Semicolon,
                    index,
                    index + 1,
                    &byte_offsets,
                    sql.len(),
                );
                index += 1;
            }
            '.' => {
                push_token(
                    &mut tokens,
                    Token::Dot,
                    index,
                    index + 1,
                    &byte_offsets,
                    sql.len(),
                );
                index += 1;
            }
            '?' => {
                let previous = tokens.last().map(|spanned| &spanned.token);
                let token =
                    if question_is_nullable_override(previous, chars.get(index + 1).copied()) {
                        Token::NullableOverride
                    } else {
                        Token::ParamAnon
                    };
                push_token(
                    &mut tokens,
                    token,
                    index,
                    index + 1,
                    &byte_offsets,
                    sql.len(),
                );
                index += 1;
            }
            '@' | ':' | '$' => {
                if let Some((name, next)) = consume_named_param(&chars, index + 1) {
                    push_token(
                        &mut tokens,
                        Token::ParamNamed { prefix: c, name },
                        index,
                        next,
                        &byte_offsets,
                        sql.len(),
                    );
                    index = next;
                } else {
                    push_token(
                        &mut tokens,
                        Token::Unknown(c),
                        index,
                        index + 1,
                        &byte_offsets,
                        sql.len(),
                    );
                    index += 1;
                }
            }
            '!' => {
                let previous = tokens.last().map(|spanned| &spanned.token);
                let token = if exclamation_is_null_override(previous, chars.get(index + 1).copied())
                {
                    Token::NullOverride
                } else {
                    Token::Unknown(c)
                };
                push_token(
                    &mut tokens,
                    token,
                    index,
                    index + 1,
                    &byte_offsets,
                    sql.len(),
                );
                index += 1;
            }
            c if c.is_ascii_digit() => {
                let start = index;
                let (number, next) = consume_number(&chars, index);
                push_token(
                    &mut tokens,
                    Token::Number(number),
                    start,
                    next,
                    &byte_offsets,
                    sql.len(),
                );
                index = next;
            }
            c if is_word_start(c) => {
                let start = index;
                let (word, next) = consume_word(&chars, index);
                push_token(
                    &mut tokens,
                    Token::Word(word),
                    start,
                    next,
                    &byte_offsets,
                    sql.len(),
                );
                index = next;
            }
            c => {
                push_token(
                    &mut tokens,
                    Token::Unknown(c),
                    index,
                    index + 1,
                    &byte_offsets,
                    sql.len(),
                );
                index += 1;
            }
        }
    }

    tokens
}

fn push_token(
    tokens: &mut Vec<SpannedToken>,
    token: Token,
    start: usize,
    end: usize,
    byte_offsets: &[usize],
    sql_len: usize,
) {
    tokens.push(SpannedToken {
        token,
        start: byte_offsets[start],
        end: byte_offsets.get(end).copied().unwrap_or(sql_len),
    });
}

fn consume_single_quoted(chars: &[char], mut index: usize) -> (String, usize) {
    let mut text = String::new();
    while index < chars.len() {
        let c = chars[index];
        if c == '\'' {
            if chars.get(index + 1) == Some(&'\'') {
                text.push('\'');
                index += 2;
                continue;
            }
            return (text, index + 1);
        }
        text.push(c);
        index += 1;
    }
    (text, index)
}

fn consume_quoted_identifier(chars: &[char], mut index: usize, quote: char) -> (String, usize) {
    let mut text = String::new();
    while index < chars.len() {
        let c = chars[index];
        if c == quote {
            if quote == '"' && chars.get(index + 1) == Some(&'"') {
                text.push('"');
                index += 2;
                continue;
            }
            return (text, index + 1);
        }
        text.push(c);
        index += 1;
    }
    (text, index)
}

fn consume_named_param(chars: &[char], index: usize) -> Option<(String, usize)> {
    if !chars.get(index).is_some_and(|c| is_parameter_start(*c)) {
        return None;
    }

    let mut end = index + 1;
    while chars.get(end).is_some_and(|c| is_parameter_continue(*c)) {
        end += 1;
    }
    Some((chars[index..end].iter().collect(), end))
}

fn consume_word(chars: &[char], index: usize) -> (String, usize) {
    let mut end = index + 1;
    while chars.get(end).is_some_and(|c| is_word_continue(*c)) {
        end += 1;
    }
    (chars[index..end].iter().collect(), end)
}

fn consume_number(chars: &[char], index: usize) -> (String, usize) {
    let mut end = index;
    while chars.get(end).is_some_and(|c| c.is_ascii_digit()) {
        end += 1;
    }

    if chars.get(end) == Some(&'.') && chars.get(end + 1).is_some_and(|c| c.is_ascii_digit()) {
        end += 1;
        while chars.get(end).is_some_and(|c| c.is_ascii_digit()) {
            end += 1;
        }
    }

    if chars[index..end] == ['0']
        && matches!(chars.get(end), Some('x' | 'X'))
        && chars.get(end + 1).is_some_and(|c| c.is_ascii_hexdigit())
    {
        end += 2;
        while chars.get(end).is_some_and(|c| c.is_ascii_hexdigit()) {
            end += 1;
        }
        return (
            chars[index..end].iter().collect::<String>().to_lowercase(),
            end,
        );
    }

    if matches!(chars.get(end), Some('e' | 'E')) {
        let exponent_start = end;
        end += 1;
        if matches!(chars.get(end), Some('+' | '-')) {
            end += 1;
        }
        if chars.get(end).is_some_and(|c| c.is_ascii_digit()) {
            while chars.get(end).is_some_and(|c| c.is_ascii_digit()) {
                end += 1;
            }
        } else {
            end = exponent_start;
        }
    }

    (
        chars[index..end].iter().collect::<String>().to_lowercase(),
        end,
    )
}

fn question_is_nullable_override(previous: Option<&Token>, next: Option<char>) -> bool {
    let previous_is_column_name = matches!(
        previous,
        Some(Token::Word(word)) if !word.eq_ignore_ascii_case("LIMIT") && !word.eq_ignore_ascii_case("OFFSET")
    );
    previous_is_column_name && is_boundary(next)
}

fn exclamation_is_null_override(previous: Option<&Token>, next: Option<char>) -> bool {
    matches!(previous, Some(Token::Word(_))) && is_boundary(next)
}

fn is_boundary(c: Option<char>) -> bool {
    matches!(c, None | Some(' ' | '\t' | '\n' | '\r' | ',' | ')'))
}

fn is_word_start(c: char) -> bool {
    c == '_' || c.is_ascii_alphabetic() || !c.is_ascii()
}

fn is_word_continue(c: char) -> bool {
    is_word_start(c) || c.is_ascii_digit()
}

fn is_parameter_start(c: char) -> bool {
    c == '_' || c.is_ascii_alphabetic()
}

fn is_parameter_continue(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_basic_selects() {
        assert_eq!(
            tokenize("SELECT id, name FROM users"),
            vec![
                Token::Word("SELECT".to_string()),
                Token::Word("id".to_string()),
                Token::Comma,
                Token::Word("name".to_string()),
                Token::Word("FROM".to_string()),
                Token::Word("users".to_string()),
            ]
        );
        assert_eq!(
            tokenize("SELECT * FROM users"),
            vec![
                Token::Word("SELECT".to_string()),
                Token::Star,
                Token::Word("FROM".to_string()),
                Token::Word("users".to_string()),
            ]
        );
        assert_eq!(
            tokenize("SELECT  id \t FROM \n users"),
            vec![
                Token::Word("SELECT".to_string()),
                Token::Word("id".to_string()),
                Token::Word("FROM".to_string()),
                Token::Word("users".to_string()),
            ]
        );
    }

    #[test]
    fn tokenizes_operators() {
        assert_eq!(
            tokenize("a = b != c <> d >= e <= f > g < h"),
            vec![
                Token::Word("a".to_string()),
                Token::Eq,
                Token::Word("b".to_string()),
                Token::Ne,
                Token::Word("c".to_string()),
                Token::Ne,
                Token::Word("d".to_string()),
                Token::Ge,
                Token::Word("e".to_string()),
                Token::Le,
                Token::Word("f".to_string()),
                Token::Gt,
                Token::Word("g".to_string()),
                Token::Lt,
                Token::Word("h".to_string()),
            ]
        );
        assert_eq!(
            tokenize("a + b - c * d / e % f || g << h >> i & j | k"),
            vec![
                Token::Word("a".to_string()),
                Token::Plus,
                Token::Word("b".to_string()),
                Token::Minus,
                Token::Word("c".to_string()),
                Token::Star,
                Token::Word("d".to_string()),
                Token::Slash,
                Token::Word("e".to_string()),
                Token::Percent,
                Token::Word("f".to_string()),
                Token::Concat,
                Token::Word("g".to_string()),
                Token::ShiftLeft,
                Token::Word("h".to_string()),
                Token::ShiftRight,
                Token::Word("i".to_string()),
                Token::BitAnd,
                Token::Word("j".to_string()),
                Token::BitOr,
                Token::Word("k".to_string()),
            ]
        );
    }

    #[test]
    fn tokenizes_string_literals_and_quoted_identifiers() {
        assert_eq!(
            tokenize("'it''s fine' \"column name\" `other col`"),
            vec![
                Token::StringLit("it's fine".to_string()),
                Token::QuotedId("column name".to_string()),
                Token::QuotedId("other col".to_string()),
            ]
        );
        assert_eq!(
            tokenize("WHERE name = 'test'"),
            vec![
                Token::Word("WHERE".to_string()),
                Token::Word("name".to_string()),
                Token::Eq,
                Token::StringLit("test".to_string()),
            ]
        );
    }

    #[test]
    fn tokenizes_numbers() {
        assert_eq!(tokenize("42"), vec![Token::Number("42".to_string())]);
        assert_eq!(tokenize("3.14"), vec![Token::Number("3.14".to_string())]);
        assert_eq!(
            tokenize("-5"),
            vec![Token::Minus, Token::Number("5".to_string())]
        );
        assert_eq!(
            tokenize("0x1f 1e-9 2E+10"),
            vec![
                Token::Number("0x1f".to_string()),
                Token::Number("1e-9".to_string()),
                Token::Number("2e+10".to_string()),
            ]
        );
    }

    #[test]
    fn tokenizes_parameters() {
        assert_eq!(
            tokenize("WHERE id = ? AND a = @user_id AND b = :id AND c = $pattern"),
            vec![
                Token::Word("WHERE".to_string()),
                Token::Word("id".to_string()),
                Token::Eq,
                Token::ParamAnon,
                Token::Word("AND".to_string()),
                Token::Word("a".to_string()),
                Token::Eq,
                Token::ParamNamed {
                    prefix: '@',
                    name: "user_id".to_string(),
                },
                Token::Word("AND".to_string()),
                Token::Word("b".to_string()),
                Token::Eq,
                Token::ParamNamed {
                    prefix: ':',
                    name: "id".to_string(),
                },
                Token::Word("AND".to_string()),
                Token::Word("c".to_string()),
                Token::Eq,
                Token::ParamNamed {
                    prefix: '$',
                    name: "pattern".to_string(),
                },
            ]
        );
    }

    #[test]
    fn tokenizes_nullability_overrides() {
        assert_eq!(
            tokenize("SELECT a?, b! FROM t WHERE id != ?"),
            vec![
                Token::Word("SELECT".to_string()),
                Token::Word("a".to_string()),
                Token::NullableOverride,
                Token::Comma,
                Token::Word("b".to_string()),
                Token::NullOverride,
                Token::Word("FROM".to_string()),
                Token::Word("t".to_string()),
                Token::Word("WHERE".to_string()),
                Token::Word("id".to_string()),
                Token::Ne,
                Token::ParamAnon,
            ]
        );
    }

    #[test]
    fn token_spans_are_byte_offsets() {
        let sql = "SELECT café AS café?";
        let spans = tokenize_spans(sql);
        let nullable = spans
            .iter()
            .find(|token| token.token == Token::NullableOverride)
            .unwrap();

        assert_eq!(&sql[nullable.start..nullable.end], "?");
        let stripped = format!("{}{}", &sql[..nullable.start], &sql[nullable.end..]);
        assert_eq!(stripped, "SELECT café AS café");
    }

    #[test]
    fn skips_comments() {
        assert_eq!(
            tokenize("SELECT id -- this is a comment\nFROM users"),
            vec![
                Token::Word("SELECT".to_string()),
                Token::Word("id".to_string()),
                Token::Word("FROM".to_string()),
                Token::Word("users".to_string()),
            ]
        );
        assert_eq!(
            tokenize("SELECT /* multi\nline ; comment */ id FROM users"),
            vec![
                Token::Word("SELECT".to_string()),
                Token::Word("id".to_string()),
                Token::Word("FROM".to_string()),
                Token::Word("users".to_string()),
            ]
        );
    }

    #[test]
    fn tokenizes_punctuation() {
        assert_eq!(
            tokenize("COUNT(*) u.name;"),
            vec![
                Token::Word("COUNT".to_string()),
                Token::OpenParen,
                Token::Star,
                Token::CloseParen,
                Token::Word("u".to_string()),
                Token::Dot,
                Token::Word("name".to_string()),
                Token::Semicolon,
            ]
        );
    }
}
