use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::model::{
    ConnectionAccess, Query, ReturnType, ScalarCardinality, ScopedIdType, ValueType,
};
use crate::sqlite::blocks::{SqlBlock, parse_sql_blocks};

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    module: Option<String>,
    #[serde(default)]
    imports: Vec<String>,
    #[serde(default)]
    ids: BTreeMap<String, Declaration>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Declaration {
    gates: Vec<String>,
}

pub(crate) struct Id {
    pub id_type: ScopedIdType,
    pub gates: Vec<GateSource>,
}

pub(crate) struct GateSource {
    pub path: PathBuf,
    pub function: String,
}

#[derive(Default)]
pub(crate) struct Registry {
    pub ids: BTreeMap<String, Id>,
    pub owned: Vec<String>,
    pub inputs: BTreeSet<PathBuf>,
}

pub(crate) fn invalid(reason: impl Into<String>) -> Error {
    Error::InvalidScope {
        reason: reason.into(),
    }
}

fn read(path: &Path) -> Result<Manifest> {
    let source = fs::read_to_string(path).map_err(|source| Error::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&source).map_err(|error| invalid(format!("{}: {error}", path.display())))
}

pub(crate) fn load(source_root: &Path) -> Result<Registry> {
    let path = source_root.join("scoped_ids.toml");
    let mut registry = Registry::default();
    registry.inputs.insert(path.clone());
    if !path.try_exists().map_err(|source| Error::ReadFile {
        path: path.clone(),
        source,
    })? {
        return Ok(registry);
    }
    let manifest = read(&path)?;
    for import in &manifest.imports {
        let imported_path = normalize(&source_root.join(import));
        let imported = read(&imported_path)?;
        if !imported.imports.is_empty() {
            return Err(invalid(
                "import home registries directly; transitive imports are not supported",
            ));
        }
        if imported.ids.is_empty() {
            return Err(invalid("an imported home registry must define IDs"));
        }
        registry.inputs.insert(imported_path.clone());
        registry.add_home(&imported_path, &imported, false)?;
    }
    registry.add_home(&path, &manifest, true)?;
    for id in registry.ids.values() {
        for gate in &id.gates {
            let sql = fs::read_to_string(&gate.path).map_err(|source| Error::ReadFile {
                path: gate.path.clone(),
                source,
            })?;
            let blocks = parse_sql_blocks(&sql).map_err(|reason| Error::InvalidSqlBlock {
                path: gate.path.clone(),
                reason,
            })?;
            let block = blocks
                .iter()
                .find(|block| block.function_name == gate.function)
                .ok_or_else(|| invalid(format!("missing registered gate {}", gate.function)))?;
            if !block
                .scope
                .produces
                .as_ref()
                .is_some_and(|binding| binding.rust_path == id.id_type.rust_path)
            {
                return Err(invalid(format!(
                    "registered gate {} does not produce {}",
                    gate.function, id.id_type.rust_path
                )));
            }
        }
    }
    Ok(registry)
}

impl Registry {
    fn add_home(&mut self, path: &Path, manifest: &Manifest, owned: bool) -> Result<()> {
        if manifest.ids.is_empty() {
            if manifest.module.is_some() {
                return Err(invalid("a module requires at least one ID declaration"));
            }
            return Ok(());
        }
        let module = manifest
            .module
            .as_ref()
            .ok_or_else(|| invalid("ID declarations require module"))?;
        let parsed =
            syn::parse_str::<syn::Path>(module).map_err(|_| invalid("invalid home module path"))?;
        if parsed.segments.len() < 2
            || parsed.leading_colon.is_some()
            || parsed
                .segments
                .iter()
                .any(|segment| !matches!(segment.arguments, syn::PathArguments::None))
            || matches!(
                parsed.segments.first().unwrap().ident.to_string().as_str(),
                "crate" | "self" | "super"
            )
        {
            return Err(invalid("home module must use its absolute crate path"));
        }
        for (name, declaration) in &manifest.ids {
            syn::parse_str::<syn::Ident>(name)
                .map_err(|_| invalid(format!("invalid scoped type name {name}")))?;
            if declaration.gates.is_empty() {
                return Err(invalid(format!("{name} requires at least one gate")));
            }
            let rust_path = format!("{module}::{name}");
            if self.ids.contains_key(&rust_path) {
                return Err(invalid(format!("duplicate scoped type {rust_path}")));
            }
            let mut names = BTreeSet::new();
            let mut gates = Vec::new();
            for gate in &declaration.gates {
                let (file, function) = gate
                    .rsplit_once('#')
                    .ok_or_else(|| invalid("gate requires companion.sql#function"))?;
                syn::parse_str::<syn::Ident>(function)
                    .map_err(|_| invalid("invalid gate function name"))?;
                if matches!(function, "get" | "check_connection") {
                    return Err(invalid(format!(
                        "gate name {function} conflicts with the scoped-ID API"
                    )));
                }
                if !names.insert(function) {
                    return Err(invalid(format!(
                        "duplicate gate name {function} for {name}"
                    )));
                }
                let source = normalize(&path.parent().unwrap().join(file));
                if source
                    .extension()
                    .is_none_or(|extension| extension != "sql")
                    || !source.is_file()
                    || !source.with_extension("rs").is_file()
                {
                    return Err(invalid(format!(
                        "gate requires an existing SQL/Rust companion pair: {}",
                        source.display()
                    )));
                }
                self.inputs.insert(source.clone());
                gates.push(GateSource {
                    path: source,
                    function: function.to_string(),
                });
            }
            self.ids.insert(
                rust_path.clone(),
                Id {
                    id_type: ScopedIdType {
                        rust_type: format!("{rust_path}<'a>"),
                        rust_path: rust_path.clone(),
                        name: name.clone(),
                    },
                    gates,
                },
            );
            if owned {
                self.owned.push(rust_path);
            }
        }
        Ok(())
    }

    pub fn apply(&self, block: &SqlBlock, query: &mut Query) -> Result<Option<usize>> {
        if block.scope.produces.is_some() && !block.scope.consumes.is_empty() {
            return Err(invalid(
                "a producing gate must establish its own authorization; consuming scoped IDs in a producer is unsupported",
            ));
        }
        for binding in &block.scope.consumes {
            let id = self.ids.get(&binding.rust_path).ok_or_else(|| {
                invalid(format!(
                    "unknown scoped type {}; import its home registry",
                    binding.rust_path
                ))
            })?;
            let parameter = query
                .parameters
                .iter_mut()
                .find(|parameter| {
                    parameter.name == crate::model::sanitize_identifier(&binding.name)
                })
                .ok_or_else(|| {
                    invalid(format!(
                        "{}: unknown scoped parameter {}",
                        query.name, binding.name
                    ))
                })?;
            if parameter.column_type != ValueType::I64 || parameter.nullable {
                return Err(invalid(format!(
                    "{}: scoped parameter {} must be a nonnullable INTEGER",
                    query.name, binding.name
                )));
            }
            parameter.column_type = ValueType::ScopedId(id.id_type.clone());
        }
        let Some(binding) = &block.scope.produces else {
            return Ok(None);
        };
        let id = self.ids.get(&binding.rust_path).ok_or_else(|| {
            invalid(format!(
                "unknown scoped type {}; import its home registry",
                binding.rust_path
            ))
        })?;
        let registered = id
            .gates
            .iter()
            .any(|gate| gate.function == query.name && same_file(&gate.path, &query.source_path));
        if !registered {
            return Err(invalid(format!(
                "{} is not a registered producer for {}",
                query.name, binding.rust_path
            )));
        }
        if query.connection_access != ConnectionAccess::Read
            || query.return_type != ReturnType::Rows
            || query.emit_batch
            || query.scalar_cardinality != ScalarCardinality::Many
            || query.column_substitution.is_some()
        {
            return Err(invalid(
                "scope production requires an ordinary read-only row query",
            ));
        }
        let index = query
            .columns
            .iter()
            .position(|column| column.name.eq_ignore_ascii_case(&binding.name))
            .ok_or_else(|| {
                invalid(format!(
                    "{} has no result column {}",
                    query.name, binding.name
                ))
            })?;
        let column = &mut query.columns[index];
        if column.column_type != ValueType::I64 || column.nullable {
            return Err(invalid(format!(
                "{}: produced ID must be a nonnullable INTEGER",
                query.name
            )));
        }
        column.column_type = ValueType::ScopedId(id.id_type.clone());
        Ok(Some(index))
    }
}

fn same_file(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => (),
            Component::ParentDir => {
                if result.file_name().is_some_and(|name| name != "..") {
                    result.pop();
                } else {
                    result.push(component.as_os_str());
                }
            }
            _ => result.push(component.as_os_str()),
        }
    }
    result
}

/// Files that participate in an owner's scoped-ID generation contract.
pub fn input_paths(source_root: &Path) -> Result<BTreeSet<PathBuf>> {
    Ok(load(source_root)?.inputs)
}
