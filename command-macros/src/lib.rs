use proc_macro::TokenStream;
use syn::{parse_macro_input, DeriveInput};

mod flags;
mod msg_command;
mod slash_command;
mod util;

/// Create a static SlashCommand `{uppercased_name}_SLASH`.
///
/// Make sure there is a function in scope with the signature
/// `async fn slash_{lowercased_name}(Arc<Context>, Box<ApplicationCommand>) -> BotResult<()>`
#[proc_macro_derive(SlashCommand, attributes(flags))]
pub fn slash_command(input: TokenStream) -> TokenStream {
    let derive_input = parse_macro_input!(input as DeriveInput);

    match slash_command::derive(derive_input) {
        Ok(result) => result.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// Attribute macro to define message commands.
///
/// The macro requires the name-value pair `name = "..."` to give the command a name.
///
/// An optional name-value pair is `dm_permission = true/false` to define whether
/// the command can be used in DMs.
///
/// ## Note
///
/// The command will always be defered so don't callback in the function body.
#[proc_macro_attribute]
pub fn msg_command(attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = parse_macro_input!(item as msg_command::Function);
    let attr = parse_macro_input!(attr as msg_command::Attributes);

    msg_command::impl_(attr, item)
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}
