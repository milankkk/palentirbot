use proc_macro2::TokenStream;
use quote::quote;
use syn::{
    parse::{Parse, ParseStream},
    Attribute, Ident, Result, Token,
};

pub struct Flags {
    tokens: TokenStream,
}

impl Flags {
    pub fn into_tokens(self) -> TokenStream {
        let bits = self.tokens;

        quote! {
            crate::core::commands::CommandFlags::from_bits_retain(#bits)
        }
    }
}

pub fn parse_flags(attrs: &[Attribute]) -> Result<Flags> {
    let mut combined = quote!(0);

    for attr in attrs {
        let is_flags = attr.path.get_ident().is_some_and(|ident| ident == "flags");
        if is_flags {
            let parsed: Flags = attr.parse_args()?;
            let inner = parsed.tokens;
            combined.extend(quote!( | (#inner)));
        }
    }

    Ok(Flags { tokens: combined })
}

impl Parse for Flags {
    fn parse(input: ParseStream) -> Result<Self> {
        let mut tokens = quote!(0);

        loop {
            let flag = input.step(|cursor| {
                let message = if let Some((ident, rest)) = cursor.ident() {
                    if accept_as_flag(&ident) {
                        return Ok((ident, rest));
                    }

                    r#"expected "AUTHORITY", "EPHEMERAL", "ONLY_OWNER", or "SKIP_DEFER""#
                } else {
                    "expected identifier"
                };

                Err(cursor.error(message))
            })?;

            tokens.extend(quote!( | crate::core::commands::CommandFlags::#flag.bits()));

            if input.is_empty() {
                break;
            }

            input.parse::<Token![,]>()?;
        }

        Ok(Self { tokens })
    }
}

fn accept_as_flag(ident: &Ident) -> bool {
    matches!(
        ident.to_string().as_str(),
        "AUTHORITY" | "EPHEMERAL" | "ONLY_OWNER" | "SKIP_DEFER"
    )
}
