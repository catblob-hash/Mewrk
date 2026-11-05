//! `#[tauri::command]` only generates the IPC glue for a webview. The native client calls commands as ordinary
//! functions, so the attribute leaves the function as it is.

use proc_macro::TokenStream;

#[proc_macro_attribute]
pub fn command(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}
