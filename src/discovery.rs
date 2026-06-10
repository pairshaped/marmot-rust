use std::path::Path;

use heck::ToSnakeCase;
use walkdir::WalkDir;

use crate::error::{Error, Result};
use crate::model::{SqlFile, query_name_from_filename};

pub fn discover_sql_files(source_root: &Path) -> Result<Vec<SqlFile>> {
    let mut files = Vec::new();

    for entry in WalkDir::new(source_root).sort_by_file_name() {
        let entry = entry.map_err(|source| Error::WalkDir {
            path: source_root.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path
            .components()
            .any(|component| component.as_os_str() == "generated")
        {
            continue;
        }
        if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("sql") {
            continue;
        }
        if path.parent().and_then(|parent| parent.file_name()) != Some("sql".as_ref()) {
            continue;
        }

        let owner_dir = path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .unwrap_or("queries");
        let stem = path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("query");
        let Some(query_name) = query_name_from_filename(stem) else {
            continue;
        };

        files.push(SqlFile {
            path: path.to_path_buf(),
            module_name: format!("{}_sql", owner_dir.to_snake_case()),
            query_name,
        });
    }

    Ok(files)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn finds_colocated_sql_files() {
        let temp = tempfile::tempdir().unwrap();
        let items_sql = temp.path().join("src/items/sql");
        fs::create_dir_all(&items_sql).unwrap();
        fs::write(items_sql.join("get_by_id.sql"), "select 1").unwrap();
        fs::write(temp.path().join("src/ignored.sql"), "select 1").unwrap();

        let files = discover_sql_files(&temp.path().join("src")).unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].module_name, "items_sql");
        assert_eq!(files[0].query_name, "get_by_id");
    }

    #[test]
    fn derives_query_names_like_gleam_marmot() {
        let temp = tempfile::tempdir().unwrap();
        let sql_dir = temp.path().join("src/items/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        for filename in [
            "get-users.sql",
            "1-get-users.sql",
            "my query.sql",
            "Find_User.sql",
            "find@user!.sql",
            "fix_sql_injection.sql",
            "sql_backup.sql",
        ] {
            fs::write(sql_dir.join(filename), "select 1").unwrap();
        }

        let names = discover_sql_files(&temp.path().join("src"))
            .unwrap()
            .into_iter()
            .map(|file| file.query_name)
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            [
                "_1_get_users",
                "find_user",
                "finduser",
                "fix_sql_injection",
                "get_users",
                "my_query",
                "sql_backup",
            ]
        );
    }

    #[test]
    fn ignores_sql_files_that_sanitize_to_empty_query_names() {
        let temp = tempfile::tempdir().unwrap();
        let sql_dir = temp.path().join("src/items/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(sql_dir.join("@#$.sql"), "select 1").unwrap();
        fs::write(sql_dir.join(".sql"), "select 1").unwrap();
        fs::write(sql_dir.join("find_user.sql"), "select 1").unwrap();

        let files = discover_sql_files(&temp.path().join("src")).unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].query_name, "find_user");
    }

    #[test]
    fn ignores_generated_sql_directories() {
        let temp = tempfile::tempdir().unwrap();
        let items_sql = temp.path().join("src/items/sql");
        let generated_sql = temp.path().join("src/generated/sql");
        fs::create_dir_all(&items_sql).unwrap();
        fs::create_dir_all(&generated_sql).unwrap();
        fs::write(items_sql.join("find_user.sql"), "select 1").unwrap();
        fs::write(generated_sql.join("stale.sql"), "select 1").unwrap();

        let files = discover_sql_files(&temp.path().join("src")).unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].module_name, "items_sql");
    }
}
