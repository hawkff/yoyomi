//! The browser reader: a local HTTP server on 127.0.0.1 that serves the open
//! books behind a one-time link and a random token.

mod owner;
mod server;

pub use server::{Server, open_in_browser};
