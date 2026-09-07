//! The wire contract between `river-data-core` and this crate.
//!
//! Nine request and response shapes are declared twice, once in core for the sync clients and once
//! here for the server. Nothing linked the two, so a field added on one side shipped green: three
//! had already drifted when this was written. Each test below sends core's struct through serde
//! into the API's, or the API's response back into core's, and asserts the values arrive.
//!
//! These are pure serde: no database, no router. Run: cargo test --test wire

mod contract;
