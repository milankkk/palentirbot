use proc_macro2::{Ident, TokenStream};
use quote::{quote, ToTokens, TokenStreamExt};
use syn::{
    punctuated::Punctuated, token::Comma, Data, DeriveInput, Error, Field, Fields, Result,
    Visibility,
};

pub use self::attributes::AttributeList;

use self::attributes::Attributes;

mod attributes;

pub fn impl_(input: DeriveInput, list: AttributeList) -> Result<TokenStream> {
    let DeriveInput {
        attrs,
        vis,
        ident,
        generics,
        data,
    } = input;

    let data = match data {
        Data::Struct(s) => s,
        Data::Enum(e) => {
            let message = "`IsPagination` can only be derived for structs";

            return Err(Error::new(e.enum_token.span, message));
        }
        Data::Union(u) => {
            let message = "`IsPagination` can only be derived for structs";

            return Err(Error::new(u.union_token.span, message));
        }
    };

    let named_fields = match data.fields {
        Fields::Named(n) => n.named,
        _ => {
            let message = "Deriving `IsPagination` requires named fields";

            return Err(Error::new(ident.span(), message));
        }
    };

    let mut name = ident.to_string();

    if !name.ends_with("Pagination") || name.len() == "Pagination".len() {
        let message = "`IsPagination` can only be derived for \
            structs with a non-empty prefix suffixed with `Pagination`, e.g. `PingPagination`";

        return Err(Error::new(ident.span(), message));
    }

    name.truncate(name.len() - "Pagination".len());
    let variant = Ident::new(&name, ident.span());

    let Attributes { per_page, entries } = Attributes::try_from(list)?;

    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    let param_fields: Punctuated<Field, Comma> = named_fields
        .iter()
        .cloned()
        .map(|mut field| {
            field.vis = Visibility::Inherited;
            field.attrs.clear();
            field
        })
        .collect();
    let token_fields = FieldNames::new(&named_fields);

    let tokens = quote! {
        #(#attrs)*
        #vis struct #ident #generics {
            #named_fields
        }

        impl #impl_generics #ident #ty_generics #where_clause {
            #[allow(clippy::too_many_arguments)]
            pub fn builder( #param_fields ) -> crate::pagination::PaginationBuilder {
                let pages = crate::pagination::Pages::new(#per_page, #entries);
                let kind = crate::pagination::PaginationKind:: #variant (Box::new(Self { #token_fields }));

                crate::pagination::PaginationBuilder::new(kind, pages)
            }
        }
    };

    Ok(tokens)
}

struct FieldNames<'a> {
    named_fields: &'a Punctuated<Field, Comma>,
}

impl<'a> FieldNames<'a> {
    fn new(named_fields: &'a Punctuated<Field, Comma>) -> Self {
        Self { named_fields }
    }
}

impl ToTokens for FieldNames<'_> {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        let fields = self.named_fields.iter().map(|field| &field.ident);

        tokens.append_separated(fields, quote!(,))
    }
}
