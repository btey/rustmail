// Keep the application implementation in one compilation unit while exposing
// it to the thin desktop platform launcher.
pub use rustmail_core;
include!("main.rs");
