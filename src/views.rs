use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::sql_text::{strip_comments, validate_sql};

pub const VIEW_DIR: &str = "db_views";
pub const GENERATED_FILE: &str = "views.sql";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewDefinition {
    pub name: String,
    pub columns: Vec<String>,
    pub select_sql: String,
    pub source_path: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ViewAudit {
    pub database_only: Vec<String>,
}

impl ViewAudit {
    pub fn warnings(&self, source_root: &Path) -> Vec<String> {
        self.database_only
            .iter()
            .map(|name| database_only_warning(name, source_root))
            .collect()
    }

    pub fn deny_warnings(self, source_root: &Path) -> Result<(), ViewError> {
        if self.database_only.is_empty() {
            Ok(())
        } else {
            Err(ViewError::DatabaseOnlyViews {
                names: self.database_only,
                source_root: source_root.to_path_buf(),
            })
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ViewError {
    #[error("could not read view directory {path}: {source}")]
    ReadDirectory {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("view source path is not a directory: {path}")]
    SourcePathIsNotDirectory { path: PathBuf },

    #[error("could not read view declaration {path}: {source}")]
    ReadFile {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("invalid view declaration in {path}: {reason}")]
    InvalidDeclaration { path: PathBuf, reason: String },

    #[error("view declaration name `{name}` must match its filename stem `{filename}` in {path}")]
    FilenameMismatch {
        path: PathBuf,
        name: String,
        filename: String,
    },

    #[error("duplicate view declaration `{name}` in {first} and {second}")]
    DuplicateDeclaration {
        name: String,
        first: PathBuf,
        second: PathBuf,
    },

    #[error("could not open SQLite database {path}: {source}")]
    OpenDatabase {
        path: PathBuf,
        source: rusqlite::Error,
    },

    #[error("could not reconcile view `{name}` from {path}: {source}")]
    Reconcile {
        name: String,
        path: PathBuf,
        source: rusqlite::Error,
    },

    #[error("could not validate view `{name}` from {path}: {source}")]
    Validate {
        name: String,
        path: PathBuf,
        source: rusqlite::Error,
    },

    #[error(
        "view `{name}` from {path} declares {declared} output columns but its SELECT returns {actual}"
    )]
    ColumnCountMismatch {
        name: String,
        path: PathBuf,
        declared: usize,
        actual: usize,
    },

    #[error("could not inspect SQLite views: {source}")]
    InspectDatabase { source: rusqlite::Error },

    #[error("declared views are missing from the database: {names:?}")]
    SourceOnlyViews { names: Vec<String> },

    #[error("{message}", message = database_only_error_message(.names, .source_root))]
    DatabaseOnlyViews {
        names: Vec<String>,
        source_root: PathBuf,
    },

    #[error("could not create generated view directory {path}: {source}")]
    CreateGeneratedDirectory {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("could not write generated view SQL {path}: {source}")]
    WriteGeneratedFile {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("could not remove generated view SQL {path}: {source}")]
    RemoveGeneratedFile {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("generated view SQL is stale: {path}")]
    StaleGeneratedFile { path: PathBuf },
}

pub fn discover(source_root: &Path) -> Result<Vec<ViewDefinition>, ViewError> {
    let directory = source_root.join(VIEW_DIR);
    let metadata = match fs::metadata(&directory) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(ViewError::ReadDirectory {
                path: directory,
                source,
            });
        }
    };
    if !metadata.is_dir() {
        return Err(ViewError::SourcePathIsNotDirectory { path: directory });
    }

    let mut paths = fs::read_dir(&directory)
        .map_err(|source| ViewError::ReadDirectory {
            path: directory.clone(),
            source,
        })?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|source| ViewError::ReadDirectory {
                    path: directory.clone(),
                    source,
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    paths.retain(|path| {
        path.is_file() && path.extension().and_then(|extension| extension.to_str()) == Some("sql")
    });
    paths.sort();

    let mut definitions = Vec::new();
    let mut by_name = BTreeMap::<String, PathBuf>::new();
    for path in paths {
        let definition = parse_file(&path)?;
        if let Some(first) = by_name.insert(definition.name.clone(), path.clone()) {
            return Err(ViewError::DuplicateDeclaration {
                name: definition.name,
                first,
                second: path,
            });
        }
        definitions.push(definition);
    }
    Ok(definitions)
}

pub fn reconcile_database(
    database_path: &Path,
    source_root: &Path,
) -> Result<ViewAudit, ViewError> {
    let mut connection =
        Connection::open(database_path).map_err(|source| ViewError::OpenDatabase {
            path: database_path.to_path_buf(),
            source,
        })?;
    reconcile_connection(&mut connection, source_root)
}

pub fn reconcile_connection(
    connection: &mut Connection,
    source_root: &Path,
) -> Result<ViewAudit, ViewError> {
    let definitions = discover(source_root)?;
    let transaction = connection
        .transaction()
        .map_err(|source| ViewError::InspectDatabase { source })?;

    for definition in &definitions {
        transaction
            .execute_batch(&format!(
                "DROP VIEW IF EXISTS {};",
                quote_identifier(&definition.name)
            ))
            .map_err(|source| ViewError::Reconcile {
                name: definition.name.clone(),
                path: definition.source_path.clone(),
                source,
            })?;
    }
    for definition in &definitions {
        transaction
            .execute_batch(&create_statement(definition))
            .map_err(|source| ViewError::Reconcile {
                name: definition.name.clone(),
                path: definition.source_path.clone(),
                source,
            })?;
    }
    let audit = audit_connection(&transaction, &definitions)?;
    transaction
        .commit()
        .map_err(|source| ViewError::InspectDatabase { source })?;
    Ok(audit)
}

pub fn audit_database(database_path: &Path, source_root: &Path) -> Result<ViewAudit, ViewError> {
    let connection = Connection::open(database_path).map_err(|source| ViewError::OpenDatabase {
        path: database_path.to_path_buf(),
        source,
    })?;
    let definitions = discover(source_root)?;
    audit_connection(&connection, &definitions)
}

pub fn emit_generated_sql(
    definitions: &[ViewDefinition],
    output: &Path,
    check: bool,
) -> Result<(), ViewError> {
    let path = output.join(GENERATED_FILE);
    if definitions.is_empty() {
        if check {
            return if path.exists() {
                Err(ViewError::StaleGeneratedFile { path })
            } else {
                Ok(())
            };
        }
        return match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(ViewError::RemoveGeneratedFile { path, source }),
        };
    }

    let expected = generated_sql(definitions);
    if check {
        return match fs::read_to_string(&path) {
            Ok(actual) if actual == expected => Ok(()),
            Ok(_) => Err(ViewError::StaleGeneratedFile { path }),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                Err(ViewError::StaleGeneratedFile { path })
            }
            Err(source) => Err(ViewError::WriteGeneratedFile { path, source }),
        };
    }

    fs::create_dir_all(output).map_err(|source| ViewError::CreateGeneratedDirectory {
        path: output.to_path_buf(),
        source,
    })?;
    fs::write(&path, expected).map_err(|source| ViewError::WriteGeneratedFile { path, source })
}

pub fn generated_sql(definitions: &[ViewDefinition]) -> String {
    let mut output = String::from("-- Generated by Marmot. Do not edit.\n\n");
    for definition in definitions {
        output.push_str(&format!(
            "DROP VIEW IF EXISTS {};\n",
            quote_identifier(&definition.name)
        ));
    }
    for definition in definitions {
        output.push('\n');
        output.push_str(&create_statement(definition));
        output.push('\n');
    }
    output
}

fn parse_file(path: &Path) -> Result<ViewDefinition, ViewError> {
    let contents = fs::read_to_string(path).map_err(|source| ViewError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    let mut declaration = None;
    let mut sql = String::new();
    for line in contents.lines() {
        if let Some(value) = view_declaration(line) {
            if declaration.replace(value.to_string()).is_some() {
                return invalid(
                    path,
                    "the file contains more than one `-- view:` declaration",
                );
            }
        } else {
            sql.push_str(line);
            sql.push('\n');
        }
    }
    let declaration = declaration.ok_or_else(|| ViewError::InvalidDeclaration {
        path: path.to_path_buf(),
        reason: "the file must contain one `-- view: view_name(column, ...)` declaration"
            .to_string(),
    })?;
    let (name, columns) = parse_declaration(path, &declaration)?;
    let filename = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default()
        .to_string();
    if name != filename {
        return Err(ViewError::FilenameMismatch {
            path: path.to_path_buf(),
            name,
            filename,
        });
    }
    let select_sql = validate_sql(&sql).map_err(|reason| ViewError::InvalidDeclaration {
        path: path.to_path_buf(),
        reason: reason.to_string(),
    })?;
    let first_token = strip_comments(&select_sql)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !matches!(first_token.as_str(), "select" | "with") {
        return invalid(path, "a view definition must be a SELECT or WITH statement");
    }

    Ok(ViewDefinition {
        name,
        columns,
        select_sql,
        source_path: path.to_path_buf(),
    })
}

fn view_declaration(line: &str) -> Option<&str> {
    let body = line.trim().strip_prefix("--")?.trim();
    body.strip_prefix("view:").map(str::trim)
}

fn parse_declaration(path: &Path, declaration: &str) -> Result<(String, Vec<String>), ViewError> {
    let Some(open) = declaration.find('(') else {
        return invalid(
            path,
            "the declaration must include an explicit output column list",
        );
    };
    if !declaration.ends_with(')') {
        return invalid(
            path,
            "the declaration must end after its output column list",
        );
    }
    let name = declaration[..open].trim();
    if !valid_view_name(name) {
        return invalid(
            path,
            "the view name must be lowercase snake_case beginning with `view_`",
        );
    }
    let raw_columns = &declaration[open + 1..declaration.len() - 1];
    let columns = raw_columns
        .split(',')
        .map(str::trim)
        .map(str::to_string)
        .collect::<Vec<_>>();
    if columns.is_empty() || columns.iter().any(|column| !valid_identifier(column)) {
        return invalid(
            path,
            "output columns must be a non-empty comma-separated list of lowercase snake_case names",
        );
    }
    let unique = columns.iter().collect::<BTreeSet<_>>();
    if unique.len() != columns.len() {
        return invalid(path, "output column names must be unique");
    }
    Ok((name.to_string(), columns))
}

fn valid_view_name(name: &str) -> bool {
    name.starts_with("view_") && valid_identifier(name)
}

fn valid_identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn invalid<T>(path: &Path, reason: impl Into<String>) -> Result<T, ViewError> {
    Err(ViewError::InvalidDeclaration {
        path: path.to_path_buf(),
        reason: reason.into(),
    })
}

fn create_statement(definition: &ViewDefinition) -> String {
    let columns = definition
        .columns
        .iter()
        .map(|column| quote_identifier(column))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "CREATE VIEW {} ({columns}) AS\n{};",
        quote_identifier(&definition.name),
        definition.select_sql
    )
}

fn validate_definitions(
    connection: &Connection,
    definitions: &[ViewDefinition],
) -> Result<(), ViewError> {
    for definition in definitions {
        let query = format!(
            "SELECT * FROM {} LIMIT 0",
            quote_identifier(&definition.name)
        );
        let statement = connection
            .prepare(&query)
            .map_err(|source| ViewError::Validate {
                name: definition.name.clone(),
                path: definition.source_path.clone(),
                source,
            })?;
        let actual = statement.column_count();
        if actual != definition.columns.len() {
            return Err(ViewError::ColumnCountMismatch {
                name: definition.name.clone(),
                path: definition.source_path.clone(),
                declared: definition.columns.len(),
                actual,
            });
        }
    }
    Ok(())
}

fn audit_connection(
    connection: &Connection,
    definitions: &[ViewDefinition],
) -> Result<ViewAudit, ViewError> {
    let source_names = definitions
        .iter()
        .map(|definition| definition.name.clone())
        .collect::<BTreeSet<_>>();
    let mut statement = connection
        .prepare(
            "SELECT name
             FROM main.sqlite_schema
             WHERE type = 'view' AND name GLOB 'view_*'
             ORDER BY name",
        )
        .map_err(|source| ViewError::InspectDatabase { source })?;
    let database_names = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|source| ViewError::InspectDatabase { source })?
        .collect::<Result<BTreeSet<_>, _>>()
        .map_err(|source| ViewError::InspectDatabase { source })?;

    let source_only = source_names
        .difference(&database_names)
        .cloned()
        .collect::<Vec<_>>();
    if !source_only.is_empty() {
        return Err(ViewError::SourceOnlyViews { names: source_only });
    }
    validate_definitions(connection, definitions)?;
    Ok(ViewAudit {
        database_only: database_names.difference(&source_names).cloned().collect(),
    })
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn database_only_warning(name: &str, source_root: &Path) -> String {
    format!(
        "warning: database view `{name}` has no Marmot declaration\n\n\
         If the view is still intentional, restore its declaration under `{}`.\n\
         If it was intentionally removed, add a migration containing:\n\n    DROP VIEW IF EXISTS {};",
        source_root.join(VIEW_DIR).display(),
        quote_identifier(name)
    )
}

fn database_only_error_message(names: &[String], source_root: &Path) -> String {
    names
        .iter()
        .map(|name| database_only_warning(name, source_root).replacen("warning:", "error:", 1))
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_view(source_root: &Path, name: &str, declaration: &str, sql: &str) {
        let directory = source_root.join(VIEW_DIR);
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join(format!("{name}.sql")),
            format!("-- view: {declaration}\n{sql}\n"),
        )
        .unwrap();
    }

    #[test]
    fn discovers_explicit_view_contracts() {
        let temp = tempfile::tempdir().unwrap();
        write_view(
            temp.path(),
            "view_active_users",
            "view_active_users(id, display_name)",
            "SELECT id, name FROM users WHERE active = 1;",
        );

        let definitions = discover(temp.path()).unwrap();

        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].name, "view_active_users");
        assert_eq!(definitions[0].columns, ["id", "display_name"]);
        assert_eq!(
            definitions[0].select_sql,
            "SELECT id, name FROM users WHERE active = 1"
        );
    }

    #[test]
    fn reconciles_nested_views_without_creation_order_requirements() {
        let temp = tempfile::tempdir().unwrap();
        let source_root = temp.path().join("src");
        let database = temp.path().join("app.db");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, active INTEGER NOT NULL);",
            )
            .unwrap();
        drop(connection);
        write_view(
            &source_root,
            "view_active_user_names",
            "view_active_user_names(name)",
            "SELECT name FROM view_active_users",
        );
        write_view(
            &source_root,
            "view_active_users",
            "view_active_users(id, name)",
            "SELECT id, name FROM users WHERE active = 1",
        );

        reconcile_database(&database, &source_root).unwrap();

        let connection = Connection::open(&database).unwrap();
        connection
            .execute("INSERT INTO users (name, active) VALUES ('Lucy', 1)", [])
            .unwrap();
        let name: String = connection
            .query_row("SELECT name FROM view_active_user_names", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(name, "Lucy");
    }

    #[test]
    fn rolls_back_all_view_changes_when_final_graph_is_invalid() {
        let temp = tempfile::tempdir().unwrap();
        let source_root = temp.path().join("src");
        let database = temp.path().join("app.db");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE users (id INTEGER PRIMARY KEY);
                 CREATE VIEW view_users (id) AS SELECT id FROM users;",
            )
            .unwrap();
        drop(connection);
        write_view(
            &source_root,
            "view_users",
            "view_users(id)",
            "SELECT id FROM missing_users",
        );

        assert!(matches!(
            reconcile_database(&database, &source_root),
            Err(ViewError::Validate { .. })
        ));

        let connection = Connection::open(&database).unwrap();
        let sql: String = connection
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE name = 'view_users'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(sql.contains("SELECT id FROM users"));
    }

    #[test]
    fn rejects_cycles_and_output_column_count_mismatches() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("app.db");
        let source_root = temp.path().join("src");
        Connection::open(&database).unwrap();
        write_view(
            &source_root,
            "view_first",
            "view_first(id)",
            "SELECT id FROM view_second",
        );
        write_view(
            &source_root,
            "view_second",
            "view_second(id)",
            "SELECT id FROM view_first",
        );
        assert!(matches!(
            reconcile_database(&database, &source_root),
            Err(ViewError::Validate { .. })
        ));

        fs::remove_dir_all(source_root.join(VIEW_DIR)).unwrap();
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch("CREATE TABLE users (id INTEGER, name TEXT);")
            .unwrap();
        drop(connection);
        write_view(
            &source_root,
            "view_users",
            "view_users(id)",
            "SELECT id, name FROM users",
        );
        assert!(matches!(
            reconcile_database(&database, &source_root),
            Err(ViewError::Reconcile { .. })
                | Err(ViewError::Validate { .. })
                | Err(ViewError::ColumnCountMismatch { .. })
        ));
    }

    #[test]
    fn audits_stale_managed_views_without_dropping_them() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("app.db");
        let source_root = temp.path().join("src");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE VIEW view_old_memberships (id) AS SELECT 1;
                 CREATE VIEW legacy_memberships (id) AS SELECT 1;",
            )
            .unwrap();
        drop(connection);

        let audit = reconcile_database(&database, &source_root).unwrap();

        assert_eq!(audit.database_only, ["view_old_memberships"]);
        let message = audit.warnings(&source_root).join("\n");
        assert!(message.contains("restore its declaration under"));
        assert!(message.contains("DROP VIEW IF EXISTS \"view_old_memberships\";"));
        let connection = Connection::open(&database).unwrap();
        let exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name = 'view_old_memberships')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(exists);
    }

    #[test]
    fn audit_reports_declarations_that_were_not_installed() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("app.db");
        let source_root = temp.path().join("src");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch("CREATE TABLE users (id INTEGER);")
            .unwrap();
        drop(connection);
        write_view(
            &source_root,
            "view_users",
            "view_users(id)",
            "SELECT id FROM users",
        );

        assert!(matches!(
            audit_database(&database, &source_root),
            Err(ViewError::SourceOnlyViews { names }) if names == ["view_users"]
        ));
    }

    #[test]
    fn replaces_changed_views_and_keeps_removed_or_renamed_views_for_migrations() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("app.db");
        let source_root = temp.path().join("src");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, active INTEGER);
                 INSERT INTO users VALUES (1, 'Lucy', 1), (2, 'Mina', 0);",
            )
            .unwrap();
        drop(connection);
        write_view(
            &source_root,
            "view_selected_users",
            "view_selected_users(id, name)",
            "SELECT id, name FROM users WHERE active = 1",
        );
        reconcile_database(&database, &source_root).unwrap();

        write_view(
            &source_root,
            "view_selected_users",
            "view_selected_users(id, name)",
            "SELECT id, name FROM users WHERE active = 0",
        );
        reconcile_database(&database, &source_root).unwrap();
        let connection = Connection::open(&database).unwrap();
        let name: String = connection
            .query_row("SELECT name FROM view_selected_users", [], |row| row.get(0))
            .unwrap();
        assert_eq!(name, "Mina");
        drop(connection);

        fs::remove_file(source_root.join(VIEW_DIR).join("view_selected_users.sql")).unwrap();
        write_view(
            &source_root,
            "view_inactive_users",
            "view_inactive_users(id, name)",
            "SELECT id, name FROM users WHERE active = 0",
        );
        let audit = reconcile_database(&database, &source_root).unwrap();

        assert_eq!(audit.database_only, ["view_selected_users"]);
        let connection = Connection::open(&database).unwrap();
        let installed = connection
            .prepare(
                "SELECT name FROM sqlite_schema
                 WHERE type = 'view' AND name GLOB 'view_*'
                 ORDER BY name",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(installed, ["view_inactive_users", "view_selected_users"]);
    }

    #[test]
    fn rejects_schema_collisions_and_parameters() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("app.db");
        let source_root = temp.path().join("src");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch("CREATE TABLE view_users (id INTEGER);")
            .unwrap();
        drop(connection);
        write_view(&source_root, "view_users", "view_users(id)", "SELECT 1");
        assert!(matches!(
            reconcile_database(&database, &source_root),
            Err(ViewError::Reconcile { .. })
        ));

        fs::remove_file(source_root.join(VIEW_DIR).join("view_users.sql")).unwrap();
        write_view(
            &source_root,
            "view_parameterized",
            "view_parameterized(id)",
            "SELECT @id",
        );
        assert!(matches!(
            reconcile_database(&database, &source_root),
            Err(ViewError::Reconcile { .. })
        ));
    }

    #[test]
    fn emits_and_checks_the_disposable_aggregate() {
        let temp = tempfile::tempdir().unwrap();
        let source_root = temp.path().join("src");
        let output = source_root.join("generated/sql");
        write_view(
            &source_root,
            "view_users",
            "view_users(id)",
            "SELECT id FROM users",
        );
        let definitions = discover(&source_root).unwrap();

        emit_generated_sql(&definitions, &output, false).unwrap();
        emit_generated_sql(&definitions, &output, true).unwrap();
        let generated = fs::read_to_string(output.join(GENERATED_FILE)).unwrap();
        assert!(generated.contains("DROP VIEW IF EXISTS \"view_users\";"));
        assert!(generated.contains("CREATE VIEW \"view_users\" (\"id\") AS"));

        assert!(matches!(
            emit_generated_sql(&[], &output, true),
            Err(ViewError::StaleGeneratedFile { .. })
        ));
        assert!(output.join(GENERATED_FILE).exists());
        emit_generated_sql(&[], &output, false).unwrap();
        assert!(!output.join(GENERATED_FILE).exists());
    }

    #[test]
    fn rejects_invalid_names_filename_mismatches_and_duplicate_columns() {
        let temp = tempfile::tempdir().unwrap();
        write_view(temp.path(), "memberships", "memberships(id)", "SELECT 1");
        assert!(matches!(
            discover(temp.path()),
            Err(ViewError::InvalidDeclaration { .. })
        ));

        fs::remove_dir_all(temp.path().join(VIEW_DIR)).unwrap();
        write_view(
            temp.path(),
            "view_memberships",
            "view_members(id)",
            "SELECT 1",
        );
        assert!(matches!(
            discover(temp.path()),
            Err(ViewError::FilenameMismatch { .. })
        ));

        fs::remove_dir_all(temp.path().join(VIEW_DIR)).unwrap();
        write_view(
            temp.path(),
            "view_memberships",
            "view_memberships(id, id)",
            "SELECT 1, 2",
        );
        assert!(matches!(
            discover(temp.path()),
            Err(ViewError::InvalidDeclaration { .. })
        ));
    }
}
