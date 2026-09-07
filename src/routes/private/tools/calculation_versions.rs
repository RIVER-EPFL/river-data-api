//! Versioning a formula calculation, and holding both engines to their group.
//!
//! A formula calculation is versioned the way a script calculation is: an edit mints a
//! `tool_script_versions` row whose body is the formula set and whose manifest is the one the
//! formulas present, and activates it. A run pins that version, so a recompute reproduces the
//! value the audit compares against.

use sea_orm::{ConnectionTrait, DatabaseConnection, FromQueryResult, Statement};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::routes::private::parameters::groups::rules::{self, Member, Role};

use super::engine::{self, Engine};
use super::formula;
use super::hash::canonical_hash;

struct CalculationRow {
    name: String,
    label: String,
    description: Option<String>,
    engine: Engine,
    parameter_group_id: Option<Uuid>,
    active_version_id: Option<Uuid>,
}

async fn load_calculation(
    db: &DatabaseConnection,
    script_id: Uuid,
) -> AppResult<Option<CalculationRow>> {
    let Some(row) = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT name, label, description, engine, parameter_group_id, active_version_id \
               FROM tool_scripts WHERE id = $1",
            [script_id.into()],
        ))
        .await?
    else {
        return Ok(None);
    };
    let row = StoredCalculation::from_query_result(&row, "")?;
    Ok(Some(CalculationRow {
        name: row.name,
        label: row.label,
        description: row.description,
        engine: Engine::parse(&row.engine).unwrap_or(Engine::Script),
        parameter_group_id: row.parameter_group_id,
        active_version_id: row.active_version_id,
    }))
}

/// A calculation as its row stands, with `engine` left as the stored word: an engine outside the
/// vocabulary is a calculation this executor runs as a script, not a decode failure.
#[derive(FromQueryResult)]
struct StoredCalculation {
    name: String,
    label: String,
    description: Option<String>,
    engine: String,
    parameter_group_id: Option<Uuid>,
    active_version_id: Option<Uuid>,
}

/// Mint and activate a version for a formula calculation whose formula set has changed. A script
/// calculation is authored through the version routes and is left alone; an unchanged formula set
/// mints nothing.
pub async fn mint_formula_version(
    db: &DatabaseConnection,
    script_id: Uuid,
    actor: Option<&str>,
) -> AppResult<Option<Uuid>> {
    let Some(calculation) = load_calculation(db, script_id).await? else {
        return Ok(None);
    };
    if calculation.engine != Engine::Formula {
        return Ok(None);
    }

    let formulas: Vec<formula::PinnedFormula> = engine::load_formulas(db, &[script_id])
        .await?
        .into_iter()
        .map(|(_, f)| f)
        .collect();
    let manifest = formula::manifest_json(
        &calculation.label,
        calculation.description.as_deref(),
        &formulas,
    )
    .map_err(AppError::Conflict)?;
    let body = formula::render(&formulas).map_err(AppError::Conflict)?;
    let content_hash = canonical_hash(&serde_json::json!({
        "script": body,
        "manifest": manifest,
    }));

    if let Some(group_id) = calculation.parameter_group_id {
        check_manifest_against_group(db, group_id, &calculation.name, &manifest).await?;
    }

    if let Some(existing) = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id FROM tool_script_versions \
              WHERE tool_script_id = $1 AND content_hash = $2",
            [script_id.into(), content_hash.clone().into()],
        ))
        .await?
    {
        let id: Uuid = existing.try_get("", "id")?;
        if calculation.active_version_id != Some(id) {
            activate(db, script_id, calculation.active_version_id, id, actor).await?;
        }
        return Ok(Some(id));
    }

    let version_id = Uuid::new_v4();
    db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO tool_script_versions \
             (id, tool_script_id, version_no, script, entry_function, manifest, content_hash, \
              created_by, validated_at) \
         SELECT $1, $2, COALESCE(MAX(version_no), 0) + 1, $3, 'formula', $4, $5, $6, now() \
           FROM tool_script_versions WHERE tool_script_id = $2",
        [
            version_id.into(),
            script_id.into(),
            body.into(),
            manifest.into(),
            content_hash.into(),
            actor.map(str::to_string).into(),
        ],
    ))
    .await?;
    activate(
        db,
        script_id,
        calculation.active_version_id,
        version_id,
        actor,
    )
    .await?;
    Ok(Some(version_id))
}

/// Re-mint every formula calculation whose formula set no longer matches its active version.
/// Idempotent, and cheap: the mint is a no-op when the content hash already matches, so this is
/// what the definition hooks call, including after a delete where the calculation the definition
/// belonged to is no longer readable from the row.
pub async fn mint_stale_formula_versions(
    db: &DatabaseConnection,
    actor: Option<&str>,
) -> AppResult<()> {
    let rows = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id FROM tool_scripts WHERE engine = 'formula'".to_string(),
        ))
        .await?;
    for row in &rows {
        let id: Uuid = row.try_get("", "id")?;
        mint_formula_version(db, id, actor).await?;
    }
    Ok(())
}

/// The dedupe key an edit to one calculation audits under, so a burst of formula edits coalesces
/// into one audit the way a burst of constant edits does.
#[must_use]
pub fn audit_dedupe_key(name: &str) -> String {
    format!("event_audit:calculation:{name}")
}

/// A calculation's active version changed, so every output it has stored may disagree with what it
/// computes now. Nothing is rewritten: the edit enqueues the report-only `event_audit`, scoped to
/// the visits whose provenance names this calculation, and repair stays the scoped
/// `event_recompute` a person asks for. This is the policy `constants/operations.rs` already
/// applies to a constant edit, which is the same kind of change.
pub async fn audit_after_activation(db: &DatabaseConnection, name: &str) {
    let key = audit_dedupe_key(name);
    if let Err(e) = crate::routes::private::reprocessing_jobs::worker::enqueue(
        db,
        "event_audit",
        None,
        None,
        &serde_json::json!({ "calculation": name }),
        Some(&key),
    )
    .await
    {
        tracing::warn!(error = %e, calculation = %name, "failed to enqueue the calculation audit");
    }
}

async fn activate(
    db: &DatabaseConnection,
    script_id: Uuid,
    from: Option<Uuid>,
    to: Uuid,
    actor: Option<&str>,
) -> AppResult<()> {
    db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO tool_script_activations \
             (tool_script_id, from_version_id, to_version_id, activated_by) \
         VALUES ($1, $2, $3, $4)",
        [
            script_id.into(),
            from.into(),
            to.into(),
            actor.map(str::to_string).into(),
        ],
    ))
    .await?;
    db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "UPDATE tool_scripts SET active_version_id = $2, updated_at = now() WHERE id = $1",
        [script_id.into(), to.into()],
    ))
    .await?;
    if let Ok(Some(calculation)) = load_calculation(db, script_id).await {
        audit_after_activation(db, &calculation.name).await;
    }
    Ok(())
}

/// The catalog codes a manifest reads and writes, lowercased and deduplicated.
fn manifest_codes(manifest: &serde_json::Value) -> (Vec<String>, Vec<String>) {
    let codes = |key: &str, field: &str| -> Vec<String> {
        manifest
            .get(key)
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get(field).and_then(serde_json::Value::as_str))
                    .map(str::to_lowercase)
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut inputs = codes("params", "parameter_code");
    inputs.extend(codes_of_event_inputs(manifest));
    inputs.sort();
    inputs.dedup();
    (inputs, codes("outputs", "suggested_parameter_code"))
}

/// Catalog ids by lowercased code, for the codes asked for. A code the catalog does not hold is
/// simply absent, which is what the caller has to decide about.
async fn ids_by_code(
    db: &DatabaseConnection,
    codes: &[String],
) -> AppResult<std::collections::HashMap<String, Uuid>> {
    let mut wanted: Vec<String> = codes.to_vec();
    wanted.sort();
    wanted.dedup();
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id, LOWER(code) AS code FROM parameters WHERE LOWER(code) = ANY($1)",
            [wanted.into()],
        ))
        .await?;
    let mut by_code = std::collections::HashMap::new();
    for row in &rows {
        let row = CodeRow::from_query_result(&row, "")?;
        by_code.insert(row.code, row.id);
    }
    Ok(by_code)
}

/// The calculations bound to a group, as the reshape rules read them: name, inputs and outputs
/// resolved from each calculation's *active* version. A calculation with no active version
/// produces nothing yet and is not one.
pub async fn calculations_of_group(
    db: &DatabaseConnection,
    group_id: Uuid,
) -> AppResult<Vec<rules::Calculation>> {
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT s.name, v.manifest FROM tool_scripts s                JOIN tool_script_versions v ON v.id = s.active_version_id               WHERE s.parameter_group_id = $1",
            [group_id.into()],
        ))
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let ActiveManifestRow { name, manifest } = ActiveManifestRow::from_query_result(row, "")?;
        let (input_codes, output_codes) = manifest_codes(&manifest);
        let mut all = input_codes.clone();
        all.extend(output_codes.clone());
        let by_code = ids_by_code(db, &all).await?;
        out.push(rules::Calculation {
            group_id,
            name,
            inputs: input_codes
                .iter()
                .filter_map(|c| by_code.get(c).copied())
                .collect(),
            outputs: output_codes
                .iter()
                .filter_map(|c| by_code.get(c).copied())
                .collect(),
        });
    }
    Ok(out)
}

/// A calculation reads and writes only its group's members, in the roles they declare. The
/// manifest names catalog codes; the group names parameter ids, so the codes are resolved first
/// and an unknown code is itself a refusal.
pub async fn check_manifest_against_group(
    db: &DatabaseConnection,
    group_id: Uuid,
    name: &str,
    manifest: &serde_json::Value,
) -> AppResult<()> {
    let (input_codes, output_codes) = manifest_codes(manifest);
    if input_codes.is_empty() && output_codes.is_empty() {
        return Ok(());
    }

    let mut wanted: Vec<String> = input_codes
        .iter()
        .chain(output_codes.iter())
        .cloned()
        .collect();
    wanted.sort();
    wanted.dedup();
    let by_code = ids_by_code(db, &wanted).await?;
    for code in &wanted {
        if !by_code.contains_key(code) {
            return Err(AppError::BadRequest(format!(
                "calculation {name} names parameter {code}, which is not in the catalog"
            )));
        }
    }

    let member_rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT group_id, parameter_id, role FROM parameter_group_members",
            [],
        ))
        .await?;
    let mut members = Vec::with_capacity(member_rows.len());
    for row in &member_rows {
        let row = MemberRow::from_query_result(row, "")?;
        let Some(role) = Role::parse(&row.role) else {
            continue;
        };
        members.push(Member {
            group_id: row.group_id,
            parameter_id: row.parameter_id,
            role,
        });
    }

    let calculation = rules::Calculation {
        group_id,
        name: name.to_string(),
        inputs: input_codes
            .iter()
            .filter_map(|c| by_code.get(c).copied())
            .collect(),
        outputs: output_codes
            .iter()
            .filter_map(|c| by_code.get(c).copied())
            .collect(),
    };
    rules::validate_calculation(&calculation, &members)
        .map_err(|refusal| AppError::BadRequest(format!("calculation {name}: {refusal}")))
}

fn codes_of_event_inputs(manifest: &serde_json::Value) -> Vec<String> {
    manifest
        .get("event_inputs")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    item.get("parameter_code")
                        .and_then(serde_json::Value::as_str)
                })
                .map(str::to_lowercase)
                .collect()
        })
        .unwrap_or_default()
}

/// The three list queries the calculation rules make, as their SELECTs return them.
#[derive(FromQueryResult)]
struct ActiveManifestRow {
    name: String,
    manifest: serde_json::Value,
}

#[derive(FromQueryResult)]
struct MemberRow {
    group_id: Uuid,
    parameter_id: Uuid,
    role: String,
}

#[derive(FromQueryResult)]
struct CodeRow {
    code: String,
    id: Uuid,
}
