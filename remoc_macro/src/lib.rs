#![forbid(unsafe_code)]

//! Procedural macros for Remoc.

use quote::quote;
use syn::{meta, parse_macro_input};

mod assoc_type;
mod method;
mod trait_def;
mod util;

use crate::trait_def::TraitDef;

#[proc_macro_attribute]
pub fn remote(args: proc_macro::TokenStream, input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let mut trait_def = parse_macro_input!(input as TraitDef);
    let meta_parser = meta::parser(|meta| trait_def.parse_meta(meta));
    parse_macro_input!(args with meta_parser);

    let vanilla_trait = trait_def.vanilla_trait();
    let request_enums = trait_def.request_enums();
    let servers = match trait_def.servers() {
        Ok(servers) => servers,
        Err(msg) => quote! { ::std::compile_error!(#msg); },
    };
    let client = trait_def.client();

    #[allow(clippy::let_and_return)]
    let output = proc_macro::TokenStream::from(quote! {
        #vanilla_trait
        #request_enums
        #servers
        #client
    });

    // println!("{}", &output);

    output
}
