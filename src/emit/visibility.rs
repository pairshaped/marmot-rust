use std::collections::BTreeSet;

use crate::error::{Error, Result};

// The policy chooses exported generated declarations. It never exempts them
// from the consuming application's compiler-backed public-use audit.
pub(super) fn apply(source: &str, public: &BTreeSet<String>) -> Result<String> {
    let syntax = syn::parse_file(source).map_err(|error| Error::InvalidBindingVisibility {
        reason: error.to_string(),
    })?;
    let mut declarations = Vec::new();
    collect(&syntax.items, "", &mut declarations);
    let known = declarations
        .iter()
        .map(|(name, _)| name.clone())
        .collect::<BTreeSet<_>>();
    if let Some(unknown) = public.difference(&known).next() {
        return Err(Error::InvalidBindingVisibility {
            reason: format!("unknown generated declaration {unknown}"),
        });
    }
    let mut line_offsets = vec![0];
    line_offsets.extend(source.match_indices('\n').map(|(index, _)| index + 1));
    let mut edits = declarations
        .into_iter()
        .filter_map(|(name, token)| {
            if public.contains(&name) {
                return None;
            }
            let location = token.span.start();
            Some(line_offsets[location.line - 1] + location.column)
        })
        .collect::<Vec<_>>();
    edits.sort_unstable();
    let mut output = source.to_owned();
    for offset in edits.into_iter().rev() {
        if source.get(offset..offset + 3) != Some("pub") {
            return Err(Error::InvalidBindingVisibility {
                reason: "unexpected generated visibility span".into(),
            });
        }
        output.insert_str(offset + 3, "(crate)");
    }
    Ok(output)
}

fn declaration(
    name: String,
    visibility: &syn::Visibility,
    output: &mut Vec<(String, syn::token::Pub)>,
) {
    if let syn::Visibility::Public(token) = visibility {
        output.push((name, *token));
    }
}

fn collect(items: &[syn::Item], prefix: &str, output: &mut Vec<(String, syn::token::Pub)>) {
    for item in items {
        match item {
            syn::Item::Fn(item) => {
                declaration(format!("{prefix}{}", item.sig.ident), &item.vis, output)
            }
            syn::Item::Struct(item) => {
                let name = format!("{prefix}{}", item.ident);
                declaration(name.clone(), &item.vis, output);
                for (index, field) in item.fields.iter().enumerate() {
                    let field_name = field
                        .ident
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_else(|| index.to_string());
                    declaration(format!("{name}::{field_name}"), &field.vis, output);
                }
            }
            syn::Item::Enum(item) => {
                declaration(format!("{prefix}{}", item.ident), &item.vis, output)
            }
            syn::Item::Trait(item) => {
                declaration(format!("{prefix}{}", item.ident), &item.vis, output)
            }
            syn::Item::Type(item) => {
                declaration(format!("{prefix}{}", item.ident), &item.vis, output)
            }
            syn::Item::Const(item) => {
                declaration(format!("{prefix}{}", item.ident), &item.vis, output)
            }
            syn::Item::Static(item) => {
                declaration(format!("{prefix}{}", item.ident), &item.vis, output)
            }
            syn::Item::Mod(item) => {
                declaration(format!("{prefix}{}", item.ident), &item.vis, output);
                if let Some((_, items)) = &item.content {
                    collect(items, &format!("{prefix}{}::", item.ident), output);
                }
            }
            syn::Item::Use(_) | syn::Item::Impl(_) => (),
            _ => (),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_exports_are_explicit_and_missing_names_refuse() {
        let source = "pub struct Params { pub id: i64, pub secret: String }\npub fn read(_: Params) {}\npub fn internal() {}\nmod sealed {pub trait Sealed {}}\n";
        let selected = BTreeSet::from(["Params".into(), "Params::id".into(), "read".into()]);
        let output = apply(source, &selected).unwrap();
        assert!(output.contains("pub struct Params { pub id: i64, pub(crate) secret: String }"));
        assert!(output.contains("pub fn read("));
        assert!(output.contains("pub(crate) fn internal()"));
        assert!(output.contains("pub(crate) trait Sealed"));
        assert!(apply(source, &BTreeSet::from(["typo".into()])).is_err());
    }

    #[test]
    fn empty_export_policy_keeps_generated_items_inside_owner() {
        let output = apply(
            "pub struct Row { pub value: i64 }\npub fn read() {}",
            &BTreeSet::new(),
        )
        .unwrap();
        assert!(output.contains("pub(crate) struct Row { pub(crate) value: i64 }"));
        assert!(output.contains("pub(crate) fn read()"));
        syn::parse_file(&output).unwrap();
    }
}
