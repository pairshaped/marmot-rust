use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlFile {
    pub path: PathBuf,
    pub module_name: String,
    pub query_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parameter {
    pub name: String,
    pub sql_names: Vec<String>,
    pub column_type: ValueType,
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub field_name: String,
    pub column_type: ValueType,
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub source_path: PathBuf,
    pub module_name: String,
    pub name: String,
    pub return_type: ReturnType,
    pub sql: String,
    pub parameters: Vec<Parameter>,
    pub columns: Vec<Column>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub queries: Vec<Query>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReturnType {
    Execute,
    Rows { row_type: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueType {
    I64,
    F64,
    Bool,
    String,
    Bytes,
    Value,
}

impl ValueType {
    pub fn from_sqlite_type(declared_type: &str) -> Self {
        let normalized = declared_type
            .split_once('(')
            .map(|(base, _)| base)
            .unwrap_or(declared_type)
            .trim()
            .to_ascii_lowercase();

        match normalized.as_str() {
            "boolean" | "bool" => return Self::Bool,
            "date" | "time" | "datetime" | "timestamp" => return Self::String,
            _ => {}
        }

        if normalized.contains("int") {
            Self::I64
        } else if normalized.contains("char")
            || normalized.contains("clob")
            || normalized.contains("text")
        {
            Self::String
        } else if normalized.contains("blob") {
            Self::Bytes
        } else if normalized.contains("real")
            || normalized.contains("floa")
            || normalized.contains("doub")
        {
            Self::F64
        } else {
            Self::Value
        }
    }

    pub fn rust_type(&self) -> &'static str {
        match self {
            Self::I64 => "i64",
            Self::F64 => "f64",
            Self::Bool => "bool",
            Self::String => "String",
            Self::Bytes => "Vec<u8>",
            Self::Value => "rusqlite::types::Value",
        }
    }
}

impl Column {
    pub fn rust_type(&self) -> String {
        let rust_type = self.column_type.rust_type();
        if self.nullable {
            format!("Option<{rust_type}>")
        } else {
            rust_type.to_string()
        }
    }
}

impl Parameter {
    pub fn rust_type(&self) -> String {
        let rust_type = self.column_type.rust_type();
        if self.nullable {
            format!("Option<{rust_type}>")
        } else {
            rust_type.to_string()
        }
    }

    pub fn rust_argument_type(&self) -> String {
        if !self.nullable {
            return match self.column_type {
                ValueType::String => "impl AsRef<str>".to_string(),
                ValueType::Bytes => "impl AsRef<[u8]>".to_string(),
                _ => self.column_type.rust_type().to_string(),
            };
        }

        let rust_type = match self.column_type {
            ValueType::String => "&str",
            ValueType::Bytes => "&[u8]",
            _ => self.column_type.rust_type(),
        };
        format!("Option<{rust_type}>")
    }
}

pub fn sanitize_identifier(name: &str) -> String {
    let mut cleaned = String::new();
    for c in name.to_ascii_lowercase().chars() {
        match c {
            '-' | ' ' => cleaned.push('_'),
            '_' | 'a'..='z' | '0'..='9' => cleaned.push(c),
            _ => {}
        }
    }

    if cleaned
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_digit())
    {
        cleaned.insert(0, '_');
    }

    if is_rust_reserved_word(&cleaned) {
        cleaned.push('_');
    }

    cleaned
}

fn is_rust_reserved_word(name: &str) -> bool {
    matches!(
        name,
        "as" | "async"
            | "await"
            | "break"
            | "const"
            | "continue"
            | "crate"
            | "dyn"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
            | "abstract"
            | "become"
            | "box"
            | "do"
            | "final"
            | "macro"
            | "override"
            | "priv"
            | "try"
            | "typeof"
            | "unsized"
            | "virtual"
            | "yield"
            | "union"
            | "gen"
    )
}

pub fn query_name_from_filename(filename: &str) -> Option<String> {
    let base = filename.strip_suffix(".sql").unwrap_or(filename);
    let name = sanitize_identifier(base);
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_sqlite_declared_types_to_rust_value_types() {
        assert_eq!(ValueType::from_sqlite_type("INTEGER"), ValueType::I64);
        assert_eq!(ValueType::from_sqlite_type("REAL"), ValueType::F64);
        assert_eq!(ValueType::from_sqlite_type("TEXT"), ValueType::String);
        assert_eq!(ValueType::from_sqlite_type("BLOB"), ValueType::Bytes);
        assert_eq!(ValueType::from_sqlite_type("BOOLEAN"), ValueType::Bool);
        assert_eq!(ValueType::from_sqlite_type("TIMESTAMP"), ValueType::String);
        assert_eq!(ValueType::from_sqlite_type("DATETIME"), ValueType::String);
        assert_eq!(ValueType::from_sqlite_type("DATE"), ValueType::String);
    }

    #[test]
    fn maps_sqlite_declared_types_case_insensitively() {
        assert_eq!(ValueType::from_sqlite_type("integer"), ValueType::I64);
        assert_eq!(ValueType::from_sqlite_type("real"), ValueType::F64);
        assert_eq!(ValueType::from_sqlite_type("text"), ValueType::String);
        assert_eq!(ValueType::from_sqlite_type("boolean"), ValueType::Bool);
    }

    #[test]
    fn maps_sqlite_declared_type_affinity_variants() {
        assert_eq!(
            ValueType::from_sqlite_type("UNSIGNED BIG INT"),
            ValueType::I64
        );
        assert_eq!(
            ValueType::from_sqlite_type("CHARACTER VARYING(255)"),
            ValueType::String
        );
        assert_eq!(
            ValueType::from_sqlite_type("DOUBLE PRECISION"),
            ValueType::F64
        );
        assert_eq!(
            ValueType::from_sqlite_type("VARCHAR(255)"),
            ValueType::String
        );
        assert_eq!(ValueType::from_sqlite_type("INTEGER(8)"), ValueType::I64);
        assert_eq!(
            ValueType::from_sqlite_type("NVARCHAR(100)"),
            ValueType::String
        );
    }

    #[test]
    fn keeps_numeric_and_unknown_sqlite_declared_types_dynamic() {
        assert_eq!(
            ValueType::from_sqlite_type("DECIMAL(10,2)"),
            ValueType::Value
        );
        assert_eq!(
            ValueType::from_sqlite_type("NUMERIC(5,2)"),
            ValueType::Value
        );
        assert_eq!(ValueType::from_sqlite_type("POLYGON"), ValueType::Value);
    }

    #[test]
    fn renders_nullable_column_rust_types() {
        let column = Column {
            name: "bio".to_string(),
            field_name: "bio".to_string(),
            column_type: ValueType::String,
            nullable: true,
        };

        assert_eq!(column.rust_type(), "Option<String>");
    }

    #[test]
    fn renders_nullable_parameter_rust_types() {
        let parameter = Parameter {
            name: "bio".to_string(),
            sql_names: vec!["@bio".to_string()],
            column_type: ValueType::String,
            nullable: true,
        };

        assert_eq!(parameter.rust_type(), "Option<String>");
    }

    #[test]
    fn renders_borrowed_parameter_argument_types() {
        let text = Parameter {
            name: "bio".to_string(),
            sql_names: vec!["@bio".to_string()],
            column_type: ValueType::String,
            nullable: true,
        };
        let bytes = Parameter {
            name: "payload".to_string(),
            sql_names: vec!["@payload".to_string()],
            column_type: ValueType::Bytes,
            nullable: false,
        };

        assert_eq!(text.rust_argument_type(), "Option<&str>");
        assert_eq!(bytes.rust_argument_type(), "impl AsRef<[u8]>");
    }

    #[test]
    fn sanitizes_identifiers_like_gleam_marmot() {
        assert_eq!(sanitize_identifier("123abc"), "_123abc");
        assert_eq!(sanitize_identifier("hello@#$world"), "helloworld");
        assert_eq!(sanitize_identifier("HELLO"), "hello");
        assert_eq!(sanitize_identifier("my-col name"), "my_col_name");
        assert_eq!(sanitize_identifier("voila"), "voila");
        assert_eq!(sanitize_identifier("voila!"), "voila");
        assert_eq!(sanitize_identifier("voilà"), "voil");
    }

    #[test]
    fn sanitizes_rust_reserved_words() {
        assert_eq!(sanitize_identifier("type"), "type_");
        assert_eq!(sanitize_identifier("let"), "let_");
        assert_eq!(sanitize_identifier("fn"), "fn_");
        assert_eq!(sanitize_identifier("use"), "use_");
        assert_eq!(sanitize_identifier("match"), "match_");
    }

    #[test]
    fn derives_query_names_from_filenames_like_gleam_marmot() {
        assert_eq!(
            query_name_from_filename("find_user.sql").as_deref(),
            Some("find_user")
        );
        assert_eq!(
            query_name_from_filename("get-users.sql").as_deref(),
            Some("get_users")
        );
        assert_eq!(
            query_name_from_filename("1-get-users.sql").as_deref(),
            Some("_1_get_users")
        );
        assert_eq!(
            query_name_from_filename("my query.sql").as_deref(),
            Some("my_query")
        );
        assert_eq!(
            query_name_from_filename("Find_User.sql").as_deref(),
            Some("find_user")
        );
        assert_eq!(
            query_name_from_filename("find@user!.sql").as_deref(),
            Some("finduser")
        );
        assert_eq!(query_name_from_filename("@#$.sql"), None);
        assert_eq!(query_name_from_filename(".sql"), None);
    }
}
