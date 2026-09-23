//! The wire contract between `river-data-core` and this crate.
//!
//! The request and response field lists the sync clients and the server share are core's alone,
//! so what is left to guard is the seam the API keeps on its own side: the fields only the server knows (`source_system`, the pinned replicate
//! indexes), the defaults this API accepted an omitted field under before core declared the field,
//! and the unknown-field refusal that has to survive being reached through a flattened core type.
//!
//! These are pure serde: no database, no router. Run: cargo test --test wire

mod contract;
