use proc_macro::TokenStream;

/// `answer!()` expands to the literal `42`.
#[proc_macro]
pub fn answer(_input: TokenStream) -> TokenStream {
    "42".parse().expect("literal")
}
