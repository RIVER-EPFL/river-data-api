//! Request bodies whose field list is declared in `river-data-core`.
//!
//! Core is the one author of every shape the sync services send, so the API reads its structs
//! rather than restating them. What is left here is what only the server knows: the fields a
//! source cannot declare (`source_system`, the pinned replicate indexes), and the defaults this
//! API has accepted an omitted field under since before core carried the field at all.

use serde::Deserialize;
use serde::de::{DeserializeOwned, Deserializer, Error};

/// Read `T` from a JSON body, filling `defaults` in for keys the caller omitted. Core's structs
/// carry no serde defaults, so a field this route has always let a caller leave out is supplied
/// here rather than by tightening the wire.
pub fn defaulted<'de, D, T>(
    deserializer: D,
    defaults: &[(&str, serde_json::Value)],
) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    read(fill(body(deserializer)?, defaults))
}

/// The same, for a request that names the source it speaks for: `source_system` is taken off the
/// body and the rest is read as the core type on its own.
///
/// The remainder is not reached through `#[serde(flatten)]`, which would swallow the core struct's
/// `deny_unknown_fields`: a flattened struct is handed only the keys the outer one did not claim,
/// so every unknown field arrives as if it belonged there. Splitting the body keeps the refusal
/// where core declares it, which is the guarantee that a version skew cannot silently drop a
/// provenance field.
pub fn with_source_system<'de, D, T>(
    deserializer: D,
    defaults: &[(&str, serde_json::Value)],
) -> Result<(String, T), D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let mut body = fill(body(deserializer)?, defaults);
    let source_system = body
        .as_object_mut()
        .and_then(|map| map.remove("source_system"))
        .ok_or_else(|| D::Error::missing_field("source_system"))?;
    let source_system: String = serde_json::from_value(source_system).map_err(D::Error::custom)?;
    Ok((source_system, read(body)?))
}

fn body<'de, D: Deserializer<'de>>(deserializer: D) -> Result<serde_json::Value, D::Error> {
    serde_json::Value::deserialize(deserializer)
}

fn fill(mut body: serde_json::Value, defaults: &[(&str, serde_json::Value)]) -> serde_json::Value {
    if let Some(map) = body.as_object_mut() {
        for (key, value) in defaults {
            map.entry(*key).or_insert_with(|| value.clone());
        }
    }
    body
}

fn read<T: DeserializeOwned, E: Error>(body: serde_json::Value) -> Result<T, E> {
    serde_json::from_value(body).map_err(E::custom)
}
