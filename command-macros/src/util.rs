use quote::format_ident;
use syn::Ident;

pub trait IdentExt: Sized {
    fn to_uppercase(&self) -> Self;
}

impl IdentExt for Ident {
    fn to_uppercase(&self) -> Self {
        format_ident!("{}", self.to_string().to_uppercase())
    }
}
