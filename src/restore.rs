//! Move a dumped production database's curated state into a blank one built from the baseline.
//!
//! The cutover is: `pg_dump` production, restore that dump into a scratch database, build the new
//! database from the baseline and let the pairing plans and the sync services mint its sites,
//! parameters, streams and instruments, then run this against the pair and read its report. The
//! rebuilt database mints its own uuids, so nothing is carried by id: every reference is resolved
//! against the target by the natural key the two databases share, and a reference with no match
//! on the target is dropped and reported rather than guessed at.
//!
//! What is carried is `CARRIED`, and the report names every other public table the dump holds rows
//! in so that nothing is left behind silently. The calculation catalogue (`tool_scripts`,
//! `tool_script_versions`, `tool_script_activations`, `calculation_formulas` and
//! `derived_parameter_sources`) is on that list by decision: it is authored on the rebuilt database
//! through `/tool_scripts` after the restore, not carried (Q176).
//!
//! No column is named here. A carried table's columns come from the target's own catalog and the
//! rows travel as `jsonb`, so a column added to `readings` after this was written is carried
//! without anybody remembering to add it, and a uuid column pointing somewhere this file does not
//! know about is reported rather than carried as an id that means nothing in the new database.
//!
//! The load runs in one transaction with `session_replication_role = replica`, which is how
//! `pg_restore --disable-triggers` loads rows: the projection trigger on `reading_decisions` and
//! the sample-statistics triggers on `readings` stay off and the columns they maintain are carried
//! verbatim, so what the rebuilt database serves is what production served rather than a replay.
//! It needs a role that may set that, which in practice means a superuser on the target.

use std::collections::HashMap;

use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbErr, FromQueryResult, Statement,
    TransactionTrait, Value,
};
use uuid::Uuid;

/// Rows carried per statement. Each batch travels as one `jsonb` document.
const BATCH: usize = 500;

/// The separator between the parts of a composite natural key. A unit separator cannot appear in
/// a site name, a parameter code or a source key.
const SEP: char = '\u{1}';

/// Where an id column points.
enum Reference {
    /// Resolved against the rebuilt database by the natural key both databases name the row by.
    Natural(&'static str),
    /// A table this cutover carries, so the id the dump holds is the id it keeps.
    Carried,
    /// A table this cutover does not carry. The reference is dropped and counted, because an id
    /// the new database has no row for is a claim about nothing.
    Dropped,
}

/// A table the cutover carries, and where each of its id columns points. The order of the list is
/// the order they load in: a table is carried after everything it references.
struct Carried {
    table: &'static str,
    /// The columns a page walks by, unique together and in index order.
    key: &'static [&'static str],
    references: &'static [(&'static str, Reference)],
    /// Id columns that live inside a jsonb document rather than in a column of their own: the
    /// column, the key holding the id, and where it points. One that does not resolve is nulled
    /// and reported, never a reason to refuse the row: the document describes the row, it is not
    /// what the row is.
    nested: &'static [(&'static str, &'static str, Reference)],
}

const CARRIED: &[Carried] = &[
    // Before `readings`: a reading's provenance blob names the run that computed it by id, and the
    // blob is carried verbatim, so the run has to be there under that same id.
    Carried {
        table: "tool_runs",
        key: &["id"],
        references: &[],
        nested: &[("context", "site_id", Reference::Natural("sites"))],
    },
    Carried {
        table: "collection_events",
        key: &["id"],
        references: &[("site_id", Reference::Natural("sites"))],
        nested: &[],
    },
    Carried {
        table: "reading_decision_sets",
        key: &["id"],
        references: &[],
        nested: &[],
    },
    Carried {
        table: "samples",
        key: &["id"],
        references: &[
            ("site_id", Reference::Natural("sites")),
            ("parameter_id", Reference::Natural("parameters")),
        ],
        nested: &[],
    },
    Carried {
        table: "readings",
        key: &["stream_id", "time", "replicate_index"],
        references: &[
            ("stream_id", Reference::Natural("data_streams")),
            ("site_id", Reference::Natural("sites")),
            ("parameter_id", Reference::Natural("parameters")),
            ("sensor_id", Reference::Natural("sensors")),
            ("calibration_id", Reference::Natural("sensor_calibrations")),
            ("deployment_id", Reference::Natural("sensor_deployments")),
            ("standard_curve_id", Reference::Natural("standard_curves")),
            (
                "derived_version_id",
                Reference::Natural("derived_parameter_definition_versions"),
            ),
            (
                "collection_event_id",
                Reference::Natural("collection_events"),
            ),
            ("sample_id", Reference::Natural("samples")),
        ],
        nested: &[],
    },
    Carried {
        table: "reading_decisions",
        key: &["id"],
        references: &[
            ("stream_id", Reference::Natural("data_streams")),
            ("supersedes", Reference::Carried),
            ("rolled_back_by", Reference::Carried),
            ("set_id", Reference::Carried),
            // A job row is pruned out from under the ledger already, which is why the column is
            // nullable and why a cutover drops it rather than inventing a run.
            ("job_id", Reference::Dropped),
        ],
        nested: &[],
    },
    Carried {
        table: "replicate_audit_holds",
        key: &["id"],
        references: &[
            ("stream_id", Reference::Natural("data_streams")),
            ("site_id", Reference::Natural("sites")),
            ("parameter_id", Reference::Natural("parameters")),
        ],
        nested: &[],
    },
    Carried {
        table: "annotations",
        key: &["id"],
        references: &[
            ("site_id", Reference::Natural("sites")),
            ("parameter_id", Reference::Natural("parameters")),
            ("standard_curve_id", Reference::Natural("standard_curves")),
            ("audit_hold_id", Reference::Carried),
        ],
        nested: &[],
    },
    Carried {
        table: "meteoswiss_subscriptions",
        key: &["id"],
        references: &[
            ("site_id", Reference::Natural("sites")),
            ("parameter_id", Reference::Natural("parameters")),
        ],
        nested: &[],
    },
    Carried {
        table: "notes",
        key: &["id"],
        references: &[("site_id", Reference::Natural("sites"))],
        nested: &[],
    },
    Carried {
        table: "alarm_events",
        key: &["id"],
        references: &[
            ("site_id", Reference::Natural("sites")),
            ("parameter_id", Reference::Natural("parameters")),
            ("sensor_id", Reference::Natural("sensors")),
        ],
        nested: &[],
    },
    Carried {
        table: "status_events",
        key: &["stream_id", "time"],
        references: &[
            ("stream_id", Reference::Natural("data_streams")),
            ("site_id", Reference::Natural("sites")),
            ("parameter_id", Reference::Natural("parameters")),
            ("sensor_id", Reference::Natural("sensors")),
        ],
        nested: &[],
    },
];

/// The natural key each referenced table is recognised by, as a query selecting `id` and `k`.
/// A table both databases build the same way from the same sources is keyed by what those sources
/// call the row, never by the uuid either database minted for it.
fn natural_keys() -> Vec<(&'static str, String)> {
    let sensor = |alias: &str| {
        format!(
            "coalesce(lower({alias}.serial_number),
                      'source:' || {alias}.source_system || '{SEP}' || {alias}.source_key)"
        )
    };
    let on_sensor = |table: &str, column: &str| {
        format!(
            "SELECT t.id, {} || '{SEP}' || t.{column}::text AS k
               FROM public.{table} t JOIN public.sensors n ON n.id = t.sensor_id",
            sensor("n")
        )
    };
    vec![
        (
            "sites",
            "SELECT id, lower(name) AS k FROM public.sites".to_string(),
        ),
        (
            "parameters",
            "SELECT id, lower(code) AS k FROM public.parameters".to_string(),
        ),
        (
            "data_streams",
            format!(
                "SELECT id, source_system || '{SEP}' || source_key AS k FROM public.data_streams"
            ),
        ),
        (
            "sensors",
            format!("SELECT n.id, {} AS k FROM public.sensors n", sensor("n")),
        ),
        (
            "sensor_calibrations",
            on_sensor("sensor_calibrations", "valid_from"),
        ),
        (
            "sensor_deployments",
            on_sensor("sensor_deployments", "deployed_from"),
        ),
        (
            "standard_curves",
            format!(
                "SELECT c.id, coalesce(c.source_system || '{SEP}' || c.source_key,
                                       {} || '{SEP}' || coalesce(c.name, '')
                                       || '{SEP}' || c.created_at::text) AS k
                   FROM public.standard_curves c JOIN public.sensors n ON n.id = c.sensor_id",
                sensor("n")
            ),
        ),
        (
            "derived_parameter_definition_versions",
            format!(
                "SELECT v.id, f.code || '{SEP}' || v.version_no::text AS k
                   FROM public.derived_parameter_definition_versions v
                   JOIN public.calculation_formulas f ON f.id = v.definition_id"
            ),
        ),
        (
            "samples",
            format!(
                "SELECT x.id, lower(s.name) || '{SEP}' || lower(p.code) || '{SEP}'
                        || x.collected_at::text AS k
                   FROM public.samples x
                   JOIN public.sites s ON s.id = x.site_id
                   JOIN public.parameters p ON p.id = x.parameter_id"
            ),
        ),
        (
            "collection_events",
            format!(
                "SELECT e.id, lower(s.name) || '{SEP}' || e.collected_at::text AS k
                   FROM public.collection_events e JOIN public.sites s ON s.id = e.site_id"
            ),
        ),
    ]
}

/// What one cutover moved, and everything it could not carry whole.
#[derive(Debug, Default)]
pub struct Restored {
    /// Rows written, by table.
    pub carried: Vec<(String, usize)>,
    pub projects_configured: usize,
    pub slots_exposed: usize,
    /// Rows the target has nowhere to put, because a reference it cannot do without does not
    /// resolve there: by table, with the column that failed.
    pub refused: Vec<String>,
    /// One line per natural key the source holds and the target does not, per reference dropped,
    /// and per id column this file does not know where to point.
    pub unmatched: Vec<String>,
    /// Tables the source holds rows in that this cutover does not carry, with their row counts.
    /// The rebuild mints its own sites, parameters, streams and instruments, and the calculation
    /// catalogue is authored on the rebuilt database afterwards, so what is here is what somebody
    /// still has to account for rather than a list of losses.
    pub not_carried: Vec<(String, usize)>,
}

impl Restored {
    pub fn rows(&self, table: &str) -> usize {
        self.carried
            .iter()
            .find(|(name, _)| name == table)
            .map_or(0, |(_, rows)| *rows)
    }
}

#[derive(FromQueryResult)]
struct Keyed {
    id: Uuid,
    k: String,
}

#[derive(FromQueryResult)]
struct Column {
    name: String,
    kind: String,
    nullable: bool,
    generated: bool,
}

#[derive(FromQueryResult)]
struct Row {
    row: serde_json::Value,
}

fn sql(statement: &str) -> Statement {
    Statement::from_string(sea_orm::DatabaseBackend::Postgres, statement)
}

/// The table's columns as the target declares them, in order.
async fn columns<T: ConnectionTrait>(target: &T, table: &str) -> Result<Vec<Column>, DbErr> {
    Column::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT a.attname AS name, format_type(a.atttypid, a.atttypmod) AS kind,
                NOT a.attnotnull AS nullable, a.attgenerated <> '' AS generated
           FROM pg_attribute a
          WHERE a.attrelid = $1::regclass AND a.attnum > 0 AND NOT a.attisdropped
          ORDER BY a.attnum",
        [format!("public.{table}").into()],
    ))
    .all(target)
    .await
}

/// A source id to target id map over rows both databases name the same way.
async fn key_map<S: ConnectionTrait, T: ConnectionTrait>(
    source: &S,
    target: &T,
    label: &str,
    statement: &str,
    unmatched: &mut Vec<String>,
) -> Result<HashMap<Uuid, Uuid>, DbErr> {
    let held: HashMap<String, Uuid> = Keyed::find_by_statement(sql(statement))
        .all(target)
        .await?
        .into_iter()
        .map(|row| (row.k, row.id))
        .collect();
    let mut map = HashMap::new();
    for row in Keyed::find_by_statement(sql(statement)).all(source).await? {
        match held.get(&row.k) {
            Some(id) => {
                map.insert(row.id, *id);
            }
            None => unmatched.push(format!("{label} {}", row.k)),
        }
    }
    Ok(map)
}

/// A page of rows as one `jsonb` value each, walked by the table's key so a production-sized pass
/// reads the index rather than counting past what it has already read.
async fn page<S: ConnectionTrait>(
    source: &S,
    carried: &Carried,
    types: &HashMap<String, String>,
    after: Option<&serde_json::Value>,
) -> Result<Vec<serde_json::Value>, DbErr> {
    let quoted: Vec<String> = carried
        .key
        .iter()
        .map(|column| format!("\"{column}\""))
        .collect();
    let order = quoted.join(", ");
    let table = carried.table;
    let statement = match after {
        None => sql(&format!(
            "SELECT to_jsonb(t) AS row FROM public.{table} t ORDER BY {order} LIMIT {BATCH}"
        )),
        Some(last) => {
            let casts: Vec<String> = carried
                .key
                .iter()
                .enumerate()
                .map(|(index, column)| format!("${}::{}", index + 1, types[*column]))
                .collect();
            let values: Vec<Value> = carried
                .key
                .iter()
                .map(|column| match &last[*column] {
                    serde_json::Value::String(text) => text.clone().into(),
                    other => other.to_string().into(),
                })
                .collect();
            Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    "SELECT to_jsonb(t) AS row FROM public.{table} t
                      WHERE ({order}) > ({})
                      ORDER BY {order} LIMIT {BATCH}",
                    casts.join(", ")
                ),
                values,
            )
        }
    };
    Ok(Row::find_by_statement(statement)
        .all(source)
        .await?
        .into_iter()
        .map(|row| row.row)
        .collect())
}

/// Carry one table, resolving every id column it declares and reporting every one it cannot.
async fn carry<S: ConnectionTrait>(
    source: &S,
    target: &DatabaseTransaction,
    carried: &Carried,
    maps: &HashMap<&str, HashMap<Uuid, Uuid>>,
    report: &mut Restored,
) -> Result<usize, DbErr> {
    let declared = columns(target, carried.table).await?;
    let types: HashMap<String, String> = declared
        .iter()
        .map(|column| (column.name.clone(), column.kind.clone()))
        .collect();
    let nullable: HashMap<&str, bool> = declared
        .iter()
        .map(|column| (column.name.as_str(), column.nullable))
        .collect();
    let writable: Vec<String> = declared
        .iter()
        .filter(|column| !column.generated)
        .map(|column| format!("\"{}\"", column.name))
        .collect();
    let writable = writable.join(", ");

    for column in &declared {
        let known = column.name == "id"
            || carried
                .references
                .iter()
                .any(|(name, _)| *name == column.name);
        if column.kind == "uuid" && !known {
            report.unmatched.push(format!(
                "{}.{} is an id column pointing at a table the cutover does not know",
                carried.table, column.name
            ));
        }
    }

    let mut written = 0;
    let mut after: Option<serde_json::Value> = None;
    loop {
        let rows = page(source, carried, &types, after.as_ref()).await?;
        let Some(last) = rows.last() else { break };
        after = Some(last.clone());
        let read = rows.len();

        let mut batch = Vec::with_capacity(read);
        'row: for mut row in rows {
            for (column, reference) in carried.references {
                let Some(id) = row
                    .get(*column)
                    .and_then(serde_json::Value::as_str)
                    .and_then(|text| Uuid::parse_str(text).ok())
                else {
                    continue;
                };
                let moved = match reference {
                    Reference::Carried => Some(id),
                    Reference::Dropped => None,
                    Reference::Natural(table) => maps[table].get(&id).copied(),
                };
                match moved {
                    Some(moved) => {
                        row[*column] = serde_json::Value::String(moved.to_string());
                    }
                    None if nullable[*column] => {
                        row[*column] = serde_json::Value::Null;
                        report
                            .unmatched
                            .push(format!("{}.{column} dropped", carried.table));
                    }
                    None => {
                        report
                            .refused
                            .push(format!("{} without {column}", carried.table));
                        continue 'row;
                    }
                }
            }
            for (column, key, reference) in carried.nested {
                let Some(id) = row
                    .get(*column)
                    .and_then(|document| document.get(*key))
                    .and_then(serde_json::Value::as_str)
                    .and_then(|text| Uuid::parse_str(text).ok())
                else {
                    continue;
                };
                let moved = match reference {
                    Reference::Carried => Some(id),
                    Reference::Dropped => None,
                    Reference::Natural(table) => maps[table].get(&id).copied(),
                };
                match moved {
                    Some(moved) => {
                        row[*column][*key] = serde_json::Value::String(moved.to_string())
                    }
                    None => {
                        row[*column][*key] = serde_json::Value::Null;
                        report
                            .unmatched
                            .push(format!("{}.{column}.{key} dropped", carried.table));
                    }
                }
            }
            batch.push(row);
        }

        if !batch.is_empty() {
            let rows_in_batch = batch.len();
            let document = serde_json::Value::Array(batch).to_string();
            target
                .execute_raw(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    format!(
                        "INSERT INTO public.{} ({writable})
                         SELECT {writable}
                           FROM jsonb_populate_recordset(null::public.{}, $1::jsonb)
                         ON CONFLICT DO NOTHING",
                        carried.table, carried.table
                    ),
                    [document.into()],
                ))
                .await?;
            written += rows_in_batch;
        }
        if read < BATCH {
            break;
        }
    }
    Ok(written)
}

/// The public API setups: what each project publishes under, and which slots it exposes. Neither
/// is re-derivable from a source system, and a rebuilt database has both off by default.
async fn move_public_settings<S: ConnectionTrait>(
    source: &S,
    target: &DatabaseTransaction,
    report: &mut Restored,
) -> Result<(), DbErr> {
    #[derive(FromQueryResult)]
    struct ProjectSettings {
        k: String,
        is_public: Option<bool>,
        public_code: Option<String>,
        public_api_title: Option<String>,
        public_api_description: Option<String>,
        public_api_version: Option<String>,
        public_contact_email: Option<String>,
    }
    let projects = ProjectSettings::find_by_statement(sql(
        "SELECT lower(name) AS k, is_public, public_code, public_api_title,
                public_api_description, public_api_version, public_contact_email
           FROM public.projects",
    ))
    .all(source)
    .await?;
    for project in projects {
        let moved = target
            .execute_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE public.projects SET is_public = $2, public_code = $3,
                        public_api_title = $4, public_api_description = $5,
                        public_api_version = $6, public_contact_email = $7
                  WHERE lower(name) = $1",
                [
                    project.k.clone().into(),
                    project.is_public.into(),
                    project.public_code.into(),
                    project.public_api_title.into(),
                    project.public_api_description.into(),
                    project.public_api_version.into(),
                    project.public_contact_email.into(),
                ],
            ))
            .await?;
        if moved.rows_affected() == 0 {
            report.unmatched.push(format!("project {}", project.k));
        } else {
            report.projects_configured += 1;
        }
    }

    #[derive(FromQueryResult)]
    struct ExposedSlot {
        site: String,
        parameter: String,
    }
    let slots = ExposedSlot::find_by_statement(sql(
        "SELECT lower(s.name) AS site, lower(p.code) AS parameter
           FROM public.site_parameters sp
           JOIN public.sites s ON s.id = sp.site_id
           JOIN public.parameters p ON p.id = sp.parameter_id
          WHERE sp.is_public",
    ))
    .all(source)
    .await?;
    for slot in slots {
        let moved = target
            .execute_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE public.site_parameters SET is_public = true
                   FROM public.sites s, public.parameters p
                  WHERE s.id = site_parameters.site_id AND p.id = site_parameters.parameter_id
                    AND lower(s.name) = $1 AND lower(p.code) = $2",
                [slot.site.clone().into(), slot.parameter.clone().into()],
            ))
            .await?;
        if moved.rows_affected() == 0 {
            report
                .unmatched
                .push(format!("exposed slot {}/{}", slot.site, slot.parameter));
        } else {
            report.slots_exposed += 1;
        }
    }
    Ok(())
}

#[derive(FromQueryResult)]
struct Named {
    name: String,
}

#[derive(FromQueryResult)]
struct Counted {
    rows: i64,
}

/// The names of `tables` that `CARRIED` does not name, in the order they arrived.
fn uncarried(tables: Vec<String>) -> Vec<String> {
    tables
        .into_iter()
        .filter(|name| !CARRIED.iter().any(|table| table.table == name))
        .collect()
}

/// Every table the source holds rows in that this cutover leaves behind, with its row count.
/// The rebuild mints the sites, parameters, streams and instruments itself and the calculation
/// catalogue is authored on the rebuilt database, so a report naming only what it carried says
/// nothing about the tables somebody still has to account for.
async fn not_carried<S: ConnectionTrait>(source: &S) -> Result<Vec<(String, usize)>, DbErr> {
    let tables = Named::find_by_statement(sql("SELECT c.relname AS name
           FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
          WHERE n.nspname = 'public' AND c.relkind IN ('r', 'p')
          ORDER BY c.relname"))
    .all(source)
    .await?
    .into_iter()
    .map(|table| table.name)
    .collect();
    let mut left = Vec::new();
    for name in uncarried(tables) {
        let counted = Counted::find_by_statement(sql(&format!(
            "SELECT count(*)::bigint AS rows FROM public.\"{name}\""
        )))
        .one(source)
        .await?
        .map_or(0, |counted| usize::try_from(counted.rows).unwrap_or(0));
        if counted > 0 {
            left.push((name, counted));
        }
    }
    Ok(left)
}

/// Carry the curated state of `source` into `target`, which is a database built from the baseline
/// and rebuilt to the point where its streams, sites, parameters and instruments exist.
///
/// Either the whole cutover lands or none of it does: one transaction, and an error rolls it back.
pub async fn restore(
    source: &DatabaseConnection,
    target: &DatabaseConnection,
) -> Result<Restored, DbErr> {
    let mut report = Restored::default();
    let transaction = target.begin().await?;
    transaction
        .execute_unprepared("SET LOCAL session_replication_role = replica")
        .await?;
    move_public_settings(source, &transaction, &mut report).await?;

    // A visit the rebuilt database does not hold is carried rather than dropped, and the map is
    // built after that, so a reading keeps the visit it was entered against either way.
    let mut maps: HashMap<&str, HashMap<Uuid, Uuid>> = HashMap::new();
    let keys = natural_keys();
    for table in CARRIED {
        let referenced = table
            .references
            .iter()
            .map(|(_, reference)| reference)
            .chain(table.nested.iter().map(|(_, _, reference)| reference));
        for reference in referenced {
            if let Reference::Natural(name) = reference
                && !maps.contains_key(name)
            {
                let statement = &keys
                    .iter()
                    .find(|(known, _)| known == name)
                    .expect("every referenced table declares a natural key")
                    .1;
                let map =
                    key_map(source, &transaction, name, statement, &mut report.unmatched).await?;
                maps.insert(name, map);
            }
        }
        let written = carry(source, &transaction, table, &maps, &mut report).await?;
        report.carried.push((table.table.to_string(), written));
        // A table carried into the target is also a table later ones resolve against.
        if let Some((_, statement)) = keys.iter().find(|(known, _)| *known == table.table) {
            let map = key_map(
                source,
                &transaction,
                table.table,
                statement,
                &mut report.unmatched,
            )
            .await?;
            maps.insert(table.table, map);
        }
    }
    report.not_carried = not_carried(source).await?;
    transaction.commit().await?;
    Ok(report)
}

#[cfg(test)]
#[path = "tests/restore.rs"]
mod tests;
