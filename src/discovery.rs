use std::path::Path;

use heck::ToSnakeCase;
use walkdir::WalkDir;

use crate::error::{Error, Result};
use crate::model::{SqlFile, query_name_from_filename};

pub fn discover_sql_files(source_root: &Path) -> Result<Vec<SqlFile>> {
    discover_sql_files_with_sql_dir(source_root, None)
}

pub fn discover_sql_files_with_sql_dir(
    source_root: &Path,
    sql_dir: Option<&Path>,
) -> Result<Vec<SqlFile>> {
    match sql_dir {
        Some(sql_dir) => discover_configured_sql_files(sql_dir),
        None => discover_colocated_sql_files(source_root),
    }
}

fn discover_colocated_sql_files(source_root: &Path) -> Result<Vec<SqlFile>> {
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
        let parent_is_sql =
            path.parent().and_then(|parent| parent.file_name()) == Some("sql".as_ref());
        let stem = path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("query");
        let (module_name, query_name) = if parent_is_sql {
            let owner_dir = path
                .parent()
                .and_then(Path::parent)
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .unwrap_or("queries");
            let Some(query_name) = query_name_from_filename(stem) else {
                continue;
            };
            (
                format!("{}_sql", owner_dir.to_snake_case()),
                Some(query_name),
            )
        } else {
            let Some(module_name) = companion_module_name(path, stem) else {
                continue;
            };
            (module_name, None)
        };

        files.push(SqlFile {
            path: path.to_path_buf(),
            module_name,
            query_name,
        });
    }

    Ok(files)
}

fn discover_configured_sql_files(sql_dir: &Path) -> Result<Vec<SqlFile>> {
    validate_configured_sql_dir(sql_dir)?;

    let mut files = Vec::new();

    for entry in WalkDir::new(sql_dir).sort_by_file_name() {
        let entry = entry.map_err(|source| Error::WalkDir {
            path: sql_dir.to_path_buf(),
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

        let Some(query_name) = query_name_from_filename(
            path.file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or("query"),
        ) else {
            continue;
        };
        let module_name = configured_sql_module_name(sql_dir, path);

        files.push(SqlFile {
            path: path.to_path_buf(),
            module_name,
            query_name: Some(query_name),
        });
    }

    Ok(files)
}

fn validate_configured_sql_dir(sql_dir: &Path) -> Result<()> {
    match std::fs::metadata(sql_dir) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(Error::SqlPathNotDirectory {
            path: sql_dir.to_path_buf(),
        }),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            Err(Error::MissingSqlDirectory {
                path: sql_dir.to_path_buf(),
            })
        }
        Err(source) => Err(Error::ReadFile {
            path: sql_dir.to_path_buf(),
            source,
        }),
    }
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

fn configured_sql_module_name(sql_dir: &Path, path: &Path) -> String {
    let parent = path.parent().unwrap_or(sql_dir);
    let relative_parent = parent.strip_prefix(sql_dir).unwrap_or(parent);
    let owner_dir = relative_parent
        .components()
        .rev()
        .filter_map(|component| component.as_os_str().to_str())
        .find(|name| !name.is_empty() && *name != "sql")
        .unwrap_or("sql");

    if owner_dir == "sql" {
        "sql".to_string()
    } else {
        format!("{}_sql", owner_dir.to_snake_case())
    }
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
        assert_eq!(files[0].query_name, None);
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
    fn keeps_legacy_sql_directory_query_names() {
        let temp = tempfile::tempdir().unwrap();
        let sql_dir = temp.path().join("src/items/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(sql_dir.join("get_by_id.sql"), "select 1").unwrap();

        let files = discover_sql_files(&temp.path().join("src")).unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].module_name, "items_sql");
        assert_eq!(files[0].query_name.as_deref(), Some("get_by_id"));
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
        let generated_sql = temp.path().join("src/generated/sql");
        fs::create_dir_all(&items_sql).unwrap();
        fs::create_dir_all(&generated_sql).unwrap();
        fs::write(items_sql.join("find_user.rs"), "").unwrap();
        fs::write(items_sql.join("find_user.sql"), "-- func: query\nselect 1").unwrap();
        fs::write(generated_sql.join("stale.sql"), "select 1").unwrap();

        let files = discover_sql_files(&temp.path().join("src")).unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].module_name, "find_user_sql");
    }

    #[test]
    fn finds_sql_files_under_configured_sql_dir() {
        let temp = tempfile::tempdir().unwrap();
        let sql_dir = temp.path().join("src/sql");
        let likes_sql = sql_dir.join("likes");
        let articles_sql = sql_dir.join("articles");
        fs::create_dir_all(&likes_sql).unwrap();
        fs::create_dir_all(&articles_sql).unwrap();
        fs::write(sql_dir.join("get_settings.sql"), "select 1").unwrap();
        fs::write(likes_sql.join("get_likes.sql"), "select 1").unwrap();
        fs::write(articles_sql.join("get_articles.sql"), "select 1").unwrap();

        let files =
            discover_sql_files_with_sql_dir(&temp.path().join("src"), Some(&sql_dir)).unwrap();
        let modules = files
            .iter()
            .map(|file| (file.module_name.as_str(), file.query_name.as_deref()))
            .collect::<Vec<_>>();

        assert_eq!(
            modules,
            [
                ("articles_sql", Some("get_articles")),
                ("sql", Some("get_settings")),
                ("likes_sql", Some("get_likes")),
            ]
        );
    }

    #[test]
    fn configured_sql_dir_uses_nearest_non_sql_directory_for_nested_sql_dirs() {
        let temp = tempfile::tempdir().unwrap();
        let sql_dir = temp.path().join("src/sql");
        fs::create_dir_all(sql_dir.join("likes/sql")).unwrap();
        fs::write(sql_dir.join("likes/sql/get_likes.sql"), "select 1").unwrap();

        let files =
            discover_sql_files_with_sql_dir(&temp.path().join("src"), Some(&sql_dir)).unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].module_name, "likes_sql");
        assert_eq!(files[0].query_name.as_deref(), Some("get_likes"));
    }

    #[test]
    fn configured_sql_dir_ignores_empty_subdirectories() {
        let temp = tempfile::tempdir().unwrap();
        let sql_dir = temp.path().join("src/sql");
        fs::create_dir_all(sql_dir.join("empty")).unwrap();
        fs::create_dir_all(sql_dir.join("likes")).unwrap();
        fs::write(sql_dir.join("likes/get_likes.sql"), "select 1").unwrap();

        let files =
            discover_sql_files_with_sql_dir(&temp.path().join("src"), Some(&sql_dir)).unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].module_name, "likes_sql");
        assert_eq!(files[0].query_name.as_deref(), Some("get_likes"));
    }
}
