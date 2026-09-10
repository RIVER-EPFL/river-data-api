//! Response-cache keys built from a struct rather than a hand-written array.
//!
//! A cache key must carry every request value that shapes the response body. Listing those values
//! by hand next to the handler is how `parameter_ids` and `split_by_sensor` came to be missing: the
//! query struct grew a field and the array did not.
//!
//! [`key_for`] takes a serialisable key struct and derives the components from its fields, so the
//! only way to omit a value is to leave it out of the struct. Give the struct the request's
//! *effective* values (defaults resolved, format decided) and flatten the query struct into it, so a
//! field added to the query enters the key without anyone remembering to do it:
//!
//! ```text
//! #[derive(Serialize)]
//! struct ReadingsKey<'a> {
//!     site_id: Uuid,
//!     effective_start: DateTime<Utc>,
//!     effective_end: Option<DateTime<Utc>>,
//!     format: &'a str,
//!     #[serde(flatten)]
//!     query: &'a SiteReadingsQuery,
//! }
//!
//! let key = cache_key::key_for("readings", &ReadingsKey { .. });
//! ```
//!
//! Field values are rendered as compact JSON, so `None`, `""` and `"null"` are three different
//! keys. Fields are emitted in name order, which makes the key independent of declaration order.
//! List order is preserved: `[a, b]` and `[b, a]` are different keys, so a handler that treats a
//! filter as a set should sort it before it reaches the key.
//!
//! Keys are namespaced by `prefix`. Put the site UUID first among the components so a per-site
//! invalidation can find them.

use serde::Serialize;

use super::cache;

/// Build a cache key from a serialisable struct of everything that shapes the response.
///
/// A struct serialises as `name=<json>` components joined by [`cache::cache_key`]; any other shape
/// (a tuple, a bare value) serialises as one component.
#[must_use]
pub fn key_for<T: Serialize + ?Sized>(prefix: &str, key: &T) -> String {
    let components = components_of(key);
    let refs: Vec<&str> = components.iter().map(String::as_str).collect();
    cache::cache_key(prefix, &refs)
}

fn components_of<T: Serialize + ?Sized>(key: &T) -> Vec<String> {
    let value = match serde_json::to_value(key) {
        Ok(v) => v,
        // A key struct that cannot serialise would otherwise collapse every request onto one entry.
        Err(e) => return vec![format!("unserializable={e}")],
    };
    match value {
        serde_json::Value::Object(map) => {
            let mut fields: Vec<(String, serde_json::Value)> = map.into_iter().collect();
            fields.sort_by(|a, b| a.0.cmp(&b.0));
            fields
                .into_iter()
                .map(|(name, v)| format!("{name}={}", render(&v)))
                .collect()
        }
        other => vec![render(&other)],
    }
}

fn render(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "?".to_string())
}

#[cfg(test)]
#[path = "tests/cache_key.rs"]
mod tests;
