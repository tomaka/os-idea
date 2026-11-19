// Copyright 2026  Pierre Krieger

extern crate proc_macro;

/// Converts a string literal in the WAT format into an array expression containing the bytes
/// of the corresponding Wasm, e.g. `[0u8, 97u8, 115u8, 109u8, ...]`.
///
/// Fails to compile if the WAT is invalid.
#[proc_macro]
pub fn wat_to_wasm(tokens: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let wat_source = syn::parse_macro_input!(tokens as syn::LitStr);

    let wasm = match wat::parse_str(wat_source.value()) {
        Ok(w) => w,
        Err(err) => {
            return syn::Error::new_spanned(&wat_source, err)
                .to_compile_error()
                .into();
        }
    };

    quote::quote! {
        [#(#wasm),*]
    }
    .into()
}
