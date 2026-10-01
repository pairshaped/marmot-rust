use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, LitStr, Path, parse_macro_input};

#[proc_macro_derive(FromSqlRow, attributes(from_sql))]
pub fn derive_from_sql_row(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let sources = source_paths(&input)?;
    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "FromSqlRow can only be derived for structs",
        ));
    };
    let Fields::Named(fields) = &data.fields else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "FromSqlRow requires named struct fields",
        ));
    };

    let assignments = fields
        .named
        .iter()
        .map(|field| {
            let name = field.ident.as_ref().expect("named field");
            let mut mapping = None;
            for attribute in &field.attrs {
                if !attribute.path().is_ident("from_sql") {
                    continue;
                }
                attribute.parse_nested_meta(|meta| {
                    let kind = if meta.path.is_ident("fnmap") {
                        "fnmap"
                    } else if meta.path.is_ident("func") {
                        "func"
                    } else {
                        return Err(meta.error("expected `fnmap = \"path\"` or `func = \"path\"`"));
                    };
                    let function = meta.value()?.parse::<LitStr>()?.parse::<Path>()?;
                    if mapping.replace((kind, function)).is_some() {
                        return Err(meta.error("a field can have only one from_sql mapping"));
                    }
                    Ok(())
                })?;
            }
            Ok(match mapping {
                Some(("fnmap", function)) => quote!(#name: #function(&row)),
                Some(("func", function)) => quote!(#name: #function()),
                Some(_) => unreachable!("mapping kinds are validated"),
                None => quote!(#name: row.#name),
            })
        })
        .collect::<syn::Result<Vec<_>>>()?;
    let destination = &input.ident;
    let generics = &input.generics;
    let (impl_generics, type_generics, where_clause) = generics.split_for_impl();

    let implementations = sources.iter().map(|source| {
        quote! {
            impl #impl_generics From<#source> for #destination #type_generics #where_clause {
                fn from(row: #source) -> Self {
                    Self { #(#assignments,)* }
                }
            }
        }
    });
    Ok(quote!(#(#implementations)*))
}

fn source_paths(input: &DeriveInput) -> syn::Result<Vec<Path>> {
    let attributes = input
        .attrs
        .iter()
        .filter(|attribute| attribute.path().is_ident("from_sql"))
        .collect::<Vec<_>>();
    if attributes.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "FromSqlRow requires at least one #[from_sql(path::ToRow)] attribute",
        ));
    }
    attributes
        .into_iter()
        .map(|attribute| attribute.parse_args::<Path>())
        .collect()
}
