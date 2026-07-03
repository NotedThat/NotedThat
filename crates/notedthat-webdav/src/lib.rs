//! `WebDAV` access surface for `NotedThat`.
//!
//! Implements the `WebDAV` protocol as specified in SPECIFICATIONS.md §6.11 using
//! the dav-server crate (see D16). All write operations (PUT, DELETE, MOVE, COPY)
//! are handled by axum middleware before reaching the `DavHandler`, ensuring that
//! HTTP headers (Content-Type, If-Match) are accessible for the shared write path.

pub mod error;
pub mod file;
pub mod filesystem;
pub mod metadata;
pub mod middleware;
pub mod router;
pub mod state;
