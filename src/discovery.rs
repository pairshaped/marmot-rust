use std::path::Path;

use heck::ToSnakeCase;
use walkdir::WalkDir;

use crate::error::{Error, Result};
use crate::model::SqlFile;

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
        let stem = path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("query");
        if path.parent().and_then(|parent| parent.file_name()) == Some("sql".as_ref()) {
            return Err(Error::SqlDirectoryQueryFile {
                path: path.to_path_buf(),
            });
        }
        let Some(module_name) = companion_module_name(path, stem) else {
            continue;
        };

        files.push(SqlFile {
            path: path.to_path_buf(),
            module_name,
        });
    }

    Ok(files)
}

fn companion_module_name(path: &Path, stem: &str) -> Option<String> {
    let parent = path.parent()?;
    if stem == "mod" && parent.join("mod.rs").exists() {
        let owner_dir = parent.file_name().and_then(|name| name.to_str())?;
        return Some(format!("{}_sql", owner_dir.to_snake_case()));
    }

    let rust_module = parent.join(format!("{stem}.rs"));
    if rust_module.exists() {
        return Some(format!("{}_sql", stem.to_snake_case()));
    }

    None
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn finds_colocated_sql_companion_files() {
        let temp = tempfile::tempdir().unwrap();
        let source_root = temp.path().join("src/items");
        fs::create_dir_all(&source_root).unwrap();
        fs::write(source_root.join("show.rs"), "").unwrap();
        fs::write(source_root.join("show.sql"), "-- func: get_by_id\nselect 1").unwrap();
        fs::write(temp.path().join("src/ignored.sql"), "select 1").unwrap();

        let files = discover_sql_files(&temp.path().join("src")).unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].module_name, "show_sql");
    }

    #[test]
    fn derives_module_names_from_companion_filenames() {
        let temp = tempfile::tempdir().unwrap();
        let source_root = temp.path().join("src/items");
        fs::create_dir_all(&source_root).unwrap();
        for filename in [
            "get-users.sql",
            "Find_User.sql",
            "fix_sql_injection.sql",
            "sql_backup.sql",
        ] {
            let stem = filename.trim_end_matches(".sql");
            fs::write(source_root.join(format!("{stem}.rs")), "").unwrap();
            fs::write(source_root.join(filename), "-- func: query\nselect 1").unwrap();
        }

        let names = discover_sql_files(&temp.path().join("src"))
            .unwrap()
            .into_iter()
            .map(|file| file.module_name)
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            [
                "find_user_sql",
                "fix_sql_injection_sql",
                "get_users_sql",
                "sql_backup_sql",
            ]
        );
    }

    #[test]
    fn rejects_sql_directory_query_files() {
        let temp = tempfile::tempdir().unwrap();
        let forbidden_sql_dir = temp.path().join("src/items/sql");
        fs::create_dir_all(&forbidden_sql_dir).unwrap();
        fs::write(forbidden_sql_dir.join("get_by_id.sql"), "select 1").unwrap();

        let error = discover_sql_files(&temp.path().join("src")).unwrap_err();

        assert!(matches!(error, Error::SqlDirectoryQueryFile { .. }));
    }

    #[test]
    fn maps_mod_sql_to_owning_directory_module() {
        let temp = tempfile::tempdir().unwrap();
        let source_root = temp.path().join("src/items");
        fs::create_dir_all(&source_root).unwrap();
        fs::write(source_root.join("mod.rs"), "").unwrap();
        fs::write(source_root.join("mod.sql"), "-- func: query\nselect 1").unwrap();
        fs::write(source_root.join("find_user.rs"), "").unwrap();
        fs::write(
            source_root.join("find_user.sql"),
            "-- func: query\nselect 1",
        )
        .unwrap();

        let files = discover_sql_files(&temp.path().join("src")).unwrap();

        let modules = files
            .iter()
            .map(|file| file.module_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(modules, ["find_user_sql", "items_sql"]);
    }

    #[test]
    fn ignores_generated_sql_directories() {
        let temp = tempfile::tempdir().unwrap();
        let items_sql = temp.path().join("src/items");
        let generated_sql = temp.path().join("src/generated");
        fs::create_dir_all(&items_sql).unwrap();
        fs::create_dir_all(&generated_sql).unwrap();
        fs::write(items_sql.join("find_user.rs"), "").unwrap();
        fs::write(items_sql.join("find_user.sql"), "-- func: query\nselect 1").unwrap();
        fs::write(generated_sql.join("stale.sql"), "select 1").unwrap();

        let files = discover_sql_files(&temp.path().join("src")).unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].module_name, "find_user_sql");
    }
}
