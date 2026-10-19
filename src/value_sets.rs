use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use heck::ToPascalCase;
use serde::Deserialize;
use syn::parse::Parser;

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueSet {
    pub name: String,
    pub rust_path: String,
    pub values: Vec<String>,
    pub columns: Vec<(String, String)>,
    pub rust_attributes: Vec<String>,
    pub declaration_path: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Declaration {
    module: Option<String>,
    #[serde(default)]
    imports: Vec<String>,
    #[serde(default)]
    sets: BTreeMap<String, Set>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Set {
    values: Vec<String>,
    columns: Vec<String>,
    #[serde(default)]
    rust_attributes: Vec<String>,
    #[serde(default)]
    rust: RustOptions,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RustOptions {
    #[serde(default)]
    visibility: Visibility,
    as_str_visibility: Option<Visibility>,
    #[serde(default)]
    emission: Emission,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Deserialize)]
enum Visibility {
    #[default]
    #[serde(rename = "pub")]
    Public,
    #[serde(rename = "pub(crate)")]
    Crate,
}

impl Visibility {
    fn source(self) -> &'static str {
        match self {
            Self::Public => "pub",
            Self::Crate => "pub(crate)",
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Emission {
    #[default]
    Always,
    Test,
    None,
}

impl RustOptions {
    fn validate(self, name: &str) -> Result<Self> {
        if self.visibility == Visibility::Crate
            && self.as_str_visibility == Some(Visibility::Public)
        {
            return Err(invalid(format!(
                "{name}: as_str visibility cannot exceed enum visibility"
            )));
        }
        Ok(self)
    }

    fn cfg(self) -> &'static str {
        if self.emission == Emission::Test {
            "#[cfg(test)]\n"
        } else {
            ""
        }
    }
}

pub(crate) fn invalid(reason: impl Into<String>) -> Error {
    Error::InvalidValueSet {
        reason: reason.into(),
    }
}

#[derive(Default)]
pub(crate) struct Registry {
    pub sets: Vec<ValueSet>,
    pub owned: Vec<ValueSet>,
    pub inputs: BTreeSet<PathBuf>,
    rust: BTreeMap<String, RustOptions>,
}

impl Registry {
    pub(crate) fn validate_rust_bindings(&self, project: &crate::model::Project) -> Result<()> {
        for query in project.queries.iter().chain(
            project
                .scoped_ids
                .iter()
                .flat_map(|definition| definition.gates.iter().map(|gate| &gate.query)),
        ) {
            for value_type in query
                .parameters
                .iter()
                .map(|parameter| &parameter.column_type)
                .chain(query.columns.iter().map(|column| &column.column_type))
            {
                if let crate::model::ValueType::ValueSet(set) = value_type
                    && let Some(options) = self.rust.get(&set.rust_path)
                    && options.emission != Emission::Always
                {
                    let emission = if options.emission == Emission::Test {
                        "test"
                    } else {
                        "none"
                    };
                    return Err(invalid(format!(
                        "{}: query {} requires {} with Rust emission {emission}; generated parameters/results require always",
                        set.declaration_path.display(),
                        query.name,
                        set.name,
                    )));
                }
            }
        }
        Ok(())
    }
}

fn read(path: &Path) -> Result<Declaration> {
    let source = fs::read_to_string(path).map_err(|source| Error::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&source).map_err(|error| invalid(format!("{}: {error}", path.display())))
}

pub fn input_paths(source_root: &Path) -> Result<BTreeSet<PathBuf>> {
    Ok(load(source_root)?.inputs)
}

pub(crate) fn load(source_root: &Path) -> Result<Registry> {
    let path = source_root.join("value_sets.toml");
    let mut registry = Registry::default();
    registry.inputs.insert(path.clone());
    if !path.try_exists().map_err(|source| Error::ReadFile {
        path: path.clone(),
        source,
    })? {
        return Ok(registry);
    }
    let declaration = read(&path)?;
    let mut names = BTreeSet::new();
    let mut columns = BTreeSet::new();
    let mut homes = Vec::new();
    for import in &declaration.imports {
        let home_path = source_root.join(import);
        let home = read(&home_path)?;
        if !home.imports.is_empty() {
            return Err(invalid(format!(
                "{}: import home registries directly; transitive imports are not supported",
                home_path.display()
            )));
        }
        homes.push((home_path, home, false));
    }
    homes.push((path, declaration, true));
    for (home_path, home, owned) in homes {
        registry.inputs.insert(home_path.clone());
        let sets = home_sets(&home, &home_path, !owned)
            .map_err(|error| invalid(format!("{}: {error}", home_path.display())))?;
        for set in sets {
            if !names.insert(set.name.clone()) {
                return Err(invalid(format!(
                    "{}: duplicate set name {}",
                    home_path.display(),
                    set.name
                )));
            }
            for column in &set.columns {
                if !columns.insert(column.clone()) {
                    return Err(invalid(format!(
                        "{}: duplicate binding for {}.{}",
                        home_path.display(),
                        column.0,
                        column.1
                    )));
                }
            }
            let rust = home.sets[&set.name]
                .rust
                .validate(&set.name)
                .map_err(|error| invalid(format!("{}: {error}", home_path.display())))?;
            registry.rust.insert(set.rust_path.clone(), rust);
            if owned {
                registry.owned.push(set.clone());
            }
            registry.sets.push(set);
        }
    }
    Ok(registry)
}

fn home_sets(
    declaration: &Declaration,
    declaration_path: &Path,
    imported: bool,
) -> Result<Vec<ValueSet>> {
    if declaration.sets.is_empty() {
        if imported || declaration.module.is_some() {
            return Err(invalid("a home registry requires nonempty sets"));
        }
        return Ok(vec![]);
    }
    let module_path = declaration
        .module
        .as_ref()
        .ok_or_else(|| invalid("sets require a module"))?;
    let module = syn::parse_str::<syn::Path>(module_path)
        .map_err(|error| invalid(format!("invalid module path: {error}")))?;
    if module.leading_colon.is_some()
        || module
            .segments
            .iter()
            .any(|part| !matches!(part.arguments, syn::PathArguments::None))
        || module.segments.len() < 2
        || matches!(
            module.segments.first().unwrap().ident.to_string().as_str(),
            "self" | "super"
        )
        || (imported && module.segments.first().unwrap().ident == "crate")
    {
        return Err(invalid(
            "module must use an absolute crate path; imported homes cannot use crate::",
        ));
    }
    let mut sets = Vec::new();
    for (name, set) in &declaration.sets {
        syn::parse_str::<syn::Ident>(name)
            .map_err(|_| invalid(format!("invalid Rust enum name {name}")))?;
        if set.values.is_empty() || set.columns.is_empty() {
            return Err(invalid(format!(
                "{name} requires nonempty values and columns"
            )));
        }
        for attribute in &set.rust_attributes {
            let parsed = syn::Attribute::parse_outer
                .parse_str(attribute)
                .map_err(|error| {
                    invalid(format!(
                        "{name}: invalid Rust attribute {attribute:?}: {error}"
                    ))
                })?;
            if parsed.len() != 1 {
                return Err(invalid(format!(
                    "{name}: expected exactly one outer Rust attribute, got {attribute:?}"
                )));
            }
        }
        let mut variants = BTreeSet::new();
        for value in &set.values {
            let variant = value.to_pascal_case();
            syn::parse_str::<syn::Ident>(&variant)
                .map_err(|_| invalid(format!("{name}: invalid variant for {value:?}")))?;
            if !variants.insert(variant) {
                return Err(invalid(format!(
                    "{name}: duplicate value or variant for {value:?}"
                )));
            }
        }
        let mut columns = Vec::new();
        for column in &set.columns {
            let parts = column.split('.').collect::<Vec<_>>();
            if parts.len() != 2 || parts.iter().any(|part| part.is_empty()) {
                return Err(invalid(format!("expected table.column, got {column:?}")));
            }
            let pair = (parts[0].to_ascii_lowercase(), parts[1].to_ascii_lowercase());
            columns.push(pair);
        }
        sets.push(ValueSet {
            rust_path: format!("{module_path}::{name}"),
            name: name.clone(),
            values: set.values.clone(),
            columns,
            rust_attributes: set.rust_attributes.clone(),
            declaration_path: declaration_path.to_path_buf(),
        });
    }
    Ok(sets)
}

pub(crate) fn render(registry: &Registry) -> String {
    let mut output =
        "// @generated by Marmot from value_sets.toml. Do not edit by hand.\n".to_string();
    for set in &registry.owned {
        let options = registry.rust[&set.rust_path];
        if options.emission == Emission::None {
            continue;
        }
        let cfg = options.cfg();
        let visibility = options.visibility.source();
        let as_str_visibility = options
            .as_str_visibility
            .unwrap_or(options.visibility)
            .source();
        let name = &set.name;
        output.push_str(cfg);
        for attribute in &set.rust_attributes {
            output.push_str(attribute);
            output.push('\n');
        }
        output.push_str(&format!(
            "#[derive(Debug, Clone, Copy, PartialEq, Eq)]\n{visibility} enum {name} {{\n"
        ));
        for value in &set.values {
            output.push_str(&format!("{},\n", value.to_pascal_case()));
        }
        output.push_str(&format!(
            "}}\n{cfg}impl {name} {{ {as_str_visibility} fn as_str(self) -> &'static str {{ match self {{\n"
        ));
        for value in &set.values {
            output.push_str(&format!("Self::{} => {value:?},\n", value.to_pascal_case()));
        }
        output.push_str("}}}\n");
        output.push_str(&format!("{cfg}impl rusqlite::types::ToSql for {name} {{ fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {{ Ok(self.as_str().into()) }} }}\n"));
        output.push_str(&format!("{cfg}impl rusqlite::types::FromSql for {name} {{ fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {{ match value.as_str()? {{\n"));
        for value in &set.values {
            output.push_str(&format!(
                "{value:?} => Ok(Self::{}),\n",
                value.to_pascal_case()
            ));
        }
        output.push_str(&format!("other => Err(rusqlite::types::FromSqlError::Other(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, format!(\"unknown {name} value {{other:?}}\"))))),\n}} }} }}\n"));
    }
    output
}
