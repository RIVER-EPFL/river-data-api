//! The content identity of a tool script version.
//!
//! Every version carries one hash rule whether it was seeded by a migration or authored through
//! the portal, so a provenance blob pinning a content hash pins the same thing either way. The
//! rule lives in this crate because the API crate depends on it and not the other way round; it is
//! the only place both the seed and the authoring path can read one implementation.

use sea_orm_migration::sea_orm::{ConnectionTrait, DbErr, Statement};
use sha2::{Digest, Sha256};

/// Serialise a value with object keys in sorted order, so two equivalent manifests produce the
/// same bytes whatever order they were written in.
fn canonical_json(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::Value::String((*key).clone()).to_string());
                out.push(':');
                canonical_json(&map[*key], out);
            }
            out.push('}');
        }
        serde_json::Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical_json(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// The identity of a version's whole content, not just its R source: an edit to the manifest or
/// the test cases is a new version, and only a re-post of everything unchanged is a duplicate.
/// Prefixed because rows created before this hashed `md5(script)` alone and keep that value.
pub fn version_content_hash(
    script: &str,
    entry_function: &str,
    manifest: &serde_json::Value,
    test_cases: &serde_json::Value,
) -> String {
    let bundle = serde_json::json!({
        "script": script,
        "entry_function": entry_function,
        "manifest": manifest,
        "test_cases": test_cases,
    });
    let mut canonical = String::new();
    canonical_json(&bundle, &mut canonical);
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

/// A version's content after Postgres has had its say about the JSON halves: the bytes to store
/// and the hash of exactly those bytes.
pub struct StoredVersionContent {
    pub content_hash: String,
    /// `manifest` and `test_cases` as `jsonb` renders them, to be written back with a `::jsonb`
    /// cast. Postgres' rendering re-parses to the same `jsonb`, so storing it changes nothing
    /// about the value and pins what the hash was taken over.
    pub manifest: String,
    pub test_cases: String,
}

/// Hash a version over the form the database will hold, so the hash on a row can be recomputed
/// from that row and match.
///
/// `jsonb` is a parsed value, not the text it arrived as: it re-renders numbers through `numeric`
/// (`1e-9` reads back as `0.000000001`), drops insignificant whitespace, orders keys its own way
/// and keeps only the last of a repeated key. Hashing the value as it sat in memory therefore
/// identifies the bytes an author sent rather than the bytes anyone can read back, which makes
/// both the provenance hash and the duplicate-version check unverifiable: fetching a version and
/// re-posting it produces a different hash and so a second copy. Rather than reimplement those
/// rules (they are Postgres', and they cover more than floats), this asks Postgres to apply them
/// and hashes the answer.
///
/// Costs one round trip before the insert. The alternative, inserting and then hashing what came
/// back, needs a transaction to keep a row that failed the duplicate check from existing.
pub async fn stored_version_content<C: ConnectionTrait>(
    db: &C,
    script: &str,
    entry_function: &str,
    manifest: &serde_json::Value,
    test_cases: &serde_json::Value,
) -> Result<StoredVersionContent, DbErr> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm_migration::sea_orm::DatabaseBackend::Postgres,
            "SELECT $1::jsonb::text AS manifest, $2::jsonb::text AS test_cases",
            [
                serde_json::to_string(manifest)
                    .map_err(|e| DbErr::Custom(format!("manifest is not serialisable: {e}")))?
                    .into(),
                serde_json::to_string(test_cases)
                    .map_err(|e| DbErr::Custom(format!("test cases are not serialisable: {e}")))?
                    .into(),
            ],
        ))
        .await?
        .ok_or_else(|| {
            DbErr::Custom("normalising the version content returned no row".to_string())
        })?;
    let manifest: String = row.try_get("", "manifest")?;
    let test_cases: String = row.try_get("", "test_cases")?;
    let content_hash = version_content_hash(
        script,
        entry_function,
        &serde_json::from_str(&manifest)
            .map_err(|e| DbErr::Custom(format!("normalised manifest is not JSON: {e}")))?,
        &serde_json::from_str(&test_cases)
            .map_err(|e| DbErr::Custom(format!("normalised test cases are not JSON: {e}")))?,
    );
    Ok(StoredVersionContent {
        content_hash,
        manifest,
        test_cases,
    })
}

#[cfg(test)]
mod tests {
    use super::version_content_hash;

    #[test]
    fn an_equivalent_manifest_hashes_equal_whatever_order_it_was_written_in() {
        let one = serde_json::json!({ "label": "T", "constants": ["a", "b"], "params": [] });
        let other = serde_json::json!({ "params": [], "label": "T", "constants": ["a", "b"] });
        let cases = serde_json::json!({});
        assert_eq!(
            version_content_hash("s", "tool", &one, &cases),
            version_content_hash("s", "tool", &other, &cases)
        );
        let relabelled = serde_json::json!({ "label": "U", "constants": ["a", "b"], "params": [] });
        assert_ne!(
            version_content_hash("s", "tool", &one, &cases),
            version_content_hash("s", "tool", &relabelled, &cases)
        );
        assert_ne!(
            version_content_hash("s", "tool", &one, &cases),
            version_content_hash("s", "tool", &one, &serde_json::json!({ "cases": [] }))
        );
    }
}
