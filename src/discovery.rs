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

        let stem = path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("query");
        let Some(query_name) = query_name_from_filename(stem) else {
            continue;
        };
        let module_name = configured_sql_module_name(sql_dir, path);

        files.push(SqlFile {
            path: path.to_path_buf(),
            module_name,
            query_name,
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

fn configured_sql_module_name(sql_dir: &Path, path: &Path) -> String {
    let parent = path.parent().unwrap_or(sql_dir);
    let relative_parent = parent.strip_prefix(sql_dir).unwrap_or(parent);
    let owner_dir = relative_parent
        .components()
        .next_back()
        .and_then(|component| component.as_os_str().to_str())
        .filter(|name| !name.is_empty())
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
            .map(|file| (file.module_name.as_str(), file.query_name.as_str()))
            .collect::<Vec<_>>();

        assert_eq!(
            modules,
            [
                ("articles_sql", "get_articles"),
                ("sql", "get_settings"),
                ("likes_sql", "get_likes"),
            ]
        );
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
        assert_eq!(files[0].query_name, "get_likes");
    }
}
