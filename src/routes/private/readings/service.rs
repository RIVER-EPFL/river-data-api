use super::decision_model;
use super::models::*;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;

use chrono::DateTime;
use chrono::Utc;
use river_data_core::models::MeasurementType;
use sea_orm::ActiveModelTrait;
use sea_orm::ActiveValue;
use sea_orm::ColumnTrait;
use sea_orm::Condition;
use sea_orm::ConnectionTrait;
use sea_orm::DatabaseConnection;
use sea_orm::EntityTrait;
use sea_orm::ExprTrait;
use sea_orm::FromQueryResult;
use sea_orm::NotSet;
use sea_orm::QueryFilter;
use sea_orm::QueryOrder;
use sea_orm::QuerySelect;
use sea_orm::Set;
use sea_orm::Statement;
use sea_orm::TransactionTrait;
use sea_orm::Value;
use sea_orm::entity::prelude::*;
use sea_orm::sea_query::Alias;
use sea_orm::sea_query::CommonTableExpression;
use sea_orm::sea_query::Expr;
use sea_orm::sea_query::Func;
use sea_orm::sea_query::JoinType;
use sea_orm::sea_query::OnConflict;
use sea_orm::sea_query::Order;
use sea_orm::sea_query::PostgresQueryBuilder;
use sea_orm::sea_query::Query;
use sea_orm::sea_query::SimpleExpr;
use sea_orm::sea_query::WithClause;
use sea_orm::sea_query::extension::postgres::PgBinOper;
use uuid::Uuid;

use crate::common::AppEvent;
use crate::common::AppState;
use crate::common::aggregates;
use crate::common::aggregates::Window;
use crate::common::authz::AccessScope;
use crate::common::bulk_write;
use crate::common::cache;
use crate::common::middleware::AuthContext;
use crate::common::middleware::enforce_project_scope_for_sites;
use crate::common::severity::Severity;
use crate::common::state::EventSender;
use crate::common::state::ResponseCache;
use crate::error::AppError;
use crate::error::AppResult;
use crate::routes::private::alarms::models::alarm_event;
use crate::routes::private::change_audit::models as change_audit;
use crate::routes::private::collection_events;
use crate::routes::private::collection_events::flows;
use crate::routes::private::collection_events::flows::TouchedEvent;
use crate::routes::private::data_streams;
use crate::routes::private::data_streams::models::receipts;
use crate::routes::private::derived_parameters::models::definition as calculation_formulas;
use crate::routes::private::parameters::models as parameters;
use crate::routes::private::readings;
use crate::routes::private::readings::models::ConflictMode;
use crate::routes::private::readings::models::Kind;
use crate::routes::private::readings::models::Origin;
use crate::routes::private::readings::models::Owner;
use crate::routes::private::readings::models::ProvenanceQuery;
use crate::routes::private::readings::models::Selection;
use crate::routes::private::readings::samples;
use crate::routes::private::reprocessing_jobs::models::job as jobs_model;
use crate::routes::private::sensor_calibrations;
use crate::routes::private::sensor_calibrations::service::{InputCandidate, chosen_input};
use crate::routes::private::sensor_deployments as deployments;
use crate::routes::private::sensors;
use crate::routes::private::sensors::models::ResolvedOwner;
use crate::routes::private::site_parameters;
use crate::routes::private::sites;
use crate::routes::private::standard_curves;
use crate::routes::private::sync::hold_model;
use crate::routes::private::sync::models::GroupAudit;
use crate::routes::private::sync::models::HoldKind;
use crate::routes::private::sync::models::HoldStatus;
use crate::routes::private::sync::service as audit;
use crate::routes::private::sync::service::GroupStats;
use crate::routes::private::tools::models::run as tool_run;
use crate::routes::private::tools::models::version as tool_version;

impl Related<crate::routes::private::data_streams::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::DataStream.def()
    }
}

impl Related<crate::routes::private::sites::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Site.def()
    }
}

impl Related<crate::routes::private::parameters::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Parameter.def()
    }
}

impl Related<crate::routes::private::sensors::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Sensor.def()
    }
}

impl Related<crate::routes::private::sensor_calibrations::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SensorCalibration.def()
    }
}

impl Related<crate::routes::private::standard_curves::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::StandardCurve.def()
    }
}

impl Related<crate::routes::private::sensor_deployments::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SensorDeployment.def()
    }
}

impl Related<crate::routes::private::readings::samples::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Sample.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

/// `POST /streams/retag` and the `measurement_retag` job take this alongside the vocabulary: it
/// writes nothing to `data_streams` and aligns each reading with its own stream's declaration.
pub const RETAG_DECLARED: &str = "declared";

pub(super) fn expected(extra: &[&str]) -> String {
    let mut names: Vec<&str> = MeasurementType::ALL
        .iter()
        .map(MeasurementType::as_str)
        .collect();
    names.extend_from_slice(extra);
    let last = names.pop().expect("the vocabulary is never empty");
    format!("{}, or {last}", names.join(", "))
}

/// Why this classification is not admissible, or `None` when it is. Callers that refuse the whole
/// request raise it as a 400; callers that skip the offending reading need the reason as a value.
pub fn measurement_type_rejection(value: Option<&str>) -> Option<String> {
    match value {
        None => None,
        Some(other) => MeasurementType::parse(other).is_none().then(|| {
            format!(
                "invalid measurement_type '{other}' (expected {})",
                expected(&[])
            )
        }),
    }
}

/// Reject anything outside the readings.measurement_type vocabulary with a clean 400 (the DB has
/// no CHECK on readings.measurement_type, so bad values would otherwise persist silently).
pub fn validate_measurement_type(value: Option<&str>) -> Result<(), AppError> {
    measurement_type_rejection(value).map_or(Ok(()), |reason| Err(AppError::BadRequest(reason)))
}

/// Why this retag target is not admissible, or `None` when it is. The route and the job body both
/// read it, so a stored job row replayed by rerun is held to the same vocabulary as the request
/// that made it.
pub fn retag_target_rejection(value: &str) -> Option<String> {
    (MeasurementType::parse(value).is_none() && value != RETAG_DECLARED).then(|| {
        format!(
            "invalid measurement_type '{value}' (expected {})",
            expected(&[RETAG_DECLARED])
        )
    })
}

/// Map each sensor to the measurement_type its `data_frequency` implies: 'low' → 'spot'
/// (lab/campaign cadence), 'high' → 'continuous'. One query for the whole batch.
pub async fn measurement_types_for_sensors<C: ConnectionTrait>(
    db: &C,
    sensor_ids: &[Uuid],
) -> Result<HashMap<Uuid, &'static str>, sea_orm::DbErr> {
    if sensor_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sensors::Entity::find()
        .filter(sensors::Column::Id.is_in(sensor_ids.to_vec()))
        .all(db)
        .await?;
    let mut map = HashMap::with_capacity(rows.len());
    for sensor in rows {
        map.insert(
            sensor.id,
            if sensor.data_frequency == "low" {
                MeasurementType::Spot.as_str()
            } else {
                MeasurementType::Continuous.as_str()
            },
        );
    }
    Ok(map)
}

/// The instrument a `/readings/batch` row is attributed to, in precedence order.
///
/// The row's own comes first: a batch is an attributed write, so a row that names an instrument is
/// naming the one that measured it. The slot's deployment comes next, because a batch is keyed by
/// (site, parameter) and the deployment is what says which instrument stood in that slot at the
/// time. The stream's own instrument is the last resort, since a batch stream is a container the
/// API opened rather than a source that declared itself.
#[must_use]
pub fn batch_instrument(
    row: Option<Uuid>,
    slot: Option<Uuid>,
    stream: Option<Uuid>,
) -> Option<Uuid> {
    row.or(slot).or(stream)
}

/// The site and parameter a batch row keyed by (site, parameter) is attributed to: the request's
/// pair when a `site_parameters` slot backs it, and neither when the site was never assigned the
/// parameter, since attribution comes from a slot and never from the request alone.
#[must_use]
pub fn slot_attribution(
    site_id: Uuid,
    parameter_id: Uuid,
    slot: Option<Uuid>,
) -> (Option<Uuid>, Option<Uuid>) {
    match slot {
        Some(_) => (Some(site_id), Some(parameter_id)),
        None => (None, None),
    }
}

/// Relative distance within which a submitted `calibrated_value` is taken to be the one its
/// calibration produces.
const CORRECTION_REL_TOL: f64 = 1e-9;

/// The calibration a `/readings/batch` row is stored against and the value stored with it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BatchCorrection {
    pub calibration_id: Option<Uuid>,
    pub calibrated_value: Option<f64>,
}

/// A submitted `calibrated_value` that the row's calibration does not produce.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
#[error(
    "calibrated_value {submitted} is not what calibration {calibration_id} produces from the raw \
     value ({computed}); omit calibrated_value to have it computed"
)]
pub struct CorrectionMismatch {
    pub calibration_id: Uuid,
    pub submitted: f64,
    pub computed: f64,
}

/// What a `/readings/batch` row stores as its correction, so the stamped `calibration_id` is
/// always a curve the stored value went through.
///
/// A standard curve composes on the base and its result is stored whatever was submitted. A row
/// on a resolved base stores the value the base produces, and a submitted value that differs is
/// refused. A row with no calibration keeps a submitted value as an unaccounted correction. A
/// `calibration_id` that resolved to no curve is passed through for the insert to judge.
pub fn batch_correction(
    raw_value: f64,
    submitted: Option<f64>,
    calibration_id: Option<Uuid>,
    base: Option<sensor_calibrations::service::Curve>,
    standard: Option<sensor_calibrations::service::Curve>,
) -> Result<BatchCorrection, CorrectionMismatch> {
    let calibrated_value = match (standard, base, submitted) {
        (Some(curve), _, _) => Some(sensor_calibrations::service::apply_curves(
            raw_value,
            base,
            Some(curve),
        )),
        (None, Some(curve), submitted) => {
            let computed = curve.apply(raw_value);
            if let Some(value) = submitted
                && (value - computed).abs() > CORRECTION_REL_TOL * computed.abs().max(1.0)
            {
                return Err(CorrectionMismatch {
                    calibration_id: curve.id,
                    submitted: value,
                    computed,
                });
            }
            Some(computed)
        }
        (None, None, submitted) => submitted,
    };
    Ok(BatchCorrection {
        calibration_id,
        calibrated_value,
    })
}

/// The instrument an `/ingest` row is attributed to, in precedence order.
///
/// The stream's own comes before the slot's deployment, which is the opposite of
/// [`batch_instrument`] and is deliberate: an ingested reading arrives on a registered stream whose
/// instrument was frozen when the source registered it, and that is a stronger statement about what
/// measured the value than whatever deployment happens to cover the slot now.
#[must_use]
pub fn ingest_instrument(
    row: Option<Uuid>,
    stream: Option<Uuid>,
    slot: Option<Uuid>,
) -> Option<Uuid> {
    row.or(stream).or(slot)
}

/// Resolve one reading's measurement_type. Most specific wins:
/// explicit per-reading override → stream-level default → owning sensor's data_frequency →
/// 'continuous'.
pub fn resolve_measurement_type(
    override_value: Option<&str>,
    stream_default: Option<&str>,
    sensor_id: Option<Uuid>,
    sensor_types: &HashMap<Uuid, &'static str>,
) -> String {
    override_value
        .or(stream_default)
        .map(str::to_string)
        .or_else(|| {
            sensor_id
                .and_then(|id| sensor_types.get(&id))
                .map(|t| (*t).to_string())
        })
        .unwrap_or_else(|| MeasurementType::Continuous.as_str().to_string())
}

/// `readings r JOIN data_streams ds`, the shape every row predicate written against `r` is
/// resolved over: the stream join is what makes a source-system clause selectable alongside a
/// reading's own columns.
pub(super) fn readings_joined(r: Alias) -> sea_orm::sea_query::SelectStatement {
    let ds = Alias::new("ds");
    Query::select()
        .from_as(readings::Entity, r.clone())
        .join_as(
            JoinType::InnerJoin,
            data_streams::Entity,
            ds.clone(),
            Expr::col((ds, data_streams::Column::Id)).equals((r, readings::Column::StreamId)),
        )
        .to_owned()
}

/// One `readings` column under the `r` alias every row predicate is written against.
pub fn r(column: readings::Column) -> Expr {
    Expr::col((Alias::new("r"), column))
}

/// `COALESCE(calibrated_value, raw_value)`, the number every reader of a reading takes as its
/// value. `alias` names the table alias the columns are qualified by, `None` for an unaliased
/// `readings`.
pub(super) fn effective_value(alias: Option<&str>) -> Expr {
    let col = |c: readings::Column| match alias {
        Some(a) => Expr::col((sea_orm::sea_query::Alias::new(a), c)),
        None => Expr::col(c),
    };
    sea_orm::sea_query::Func::coalesce([
        col(readings::Column::CalibratedValue),
        col(readings::Column::RawValue),
    ])
    .into()
}

/// The `TRUE` keyword. `IS TRUE` and `IS NOT TRUE` read a nullable boolean as false where it is
/// NULL; the right-hand side has to be the keyword, because a bound `true` emits `IS $1`, which is
/// not SQL.
fn sql_true() -> sea_orm::sea_query::Keyword {
    sea_orm::sea_query::Keyword::Custom(sea_orm::sea_query::IntoIden::into_iden(Alias::new("TRUE")))
}

/// A readings select narrowed to the columns a [`PreviewRow`] decodes: the slot, the replicate's
/// served value, and each reason it may be excluded from the statistics.
pub(super) fn preview_rows() -> sea_orm::Select<readings::Entity> {
    readings::Entity::find()
        .select_only()
        .column(readings::Column::SiteId)
        .column(readings::Column::ParameterId)
        .column(readings::Column::ReplicateIndex)
        .column_as(effective_value(None), "value")
        .column_as(
            Expr::col(readings::Column::IsFlagged).is(sql_true()),
            "flagged",
        )
        .column_as(
            Expr::col(readings::Column::WithdrawnAt).is_not_null(),
            "withdrawn",
        )
        .column_as(
            Expr::col(readings::Column::Unverified).is(sql_true()),
            "unverified",
        )
}

/// Why a replicate survived a grab replace, in the order the reasons are checked.
pub(super) fn kept_reason() -> Expr {
    sea_orm::sea_query::CaseStatement::new()
        .case(
            Expr::col(readings::Column::IsFlagged).is(sql_true()),
            "flagged",
        )
        .case(
            Expr::col(readings::Column::WithdrawnAt).is_not_null(),
            "withdrawn",
        )
        .finally("standard_curve")
        .into()
}

/// The replicates a grab replace leaves standing: curation always, and a hand-curved row unless
/// the request supplies a curve of its own.
pub(super) fn curated_or_curved(supplies_curve: bool) -> Condition {
    let mut kept = Condition::any()
        .add(Expr::col(readings::Column::IsFlagged).is(sql_true()))
        .add(readings::Column::WithdrawnAt.is_not_null());
    if !supplies_curve {
        kept = kept.add(readings::Column::StandardCurveId.is_not_null());
    }
    kept
}

/// Grabs are spot measurements: a bottle, not a logger cadence.
pub const SPOT: &str = river_data_core::models::MeasurementType::Spot.as_str();

/// How many readings sharing a slot instant make a sample. A single measurement is a reading, not
/// a group of them: mean, min and max would be the value and sd undefined, so the row would only
/// denormalise what the reading already says.
pub const MIN_REPLICATES: usize = 2;

/// A `(site, parameter, instant)` group forms a `samples` row when it carries two or more spot
/// readings on a paired slot, whoever wrote them and whatever they declared.
///
/// A spot instant with no sample row is by definition a single measurement, which is why serving
/// and every export derive n = 1 from the reading rather than from a row here.
#[must_use]
pub const fn forms_sample(replicates: usize) -> bool {
    replicates >= MIN_REPLICATES
}

/// The groups a selection of readings forms, as SQL: unstamped spot readings on an attributed slot,
/// grouped by `(site, parameter, instant)`, kept when the group reaches [`MIN_REPLICATES`] or when
/// the instant already has a sample for a late replicate to join.
/// The instants a stamping pass groups: a slot's unstamped spot readings at one time, where the
/// group is either big enough to be a sample or already has one (a late replicate joins it).
pub(super) fn group_select(rows: Condition) -> sea_orm::sea_query::SelectStatement {
    let r = Alias::new("r");
    let s2 = Alias::new("s2");
    let already_a_sample = Query::select()
        .expr(Expr::val(1))
        .from_as(samples::Entity, s2.clone())
        .cond_where(
            Condition::all()
                .add(
                    Expr::col((s2.clone(), samples::Column::SiteId))
                        .equals((r.clone(), readings::Column::SiteId)),
                )
                .add(
                    Expr::col((s2.clone(), samples::Column::ParameterId))
                        .equals((r.clone(), readings::Column::ParameterId)),
                )
                .add(
                    Expr::col((s2, samples::Column::CollectedAt))
                        .equals((r.clone(), readings::Column::Time)),
                ),
        )
        .to_owned();
    readings_joined(r.clone())
        .column((r.clone(), readings::Column::SiteId))
        .column((r.clone(), readings::Column::ParameterId))
        .column((r.clone(), readings::Column::Time))
        .cond_where(
            rows.add(Expr::col((r.clone(), readings::Column::SampleId)).is_null())
                .add(Expr::col((r.clone(), readings::Column::SiteId)).is_not_null())
                .add(Expr::col((r.clone(), readings::Column::ParameterId)).is_not_null())
                .add(Expr::col((r.clone(), readings::Column::MeasurementType)).eq(SPOT)),
        )
        .add_group_by([
            Expr::col((r.clone(), readings::Column::SiteId)),
            Expr::col((r.clone(), readings::Column::ParameterId)),
            Expr::col((r, readings::Column::Time)),
        ])
        .cond_having(
            Condition::any()
                .add(Expr::cust("COUNT(*)").gte(i64::try_from(MIN_REPLICATES).unwrap_or(i64::MAX)))
                .add(Expr::exists(already_a_sample)),
        )
        .to_owned()
}

/// Find or create the sample of every group a predicate's readings form, then stamp the
/// `sample_id` onto the readings of those groups.
///
/// `rows` selects which readings are in scope over the aliases `r` (`readings`) and `ds`
/// (`data_streams`), and is applied to the grouping and the stamping alike, so the stamping
/// cannot reach an unrelated stream's reading sitting on the same slot at the same instant.
/// Grouping is always by `(site_id, parameter_id, time)`, the `samples` unique key, so the
/// find-or-create and the stamping cannot disagree about what a group is.
///
/// A group whose sample already exists takes the late replicate whatever the unstamped count is:
/// the rule is about how many readings share the instant, not how many arrived in this write.
pub async fn materialise_samples<C: ConnectionTrait>(conn: &C, rows: Condition) -> AppResult<()> {
    let g = Alias::new("g");
    let groups = Query::select()
        .column((g.clone(), samples::Column::SiteId))
        .column((g.clone(), samples::Column::ParameterId))
        .column((g.clone(), readings::Column::Time))
        .from_subquery(group_select(rows.clone()), g.clone())
        .to_owned();
    let mut insert = Query::insert();
    insert
        .into_table(samples::Entity)
        .columns([
            samples::Column::SiteId,
            samples::Column::ParameterId,
            samples::Column::CollectedAt,
        ])
        .select_from(groups)
        .map_err(|e| AppError::Internal(format!("materialising samples: {e}")))?
        .on_conflict(
            sea_orm::sea_query::OnConflict::columns([
                samples::Column::SiteId,
                samples::Column::ParameterId,
                samples::Column::CollectedAt,
            ])
            .do_nothing()
            .to_owned(),
        );
    let (sql, values) = insert.build(PostgresQueryBuilder);
    conn.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .await?;

    // The stamping UPDATE can reach chunks the compression policy already closed.
    bulk_write::lift_decompression_cap(conn).await?;
    let r = Alias::new("r");
    let ds = Alias::new("ds");
    let sample = Alias::new("s");
    let stamped = rows.clone();
    let (sql, values) = Query::update()
        .table(sea_orm::sea_query::IntoTableRef::into_table_ref(readings::Entity).alias(r.clone()))
        .value(
            readings::Column::SampleId,
            Expr::col((sample.clone(), samples::Column::Id)),
        )
        .from(
            sea_orm::sea_query::IntoTableRef::into_table_ref(data_streams::Entity)
                .alias(ds.clone()),
        )
        .from(sea_orm::sea_query::TableRef::SubQuery(
            Box::new(group_select(rows)),
            sea_orm::sea_query::IntoIden::into_iden(g.clone()),
        ))
        .from(
            sea_orm::sea_query::IntoTableRef::into_table_ref(samples::Entity).alias(sample.clone()),
        )
        .cond_where(
            Condition::all()
                .add(
                    Expr::col((r.clone(), readings::Column::StreamId))
                        .equals((ds, data_streams::Column::Id)),
                )
                .add(
                    Expr::col((sample.clone(), samples::Column::SiteId))
                        .equals((g.clone(), samples::Column::SiteId)),
                )
                .add(
                    Expr::col((sample.clone(), samples::Column::ParameterId))
                        .equals((g.clone(), samples::Column::ParameterId)),
                )
                .add(
                    Expr::col((sample, samples::Column::CollectedAt))
                        .equals((g.clone(), readings::Column::Time)),
                )
                .add(
                    Expr::col((r.clone(), readings::Column::SiteId))
                        .equals((g.clone(), samples::Column::SiteId)),
                )
                .add(
                    Expr::col((r.clone(), readings::Column::ParameterId))
                        .equals((g.clone(), samples::Column::ParameterId)),
                )
                .add(
                    Expr::col((r.clone(), readings::Column::Time))
                        .equals((g, readings::Column::Time)),
                )
                .add(Expr::col((r.clone(), readings::Column::SampleId)).is_null())
                .add(Expr::col((r, readings::Column::MeasurementType)).eq(SPOT))
                .add(stamped),
        )
        .to_owned()
        .build(PostgresQueryBuilder);
    conn.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .await?;

    Ok(())
}

/// One stored replicate as the preview_statistics reads it.
#[derive(Debug, Clone, Copy)]
pub struct Replicate {
    pub index: i16,
    pub value: f64,
    pub flagged: bool,
    pub withdrawn: bool,
    pub unverified: bool,
}

/// Whether a replicate counts in its sample's statistics: the rule the samples trigger applies,
/// which leaves out a flagged, a withdrawn and a pending (unverified) replicate.
#[must_use]
pub fn counts_in_sample(flagged: bool, withdrawn: bool, unverified: bool) -> bool {
    !flagged && !withdrawn && !unverified
}

/// The change being previewed.
#[derive(Debug, Clone, Default)]
pub struct Change<'a> {
    pub exclude: &'a [i16],
    pub include: &'a [i16],
}

pub(super) fn stats_over(values: &[f64]) -> PreviewStats {
    let s = audit::group_stats(values);
    PreviewStats {
        n: s.n,
        mean: s.mean,
        sd: s.sd,
    }
}

/// The statistics now and after the change, over the same rule the samples trigger applies
/// ([`counts_in_sample`]). A withdrawn or pending replicate is outside every count and no flag
/// change brings it back.
#[must_use]
pub fn preview_statistics(
    replicates: &[Replicate],
    change: &Change<'_>,
) -> (
    PreviewStats,
    PreviewStats,
    PreviewDelta,
    Vec<PreviewReplicate>,
) {
    let mut rows = Vec::with_capacity(replicates.len());
    let mut now = Vec::new();
    let mut after = Vec::new();
    for r in replicates {
        let included_now = counts_in_sample(r.flagged, r.withdrawn, r.unverified);
        let flagged_after = r.flagged && !change.include.contains(&r.index);
        let included_after = counts_in_sample(flagged_after, r.withdrawn, r.unverified)
            && !change.exclude.contains(&r.index);
        if included_now {
            now.push(r.value);
        }
        if included_after {
            after.push(r.value);
        }
        rows.push(PreviewReplicate {
            index: r.index,
            value: r.value,
            flagged: r.flagged,
            withdrawn: r.withdrawn,
            included_now,
            included_after,
        });
    }
    let current = stats_over(&now);
    let proposed = stats_over(&after);
    let delta = PreviewDelta {
        n: i64::try_from(proposed.n).unwrap_or(0) - i64::try_from(current.n).unwrap_or(0),
        mean: proposed.mean.zip(current.mean).map(|(p, c)| p - c),
        sd: proposed.sd.zip(current.sd).map(|(p, c)| p - c),
    };
    (current, proposed, delta, rows)
}

/// Whether statistics meet a hold's recorded expectation, under the tolerances the audit itself
/// compares with.
#[must_use]
pub fn hold_match(
    hold_id: Uuid,
    expected: &GroupAudit,
    now: &PreviewStats,
    after: &PreviewStats,
) -> HoldMatch {
    let as_group = |s: &PreviewStats| GroupStats {
        n: s.n,
        mean: s.mean,
        sd: s.sd,
    };
    let after_group = as_group(after);
    HoldMatch {
        hold_id,
        expected_mean: expected.expected_mean,
        expected_sd: expected.expected_sd,
        expected_n: expected.expected_n,
        meets_now: audit::agrees(expected, &as_group(now)),
        meets_after: audit::agrees(expected, &after_group),
        mean_agrees: audit::stats_agree(expected.expected_mean, after.mean, audit::DEFAULT_REL_TOL),
        sd_agrees: audit::stats_agree_with(
            expected.expected_sd,
            after.sd,
            audit::SD_REL_TOL,
            audit::SD_ABS_TOL,
        ),
        n_agrees: expected
            .expected_n
            .is_none_or(|n| i64::try_from(after.n) == Ok(n)),
    }
}

/// Half-width of the seasonal window: the entry month plus and minus this many months.
pub const WINDOW_MONTHS: i32 = 2;

/// Modulus of the cyclic month distance in [`month_distance`].
const MONTHS_IN_YEAR: i32 = 12;

/// Cap on the per-parameter distribution sample returned for plotting.
pub(super) const DISTRIBUTION_CAP: i64 = 500;

/// Months between a stored instant and the one being screened, counted the short way round the
/// year, so December is two months from February.
fn month_distance(column: readings::Column, at: sea_orm::prelude::DateTimeWithTimeZone) -> Expr {
    let month_of = |e: Expr| {
        Func::cust(Alias::new("date_part"))
            .arg("month")
            .arg(e)
            .cast_as(Alias::new("int"))
    };
    let stored = month_of(Expr::col(column));
    let screened = || month_of(Expr::val(at).cast_as(Alias::new("timestamptz")));
    let forward = stored
        .clone()
        .sub(screened())
        .add(MONTHS_IN_YEAR)
        .modulo(MONTHS_IN_YEAR);
    let backward = screened()
        .sub(stored)
        .add(MONTHS_IN_YEAR)
        .modulo(MONTHS_IN_YEAR);
    Func::cust(Alias::new("LEAST"))
        .arg(forward)
        .arg(backward)
        .into()
}

/// The rows the window pools: one slot's unflagged, non-withdrawn spot replicates whose month is
/// within [`WINDOW_MONTHS`] of the entry month, cyclically, across every year.
pub(super) fn pooled_rows(
    site_id: Uuid,
    parameter_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
) -> sea_orm::sea_query::SelectStatement {
    let at = sea_orm::prelude::DateTimeWithTimeZone::from(time);
    Query::select()
        .expr_as(Expr::col(readings::Column::RawValue), Alias::new("v"))
        .from(readings::Entity)
        .cond_where(
            Condition::all()
                .add(readings::Column::SiteId.eq(site_id))
                .add(readings::Column::ParameterId.eq(parameter_id))
                .add(readings::Column::MeasurementType.eq("spot"))
                .add(Expr::col(readings::Column::IsFlagged).is_not(sql_true()))
                .add(readings::Column::WithdrawnAt.is_null())
                .add(Expr::col(readings::Column::Unverified).is_not(sql_true()))
                // Cyclic month distance, so December is two months from February.
                .add(month_distance(readings::Column::Time, at).lte(WINDOW_MONTHS)),
        )
        .to_owned()
}

/// The method description for the query in [`pooled_rows`] and the classification in
/// `classify`.
#[must_use]
pub fn method() -> SeasonalMethod {
    SeasonalMethod {
        window_months: WINDOW_MONTHS,
        window: format!(
            "Same site and parameter, entry month ±{WINDOW_MONTHS} months (cyclic, so December \
             is two months from February), across every year of stored history."
        ),
        pooled: "Spot (grab) readings only. Flagged, withdrawn and pending (unverified) readings \
                 are excluded. Replicates enter as individual values, not as their sample mean.",
        value: "The stored raw value, compared against the entered number as typed; \
                corrections enter on neither side.",
        statistics: "Minimum, maximum, and the 10th and 90th percentiles of the pooled values \
                     (percentile_cont, linear interpolation between ranks).",
        classes: SeasonalClass::ALL
            .iter()
            .map(|c| SeasonalClassDescription {
                class: *c,
                meaning: c.meaning(),
                warning: c.is_warning(),
            })
            .collect(),
    }
}

/// Classify with the extremes before the quantiles, so a value beyond the recorded range reports
/// as beyond it.
#[must_use]
pub fn classify(
    value: f64,
    min: Option<f64>,
    q10: Option<f64>,
    q90: Option<f64>,
    max: Option<f64>,
) -> SeasonalClass {
    let (Some(min), Some(max)) = (min, max) else {
        return SeasonalClass::NoHistory;
    };
    if value < min {
        return SeasonalClass::BelowMin;
    }
    if value > max {
        return SeasonalClass::AboveMax;
    }
    if let Some(q10) = q10
        && value < q10
    {
        return SeasonalClass::BelowQ10;
    }
    if let Some(q90) = q90
        && value > q90
    {
        return SeasonalClass::AboveQ90;
    }
    SeasonalClass::Normal
}

/// min/Q10/Q90/max over the slot's pooled window for `time`.
pub async fn seasonal_stats(
    db: &impl ConnectionTrait,
    site_id: Uuid,
    parameter_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
) -> AppResult<SeasonalStats> {
    let (sql, values) = Query::select()
        .expr_as(Expr::cust("COUNT(*)"), Alias::new("n"))
        .expr_as(Func::min(Expr::col(Alias::new("v"))), Alias::new("min"))
        .expr_as(Func::max(Expr::col(Alias::new("v"))), Alias::new("max"))
        .expr_as(
            Expr::cust("percentile_cont(0.1) WITHIN GROUP (ORDER BY v)"),
            Alias::new("q10"),
        )
        .expr_as(
            Expr::cust("percentile_cont(0.9) WITHIN GROUP (ORDER BY v)"),
            Alias::new("q90"),
        )
        .from_subquery(
            pooled_rows(site_id, parameter_id, time),
            Alias::new("pooled"),
        )
        .to_owned()
        .build(PostgresQueryBuilder);
    SeasonalStats::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .one(db)
    .await?
    .ok_or_else(|| AppError::Internal("seasonal stats query returned nothing".into()))
}

/// The most recent pooled values, capped, for the distribution plot.
pub(super) async fn seasonal_distribution(
    db: &impl ConnectionTrait,
    site_id: Uuid,
    parameter_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
) -> AppResult<Vec<f64>> {
    let (sql, values) = pooled_rows(site_id, parameter_id, time)
        .order_by(readings::Column::Time, Order::Desc)
        .limit(u64::try_from(DISTRIBUTION_CAP).unwrap_or(u64::MAX))
        .to_owned()
        .build(PostgresQueryBuilder);
    Ok(
        PooledValue::find_by_statement(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .all(db)
        .await?
        .into_iter()
        .map(|r| r.v)
        .collect(),
    )
}

/// Screen many cells at once, each against the window its own instant anchors. The statistics
/// are computed once per (parameter, month) rather than once per cell, so a file with thousands
/// of rows costs one query per distinct slot and month.
pub async fn screen_cells(
    db: &impl ConnectionTrait,
    site_id: Uuid,
    cells: &[(usize, Uuid, chrono::DateTime<chrono::Utc>, f64)],
) -> AppResult<Vec<ScreenedCell>> {
    use chrono::Datelike;
    let mut stats: HashMap<(Uuid, u32), SeasonalStats> = HashMap::new();
    let mut out = Vec::with_capacity(cells.len());
    for (row, parameter_id, time, value) in cells {
        let key = (*parameter_id, time.month());
        let s = match stats.get(&key) {
            Some(s) => *s,
            None => {
                let s = seasonal_stats(db, site_id, *parameter_id, *time).await?;
                stats.insert(key, s);
                s
            }
        };
        let class = s.classify(*value);
        out.push(ScreenedCell {
            row: *row,
            parameter_id: *parameter_id,
            value: *value,
            class,
            warning: class.is_warning(),
            n: s.n,
            min: s.min,
            max: s.max,
        });
    }
    Ok(out)
}

/// Store a check row and return its id: the reference a save is later held to.
pub async fn store_check(
    db: &impl ConnectionTrait,
    site_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
    entries: &[SeasonalCheckValue],
    actor: String,
) -> AppResult<Uuid> {
    let check = seasonal_check::ActiveModel {
        id: Set(Uuid::new_v4()),
        site_id: Set(site_id),
        checked_time: Set(time),
        entries: Set(serde_json::to_value(entries).unwrap_or(serde_json::Value::Null)),
        created_by: Set(Some(actor)),
        ..Default::default()
    }
    .insert(db)
    .await?;
    Ok(check.id)
}

/// Validate a save's claimed check: it must belong to the same site and cover every
/// `(parameter, value)` the save writes. A pair the check did not screen is the "edited after
/// checking" case and is refused, so the gate cannot be satisfied by a stale check.
pub async fn validate_check_claim(
    db: &sea_orm::DatabaseConnection,
    check_id: Uuid,
    site_id: Uuid,
    pairs: &[(Uuid, f64)],
) -> AppResult<()> {
    let row = seasonal_check::Entity::find_by_id(check_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::BadRequest(format!("Check {check_id} does not exist")))?;
    if row.site_id != site_id {
        return Err(AppError::BadRequest(
            "The named check screened values for a different site".to_string(),
        ));
    }
    let checked: Vec<SeasonalCheckValue> =
        serde_json::from_value(row.entries).map_err(|e| AppError::Internal(e.to_string()))?;
    for (parameter_id, value) in pairs {
        let covered = checked
            .iter()
            .any(|c| c.parameter_id == *parameter_id && c.value == *value);
        if !covered {
            return Err(AppError::Conflict(format!(
                "Value {value} for parameter {parameter_id} was not screened by check \
                 {check_id}; values edited after a check need a fresh check"
            )));
        }
    }
    Ok(())
}

/// Every code path that writes a curation column, and what it becomes under the record: a
/// curation writer appends a decision of the given kind; a derivation writer appends nothing and
/// must honour pins. A new writer declares itself here or the classification test fails.
///
/// The derivations that move a stored value are the exception (Q116, Q118, Q125): a recompute
/// records the move, because the ledger is the one place a value's history is read from and a
/// value that changed under a new formula version, under a curve the sweep repaired it to, or
/// under the timelines a reprocess re-derives from, would otherwise leave no trace of having
/// changed. A reprocess records only the readings it moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writer {
    FlagRoute,
    UnflagRoute,
    AuditResolveFlag,
    AuditReopen,
    WindowedWithdraw,
    WindowedReassert,
    CsvDisplacement,
    CsvDisplacementReversal,
    GrabCurveClaim,
    IngestCurveClaim,
    GrabReplace,
    IngestOverwrite,
    BatchOverwrite,
    ChainSave,
    MergeMove,
    ReprocessSensor,
    ReprocessSlot,
    CalibrationResolver,
    BackfillAttribution,
    PairingBackfill,
    JanitorRecompose,
    DerivedGapFill,
    MeasurementRetag,
    CurveRetirement,
    DerivedRecompute,
}

impl Writer {
    /// The decision a writer appends, or `None` for a derivation.
    #[must_use]
    pub fn decision(self) -> Option<(Kind, Origin)> {
        match self {
            Self::FlagRoute => Some((Kind::Flag, Origin::Manual)),
            Self::UnflagRoute => Some((Kind::Unflag, Origin::Manual)),
            Self::AuditResolveFlag => Some((Kind::Flag, Origin::Audit)),
            Self::AuditReopen => Some((Kind::Unflag, Origin::Audit)),
            Self::WindowedWithdraw => Some((Kind::Withdraw, Origin::Sync)),
            Self::WindowedReassert => Some((Kind::Reassert, Origin::Sync)),
            Self::CsvDisplacement => Some((Kind::Withdraw, Origin::Csv)),
            Self::CsvDisplacementReversal => Some((Kind::Reassert, Origin::Csv)),
            Self::GrabCurveClaim => Some((Kind::Curve, Origin::Manual)),
            Self::IngestCurveClaim => Some((Kind::Curve, Origin::Sync)),
            Self::GrabReplace => Some((Kind::ValueCorrection, Origin::Manual)),
            Self::IngestOverwrite => Some((Kind::ValueCorrection, Origin::Sync)),
            Self::BatchOverwrite => Some((Kind::ValueCorrection, Origin::Manual)),
            Self::ChainSave => Some((Kind::Chain, Origin::Chain)),
            Self::MergeMove => Some((Kind::SlotMove, Origin::Manual)),
            Self::CurveRetirement => Some((Kind::CurveRetire, Origin::Manual)),
            Self::DerivedRecompute => Some((Kind::FormulaTransition, Origin::System)),
            Self::JanitorRecompose => Some((Kind::CurveRecompose, Origin::Janitor)),
            Self::DerivedGapFill => Some((Kind::DerivedComputed, Origin::System)),
            Self::ReprocessSensor | Self::ReprocessSlot => Some((Kind::Reprocess, Origin::System)),
            Self::CalibrationResolver
            | Self::BackfillAttribution
            | Self::PairingBackfill
            | Self::MeasurementRetag => None,
        }
    }
}

/// The columns every writer of the ledger names, in the one order they all name them in. A column
/// added to the table is added here and lands in every INSERT, rather than in the one whose author
/// noticed it; the per-writer columns (`reason`, `set_id`, `job_id`) follow this list.
pub const DECISION_COLUMNS: &str =
    "stream_id, time, replicate_index, kind, old, new, actor, origin, supersedes";

/// The reading, or the whole replicate group, a decision is about.
#[derive(Debug, Clone, Copy)]
pub struct DecisionKey {
    pub stream_id: Uuid,
    pub time: chrono::DateTime<chrono::Utc>,
    /// `None` for a decision over every replicate at the instant.
    pub replicate_index: Option<i16>,
}

/// A decision to record. `old` is captured by the record from the reading itself.
#[derive(Debug, Clone)]
pub struct Decision {
    pub key: DecisionKey,
    pub kind: Kind,
    pub new: serde_json::Value,
    pub actor: String,
    pub reason: Option<String>,
    pub origin: Origin,
    pub set_id: Option<Uuid>,
}

/// One stored row as the readers expose it. The two vocabularies are parsed here, because a value
/// outside either is a corrupt row rather than a decode failure and says so.
pub(super) fn row_from(stored: decision_model::Model) -> AppResult<DecisionRow> {
    let kind = stored.kind;
    let origin = stored.origin;
    let parsed = Kind::parse(&kind)
        .ok_or_else(|| AppError::Internal(format!("unknown decision kind {kind}")))?;
    Ok(DecisionRow {
        id: stored.id,
        stream_id: stored.stream_id,
        time: stored.time.with_timezone(&chrono::Utc),
        replicate_index: stored.replicate_index,
        kind: parsed,
        reversible: parsed.reversible(),
        old: stored.old,
        new: stored.new,
        actor: stored.actor,
        at: stored.at.with_timezone(&chrono::Utc),
        reason: stored.reason,
        origin: match origin.as_str() {
            "manual" => Origin::Manual,
            "sync" => Origin::Sync,
            "csv" => Origin::Csv,
            "audit" => Origin::Audit,
            "chain" => Origin::Chain,
            "rollback" => Origin::Rollback,
            "system" => Origin::System,
            "janitor" => Origin::Janitor,
            other => {
                return Err(AppError::Internal(format!(
                    "unknown decision origin {other}"
                )));
            }
        },
        supersedes: stored.supersedes,
        rolled_back_by: stored.rolled_back_by,
        set_id: stored.set_id,
        job_id: stored.job_id,
    })
}

pub(super) fn key_binds(key: &DecisionKey) -> Vec<sea_orm::Value> {
    vec![
        key.stream_id.into(),
        sea_orm::prelude::DateTimeWithTimeZone::from(key.time).into(),
        key.replicate_index.into(),
    ]
}

/// The projected state of the columns `kind` touches, read from the key's lowest replicate. This
/// is what the decision records as `old`. `None` when nothing is stored at the key.
pub(super) async fn current_state<C: ConnectionTrait>(
    conn: &C,
    key: &DecisionKey,
    kind: Kind,
) -> AppResult<Option<serde_json::Value>> {
    let (sql, values) = Query::select()
        .expr_as(state_object(None), Alias::new("state"))
        .from(readings::Entity)
        .cond_where(
            Condition::all()
                .add(readings::Column::StreamId.eq(key.stream_id))
                .add(readings::Column::Time.eq(key.time))
                .add(match key.replicate_index {
                    Some(index) => Condition::all().add(readings::Column::ReplicateIndex.eq(index)),
                    None => Condition::all(),
                }),
        )
        .order_by(readings::Column::ReplicateIndex, Order::Asc)
        .limit(1)
        .to_owned()
        .build(PostgresQueryBuilder);
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let state: serde_json::Value = row.try_get("", "state")?;
    Ok(Some(old_state_for(kind, &state)))
}

/// The subset of a row's projected state a kind records as `old`.
#[must_use]
pub fn old_state_for(kind: Kind, state: &serde_json::Value) -> serde_json::Value {
    let mut old = serde_json::Map::new();
    for col in kind.recorded_columns() {
        old.insert(
            (*col).to_string(),
            state.get(*col).cloned().unwrap_or(serde_json::Value::Null),
        );
    }
    serde_json::Value::Object(old)
}

/// The latest live decision of `family` on the key: the one a new decision of that family
/// supersedes.
pub(super) async fn latest_live<C: ConnectionTrait>(
    conn: &C,
    key: &DecisionKey,
    family: &str,
) -> AppResult<Option<Uuid>> {
    let kinds: Vec<String> = [
        Kind::Flag,
        Kind::Unflag,
        Kind::Withdraw,
        Kind::Reassert,
        Kind::Reject,
        Kind::Curve,
        Kind::CalibrationPin,
        Kind::InstrumentPin,
        Kind::SlotMove,
        Kind::ValueCorrection,
        Kind::UnverifiedEntry,
        Kind::Verify,
        Kind::Chain,
        Kind::Detach,
        Kind::Return,
        Kind::CurveRetire,
    ]
    .into_iter()
    .filter(|k| k.family() == Some(family))
    .map(|k| k.as_str().to_string())
    .collect();
    let index = decision_model::Column::ReplicateIndex;
    let row = decision_model::Entity::find()
        .filter(decision_model::Column::StreamId.eq(key.stream_id))
        .filter(decision_model::Column::Time.eq(key.time))
        .filter(match key.replicate_index {
            Some(i) => index.eq(i),
            None => index.is_null(),
        })
        .filter(decision_model::Column::Kind.is_in(kinds))
        .filter(decision_model::Column::RolledBackBy.is_null())
        .order_by_desc(decision_model::Column::At)
        .order_by_desc(decision_model::Column::Id)
        .one(conn)
        .await?;
    Ok(row.map(|r| r.id))
}

/// Append a decision and, through the table's trigger, project it onto the reading. Runs on the
/// caller's connection, which must be a guarded transaction when the row can sit in a compressed
/// chunk. Refuses a key nothing is stored at, and a per-row kind recorded for a whole group.
pub async fn record<C: ConnectionTrait>(conn: &C, d: &Decision) -> AppResult<Uuid> {
    if d.kind == Kind::Rollback {
        return Err(AppError::BadRequest(
            "A rollback is recorded through rollback(), not as a decision of its own".to_string(),
        ));
    }
    refuse_historical(d.kind)?;
    if d.kind.per_row_only() && d.key.replicate_index.is_none() {
        return Err(AppError::BadRequest(format!(
            "A {} names one replicate, not a group",
            d.kind.as_str()
        )));
    }
    let Some(old) = current_state(conn, &d.key, d.kind).await? else {
        return Err(AppError::NotFound(format!(
            "No reading at stream {} / {} / replicate {:?}",
            d.key.stream_id, d.key.time, d.key.replicate_index
        )));
    };
    let supersedes = match d.kind.family() {
        Some(family) => latest_live(conn, &d.key, family).await?,
        None => None,
    };
    let id = Uuid::new_v4();
    decision_model::ActiveModel {
        id: Set(id),
        stream_id: Set(d.key.stream_id),
        time: Set(d.key.time.into()),
        replicate_index: Set(d.key.replicate_index),
        kind: Set(d.kind.as_str().to_string()),
        old: Set(old),
        new: Set(d.new.clone()),
        actor: Set(d.actor.clone()),
        at: NotSet,
        reason: Set(d.reason.clone()),
        origin: Set(d.origin.as_str().to_string()),
        supersedes: Set(supersedes),
        rolled_back_by: Set(None),
        set_id: Set(d.set_id),
        job_id: Set(None),
        seq: NotSet,
    }
    .insert(conn)
    .await?;
    if d.kind.recomposes() {
        recompose_corrected(
            conn,
            "r.stream_id = $1 AND r.time = $2 \
             AND ($3::smallint IS NULL OR r.replicate_index = $3)",
            key_binds(&d.key),
        )
        .await?;
    }
    Ok(id)
}

/// Invert one decision: append a `rollback` carrying the state the decision recorded as `old`,
/// which the trigger restores, and stamp the decision `rolled_back_by`. A decision already rolled
/// back, or a rollback itself, is refused: the way forward from there is a fresh decision.
pub async fn rollback<C: ConnectionTrait>(
    conn: &C,
    decision_id: Uuid,
    actor: &str,
    reason: Option<&str>,
) -> AppResult<(Uuid, Recorded)> {
    let d = load(conn, decision_id).await?;
    if d.rolled_back_by.is_some() {
        return Err(AppError::Conflict(format!(
            "Decision {decision_id} was already rolled back"
        )));
    }
    if d.kind == Kind::Rollback {
        return Err(AppError::Conflict(
            "A rollback is not rolled back; record the decision again instead".to_string(),
        ));
    }
    if d.kind.projected_columns().is_empty() {
        return Err(AppError::Conflict(format!(
            "A {} projects nothing, so there is nothing to restore",
            d.kind.as_str()
        )));
    }
    let restore = old_state_for(d.kind, &d.old);
    let key = DecisionKey {
        stream_id: d.stream_id,
        time: d.time,
        replicate_index: d.replicate_index,
    };
    let Some(current) = current_state(conn, &key, d.kind).await? else {
        return Err(AppError::NotFound(format!(
            "No reading at stream {} / {} / replicate {:?}",
            key.stream_id, key.time, key.replicate_index
        )));
    };
    // Read before the projection, as the forward path does: the visit membership is what the
    // caller enqueues on, and a predicate over the state about to change matches nothing after.
    let recorded = Recorded {
        rows: 1,
        span: Some((
            d.time.with_timezone(&chrono::Utc),
            d.time.with_timezone(&chrono::Utc),
        )),
        touched_events: if d.kind.fires_recompute() {
            crate::routes::private::collection_events::flows::touched_events(conn, {
                use crate::routes::private::collection_events::flows::row;
                Condition::all()
                    .add(row(readings::Column::StreamId).eq(key.stream_id))
                    .add(row(readings::Column::Time).eq(key.time))
                    .add_option(
                        key.replicate_index
                            .map(|i| row(readings::Column::ReplicateIndex).eq(i)),
                    )
            })
            .await?
        } else {
            Vec::new()
        },
    };
    let rollback_id = Uuid::new_v4();
    decision_model::ActiveModel {
        id: Set(rollback_id),
        stream_id: Set(key.stream_id),
        time: Set(key.time.into()),
        replicate_index: Set(key.replicate_index),
        kind: Set(Kind::Rollback.as_str().to_string()),
        old: Set(current),
        new: Set(serde_json::json!({ "columns": restore, "of": decision_id })),
        actor: Set(actor.to_string()),
        at: NotSet,
        reason: Set(reason.map(ToString::to_string)),
        origin: Set(Origin::Rollback.as_str().to_string()),
        supersedes: Set(None),
        rolled_back_by: Set(None),
        set_id: Set(None),
        job_id: Set(None),
        seq: NotSet,
    }
    .insert(conn)
    .await?;
    decision_model::ActiveModel {
        id: Set(decision_id),
        rolled_back_by: Set(Some(rollback_id)),
        ..Default::default()
    }
    .update(conn)
    .await?;
    if d.kind.recomposes() {
        recompose_corrected(
            conn,
            "r.stream_id = $1 AND r.time = $2 \
             AND ($3::smallint IS NULL OR r.replicate_index = $3)",
            key_binds(&key),
        )
        .await?;
    }
    Ok((rollback_id, recorded))
}

/// What a bulk record did: rows decided, the time span they cover (for the aggregate refresh),
/// and the visits they touched (for the reactive hook, enqueued by the caller after commit).
#[derive(Debug, Default)]
pub struct Recorded {
    pub rows: u64,
    pub span: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>,
    pub touched_events: Vec<crate::routes::private::collection_events::flows::TouchedEvent>,
}

impl Recorded {
    /// Fold another record in, so a caller writing per stream reports one span and one set of
    /// visits rather than one per stream.
    pub fn absorb(&mut self, other: Self) {
        self.rows += other.rows;
        self.span = match (self.span, other.span) {
            (Some((a, b)), Some((c, d))) => Some((Ord::min(a, c), Ord::max(b, d))),
            (x, None) => x,
            (None, y) => y,
        };
        self.touched_events.extend(other.touched_events);
    }

    #[must_use]
    pub fn touched(&self) -> crate::common::bulk_write::TouchedRange {
        crate::common::bulk_write::TouchedRange {
            rows: self.rows,
            min_time: self.span.map(|(lo, _)| lo),
            max_time: self.span.map(|(_, hi)| hi),
        }
    }
}

/// How a bulk record fills `new`.
#[derive(Debug, Clone)]
pub enum NewValue {
    /// The same assertion for every row.
    Literal(serde_json::Value),
    /// The row's own projected state of the kind's columns, for a decision recorded over rows
    /// born with the state already in place (an insert-time curve claim): `old` is then null.
    Born,
    /// [`NewValue::Born`] with extra keys merged into `new`: what a computed row's arrival
    /// consumed (Q215), which is not a column of the row.
    BornWith(serde_json::Value),
    /// A jsonb expression evaluated once per row against `r` (`readings`), for a decision whose
    /// assertion differs per reading: a curve retirement moves each of its readings onto whichever
    /// curve covers that reading, which is a different answer per row. The expression carries its
    /// own binds, because it is placed in a statement whose other values it cannot see.
    Sql(Expr),
}

/// The projected columns a decision records, as one jsonb object. `alias` names the table alias
/// the reading is read under, `None` for an unaliased `readings`.
pub(super) fn state_object(alias: Option<&str>) -> Expr {
    let col = |c: readings::Column| match alias {
        Some(a) => Expr::col((Alias::new(a), c)),
        None => Expr::col(c),
    };
    let pair = |name: &'static str, value: Expr| [Expr::val(name), value];
    sea_orm::sea_query::Func::cust(Alias::new("jsonb_build_object"))
        .args(
            [
                pair(
                    "is_flagged",
                    sea_orm::sea_query::Func::coalesce([
                        col(readings::Column::IsFlagged),
                        Expr::val(false),
                    ])
                    .into(),
                ),
                pair("flag_reason", col(readings::Column::FlagReason)),
                pair("withdrawn_at", col(readings::Column::WithdrawnAt)),
                pair("withdrawn_reason", col(readings::Column::WithdrawnReason)),
                pair("standard_curve_id", col(readings::Column::StandardCurveId)),
                pair("sensor_id", col(readings::Column::SensorId)),
                pair("calibration_id", col(readings::Column::CalibrationId)),
                pair("raw_value", col(readings::Column::RawValue)),
                pair("calibrated_value", col(readings::Column::CalibratedValue)),
                pair("unverified", col(readings::Column::Unverified)),
                pair("ingested_at", col(readings::Column::IngestedAt)),
                pair(
                    "derived_version_id",
                    col(readings::Column::DerivedVersionId),
                ),
                pair(
                    "run_id",
                    col(readings::Column::Provenance)
                        .binary(PgBinOper::CastJsonField, Expr::val("run_id")),
                ),
                pair("site_id", col(readings::Column::SiteId)),
                pair("parameter_id", col(readings::Column::ParameterId)),
            ]
            .concat(),
        )
        .into()
}

/// Which rows a keyed record decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyed {
    /// Every key that exists.
    All,
    /// Only keys whose projected state does not already contain `new` (a correction that
    /// changes nothing is not a decision).
    Changed,
    /// Only keys with no live decision of this kind already asserting `new` (a claim re-sent
    /// every cycle is recorded once).
    Claim,
}

/// Every kind, in declaration order. The enumerations below filter this rather than repeat it.
pub(super) const ALL_KINDS: [Kind; 17] = [
    Kind::Flag,
    Kind::Unflag,
    Kind::Withdraw,
    Kind::Reassert,
    Kind::Curve,
    Kind::CalibrationPin,
    Kind::InstrumentPin,
    Kind::SlotMove,
    Kind::ValueCorrection,
    Kind::UnverifiedEntry,
    Kind::Verify,
    Kind::Reject,
    Kind::Chain,
    Kind::Detach,
    Kind::Return,
    Kind::CurveRetire,
    Kind::Rollback,
];

pub(super) fn family_kinds(kind: Kind) -> Vec<String> {
    [
        Kind::Flag,
        Kind::Unflag,
        Kind::Withdraw,
        Kind::Reassert,
        Kind::Reject,
        Kind::Curve,
        Kind::CalibrationPin,
        Kind::InstrumentPin,
        Kind::SlotMove,
        Kind::ValueCorrection,
        Kind::UnverifiedEntry,
        Kind::Verify,
        Kind::Chain,
        Kind::Detach,
        Kind::Return,
    ]
    .into_iter()
    .filter(|k| k.family().is_some() && k.family() == kind.family())
    .map(|k| k.as_str().to_string())
    .collect()
}

/// The live decision of this kind's family standing on the same reading key, which the row being
/// written supersedes. NULL where the key carries none. `target` is the alias the enclosing select
/// reads its keys from.
pub(crate) fn supersedes(kind: Kind, target: &Alias) -> Expr {
    let d = Alias::new("d");
    Expr::from(
        Query::select()
            .column((d.clone(), decision_model::Column::Id))
            .from_as(decision_model::Entity, d.clone())
            .cond_where(
                Condition::all()
                    .add(
                        Expr::col((d.clone(), decision_model::Column::StreamId))
                            .equals((target.clone(), decision_model::Column::StreamId)),
                    )
                    .add(
                        Expr::col((d.clone(), decision_model::Column::Time))
                            .equals((target.clone(), decision_model::Column::Time)),
                    )
                    .add(
                        Expr::col((d.clone(), decision_model::Column::ReplicateIndex)).binary(
                            sea_orm::sea_query::BinOper::Custom("IS NOT DISTINCT FROM"),
                            Expr::col((target.clone(), decision_model::Column::ReplicateIndex)),
                        ),
                    )
                    .add(
                        Expr::col((d.clone(), decision_model::Column::Kind))
                            .is_in(family_kinds(kind)),
                    )
                    .add(Expr::col((d.clone(), decision_model::Column::RolledBackBy)).is_null()),
            )
            .order_by((d.clone(), decision_model::Column::At), Order::Desc)
            .order_by((d, decision_model::Column::Id), Order::Desc)
            .limit(1)
            .take(),
    )
}

/// Record one decision per reading a predicate selects, in one statement, capturing each row's
/// prior state as `old` and naming the decision each supersedes. `row_predicate` is SQL over
/// `r` (`readings`) and `ds` (`data_streams`) with `binds` numbered from `$1`.
#[allow(clippy::too_many_arguments)]
/// Refuse a kind nothing writes any more, naming what to correct instead.
pub(super) fn refuse_historical(kind: Kind) -> AppResult<()> {
    if kind.writable() {
        return Ok(());
    }
    Err(AppError::BadRequest(format!(
        "'{}' is not recorded any more: a reading's instrument comes from its deployment and its \
         correction from the calibration window, so the fix is to that record and the reprocess \
         carries it through",
        kind.as_str()
    )))
}

#[allow(clippy::too_many_arguments)]
pub async fn record_many<C: ConnectionTrait>(
    conn: &C,
    kind: Kind,
    rows: Condition,
    new: NewValue,
    actor: &str,
    reason: Option<&str>,
    origin: Origin,
    set_id: Option<Uuid>,
) -> AppResult<Recorded> {
    if kind == Kind::Rollback {
        return Err(AppError::BadRequest(
            "A rollback is recorded through rollback(), not in bulk".to_string(),
        ));
    }
    refuse_historical(kind)?;
    let cols: Vec<String> = kind
        .recorded_columns()
        .iter()
        .map(|c| (*c).to_string())
        .collect();
    // `old` is always the state the decision found; `new` is the literal, the same state (a
    // row born under the decision records nulls as its before) or the resolved expression.
    let touched_columns = || {
        Expr::cust_with_values(
            "(SELECT COALESCE(jsonb_object_agg(k, t.state -> k), '{}'::jsonb) \
              FROM unnest($1::text[]) AS k)",
            [sea_orm::Value::from(cols.clone())],
        )
    };
    let nulled_columns = || {
        Expr::cust_with_values(
            "(SELECT COALESCE(jsonb_object_agg(k, 'null'::jsonb), '{}'::jsonb) \
              FROM unnest($1::text[]) AS k)",
            [sea_orm::Value::from(cols.clone())],
        )
    };
    let (old_expr, new_expr) = match &new {
        NewValue::Literal(value) => (
            touched_columns(),
            Expr::cust_with_values("$1::jsonb", [sea_orm::Value::from(value.clone())]),
        ),
        NewValue::Born => (nulled_columns(), touched_columns()),
        NewValue::BornWith(extra) => (
            nulled_columns(),
            Expr::cust_with_values(
                "(SELECT COALESCE(jsonb_object_agg(k, t.state -> k), '{}'::jsonb) \
                  FROM unnest($1::text[]) AS k) || $2::jsonb",
                [
                    sea_orm::Value::from(cols.clone()),
                    sea_orm::Value::from(extra.clone()),
                ],
            ),
        ),
        NewValue::Sql(_) => (
            touched_columns(),
            Expr::col((Alias::new("t"), Alias::new("resolved"))),
        ),
    };
    let resolved = match &new {
        NewValue::Sql(expr) => expr.clone(),
        _ => Expr::cust("NULL::jsonb"),
    };

    let r = Alias::new("r");
    let ds = Alias::new("ds");
    let t = Alias::new("t");
    let target = Query::select()
        .column((r.clone(), readings::Column::StreamId))
        .column((r.clone(), readings::Column::Time))
        .column((r.clone(), readings::Column::ReplicateIndex))
        .expr_as(state_object(Some("r")), Alias::new("state"))
        .expr_as(resolved, Alias::new("resolved"))
        .from_as(readings::Entity, r.clone())
        .join_as(
            JoinType::InnerJoin,
            data_streams::Entity,
            ds.clone(),
            Expr::col((ds, data_streams::Column::Id))
                .equals((r.clone(), readings::Column::StreamId)),
        )
        .cond_where(rows.clone())
        .to_owned();

    let source = Query::select()
        .column((t.clone(), decision_model::Column::StreamId))
        .column((t.clone(), decision_model::Column::Time))
        .column((t.clone(), decision_model::Column::ReplicateIndex))
        .expr(Expr::val(kind.as_str()))
        .expr(old_expr)
        .expr(new_expr)
        .expr(Expr::val(actor))
        .expr(Expr::val(origin.as_str()))
        .expr(supersedes(kind, &t))
        .expr(Expr::val(reason))
        .expr(Expr::val(set_id))
        .from_as(Alias::new("target"), t)
        .to_owned();

    let (sql, values) = decisions_from(target, source)?;

    // The visits the decisions touch are read before the insert: the predicate may name the
    // state the projection is about to change, so afterwards it would match nothing.
    let touched_events = if kind.fires_recompute() {
        crate::routes::private::collection_events::flows::touched_events(conn, rows).await?
    } else {
        Vec::new()
    };
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?
        .ok_or_else(|| AppError::Internal("recording decisions returned no row".to_string()))?;
    let RecordedSpan { rows, lo, hi } = RecordedSpan::from_query_result(&row, "")?;
    let rows = u64::try_from(rows).unwrap_or(0);
    Ok(Recorded {
        rows,
        span: lo
            .zip(hi)
            .map(|(a, b)| (a.with_timezone(&chrono::Utc), b.with_timezone(&chrono::Utc))),
        touched_events: if rows > 0 { touched_events } else { Vec::new() },
    })
}

/// The keys a keyed record decides, one per position of three parallel arrays, so a batch of any
/// size is one statement and one round trip.
///
/// The three unnests share a select list, which Postgres steps in lockstep, and which is what
/// names `t`, `ri` and `n` for the join and the mode filters that read them.
fn key_set(
    times: Vec<chrono::DateTime<chrono::Utc>>,
    indices: Vec<i16>,
    news: Vec<String>,
) -> sea_orm::sea_query::SelectStatement {
    let news = Expr::val(news).cast_as(Alias::new("jsonb[]"));
    key_pairs(times, indices)
        .expr_as(unnest(news), Alias::new("n"))
        .to_owned()
}

/// The `(t, ri)` pairs of two parallel arrays, stepped in lockstep.
fn key_pairs(
    times: Vec<chrono::DateTime<chrono::Utc>>,
    indices: Vec<i16>,
) -> sea_orm::sea_query::SelectStatement {
    Query::select()
        .expr_as(unnest(Expr::val(times)), Alias::new("t"))
        .expr_as(unnest(Expr::val(indices)), Alias::new("ri"))
        .to_owned()
}

fn unnest(array: Expr) -> Expr {
    Expr::from(sea_orm::sea_query::Func::cust(Alias::new("unnest")).arg(array))
}

/// The statement both recorders run: the rows in scope as `target`, the decision each one owes
/// as `source`, the insert as `ins`, and the count and span of what it wrote as the result.
fn decisions_from(
    target: sea_orm::sea_query::SelectStatement,
    source: sea_orm::sea_query::SelectStatement,
) -> AppResult<(String, sea_orm::sea_query::Values)> {
    let mut ins = Query::insert();
    ins.into_table(decision_model::Entity)
        .columns([
            decision_model::Column::StreamId,
            decision_model::Column::Time,
            decision_model::Column::ReplicateIndex,
            decision_model::Column::Kind,
            decision_model::Column::Old,
            decision_model::Column::New,
            decision_model::Column::Actor,
            decision_model::Column::Origin,
            decision_model::Column::Supersedes,
            decision_model::Column::Reason,
            decision_model::Column::SetId,
        ])
        .select_from(source)
        .map_err(|e| AppError::Internal(format!("recording decisions: {e}")))?
        .returning_col(decision_model::Column::Time);
    let with = WithClause::new()
        .cte(
            CommonTableExpression::new()
                .table_name(Alias::new("target"))
                .query(target)
                .to_owned(),
        )
        .cte(
            CommonTableExpression::new()
                .table_name(Alias::new("ins"))
                .query(ins)
                .to_owned(),
        )
        .to_owned();
    Ok(Query::select()
        .expr_as(Expr::cust("count(*)::bigint"), Alias::new("rows"))
        .expr_as(
            Func::min(Expr::col(decision_model::Column::Time)),
            Alias::new("lo"),
        )
        .expr_as(
            Func::max(Expr::col(decision_model::Column::Time)),
            Alias::new("hi"),
        )
        .from(Alias::new("ins"))
        .to_owned()
        .with(with)
        .build(PostgresQueryBuilder))
}

/// One decision per explicit key on one stream, each with its own `new`. Keys nothing stores
/// are skipped; `mode` says which of the rest are decided.
#[allow(clippy::too_many_arguments)]
pub async fn record_keyed<C: ConnectionTrait>(
    conn: &C,
    kind: Kind,
    stream_id: Uuid,
    rows: &[(chrono::DateTime<chrono::Utc>, i16, serde_json::Value)],
    actor: &str,
    reason: Option<&str>,
    origin: Origin,
    mode: Keyed,
    guard: Option<&str>,
    set_id: Option<Uuid>,
) -> AppResult<Recorded> {
    if rows.is_empty() {
        return Ok(Recorded::default());
    }
    refuse_historical(kind)?;
    let times: Vec<chrono::DateTime<chrono::Utc>> = rows.iter().map(|(t, _, _)| *t).collect();
    let indices: Vec<i16> = rows.iter().map(|(_, i, _)| *i).collect();
    let news: Vec<String> = rows.iter().map(|(_, _, n)| n.to_string()).collect();
    let cols: Vec<String> = kind
        .recorded_columns()
        .iter()
        .map(|c| (*c).to_string())
        .collect();

    let r = Alias::new("r");
    let k = Alias::new("k");
    let t = Alias::new("t");
    let d = Alias::new("d");
    let state = state_object(Some("r"));
    let keys = key_set(times.clone(), indices.clone(), news);
    let already_decided = Query::select()
        .expr(Expr::val(1))
        .from_as(decision_model::Entity, d.clone())
        .cond_where(
            Condition::all()
                .add(
                    Expr::col((d.clone(), decision_model::Column::StreamId))
                        .equals((r.clone(), readings::Column::StreamId)),
                )
                .add(
                    Expr::col((d.clone(), decision_model::Column::Time))
                        .equals((r.clone(), readings::Column::Time)),
                )
                .add(
                    Expr::col((d.clone(), decision_model::Column::ReplicateIndex)).binary(
                        sea_orm::sea_query::BinOper::Custom("IS NOT DISTINCT FROM"),
                        Expr::col((r.clone(), readings::Column::ReplicateIndex)),
                    ),
                )
                .add(Expr::col((d.clone(), decision_model::Column::Kind)).eq(kind.as_str()))
                .add(Expr::col((d.clone(), decision_model::Column::RolledBackBy)).is_null())
                .add(
                    Expr::col((d.clone(), decision_model::Column::New))
                        .binary(PgBinOper::Contains, Expr::col((k.clone(), Alias::new("n")))),
                ),
        )
        .to_owned();
    let mut scope = Condition::all();
    match mode {
        Keyed::All => {}
        Keyed::Changed => {
            scope = scope.add(
                Expr::expr(state.clone())
                    .binary(PgBinOper::Contains, Expr::col((k.clone(), Alias::new("n"))))
                    .not(),
            );
        }
        Keyed::Claim => scope = scope.add(Expr::exists(already_decided).not()),
    }
    if let Some(g) = guard {
        scope = scope.add(Expr::cust(g.to_string()));
    }
    let target = Query::select()
        .column((r.clone(), readings::Column::StreamId))
        .column((r.clone(), readings::Column::Time))
        .column((r.clone(), readings::Column::ReplicateIndex))
        .expr_as(state, Alias::new("state"))
        .column((k.clone(), Alias::new("n")))
        .from(sea_orm::sea_query::TableRef::SubQuery(
            Box::new(keys),
            sea_orm::sea_query::IntoIden::into_iden(k.clone()),
        ))
        .join_as(
            JoinType::InnerJoin,
            readings::Entity,
            r.clone(),
            Expr::from(
                Condition::all()
                    .add(Expr::col((r.clone(), readings::Column::StreamId)).eq(stream_id))
                    .add(
                        Expr::col((r.clone(), readings::Column::Time))
                            .equals((k.clone(), Alias::new("t"))),
                    )
                    .add(
                        Expr::col((r, readings::Column::ReplicateIndex))
                            .equals((k, Alias::new("ri"))),
                    ),
            ),
        )
        .cond_where(scope)
        .to_owned();

    let source = Query::select()
        .column((t.clone(), decision_model::Column::StreamId))
        .column((t.clone(), decision_model::Column::Time))
        .column((t.clone(), decision_model::Column::ReplicateIndex))
        .expr(Expr::val(kind.as_str()))
        .expr(Expr::cust_with_values(
            "(SELECT COALESCE(jsonb_object_agg(c, t.state -> c), '{}'::jsonb) \
              FROM unnest($1::text[]) AS c)",
            [sea_orm::Value::from(cols)],
        ))
        .column((t.clone(), Alias::new("n")))
        .expr(Expr::val(actor))
        .expr(Expr::val(origin.as_str()))
        .expr(supersedes(kind, &t))
        .expr(Expr::val(reason))
        .expr(Expr::val(set_id))
        .from_as(Alias::new("target"), t)
        .to_owned();

    let (sql, values) = decisions_from(target, source)?;
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?
        .ok_or_else(|| AppError::Internal("recording decisions returned no row".to_string()))?;
    let RecordedSpan {
        rows: rows_n,
        lo,
        hi,
    } = RecordedSpan::from_query_result(&row, "")?;
    let mut recorded = Recorded {
        rows: u64::try_from(rows_n).unwrap_or(0),
        span: lo
            .zip(hi)
            .map(|(a, b)| (a.with_timezone(&chrono::Utc), b.with_timezone(&chrono::Utc))),
        touched_events: Vec::new(),
    };
    if recorded.rows > 0 && kind.fires_recompute() {
        recorded.touched_events = crate::routes::private::collection_events::flows::touched_events(
            conn,
            Condition::all()
                .add(
                    crate::routes::private::collection_events::flows::row(
                        readings::Column::StreamId,
                    )
                    .eq(stream_id),
                )
                .add(
                    Expr::tuple([
                        Expr::col((Alias::new("r"), readings::Column::Time)),
                        Expr::col((Alias::new("r"), readings::Column::ReplicateIndex)),
                    ])
                    .in_subquery(key_pairs(times, indices)),
                ),
        )
        .await?;
    }
    Ok(recorded)
}

pub(super) type Model = crate::routes::private::readings::models::ActiveModel;

pub(super) fn model_key(m: &Model) -> Option<(Uuid, chrono::DateTime<chrono::Utc>, i16)> {
    let stream = *m.stream_id.try_as_ref()?;
    let time = m.time.try_as_ref()?.with_timezone(&chrono::Utc);
    let index = *m.replicate_index.try_as_ref()?;
    Some((stream, time, index))
}

/// Group `(time, index, new)` rows by stream for a keyed record.
pub(super) fn by_stream(
    models: &[Model],
    new_for: impl Fn(&Model) -> Option<serde_json::Value>,
) -> HashMapByStream {
    let mut out: HashMapByStream = std::collections::HashMap::new();
    for m in models {
        let Some((stream, time, index)) = model_key(m) else {
            continue;
        };
        let Some(new) = new_for(m) else {
            continue;
        };
        out.entry(stream).or_default().push((time, index, new));
    }
    out
}

pub(super) type HashMapByStream =
    std::collections::HashMap<Uuid, Vec<(chrono::DateTime<chrono::Utc>, i16, serde_json::Value)>>;

/// Before an overwrite lands: one `value_correction` per stored row whose raw value the models
/// about to be written change. Run on the writing transaction, before the upsert.
pub async fn record_value_corrections<C: ConnectionTrait>(
    conn: &C,
    models: &[Model],
    actor: &str,
    origin: Origin,
) -> AppResult<Recorded> {
    let mut all = Recorded::default();
    for (stream, rows) in by_stream(models, |m| {
        m.raw_value
            .try_as_ref()
            .map(|v| serde_json::json!({ "raw_value": v }))
    }) {
        let r = record_keyed(
            conn,
            Kind::ValueCorrection,
            stream,
            &rows,
            actor,
            None,
            origin,
            Keyed::Changed,
            None,
            None,
        )
        .await?;
        all.rows += r.rows;
        all.touched_events.extend(r.touched_events);
    }
    Ok(all)
}

/// After a write lands: one `curve` decision per row carrying a hand-picked standard curve the
/// record does not already hold for it. Re-sent claims are recorded once.
pub async fn record_curve_claims<C: ConnectionTrait>(
    conn: &C,
    models: &[Model],
    actor: &str,
    origin: Origin,
) -> AppResult<Recorded> {
    let mut all = Recorded::default();
    for (stream, rows) in by_stream(models, |m| match m.standard_curve_id.try_as_ref() {
        Some(Some(id)) => Some(serde_json::json!({ "standard_curve_id": id })),
        _ => None,
    }) {
        let r = record_keyed(
            conn,
            Kind::Curve,
            stream,
            &rows,
            actor,
            Some("chosen with the entry"),
            origin,
            Keyed::Claim,
            None,
            None,
        )
        .await?;
        all.rows += r.rows;
    }
    Ok(all)
}

/// Before the chain rewrites an output: one `chain` decision per stored row a fresh run
/// supersedes, naming the run it replaces and the run that replaces it.
pub async fn record_chain_supersessions<C: ConnectionTrait>(
    conn: &C,
    models: &[Model],
    run_id: Uuid,
    actor: &str,
) -> AppResult<Recorded> {
    let mut all = Recorded::default();
    for (stream, rows) in by_stream(models, |_| Some(serde_json::json!({ "run_id": run_id }))) {
        let r = record_keyed(
            conn,
            Kind::Chain,
            stream,
            &rows,
            actor,
            Some("superseded by a recompute"),
            Origin::Chain,
            Keyed::Changed,
            None,
            None,
        )
        .await?;
        all.rows += r.rows;
    }
    Ok(all)
}

/// The live decisions covering the `readings` row `alias` names, as a correlated subquery.
/// `kind` confines it to one kind; `None` takes every kind.
fn live_decisions(alias: &str, kind: Option<Kind>) -> sea_orm::sea_query::SelectStatement {
    let r = Alias::new(alias);
    let d = Alias::new("d");
    Query::select()
        .expr(Expr::val(1))
        .from_as(decision_model::Entity, d.clone())
        .cond_where(
            Condition::all()
                .add(
                    Expr::col((d.clone(), decision_model::Column::StreamId))
                        .equals((r.clone(), readings::Column::StreamId)),
                )
                .add(
                    Expr::col((d.clone(), decision_model::Column::Time))
                        .equals((r.clone(), readings::Column::Time)),
                )
                .add(
                    Condition::any()
                        .add(
                            Expr::col((d.clone(), decision_model::Column::ReplicateIndex))
                                .is_null(),
                        )
                        .add(
                            Expr::col((d.clone(), decision_model::Column::ReplicateIndex))
                                .equals((r, readings::Column::ReplicateIndex)),
                        ),
                )
                .add_option(
                    kind.map(|k| {
                        Expr::col((d.clone(), decision_model::Column::Kind)).eq(k.as_str())
                    }),
                )
                .add(Expr::col((d, decision_model::Column::RolledBackBy)).is_null()),
        )
        .to_owned()
}

/// True over `alias` (a `readings` row) when no live pin of `kind` covers the row. Every
/// derivation that would rewrite the pinned column carries this, so a pin outlives reprocess.
#[must_use]
pub fn not_pinned(alias: &str, kind: Kind) -> SimpleExpr {
    Expr::exists(live_decisions(alias, Some(kind))).not()
}

/// A judgement is a person's ruling on a reading: whether it counts, which curve made it, what
/// instrument it belongs to, whether it has been reviewed. Sync owns the measurement, the value
/// and whether the source still asserts the row, and never a judgement (M61), so a re-send may
/// correct a judged row but never clears the ruling and never moves its servedness silently.
#[must_use]
pub fn is_judgement(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::Flag
            | Kind::Curve
            | Kind::CalibrationPin
            | Kind::InstrumentPin
            | Kind::UnverifiedEntry
            | Kind::Verify
            | Kind::Reject
    )
}

/// The judgements standing on a row, from its live decision kinds, newest first as given. Empty
/// means a re-send may correct and retract the row without anyone ruling again.
#[must_use]
pub fn judgements_on(live_kinds: &[Kind]) -> Vec<Kind> {
    live_kinds
        .iter()
        .copied()
        .filter(|k| is_judgement(*k))
        .collect()
}

/// Every kind [`is_judgement`] holds, so the statement and the function cannot disagree about
/// what a judgement is.
pub(super) fn judgement_kinds() -> Vec<&'static str> {
    ALL_KINDS
        .iter()
        .copied()
        .filter(|k| is_judgement(*k))
        .map(Kind::as_str)
        .collect()
}

/// A live judgement of `d` standing on the reading `alias` names: same key, the group-wide form
/// included, not rolled back, and of a kind a person rules with.
fn live_judgement_of(alias: &str, d: &Alias) -> Condition {
    let r = Alias::new(alias);
    Condition::all()
        .add(
            Expr::col((d.clone(), decision_model::Column::StreamId))
                .equals((r.clone(), readings::Column::StreamId)),
        )
        .add(
            Expr::col((d.clone(), decision_model::Column::Time))
                .equals((r.clone(), readings::Column::Time)),
        )
        .add(
            Condition::any()
                .add(Expr::col((d.clone(), decision_model::Column::ReplicateIndex)).is_null())
                .add(
                    Expr::col((d.clone(), decision_model::Column::ReplicateIndex))
                        .equals((r, readings::Column::ReplicateIndex)),
                ),
        )
        .add(Expr::col((d.clone(), decision_model::Column::RolledBackBy)).is_null())
        .add(Expr::col((d.clone(), decision_model::Column::Kind)).is_in(judgement_kinds()))
}

/// SQL over `alias` (a `readings` row) that is true when no live judgement stands on it, so a
/// writer that must not override a person's ruling can say so in one clause.
#[must_use]
pub fn unjudged(alias: &str) -> Expr {
    let d = Alias::new("d");
    Expr::exists(
        Query::select()
            .expr(Expr::val(1))
            .from_as(decision_model::Entity, d.clone())
            .cond_where(live_judgement_of(alias, &d))
            .to_owned(),
    )
    .not()
}

/// SQL over `alias` (a `readings` row) producing the live judgements standing on it as a jsonb
/// array of `{id, kind}`, newest first, or `'[]'`. This is what a `source_modified` hold names,
/// so an operator is told which of their rulings the re-send collided with.
#[must_use]
pub fn live_judgements(alias: &str) -> Expr {
    let d = Alias::new("d");
    let agg = Query::select()
        .expr(Expr::cust(
            "jsonb_agg(jsonb_build_object('id', d.id, 'kind', d.kind) \
             ORDER BY d.at DESC, d.id DESC)",
        ))
        .from_as(decision_model::Entity, d.clone())
        .cond_where(live_judgement_of(alias, &d))
        .to_owned();
    sea_orm::sea_query::Func::coalesce([
        Expr::SubQuery(
            None,
            Box::new(sea_orm::sea_query::SubQueryStatement::SelectStatement(agg)),
        ),
        Expr::cust("'[]'::jsonb"),
    ])
    .into()
}

/// The state a save lands in, by the caller's standing: an intern's entry is pending until a
/// manager verifies or rejects it (Q21). Every other level, and every API token, enters verified.
#[must_use]
pub fn entry_state(highest_role: Option<&crate::common::authz::Role>) -> Option<Kind> {
    match highest_role {
        Some(crate::common::authz::Role::Intern) => Some(Kind::UnverifiedEntry),
        _ => None,
    }
}

/// The state an entry lands in: a value computed from a measurement still awaiting verification is
/// pending itself (M62), whoever the chain ran as, and otherwise the writer's own level decides.
#[must_use]
pub fn entry_kind(
    pending_inputs: bool,
    highest_role: Option<&crate::common::authz::Role>,
) -> Option<Kind> {
    if pending_inputs {
        return Some(Kind::UnverifiedEntry);
    }
    entry_state(highest_role)
}

/// After an entry lands: one `unverified_entry` per row it wrote, so the review queue and every
/// exclusion read the same record the columns project from.
pub async fn record_unverified_entries<C: ConnectionTrait>(
    conn: &C,
    models: &[Model],
    actor: &str,
    origin: Origin,
) -> AppResult<Recorded> {
    let mut all = Recorded::default();
    for (stream, rows) in by_stream(models, |_| Some(serde_json::json!({ "unverified": true }))) {
        let r = record_keyed(
            conn,
            Kind::UnverifiedEntry,
            stream,
            &rows,
            actor,
            Some("entered by an intern, pending review"),
            origin,
            Keyed::Changed,
            None,
            None,
        )
        .await?;
        all.rows += r.rows;
    }
    Ok(all)
}

/// The reading columns the record owns outright: nothing outside the projection trigger writes
/// them, so each must equal the fold of the key's live decisions. The attribution and value
/// columns are deliberately absent, because a derivation rewrites those with no decision at all.
pub const FOLDED_COLUMNS: [&str; 5] = [
    "is_flagged",
    "flag_reason",
    "withdrawn_at",
    "withdrawn_reason",
    "unverified",
];

/// One live decision, as the fold reads it.
#[derive(Debug, Clone)]
pub struct FoldEntry {
    pub kind: Kind,
    pub at: chrono::DateTime<chrono::Utc>,
    pub new: serde_json::Value,
}

/// The folded columns of one reading. `Default` is the state a reading is born in, which is what
/// a column no decision asserts must equal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectedColumns {
    pub is_flagged: bool,
    pub flag_reason: Option<String>,
    pub withdrawn_at: Option<chrono::DateTime<chrono::Utc>>,
    pub withdrawn_reason: Option<String>,
    pub unverified: bool,
}

/// What one decision asserts about the folded columns, mirroring `reading_decisions_project()`.
/// A JSON null asserts the column empty; a kind that touches none of them asserts nothing.
pub(super) fn asserted_columns(entry: &FoldEntry) -> Vec<(&'static str, serde_json::Value)> {
    use serde_json::{Value, json};
    let reason = || entry.new.get("reason").cloned().unwrap_or(Value::Null);
    let withdrawn_at = || {
        entry
            .new
            .get("withdrawn_at")
            .cloned()
            .unwrap_or_else(|| json!(entry.at))
    };
    match entry.kind {
        Kind::Flag => vec![("is_flagged", json!(true)), ("flag_reason", reason())],
        Kind::Unflag => vec![("is_flagged", json!(false)), ("flag_reason", Value::Null)],
        Kind::Withdraw => vec![
            ("withdrawn_at", withdrawn_at()),
            ("withdrawn_reason", reason()),
        ],
        Kind::Reject => vec![
            ("withdrawn_at", withdrawn_at()),
            ("withdrawn_reason", reason()),
            ("unverified", json!(false)),
        ],
        Kind::Reassert => vec![
            ("withdrawn_at", Value::Null),
            ("withdrawn_reason", Value::Null),
        ],
        Kind::UnverifiedEntry => vec![("unverified", json!(true))],
        Kind::Verify => vec![("unverified", json!(false))],
        Kind::Rollback => entry
            .new
            .get("columns")
            .and_then(serde_json::Value::as_object)
            .map(|c| {
                FOLDED_COLUMNS
                    .iter()
                    .filter_map(|col| c.get(*col).map(|v| (*col, v.clone())))
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// The folded columns as the record says they stand: newest live decision asserting a column
/// wins, and a column nothing asserts stands as the reading was born. `newest_first` is the key's
/// live decisions, group decisions included, ordered by `at` descending.
#[must_use]
pub fn projected_state(newest_first: &[FoldEntry]) -> ProjectedColumns {
    let mut asserted: std::collections::HashMap<&'static str, serde_json::Value> =
        std::collections::HashMap::new();
    for entry in newest_first {
        for (col, value) in asserted_columns(entry) {
            asserted.entry(col).or_insert(value);
        }
    }
    let text = |col| {
        asserted
            .get(col)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let flag = |col| asserted.get(col).and_then(serde_json::Value::as_bool) == Some(true);
    ProjectedColumns {
        is_flagged: flag("is_flagged"),
        flag_reason: text("flag_reason"),
        withdrawn_at: asserted
            .get("withdrawn_at")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&chrono::Utc)),
        withdrawn_reason: text("withdrawn_reason"),
        unverified: flag("unverified"),
    }
}

/// Each column a decision asserts, with the expression that reads its asserted value off a
/// decision row. A jsonb projection has no builder; the statement folding it does.
const FOLD_ARMS: [(&str, &str); 9] = [
    (
        "is_flagged",
        "CASE d.kind WHEN 'flag' THEN 'true'::jsonb WHEN 'unflag' THEN 'false'::jsonb \
         WHEN 'rollback' THEN d.new -> 'columns' -> 'is_flagged' END",
    ),
    (
        "flag_reason",
        "CASE d.kind WHEN 'flag' THEN COALESCE(d.new -> 'reason', 'null'::jsonb) \
         WHEN 'unflag' THEN 'null'::jsonb \
         WHEN 'rollback' THEN d.new -> 'columns' -> 'flag_reason' END",
    ),
    (
        "withdrawn_at",
        "CASE WHEN d.kind IN ('withdraw', 'reject') \
                  THEN to_jsonb(COALESCE((d.new ->> 'withdrawn_at')::timestamptz, d.at)) \
              WHEN d.kind = 'reassert' THEN 'null'::jsonb \
              WHEN d.kind = 'rollback' THEN d.new -> 'columns' -> 'withdrawn_at' END",
    ),
    (
        "withdrawn_reason",
        "CASE WHEN d.kind IN ('withdraw', 'reject') \
                  THEN COALESCE(d.new -> 'reason', 'null'::jsonb) \
              WHEN d.kind = 'reassert' THEN 'null'::jsonb \
              WHEN d.kind = 'rollback' THEN d.new -> 'columns' -> 'withdrawn_reason' END",
    ),
    (
        "unverified",
        "CASE WHEN d.kind = 'unverified_entry' THEN 'true'::jsonb \
              WHEN d.kind IN ('verify', 'reject') THEN 'false'::jsonb \
              WHEN d.kind = 'rollback' THEN d.new -> 'columns' -> 'unverified' END",
    ),
    (
        "standard_curve_id",
        "CASE d.kind WHEN 'curve' THEN d.new -> 'standard_curve_id' \
         WHEN 'rollback' THEN d.new -> 'columns' -> 'standard_curve_id' END",
    ),
    (
        "calibration_id",
        "CASE d.kind WHEN 'calibration_pin' THEN d.new -> 'calibration_id' \
         WHEN 'rollback' THEN d.new -> 'columns' -> 'calibration_id' END",
    ),
    (
        "sensor_id",
        "CASE d.kind WHEN 'instrument_pin' THEN d.new -> 'sensor_id' \
         WHEN 'rollback' THEN d.new -> 'columns' -> 'sensor_id' END",
    ),
    (
        "raw_value",
        "CASE d.kind WHEN 'value_correction' THEN d.new -> 'raw_value' \
         WHEN 'rollback' THEN d.new -> 'columns' -> 'raw_value' END",
    ),
];

/// Where the row and the fold of its decisions disagree. `IS DISTINCT FROM` over a jsonb
/// extraction has no builder; the statement carrying it does.
const FOLD_DISAGREES: &str = "c.is_flagged IS DISTINCT FROM COALESCE((e.m ->> 'is_flagged')::boolean, false) \
     OR c.flag_reason IS DISTINCT FROM (e.m ->> 'flag_reason') \
     OR c.withdrawn_at IS DISTINCT FROM (e.m ->> 'withdrawn_at')::timestamptz \
     OR c.withdrawn_reason IS DISTINCT FROM (e.m ->> 'withdrawn_reason') \
     OR c.unverified IS DISTINCT FROM COALESCE((e.m ->> 'unverified')::boolean, false) \
     OR (jsonb_exists(e.m, 'standard_curve_id') \
         AND c.standard_curve_id IS DISTINCT FROM (e.m ->> 'standard_curve_id')::uuid) \
     OR (jsonb_exists(e.m, 'calibration_id') \
         AND c.calibration_id IS DISTINCT FROM (e.m ->> 'calibration_id')::uuid) \
     OR (jsonb_exists(e.m, 'sensor_id') AND c.sensor_id IS DISTINCT FROM (e.m ->> 'sensor_id')::uuid) \
     OR (jsonb_exists(e.m, 'raw_value') \
         AND c.raw_value IS DISTINCT FROM (e.m ->> 'raw_value')::double precision)";

/// The columns a decision can assert, read off one decision row as `(col, val)` pairs. One row of
/// `reading_decisions` yields nine, of which the kinds that assert nothing leave NULL.
fn fold_arms() -> sea_orm::sea_query::SelectStatement {
    let mut arms = FOLD_ARMS.iter().map(|(col, case)| {
        Query::select()
            .expr_as(Expr::val(*col), Alias::new("col"))
            .expr_as(Expr::cust(*case), Alias::new("val"))
            .to_owned()
    });
    let mut first = arms.next().expect("FOLD_ARMS is not empty");
    for arm in arms {
        first.union(sea_orm::sea_query::UnionType::All, arm);
    }
    first
}

/// The same fold as a statement, anti-joined against the readings: every key whose folded columns
/// are not what its live decisions say they should be, with the columns the reading holds and the
/// map its decisions fold to (`folded`), so a caller can show a person which side says what.
/// Report-only, because which side is wrong is a decision (a rollback, or a fresh decision), never
/// something a sweep may pick.
///
/// It folds every column a decision's own assertion determines. The one it cannot is
/// `calibrated_value`, which is recomposed from the row's own curves rather than asserted, so it
/// cannot be predicted from `new`; a decision records it as `old` for a rollback to restore. The
/// columns only some kinds assert are compared only where a decision asserted one, so a row
/// carrying an instrument nothing pinned is not drift.
#[must_use]
pub fn inconsistent_rows() -> sea_orm::sea_query::SelectStatement {
    let r = Alias::new("r");
    let c = Alias::new("c");
    let d = Alias::new("d");
    let v = Alias::new("v");
    let a = Alias::new("a");
    let e = Alias::new("e");
    let col = Alias::new("col");
    let val = Alias::new("val");

    let candidate = Query::select()
        .column((r.clone(), readings::Column::StreamId))
        .column((r.clone(), readings::Column::Time))
        .column((r.clone(), readings::Column::ReplicateIndex))
        .expr_as(
            Func::coalesce([
                Expr::col((r.clone(), readings::Column::IsFlagged)),
                Expr::val(false),
            ]),
            Alias::new("is_flagged"),
        )
        .column((r.clone(), readings::Column::FlagReason))
        .column((r.clone(), readings::Column::WithdrawnAt))
        .column((r.clone(), readings::Column::WithdrawnReason))
        .expr_as(
            Func::coalesce([
                Expr::col((r.clone(), readings::Column::Unverified)),
                Expr::val(false),
            ]),
            Alias::new("unverified"),
        )
        .column((r.clone(), readings::Column::StandardCurveId))
        .column((r.clone(), readings::Column::CalibrationId))
        .column((r.clone(), readings::Column::SensorId))
        .column((r.clone(), readings::Column::RawValue))
        .from_as(readings::Entity, r.clone())
        .cond_where(
            Condition::any()
                .add(Expr::col((r.clone(), readings::Column::IsFlagged)).is(sql_true()))
                .add(Expr::col((r.clone(), readings::Column::FlagReason)).is_not_null())
                .add(Expr::col((r.clone(), readings::Column::WithdrawnAt)).is_not_null())
                .add(Expr::col((r.clone(), readings::Column::WithdrawnReason)).is_not_null())
                .add(Expr::col((r.clone(), readings::Column::Unverified)).is(sql_true()))
                .add(Expr::exists(live_decisions("r", None))),
        )
        .to_owned();

    let live = Query::select()
        .distinct_on([(v.clone(), col.clone())])
        .column((v.clone(), col.clone()))
        .column((v.clone(), val.clone()))
        .from_as(decision_model::Entity, d.clone())
        .join_lateral(
            JoinType::InnerJoin,
            fold_arms(),
            v.clone(),
            Expr::cust("TRUE"),
        )
        .cond_where(
            Condition::all()
                .add(
                    Expr::col((d.clone(), decision_model::Column::StreamId))
                        .equals((c.clone(), Alias::new("stream_id"))),
                )
                .add(
                    Expr::col((d.clone(), decision_model::Column::Time))
                        .equals((c.clone(), Alias::new("time"))),
                )
                .add(
                    Condition::any()
                        .add(
                            Expr::col((d.clone(), decision_model::Column::ReplicateIndex))
                                .is_null(),
                        )
                        .add(
                            Expr::col((d.clone(), decision_model::Column::ReplicateIndex))
                                .equals((c.clone(), Alias::new("replicate_index"))),
                        ),
                )
                .add(Expr::col((d.clone(), decision_model::Column::RolledBackBy)).is_null())
                .add(Expr::col((v.clone(), val.clone())).is_not_null()),
        )
        .order_by((v.clone(), col.clone()), Order::Asc)
        .order_by((d.clone(), decision_model::Column::At), Order::Desc)
        .order_by((d, decision_model::Column::Id), Order::Desc)
        .to_owned();

    let folded = Query::select()
        .expr_as(
            Expr::cust("jsonb_object_agg(a.col, a.val)"),
            Alias::new("m"),
        )
        .from_subquery(live, a)
        .to_owned();

    Query::select()
        .expr(Expr::col((c.clone(), sea_orm::sea_query::Asterisk)))
        .expr_as(
            Expr::col((e.clone(), Alias::new("m"))),
            Alias::new("folded"),
        )
        .from_subquery(candidate, c.clone())
        .join_lateral(JoinType::LeftJoin, folded, e, Expr::cust("TRUE"))
        .cond_where(Expr::cust(FOLD_DISAGREES))
        .to_owned()
}

/// How many readings the record and the columns disagree about. The janitor reports it; nothing
/// repairs it.
pub async fn curation_drift_count<C: ConnectionTrait>(conn: &C) -> AppResult<i64> {
    let (sql, values) = Query::select()
        .expr_as(Expr::cust("count(*)::bigint"), Alias::new("n"))
        .from_subquery(inconsistent_rows(), Alias::new("drift"))
        .to_owned()
        .build(PostgresQueryBuilder);
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?
        .ok_or_else(|| AppError::Internal("counting curation drift returned no row".to_string()))?;
    Ok(row.try_get("", "n")?)
}

/// Record one decision per reading a selection covers, as one set. Returns the set id and what
/// was recorded.
/// Put the corrected value back under the rows a value correction or a curve edit touched.
///
/// The projection writes `raw_value` (leaving `calibrated_value` NULL) or `standard_curve_id`, and
/// never a corrected value, because that is a claim about the curves the row itself names rather
/// than a number a decision may record. This recomposes it from exactly those curves, so value and
/// provenance move together.
pub(super) async fn recompose_corrected<C: ConnectionTrait>(
    conn: &C,
    scope_sql: &str,
    params: Vec<sea_orm::Value>,
) -> AppResult<()> {
    crate::routes::private::sensor_calibrations::service::recompose_from_own_curves(
        conn,
        crate::routes::private::sensor_calibrations::service::corrected_rows("r"),
        scope_sql,
        params,
    )
    .await?;
    Ok(())
}

/// The per-key corrections a selection carries, grouped by stream, or None when the selection
/// asserts one value for every row it names.
///
/// A block of cells corrected together carries a different number in each, so the decision cannot
/// be one literal over a predicate. Mixing the two is refused rather than guessed: a key with no
/// value beside keys that have one is a selection nobody meant.
pub fn keyed_corrections(selection: &Selection) -> AppResult<Option<HashMapByStream>> {
    if !selection.keys.iter().any(|k| k.value.is_some()) {
        return Ok(None);
    }
    if selection.keys.iter().any(|k| k.value.is_none()) {
        return Err(AppError::BadRequest(
            "a correction naming a value per key names one for every key it selects".to_string(),
        ));
    }
    let mut by_stream: HashMapByStream = std::collections::HashMap::new();
    for key in &selection.keys {
        let Some(value) = key.value else { continue };
        let index = key.replicate_index.ok_or_else(|| {
            AppError::BadRequest(
                "a correction naming a value per key names the replicate each one belongs to"
                    .to_string(),
            )
        })?;
        by_stream.entry(key.stream_id).or_default().push((
            key.time,
            index,
            serde_json::json!({ "raw_value": value }),
        ));
    }
    Ok(Some(by_stream))
}

/// Open a decision set: the row a set of decisions belongs to, and what a rollback names. A writer
/// whose rows are chosen by something no [`Selection`] expresses records its decisions with
/// [`record_many`] between this and [`close_set`]; everything else goes through [`record_set`].
pub async fn open_set<C: ConnectionTrait>(
    conn: &C,
    kind: Kind,
    selection: &Selection,
    new: serde_json::Value,
    actor: &str,
    reason: Option<&str>,
) -> AppResult<Uuid> {
    let set = decision_set::ActiveModel {
        id: Set(Uuid::new_v4()),
        kind: Set(kind.as_str().to_string()),
        selection: Set(
            serde_json::to_value(selection).map_err(|e| AppError::Internal(e.to_string()))?
        ),
        new: Set(new),
        actor: Set(actor.to_string()),
        reason: Set(reason.map(ToString::to_string)),
        ..Default::default()
    }
    .insert(conn)
    .await?;
    Ok(set.id)
}

/// Record how many rows the set decided, which is what the surfaces report.
pub async fn close_set<C: ConnectionTrait>(conn: &C, set_id: Uuid, rows: u64) -> AppResult<()> {
    decision_set::Entity::update_many()
        .col_expr(
            decision_set::Column::RowsDecided,
            Expr::value(i64::try_from(rows).unwrap_or(i64::MAX)),
        )
        .filter(decision_set::Column::Id.eq(set_id))
        .exec(conn)
        .await?;
    Ok(())
}

pub async fn record_set<C: ConnectionTrait>(
    conn: &C,
    kind: Kind,
    selection: &Selection,
    new: serde_json::Value,
    actor: &str,
    reason: Option<&str>,
    origin: Origin,
) -> AppResult<(Uuid, Recorded)> {
    refuse_historical(kind)?;
    let rows = selection.condition()?;
    let set_id = open_set(conn, kind, selection, new.clone(), actor, reason).await?;
    let recorded = match keyed_corrections(selection)? {
        Some(by_stream) if kind == Kind::ValueCorrection => {
            let mut all = Recorded::default();
            for (stream_id, rows) in by_stream {
                let one = record_keyed(
                    conn,
                    kind,
                    stream_id,
                    &rows,
                    actor,
                    reason,
                    origin,
                    Keyed::All,
                    None,
                    Some(set_id),
                )
                .await?;
                all.rows += one.rows;
                all.span = match (all.span, one.span) {
                    (Some((a, b)), Some((c, d))) => Some((Ord::min(a, c), Ord::max(b, d))),
                    (x, None) => x,
                    (None, y) => y,
                };
                all.touched_events.extend(one.touched_events);
            }
            all
        }
        Some(_) => {
            return Err(AppError::BadRequest(
                "only a value correction names a value per key".to_string(),
            ));
        }
        None => {
            record_many(
                conn,
                kind,
                rows,
                NewValue::Literal(new),
                actor,
                reason,
                origin,
                Some(set_id),
            )
            .await?
        }
    };
    close_set(conn, set_id, recorded.rows).await?;
    if kind.recomposes() && recorded.rows > 0 {
        recompose_corrected(
            conn,
            "EXISTS (SELECT 1 FROM reading_decisions d WHERE d.set_id = $1 \
                       AND d.stream_id = r.stream_id AND d.time = r.time \
                       AND d.replicate_index = r.replicate_index)",
            vec![set_id.into()],
        )
        .await?;
    }
    Ok((set_id, recorded))
}

/// Roll back every live decision of a set, one rollback decision each, and stamp the set.
pub async fn rollback_set<C: ConnectionTrait>(
    conn: &C,
    set_id: Uuid,
    actor: &str,
    reason: Option<&str>,
) -> AppResult<(usize, Recorded)> {
    let already = decision_set::Entity::find_by_id(set_id)
        .one(conn)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Decision set {set_id} not found")))?;
    if already.rolled_back_at.is_some() {
        return Err(AppError::Conflict(format!(
            "Decision set {set_id} was already rolled back"
        )));
    }
    let live: Vec<Uuid> = decision_model::Entity::find()
        .select_only()
        .column(decision_model::Column::Id)
        .filter(decision_model::Column::SetId.eq(set_id))
        .filter(decision_model::Column::RolledBackBy.is_null())
        .into_tuple()
        .all(conn)
        .await?;
    let mut n = 0usize;
    let mut recorded = Recorded::default();
    for id in live {
        let (_, one) = rollback(conn, id, actor, reason).await?;
        recorded.rows += one.rows;
        recorded.span = match (recorded.span, one.span) {
            (Some((lo, hi)), Some((a, b))) => Some((Ord::min(lo, a), Ord::max(hi, b))),
            (existing, incoming) => existing.or(incoming),
        };
        recorded.touched_events.extend(one.touched_events);
        n += 1;
    }
    decision_set::Entity::update_many()
        .col_expr(
            decision_set::Column::RolledBackAt,
            Expr::current_timestamp(),
        )
        .col_expr(
            decision_set::Column::RolledBackBy,
            Expr::value(Some(actor.to_string())),
        )
        .filter(decision_set::Column::Id.eq(set_id))
        .exec(conn)
        .await?;
    Ok((n, recorded))
}

/// The slots a predicate's readings belong to, for the reprocess a pin enqueues.
pub(super) async fn slots_of<C: ConnectionTrait>(
    conn: &C,
    rows: Condition,
) -> AppResult<Vec<(Uuid, Uuid)>> {
    let r = sea_orm::sea_query::Alias::new("r");
    let query = sea_orm::sea_query::Query::select()
        .distinct()
        .column((r.clone(), readings::Column::SiteId))
        .column((r.clone(), readings::Column::ParameterId))
        .from_as(readings::Entity, r.clone())
        .cond_where(
            rows.add(Expr::col((r.clone(), readings::Column::SiteId)).is_not_null())
                .add(Expr::col((r, readings::Column::ParameterId)).is_not_null()),
        )
        .to_owned();
    let (sql, values) = query.build(sea_orm::sea_query::PostgresQueryBuilder);
    let rows = SlotKeyRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.site_id, r.parameter_id))
        .collect())
}

/// The reprocess a pin owes: every slot the selection touches re-derives under the pinned
/// attribution, which is what makes the pinned rows' corrections follow the pin rather than the
/// window. Both surfaces that record a pin call this, so a pin cannot be recorded on one of them
/// and left inert.
pub async fn enqueue_attribution_pin(
    db: &sea_orm::DatabaseConnection,
    kind: Kind,
    sensor_id: Option<Uuid>,
    set_id: Uuid,
    rows: Condition,
) -> AppResult<Vec<Uuid>> {
    if !matches!(kind, Kind::InstrumentPin | Kind::CalibrationPin) {
        return Ok(Vec::new());
    }
    let mut jobs = Vec::new();
    for (site_id, parameter_id) in slots_of(db, rows).await? {
        if let Some(job) = crate::routes::private::reprocessing_jobs::service::enqueue(
            db,
            "attribution_pin",
            sensor_id,
            Some(set_id),
            &serde_json::json!({ "site_id": site_id, "parameter_id": parameter_id }),
            None,
        )
        .await?
        {
            jobs.push(job);
        }
    }
    Ok(jobs)
}

/// The reprocess an inverted pin owes, for a whole set. Clearing a pin changes what the window
/// resolves, so the slots have to be re-derived exactly as they were when it was recorded; a set
/// that recorded no pin enqueues nothing.
pub async fn enqueue_pin_reprocess_for_set(
    db: &sea_orm::DatabaseConnection,
    set_id: Uuid,
) -> AppResult<Vec<Uuid>> {
    let Some(row) = decision_set::Entity::find_by_id(set_id).one(db).await? else {
        return Ok(Vec::new());
    };
    let Some(kind) = Kind::parse(&row.kind) else {
        return Ok(Vec::new());
    };
    let selection: Selection = serde_json::from_value(row.selection)
        .map_err(|e| AppError::Internal(format!("stored selection unreadable: {e}")))?;
    enqueue_attribution_pin(db, kind, None, set_id, selection.condition()?).await
}

/// The same, for one decision rolled back on its own: the slot it names re-derives.
pub async fn enqueue_pin_reprocess_for_decision(
    db: &sea_orm::DatabaseConnection,
    decision_id: Uuid,
) -> AppResult<Vec<Uuid>> {
    let d = load(db, decision_id).await?;
    if !matches!(d.kind, Kind::InstrumentPin | Kind::CalibrationPin) {
        return Ok(Vec::new());
    }
    let selection = Selection {
        keys: vec![SelectionKey {
            stream_id: d.stream_id,
            time: d.time,
            replicate_index: d.replicate_index,
            value: None,
        }],
        ..Default::default()
    };
    enqueue_attribution_pin(
        db,
        d.kind,
        None,
        d.set_id.unwrap_or(decision_id),
        selection.condition()?,
    )
    .await
}

/// The ownership fold over a slot's ownership decisions (`chain`, `detach`, `return`), newest
/// first, and the instant of the latest input decision at the visit. A `detach` makes the slot
/// manual until a `return`, a `chain` supersession, or an input decision newer than it, which
/// re-engages the tool (Q47's clarification).
#[must_use]
pub fn slot_owner(
    ownership_newest_first: &[(Kind, chrono::DateTime<chrono::Utc>)],
    latest_input_decision: Option<chrono::DateTime<chrono::Utc>>,
) -> Owner {
    match ownership_newest_first.first() {
        Some((Kind::Detach, at)) => match latest_input_decision {
            Some(input) if input > *at => Owner::Tool,
            _ => Owner::Manual,
        },
        _ => Owner::Tool,
    }
}

/// The decisions that say who owns an output slot, and the ones that count as an input moving
/// under it. Together they are the fold [`slot_owner`] applies.
const OWNERSHIP_KINDS: [Kind; 3] = [Kind::Chain, Kind::Detach, Kind::Return];
const INPUT_KINDS: [Kind; 7] = [
    Kind::Flag,
    Kind::Unflag,
    Kind::Withdraw,
    Kind::Reassert,
    Kind::Reject,
    Kind::ValueCorrection,
    Kind::Rollback,
];

/// The output rows at one slot instant: `(stream_id, replicate indices)` per stream.
pub(super) async fn output_rows_at<C: ConnectionTrait>(
    conn: &C,
    site_id: Uuid,
    parameter_id: Uuid,
    at: chrono::DateTime<chrono::Utc>,
) -> AppResult<Vec<(Uuid, i16)>> {
    let rows = readings::Entity::find()
        .select_only()
        .column(readings::Column::StreamId)
        .column(readings::Column::ReplicateIndex)
        .filter(readings::Column::SiteId.eq(site_id))
        .filter(readings::Column::ParameterId.eq(parameter_id))
        .filter(readings::Column::Time.eq(at))
        .filter(readings::Column::MeasurementType.eq("spot"))
        .order_by_asc(readings::Column::StreamId)
        .order_by_asc(readings::Column::ReplicateIndex)
        .into_model::<ReplicateKeyRow>()
        .all(conn)
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.stream_id, row.replicate_index))
        .collect())
}

/// The owner of an output slot at a visit, read from the record: the latest live ownership
/// decision on the slot's rows against the latest decision on any other parameter's rows at the
/// same site and instant (an input edit).
pub async fn output_owner<C: ConnectionTrait>(
    conn: &C,
    site_id: Uuid,
    parameter_id: Uuid,
    at: chrono::DateTime<chrono::Utc>,
) -> AppResult<Owner> {
    let d = Alias::new("d");
    let r = Alias::new("r");
    let at_slot = |same_parameter: bool| {
        let parameter = Expr::col((r.clone(), readings::Column::ParameterId));
        Condition::all()
            .add(Expr::col((r.clone(), readings::Column::SiteId)).eq(site_id))
            .add(if same_parameter {
                parameter.eq(parameter_id)
            } else {
                parameter.ne(parameter_id)
            })
            .add(Expr::col((r.clone(), readings::Column::Time)).eq(at))
    };
    let decisions_on_the_slot = || {
        let mut query = Query::select();
        query.from_as(decision_model::Entity, d.clone()).join_as(
            JoinType::InnerJoin,
            readings::Entity,
            r.clone(),
            sea_orm::sea_query::Expr::from(
                Condition::all()
                    .add(
                        Expr::col((r.clone(), readings::Column::StreamId))
                            .equals((d.clone(), decision_model::Column::StreamId)),
                    )
                    .add(
                        Expr::col((r.clone(), readings::Column::Time))
                            .equals((d.clone(), decision_model::Column::Time)),
                    )
                    .add(
                        Condition::any()
                            .add(
                                Expr::col((d.clone(), decision_model::Column::ReplicateIndex))
                                    .is_null(),
                            )
                            .add(
                                Expr::col((d.clone(), decision_model::Column::ReplicateIndex))
                                    .equals((r.clone(), readings::Column::ReplicateIndex)),
                            ),
                    ),
            ),
        );
        query
    };
    let (sql, values) = decisions_on_the_slot()
        .column((d.clone(), decision_model::Column::Kind))
        .column((d.clone(), decision_model::Column::At))
        .cond_where(
            at_slot(true)
                .add(
                    Expr::col((d.clone(), decision_model::Column::Kind))
                        .is_in(OWNERSHIP_KINDS.map(Kind::as_str)),
                )
                .add(Expr::col((d.clone(), decision_model::Column::RolledBackBy)).is_null()),
        )
        .order_by((d.clone(), decision_model::Column::At), Order::Desc)
        .order_by((d.clone(), decision_model::Column::Id), Order::Desc)
        .to_owned()
        .build(PostgresQueryBuilder);
    let ownership: Vec<(Kind, chrono::DateTime<chrono::Utc>)> = OwnershipRow::find_by_statement(
        Statement::from_sql_and_values(sea_orm::DatabaseBackend::Postgres, sql, values),
    )
    .all(conn)
    .await?
    .into_iter()
    // A kind outside the vocabulary is a corrupt row, not a decode failure, and the query
    // already names the three this reads.
    .filter_map(|r| Some((Kind::parse(&r.kind)?, r.at.with_timezone(&chrono::Utc))))
    .collect();
    let (sql, values) =
        decisions_on_the_slot()
            .expr_as(
                Func::max(Expr::col((d.clone(), decision_model::Column::At))),
                Alias::new("at"),
            )
            .cond_where(at_slot(false).add(
                Expr::col((d, decision_model::Column::Kind)).is_in(INPUT_KINDS.map(Kind::as_str)),
            ))
            .to_owned()
            .build(PostgresQueryBuilder);
    let input = conn
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?
        .and_then(|r| {
            r.try_get::<Option<sea_orm::prelude::DateTimeWithTimeZone>>("", "at")
                .ok()
                .flatten()
        })
        .map(|t| t.with_timezone(&chrono::Utc));
    Ok(slot_owner(&ownership, input))
}

pub async fn load<C: ConnectionTrait>(conn: &C, id: Uuid) -> AppResult<DecisionRow> {
    let row = decision_model::Entity::find_by_id(id)
        .one(conn)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Decision {id} not found")))?;
    row_from(row)
}

/// Every decision on a key, newest first. A group key lists group decisions only; a replicate
/// key lists the replicate's own decisions and the group decisions that covered it.
pub async fn history<C: ConnectionTrait>(
    conn: &C,
    key: &DecisionKey,
) -> AppResult<Vec<DecisionRow>> {
    let mut find = decision_model::Entity::find()
        .filter(decision_model::Column::StreamId.eq(key.stream_id))
        .filter(decision_model::Column::Time.eq(key.time));
    if let Some(index) = key.replicate_index {
        find = find.filter(
            decision_model::Column::ReplicateIndex
                .is_null()
                .or(decision_model::Column::ReplicateIndex.eq(index)),
        );
    }
    let rows = find
        .order_by_desc(decision_model::Column::At)
        .order_by_desc(decision_model::Column::Id)
        .all(conn)
        .await?;
    rows.into_iter().map(row_from).collect()
}

/// A derived value's recorded arithmetic, run again over the values it consumed.
///
/// The newest `derived_computed` or `formula_transition` at the key is the computation that made
/// the value standing there; both carry the whole set in `new.consumed`, formula included, so
/// nothing here reads a formula, a version or an input from the store. The answer is the stored
/// number's own arithmetic, which is what a reader compares against the inputs as they stand now.
pub async fn replay_at<C: ConnectionTrait>(
    conn: &C,
    key: &DecisionKey,
) -> AppResult<crate::routes::private::readings::models::ReplayResponse> {
    use crate::routes::private::readings::models::{Kind, ReplayResponse};
    let computation = history(conn, key)
        .await?
        .into_iter()
        .find(|row| matches!(row.kind, Kind::DerivedComputed | Kind::FormulaTransition))
        .ok_or_else(|| {
            AppError::NotFound("No computation is recorded at that reading".to_string())
        })?;
    let consumed: Vec<crate::routes::private::readings::models::ConsumedInput> =
        serde_json::from_value(
            computation
                .new
                .get("consumed")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        )
        .map_err(|e| AppError::Conflict(format!("the captured set is unreadable: {e}")))?;
    let set = crate::routes::private::sensor_calibrations::service::captured_set(&consumed)
        .map_err(AppError::Conflict)?;
    let replayed = crate::routes::private::sensor_calibrations::service::replay_captured(&consumed)
        .map_err(AppError::Conflict)?;
    let stored = readings::Entity::find()
        .filter(readings::Column::StreamId.eq(key.stream_id))
        .filter(readings::Column::Time.eq(key.time))
        .filter(readings::Column::ReplicateIndex.eq(key.replicate_index.unwrap_or(0)))
        .one(conn)
        .await?;
    Ok(ReplayResponse {
        formula: set.formula.to_string(),
        derived_version_id: stored.as_ref().and_then(|r| r.derived_version_id),
        replayed,
        stored: stored.map(|r| r.raw_value),
        variables: serde_json::to_value(&set.variables).unwrap_or(serde_json::Value::Null),
        captured_at: computation.at,
    })
}

/// The default and the ceiling on how much history one read returns. A value with a thousand
/// entries is a value with a story to tell in pages, not in one response.
pub(super) const DEFAULT_LIMIT: u64 = 200;

pub(super) const MAX_LIMIT: u64 = 1000;

pub(super) fn dedup(ids: impl Iterator<Item = Uuid>) -> Vec<Uuid> {
    let mut out: Vec<Uuid> = ids.collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Every curation decision on the instant. A decision is a person's act, so it is never a failure.
pub(super) async fn decisions<C: ConnectionTrait>(
    conn: &C,
    streams: &[Uuid],
    time: DateTime<Utc>,
) -> AppResult<Vec<LedgerEntry>> {
    if streams.is_empty() {
        return Ok(Vec::new());
    }
    let rows = decision_model::Entity::find()
        .filter(decision_model::Column::StreamId.is_in(streams.to_vec()))
        .filter(decision_model::Column::Time.eq(time))
        .order_by_desc(decision_model::Column::At)
        .all(conn)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| LedgerEntry {
            at: r.at.into(),
            source: "decision".to_string(),
            severity: Severity::Info.as_str().to_string(),
            actor: Some(r.actor),
            what: match r.reason {
                Some(why) if !why.trim().is_empty() => format!("{}: {why}", r.kind),
                _ => r.kind,
            },
            old: Some(r.old),
            new: Some(r.new),
            id: r.id,
        })
        .collect())
}

/// Every windowed ingest pass whose claimed window covers the instant, not only the latest: the
/// question the ledger answers is what carried this value, over all the passes that did.
pub(super) async fn ingest_passes<C: ConnectionTrait>(
    conn: &C,
    streams: &[Uuid],
    time: DateTime<Utc>,
) -> AppResult<Vec<LedgerEntry>> {
    if streams.is_empty() {
        return Ok(Vec::new());
    }
    let query = Query::select()
        .columns([
            receipts::Column::Id,
            receipts::Column::At,
            receipts::Column::Submitted,
            receipts::Column::NewRows,
            receipts::Column::Changed,
            receipts::Column::Unchanged,
            receipts::Column::Withdrawn,
            receipts::Column::RejectedTotal,
            receipts::Column::Braked,
        ])
        .from(receipts::Entity)
        .and_where(Expr::col(receipts::Column::StreamId).is_in(streams.iter().copied()))
        .and_where(Expr::col(receipts::Column::WindowFrom).lte(time))
        .and_where(Expr::col(receipts::Column::WindowTo).gte(time))
        .order_by(receipts::Column::At, Order::Desc)
        .take();
    let (sql, values) = query.build(PostgresQueryBuilder);
    let rows = ReceiptRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| LedgerEntry {
            at: r.at,
            source: "ingest".to_string(),
            severity: Severity::ingest_receipt(i64::from(r.rejected_total), r.braked)
                .as_str()
                .to_string(),
            actor: None,
            what: if r.braked {
                "windowed ingest, braked".to_string()
            } else {
                format!("windowed ingest: {} new, {} changed", r.new_rows, r.changed)
            },
            old: None,
            new: Some(serde_json::json!({
                "submitted": r.submitted,
                "new": r.new_rows,
                "changed": r.changed,
                "unchanged": r.unchanged,
                "withdrawn": r.withdrawn,
                "rejected": r.rejected_total,
                "braked": r.braked,
            })),
            id: r.id,
        })
        .collect())
}

/// Holds raised on one of these streams.
fn on_streams(streams: &[Uuid]) -> SimpleExpr {
    Expr::col(hold_model::Column::StreamId).is_in(streams.iter().copied())
}

/// The review queue, by both of its key shapes: a statistics hold is keyed by stream, an event
/// finding by the slot it was found at.
pub(super) async fn holds<C: ConnectionTrait>(
    conn: &C,
    streams: &[Uuid],
    site_id: Option<Uuid>,
    parameter_id: Option<Uuid>,
) -> AppResult<Vec<LedgerEntry>> {
    let (Some(site_id), Some(parameter_id)) = (site_id, parameter_id) else {
        if streams.is_empty() {
            return Ok(Vec::new());
        }
        return hold_rows(conn, Condition::all().add(on_streams(streams))).await;
    };
    hold_rows(
        conn,
        Condition::any().add(on_streams(streams)).add(
            Condition::all()
                .add(Expr::col(hold_model::Column::SiteId).eq(site_id))
                .add(Expr::col(hold_model::Column::ParameterId).eq(parameter_id)),
        ),
    )
    .await
}

pub(super) async fn hold_rows<C: ConnectionTrait>(
    conn: &C,
    reached: Condition,
) -> AppResult<Vec<LedgerEntry>> {
    let (sql, values) = Query::select()
        .columns([
            hold_model::Column::Id,
            hold_model::Column::Kind,
            hold_model::Column::Status,
            hold_model::Column::CreatedAt,
            hold_model::Column::Tool,
        ])
        .from(hold_model::Entity)
        .cond_where(reached)
        .order_by(hold_model::Column::CreatedAt, Order::Desc)
        .take()
        .build(PostgresQueryBuilder);
    let rows = LedgerHoldRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| LedgerEntry {
            at: r.created_at,
            source: "hold".to_string(),
            severity: Severity::audit_hold(&r.status).as_str().to_string(),
            actor: None,
            what: format!("{} ({})", r.kind, r.status),
            old: None,
            new: r.tool.map(Into::into),
            id: r.id,
        })
        .collect())
}

/// The runs that produced the value, and the runs made at the visit it belongs to: a chain step
/// that wrote a neighbouring cell is part of this value's story when it read this one.
pub(super) async fn tool_runs<C: ConnectionTrait>(
    conn: &C,
    runs: &[Uuid],
    events: &[Uuid],
) -> AppResult<Vec<LedgerEntry>> {
    if runs.is_empty() && events.is_empty() {
        return Ok(Vec::new());
    }
    let event_keys: Vec<String> = events.iter().map(ToString::to_string).collect();
    // The event arm matches on a field inside `context`, which no typed column expresses, so the
    // statement stays; decoding into the entity's own model is what keeps the columns honest.
    let rows = tool_run::Entity::find()
        .filter(
            Condition::any()
                .add(tool_run::Column::Id.is_in(runs.iter().copied()))
                .add(
                    Expr::col(tool_run::Column::Context)
                        .binary(PgBinOper::CastJsonField, Expr::val("collection_event_id"))
                        .is_in(event_keys),
                ),
        )
        .order_by_desc(tool_run::Column::CreatedAt)
        .all(conn)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| LedgerEntry {
            at: r.created_at,
            source: "tool_run".to_string(),
            severity: Severity::Info.as_str().to_string(),
            actor: Some(r.created_by),
            what: format!("{} ({})", r.tool_name, r.source),
            old: None,
            new: Some(r.tool_version),
            id: r.id,
        })
        .collect())
}

/// The tracked jobs that touched this slot or the visit it belongs to. A job names its subject in
/// `params`, which is what makes "what rewrote this row" reachable from the row.
pub(super) async fn job_entries<C: ConnectionTrait>(
    conn: &C,
    site_id: Option<Uuid>,
    parameter_id: Option<Uuid>,
    events: &[Uuid],
) -> AppResult<Vec<LedgerEntry>> {
    let (Some(site_id), Some(parameter_id)) = (site_id, parameter_id) else {
        return Ok(Vec::new());
    };
    let event_keys: Vec<String> = events.iter().map(ToString::to_string).collect();
    let param = |key: &str| {
        Expr::col(jobs_model::Column::Params)
            .binary(PgBinOper::CastJsonField, Expr::val(key.to_string()))
    };
    let (sql, values) = Query::select()
        .columns([
            jobs_model::Column::Id,
            jobs_model::Column::TriggerType,
            jobs_model::Column::Status,
            jobs_model::Column::ErrorMessage,
            jobs_model::Column::CreatedAt,
            jobs_model::Column::CompletedAt,
            jobs_model::Column::ReadingsUpdated,
        ])
        .from(jobs_model::Entity)
        .cond_where(
            Condition::any()
                .add(
                    Condition::all()
                        .add(param("site_id").eq(site_id.to_string()))
                        .add(param("parameter_id").eq(parameter_id.to_string())),
                )
                .add(param("collection_event_id").is_in(event_keys)),
        )
        .order_by(jobs_model::Column::CreatedAt, Order::Desc)
        .take()
        .build(PostgresQueryBuilder);
    let rows = JobRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| LedgerEntry {
            at: r.completed_at.unwrap_or(r.created_at),
            source: "job".to_string(),
            severity: Severity::job(&r.status).as_str().to_string(),
            actor: None,
            what: match &r.error_message {
                Some(message) if r.status == "failed" => {
                    format!("{} failed: {message}", r.trigger_type)
                }
                _ => format!("{} {}", r.trigger_type, r.status),
            },
            old: None,
            new: Some(serde_json::json!({
                "status": r.status,
                "readings_updated": r.readings_updated,
                "error_message": r.error_message,
            })),
            id: r.id,
        })
        .collect())
}

/// What those jobs said while they ran, minus the routine. A cascade step that was skipped is
/// recorded here and nowhere else, which is the half of the story the job row cannot tell.
pub(super) async fn job_logs<C: ConnectionTrait>(
    conn: &C,
    jobs: &[Uuid],
) -> AppResult<Vec<LedgerEntry>> {
    if jobs.is_empty() {
        return Ok(Vec::new());
    }
    use crate::routes::private::reprocessing_jobs::models::job_log;
    let rows = job_log::Entity::find()
        .filter(job_log::Column::JobId.is_in(jobs.to_vec()))
        .filter(job_log::Column::Level.ne("info"))
        .order_by_desc(job_log::Column::Ts)
        .all(conn)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| LedgerEntry {
            at: r.ts.into(),
            source: "job_log".to_string(),
            severity: Severity::job_log(&r.level).as_str().to_string(),
            actor: None,
            what: r.message,
            old: None,
            new: None,
            // A timeline entry is keyed by (job_id, seq) and has no id of its own; the job is
            // where a reader opens it.
            id: r.job_id,
        })
        .collect())
}

/// The edits to the slot and to the catalog parameter behind it: not what the value is, but what
/// changed about how it is served.
pub(super) async fn slot_changes<C: ConnectionTrait>(
    conn: &C,
    site_id: Option<Uuid>,
    parameter_id: Option<Uuid>,
) -> AppResult<Vec<LedgerEntry>> {
    let (Some(site_id), Some(parameter_id)) = (site_id, parameter_id) else {
        return Ok(Vec::new());
    };
    let slot_subjects = Query::select()
        .expr(Expr::cust_with_expr(
            "'site_parameter:' || $1",
            Expr::col(site_parameters::Column::Id).cast_as(Alias::new("text")),
        ))
        .from(site_parameters::Entity)
        .and_where(Expr::col(site_parameters::Column::SiteId).eq(site_id))
        .and_where(Expr::col(site_parameters::Column::ParameterId).eq(parameter_id))
        .take();
    let (sql, values) = Query::select()
        .columns([
            change_audit::Column::Id,
            change_audit::Column::Change,
            change_audit::Column::OldValue,
            change_audit::Column::NewValue,
            change_audit::Column::ChangedBy,
            change_audit::Column::ChangedAt,
        ])
        .from(change_audit::Entity)
        .cond_where(
            Condition::any()
                .add(
                    Expr::col(change_audit::Column::Subject)
                        .eq(format!("parameter:{parameter_id}")),
                )
                .add(Expr::col(change_audit::Column::Subject).in_subquery(slot_subjects)),
        )
        .order_by(change_audit::Column::ChangedAt, Order::Desc)
        .take()
        .build(PostgresQueryBuilder);
    let rows = ChangeRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| LedgerEntry {
            at: r.changed_at,
            source: "change".to_string(),
            severity: Severity::Info.as_str().to_string(),
            actor: r.changed_by,
            what: r.change,
            old: r.old_value,
            new: r.new_value,
            id: r.id,
        })
        .collect())
}

/// The alarm episodes this value fell inside, which is the half nothing else answers: what
/// happened because of it.
pub(super) async fn alarms<C: ConnectionTrait>(
    conn: &C,
    site_id: Option<Uuid>,
    parameter_id: Option<Uuid>,
    time: DateTime<Utc>,
) -> AppResult<Vec<LedgerEntry>> {
    let (Some(site_id), Some(parameter_id)) = (site_id, parameter_id) else {
        return Ok(Vec::new());
    };
    let (sql, values) = Query::select()
        .columns([
            alarm_event::Column::Id,
            alarm_event::Column::Severity,
            alarm_event::Column::MaxSeverity,
            alarm_event::Column::StartedAt,
            alarm_event::Column::ResolvedAt,
            alarm_event::Column::AcknowledgedBy,
            alarm_event::Column::MeasurementType,
        ])
        .from(alarm_event::Entity)
        .and_where(Expr::col(alarm_event::Column::SiteId).eq(site_id))
        .and_where(Expr::col(alarm_event::Column::ParameterId).eq(parameter_id))
        .and_where(Expr::col(alarm_event::Column::StartedAt).lte(time))
        // An open episode has no resolution, so its last sighting stands in for one.
        .and_where(
            Expr::expr(Func::coalesce([
                Expr::col(alarm_event::Column::ResolvedAt),
                Expr::col(alarm_event::Column::LastSeenAt),
            ]))
            .gte(time),
        )
        .take()
        .build(PostgresQueryBuilder);
    let rows = AlarmRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| LedgerEntry {
            at: r.started_at,
            source: "alarm".to_string(),
            severity: Severity::alarm(r.max_severity, r.resolved_at.is_some())
                .as_str()
                .to_string(),
            actor: r.acknowledged_by,
            what: match r.resolved_at {
                Some(_) => format!("{} alarm, resolved", r.measurement_type),
                None => format!("{} alarm, open", r.measurement_type),
            },
            old: None,
            new: Some(serde_json::json!({
                "severity": r.severity,
                "max_severity": r.max_severity,
                "resolved_at": r.resolved_at,
            })),
            id: r.id,
        })
        .collect())
}

/// The options a row's provenance permits, in the order the surface offers them.
///
/// The rule is Q8's: the provenance already stored decides which path a cell takes. What decides
/// is ownership rather than the presence of a run, so a tool-run value offers no in-place
/// correction while its calculation owns the slot, and offers one once the slot is detached,
/// which is the single override Q117 kept.
#[must_use]
pub fn edit_options(p: &RowProvenance) -> Vec<EditOption> {
    let mut options = Vec::new();
    if p.has_tool_run && !p.slot_detached {
        options.push(EditOption::ReopenRun);
        options.push(EditOption::Detach);
    } else {
        if p.slot_detached {
            options.push(EditOption::Return);
        }
        options.push(EditOption::ValueCorrection);
        if p.has_standard_curve {
            options.push(EditOption::Curve);
        }
        // Attribution is never stamped on a row (Q117): a wrong instrument is a wrong deployment
        // and a wrong corrected value is a wrong calibration window, and the reprocess carries the
        // correction through. So the surface points at the record that decides, and offers nothing
        // where there is no such record to correct.
        if p.has_calibration {
            options.push(EditOption::EditCalibration);
        }
        // With no deployment the fix is to create one, so the surface points at the deployment
        // either way rather than falling silent.
        options.push(EditOption::EditDeployment);
    }
    options.push(if p.is_flagged {
        EditOption::Unflag
    } else {
        EditOption::Flag
    });
    options.push(if p.withdrawn {
        EditOption::Reassert
    } else {
        EditOption::Withdraw
    });
    if p.unverified {
        options.push(EditOption::Verify);
        options.push(EditOption::Reject);
    }
    options
}

/// The id a preview and its commit share.
///
/// It is a digest of the selection and the decision, not a stored row: a commit naming a preview
/// of a different selection or a different decision cannot produce the same id, which is exactly
/// what holding the commit to the preview means. Nothing expires, because nothing is stored.
pub(super) fn preview_id(selection: &Selection, decision: &EditDecision) -> AppResult<Uuid> {
    let canonical = serde_json::json!({
        "selection": serde_json::to_value(selection)
            .map_err(|e| AppError::Internal(e.to_string()))?,
        "decision": serde_json::to_value(decision)
            .map_err(|e| AppError::Internal(e.to_string()))?,
    });
    Ok(Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        canonical.to_string().as_bytes(),
    ))
}

/// The columns an [`InspectedRow`] is read from: the key, the value, and whether each piece of
/// attribution and curation is present.
pub(super) fn stored_rows() -> sea_orm::sea_query::SelectStatement {
    let r = Alias::new("r");
    let ds = Alias::new("ds");
    let flag = |expr: Expr, name: &'static str| (expr, Alias::new(name));
    let mut query = Query::select();
    query
        .column((r.clone(), readings::Column::StreamId))
        .column((r.clone(), readings::Column::Time))
        .column((r.clone(), readings::Column::ReplicateIndex))
        .column((r.clone(), readings::Column::RawValue))
        .column((r.clone(), readings::Column::SiteId))
        .column((r.clone(), readings::Column::ParameterId))
        .expr_as(
            Expr::cust("r.provenance ->> 'run_id'"),
            Alias::new("run_id"),
        )
        .from_as(readings::Entity, r.clone())
        .join_as(
            JoinType::InnerJoin,
            data_streams::Entity,
            ds.clone(),
            Expr::col((ds.clone(), data_streams::Column::Id))
                .equals((r.clone(), readings::Column::StreamId)),
        )
        .column((ds, data_streams::Column::SourceSystem));
    for (expr, name) in [
        flag(
            Expr::col((r.clone(), readings::Column::StandardCurveId)).is_not_null(),
            "has_curve",
        ),
        flag(
            Expr::col((r.clone(), readings::Column::CalibrationId)).is_not_null(),
            "has_calibration",
        ),
        flag(
            Expr::col((r.clone(), readings::Column::DeploymentId)).is_not_null(),
            "has_deployment",
        ),
        flag(Expr::cust("COALESCE(r.is_flagged, false)"), "is_flagged"),
        flag(
            Expr::col((r.clone(), readings::Column::WithdrawnAt)).is_not_null(),
            "withdrawn",
        ),
        flag(Expr::cust("COALESCE(r.unverified, false)"), "unverified"),
    ] {
        query.expr_as(expr, name);
    }
    query.to_owned()
}

pub(super) fn classification(source_system: &str) -> String {
    match source_system {
        "grab_sample" => "manual",
        "api" => "api",
        "csv_import" => "csv",
        _ => "sync",
    }
    .to_string()
}

pub(super) async fn inspect_rows<C: ConnectionTrait>(
    conn: &C,
    selection: &Selection,
    within: Option<&[Uuid]>,
) -> AppResult<Vec<InspectedRow>> {
    let mut rows_where = selection.condition()?;
    if let Some(sites) = within {
        rows_where = rows_where.add(
            Expr::col((Alias::new("r"), readings::Column::SiteId)).is_in(sites.iter().copied()),
        );
    }
    let (sql, values) = stored_rows()
        .cond_where(rows_where)
        .order_by((Alias::new("r"), readings::Column::Time), Order::Asc)
        .order_by((Alias::new("r"), readings::Column::StreamId), Order::Asc)
        .order_by(
            (Alias::new("r"), readings::Column::ReplicateIndex),
            Order::Asc,
        )
        .to_owned()
        .build(PostgresQueryBuilder);
    let rows = StoredRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .all(conn)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    let mut owners: HashMap<(Uuid, Uuid, chrono::DateTime<chrono::Utc>), Owner> = HashMap::new();
    for row in rows {
        // The run id is stored inside the provenance blob, so it arrives as text and is a run
        // reference only if it parses as one.
        let tool_run_id = row.run_id.as_deref().and_then(|s| s.parse::<Uuid>().ok());
        let time = row.time.with_timezone(&chrono::Utc);
        // Ownership is a fold over the slot's decisions, so it is read once per slot instant
        // rather than once per replicate.
        let mut owner = Owner::Tool;
        if let (Some(site), Some(parameter)) = (row.site_id, row.parameter_id) {
            owner = match owners.entry((site, parameter, time)) {
                Entry::Occupied(e) => *e.get(),
                Entry::Vacant(e) => *e.insert(output_owner(conn, site, parameter, time).await?),
            };
        }
        let provenance = RowProvenance {
            has_tool_run: tool_run_id.is_some(),
            slot_detached: owner == Owner::Manual,
            classification: classification(&row.source_system),
            has_standard_curve: row.has_curve,
            has_calibration: row.has_calibration,
            has_deployment: row.has_deployment,
            is_flagged: row.is_flagged,
            withdrawn: row.withdrawn,
            unverified: row.unverified,
        };
        out.push(InspectedRow {
            stream_id: row.stream_id,
            time,
            site_id: row.site_id,
            parameter_id: row.parameter_id,
            replicate_index: row.replicate_index,
            raw_value: row.raw_value,
            options: edit_options(&provenance),
            provenance,
            tool_run_id,
        });
    }
    Ok(out)
}

pub(super) const EDIT_STATE_SQL: &str = "jsonb_build_object(
    'raw_value', r.raw_value, 'calibrated_value', r.calibrated_value,
    'is_flagged', COALESCE(r.is_flagged, false), 'flag_reason', r.flag_reason,
    'withdrawn_at', r.withdrawn_at, 'unverified', COALESCE(r.unverified, false),
    'standard_curve_id', r.standard_curve_id, 'calibration_id', r.calibration_id,
    'sensor_id', r.sensor_id)";

pub(super) async fn row_states<C: ConnectionTrait>(
    conn: &C,
    rows: Condition,
) -> AppResult<Vec<(Uuid, chrono::DateTime<chrono::Utc>, i16, serde_json::Value)>> {
    let r = Alias::new("r");
    let (sql, values) = readings_joined(r.clone())
        .column((r.clone(), readings::Column::StreamId))
        .column((r.clone(), readings::Column::Time))
        .column((r.clone(), readings::Column::ReplicateIndex))
        .expr_as(Expr::cust(EDIT_STATE_SQL), Alias::new("state"))
        .cond_where(rows)
        .order_by((r.clone(), readings::Column::Time), Order::Asc)
        .order_by((r.clone(), readings::Column::StreamId), Order::Asc)
        .order_by((r, readings::Column::ReplicateIndex), Order::Asc)
        .to_owned()
        .build(PostgresQueryBuilder);
    let rows = StateRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.stream_id,
                row.time.with_timezone(&chrono::Utc),
                row.replicate_index,
                row.state,
            )
        })
        .collect())
}

pub(super) async fn sample_states<C: ConnectionTrait>(
    conn: &C,
    rows: Condition,
) -> AppResult<Vec<(Uuid, serde_json::Value)>> {
    let r = Alias::new("r");
    let sample = Alias::new("s");
    let (sql, values) = readings_joined(r.clone())
        .distinct()
        .column((sample.clone(), samples::Column::Id))
        .expr_as(
            Expr::cust(
                "jsonb_build_object('mean', s.mean, 'stdev', s.stdev, 'n', s.n, \
                 'min_value', s.min_value, 'max_value', s.max_value)",
            ),
            Alias::new("stats"),
        )
        .join_as(
            JoinType::InnerJoin,
            samples::Entity,
            sample.clone(),
            Expr::col((sample.clone(), samples::Column::Id))
                .equals((r, readings::Column::SampleId)),
        )
        .cond_where(rows)
        .order_by((sample, samples::Column::Id), Order::Asc)
        .to_owned()
        .build(PostgresQueryBuilder);
    let rows = SampleStatsRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .all(conn)
    .await?;
    Ok(rows.into_iter().map(|row| (row.id, row.stats)).collect())
}

/// The parameters a selection's rows belong to, for the calculation closure.
pub(super) async fn touched_parameters<C: ConnectionTrait>(
    conn: &C,
    rows: Condition,
) -> AppResult<Vec<Uuid>> {
    let r = Alias::new("r");
    let (sql, values) = readings_joined(r.clone())
        .distinct()
        .column((r.clone(), readings::Column::ParameterId))
        .cond_where(rows.add(Expr::col((r, readings::Column::ParameterId)).is_not_null()))
        .to_owned()
        .build(PostgresQueryBuilder);
    let rows = ParameterRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .all(conn)
    .await?;
    Ok(rows.into_iter().map(|row| row.parameter_id).collect())
}

/// The live decisions of one kind standing on the rows a selection names, newest first. The
/// limit is the number of rows the write reported, so an edit reads back exactly what it wrote.
pub(super) async fn decisions_recorded<C: ConnectionTrait>(
    conn: &C,
    selection: &Selection,
    kind: Kind,
    rows_decided: u64,
) -> AppResult<Vec<Uuid>> {
    let r = Alias::new("r");
    let d = Alias::new("d");
    let names_the_row = readings_joined(r.clone())
        .expr(Expr::val(1))
        .cond_where(
            selection
                .condition()?
                .add(
                    Expr::col((r.clone(), readings::Column::StreamId))
                        .equals((d.clone(), decision_model::Column::StreamId)),
                )
                .add(
                    Expr::col((r.clone(), readings::Column::Time))
                        .equals((d.clone(), decision_model::Column::Time)),
                )
                .add(
                    Condition::any()
                        .add(
                            Expr::col((d.clone(), decision_model::Column::ReplicateIndex))
                                .is_null(),
                        )
                        .add(
                            Expr::col((d.clone(), decision_model::Column::ReplicateIndex))
                                .equals((r, readings::Column::ReplicateIndex)),
                        ),
                ),
        )
        .to_owned();
    let (sql, values) = Query::select()
        .expr_as(
            Expr::col((d.clone(), decision_model::Column::Id)),
            Alias::new("id"),
        )
        .from_as(decision_model::Entity, d.clone())
        .cond_where(
            Condition::all()
                .add(Expr::col((d.clone(), decision_model::Column::RolledBackBy)).is_null())
                .add(Expr::col((d.clone(), decision_model::Column::Kind)).eq(kind.as_str()))
                .add(Expr::exists(names_the_row)),
        )
        .order_by((d.clone(), decision_model::Column::At), Order::Desc)
        .order_by((d, decision_model::Column::Id), Order::Desc)
        .limit(Ord::max(rows_decided, 1))
        .to_owned()
        .build(PostgresQueryBuilder);
    let rows = DecisionIdRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .all(conn)
    .await?;
    Ok(rows.into_iter().map(|row| row.id).collect())
}

/// Apply the decision on `conn` as one decision set, which the caller may then roll back. The
/// set is what makes a many-row edit, a visit retracted whole, one act to undo.
pub(super) async fn apply<C: ConnectionTrait>(
    conn: &C,
    selection: &Selection,
    decision: &EditDecision,
    actor: &str,
    origin: Origin,
) -> AppResult<(Uuid, Recorded)> {
    let kind = decision.parsed()?;
    let (new, _) = decision.assertion_over(kind, selection)?;
    record_set(
        conn,
        kind,
        selection,
        new,
        actor,
        decision.reason.as_deref(),
        origin,
    )
    .await
}

/// Refuse an edit the selected rows' provenance does not permit, naming the row that refused it.
pub(super) async fn refuse_unrouted<C: ConnectionTrait>(
    conn: &C,
    selection: &Selection,
    option: EditOption,
) -> AppResult<()> {
    for row in inspect_rows(conn, selection, None).await? {
        if !row.options.contains(&option) {
            return Err(AppError::BadRequest(format!(
                "the reading at {} replicate {} is not corrected here: {}",
                row.time,
                row.replicate_index,
                if row.provenance.has_tool_run && !row.provenance.slot_detached {
                    "a tool run produced it, so it is reopened in its tool and saved again"
                } else {
                    "its provenance does not offer that edit"
                }
            )));
        }
    }
    Ok(())
}

/// The caller's standing against the option's capability.
pub(super) fn authorise(auth: &AuthContext, option: EditOption) -> AppResult<()> {
    if auth.allows(option.capability()) {
        return Ok(());
    }
    Err(AppError::Forbidden(format!(
        "that edit requires {}",
        option.capability()
    )))
}

/// The sites a restricted caller must hold to edit readings at these sites. A reading paired to no
/// site belongs to no project, so a restricted caller is refused it rather than let through.
pub(super) fn sites_to_confine(
    scope: &AccessScope,
    sites: &[Option<Uuid>],
) -> AppResult<Vec<Uuid>> {
    if scope.is_restricted() && sites.iter().any(Option::is_none) {
        return Err(AppError::Forbidden(
            "A reading paired to no site is outside your project access".to_string(),
        ));
    }
    Ok(sites.iter().flatten().copied().collect())
}

/// Refuse an edit reaching a site outside the caller's projects.
pub(super) async fn require_edit_in_scope(
    db: &DatabaseConnection,
    scope: &AccessScope,
    sites: &[Option<Uuid>],
) -> AppResult<()> {
    enforce_project_scope_for_sites(db, scope, &sites_to_confine(scope, sites)?).await
}

/// The sites of the readings a selection names, NULL for an unpaired one.
pub(super) async fn selected_sites<C: ConnectionTrait>(
    conn: &C,
    rows: Condition,
) -> AppResult<Vec<Option<Uuid>>> {
    let r = Alias::new("r");
    let (sql, values) = readings_joined(r.clone())
        .distinct()
        .column((r, readings::Column::SiteId))
        .cond_where(rows)
        .to_owned()
        .build(PostgresQueryBuilder);
    sites_of(conn, sql, values).await
}

/// A curve edit is held to the admission every other curve writer applies: the curve was fitted on
/// each selected row's own instrument, and the row is a spot measurement.
pub(super) async fn admit_edit_curve(
    db: &DatabaseConnection,
    rows: Condition,
    kind: Kind,
    curve_id: Option<Uuid>,
) -> AppResult<()> {
    #[derive(FromQueryResult)]
    struct Claimant {
        sensor_id: Option<Uuid>,
        measurement_type: Option<String>,
    }
    let (Kind::Curve, Some(curve_id)) = (kind, curve_id) else {
        return Ok(());
    };
    let r = Alias::new("r");
    let (sql, values) = readings_joined(r.clone())
        .distinct()
        .column((r.clone(), readings::Column::SensorId))
        .column((r, readings::Column::MeasurementType))
        .cond_where(rows)
        .to_owned()
        .build(PostgresQueryBuilder);
    let claimants = Claimant::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .all(db)
    .await?;
    let claims: Vec<CurveClaim<'_>> = claimants
        .iter()
        .map(|c| CurveClaim {
            standard_curve_id: curve_id,
            sensor_id: c.sensor_id,
            measurement_type: c
                .measurement_type
                .as_deref()
                .unwrap_or(river_data_core::models::MeasurementType::Continuous.as_str()),
        })
        .collect();
    admit_standard_curves(db, &claims).await?;
    Ok(())
}

/// The sites of the readings the decisions whose `column` is `id` stand on (one decision by its
/// id, or a set by its `set_id`), NULL for an unpaired one.
pub(super) async fn decided_sites<C: ConnectionTrait>(
    conn: &C,
    column: decision_model::Column,
    id: Uuid,
) -> AppResult<Vec<Option<Uuid>>> {
    let r = Alias::new("r");
    let d = Alias::new("d");
    let (sql, values) = Query::select()
        .distinct()
        .column((r.clone(), readings::Column::SiteId))
        .from_as(decision_model::Entity, d.clone())
        .join_as(
            JoinType::InnerJoin,
            readings::Entity,
            r.clone(),
            Condition::all()
                .add(
                    Expr::col((r.clone(), readings::Column::StreamId))
                        .equals((d.clone(), decision_model::Column::StreamId)),
                )
                .add(
                    Expr::col((r.clone(), readings::Column::Time))
                        .equals((d.clone(), decision_model::Column::Time)),
                )
                .add(
                    Condition::any()
                        .add(
                            Expr::col((d.clone(), decision_model::Column::ReplicateIndex))
                                .is_null(),
                        )
                        .add(
                            Expr::col((d, decision_model::Column::ReplicateIndex))
                                .equals((r, readings::Column::ReplicateIndex)),
                        ),
                ),
        )
        .and_where(Expr::col((Alias::new("d"), column)).eq(id))
        .to_owned()
        .build(PostgresQueryBuilder);
    sites_of(conn, sql, values).await
}

async fn sites_of<C: ConnectionTrait>(
    conn: &C,
    sql: String,
    values: sea_orm::sea_query::Values,
) -> AppResult<Vec<Option<Uuid>>> {
    #[derive(FromQueryResult)]
    struct SiteRow {
        site_id: Option<Uuid>,
    }
    Ok(SiteRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .all(conn)
    .await?
    .into_iter()
    .map(|row| row.site_id)
    .collect())
}

/// What every edit owes after its transaction commits, forward or inverted: the rollups over the
/// span it moved, and the calculations at the visits it touched.
pub(crate) async fn propagate(state: &AppState, recorded: &Recorded, actor: &str) -> AppResult<()> {
    if let Some((lo, hi)) = recorded.span
        && let Err(e) = crate::common::aggregates::refresh(
            &state.db,
            crate::common::aggregates::Window::Range(lo, hi),
        )
        .await
    {
        tracing::warn!(error = %e, "edit: aggregate refresh failed");
    }
    crate::routes::private::collection_events::flows::enqueue_for(
        &state.db,
        &recorded.touched_events,
        actor,
        crate::routes::private::collection_events::flows::Writer::Person,
    )
    .await?;
    Ok(())
}

/// Record what the source now asserts at a key whose stored value differs.
///
/// Returns whether this pass raised a *new* proposal, which is what stops the pass counting as
/// clean: a decision already taken on this exact number stands, and only a number nobody has seen
/// resets the row to `pending`.
/// The same predicate as SQL, over the conflicting row and the one being inserted. The upsert has
/// to decide this inside the statement: whether a decision survives is read and written in one
/// place, and a read-then-write would let a concurrent pass reopen a row a person just decided.
///
/// `IS DISTINCT FROM` is named as text because sea_query has no operator for it; both sides are
/// the entity's own columns, so no column name is spelled.
fn distinct_from(column: change_proposal::Column) -> SimpleExpr {
    Expr::cust_with_exprs(
        "$1 IS DISTINCT FROM $2",
        [
            Expr::col((change_proposal::Entity, column)),
            Expr::col((Alias::new("excluded"), column)),
        ],
    )
}

fn reopens_expr() -> SimpleExpr {
    distinct_from(change_proposal::Column::ProposedRawValue).or(distinct_from(
        change_proposal::Column::ProposedStandardCurveId,
    ))
}

/// Keep the conflicting row's value where the proposal is unchanged, reset it where it is not.
fn kept_unless_reopened(column: change_proposal::Column, reset_to: Value) -> SimpleExpr {
    Expr::case(reopens_expr(), reset_to)
        .finally(Expr::col((change_proposal::Entity, column)))
        .into()
}

pub async fn propose<C: ConnectionTrait>(
    conn: &C,
    stream_id: Uuid,
    key: (DateTime<Utc>, i16),
    proposed: (f64, Option<Uuid>),
    stored: (f64, Option<Uuid>),
) -> AppResult<bool> {
    let row = change_proposal::ActiveModel {
        id: ActiveValue::Set(Uuid::new_v4()),
        stream_id: ActiveValue::Set(stream_id),
        time: ActiveValue::Set(key.0),
        replicate_index: ActiveValue::Set(key.1),
        proposed_raw_value: ActiveValue::Set(proposed.0),
        proposed_standard_curve_id: ActiveValue::Set(proposed.1),
        stored_raw_value: ActiveValue::Set(stored.0),
        stored_standard_curve_id: ActiveValue::Set(stored.1),
        ..Default::default()
    };
    let written = change_proposal::Entity::insert(row)
        .on_conflict(proposal_conflict())
        .exec_with_returning(conn)
        .await?;
    Ok(written.status == "pending" && written.decided_at.is_none())
}

/// The conflict arm: the source's latest numbers always land, and the decision columns survive
/// unless the proposal itself changed.
pub(super) fn proposal_conflict() -> OnConflict {
    use change_proposal::Column as P;
    OnConflict::columns([P::StreamId, P::Time, P::ReplicateIndex])
        .update_columns([
            P::StoredRawValue,
            P::StoredStandardCurveId,
            P::ProposedRawValue,
            P::ProposedStandardCurveId,
        ])
        .value(P::LastSeenAt, Expr::current_timestamp())
        .value(P::Status, kept_unless_reopened(P::Status, "pending".into()))
        .value(
            P::DecidedBy,
            kept_unless_reopened(P::DecidedBy, Value::String(None)),
        )
        .value(
            P::DecidedAt,
            kept_unless_reopened(P::DecidedAt, Value::ChronoDateTimeUtc(None)),
        )
        .to_owned()
}

/// The stream's naming and the slot its pairing places it in, for one set of streams.
///
/// A proposal is keyed by stream; the review list shows the slot, so the labels are read once per
/// page rather than joined into every row of the queue.
#[derive(Debug, sea_orm::FromQueryResult)]
struct StreamSlot {
    id: Uuid,
    source_system: String,
    source_key: String,
    site_id: Option<Uuid>,
    site_name: Option<String>,
    parameter_id: Option<Uuid>,
    parameter_code: Option<String>,
}

async fn stream_slots<C: ConnectionTrait>(
    conn: &C,
    stream_ids: Vec<Uuid>,
) -> Result<HashMap<Uuid, StreamSlot>, sea_orm::DbErr> {
    use crate::routes::private::{data_streams, parameters, site_parameters, sites};
    if stream_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = data_streams::Entity::find()
        .select_only()
        .column(data_streams::Column::Id)
        .column(data_streams::Column::SourceSystem)
        .column(data_streams::Column::SourceKey)
        .column(site_parameters::Column::SiteId)
        .column_as(sites::Column::Name, "site_name")
        .column(site_parameters::Column::ParameterId)
        .column_as(parameters::Column::Code, "parameter_code")
        .join(
            JoinType::LeftJoin,
            data_streams::Relation::SiteParameter.def(),
        )
        .join(JoinType::LeftJoin, site_parameters::Relation::Site.def())
        .join(
            JoinType::LeftJoin,
            site_parameters::Relation::Parameter.def(),
        )
        .filter(data_streams::Column::Id.is_in(stream_ids))
        .into_model::<StreamSlot>()
        .all(conn)
        .await?;
    Ok(rows.into_iter().map(|r| (r.id, r)).collect())
}

/// A proposal row the labels are written onto. The detail and the list model are separate structs
/// carrying the same six fields, so the lookup is written once against this.
trait Labelled {
    fn stream_id(&self) -> Uuid;
    fn label(&mut self, slot: &StreamSlot);
}

macro_rules! labelled {
    ($t:ty) => {
        impl Labelled for $t {
            fn stream_id(&self) -> Uuid {
                self.stream_id
            }
            fn label(&mut self, slot: &StreamSlot) {
                self.source_system = Some(slot.source_system.clone());
                self.source_key = Some(slot.source_key.clone());
                self.site_id = slot.site_id;
                self.site_name = slot.site_name.clone();
                self.parameter_id = slot.parameter_id;
                self.parameter_code = slot.parameter_code.clone();
            }
        }
    };
}

labelled!(change_proposal::ReadingChangeProposal);
labelled!(change_proposal::ReadingChangeProposalList);

/// What the generated proposal list cannot state on its own: the stream's naming and its slot.
pub struct ChangeProposalOperations;

async fn label_proposals<C: ConnectionTrait, T: Labelled>(
    db: &C,
    rows: &mut [T],
) -> Result<(), crudcrate::ApiError> {
    let slots = stream_slots(db, rows.iter().map(Labelled::stream_id).collect()).await?;
    for row in rows {
        if let Some(slot) = slots.get(&row.stream_id()) {
            row.label(slot);
        }
    }
    Ok(())
}

impl crudcrate::CRUDOperations for ChangeProposalOperations {
    type Resource = change_proposal::ReadingChangeProposal;

    async fn after_get_one<C: ConnectionTrait + sea_orm::TransactionTrait>(
        &self,
        db: &C,
        entity: &mut change_proposal::ReadingChangeProposal,
    ) -> Result<(), crudcrate::ApiError> {
        label_proposals(db, std::slice::from_mut(entity)).await
    }

    async fn after_get_all<C: ConnectionTrait + sea_orm::TransactionTrait>(
        &self,
        db: &C,
        entities: &mut Vec<change_proposal::ReadingChangeProposalList>,
    ) -> Result<(), crudcrate::ApiError> {
        label_proposals(db, entities.as_mut_slice()).await
    }
}

/// The proposals a caller may decide, out of the ids they named. A restricted caller reaches only
/// what their projects hold, so an id outside them is simply not found and comes back refused: the
/// same confinement the listing applies, at the write.
pub(super) async fn load_undecided<C: ConnectionTrait>(
    conn: &C,
    ids: &[Uuid],
    projects: Option<&[Uuid]>,
) -> AppResult<Vec<Pending>> {
    let scoped_streams = projects.map(crate::common::middleware::scoped_stream_ids_query);
    let mut query = change_proposal::Entity::find()
        .select_only()
        .column(change_proposal::Column::Id)
        .column(change_proposal::Column::StreamId)
        .column(change_proposal::Column::Time)
        .column(change_proposal::Column::ReplicateIndex)
        .column(change_proposal::Column::ProposedRawValue)
        .column(change_proposal::Column::ProposedStandardCurveId)
        .column(change_proposal::Column::StoredStandardCurveId)
        .filter(change_proposal::Column::Id.is_in(ids.to_vec()))
        .filter(change_proposal::Column::Status.ne("accepted"))
        .lock(sea_orm::sea_query::LockType::Update);
    if let Some(scoped) = scoped_streams {
        query = query.filter(change_proposal::Column::StreamId.in_subquery(scoped));
    }
    query
        .into_model::<Pending>()
        .all(conn)
        .await
        .map_err(Into::into)
}

/// Accept or reject proposals. An accepted one is written through the curation record, so the
/// value carries who accepted it and that it arrived through this stream's source system; a
/// rejected one keeps its number, so the next pass re-proposes nothing.
pub async fn decide<C: ConnectionTrait>(
    conn: &C,
    ids: &[Uuid],
    accept: bool,
    actor: &str,
    reason: Option<&str>,
    projects: Option<&[Uuid]>,
) -> AppResult<(DecideResponse, Recorded)> {
    let mut response = DecideResponse {
        accepted: 0,
        rejected: 0,
        refused: Vec::new(),
    };
    let pending = load_undecided(conn, ids, projects).await?;
    let found: Vec<Uuid> = pending.iter().map(|p| p.id).collect();
    for id in ids {
        if !found.contains(id) {
            response.refused.push((
                *id,
                "no undecided proposal with that id in your projects".to_string(),
            ));
        }
    }
    let mut written = Recorded::default();
    if accept {
        let mut by_stream: std::collections::HashMap<Uuid, Vec<&Pending>> =
            std::collections::HashMap::new();
        for p in &pending {
            by_stream.entry(p.stream_id).or_default().push(p);
        }
        for (stream_id, rows) in by_stream {
            let corrections: Vec<(DateTime<Utc>, i16, serde_json::Value)> = rows
                .iter()
                .map(|p| {
                    (
                        p.time,
                        p.replicate_index,
                        serde_json::json!({ "raw_value": p.proposed_raw_value }),
                    )
                })
                .collect();
            written.absorb(
                record_keyed(
                    conn,
                    crate::routes::private::readings::models::Kind::ValueCorrection,
                    stream_id,
                    &corrections,
                    actor,
                    Some(reason.unwrap_or("accepted source correction")),
                    crate::routes::private::readings::models::Origin::Sync,
                    Keyed::Changed,
                    None,
                    None,
                )
                .await?,
            );
            // A curve the source named travels with the value it produced: correcting one and
            // leaving the other would store a number no curve accounts for.
            let curves: Vec<(DateTime<Utc>, i16, serde_json::Value)> = rows
                .iter()
                .filter(|p| p.proposed_standard_curve_id != p.stored_standard_curve_id)
                .map(|p| {
                    (
                        p.time,
                        p.replicate_index,
                        serde_json::json!({ "standard_curve_id": p.proposed_standard_curve_id }),
                    )
                })
                .collect();
            record_keyed(
                conn,
                crate::routes::private::readings::models::Kind::Curve,
                stream_id,
                &curves,
                actor,
                Some("accepted source correction"),
                crate::routes::private::readings::models::Origin::Sync,
                Keyed::Changed,
                None,
                None,
            )
            .await?;
        }
        // The decision trigger nulls `calibrated_value` for every correction naming `raw_value`,
        // so the accepted rows are put back through the curves they name here, in the transaction
        // that moved them. Left to the janitor's drift sweep, the uncorrected raw number is served
        // in the meantime and the move is recorded as a curve drift rather than as this decision.
        if !pending.is_empty() {
            let mut stream_ids: Vec<Uuid> = pending.iter().map(|p| p.stream_id).collect();
            stream_ids.sort_unstable();
            stream_ids.dedup();
            let first = pending.iter().map(|p| p.time).min().expect("non-empty");
            let last = pending.iter().map(|p| p.time).max().expect("non-empty");
            crate::routes::private::sensor_calibrations::service::recompose_from_own_curves(
                conn,
                Expr::cust("TRUE"),
                "r.stream_id = ANY($1) AND r.time >= $2 AND r.time <= $3",
                vec![stream_ids.into(), first.into(), last.into()],
            )
            .await?;
        }
        response.accepted = pending.len();
    } else {
        response.rejected = pending.len();
    }
    if !found.is_empty() {
        change_proposal::Entity::update_many()
            .col_expr(
                change_proposal::Column::Status,
                Expr::value(if accept { "accepted" } else { "rejected" }),
            )
            .col_expr(change_proposal::Column::DecidedBy, Expr::value(actor))
            .col_expr(
                change_proposal::Column::DecidedAt,
                Expr::current_timestamp(),
            )
            .filter(change_proposal::Column::Id.is_in(found.clone()))
            .exec(conn)
            .await?;
    }
    Ok((response, written))
}

/// How many proposals are awaiting a decision, per source system. The notification's subject.
pub async fn pending_by_source(db: &DatabaseConnection) -> AppResult<Vec<(String, i64)>> {
    use crate::routes::private::data_streams;
    change_proposal::Entity::find()
        .select_only()
        .column_as(data_streams::Column::SourceSystem, "source_system")
        .column_as(change_proposal::Column::Id.count(), "n")
        .join(
            JoinType::InnerJoin,
            change_proposal::Relation::DataStream.def(),
        )
        .filter(change_proposal::Column::Status.eq("pending"))
        .group_by(data_streams::Column::SourceSystem)
        .into_model::<SourceCount>()
        .all(db)
        .await
        .map(|rows| rows.into_iter().map(|c| (c.source_system, c.n)).collect())
        .map_err(Into::into)
}

/// The decision vocabulary, refused rather than defaulted: an unrecognised word is a bug in the
/// caller, not a licence to guess which way a value went.
pub fn parse_decision(decision: &str) -> AppResult<bool> {
    match decision {
        "accept" => Ok(true),
        "reject" => Ok(false),
        other => Err(AppError::BadRequest(format!(
            "decision must be 'accept' or 'reject', not '{other}'"
        ))),
    }
}

/// The tail an accepted correction takes. Identical in shape to the flag routes' curation tail:
/// the value moved, so everything computed from it moves with it.
pub(super) const ACCEPT_TAIL: Axes = Axes {
    cache: Cache::All,
    refresh: Refresh::Range { fatal: true },
    announce: false,
    reconcile_alarms: false,
    episodes: Episodes::None,
    recompute_derived: true,
    writer: crate::routes::private::collection_events::flows::Writer::Person,
};

/// When the value at each replicate last changed: the latest live `value_correction`'s `at`,
/// keyed by `(stream_id, replicate_index)`. `ingested_at` is the row's first arrival and no
/// decision moves it, so this is what says when the number now served appeared.
pub(super) async fn load_value_arrivals(
    db: &sea_orm::DatabaseConnection,
    stream_ids: &[Uuid],
    at: DateTime<Utc>,
) -> AppResult<HashMap<(Uuid, i16), DateTime<Utc>>> {
    let rows = decision_model::Entity::find()
        .select_only()
        .column(decision_model::Column::StreamId)
        .column(decision_model::Column::ReplicateIndex)
        .column_as(decision_model::Column::At.max(), "at")
        .filter(decision_model::Column::StreamId.is_in(stream_ids.to_vec()))
        .filter(decision_model::Column::Time.eq(sea_orm::prelude::DateTimeWithTimeZone::from(at)))
        .filter(decision_model::Column::Kind.eq(Kind::ValueCorrection.as_str()))
        .filter(decision_model::Column::RolledBackBy.is_null())
        .filter(decision_model::Column::ReplicateIndex.is_not_null())
        .group_by(decision_model::Column::StreamId)
        .group_by(decision_model::Column::ReplicateIndex)
        .into_model::<ArrivalRow>()
        .all(db)
        .await?;
    let mut out = HashMap::new();
    for r in rows {
        out.insert((r.stream_id, r.replicate_index), r.at.with_timezone(&Utc));
    }
    Ok(out)
}

/// Live instrument and calibration pins on the streams' instant (ADR 0008, M59), keyed by stream.
pub(super) async fn load_pins(
    db: &sea_orm::DatabaseConnection,
    stream_ids: &[Uuid],
    at: DateTime<Utc>,
) -> AppResult<HashMap<Uuid, Vec<PinRef>>> {
    let rows = decision_model::Entity::find()
        .filter(decision_model::Column::StreamId.is_in(stream_ids.to_vec()))
        .filter(decision_model::Column::Time.eq(sea_orm::prelude::DateTimeWithTimeZone::from(at)))
        .filter(
            decision_model::Column::Kind
                .is_in([Kind::InstrumentPin.as_str(), Kind::CalibrationPin.as_str()]),
        )
        .filter(decision_model::Column::RolledBackBy.is_null())
        .order_by_desc(decision_model::Column::At)
        .all(db)
        .await?;
    let mut out: HashMap<Uuid, Vec<PinRef>> = HashMap::new();
    for r in rows {
        out.entry(r.stream_id).or_default().push(PinRef {
            decision_id: r.id,
            kind: r.kind,
            replicate_index: r.replicate_index,
            target: r.new,
            actor: r.actor,
            at: r.at.with_timezone(&Utc),
            reason: r.reason,
            set_id: r.set_id,
        });
    }
    Ok(out)
}

/// The stream's origin class, from the writer-side source-system set in
/// `collection_events::attach`.
/// How a reading reached the store, from the stream it arrived on: `manual`, `csv`, `api`,
/// `derived` or `sync`. One definition, so every surface naming an origin names the same thing. A
/// computed value was made here rather than sent, so it is not a sync.
pub fn classify_source(source_system: &str) -> &'static str {
    match source_system {
        "grab_sample" => "manual",
        "csv" | "csv_import" => "csv",
        "api" => "api",
        "derived" => "derived",
        _ => "sync",
    }
}

/// Where a reading came from, as the row itself records it.
///
/// Q49: a blob is stored only where nothing else records the story (a tool run, a chain, a CSV
/// import, a hand entry, a batch); a sync or derived reading's story is resolved from the stream,
/// the covering receipt and the definition. The discriminator is stored on every row either way,
/// so an origin nothing recorded is a named kind rather than a NULL blob.
pub const PROVENANCE_KINDS: [&str; 7] = [
    "tool_run",
    "chain",
    "csv_import",
    "manual",
    "batch",
    "sync",
    "derived",
];

/// The kind a writer with no better evidence stamps, from the row's own classification and the
/// stream it arrived on. Mirrors `readings_default_provenance_kind`, the trigger that holds the
/// column total for a writer that names none.
#[must_use]
pub fn provenance_kind_for_stream(
    measurement_type: Option<&str>,
    source_system: &str,
) -> &'static str {
    if measurement_type == Some("derived") {
        return "derived";
    }
    match source_system {
        "grab_sample" => "manual",
        "api" => "batch",
        _ => "sync",
    }
}

/// The kind of a save that names a tool run, from the run's own minting path
/// (`tool_runs.source`). A hand entry that names no run is `manual`.
#[must_use]
pub fn provenance_kind_for_run(run_source: Option<&str>) -> &'static str {
    match run_source {
        None => "manual",
        Some("chain") => "chain",
        Some("csv_import") => "csv_import",
        Some(_) => "tool_run",
    }
}

pub(super) const PROVENANCE_ROW_COLUMNS: [readings::Column; 25] = [
    readings::Column::StreamId,
    readings::Column::ReplicateIndex,
    readings::Column::SiteId,
    readings::Column::ParameterId,
    readings::Column::RawValue,
    readings::Column::CalibratedValue,
    readings::Column::SensorId,
    readings::Column::CalibrationId,
    readings::Column::StandardCurveId,
    readings::Column::DeploymentId,
    readings::Column::MeasurementType,
    readings::Column::IsFlagged,
    readings::Column::FlagReason,
    readings::Column::SampleId,
    readings::Column::CollectionEventId,
    readings::Column::WithdrawnAt,
    readings::Column::WithdrawnReason,
    readings::Column::Unverified,
    readings::Column::IngestedAt,
    readings::Column::ProvenanceKind,
    readings::Column::Provenance,
    readings::Column::DerivedVersionId,
    readings::Column::Label,
    readings::Column::Notes,
    readings::Column::CreatedBy,
];

/// A readings select narrowed to the columns a [`RawRow`] decodes.
fn provenance_rows() -> sea_orm::Select<readings::Entity> {
    readings::Entity::find()
        .select_only()
        .columns(PROVENANCE_ROW_COLUMNS)
}

/// Refuse a scoped caller the reading at `stream_id` and `time` when it is outside its projects,
/// as not-found, by the same rows the provenance record reads.
pub async fn require_reading_in_scope(
    db: &sea_orm::DatabaseConnection,
    scope: &crate::common::authz::AccessScope,
    stream_id: Uuid,
    time: DateTime<Utc>,
) -> AppResult<()> {
    if !scope.is_restricted() {
        return Ok(());
    }
    let key = ProvenanceQuery {
        time,
        stream_id: Some(stream_id),
        site_id: None,
        parameter_id: None,
        measurement_type: None,
    };
    rows_at(db, &key, scope).await.map(|_| ())
}

/// The replicate group at the instant, by either key form, refused to a scoped caller who may not
/// see it. Both the record and the ledger start from these rows, so they can never disagree about
/// which reading was asked for.
pub async fn rows_at(
    db: &sea_orm::DatabaseConnection,
    q: &ProvenanceQuery,
    scope: &crate::common::authz::AccessScope,
) -> AppResult<Vec<RawRow>> {
    let find = provenance_rows().filter(readings::Column::Time.eq(q.time));
    let rows: Vec<RawRow> = match (q.stream_id, q.site_id, q.parameter_id) {
        (Some(stream_id), _, _) => find
            .filter(readings::Column::StreamId.eq(stream_id))
            .order_by_asc(readings::Column::ReplicateIndex),
        (None, Some(site_id), Some(parameter_id)) => {
            let cadence = match q.measurement_type.as_deref() {
                None => Condition::all(),
                // The same word the readings query serves under: everything that is not a grab.
                // A derived row plots on the continuous line, so a chart that drew it must be able
                // to resolve the point it drew.
                Some("continuous") => {
                    Condition::all().add(Expr::cust("measurement_type IS DISTINCT FROM 'spot'"))
                }
                Some(other) => Condition::all()
                    .add(readings::Column::MeasurementType.eq(sanitize_cadence(other)?)),
            };
            find.filter(readings::Column::SiteId.eq(site_id))
                .filter(readings::Column::ParameterId.eq(parameter_id))
                .filter(cadence)
                .order_by_asc(readings::Column::StreamId)
                .order_by_asc(readings::Column::ReplicateIndex)
        }
        _ => {
            return Err(AppError::BadRequest(
                "Provide either stream_id or both site_id and parameter_id".to_string(),
            ));
        }
    }
    .into_model::<RawRow>()
    .all(db)
    .await?;

    if rows.is_empty() {
        return Err(AppError::NotFound("No reading at that instant".to_string()));
    }

    // A project-scoped key sees another project's data (or unattributed rows) as not-found.
    if scope.is_restricted() {
        let project = match rows.iter().find_map(|r| r.site_id) {
            Some(site_id) => sites::Entity::find_by_id(site_id)
                .one(db)
                .await?
                .and_then(|s| s.project_id),
            None => None,
        };
        if !scope.allows_project_opt(project) {
            return Err(AppError::NotFound("No reading at that instant".to_string()));
        }
    }
    Ok(rows)
}

/// The readings of one instant, grouped by stream into assembled records. Every lookup is batched
/// over the whole row set, so a visit's twenty cells cost the same number of queries as one.
pub async fn assemble_records(
    db: &sea_orm::DatabaseConnection,
    rows: &[RawRow],
    time: DateTime<Utc>,
) -> AppResult<Vec<ProvenanceRecord>> {
    // --- Batch-resolve everything the rows reference ---
    let mut groups: BTreeMap<Uuid, Vec<&RawRow>> = BTreeMap::new();
    for r in rows {
        groups.entry(r.stream_id).or_default().push(r);
    }
    let collect = |f: fn(&RawRow) -> Option<Uuid>| -> Vec<Uuid> {
        rows.iter()
            .filter_map(f)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    };

    let streams: HashMap<Uuid, data_streams::Model> = data_streams::Entity::find()
        .filter(data_streams::Column::Id.is_in(groups.keys().copied().collect::<Vec<_>>()))
        .all(db)
        .await?
        .into_iter()
        .map(|s| (s.id, s))
        .collect();
    let sensor_map: HashMap<Uuid, sensors::Model> = sensors::Entity::find()
        .filter(sensors::Column::Id.is_in(collect(|r| r.sensor_id)))
        .all(db)
        .await?
        .into_iter()
        .map(|s| (s.id, s))
        .collect();
    let deployment_map: HashMap<Uuid, deployments::Model> = deployments::Entity::find()
        .filter(deployments::Column::Id.is_in(collect(|r| r.deployment_id)))
        .all(db)
        .await?
        .into_iter()
        .map(|d| (d.id, d))
        .collect();
    let calibration_map: HashMap<Uuid, sensor_calibrations::Model> =
        sensor_calibrations::Entity::find()
            .filter(sensor_calibrations::Column::Id.is_in(collect(|r| r.calibration_id)))
            .all(db)
            .await?
            .into_iter()
            .map(|c| (c.id, c))
            .collect();
    let curve_map: HashMap<Uuid, standard_curves::Model> = standard_curves::Entity::find()
        .filter(standard_curves::Column::Id.is_in(collect(|r| r.standard_curve_id)))
        .all(db)
        .await?
        .into_iter()
        .map(|c| (c.id, c))
        .collect();
    let event_map: HashMap<Uuid, collection_events::Model> = collection_events::Entity::find()
        .filter(collection_events::Column::Id.is_in(collect(|r| r.collection_event_id)))
        .all(db)
        .await?
        .into_iter()
        .map(|e| (e.id, e))
        .collect();
    let sample_map: HashMap<Uuid, samples::Model> = samples::Entity::find()
        .filter(samples::Column::Id.is_in(collect(|r| r.sample_id)))
        .all(db)
        .await?
        .into_iter()
        .map(|s| (s.id, s))
        .collect();
    let site_names: HashMap<Uuid, String> = sites::Entity::find()
        .filter(
            sites::Column::Id.is_in(
                deployment_map
                    .values()
                    .map(|d| d.site_id)
                    .collect::<HashSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>(),
            ),
        )
        .all(db)
        .await?
        .into_iter()
        .map(|s| (s.id, s.name))
        .collect();

    let stream_ids: Vec<Uuid> = groups.keys().copied().collect();
    let mut receipts = fetch_covering_receipts(db, &stream_ids, time).await?;
    let mut holds_by_stream = fetch_stream_holds(db, &stream_ids, time).await?;
    let mut holds_by_slot = fetch_slot_holds(db, rows, time).await?;
    let mut pins = load_pins(db, &stream_ids, time).await?;
    let value_arrivals = load_value_arrivals(db, &stream_ids, time).await?;
    let run_sources = fetch_run_sources(db, rows).await?;
    let (mut calculations, formula_versions) = fetch_calculations(db, rows).await?;
    let (retired_by_id, retired_by_name) = fetch_decommissions(
        db,
        &calculations
            .values()
            .map(|c| c.tool_script_id)
            .collect::<Vec<_>>(),
        &run_sources
            .values()
            .map(|(_, tool)| tool.clone())
            .collect::<Vec<_>>(),
    )
    .await?;
    for calc in calculations.values_mut() {
        calc.decommissioned = retired_by_id.get(&calc.tool_script_id).cloned();
    }
    let links = fetch_formula_links(db, rows).await?;
    let served = fetch_served_values(db, rows, &links, time).await?;
    let mut consumed_sets =
        super::consumed::resolve_many(db, &fetch_consumed_sets(db, rows, time).await?).await?;

    let mut records = Vec::with_capacity(groups.len());
    for (stream_id, group) in &groups {
        let stream = streams
            .get(stream_id)
            .ok_or_else(|| AppError::NotFound("Stream not found".to_string()))?;

        let receipt = receipts.remove(stream_id);
        let mut holds = holds_by_stream.remove(stream_id).unwrap_or_default();
        if let (Some(site_id), Some(parameter_id)) = (group[0].site_id, group[0].parameter_id) {
            holds.extend(
                holds_by_slot
                    .remove(&(site_id, parameter_id))
                    .unwrap_or_default(),
            );
        }

        let readings_out: Vec<ReadingFacet> = group
            .iter()
            .map(|r| ReadingFacet {
                replicate_index: r.replicate_index,
                raw_value: r.raw_value,
                calibrated_value: r.calibrated_value,
                measurement_type: r.measurement_type.clone(),
                is_flagged: r.is_flagged.unwrap_or(false),
                flag_reason: r.flag_reason.clone(),
                withdrawn_at: r.withdrawn_at,
                unverified: r.unverified.unwrap_or(false),
                withdrawn_reason: r.withdrawn_reason.clone(),
                ingested_at: r.ingested_at,
                value_arrived_at: value_arrivals
                    .get(&(*stream_id, r.replicate_index))
                    .copied()
                    .or(r.ingested_at),
                provenance_kind: r.provenance_kind.clone(),
                calibration: r.calibration_id.and_then(|id| {
                    calibration_map.get(&id).map(|c| CalibrationRef {
                        id: c.id,
                        slope: c.slope,
                        intercept: c.intercept,
                        valid_from: c.valid_from,
                        valid_until: c.valid_until,
                        retired_at: c.retired_at,
                    })
                }),
                standard_curve: r.standard_curve_id.and_then(|id| {
                    curve_map.get(&id).map(|c| CurveRef {
                        id: c.id,
                        sensor_id: c.sensor_id,
                        name: c.name.clone(),
                        slope: c.slope,
                        intercept: c.intercept,
                        retired_at: c.retired_at,
                    })
                }),
            })
            .collect();

        let sensor = group
            .iter()
            .find_map(|r| r.sensor_id)
            .and_then(|id| sensor_map.get(&id))
            .map(|s| SensorRef {
                id: s.id,
                serial_number: s.serial_number.clone(),
                name: s.name.clone(),
                manufacturer: s.manufacturer.clone(),
                model: s.model.clone(),
            });
        let deployment = group
            .iter()
            .find_map(|r| r.deployment_id)
            .and_then(|id| deployment_map.get(&id))
            .map(|d| DeploymentRef {
                id: d.id,
                site_id: d.site_id,
                site_name: site_names.get(&d.site_id).cloned(),
                deployed_from: d.deployed_from,
                deployed_until: d.deployed_until,
            });
        let event = group
            .iter()
            .find_map(|r| r.collection_event_id)
            .and_then(|id| event_map.get(&id))
            .map(|e| EventRef {
                id: e.id,
                collected_at: e.collected_at,
                source: e.source.clone(),
                created_by: e.created_by.clone(),
            });
        // The story of a measurement lives on the reading, so a group with no statistics row
        // still has one; the sample adds its statistics.
        let sample = group
            .iter()
            .find_map(|r| r.sample_id)
            .and_then(|id| sample_map.get(&id));
        let blob = group.iter().find_map(|r| r.provenance.clone());
        let entered_by = group.iter().find_map(|r| r.created_by.clone());
        let computation = if blob.is_some() || entered_by.is_some() || sample.is_some() {
            let run = run_id_of(blob.as_ref()).and_then(|id| run_sources.get(&id));
            let run_source = run.map(|(source, _)| source.clone());
            let decommissioned = run.and_then(|(_, tool)| retired_by_name.get(tool).cloned());
            Some(ComputationInfo {
                sample_id: sample.map(|s| s.id),
                created_by: entered_by,
                label: group.iter().find_map(|r| r.label.clone()),
                notes: group.iter().find_map(|r| r.notes.clone()),
                provenance: blob,
                run_source,
                decommissioned,
                n: sample.map(|s| s.n),
                mean: sample.and_then(|s| s.mean),
                stdev: sample.and_then(|s| s.stdev),
                median: sample.and_then(|s| s.median),
                min: sample.and_then(|s| s.min_value),
                max: sample.and_then(|s| s.max_value),
            })
        } else {
            None
        };

        // A derived value's calculation, with the version this group's rows name where they
        // name one at all.
        let calculation = group
            .iter()
            .filter(|r| r.measurement_type.as_deref() == Some("derived"))
            .find_map(|r| r.parameter_id)
            .and_then(|parameter_id| calculations.get(&parameter_id).cloned())
            .map(|mut calc| {
                if let Some((version_id, (version_no, formula, content_hash))) = group
                    .iter()
                    .find_map(|r| r.derived_version_id)
                    .and_then(|id| formula_versions.get(&id).map(|v| (id, v)))
                {
                    calc.version_id = Some(version_id);
                    calc.version_no = Some(*version_no);
                    calc.formula = Some(formula.clone());
                    calc.content_hash = Some(content_hash.clone());
                }
                calc
            });

        let indexes: Vec<i16> = group.iter().map(|r| r.replicate_index).collect();
        let (inputs, consumers) = match (group[0].site_id, group[0].parameter_id) {
            (Some(site_id), Some(parameter_id)) => (
                links.inputs_of(parameter_id, &indexes, &served, site_id),
                links.consumers_of(parameter_id, &indexes, &served, site_id),
            ),
            _ => (Vec::new(), Vec::new()),
        };
        let consumed = consumed_sets.remove(stream_id).unwrap_or_default();

        records.push(ProvenanceRecord {
            origin: OriginInfo {
                stream_id: *stream_id,
                source_system: stream.source_system.clone(),
                source_key: stream.source_key.clone(),
                source_name: stream.source_name.clone(),
                classification: classify_source(&stream.source_system).to_string(),
                paired_at: stream.paired_at.map(|t| t.with_timezone(&Utc)),
                ingested_at: group.iter().filter_map(|r| r.ingested_at).max(),
                value_arrived_at: group
                    .iter()
                    .filter_map(|r| {
                        value_arrivals
                            .get(&(*stream_id, r.replicate_index))
                            .copied()
                            .or(r.ingested_at)
                    })
                    .max(),
                receipt,
                portal_calculation: super::portal_calculation::of_stream(
                    db,
                    stream,
                    group[0].site_id,
                    time,
                )
                .await?,
            },
            readings: readings_out,
            chain: ChainInfo {
                sensor,
                deployment,
                pins: pins.remove(stream_id).unwrap_or_default(),
            },
            event,
            computation,
            calculation,
            inputs,
            consumers,
            consumed,
            holds,
        });
    }

    Ok(records)
}

/// Every record at a collection event, keyed by the stream that serves it.
pub async fn records_for_event(
    db: &sea_orm::DatabaseConnection,
    event_id: Uuid,
    collected_at: DateTime<Utc>,
) -> AppResult<HashMap<Uuid, ProvenanceRecord>> {
    let rows: Vec<RawRow> = provenance_rows()
        .filter(readings::Column::CollectionEventId.eq(event_id))
        .order_by_asc(readings::Column::StreamId)
        .order_by_asc(readings::Column::ReplicateIndex)
        .into_model::<RawRow>()
        .all(db)
        .await?;
    Ok(assemble_records(db, &rows, collected_at)
        .await?
        .into_iter()
        .map(|r| (r.origin.stream_id, r))
        .collect())
}

/// What each record's calculation consumed, as it was captured at the read (Q215), keyed by the
/// stream the record is served by.
///
/// A tool run keeps its set on the run row, one per run. A derived value keeps its on the
/// decision that wrote it, so the newest of those at the key is the set behind the stored number;
/// a recompute that moved the value appended a `formula_transition` and a first computation a
/// `derived_computed`, and both are read here by the ledger order the lock makes effect order.
/// What the catalog supplied is reported as the first computation read it, which
/// [`super::consumed::as_first_read`] resolves against the oldest of the same rows.
pub(super) async fn fetch_consumed_sets(
    db: &sea_orm::DatabaseConnection,
    rows: &[RawRow],
    time: DateTime<Utc>,
) -> AppResult<HashMap<Uuid, Vec<ConsumedInput>>> {
    let mut out: HashMap<Uuid, Vec<ConsumedInput>> = HashMap::new();

    // The first blob in the group, which is the one the record itself reports.
    let mut runs: HashMap<Uuid, Uuid> = HashMap::new();
    for row in rows {
        if let Some(run) = run_id_of(row.provenance.as_ref()) {
            runs.entry(row.stream_id).or_insert(run);
        }
    }
    if !runs.is_empty() {
        let contexts: HashMap<Uuid, Option<serde_json::Value>> = tool_run::Entity::find()
            .filter(tool_run::Column::Id.is_in(runs.values().copied().collect::<Vec<_>>()))
            .select_only()
            .column(tool_run::Column::Id)
            .column(tool_run::Column::Context)
            .into_tuple::<(Uuid, Option<serde_json::Value>)>()
            .all(db)
            .await?
            .into_iter()
            .collect();
        for (stream_id, run_id) in &runs {
            let captured = contexts
                .get(run_id)
                .and_then(|c| c.as_ref())
                .and_then(|c| c.get("consumed"))
                .cloned();
            if let Some(set) = captured.and_then(consumed_set) {
                out.insert(*stream_id, set);
            }
        }
    }

    let derived: Vec<Uuid> = rows
        .iter()
        .filter(|r| r.measurement_type.as_deref() == Some("derived"))
        .map(|r| r.stream_id)
        .collect();
    if !derived.is_empty() {
        let ledger = decision_model::Entity::find()
            .filter(decision_model::Column::StreamId.is_in(derived))
            .filter(decision_model::Column::Time.eq(time))
            .filter(decision_model::Column::Kind.is_in([
                Kind::FormulaTransition.as_str(),
                Kind::DerivedComputed.as_str(),
            ]))
            .order_by_desc(decision_model::Column::Seq)
            .all(db)
            .await?;
        // Ordered newest first, so the last set seen at a stream is its first computation.
        let mut newest: HashMap<Uuid, Vec<ConsumedInput>> = HashMap::new();
        let mut first: HashMap<Uuid, Vec<ConsumedInput>> = HashMap::new();
        for row in ledger {
            if out.contains_key(&row.stream_id) {
                continue;
            }
            let Some(set) = row.new.get("consumed").cloned().and_then(consumed_set) else {
                continue;
            };
            newest.entry(row.stream_id).or_insert_with(|| set.clone());
            first.insert(row.stream_id, set);
        }
        for (stream_id, set) in newest {
            let origin = first.get(&stream_id).map_or(&[][..], Vec::as_slice);
            out.insert(stream_id, super::consumed::as_first_read(set, origin));
        }
    }
    Ok(out)
}

/// A stored `consumed` array as the capture wrote it. A shape this build does not understand is
/// no set at all, which reads as unknown rather than as a partial one.
fn consumed_set(stored: serde_json::Value) -> Option<Vec<ConsumedInput>> {
    serde_json::from_value::<Vec<ConsumedInput>>(stored)
        .ok()
        .filter(|set| !set.is_empty())
}

pub fn run_id_of(blob: Option<&serde_json::Value>) -> Option<Uuid> {
    blob?
        .get("run_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
}

pub(super) fn sanitize_cadence(value: &str) -> AppResult<&str> {
    match value {
        "spot" | "derived" => Ok(value),
        _ => Err(AppError::BadRequest(format!(
            "measurement_type must be continuous, spot or derived, got '{value}'"
        ))),
    }
}

/// The latest windowed-ingest pass covering the instant, per stream.
pub(super) async fn fetch_covering_receipts(
    db: &sea_orm::DatabaseConnection,
    stream_ids: &[Uuid],
    time: DateTime<Utc>,
) -> AppResult<HashMap<Uuid, ReceiptSummary>> {
    let rows = db
        .query_all_raw(build(covering_receipts_query(stream_ids, time)))
        .await?;
    let mut out = HashMap::new();
    for row in rows
        .iter()
        .map(|r| CoveringReceipt::from_query_result(r, ""))
    {
        let row = row?;
        out.insert(
            row.stream_id,
            ReceiptSummary {
                id: row.id,
                at: row.at.map_or(time, |t| t.with_timezone(&Utc)),
                window_from: row.window_from.map(|t| t.with_timezone(&Utc)),
                window_to: row.window_to.map(|t| t.with_timezone(&Utc)),
                submitted: row.submitted,
                new_rows: row.new_rows,
                changed: row.changed,
                unchanged: row.unchanged,
                withdrawn: row.withdrawn,
                rejected_total: row.rejected_total,
                braked: row.braked,
            },
        );
    }
    Ok(out)
}

/// The statuses a hold is still live under, as the ledger's "what is open here" readers name it:
/// awaiting review, or reviewed but not yet acted on.
fn live_hold_statuses() -> Vec<&'static str> {
    vec![
        HoldStatus::Pending.as_str(),
        HoldStatus::Deferred.as_str(),
        HoldStatus::Acknowledged.as_str(),
    ]
}

/// A built statement as SeaORM takes it. Every reader below builds its query and hands it over
/// here, so no reader spells SQL and none of them repeats the handoff.
pub(super) fn build(query: sea_orm::sea_query::SelectStatement) -> Statement {
    let (sql, values) = query.build(PostgresQueryBuilder);
    Statement::from_sql_and_values(sea_orm::DatabaseBackend::Postgres, sql, values)
}

/// The latest receipt per stream whose window covers the instant.
fn covering_receipts_query(
    stream_ids: &[Uuid],
    time: DateTime<Utc>,
) -> sea_orm::sea_query::SelectStatement {
    use crate::routes::private::data_streams::models::receipts as ingest_receipt;
    Query::select()
        .distinct_on([ingest_receipt::Column::StreamId])
        .columns([
            ingest_receipt::Column::StreamId,
            ingest_receipt::Column::Id,
            ingest_receipt::Column::At,
            ingest_receipt::Column::WindowFrom,
            ingest_receipt::Column::WindowTo,
            ingest_receipt::Column::Submitted,
            ingest_receipt::Column::NewRows,
            ingest_receipt::Column::Changed,
            ingest_receipt::Column::Unchanged,
            ingest_receipt::Column::Withdrawn,
            ingest_receipt::Column::RejectedTotal,
            ingest_receipt::Column::Braked,
        ])
        .from(ingest_receipt::Entity)
        .cond_where(
            Condition::all()
                .add(Expr::col(ingest_receipt::Column::StreamId).is_in(stream_ids.to_vec()))
                .add(Expr::col(ingest_receipt::Column::WindowFrom).lte(time))
                .add(Expr::col(ingest_receipt::Column::WindowTo).gte(time)),
        )
        .order_by(ingest_receipt::Column::StreamId, Order::Asc)
        .order_by(ingest_receipt::Column::At, Order::Desc)
        .take()
}

/// Live replicate-statistics holds on these streams at the instant. The `parameter_id` column is
/// null so the row reads as [`HoldRow`], which both hold readers share.
fn stream_holds_query(
    stream_ids: &[Uuid],
    time: DateTime<Utc>,
) -> sea_orm::sea_query::SelectStatement {
    use crate::routes::private::sync::hold_model as holds;
    Query::select()
        .column(holds::Column::StreamId)
        .expr_as(
            Expr::cust("NULL::uuid"),
            Alias::new(holds::Column::ParameterId.as_str()),
        )
        .columns([
            holds::Column::Id,
            holds::Column::Kind,
            holds::Column::Status,
            holds::Column::CreatedAt,
            holds::Column::Tool,
        ])
        .from(holds::Entity)
        .cond_where(
            Condition::all()
                .add(Expr::col(holds::Column::StreamId).is_in(stream_ids.to_vec()))
                .add(Expr::col(holds::Column::GroupTime).eq(time))
                .add(Expr::col(holds::Column::Status).is_in(live_hold_statuses())),
        )
        .order_by(holds::Column::CreatedAt, Order::Desc)
        .take()
}

/// Live holds no stream produced: the event-audit findings and reconciliation holds keyed on the
/// slot instead.
fn slot_holds_query(
    site_id: Uuid,
    parameter_ids: &[Uuid],
    time: DateTime<Utc>,
) -> sea_orm::sea_query::SelectStatement {
    use crate::routes::private::sync::hold_model as holds;
    Query::select()
        .expr_as(
            Expr::cust("NULL::uuid"),
            Alias::new(holds::Column::StreamId.as_str()),
        )
        .columns([
            holds::Column::ParameterId,
            holds::Column::Id,
            holds::Column::Kind,
            holds::Column::Status,
            holds::Column::CreatedAt,
            holds::Column::Tool,
        ])
        .from(holds::Entity)
        .cond_where(
            Condition::all()
                .add(Expr::col(holds::Column::StreamId).is_null())
                .add(Expr::col(holds::Column::SiteId).eq(site_id))
                .add(Expr::col(holds::Column::ParameterId).is_in(parameter_ids.to_vec()))
                .add(Expr::col(holds::Column::GroupTime).eq(time))
                .add(Expr::col(holds::Column::Status).is_in(live_hold_statuses())),
        )
        .order_by(holds::Column::CreatedAt, Order::Desc)
        .take()
}

/// The formulas one hop from these parameters: the ones that output them, and the ones that read
/// them as a source.
fn formulas_one_hop_query(parameter_ids: &[Uuid]) -> sea_orm::sea_query::SelectStatement {
    use crate::routes::private::derived_parameters::models::definition::{
        Column as FormulaColumn, Entity as FormulaEntity,
    };
    use crate::routes::private::derived_parameters::models::source;
    use crate::routes::private::parameters as parameters_entity;
    use crate::routes::private::tools::models::script as tool_script;

    let d = Alias::new("d");
    let p = Alias::new("p");
    let sc = Alias::new("sc");
    let reads_one = Query::select()
        .column(source::Column::DerivedDefinitionId)
        .from(source::Entity)
        .and_where(Expr::col(source::Column::ParameterId).is_in(parameter_ids.to_vec()))
        .take();

    Query::select()
        .columns([
            (d.clone(), FormulaColumn::Id),
            (d.clone(), FormulaColumn::Code),
            (d.clone(), FormulaColumn::Name),
            (d.clone(), FormulaColumn::PerReplicate),
            (d.clone(), FormulaColumn::OutputParameterId),
        ])
        .expr_as(
            Expr::col((p.clone(), parameters_entity::Column::Code)),
            Alias::new("output_parameter_code"),
        )
        .expr_as(
            Expr::col((sc.clone(), tool_script::Column::Name)),
            Alias::new("calculation"),
        )
        .expr_as(
            Func::coalesce([
                Expr::col((sc.clone(), tool_script::Column::Enabled)),
                Expr::value(true),
            ]),
            Alias::new("enabled"),
        )
        .from_as(FormulaEntity, d.clone())
        .join_as(
            JoinType::LeftJoin,
            tool_script::Entity,
            sc.clone(),
            Expr::col((sc, tool_script::Column::Id))
                .equals((d.clone(), FormulaColumn::ToolScriptId)),
        )
        .join_as(
            JoinType::LeftJoin,
            parameters_entity::Entity,
            p.clone(),
            Expr::col((p, parameters_entity::Column::Id))
                .equals((d.clone(), FormulaColumn::OutputParameterId)),
        )
        .cond_where(
            Condition::any()
                .add(
                    Expr::col((d.clone(), FormulaColumn::OutputParameterId))
                        .is_in(parameter_ids.to_vec()),
                )
                .add(Expr::col((d.clone(), FormulaColumn::Id)).in_subquery(reads_one)),
        )
        .order_by((d.clone(), FormulaColumn::Ordinal), Order::Asc)
        .order_by((d, FormulaColumn::Code), Order::Asc)
        .take()
}

/// The sources of these formulas, with the code of the parameter each reads.
fn formula_sources_query(definition_ids: &[Uuid]) -> sea_orm::sea_query::SelectStatement {
    use crate::routes::private::derived_parameters::models::source;
    use crate::routes::private::parameters as parameters_entity;

    let ds = Alias::new("ds");
    let p = Alias::new("p");
    Query::select()
        .columns([
            (ds.clone(), source::Column::DerivedDefinitionId),
            (ds.clone(), source::Column::VariableName),
            (ds.clone(), source::Column::ParameterId),
            (ds.clone(), source::Column::SiteProperty),
        ])
        .expr_as(
            Expr::col((p.clone(), parameters_entity::Column::Code)),
            Alias::new("parameter_code"),
        )
        .from_as(source::Entity, ds.clone())
        .join_as(
            JoinType::LeftJoin,
            parameters_entity::Entity,
            p.clone(),
            Expr::col((p, parameters_entity::Column::Id))
                .equals((ds.clone(), source::Column::ParameterId)),
        )
        .and_where(
            Expr::col((ds.clone(), source::Column::DerivedDefinitionId))
                .is_in(definition_ids.to_vec()),
        )
        .order_by((ds, source::Column::VariableName), Order::Asc)
        .take()
}

/// Each site as one jsonb row, which is what lets a formula read whichever site property it names
/// without the reader knowing the column list.
fn site_rows_query(site_ids: &[Uuid]) -> sea_orm::sea_query::SelectStatement {
    use crate::routes::private::sites::models as sites_model;
    Query::select()
        .column(sites_model::Column::Id)
        .expr_as(Expr::cust("to_jsonb(sites)"), Alias::new("row"))
        .from(sites_model::Entity)
        .and_where(Expr::col(sites_model::Column::Id).is_in(site_ids.to_vec()))
        .take()
}

/// The catalog's code, name and units, and the slot's declared precision.
fn slot_identity_query(site_id: Uuid, parameter_id: Uuid) -> sea_orm::sea_query::SelectStatement {
    use crate::routes::private::parameters as parameters_entity;
    use crate::routes::private::site_parameters::models as site_parameters_model;

    let p = Alias::new("p");
    let sp = Alias::new("sp");
    Query::select()
        .columns([
            (p.clone(), parameters_entity::Column::Code),
            (p.clone(), parameters_entity::Column::Name),
        ])
        .expr_as(
            Expr::col((p.clone(), parameters_entity::Column::DefaultUnits)),
            Alias::new("units"),
        )
        .column((sp.clone(), site_parameters_model::Column::DecimalPlaces))
        .from_as(parameters_entity::Entity, p.clone())
        .join_as(
            JoinType::LeftJoin,
            site_parameters_model::Entity,
            sp.clone(),
            Condition::all()
                .add(
                    Expr::col((sp.clone(), site_parameters_model::Column::ParameterId))
                        .equals((p.clone(), parameters_entity::Column::Id)),
                )
                .add(Expr::col((sp, site_parameters_model::Column::SiteId)).eq(site_id)),
        )
        .and_where(Expr::col((p, parameters_entity::Column::Id)).eq(parameter_id))
        .limit(1)
        .take()
}

/// Replicate-statistics holds keyed by stream at the instant. Terminal holds are left out.
pub(super) async fn fetch_stream_holds(
    db: &sea_orm::DatabaseConnection,
    stream_ids: &[Uuid],
    time: DateTime<Utc>,
) -> AppResult<HashMap<Uuid, Vec<HoldRef>>> {
    let rows = db
        .query_all_raw(build(stream_holds_query(stream_ids, time)))
        .await?;
    let mut out: HashMap<Uuid, Vec<HoldRef>> = HashMap::new();
    for row in rows.iter().map(|r| HoldRow::from_query_result(r, "")) {
        let row = row?;
        if let Some(stream_id) = row.stream_id {
            out.entry(stream_id).or_default().push((&row).into());
        }
    }
    Ok(out)
}

/// Event-audit findings and reconciliation holds keyed by (site, parameter) at the instant.
pub(super) async fn fetch_slot_holds(
    db: &sea_orm::DatabaseConnection,
    rows: &[RawRow],
    time: DateTime<Utc>,
) -> AppResult<HashMap<(Uuid, Uuid), Vec<HoldRef>>> {
    let mut by_site: BTreeMap<Uuid, HashSet<Uuid>> = BTreeMap::new();
    for r in rows {
        if let (Some(site_id), Some(parameter_id)) = (r.site_id, r.parameter_id) {
            by_site.entry(site_id).or_default().insert(parameter_id);
        }
    }
    let mut out: HashMap<(Uuid, Uuid), Vec<HoldRef>> = HashMap::new();
    for (site_id, parameter_ids) in by_site {
        let found = db
            .query_all_raw(build(slot_holds_query(
                site_id,
                &parameter_ids.into_iter().collect::<Vec<_>>(),
                time,
            )))
            .await?;
        for row in found.iter().map(|r| HoldRow::from_query_result(r, "")) {
            let row = row?;
            if let Some(parameter_id) = row.parameter_id {
                out.entry((site_id, parameter_id))
                    .or_default()
                    .push((&row).into());
            }
        }
    }
    Ok(out)
}

/// The minting path and calculation name of every tool run the rows' provenance blobs name.
pub(super) async fn fetch_run_sources(
    db: &sea_orm::DatabaseConnection,
    rows: &[RawRow],
) -> AppResult<HashMap<Uuid, (String, String)>> {
    let run_ids: Vec<Uuid> = rows
        .iter()
        .filter_map(|r| run_id_of(r.provenance.as_ref()))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    if run_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let found: Vec<(Uuid, String, String)> = tool_run::Entity::find()
        .filter(tool_run::Column::Id.is_in(run_ids))
        .select_only()
        .column(tool_run::Column::Id)
        .column(tool_run::Column::Source)
        .column(tool_run::Column::ToolName)
        .into_tuple()
        .all(db)
        .await?;
    Ok(found
        .into_iter()
        .map(|(id, source, tool)| (id, (source, tool)))
        .collect())
}

/// The decommission a `tool_scripts` row records, when all three of its columns are set.
pub(super) fn decommission_of(
    at: Option<DateTime<Utc>>,
    by: Option<String>,
    reason: Option<String>,
) -> Option<Decommission> {
    Some(Decommission {
        at: at?,
        by: by?,
        reason: reason?,
    })
}

/// The decommissioned calculations among those named, keyed by id and by name: a formula value
/// names its calculation by id, a run by the name it was executed under.
pub(super) async fn fetch_decommissions(
    db: &sea_orm::DatabaseConnection,
    ids: &[Uuid],
    names: &[String],
) -> AppResult<(HashMap<Uuid, Decommission>, HashMap<String, Decommission>)> {
    use crate::routes::private::tools::models::script as tool_script;
    if ids.is_empty() && names.is_empty() {
        return Ok((HashMap::new(), HashMap::new()));
    }
    type Row = (
        Uuid,
        String,
        Option<DateTime<Utc>>,
        Option<String>,
        Option<String>,
    );
    let found: Vec<Row> = tool_script::Entity::find()
        .filter(tool_script::Column::DecommissionedAt.is_not_null())
        .filter(
            Condition::any()
                .add(tool_script::Column::Id.is_in(ids.iter().copied()))
                .add(tool_script::Column::Name.is_in(names.iter().cloned())),
        )
        .select_only()
        .column(tool_script::Column::Id)
        .column(tool_script::Column::Name)
        .column(tool_script::Column::DecommissionedAt)
        .column(tool_script::Column::DecommissionedBy)
        .column(tool_script::Column::DecommissionReason)
        .into_tuple()
        .all(db)
        .await?;
    let mut by_id = HashMap::new();
    let mut by_name = HashMap::new();
    for (id, name, at, by, reason) in found {
        if let Some(decommission) = decommission_of(at, by, reason) {
            by_id.insert(id, decommission.clone());
            by_name.insert(name, decommission);
        }
    }
    Ok((by_id, by_name))
}

/// A calculation formula as `(id, code, name, output_parameter_id, tool_script_id)`.
type FormulaOutputRow = (Uuid, String, String, Option<Uuid>, Option<Uuid>);

/// The calculation behind every derived row, keyed by the output parameter it writes, and the
/// versions the stored values name, keyed by their own ids. A row naming no version keeps the
/// definition and reports no formula, because the text that produced it is not recoverable (M134).
pub(super) type FormulaVersion = (i32, String, String);

pub(super) async fn fetch_calculations(
    db: &sea_orm::DatabaseConnection,
    rows: &[RawRow],
) -> AppResult<(
    HashMap<Uuid, CalculationInfo>,
    HashMap<Uuid, FormulaVersion>,
)> {
    let parameter_ids: Vec<Uuid> = rows
        .iter()
        .filter(|r| r.measurement_type.as_deref() == Some("derived"))
        .filter_map(|r| r.parameter_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    if parameter_ids.is_empty() {
        return Ok((HashMap::new(), HashMap::new()));
    }
    let definitions: Vec<FormulaOutputRow> = calculation_formulas::Entity::find()
        .filter(calculation_formulas::Column::OutputParameterId.is_in(parameter_ids))
        .select_only()
        .column(calculation_formulas::Column::Id)
        .column(calculation_formulas::Column::Code)
        .column(calculation_formulas::Column::Name)
        .column(calculation_formulas::Column::OutputParameterId)
        .column(calculation_formulas::Column::ToolScriptId)
        .into_tuple()
        .all(db)
        .await?;
    let mut by_parameter: HashMap<Uuid, CalculationInfo> = HashMap::new();
    let mut output_codes: HashMap<Uuid, String> = HashMap::new();
    for (id, code, name, output_parameter_id, tool_script_id) in definitions {
        let (Some(output), Some(tool_script_id)) = (output_parameter_id, tool_script_id) else {
            continue;
        };
        output_codes.insert(tool_script_id, code.clone());
        by_parameter.insert(
            output,
            CalculationInfo {
                definition_id: id,
                tool_script_id,
                code,
                name,
                version_id: None,
                version_no: None,
                formula: None,
                content_hash: None,
                active_version_no: None,
                decommissioned: None,
            },
        );
    }

    let active =
        active_version_numbers(db, &output_codes.keys().copied().collect::<Vec<_>>()).await?;
    for info in by_parameter.values_mut() {
        info.active_version_no = active.get(&info.tool_script_id).copied();
    }

    let named: Vec<Uuid> = rows
        .iter()
        .filter_map(|r| r.derived_version_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let versions = formula_versions_named(db, &named, &output_codes).await?;
    Ok((by_parameter, versions))
}

/// The newest version number of each calculation, so a value made by an older one reads as such.
async fn active_version_numbers(
    db: &sea_orm::DatabaseConnection,
    script_ids: &[Uuid],
) -> AppResult<HashMap<Uuid, i32>> {
    if script_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT s.id AS script_id, v.version_no \
               FROM tool_scripts s \
               JOIN tool_script_versions v ON v.id = s.active_version_id \
              WHERE s.id = ANY($1::uuid[])",
            [script_ids.to_vec().into()],
        ))
        .await?;
    let mut active = HashMap::new();
    for row in &rows {
        let script_id: Uuid = row.try_get("", "script_id")?;
        let version_no: i32 = row.try_get("", "version_no")?;
        active.insert(script_id, version_no);
    }
    Ok(active)
}

/// The versions the stored values name, each with the formula text that produced the output.
///
/// The text comes out of the version's own rendered set rather than the formula row as it stands,
/// which is the whole point of naming a version: the number was made by that text, whatever the
/// calculation says today.
async fn formula_versions_named(
    db: &sea_orm::DatabaseConnection,
    version_ids: &[Uuid],
    output_codes: &HashMap<Uuid, String>,
) -> AppResult<HashMap<Uuid, FormulaVersion>> {
    if version_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id, tool_script_id, version_no, script, content_hash \
               FROM tool_script_versions WHERE id = ANY($1::uuid[])",
            [version_ids.to_vec().into()],
        ))
        .await?;
    let mut versions: HashMap<Uuid, FormulaVersion> = HashMap::new();
    for row in &rows {
        let id: Uuid = row.try_get("", "id")?;
        let tool_script_id: Uuid = row.try_get("", "tool_script_id")?;
        let version_no: i32 = row.try_get("", "version_no")?;
        let script: String = row.try_get("", "script")?;
        let content_hash: String = row.try_get("", "content_hash")?;
        let Some(code) = output_codes.get(&tool_script_id) else {
            continue;
        };
        let formula = crate::routes::private::tools::service::parse_pinned(&script)
            .ok()
            .and_then(|set| {
                set.into_iter()
                    .find(|f| f.code.eq_ignore_ascii_case(code))
                    .map(|f| f.formula)
            });
        if let Some(formula) = formula {
            versions.insert(id, (version_no, formula, content_hash));
        }
    }
    Ok(versions)
}

/// A formula and what it reads, as the sources table records it today. Sources are not versioned,
/// so a value made by an older version is joined to the definition's current inputs.
#[derive(Debug, Clone)]
pub(super) struct FormulaLink {
    pub(super) definition_id: Uuid,
    pub(super) code: String,
    pub(super) name: String,
    pub(super) per_replicate: Option<String>,
    pub(super) output_parameter_id: Option<Uuid>,
    pub(super) output_parameter_code: Option<String>,
    pub(super) calculation: Option<String>,
    pub(super) enabled: bool,
    pub(super) sources: Vec<SourceLink>,
}

#[derive(Debug, Clone)]
pub(super) struct SourceLink {
    pub(super) variable_name: String,
    pub(super) parameter_id: Option<Uuid>,
    pub(super) parameter_code: Option<String>,
    pub(super) site_property: Option<String>,
}

/// The formulas one hop from the rows' parameters: those producing one, and those reading one.
#[derive(Debug, Default)]
pub(super) struct FormulaLinks {
    pub(super) formulas: Vec<FormulaLink>,
}

/// What a parameter's slot serves at one instant: each replicate's value, the family's mean where
/// the trigger derived one, and the value a scalar read of the slot receives.
#[derive(Debug, Default)]
pub(super) struct ServedSlot {
    pub(super) by_index: BTreeMap<i16, f64>,
    pub(super) live: Vec<InputCandidate>,
}

/// Served values keyed by `(site_id, parameter_id)`, plus the site rows' numeric columns.
#[derive(Debug, Default)]
pub(super) struct ServedValues {
    pub(super) slots: HashMap<(Uuid, Uuid), ServedSlot>,
    pub(super) site_properties: HashMap<Uuid, HashMap<String, f64>>,
}

impl ServedValues {
    pub(super) fn scalar(&self, site_id: Uuid, parameter_id: Uuid) -> (Option<f64>, &'static str) {
        match self
            .slots
            .get(&(site_id, parameter_id))
            .and_then(|slot| chosen_input(&slot.live))
        {
            Some(input) if input.from_mean => (Some(input.value), "mean"),
            Some(input) => (Some(input.value), "reading"),
            None => (None, "missing"),
        }
    }

    pub(super) fn at_index(
        &self,
        site_id: Uuid,
        parameter_id: Uuid,
        index: i16,
    ) -> (Option<f64>, &'static str) {
        match self
            .slots
            .get(&(site_id, parameter_id))
            .and_then(|slot| slot.by_index.get(&index).copied())
        {
            Some(value) => (Some(value), "replicate"),
            None => (None, "missing"),
        }
    }

    pub(super) fn site_property(&self, site_id: Uuid, column: &str) -> (Option<f64>, &'static str) {
        match self
            .site_properties
            .get(&site_id)
            .and_then(|row| row.get(column).copied())
        {
            Some(value) => (Some(value), "site"),
            None => (None, "missing"),
        }
    }
}

impl FormulaLinks {
    /// Every parameter a lookup at the instant has to serve: the sources of the producers and the
    /// outputs of the consumers.
    pub(super) fn parameters_to_serve(&self) -> Vec<Uuid> {
        self.formulas
            .iter()
            .flat_map(|f| {
                f.sources
                    .iter()
                    .filter_map(|s| s.parameter_id)
                    .chain(f.output_parameter_id)
            })
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    }

    pub(super) fn site_properties_to_serve(&self) -> Vec<String> {
        self.formulas
            .iter()
            .flat_map(|f| f.sources.iter().filter_map(|s| s.site_property.clone()))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    }

    /// The inputs of every formula producing `parameter_id`, read at the record's indexes.
    pub(super) fn inputs_of(
        &self,
        parameter_id: Uuid,
        indexes: &[i16],
        served: &ServedValues,
        site_id: Uuid,
    ) -> Vec<InputRef> {
        let mut out = Vec::new();
        for formula in self
            .formulas
            .iter()
            .filter(|f| f.output_parameter_id == Some(parameter_id))
        {
            for source in &formula.sources {
                let make = |replicate_index, (value, served_as): (Option<f64>, &str)| InputRef {
                    definition_id: formula.definition_id,
                    formula_code: formula.code.clone(),
                    variable_name: source.variable_name.clone(),
                    parameter_id: source.parameter_id,
                    parameter_code: source.parameter_code.clone(),
                    site_property: source.site_property.clone(),
                    replicate_index,
                    served_as: served_as.to_string(),
                    value,
                };
                match (source.parameter_id, &source.site_property) {
                    (Some(input), _)
                        if formula.per_replicate.as_deref() == Some(&source.variable_name) =>
                    {
                        for &index in indexes {
                            out.push(make(Some(index), served.at_index(site_id, input, index)));
                        }
                    }
                    (Some(input), _) => out.push(make(None, served.scalar(site_id, input))),
                    (None, Some(column)) => {
                        out.push(make(None, served.site_property(site_id, column)));
                    }
                    (None, None) => {}
                }
            }
        }
        out
    }

    /// Every enabled formula reading `parameter_id`, with its output at the record's indexes.
    pub(super) fn consumers_of(
        &self,
        parameter_id: Uuid,
        indexes: &[i16],
        served: &ServedValues,
        site_id: Uuid,
    ) -> Vec<ConsumerRef> {
        let mut out = Vec::new();
        for formula in self.formulas.iter().filter(|f| f.enabled) {
            for source in formula
                .sources
                .iter()
                .filter(|s| s.parameter_id == Some(parameter_id))
            {
                let make = |replicate_index, value| ConsumerRef {
                    definition_id: formula.definition_id,
                    formula_code: formula.code.clone(),
                    formula_name: formula.name.clone(),
                    calculation: formula.calculation.clone(),
                    variable_name: source.variable_name.clone(),
                    output_parameter_id: formula.output_parameter_id,
                    output_parameter_code: formula.output_parameter_code.clone(),
                    replicate_index,
                    value,
                };
                let per_replicate = formula.per_replicate.as_deref() == Some(&source.variable_name);
                match (formula.output_parameter_id, per_replicate) {
                    (Some(output), true) => {
                        for &index in indexes {
                            out.push(make(Some(index), served.at_index(site_id, output, index).0));
                        }
                    }
                    (Some(output), false) => out.push(make(None, served.scalar(site_id, output).0)),
                    (None, _) => out.push(make(None, None)),
                }
            }
        }
        out
    }
}

/// The formulas one hop from the rows' parameters, with their sources. A formula under a disabled
/// calculation is kept as a producer (the value it made is still its) and dropped as a consumer.
pub(super) async fn fetch_formula_links(
    db: &sea_orm::DatabaseConnection,
    rows: &[RawRow],
) -> AppResult<FormulaLinks> {
    let parameter_ids: Vec<Uuid> = rows
        .iter()
        .filter_map(|r| r.parameter_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    if parameter_ids.is_empty() {
        return Ok(FormulaLinks::default());
    }
    let found = db
        .query_all_raw(build(formulas_one_hop_query(&parameter_ids)))
        .await?;
    let mut formulas: Vec<FormulaLink> = Vec::with_capacity(found.len());
    for row in found.iter().map(|r| LinkRow::from_query_result(r, "")) {
        let row = row?;
        formulas.push(FormulaLink {
            definition_id: row.id,
            code: row.code,
            name: row.name,
            per_replicate: row.per_replicate,
            output_parameter_id: row.output_parameter_id,
            output_parameter_code: row.output_parameter_code,
            calculation: row.calculation,
            enabled: row.enabled,
            sources: Vec::new(),
        });
    }
    if formulas.is_empty() {
        return Ok(FormulaLinks::default());
    }
    let definition_ids: Vec<Uuid> = formulas.iter().map(|f| f.definition_id).collect();
    let sources = db
        .query_all_raw(build(formula_sources_query(&definition_ids)))
        .await?;
    let mut by_definition: HashMap<Uuid, Vec<SourceLink>> = HashMap::new();
    for row in sources.iter().map(|r| SourceRow::from_query_result(r, "")) {
        let row = row?;
        by_definition
            .entry(row.derived_definition_id)
            .or_default()
            .push(SourceLink {
                variable_name: row.variable_name,
                parameter_id: row.parameter_id,
                parameter_code: row.parameter_code,
                site_property: row.site_property,
            });
    }
    for formula in &mut formulas {
        formula.sources = by_definition
            .remove(&formula.definition_id)
            .unwrap_or_default();
    }
    Ok(FormulaLinks { formulas })
}

/// What the linked parameters serve at the instant, at every site the rows name. A scalar read
/// of a slot receives the family's mean where the trigger derived one, else the lowest live
/// replicate's value, which is the serving contract's spot arm.
pub(super) async fn fetch_served_values(
    db: &sea_orm::DatabaseConnection,
    rows: &[RawRow],
    links: &FormulaLinks,
    time: DateTime<Utc>,
) -> AppResult<ServedValues> {
    let mut served = ServedValues::default();
    let site_ids: Vec<Uuid> = rows
        .iter()
        .filter_map(|r| r.site_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let parameter_ids = links.parameters_to_serve();
    if site_ids.is_empty() || parameter_ids.is_empty() {
        return Ok(served);
    }
    let r = Alias::new("r");
    let sample = Alias::new("s");
    let (sql, values) = Query::select()
        .column((r.clone(), readings::Column::SiteId))
        .column((r.clone(), readings::Column::ParameterId))
        .column((r.clone(), readings::Column::ReplicateIndex))
        .expr_as(effective_value(Some("r")), Alias::new("value"))
        .expr_as(
            Expr::cust("r.is_flagged IS NOT TRUE AND r.withdrawn_at IS NULL"),
            Alias::new("live"),
        )
        .column((r.clone(), readings::Column::MeasurementType))
        .column((r.clone(), readings::Column::StreamId))
        .expr_as(
            Expr::cust(
                "COALESCE(CASE WHEN s.n > 0 THEN s.mean END, r.calibrated_value, r.raw_value)",
            ),
            Alias::new("input_value"),
        )
        .expr_as(
            Expr::cust("COALESCE(s.n > 0 AND s.mean IS NOT NULL, false)"),
            Alias::new("from_mean"),
        )
        .expr_as(
            crate::routes::private::tools::service::reading_revision_expr(),
            Alias::new("revision"),
        )
        .from_as(readings::Entity, r.clone())
        .join_as(
            JoinType::LeftJoin,
            samples::Entity,
            sample.clone(),
            Expr::col((sample, samples::Column::Id))
                .equals((r.clone(), readings::Column::SampleId)),
        )
        .cond_where(
            Condition::all()
                .add(Expr::col((r.clone(), readings::Column::SiteId)).is_in(site_ids.clone()))
                .add(Expr::col((r.clone(), readings::Column::ParameterId)).is_in(parameter_ids))
                .add(Expr::col((r.clone(), readings::Column::Time)).eq(time)),
        )
        .order_by((r.clone(), readings::Column::SiteId), Order::Asc)
        .order_by((r.clone(), readings::Column::ParameterId), Order::Asc)
        .order_by((r, readings::Column::ReplicateIndex), Order::Asc)
        .to_owned()
        .build(PostgresQueryBuilder);
    let found = ServedRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .all(db)
    .await?;
    for row in found {
        let slot = served
            .slots
            .entry((row.site_id, row.parameter_id))
            .or_default();
        slot.by_index.insert(row.replicate_index, row.value);
        if row.live {
            slot.live.push(InputCandidate {
                // This query is over one instant, so every row it returns stands at it.
                time,
                measurement_type: row.measurement_type,
                replicate_index: row.replicate_index,
                stream_id: row.stream_id,
                value: row.input_value,
                from_mean: row.from_mean,
                revision: row.revision,
            });
        }
    }

    let columns = links.site_properties_to_serve();
    if columns.is_empty() {
        return Ok(served);
    }
    let sites = db.query_all_raw(build(site_rows_query(&site_ids))).await?;
    for row in sites.iter().map(|r| SiteRow::from_query_result(r, "")) {
        let row = row?;
        let values: HashMap<String, f64> = columns
            .iter()
            .filter_map(|c| {
                row.row
                    .get(c)
                    .and_then(serde_json::Value::as_f64)
                    .map(|v| (c.clone(), v))
            })
            .collect();
        served.site_properties.insert(row.id, values);
    }
    Ok(served)
}

/// The slot's code, name, unit and declared precision. The site's own configuration wins over the
/// catalog default, which is what makes the number on screen readable in the unit it was served in.
pub(super) async fn slot_identity(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
) -> AppResult<Option<(String, String, Option<String>, Option<i16>)>> {
    let row = db
        .query_one_raw(build(slot_identity_query(site_id, parameter_id)))
        .await?;
    let Some(row) = row else { return Ok(None) };
    let row = SlotRow::from_query_result(&row, "")?;
    Ok(Some((row.code, row.name, row.units, row.decimal_places)))
}

/// How long a chunked upload's rows are kept before the janitor removes them. An upload that
/// stops part-way leaves rows nothing will ever read, and a commit that names a session older
/// than this is told to re-upload rather than given half a file.
pub const IMPORT_SESSION_RETENTION_MINUTES: i64 = 60;

/// The session a chunk names has to be one this caller opened, so a typo in a session id starts
/// no second file under it and a session id someone else learns reaches nothing. Another caller's
/// session reads as not found, which says nothing about whether it exists.
pub async fn require_open_session<C: ConnectionTrait>(
    db: &C,
    session_id: Uuid,
    opener: &str,
) -> AppResult<()> {
    let held = import_chunk::Entity::find()
        .filter(import_chunk::Column::SessionId.eq(session_id))
        .filter(import_chunk::Column::OpenedBy.eq(opener))
        .count(db)
        .await?;
    if held == 0 {
        return Err(AppError::BadRequest(
            "Staging session expired or not found, start the upload again".to_string(),
        ));
    }
    Ok(())
}

/// Append one chunk to a session and report what the session now holds. The sequence is the row
/// count, so the chunks reassemble in the order they arrived whichever replica took each one.
pub async fn append_chunk<C: ConnectionTrait>(
    db: &C,
    session_id: Uuid,
    opener: &str,
    chunk: &str,
) -> AppResult<usize> {
    let seq = import_chunk::Entity::find()
        .filter(import_chunk::Column::SessionId.eq(session_id))
        .select_only()
        .column_as(import_chunk::Column::Seq.max(), "seq")
        .into_tuple::<Option<i32>>()
        .one(db)
        .await?
        .flatten()
        .map_or(0, |highest| highest + 1);
    import_chunk::ActiveModel {
        session_id: Set(session_id),
        seq: Set(seq),
        chunk: Set(chunk.to_string()),
        opened_by: Set(opener.to_string()),
        ..Default::default()
    }
    .insert(db)
    .await?;
    staged_size(db, session_id).await
}

/// The bytes a session holds, counted in the database rather than by reassembling the file.
async fn staged_size<C: ConnectionTrait>(db: &C, session_id: Uuid) -> AppResult<usize> {
    let bytes = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT COALESCE(SUM(octet_length(chunk)), 0)::bigint AS n \
             FROM csv_import_chunks WHERE session_id = $1",
            [session_id.into()],
        ))
        .await?
        .ok_or_else(|| AppError::Internal("the chunk size query returned no row".to_string()))?;
    let n: i64 = bytes.try_get("", "n")?;
    Ok(usize::try_from(n).unwrap_or(0))
}

/// The file a session holds, its chunks in the order they arrived. Only its opener reads it.
pub async fn staged_text<C: ConnectionTrait>(
    db: &C,
    session_id: Uuid,
    opener: &str,
) -> AppResult<String> {
    let rows = import_chunk::Entity::find()
        .filter(import_chunk::Column::SessionId.eq(session_id))
        .filter(import_chunk::Column::OpenedBy.eq(opener))
        .order_by_asc(import_chunk::Column::Seq)
        .all(db)
        .await?;
    if rows.is_empty() {
        return Err(AppError::BadRequest(
            "Staging session expired or not found, re-upload the file".to_string(),
        ));
    }
    let mut text = String::with_capacity(rows.iter().map(|r| r.chunk.len()).sum());
    for row in rows {
        text.push_str(&row.chunk);
    }
    Ok(text)
}

/// Remove the chunks of every upload older than the retention, which is what stops an abandoned
/// one keeping its bytes forever. Returns the rows removed.
pub async fn prune_import_sessions<C: ConnectionTrait>(db: &C) -> AppResult<u64> {
    let cutoff = Utc::now() - chrono::Duration::minutes(IMPORT_SESSION_RETENTION_MINUTES);
    Ok(import_chunk::Entity::delete_many()
        .filter(import_chunk::Column::CreatedAt.lt(cutoff))
        .exec(db)
        .await?
        .rows_affected)
}

/// Keys per statement. A statement is one OR-chain, and each term carries `time = $n` equality, so
/// chunk exclusion prunes; the bound is on statement size, not on correctness.
pub(super) const KEYS_PER_STATEMENT: usize = 500;

/// Curation moves values already served, at instants a bounded query may hold cached anywhere, and
/// the rollups exclude what a flag hides, so the refresh is the write's own span and its failure is
/// the caller's. Nothing arrives here, so no slot is announced and no alarm is re-evaluated. A
/// derived value computed from a flagged input is one the flag has just contradicted, so the slots
/// the write named are recomputed over the same span.
pub(super) const CURATION_TAIL: Axes = Axes {
    cache: Cache::All,
    refresh: Refresh::Range { fatal: true },
    announce: false,
    reconcile_alarms: false,
    episodes: Episodes::None,
    recompute_derived: true,
    writer: crate::routes::private::collection_events::flows::Writer::Person,
};

/// What a recorded curation left behind, in the shape the shared tail reads. The keys carry the
/// slots, which the record does not: the tail needs them to recompute what the flagged rows fed.
pub(super) fn written(recorded: &Recorded, keys: &[ReadingKey]) -> Written {
    let slots: Vec<Slot> = keys
        .iter()
        .map(|k| Slot::paired(k.site_id, k.parameter_id))
        .collect();
    Written::new(recorded.rows)
        .over(recorded.span)
        .touching(recorded.touched_events.clone())
        .at(slots)
}

/// `SET` clause of the flag write, and the values it binds ahead of the keys.
pub(super) enum FlagWrite {
    Set(String),
    Clear,
}

impl FlagWrite {
    pub(super) fn kind(&self) -> Kind {
        match self {
            FlagWrite::Set(_) => Kind::Flag,
            FlagWrite::Clear => Kind::Unflag,
        }
    }

    pub(super) fn new_value(&self) -> serde_json::Value {
        match self {
            FlagWrite::Set(reason) => serde_json::json!({ "reason": reason }),
            FlagWrite::Clear => serde_json::json!({}),
        }
    }

    pub(super) fn reason(&self) -> Option<&str> {
        match self {
            FlagWrite::Set(reason) => Some(reason.as_str()),
            FlagWrite::Clear => None,
        }
    }

    /// Rows already in the requested state are not decided again.
    pub(super) fn state_predicate(&self) -> Expr {
        match self {
            FlagWrite::Set(_) => Expr::cust("r.is_flagged IS NOT TRUE"),
            FlagWrite::Clear => Expr::cust("r.is_flagged IS TRUE"),
        }
    }
}

/// Flag or unflag an explicit key set, then refresh the rollups over the buckets it changed.
///
/// Each key becomes a decision (ADR 0008); the record's trigger projects it onto the row. The
/// whole key set is one transaction with the decompression cap lifted: a partial flag set is a
/// state no reader can interpret, and any key may land in a chunk the compression policy has
/// reached. The refresh runs after the commit because `refresh_continuous_aggregate` is a
/// procedure with its own transaction control, and so does the reactive hook the decisions
/// reach.
pub(super) async fn apply_flags(
    state: &AppState,
    scope: &AccessScope,
    actor: &str,
    origin: Origin,
    keys: &[ReadingKey],
    write: FlagWrite,
) -> AppResult<u64> {
    if keys.is_empty() {
        return Err(AppError::BadRequest("No readings specified".to_string()));
    }
    let target_sites: Vec<Uuid> = keys.iter().map(|r| r.site_id).collect();
    enforce_project_scope_for_sites(&state.db, scope, &target_sites).await?;

    let set_id = Uuid::new_v4();
    let recorded = bulk_write::guarded(&state.db, async |txn| {
        let mut all = Recorded::default();
        for chunk in keys.chunks(KEYS_PER_STATEMENT) {
            let mut any = Condition::any();
            for key in chunk {
                let mut one = Condition::all()
                    .add(r(readings::Column::SiteId).eq(key.site_id))
                    .add(r(readings::Column::ParameterId).eq(key.parameter_id))
                    .add(r(readings::Column::Time).eq(key.time));
                // A named replicate is a spot row by definition; a named cadence matches the
                // column, and 'continuous' also matches the rows that declare nothing.
                if let Some(index) = key.replicate_index {
                    one = one
                        .add(r(readings::Column::ReplicateIndex).eq(index))
                        .add(r(readings::Column::MeasurementType).eq("spot"));
                }
                if let Some(cadence) = key.measurement_type.as_deref() {
                    let mut matches =
                        Condition::any().add(r(readings::Column::MeasurementType).eq(cadence));
                    if cadence == "continuous" {
                        matches = matches.add(r(readings::Column::MeasurementType).is_null());
                    }
                    one = one.add(matches);
                }
                any = any.add(one);
            }
            let rows = Condition::all().add(any).add(write.state_predicate());
            let recorded = record_many(
                txn,
                write.kind(),
                rows,
                NewValue::Literal(write.new_value()),
                actor,
                write.reason(),
                origin,
                Some(set_id),
            )
            .await?;
            all.rows += recorded.rows;
            all.span = match (all.span, recorded.span) {
                (Some((a, b)), Some((c, d))) => Some((Ord::min(a, c), Ord::max(b, d))),
                (x, None) => x,
                (None, y) => y,
            };
            all.touched_events.extend(recorded.touched_events);
        }
        Ok(all)
    })
    .await?;

    run(state, &written(&recorded, keys), &CURATION_TAIL, actor).await?;
    Ok(recorded.rows)
}

/// One slot over a closed time range.
pub(super) struct SlotRange {
    pub(super) site_id: Uuid,
    pub(super) parameter_id: Uuid,
    pub(super) start_time: DateTime<Utc>,
    pub(super) end_time: DateTime<Utc>,
}

impl SlotRange {
    pub(super) async fn admit(&self, state: &AppState, scope: &AccessScope) -> AppResult<()> {
        if self.end_time < self.start_time {
            return Err(AppError::BadRequest(
                "end_time must be >= start_time".to_string(),
            ));
        }
        enforce_project_scope_for_sites(&state.db, scope, &[self.site_id]).await
    }

    /// The rows the write selects: the slot, the range, and not already in the requested state.
    pub(super) fn rows(&self, write: &FlagWrite) -> Condition {
        Condition::all()
            .add(r(readings::Column::SiteId).eq(self.site_id))
            .add(r(readings::Column::ParameterId).eq(self.parameter_id))
            .add(r(readings::Column::Time).gte(self.start_time))
            .add(r(readings::Column::Time).lte(self.end_time))
            .add(write.state_predicate())
    }

    /// The slot the range covers, in the key shape the tail reads slots from.
    pub(super) fn key(&self) -> [ReadingKey; 1] {
        [ReadingKey {
            site_id: self.site_id,
            parameter_id: self.parameter_id,
            time: self.start_time,
            replicate_index: None,
            measurement_type: None,
        }]
    }
}

/// The count `apply_flags_over_range` would report, with nothing written.
pub(super) async fn count_flags_over_range(
    state: &AppState,
    scope: &AccessScope,
    range: SlotRange,
    write: &FlagWrite,
) -> AppResult<u64> {
    range.admit(state, scope).await?;
    let (sql, values) = Query::select()
        .expr_as(Expr::cust("COUNT(*)"), Alias::new("n"))
        .from_as(readings::Entity, Alias::new("r"))
        .cond_where(range.rows(write))
        .to_owned()
        .build(PostgresQueryBuilder);
    let row = state
        .db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?
        .ok_or_else(|| AppError::Internal("count returned no row".to_string()))?;
    let n: i64 = row.try_get("", "n")?;
    Ok(u64::try_from(n).unwrap_or(0))
}

pub(super) async fn apply_flags_over_range(
    state: &AppState,
    scope: &AccessScope,
    actor: &str,
    origin: Origin,
    range: SlotRange,
    write: &FlagWrite,
) -> AppResult<u64> {
    range.admit(state, scope).await?;
    let rows = range.rows(write);
    let recorded = bulk_write::guarded(&state.db, async |txn| {
        record_many(
            txn,
            write.kind(),
            rows.clone(),
            NewValue::Literal(write.new_value()),
            actor,
            write.reason(),
            origin,
            Some(Uuid::new_v4()),
        )
        .await
    })
    .await?;

    run(
        state,
        &written(&recorded, &range.key()),
        &CURATION_TAIL,
        actor,
    )
    .await?;
    Ok(recorded.rows)
}

/// One slot a write landed in, and the stream it arrived through. An unpaired stream names
/// neither site nor parameter: its rows are stored and announced, and there is no slot to
/// invalidate, reconcile or roll up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Slot {
    pub site_id: Option<Uuid>,
    pub parameter_id: Option<Uuid>,
    pub stream_id: Option<Uuid>,
}

impl Slot {
    #[must_use]
    pub fn paired(site_id: Uuid, parameter_id: Uuid) -> Self {
        Self {
            site_id: Some(site_id),
            parameter_id: Some(parameter_id),
            stream_id: None,
        }
    }

    #[must_use]
    pub fn through(mut self, stream_id: Uuid) -> Self {
        self.stream_id = Some(stream_id);
        self
    }

    pub(super) fn slot(self) -> Option<(Uuid, Uuid)> {
        self.site_id.zip(self.parameter_id)
    }
}

/// What a write left behind. `rows` is the tail's own gate: a pass that moved nothing runs no
/// step, whatever it counted on the way.
#[derive(Debug, Clone, Default)]
pub struct Written {
    pub rows: u64,
    /// The count carried on `DataIngested`, which is the rows a consumer would fetch rather than
    /// everything the pass touched.
    pub announced: u64,
    pub span: Option<(DateTime<Utc>, DateTime<Utc>)>,
    pub slots: Vec<Slot>,
    pub touched_events: Vec<TouchedEvent>,
}

impl Written {
    #[must_use]
    pub fn new(rows: u64) -> Self {
        Self {
            rows,
            announced: rows,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn announced(mut self, announced: u64) -> Self {
        self.announced = announced;
        self
    }

    #[must_use]
    pub fn over(mut self, span: Option<(DateTime<Utc>, DateTime<Utc>)>) -> Self {
        self.span = span;
        self
    }

    #[must_use]
    pub fn at(mut self, slots: Vec<Slot>) -> Self {
        self.slots = slots;
        self
    }

    #[must_use]
    pub fn touching(mut self, touched_events: Vec<TouchedEvent>) -> Self {
        self.touched_events = touched_events;
        self
    }

    pub(super) fn sites(&self) -> Vec<Uuid> {
        self.slots
            .iter()
            .filter_map(|s| s.site_id)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

/// How wide the cache invalidation goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cache {
    /// Every entry. For a write whose effect is not confined to the sites it names: sample
    /// formation and withdrawal rewrite served history for anyone reading it.
    All,
    /// The sites the write landed in.
    Sites,
}

/// The window the rollups are refreshed over, and whether a failure there is fatal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refresh {
    /// No refresh: the write cannot have moved a rollup (spot rows are excluded from them).
    Skip,
    /// The span the write covers. A request path swallows the error, since the rows are committed
    /// and a 500 would replay a write that happened.
    Range { fatal: bool },
    /// From the earliest instant the write touched up to now, for a path that also moved rows it
    /// did not report.
    Since { fatal: bool },
}

/// How alarm episodes are rebuilt over what was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Episodes {
    /// Rebuilt inline, per slot. For paths that fire per sync cycle or per field entry, where one
    /// `reprocessing_jobs` row per write would be the noise.
    Inline,
    /// Enqueued as one `alarm_backfill` job over every slot.
    Job,
    None,
}

/// The three axes the write paths encode, stated once each.
#[derive(Debug, Clone, Copy)]
pub struct Axes {
    pub cache: Cache,
    pub refresh: Refresh,
    /// Whether each written slot is announced on the event bus, which is both the SSE feed and
    /// what the cache invalidator subscribes to.
    pub announce: bool,
    /// Whether the open-alarm state of the written slots is reconciled now rather than waiting for
    /// the periodic sweep.
    pub reconcile_alarms: bool,
    pub episodes: Episodes,
    /// Whether the derived values the written slots feed are recomputed over the same span. A
    /// derived value is computed from what its inputs served, so a decision that changes what an
    /// input serves leaves it stating a number nothing supports any more.
    pub recompute_derived: bool,
    /// Who wrote, which is what decides whether the visit's calculations run again.
    pub writer: crate::routes::private::collection_events::flows::Writer,
}

/// Which steps run, over which window. Everything here is decided from [`Written`] and [`Axes`]
/// alone.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub refresh: Option<Window>,
    pub refresh_fatal: bool,
    pub invalidate_all: bool,
    pub invalidate_sites: Vec<Uuid>,
    pub recompute_events: Vec<Uuid>,
    pub announce: Vec<Slot>,
    pub reconcile: Vec<(Uuid, Uuid)>,
    pub episodes: Episodes,
    pub episode_span: Option<(DateTime<Utc>, DateTime<Utc>)>,
    /// The slots whose derived values are recomputed, over [`Plan::episode_span`]'s window.
    pub recompute_derived: Vec<(Uuid, Uuid)>,
}

impl Plan {
    /// The tail of a write that moved nothing.
    pub(super) fn nothing() -> Self {
        Self {
            refresh: None,
            refresh_fatal: false,
            invalidate_all: false,
            invalidate_sites: Vec::new(),
            recompute_events: Vec::new(),
            announce: Vec::new(),
            reconcile: Vec::new(),
            episodes: Episodes::None,
            episode_span: None,
            recompute_derived: Vec::new(),
        }
    }
}

/// What the tail does for this write, before it does any of it.
#[must_use]
pub fn plan(written: &Written, axes: &Axes) -> Plan {
    if written.rows == 0 {
        return Plan::nothing();
    }
    let span = written.span;
    let refresh = match (axes.refresh, span) {
        (Refresh::Skip, _) | (_, None) => None,
        (Refresh::Range { .. }, Some((lo, hi))) => Some(Window::Range(lo, hi)),
        (Refresh::Since { .. }, Some((lo, _))) => Some(Window::Since(lo)),
    };
    let refresh_fatal = match axes.refresh {
        Refresh::Skip => false,
        Refresh::Range { fatal } | Refresh::Since { fatal } => fatal,
    };
    let recompute_events =
        if axes.writer == crate::routes::private::collection_events::flows::Writer::Chain {
            Vec::new()
        } else {
            written.touched_events.iter().map(|e| e.id).collect()
        };
    let episodes = if span.is_some() {
        axes.episodes
    } else {
        Episodes::None
    };
    Plan {
        refresh,
        refresh_fatal,
        invalidate_all: axes.cache == Cache::All,
        invalidate_sites: if axes.cache == Cache::All {
            Vec::new()
        } else {
            written.sites()
        },
        recompute_events,
        announce: if axes.announce {
            written.slots.clone()
        } else {
            Vec::new()
        },
        reconcile: if axes.reconcile_alarms {
            written
                .slots
                .iter()
                .filter_map(|s| s.slot())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        } else {
            Vec::new()
        },
        episodes,
        episode_span: span,
        recompute_derived: if axes.recompute_derived && span.is_some() {
            written
                .slots
                .iter()
                .filter_map(|s| s.slot())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        } else {
            Vec::new()
        },
    }
}

/// What the tail writes to. A request path holds the whole `AppState`; a tracked job holds its
/// `JobContext`'s connection and event sender, and reaches the response cache only if the process
/// built one.
pub struct Sink<'a> {
    pub db: &'a DatabaseConnection,
    pub events: &'a EventSender,
    pub cache: Option<&'a ResponseCache>,
}

impl<'a> From<&'a AppState> for Sink<'a> {
    fn from(state: &'a AppState) -> Self {
        Self {
            db: &state.db,
            events: &state.events,
            cache: Some(&state.response_cache),
        }
    }
}

/// Run the tail. Call it after the guarded write has committed: an aggregate refresh is a
/// procedure with its own transaction control, and so are the jobs the reactive hook enqueues.
pub async fn run<'a>(
    sink: impl Into<Sink<'a>>,
    written: &Written,
    axes: &Axes,
    actor: &str,
) -> AppResult<Plan> {
    let sink = sink.into();
    let plan = plan(written, axes);

    if let Some(window) = plan.refresh {
        match aggregates::refresh(sink.db, window).await {
            Ok(_) => {}
            Err(e) if plan.refresh_fatal => return Err(e),
            Err(e) => tracing::warn!(error = %e, "aggregate refresh after a write failed"),
        }
    }

    if let Some(response_cache) = sink.cache {
        if plan.invalidate_all {
            cache::invalidate_all(response_cache, "write rewrote served history");
        }
        for site_id in &plan.invalidate_sites {
            cache::invalidate_site(response_cache, *site_id);
        }
    }

    if !plan.recompute_events.is_empty() {
        // The rows are committed, so a failure here is reported rather than returned: a 500 reads
        // as a failed save and the operator writes the same values again. A visit whose recompute
        // was never queued is raised by the event audit as a missing or stale output.
        if let Err(e) =
            flows::enqueue_for(sink.db, &written.touched_events, actor, axes.writer).await
        {
            tracing::warn!(error = %e, "recompute enqueue after a write failed");
        }
    }

    for slot in &plan.announce {
        let _ = sink.events.send(AppEvent::DataIngested {
            site_id: slot.site_id,
            parameter_id: slot.parameter_id,
            stream_id: slot.stream_id,
            count: usize::try_from(written.announced).unwrap_or(usize::MAX),
        });
    }

    if !plan.reconcile.is_empty() {
        crate::routes::private::alarms::flows::reconcile_and_notify(
            sink.db,
            sink.events,
            &plan.reconcile,
        )
        .await;
    }

    if let Some((lo, hi)) = plan.episode_span {
        match plan.episodes {
            Episodes::Inline => {
                for (site_id, parameter_id) in written.slots.iter().filter_map(|s| s.slot()) {
                    if let Err(e) = crate::routes::private::alarms::flows::evaluate_alarm_episodes(
                        sink.db,
                        site_id,
                        parameter_id,
                        lo,
                        hi,
                    )
                    .await
                    {
                        tracing::warn!(error = %e, %site_id, %parameter_id, "alarm episode reconstruction failed");
                    }
                }
            }
            Episodes::Job => {
                let slots: Vec<serde_json::Value> = written
                    .slots
                    .iter()
                    .filter_map(|s| s.slot())
                    .map(|(site_id, parameter_id)| serde_json::json!([site_id, parameter_id]))
                    .collect();
                crate::routes::private::reprocessing_jobs::service::enqueue(
                    sink.db,
                    "alarm_backfill",
                    None,
                    None,
                    &serde_json::json!({
                        "slots": slots,
                        "start": lo.to_rfc3339(),
                        "end": hi.to_rfc3339(),
                    }),
                    None,
                )
                .await?;
            }
            Episodes::None => {}
        }
    }

    if let (false, Some((lo, hi))) = (plan.recompute_derived.is_empty(), plan.episode_span) {
        let sites: BTreeSet<Uuid> = plan.recompute_derived.iter().map(|(s, _)| *s).collect();
        let parameters: BTreeSet<Uuid> = plan.recompute_derived.iter().map(|(_, p)| *p).collect();
        crate::routes::private::reprocessing_jobs::service::enqueue(
            sink.db,
            "derived_recompute",
            None,
            None,
            &serde_json::json!({
                "site_ids": sites.iter().map(ToString::to_string).collect::<Vec<_>>(),
                "parameter_ids": parameters.iter().map(ToString::to_string).collect::<Vec<_>>(),
                "start": lo.to_rfc3339(),
                "end": hi.to_rfc3339(),
            }),
            None,
        )
        .await?;
    }

    Ok(plan)
}

/// What a reading must satisfy to be stored, whichever path it arrived on.
///
/// The rules are here rather than in each handler because they were in each handler: the timestamp
/// bound existed on `/readings/batch` alone, no path rejected a non-finite value, and the
/// missing-value sentinel was recognised by its spelling in one branch of one importer.
pub mod admission {
    use chrono::{DateTime, Duration, Utc};
    use std::sync::LazyLock;

    use crate::error::{AppError, AppResult};
    use crate::routes::private::readings::service::measurement_type_rejection;

    /// Absolute, so an archive series does not age out of admissibility as the clock moves.
    const MIN_READING_TIME: &str = "2000-01-01T00:00:00Z";
    /// Logger clock skew. `last_data_time` only moves forward, so a far-future timestamp admitted
    /// here would latch a stream's cursor until wall-clock caught up.
    const MAX_LEAD_DAYS: i64 = 1;

    /// Missing-value marker the loggers and the portal exports write in place of a measurement.
    /// Compared numerically, so `-9999`, `-9999.0` and `-9999.00` are one marker.
    pub const MISSING_SENTINEL: f64 = -9999.0;
    const SENTINEL_TOLERANCE: f64 = 1e-9;

    /// The floor, parsed once. It is a compile-time literal, so parsing it per reading is pure
    /// overhead on a thousand-reading batch.
    static MIN_TIME: LazyLock<DateTime<Utc>> = LazyLock::new(|| {
        DateTime::parse_from_rfc3339(MIN_READING_TIME)
            .expect("MIN_READING_TIME is a literal RFC 3339 timestamp")
            .with_timezone(&Utc)
    });

    /// The window a reading's timestamp must fall in. Only the upper bound moves with `now`.
    pub fn window(now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
        (*MIN_TIME, now + Duration::days(MAX_LEAD_DAYS))
    }

    /// Why this timestamp is not admissible against a caller-supplied `now`, or `None` when it is.
    ///
    /// The `_at` variants exist so a caller filtering a whole batch reads the clock once and every
    /// reading in that batch is judged against the same window: two readings a microsecond apart
    /// cannot land on opposite sides of a moving bound.
    pub fn time_rejection_at(now: DateTime<Utc>, time: DateTime<Utc>) -> Option<String> {
        let (min_time, max_time) = window(now);
        if time >= min_time && time <= max_time {
            return None;
        }
        Some(format!(
            "Reading timestamp {} is outside valid range ({} to {})",
            time.to_rfc3339(),
            min_time.to_rfc3339(),
            max_time.to_rfc3339(),
        ))
    }

    /// Why this value is not admissible, or `None` when it is. `NaN` and the infinities have no
    /// meaning as a measurement and blank every aggregate bucket they reach.
    pub fn value_rejection(raw_value: f64) -> Option<String> {
        if raw_value.is_finite() {
            return None;
        }
        Some(format!("{raw_value} is not a finite number"))
    }

    /// Why this replicate index is not admissible for this cadence, or `None` when it is.
    ///
    /// Only a spot instant has replicates. Every continuous and derived reader filters
    /// `replicate_index = 0`, and so do the four continuous aggregates, so a non-zero index on a
    /// non-spot row is stored and served nowhere.
    pub fn replicate_index_rejection(
        measurement_type: Option<&str>,
        replicate_index: i16,
    ) -> Option<String> {
        if replicate_index == 0
            || measurement_type == Some(crate::routes::private::readings::service::SPOT)
        {
            return None;
        }
        Some(format!(
            "Replicate index {replicate_index} is only valid on a spot reading; {} readings are served at index 0 alone",
            measurement_type
                .unwrap_or(river_data_core::models::MeasurementType::Continuous.as_str())
        ))
    }

    pub fn admit_replicate_index(
        measurement_type: Option<&str>,
        replicate_index: i16,
    ) -> AppResult<()> {
        replicate_index_rejection(measurement_type, replicate_index)
            .map_or(Ok(()), |reason| Err(AppError::BadRequest(reason)))
    }

    pub fn admit_value(raw_value: f64) -> AppResult<()> {
        value_rejection(raw_value).map_or(Ok(()), |reason| {
            Err(AppError::BadRequest(format!("Reading value {reason}")))
        })
    }

    /// Why this reading is not admissible, or `None` when it is. The rules [`admit`] enforces,
    /// reported as a value for the callers that skip the reading rather than refuse the request.
    pub fn rejection(
        time: DateTime<Utc>,
        raw_value: f64,
        measurement_type: Option<&str>,
    ) -> Option<String> {
        rejection_at(Utc::now(), time, raw_value, measurement_type)
    }

    /// [`rejection`] against a caller-supplied `now`.
    pub fn rejection_at(
        now: DateTime<Utc>,
        time: DateTime<Utc>,
        raw_value: f64,
        measurement_type: Option<&str>,
    ) -> Option<String> {
        measurement_type_rejection(measurement_type)
            .or_else(|| time_rejection_at(now, time))
            .or_else(|| value_rejection(raw_value).map(|reason| format!("Reading value {reason}")))
    }

    /// Why a reading cannot be stored, as a closed set. The messages [`rejection`] returns carry
    /// the offending value and so cannot be grouped; a caller summarising a batch wants the kind.
    ///
    /// `UnknownCalibration` needs the calibration rows, so [`rejection_kind`] cannot decide it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum RejectionKind {
        OutOfWindow,
        NonFinite,
        UnknownMeasurementType,
        UnknownCalibration,
        /// Two payload rows at one (time, replicate_index) key under a completeness window. The
        /// last occurrence wins (the backends emit source-id order, so last is deterministic);
        /// the losers are counted here so the receipt arithmetic still closes.
        DuplicateKey,
        /// A non-zero `replicate_index` on a reading whose resolved cadence is not spot. Only a
        /// spot instant has replicates; every continuous and derived reader, and all four
        /// continuous aggregates, filter `replicate_index = 0`.
        ReplicateIndexOnNonSpot,
    }

    impl RejectionKind {
        pub const fn as_str(self) -> &'static str {
            match self {
                Self::OutOfWindow => "timestamp outside the admissible window",
                Self::NonFinite => "value is not a finite number",
                Self::UnknownMeasurementType => "measurement_type outside the vocabulary",
                Self::UnknownCalibration => "calibration_id names no calibration",
                Self::DuplicateKey => "duplicate (time, replicate_index) key in one payload",
                Self::ReplicateIndexOnNonSpot => "replicate_index is only valid on a spot reading",
            }
        }
    }

    /// Which kind of rejection applies, or `None` when the reading is admissible. Same order of
    /// precedence as [`rejection`].
    pub fn rejection_kind(
        time: DateTime<Utc>,
        raw_value: f64,
        measurement_type: Option<&str>,
    ) -> Option<RejectionKind> {
        rejection_kind_at(Utc::now(), time, raw_value, measurement_type)
    }

    /// [`rejection_kind`] against a caller-supplied `now`.
    pub fn rejection_kind_at(
        now: DateTime<Utc>,
        time: DateTime<Utc>,
        raw_value: f64,
        measurement_type: Option<&str>,
    ) -> Option<RejectionKind> {
        if measurement_type_rejection(measurement_type).is_some() {
            Some(RejectionKind::UnknownMeasurementType)
        } else if time_rejection_at(now, time).is_some() {
            Some(RejectionKind::OutOfWindow)
        } else if value_rejection(raw_value).is_some() {
            Some(RejectionKind::NonFinite)
        } else {
            None
        }
    }

    /// The full admission check for one reading: classification vocabulary, timestamp bound,
    /// finite value.
    pub fn admit(
        time: DateTime<Utc>,
        raw_value: f64,
        measurement_type: Option<&str>,
    ) -> AppResult<()> {
        rejection(time, raw_value, measurement_type)
            .map_or(Ok(()), |reason| Err(AppError::BadRequest(reason)))
    }

    pub fn is_missing_sentinel(value: f64) -> bool {
        (value - MISSING_SENTINEL).abs() <= SENTINEL_TOLERANCE
    }

    /// What one delimited cell resolves to.
    #[derive(Debug, PartialEq)]
    pub enum Cell {
        Value(f64),
        /// Declared missing: empty, `NaN`/`NA`, or the sentinel. Contributes no reading and is not
        /// a row error.
        Missing,
        /// Unusable: unparseable, or parseable but not finite.
        Invalid(String),
    }

    /// Classify a cell by value, not by spelling. Declared missing markers are recognised before
    /// parsing (`NaN` is a marker, not a number), and the sentinel after it, so every spelling of
    /// `-9999` lands in the same branch.
    pub fn classify_cell(cell: &str) -> Cell {
        let cell = cell.trim();
        if cell.is_empty() || cell.eq_ignore_ascii_case("nan") || cell.eq_ignore_ascii_case("na") {
            return Cell::Missing;
        }
        let Ok(value) = cell.parse::<f64>() else {
            return Cell::Invalid(format!("'{cell}' is not a number"));
        };
        if value_rejection(value).is_some() {
            return Cell::Invalid(format!("'{cell}' is not a finite number"));
        }
        if is_missing_sentinel(value) {
            return Cell::Missing;
        }
        Cell::Value(value)
    }

    #[cfg(test)]
    #[path = "tests/admission.rs"]
    mod tests;
}

/// What one reading claims about the standard curve that corrected it, as its writer resolved it.
#[derive(Debug, Clone, Copy)]
pub struct CurveClaim<'a> {
    pub standard_curve_id: Uuid,
    /// The instrument the reading is attributed to, after slot-owner resolution.
    pub sensor_id: Option<Uuid>,
    /// The reading's classification, after the resolution chain.
    pub measurement_type: &'a str,
}

/// Classification a hand-picked curve belongs to: a curve is fitted for one measurement, and a
/// logger cadence has no such measurement to pick it for.
pub(super) const CURVE_MEASUREMENT_TYPE: &str =
    river_data_core::models::MeasurementType::Spot.as_str();

/// The standard curves a request names, refused unless every reading naming one may carry it.
///
/// A curve is fitted on one instrument and chosen by hand for one measurement, so a reading may
/// name it only when the reading is that instrument's own spot measurement. Four claims are
/// refused: an id no curve carries, a curve fitted on a different instrument, a reading that names
/// no instrument at all, and a reading classified as anything but spot. The reference is not
/// decoration: it freezes the curve against edits and deletion, and it is what a served value
/// claims to have been corrected by.
///
/// Returns the curves so the caller computes the corrected value from the coefficients. A submitted
/// `calibrated_value` cannot be checked against a curve, only recomputed from it, so no path trusts
/// one alongside a curve reference.
pub async fn admit_standard_curves(
    db: &DatabaseConnection,
    claims: &[CurveClaim<'_>],
) -> AppResult<HashMap<Uuid, standard_curves::Model>> {
    if claims.is_empty() {
        return Ok(HashMap::new());
    }

    let ids: Vec<Uuid> = claims.iter().map(|c| c.standard_curve_id).collect();
    let curves: HashMap<Uuid, standard_curves::Model> = standard_curves::Entity::find()
        .filter(standard_curves::Column::Id.is_in(ids))
        .all(db)
        .await?
        .into_iter()
        .map(|c| (c.id, c))
        .collect();

    for claim in claims {
        let id = claim.standard_curve_id;
        let Some(curve) = curves.get(&id) else {
            return Err(AppError::BadRequest(format!(
                "Standard curve {id} not found"
            )));
        };
        match claim.sensor_id {
            Some(sensor_id) if sensor_id == curve.sensor_id => {}
            Some(sensor_id) => {
                return Err(AppError::BadRequest(format!(
                    "Standard curve {id} was fitted on instrument {}, not on {sensor_id}",
                    curve.sensor_id
                )));
            }
            None => {
                return Err(AppError::BadRequest(format!(
                    "Standard curve {id} was fitted on instrument {}, which this reading does not \
                     name",
                    curve.sensor_id
                )));
            }
        }
        if claim.measurement_type != CURVE_MEASUREMENT_TYPE {
            return Err(AppError::BadRequest(format!(
                "Standard curve {id} corrects a {CURVE_MEASUREMENT_TYPE} measurement, and this \
                 reading is classified '{}'",
                claim.measurement_type
            )));
        }
    }

    Ok(curves)
}

/// What an upsert may replace on the row it collides with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replace {
    /// Keep the stored row, drop the incoming one.
    Nothing,
    /// The measurement and its classification, ie. an operator or source correction.
    Values,
    /// The measurement plus the row's attribution, ie. a sync re-send that also re-resolves which
    /// site, parameter, sensor, calibration and deployment the row belongs to. The standard curve
    /// is never among them: it is chosen by hand per measurement and no query can recover it, so a
    /// re-send that names none leaves the stored one standing.
    ValuesAndAttribution,
    /// A replacing grab save: everything the entry states, rewritten in place on a live,
    /// unflagged row, and on a curved one only when the save names a curve of its own (`curved`).
    /// A curated row is left as it stands. What no entry sets (the verification state, a flag, the
    /// withdrawal) is carried.
    Entry { curved: bool },
}

impl From<ConflictMode> for Replace {
    fn from(mode: ConflictMode) -> Self {
        match mode {
            ConflictMode::Skip => Replace::Nothing,
            ConflictMode::Overwrite => Replace::Values,
        }
    }
}

/// Build the `ON CONFLICT` clause for the readings PK, shared by every path that upserts a
/// reading.
///
/// Operator state is never replaced: `is_flagged` and `flag_reason` are left out entirely, and the
/// sample link is written as `COALESCE(EXCLUDED.sample_id, readings.sample_id)` so a correction
/// that carries only a value keeps the reading inside its sample. Writing the incoming NULL there
/// would fire the samples refresh trigger with the replicate removed, and the refresh deletes a
/// `samples` row nothing references any more, taking its label, notes and created_by with it.
pub(crate) fn readings_upsert(replace: Replace) -> sea_orm::sea_query::OnConflict {
    let mut clause = sea_orm::sea_query::OnConflict::columns([
        readings::Column::StreamId,
        readings::Column::Time,
        readings::Column::ReplicateIndex,
    ]);
    match replace {
        Replace::Nothing => {
            clause.do_nothing();
        }
        Replace::Entry { curved } => {
            clause.update_columns([
                readings::Column::RawValue,
                readings::Column::CalibratedValue,
                readings::Column::MeasurementType,
                readings::Column::SiteId,
                readings::Column::ParameterId,
                readings::Column::SensorId,
                readings::Column::CalibrationId,
                readings::Column::DeploymentId,
                readings::Column::StandardCurveId,
                readings::Column::Label,
                readings::Column::Notes,
                readings::Column::CreatedBy,
                readings::Column::Provenance,
                readings::Column::ProvenanceKind,
            ]);
            clause.value(
                readings::Column::SampleId,
                sea_orm::sea_query::Expr::cust(
                    r#"COALESCE(EXCLUDED.sample_id, "readings"."sample_id")"#,
                ),
            );
            clause.value(
                readings::Column::IngestedAt,
                sea_orm::sea_query::Expr::cust(
                    r#"CASE WHEN "readings"."raw_value" IS DISTINCT FROM EXCLUDED.raw_value
                        OR "readings"."calibrated_value" IS DISTINCT FROM EXCLUDED.calibrated_value
                        THEN NOW() ELSE "readings"."ingested_at" END"#,
                ),
            );
            let live = sea_orm::sea_query::Expr::cust(
                r#""readings"."is_flagged" IS NOT TRUE AND "readings"."withdrawn_at" IS NULL"#,
            );
            clause.action_and_where(if curved {
                live
            } else {
                live.and(sea_orm::sea_query::Expr::cust(
                    r#""readings"."standard_curve_id" IS NULL"#,
                ))
            });
        }
        Replace::Values | Replace::ValuesAndAttribution => {
            clause.update_columns([
                readings::Column::RawValue,
                readings::Column::CalibratedValue,
                readings::Column::MeasurementType,
            ]);
            if replace == Replace::ValuesAndAttribution {
                clause.update_columns([
                    readings::Column::SiteId,
                    readings::Column::ParameterId,
                    readings::Column::SensorId,
                    readings::Column::CalibrationId,
                    readings::Column::DeploymentId,
                ]);
                // A hand-picked curve outlives any correction that does not name one of its own.
                clause.value(
                    readings::Column::StandardCurveId,
                    sea_orm::sea_query::Expr::cust(
                        r#"COALESCE(EXCLUDED.standard_curve_id, "readings"."standard_curve_id")"#,
                    ),
                );
            }
            clause.value(
                readings::Column::SampleId,
                sea_orm::sea_query::Expr::cust(
                    r#"COALESCE(EXCLUDED.sample_id, "readings"."sample_id")"#,
                ),
            );
            // Arrival time follows the value: an overwrite that changes nothing keeps the
            // original stamp, so a full-content re-assert pass does not claim every row
            // arrived today.
            clause.value(
                readings::Column::IngestedAt,
                sea_orm::sea_query::Expr::cust(
                    r#"CASE WHEN "readings"."raw_value" IS DISTINCT FROM EXCLUDED.raw_value
                        OR "readings"."calibrated_value" IS DISTINCT FROM EXCLUDED.calibrated_value
                        THEN NOW() ELSE "readings"."ingested_at" END"#,
                ),
            );
        }
    }
    clause.to_owned()
}

/// The upsert clause for a request-level `conflict` mode.
pub(crate) fn readings_on_conflict(mode: ConflictMode) -> sea_orm::sea_query::OnConflict {
    readings_upsert(mode.into())
}

/// Count how many of the chunk's (stream_id, time, replicate_index) keys already exist, so the
/// caller can split `rows_affected` into inserts vs overwrites in `overwrite` mode.
pub(super) async fn count_existing<C: ConnectionTrait>(
    db: &C,
    chunk: &[readings::ActiveModel],
) -> AppResult<usize> {
    use sea_orm::{ColumnTrait, Condition, QueryFilter, QuerySelect, sea_query::Expr};

    if chunk.is_empty() {
        return Ok(0);
    }

    let mut condition = Condition::any();
    for m in chunk {
        let (
            sea_orm::ActiveValue::Set(stream_id),
            sea_orm::ActiveValue::Set(time),
            sea_orm::ActiveValue::Set(rep),
        ) = (
            m.stream_id.clone(),
            m.time.clone(),
            m.replicate_index.clone(),
        )
        else {
            continue;
        };
        condition = condition.add(
            Condition::all()
                .add(readings::Column::StreamId.eq(stream_id))
                .add(readings::Column::Time.eq(time))
                .add(readings::Column::ReplicateIndex.eq(rep)),
        );
    }

    let count = readings::Entity::find()
        .select_only()
        .column_as(Expr::col(readings::Column::StreamId).count(), "n")
        .filter(condition)
        .into_tuple::<i64>()
        .one(db)
        .await?
        .unwrap_or(0);

    Ok(usize::try_from(count).unwrap_or(0))
}

/// Batched insert. With `overwrite`, conflicting rows are updated in place on the value
/// and attribution columns; operator state (is_flagged, flag_reason, sample_id) is never
/// touched so a correction cannot clear a flag or unlink a sample. Without it the conflict is
/// `Replace::Nothing`, so a resync of the same rows is a no-op: `replicate_index` is the source's
/// column position and nothing renumbers it, so a replayed reading carries the same primary key.
pub(super) async fn insert_reading_chunks<C: ConnectionTrait>(
    conn: &C,
    models: &[readings::ActiveModel],
    replace: Replace,
) -> Result<usize, AppError> {
    let overwrite = replace != Replace::Nothing;
    let conflict = readings_upsert(replace);

    let mut inserted = 0usize;
    for chunk in models.chunks(BATCH_SIZE) {
        match readings::Entity::insert_many(chunk.to_vec())
            .on_conflict(conflict.clone())
            .exec_without_returning(conn)
            .await
        {
            Ok(rows) => inserted += rows as usize,
            Err(e) => {
                let msg = e.to_string();
                if !overwrite && msg.contains("None of the records") {
                    // All duplicates in this chunk
                } else {
                    tracing::warn!(error = %e, batch_size = chunk.len(), "Failed to insert reading batch");
                    return Err(AppError::Database(e));
                }
            }
        }
    }
    Ok(inserted)
}

/// One review-queue row per instant whose standard curve claims were stripped. `expected` carries
/// the claims as the source made them, `computed` what was stored instead.
///
/// A stripped claim explains a statistics disagreement at the same instant rather than sitting
/// beside it, so raising this hold supersedes a live `replicate_stats` hold there. The upsert key
/// carries `kind`, so the precedence is stated here rather than falling out of the key.
pub(super) async fn upsert_curve_claim_hold<C: ConnectionTrait>(
    conn: &C,
    stream_id: Uuid,
    group_time: chrono::DateTime<Utc>,
    claims: &[serde_json::Value],
    status: HoldStatus,
) -> AppResult<()> {
    audit::upsert_hold(
        conn,
        &audit::Hold {
            key: audit::HoldKey::Stream {
                stream_id,
                group_time,
            },
            kind: HoldKind::CurveClaimStripped,
            expected: serde_json::json!({ "claims": claims }),
            computed: serde_json::json!({ "stored_without_curve": claims.len() }),
            delta: serde_json::json!({}),
            status,
            tool: None,
        },
    )
    .await?;
    crate::routes::private::sync::hold_model::Entity::update_many()
        .col_expr(
            crate::routes::private::sync::hold_model::Column::Status,
            Expr::val(HoldStatus::Superseded.as_str()),
        )
        .filter(crate::routes::private::sync::hold_model::Column::StreamId.eq(stream_id))
        .filter(crate::routes::private::sync::hold_model::Column::GroupTime.eq(group_time))
        .filter(
            crate::routes::private::sync::hold_model::Column::Kind
                .eq(HoldKind::ReplicateStats.as_str()),
        )
        .filter(
            crate::routes::private::sync::hold_model::Column::Status
                .is_in(crate::routes::private::sync::service::open_statuses()),
        )
        .exec(conn)
        .await?;
    Ok(())
}

/// Add one rejection to the per-kind tally.
pub(super) fn record_rejection(
    counts: &mut Vec<(admission::RejectionKind, usize)>,
    kind: admission::RejectionKind,
) {
    match counts.iter_mut().find(|(seen, _)| *seen == kind) {
        Some((_, n)) => *n += 1,
        None => counts.push((kind, 1)),
    }
}

/// The response, with the skipped tally logged on the way out. Every return path builds it here so
/// a dropped reading is reported to the caller and to the operator by the same code.
pub(super) fn ingest_outcome(
    stream_id: Uuid,
    paired: bool,
    submitted: usize,
    inserted: usize,
    counts: &[(admission::RejectionKind, usize)],
) -> IngestResponse {
    let skipped: usize = counts.iter().map(|(_, n)| n).sum();
    let skipped_reasons: Vec<String> = counts
        .iter()
        .map(|(kind, n)| format!("{} ({n})", kind.as_str()))
        .collect();
    if skipped > 0 {
        tracing::warn!(
            %stream_id,
            skipped,
            submitted,
            reasons = ?skipped_reasons,
            "Skipped inadmissible readings"
        );
    }
    IngestResponse {
        inserted,
        skipped,
        skipped_reasons,
        changed: 0,
        proposed: 0,
        withdrawn: 0,
        unchanged: 0,
        retained: 0,
        accepted_window: None,
        stream_id,
        paired,
    }
}

/// The (site_id, parameter_id) a stream's pairing resolves to. Both are `None` when the stream is
/// unpaired, ie. its readings land unattributed and stay out of the rollups until it is paired.
pub(super) async fn resolve_stream_slot<C: ConnectionTrait>(
    db: &C,
    site_parameter_id: Option<Uuid>,
) -> AppResult<(Option<Uuid>, Option<Uuid>)> {
    let Some(sp_id) = site_parameter_id else {
        return Ok((None, None));
    };
    let Some(slot) = site_parameters::Entity::find_by_id(sp_id).one(db).await? else {
        return Ok((None, None));
    };
    Ok((Some(slot.site_id), Some(slot.parameter_id)))
}

/// Project-scope check for stream-based ingest. A scoped token may only write to a stream paired
/// to a site within its project; an unpaired stream (no resolved site) is rejected outright so a
/// scoped key cannot create unattributed, project-less data.
pub(super) async fn enforce_ingest_scope(
    db: &sea_orm::DatabaseConnection,
    scope: &crate::common::authz::AccessScope,
    site_id: Option<Uuid>,
) -> AppResult<()> {
    if !scope.is_restricted() {
        return Ok(());
    }
    match site_id {
        Some(sid) => enforce_project_scope_for_sites(db, scope, &[sid]).await?,
        None => {
            return Err(AppError::Forbidden(
                "Project-scoped token cannot ingest into an unpaired stream".to_string(),
            ));
        }
    }
    Ok(())
}

pub(super) async fn run_replicate_audit(
    txn: &sea_orm::DatabaseTransaction,
    stream_id: Uuid,
    audits: &[crate::routes::private::sync::models::GroupAudit],
    paired: bool,
) -> AppResult<()> {
    use crate::routes::private::sync::service as audit;

    let audit_times: Vec<chrono::DateTime<Utc>> = audits.iter().map(|a| a.time).collect();
    let (Some(lo), Some(hi)) = (
        audit_times.iter().min().copied(),
        audit_times.iter().max().copied(),
    ) else {
        return Ok(());
    };
    let audited_instants: std::collections::HashSet<chrono::DateTime<Utc>> =
        audit_times.iter().copied().collect();
    let rows = readings::Entity::find()
        .select_only()
        .column(readings::Column::Time)
        .column(readings::Column::ReplicateIndex)
        .column_as(effective_value(None), "value")
        .filter(readings::Column::StreamId.eq(stream_id))
        .filter(readings::Column::Time.gte(lo))
        .filter(readings::Column::Time.lte(hi))
        .filter(readings::Column::WithdrawnAt.is_null())
        .order_by_asc(readings::Column::Time)
        .order_by_asc(readings::Column::ReplicateIndex)
        .into_model::<StoredReplicate>()
        .all(txn)
        .await?;
    let mut group_values: HashMap<chrono::DateTime<Utc>, Vec<audit::ReplicateValue>> =
        HashMap::new();
    for stored in rows {
        let time = stored.time;
        if !audited_instants.contains(&time.with_timezone(&Utc)) {
            continue;
        }
        let (index, value) = (stored.replicate_index, stored.value);
        group_values
            .entry(time.with_timezone(&Utc))
            .or_default()
            .push(audit::ReplicateValue { index, value });
    }

    let holds_by_time: HashMap<chrono::DateTime<Utc>, audit::LatestHold> =
        audit::latest_holds(txn, stream_id, &audit_times)
            .await?
            .into_iter()
            .map(|h| (h.time, h))
            .collect();

    for a in audits {
        let values = group_values
            .get(&a.time)
            .map_or(&[] as &[audit::ReplicateValue], Vec::as_slice);
        let numbers: Vec<f64> = values.iter().map(|v| v.value).collect();
        let stats = audit::group_stats(&numbers);
        let agree = audit::agrees(a, &stats);
        let mismatch = audit::GroupMismatch {
            time: a.time,
            expected_mean: a.expected_mean,
            expected_sd: a.expected_sd,
            expected_n: a.expected_n,
            computed_mean: stats.mean,
            computed_sd: stats.sd,
            n: stats.n,
            values: values.to_vec(),
        };
        let hold_status = audit::status_for(paired);
        match (agree, holds_by_time.get(&a.time)) {
            (true, Some(hold)) if matches!(hold.status.as_str(), "pending" | "deferred") => {
                audit::close_hold(txn, hold.id, HoldStatus::Superseded).await?;
            }
            (true, _) => {}
            // The operator's decision stands against re-detection of the SAME disagreement. A
            // cycle whose expected statistics moved is new evidence the decision never covered,
            // so it opens a fresh hold beside the terminal one.
            (false, Some(hold))
                if matches!(hold.status.as_str(), "acknowledged" | "remediated") =>
            {
                if audit::expected_changed(&hold.expected, a) {
                    audit::upsert_stats_hold(txn, stream_id, &mismatch, hold_status).await?;
                }
            }
            (false, _) => {
                audit::upsert_stats_hold(txn, stream_id, &mismatch, hold_status).await?;
            }
        }
    }
    Ok(())
}

// --- The ingest pass, one step at a time ---

/// The first and last instant a pass covers.
pub(super) type IngestSpan = (DateTime<Utc>, DateTime<Utc>);

/// Standard curve claims stripped from a pass, by instant, each as the source made it.
pub(super) type StrippedClaims = HashMap<DateTime<Utc>, Vec<serde_json::Value>>;

/// The fields only a sync service may send, refused from any other caller, and a completeness
/// window that does not open before it closes.
pub(super) fn refuse_sync_only_claims(
    payload: &IngestReadingsRequest,
    is_sync_service: bool,
) -> AppResult<()> {
    let sync_only = [
        (
            payload.overwrite,
            "overwrite is restricted to sync services",
        ),
        (
            payload.collection,
            "collection is restricted to sync services; grab entry goes through /grab_samples",
        ),
        (
            payload.audit.is_some(),
            "audit is restricted to sync services",
        ),
        (
            payload.window.is_some(),
            "window is restricted to sync services",
        ),
    ];
    if !is_sync_service && let Some((_, refusal)) = sync_only.iter().find(|(sent, _)| *sent) {
        return Err(AppError::Forbidden((*refusal).to_string()));
    }
    if let Some(window) = &payload.window
        && window.from >= window.to
    {
        return Err(AppError::BadRequest(
            "window.from must be before window.to".to_string(),
        ));
    }
    Ok(())
}

/// A pass with no readings and no completeness claim has nothing to do. With a window, an empty
/// payload is a claim the source holds nothing there, which the diff judges against the store.
pub(super) fn is_nothing_to_ingest(payload: &IngestReadingsRequest) -> bool {
    payload.readings.is_empty() && payload.window.is_none()
}

/// The stream a pass writes to, share-locked in the pass's own transaction until it commits: a
/// pairing that commits first is what the pass attributes from, and one arriving later waits and
/// then backfills what the pass wrote.
pub(super) async fn lock_ingest_stream(
    txn: &sea_orm::DatabaseTransaction,
    stream_id: Uuid,
) -> AppResult<data_streams::models::Model> {
    data_streams::models::Entity::find_by_id(stream_id)
        .lock_shared()
        .one(txn)
        .await?
        .ok_or_else(|| AppError::NotFound("Stream not found".to_string()))
}

/// A completeness window is accepted only on a stream declared spot. Withdrawal is confined to
/// spot rows by a database CHECK and the rollups exclude spot, which is what keeps a retraction
/// out of every rollup, so a window elsewhere is refused rather than half-honoured.
pub(super) fn require_spot_window(
    window: Option<&SourceWindow>,
    stream_measurement_type: Option<&str>,
) -> AppResult<()> {
    if window.is_some() && stream_measurement_type != Some(SPOT) {
        return Err(AppError::BadRequest(
            "A completeness window is only accepted for streams declared measurement_type \
             'spot'; continuous sources stay append-only"
                .to_string(),
        ));
    }
    Ok(())
}

/// What admission refused from a pass: a tally by kind, since the per-reading messages carry the
/// offending value and cannot group, and under a completeness window the keys refused, which the
/// diff retains rather than reading as absent at source.
///
/// Admission is per reading, not per request: the caller replays from `last_data_time`, which only
/// advances on success, so refusing a batch for one bad row would stall the stream.
pub(super) struct IngestFunnel {
    submitted: usize,
    windowed: bool,
    counts: Vec<(admission::RejectionKind, usize)>,
    rejected_keys: HashSet<Key>,
}

impl IngestFunnel {
    pub(super) fn new(readings: &[IngestReading], windowed: bool) -> Self {
        Self {
            submitted: readings.len(),
            windowed,
            counts: Vec::new(),
            rejected_keys: HashSet::new(),
        }
    }

    fn refuse(&mut self, r: &IngestReading, kind: admission::RejectionKind) {
        record_rejection(&mut self.counts, kind);
        if self.windowed {
            self.rejected_keys.insert((r.time, r.replicate_index));
        }
    }

    /// Drops every reading admission refuses at `now`.
    pub(super) fn admit(&mut self, readings: &mut Vec<IngestReading>, now: DateTime<Utc>) {
        readings.retain(|r| {
            match admission::rejection_kind_at(
                now,
                r.time,
                r.raw_value,
                r.measurement_type.as_deref(),
            ) {
                None => true,
                Some(kind) => {
                    self.refuse(r, kind);
                    false
                }
            }
        });
    }

    /// Under a completeness window, two rows at one key would classify one submitted row twice.
    /// The last occurrence wins (backends emit source-id order, so last is deterministic) and the
    /// losers are counted, so the receipt arithmetic closes.
    pub(super) fn keep_last_per_key(&mut self, readings: &mut Vec<IngestReading>) {
        if !self.windowed {
            return;
        }
        let mut seen = HashSet::new();
        let mut keep = vec![false; readings.len()];
        for (i, r) in readings.iter().enumerate().rev() {
            keep[i] = seen.insert((r.time, r.replicate_index));
        }
        let mut keep = keep.into_iter();
        readings.retain(|_| {
            let kept = keep.next().unwrap_or(true);
            if !kept {
                record_rejection(&mut self.counts, admission::RejectionKind::DuplicateKey);
            }
            kept
        });
    }

    /// Drops every reading naming a calibration that does not exist. A deleted calibration never
    /// reappears, so refusing the request would stall the cursor for good.
    pub(super) fn drop_unknown_calibrations(
        &mut self,
        readings: &mut Vec<IngestReading>,
        declared: &HashMap<Uuid, sensor_calibrations::service::Curve>,
    ) {
        readings.retain(|r| match r.calibration_id {
            Some(id) if !declared.contains_key(&id) => {
                self.refuse(r, admission::RejectionKind::UnknownCalibration);
                false
            }
            _ => true,
        });
    }

    /// Drops every replicate whose resolved cadence is not spot. Only a spot instant has
    /// replicates, and a row that is not one would sit at an index every continuous reader
    /// filters out, ie. served nowhere.
    pub(super) fn drop_replicates_off_spot(
        &mut self,
        readings: &mut Vec<IngestReading>,
        attribution: &IngestAttribution,
    ) {
        readings.retain(|r| {
            if r.replicate_index == 0 || attribution.measurement_type_of(r) == SPOT {
                return true;
            }
            self.refuse(r, admission::RejectionKind::ReplicateIndexOnNonSpot);
            false
        });
    }

    pub(super) fn rejected_keys(&self) -> &HashSet<Key> {
        &self.rejected_keys
    }

    pub(super) fn rejected_total(&self) -> usize {
        self.counts.iter().map(|(_, n)| n).sum()
    }

    /// The tally as the receipt stores it, keyed by the rejection's description.
    pub(super) fn rejected_by_kind(&self) -> serde_json::Value {
        serde_json::Value::Object(
            self.counts
                .iter()
                .map(|(kind, n)| (kind.as_str().to_string(), serde_json::json!(n)))
                .collect(),
        )
    }

    /// The response for a pass that stored `inserted` rows.
    pub(super) fn outcome(&self, stream_id: Uuid, paired: bool, inserted: usize) -> IngestResponse {
        ingest_outcome(stream_id, paired, self.submitted, inserted, &self.counts)
    }
}

/// What a pass's readings are attributed to: the stream's pairing and its frozen instrument, the
/// deployment windows covering each reading's own time, and the cadence each candidate instrument
/// declares.
pub(super) struct IngestAttribution {
    pub(super) stream_sensor: Option<Uuid>,
    pub(super) stream_measurement_type: Option<String>,
    pub(super) site_id: Option<Uuid>,
    pub(super) parameter_id: Option<Uuid>,
    /// Calibration, deployment and site by reading time, from the stream instrument's windows.
    pub(super) windows:
        HashMap<DateTime<Utc>, crate::routes::private::sensors::models::ResolvedSlot>,
    /// The slot's owner by reading time, for a stream that carries no instrument of its own.
    pub(super) owners:
        HashMap<DateTime<Utc>, crate::routes::private::sensors::models::ResolvedOwner>,
    pub(super) sensor_types: HashMap<Uuid, &'static str>,
}

impl IngestAttribution {
    /// Pairing is what attributes a reading to a site; an unpaired stream's readings are staged.
    pub(super) fn paired(&self) -> bool {
        self.site_id.is_some()
    }

    /// The instrument a reading resolves to: its own, the stream's, or the slot owner's.
    pub(super) fn instrument_of(&self, r: &IngestReading) -> Option<Uuid> {
        ingest_instrument(
            r.sensor_id,
            self.stream_sensor,
            self.owners.get(&r.time).and_then(|o| o.sensor_id),
        )
    }

    /// The cadence a reading is stored under: its own, the stream's, then its instrument's.
    pub(super) fn measurement_type_of(&self, r: &IngestReading) -> String {
        resolve_measurement_type(
            r.measurement_type.as_deref(),
            self.stream_measurement_type.as_deref(),
            self.instrument_of(r),
            &self.sensor_types,
        )
    }

    /// Every instrument a reading could resolve to, sorted and once each.
    pub(super) fn candidate_instruments(&self, readings: &[IngestReading]) -> Vec<Uuid> {
        let mut ids: Vec<Uuid> = readings
            .iter()
            .filter_map(|r| r.sensor_id)
            .chain(self.stream_sensor)
            .chain(self.owners.values().filter_map(|o| o.sensor_id))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// One resolver request per reading that resolves to an instrument: that instrument's curve
    /// on the stream's parameter at the reading's own time.
    pub(super) fn calibration_requests(
        &self,
        readings: &[IngestReading],
    ) -> Vec<(Uuid, Option<Uuid>, DateTime<Utc>)> {
        readings
            .iter()
            .filter_map(|r| Some((self.instrument_of(r)?, self.parameter_id, r.time)))
            .collect()
    }

    /// Reads the sensor-frequency default of every candidate instrument, applied when neither the
    /// reading nor the stream declares a cadence.
    pub(super) async fn read_cadences<C: ConnectionTrait>(
        &mut self,
        conn: &C,
        readings: &[IngestReading],
    ) -> AppResult<()> {
        self.sensor_types =
            measurement_types_for_sensors(conn, &self.candidate_instruments(readings)).await?;
        Ok(())
    }
}

/// A pass's attribution before any cadence is read: the deployment windows of the stream's frozen
/// instrument by reading time, agreeing with the reprocess, or where the stream carries none, the
/// slot's deployment timeline, so a reading still lands owned when a deployment covers it.
pub(super) async fn resolve_ingest_attribution<C: ConnectionTrait>(
    conn: &C,
    stream: &data_streams::models::Model,
    (site_id, parameter_id): (Option<Uuid>, Option<Uuid>),
    readings: &[IngestReading],
) -> AppResult<IngestAttribution> {
    let times: Vec<DateTime<Utc>> = readings.iter().map(|r| r.time).collect();
    let windows = match stream.sensor_id {
        Some(sensor_id) => {
            sensors::service::resolve_windows_for_times(conn, sensor_id, None, parameter_id, &times)
                .await?
        }
        None => HashMap::new(),
    };
    let owners = match (stream.sensor_id, site_id, parameter_id) {
        (None, Some(site_id), Some(parameter_id)) => {
            sensors::service::resolve_slot_owner_for_times(conn, site_id, parameter_id, &times)
                .await?
        }
        _ => HashMap::new(),
    };
    Ok(IngestAttribution {
        stream_sensor: stream.sensor_id,
        stream_measurement_type: stream.measurement_type.clone(),
        site_id,
        parameter_id,
        windows,
        owners,
        sensor_types: HashMap::new(),
    })
}

/// The curves a pass's readings may be stored against: the calibration covering each reading's own
/// time, ranked by the resolver the reprocess uses, so a stored value and a later recompute agree;
/// the calibrations callers named; and the standard curves whose claims were admitted.
pub(super) struct IngestCurves {
    pub(super) resolved:
        HashMap<(Uuid, Option<Uuid>, DateTime<Utc>), sensor_calibrations::service::Curve>,
    pub(super) declared: HashMap<Uuid, sensor_calibrations::service::Curve>,
    pub(super) standard: HashMap<Uuid, sensor_calibrations::service::Curve>,
}

fn curves_by_id(
    rows: impl IntoIterator<Item = (Uuid, f64, f64)>,
) -> HashMap<Uuid, sensor_calibrations::service::Curve> {
    rows.into_iter()
        .map(|(id, slope, intercept)| {
            (
                id,
                sensor_calibrations::service::Curve {
                    id,
                    slope,
                    intercept,
                },
            )
        })
        .collect()
}

fn named_once(ids: impl Iterator<Item = Uuid>) -> Vec<Uuid> {
    let mut ids: Vec<Uuid> = ids.collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// The calibrations callers named, by id. A caller is taken at its word about which curve applies,
/// but the stored value is still computed from that curve's coefficients, so reference and value
/// come from one curve.
pub(super) async fn find_declared_calibrations<C: ConnectionTrait>(
    conn: &C,
    readings: &[IngestReading],
) -> AppResult<HashMap<Uuid, sensor_calibrations::service::Curve>> {
    let ids = named_once(readings.iter().filter_map(|r| r.calibration_id));
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sensor_calibrations::Entity::find()
        .filter(sensor_calibrations::Column::Id.is_in(ids))
        .all(conn)
        .await?;
    Ok(curves_by_id(
        rows.into_iter().map(|c| (c.id, c.slope, c.intercept)),
    ))
}

/// The standard curves a pass's readings claim, with the instrument each was fitted on.
pub(super) struct ClaimedCurves {
    pub(super) instruments: HashMap<Uuid, Uuid>,
    pub(super) curves: HashMap<Uuid, sensor_calibrations::service::Curve>,
}

pub(super) async fn find_claimed_standard_curves<C: ConnectionTrait>(
    conn: &C,
    readings: &[IngestReading],
) -> AppResult<ClaimedCurves> {
    let ids = named_once(readings.iter().filter_map(|r| r.standard_curve_id));
    if ids.is_empty() {
        return Ok(ClaimedCurves {
            instruments: HashMap::new(),
            curves: HashMap::new(),
        });
    }
    let rows = standard_curves::Entity::find()
        .filter(standard_curves::Column::Id.is_in(ids))
        .all(conn)
        .await?;
    Ok(ClaimedCurves {
        instruments: rows.iter().map(|c| (c.id, c.sensor_id)).collect(),
        curves: curves_by_id(rows.iter().map(|c| (c.id, c.slope, c.intercept))),
    })
}

/// Holds each standard curve claim to the grab rules: fitted on the reading's instrument, on a spot
/// reading. An inadmissible claim is stripped, never the reading, and returned for the review
/// queue, so a mis-homed curve or a mis-declared stream instrument costs a correction, not data.
pub(super) fn strip_inadmissible_curve_claims(
    readings: &mut [IngestReading],
    claimed: &ClaimedCurves,
    attribution: &IngestAttribution,
) -> StrippedClaims {
    let mut stripped = StrippedClaims::new();
    for r in readings.iter_mut() {
        let Some(id) = r.standard_curve_id else {
            continue;
        };
        let instrument = attribution.instrument_of(r);
        let curve_instrument = claimed.instruments.get(&id).copied();
        let reason = match curve_instrument {
            None => "names no standard curve",
            Some(fitted_on) if Some(fitted_on) != instrument => {
                "fitted on a different instrument than the reading's"
            }
            Some(_) if attribution.measurement_type_of(r) != SPOT => {
                "the reading is not a spot measurement"
            }
            Some(_) => continue,
        };
        r.standard_curve_id = None;
        stripped.entry(r.time).or_default().push(serde_json::json!({
            "replicate_index": r.replicate_index,
            "standard_curve_id": id,
            "curve_instrument_id": curve_instrument,
            "reading_instrument_id": instrument,
            "reason": reason,
        }));
    }
    stripped
}

/// The row a reading is stored as.
///
/// On a paired stream every column is resolved: the site from the deployment covering the reading
/// (a sensor can move while the stream keeps pointing at one slot), the instrument, the one curve
/// both `calibration_id` and `calibrated_value` come from (the caller's if it named one, else
/// whichever window covers the reading's own time), and a hand-picked standard curve composed on
/// that result. No curve leaves `calibrated_value` NULL, which is what a reprocess over the same
/// windows leaves too.
///
/// On an unpaired stream the reading is staged: what the caller declared is kept, because that is a
/// claim somebody made, and nothing is derived until the pairing stamps site, parameter and
/// instrument together (B223). The cadence still reads the channel's instrument, since which device
/// a feed comes through is a fact about the channel.
pub(super) fn reading_model(
    stream_id: Uuid,
    r: &IngestReading,
    attribution: &IngestAttribution,
    curves: &IngestCurves,
) -> readings::ActiveModel {
    let paired = attribution.paired();
    let slot = attribution.windows.get(&r.time);
    let owner = attribution.owners.get(&r.time);
    let sensor_id = attribution.instrument_of(r);
    let curve = r
        .calibration_id
        .and_then(|id| curves.declared.get(&id).copied())
        .or_else(|| {
            sensor_id.and_then(|s| {
                curves
                    .resolved
                    .get(&(s, attribution.parameter_id, r.time))
                    .copied()
            })
        });
    let standard = r
        .standard_curve_id
        .and_then(|id| curves.standard.get(&id).copied());
    readings::ActiveModel {
        standard_curve_id: Set(standard.map(|c| c.id)),
        provenance_kind: Set(Some("sync".to_string())),
        site_id: Set(attribution
            .site_id
            .map(|paired_site| slot.and_then(|s| s.site_id).unwrap_or(paired_site))),
        parameter_id: Set(attribution.parameter_id),
        calibrated_value: Set(match (curve.filter(|_| paired), standard) {
            (None, None) => None,
            (base, standard) => Some(sensor_calibrations::service::apply_curves(
                r.raw_value,
                base,
                standard,
            )),
        }),
        sensor_id: Set(if paired { sensor_id } else { r.sensor_id }),
        calibration_id: Set(if paired {
            curve.map(|c| c.id)
        } else {
            r.calibration_id
        }),
        deployment_id: Set(if paired {
            r.deployment_id
                .or_else(|| slot.and_then(|s| s.deployment_id))
                .or_else(|| owner.and_then(|o| o.deployment_id))
        } else {
            r.deployment_id
        }),
        measurement_type: Set(Some(attribution.measurement_type_of(r))),
        ..readings::new(stream_id, r.time.into(), r.replicate_index, r.raw_value)
    }
}

/// The rows a pass's readings are stored as, in payload order.
pub(super) fn reading_models(
    stream_id: Uuid,
    readings: &[IngestReading],
    attribution: &IngestAttribution,
    curves: &IngestCurves,
) -> Vec<readings::ActiveModel> {
    readings
        .iter()
        .map(|r| reading_model(stream_id, r, attribution, curves))
        .collect()
}

/// The span of a pass's spot rows on a paired stream, the window its sample groups and visits are
/// rebuilt over: a (site, parameter, instant) group takes in rows already stored at the slot. An
/// unpaired stream has none, since its pairing backfill materialises them.
pub(super) fn spot_window(models: &[readings::ActiveModel], paired: bool) -> Option<IngestSpan> {
    if !paired {
        return None;
    }
    let spot_times = models
        .iter()
        .filter_map(|m| match (&m.measurement_type, &m.time) {
            (ActiveValue::Set(Some(t)), ActiveValue::Set(time)) if t == SPOT => {
                Some(time.with_timezone(&Utc))
            }
            _ => None,
        });
    spot_times.clone().min().zip(spot_times.max())
}

/// The rows a pass writes. Under a diff, only the keys it classified new: an unchanged row
/// rewritten with identical values is WAL and an upsert count for nothing, and a changed one is a
/// proposal (Q84). An overwrite writes every row, since it exists to rewrite attribution, which
/// value equality cannot see.
pub(super) fn rows_to_write<'a>(
    models: &'a [readings::ActiveModel],
    readings: &[IngestReading],
    diff: Option<&DiffOutcome>,
    overwrite: bool,
) -> std::borrow::Cow<'a, [readings::ActiveModel]> {
    match diff {
        Some(d) if !overwrite => models
            .iter()
            .zip(readings)
            .filter(|(_, r)| d.write_keys.contains(&(r.time, r.replicate_index)))
            .map(|(m, _)| m.clone())
            .collect::<Vec<_>>()
            .into(),
        _ => models.into(),
    }
}

/// Whether a pass changed any group's content. One that wrote, withdrew or reinstated nothing left
/// the sample statistics and visit attachments as they stood; an overwrite may have moved
/// attribution, so it always counts.
pub(super) fn pass_touched_groups(overwrite: bool, diff: Option<&DiffOutcome>) -> bool {
    overwrite || diff.is_none_or(|d| d.new_rows + d.changed + d.withdrawn + d.reinstated > 0)
}

/// A pass's write: the windowed diff, the rows and the decisions they carry, the sample groups and
/// visits they touch, the statistics audit, the stripped-claim holds and the receipt, all
/// committing together.
pub(super) struct IngestPass<'a> {
    pub(super) payload: &'a IngestReadingsRequest,
    pub(super) models: &'a [readings::ActiveModel],
    pub(super) funnel: &'a IngestFunnel,
    pub(super) stripped: &'a StrippedClaims,
    pub(super) sample_window: Option<IngestSpan>,
    pub(super) paired: bool,
    pub(super) is_sync_service: bool,
    pub(super) actor: &'a str,
}

/// What a pass's write left: the rows the upsert counted, the diff's classification, and the visits
/// it touched.
pub(super) struct IngestWritten {
    pub(super) inserted: usize,
    pub(super) diff: Option<DiffOutcome>,
    pub(super) touched_events: Vec<TouchedEvent>,
}

impl IngestPass<'_> {
    /// Writes the pass on `txn`. An overwrite, a window, a sample stamping reaching back-dated
    /// groups or an audit can touch compressed chunks, so those run guarded, with the decompression
    /// cap lifted; a plain append of new rows is a plain insert.
    pub(super) async fn write(
        &self,
        txn: &sea_orm::DatabaseTransaction,
    ) -> AppResult<IngestWritten> {
        if !self.is_guarded() {
            let inserted = insert_reading_chunks(txn, self.models, Replace::Nothing).await?;
            return Ok(IngestWritten {
                inserted,
                diff: None,
                touched_events: Vec::new(),
            });
        }
        bulk_write::guarded(txn, async |guarded| self.write_guarded(guarded).await).await
    }

    fn is_guarded(&self) -> bool {
        self.payload.overwrite
            || self.sample_window.is_some()
            || self.payload.window.is_some()
            || self.payload.audit.as_deref().is_some_and(|a| !a.is_empty())
            || !self.stripped.is_empty()
    }

    async fn write_guarded(&self, txn: &sea_orm::DatabaseTransaction) -> AppResult<IngestWritten> {
        let diff = self.diff_window(txn).await?;
        let replace = if self.payload.overwrite {
            Replace::ValuesAndAttribution
        } else {
            Replace::Nothing
        };
        let rows = rows_to_write(
            self.models,
            &self.payload.readings,
            diff.as_ref(),
            self.payload.overwrite,
        );
        self.record_corrections(txn, &rows, replace).await?;
        let inserted = insert_reading_chunks(txn, &rows, replace).await?;
        record_curve_claims(txn, &rows, self.actor, Origin::Sync).await?;
        let touched_events = self.regroup_spot_rows(txn, diff.as_ref()).await?;
        self.audit_replicates(txn, diff.as_ref()).await?;
        self.hold_stripped_claims(txn).await?;
        self.record_receipt(txn, diff.as_ref()).await?;
        Ok(IngestWritten {
            inserted,
            diff,
            touched_events,
        })
    }

    /// The windowed diff of a completeness claim against the store; none without a claim.
    async fn diff_window(
        &self,
        txn: &sea_orm::DatabaseTransaction,
    ) -> AppResult<Option<DiffOutcome>> {
        let Some(window) = &self.payload.window else {
            return Ok(None);
        };
        let admitted: Vec<AdmittedRow> = self
            .payload
            .readings
            .iter()
            .map(|r| {
                (
                    (r.time, r.replicate_index),
                    r.raw_value,
                    r.standard_curve_id,
                )
            })
            .collect();
        run_windowed_diff(
            txn,
            self.payload.stream_id,
            window,
            &admitted,
            self.funnel.rejected_keys(),
            self.actor,
            self.paired,
        )
        .await
        .map(Some)
    }

    /// A correction is a decision of sync origin (ADR 0008), recorded before the upsert so the
    /// value it replaces is what the record holds.
    async fn record_corrections(
        &self,
        txn: &sea_orm::DatabaseTransaction,
        rows: &[readings::ActiveModel],
        replace: Replace,
    ) -> AppResult<()> {
        if replace != Replace::Nothing {
            record_value_corrections(txn, rows, self.actor, Origin::Sync).await?;
        }
        Ok(())
    }

    /// Rebuilds the sample groups and the visit attachments over the pass's spot window and names
    /// the visits touched. Each source row maps onto one collection event (D7): a sync service
    /// replaying a portal row writes a portal_sync event, any other writer is a person.
    async fn regroup_spot_rows(
        &self,
        txn: &sea_orm::DatabaseTransaction,
        diff: Option<&DiffOutcome>,
    ) -> AppResult<Vec<TouchedEvent>> {
        let Some((lo, hi)) = self
            .sample_window
            .filter(|_| pass_touched_groups(self.payload.overwrite, diff))
        else {
            return Ok(Vec::new());
        };
        let window = || {
            Condition::all()
                .add(flows::row(readings::Column::StreamId).eq(self.payload.stream_id))
                .add(flows::row(readings::Column::Time).gte(DateTimeWithTimeZone::from(lo)))
                .add(flows::row(readings::Column::Time).lte(DateTimeWithTimeZone::from(hi)))
        };
        let source = if self.is_sync_service {
            collection_events::service::EventSource::PortalSync
        } else {
            collection_events::service::EventSource::Manual
        };
        materialise_samples(txn, window()).await?;
        collection_events::service::attach_collection_events(txn, window(), source).await?;
        flows::touched_events(txn, window()).await
    }

    /// The statistics audit judges what this transaction stored, so its holds commit or roll back
    /// with the writes. A braked pass withheld the corrections the claim describes, so the stored
    /// groups are not what the source asserted and the holds stand.
    async fn audit_replicates(
        &self,
        txn: &sea_orm::DatabaseTransaction,
        diff: Option<&DiffOutcome>,
    ) -> AppResult<()> {
        let Some(audits) = self.payload.audit.as_deref().filter(|a| !a.is_empty()) else {
            return Ok(());
        };
        if diff.is_some_and(|d| d.braked) {
            return Ok(());
        }
        run_replicate_audit(txn, self.payload.stream_id, audits, self.paired).await
    }

    /// One review-queue hold per instant whose curve claim was stripped. Raised after the
    /// statistics audit on purpose: at one (stream, instant) key the later upsert wins, and a
    /// stripped claim explains the disagreement the audit would otherwise report bare.
    async fn hold_stripped_claims(&self, txn: &sea_orm::DatabaseTransaction) -> AppResult<()> {
        if self.stripped.is_empty() {
            return Ok(());
        }
        let status = audit::status_for(self.paired);
        for (time, claims) in self.stripped {
            upsert_curve_claim_hold(txn, self.payload.stream_id, *time, claims, status).await?;
        }
        tracing::warn!(
            stream_id = %self.payload.stream_id,
            instants = self.stripped.len(),
            "Inadmissible standard curve claims stripped; readings stored uncorrected and held for review"
        );
        Ok(())
    }

    /// The receipt of a windowed pass, committed with it.
    async fn record_receipt(
        &self,
        txn: &sea_orm::DatabaseTransaction,
        diff: Option<&DiffOutcome>,
    ) -> AppResult<()> {
        let (Some(window), Some(d)) = (&self.payload.window, diff) else {
            return Ok(());
        };
        write_receipt(
            txn,
            self.payload.stream_id,
            window,
            self.funnel.submitted,
            d,
            self.funnel.rejected_total(),
            &self.funnel.rejected_by_kind(),
        )
        .await
    }
}

/// What a committed pass did to served content, the gate every step after the commit reads.
pub(super) struct IngestEffect {
    pub(super) inserted: usize,
    pub(super) withdrawn: usize,
    pub(super) reinstated: usize,
    /// An overwrite that wrote rows: history bounded queries may have cached, and values the
    /// rollups have already materialised.
    pub(super) corrected: bool,
    /// Sample formation, a window, a withdrawal or a correction rewrote served history, which no
    /// cache anywhere may be left to expire on TTL.
    pub(super) rewrote_history: bool,
    pub(super) span: Option<IngestSpan>,
}

impl IngestEffect {
    pub(super) fn of(
        payload: &IngestReadingsRequest,
        sample_window: Option<IngestSpan>,
        inserted: usize,
        diff: Option<&DiffOutcome>,
    ) -> Self {
        let withdrawn = diff.map_or(0, |d| d.withdrawn);
        let corrected = payload.overwrite && inserted > 0;
        let times = || payload.readings.iter().map(|r| r.time);
        Self {
            inserted,
            withdrawn,
            reinstated: diff.map_or(0, |d| d.reinstated),
            corrected,
            rewrote_history: corrected
                || sample_window.is_some()
                || payload.window.is_some()
                || withdrawn > 0,
            span: times().min().zip(times().max()),
        }
    }

    /// Rows whose served value moved: a withdrawal with nothing inserted still rewrote history. A
    /// classified-changed key moved nothing; it is a proposal until somebody accepts it.
    pub(super) fn moved(&self) -> u64 {
        u64::try_from(self.inserted + self.withdrawn + self.reinstated).unwrap_or(u64::MAX)
    }

    pub(super) fn written(&self, slot: Slot, touched_events: Vec<TouchedEvent>) -> Written {
        Written::new(self.moved())
            .announced(u64::try_from(self.inserted).unwrap_or(u64::MAX))
            .over(self.span)
            .at(vec![slot])
            .touching(touched_events)
    }

    /// The refresh is best-effort: the rows are committed and the cursor is about to advance past
    /// them, so a refresh losing a lock to the janitor must not turn the write into a 500 that
    /// replays it forever. Episodes are rebuilt inline, since a pass fires every sync cycle per
    /// stream and one job each would be the noise.
    pub(super) fn axes(&self) -> Axes {
        Axes {
            cache: if self.rewrote_history {
                Cache::All
            } else {
                Cache::Sites
            },
            refresh: if self.corrected {
                Refresh::Range { fatal: false }
            } else {
                Refresh::Skip
            },
            announce: true,
            reconcile_alarms: true,
            episodes: Episodes::Inline,
            recompute_derived: false,
            writer: flows::Writer::Person,
        }
    }
}

/// Recomposes a corrected span from whichever curves each row ends up carrying, before the rollups
/// read it back: the upsert leaves a hand-picked curve standing and the correction resolved only a
/// base. Best effort, since the rows are committed.
pub(super) async fn recompose_corrected_span(
    db: &DatabaseConnection,
    stream_id: Uuid,
    effect: &IngestEffect,
) {
    let Some((lo, hi)) = effect.span.filter(|_| effect.corrected) else {
        return;
    };
    if let Err(e) = sensor_calibrations::service::recompose_from_own_curves_guarded(
        db,
        Expr::cust("TRUE"),
        "r.stream_id = $1 AND r.time >= $2 AND r.time <= $3",
        vec![
            stream_id.into(),
            DateTimeWithTimeZone::from(lo).into(),
            DateTimeWithTimeZone::from(hi).into(),
        ],
    )
    .await
    {
        tracing::warn!(error = %e, "recompose after overwrite failed");
    }
}

/// Where the stream's cursor moves: the pass's newest instant, when it is past the stored one.
/// Every group is admitted (an audit disagreement is a review record, not a gate), so the cursor
/// always advances to the batch's newest.
pub(super) fn advance_cursor(
    readings: &[IngestReading],
    last_data_time: Option<DateTimeWithTimeZone>,
) -> Option<DateTime<Utc>> {
    readings
        .iter()
        .map(|r| r.time)
        .max()
        .filter(|newest| last_data_time.is_none_or(|t| *newest > t.with_timezone(&Utc)))
}

/// The handshake digest a cleanly applied window leaves on its stream: no brake, no holds, no
/// proposal awaiting, nothing rejected and no curve claim stripped. The sync client compares its
/// next payload against it to skip re-sending unchanged content, so a pass that was not clean
/// stores none and the window keeps re-asserting until a person rules.
pub(super) fn clean_digest(
    window: Option<&SourceWindow>,
    rejected_total: usize,
    claims_kept: bool,
    diff: Option<&DiffOutcome>,
) -> Option<String> {
    let clean = rejected_total == 0
        && claims_kept
        && diff.is_some_and(|d| !d.braked && d.holds_raised == 0 && d.proposals_awaiting == 0);
    window
        .and_then(|w| w.content_digest.clone())
        .filter(|_| clean)
}

/// Records what a pass leaves on its stream, the cursor and a changed digest. Written only while
/// the stream's pairing is still the one the pass attributed under, and best effort: the rows are
/// committed.
pub(super) async fn record_stream_pass(
    db: &DatabaseConnection,
    stream: &data_streams::models::Model,
    cursor: Option<DateTime<Utc>>,
    digest: Option<String>,
) {
    let digest = digest.filter(|d| stream.last_window_digest.as_ref() != Some(d));
    if cursor.is_none() && digest.is_none() {
        return;
    }
    let recorded = data_streams::service::record_pass(
        stream.id,
        stream.site_parameter_id,
        cursor.map(Into::into),
        digest,
    )
    .exec(db)
    .await;
    match recorded {
        Ok(r) if r.rows_affected == 0 => tracing::info!(
            stream_id = %stream.id,
            "Stream pairing changed after the pass read it; cursor and digest left to the next pass"
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "Failed to update stream sync state"),
    }
}

/// The instants whose derived values a pass may have moved, sorted and once each. A withdrawn key
/// is absent from the payload by construction and a reinstated one likewise, so both are unioned
/// in, or a derived value computed from a retracted input is never revisited.
pub(super) fn derived_timestamps(
    readings: &[IngestReading],
    diff: Option<&DiffOutcome>,
) -> Vec<DateTime<Utc>> {
    let mut times: Vec<DateTime<Utc>> = readings.iter().map(|r| r.time).collect();
    if let Some(d) = diff {
        times.extend(d.withdrawn_keys.iter().map(|(t, _)| *t));
        times.extend(d.reinstated_keys.iter().map(|(t, _)| *t));
    }
    times.sort();
    times.dedup();
    times
}

/// Enqueues the derived values a paired pass's instants feed, as a tracked `ingest_derived` job.
/// Skipped where the site has no active derived parameter: that job would compute nothing, and was
/// the dominant source of empty ones.
pub(super) async fn enqueue_ingest_derived(
    db: &DatabaseConnection,
    stream_id: Uuid,
    site_id: Option<Uuid>,
    effect: &IngestEffect,
    readings: &[IngestReading],
    diff: Option<&DiffOutcome>,
) -> AppResult<()> {
    let Some(site_id) = site_id.filter(|_| effect.moved() > 0) else {
        return Ok(());
    };
    let active =
        crate::routes::private::derived_parameters::flows::site_has_active_derived(db, site_id)
            .await
            .unwrap_or(true);
    if !active {
        return Ok(());
    }
    crate::routes::private::reprocessing_jobs::service::enqueue(
        db,
        "ingest_derived",
        None,
        None,
        &serde_json::json!({
            "site_id": site_id,
            "stream_id": stream_id,
            "timestamps": derived_timestamps(readings, diff),
        }),
        None,
    )
    .await?;
    Ok(())
}

/// The response, with the diff's classification in place of the upsert's count under a window:
/// the upsert counts updates too, and the classification is the accurate account.
pub(super) fn classified_outcome(
    mut outcome: IngestResponse,
    diff: Option<&DiffOutcome>,
    window: Option<&SourceWindow>,
) -> IngestResponse {
    if let Some(d) = diff {
        outcome.inserted = d.new_rows;
        outcome.changed = d.changed;
        outcome.proposed = d.proposed;
        outcome.withdrawn = d.withdrawn;
        outcome.unchanged = d.unchanged;
        outcome.retained = d.retained;
        outcome.accepted_window = window.cloned();
        if d.braked {
            tracing::warn!(stream_id = %outcome.stream_id, changed = d.changed, withdrawn = d.withdrawn, "Windowed pass braked; corrections and withdrawals held for review");
        }
    }
    tracing::debug!(inserted = outcome.inserted, skipped = outcome.skipped, changed = outcome.changed, withdrawn = outcome.withdrawn, reinstated = diff.map_or(0, |d| d.reinstated), stream_id = %outcome.stream_id, paired = outcome.paired, "Ingest complete");
    outcome
}

/// Grabs are spot measurements by definition: a bottle, not a logger cadence.
pub(super) const GRAB_MEASUREMENT_TYPE: &str =
    river_data_core::models::MeasurementType::Spot.as_str();

impl From<PriorFactsRow> for StoredFacts {
    fn from(row: PriorFactsRow) -> Self {
        Self {
            label: row.label,
            notes: row.notes,
            created_by: row.created_by,
            provenance: row.provenance,
            kind: row.provenance_kind,
        }
    }
}

/// The line as the operator reads it, sign folded into the operator: `y = 2x - 3`.
pub(super) fn equation(slope: f64, intercept: f64) -> String {
    if intercept < 0.0 {
        format!("y = {slope}x - {}", -intercept)
    } else {
        format!("y = {slope}x + {intercept}")
    }
}

/// Replicate indices per (parameter, time) group: either every index in a group is explicit and
/// unique, or none is and the group numbers from 0. A mix would renumber around the explicit rows
/// and a duplicate would silently drop a measurement, so both are refused. Once written an index
/// is never renumbered.
pub(super) fn assign_replicate_indices(
    readings: &[GrabSampleReading],
) -> Result<Vec<i16>, AppError> {
    let mut groups: HashMap<(Uuid, chrono::DateTime<chrono::Utc>), Vec<usize>> = HashMap::new();
    for (i, r) in readings.iter().enumerate() {
        groups.entry((r.parameter_id, r.time)).or_default().push(i);
    }

    let mut indices = vec![0i16; readings.len()];
    for ((parameter_id, time), members) in groups {
        let explicit: Vec<Option<i16>> = members
            .iter()
            .map(|&i| readings[i].replicate_index)
            .collect();
        if explicit.iter().all(Option::is_some) {
            let mut seen = std::collections::HashSet::new();
            for (&i, idx) in members.iter().zip(&explicit) {
                let idx = idx.expect("all explicit");
                if !seen.insert(idx) {
                    return Err(AppError::Conflict(format!(
                        "Replicate index {idx} appears twice for parameter {parameter_id} at {time}"
                    )));
                }
                indices[i] = idx;
            }
        } else if explicit.iter().all(Option::is_none) {
            for (n, &i) in members.iter().enumerate() {
                indices[i] = i16::try_from(n).map_err(|_| {
                    AppError::BadRequest(format!(
                        "Too many replicates for parameter {parameter_id} at {time}"
                    ))
                })?;
            }
        } else {
            return Err(AppError::BadRequest(format!(
                "Replicate indices for parameter {parameter_id} at {time} mix explicit and \
                 automatic; send all of them or none"
            )));
        }
    }
    Ok(indices)
}

/// The spot rows already stored at each requested (parameter, time), across every stream feeding
/// the slot, so a CSV-imported grab and a hand-entered one count as the same group.
/// One spot group: every replicate of a parameter at a site at one instant.
pub(super) fn spot_group(
    site_id: Uuid,
    parameter_id: Uuid,
    at: chrono::DateTime<chrono::Utc>,
) -> Condition {
    Condition::all()
        .add(readings::Column::SiteId.eq(site_id))
        .add(readings::Column::ParameterId.eq(parameter_id))
        .add(readings::Column::Time.eq(at))
        .add(readings::Column::MeasurementType.eq(SPOT))
}

/// The groups that grew or shrank under a client between its read and its save. A replace rewrites
/// the indexes it carries and retracts the stored ones it leaves out, so a group holding a
/// replicate the client never saw is one the save must not be allowed to retract.
pub(super) fn groups_changed(
    expected: &[ExpectedGroup],
    existing: &[ExistingGroup],
) -> Vec<(Uuid, chrono::DateTime<chrono::Utc>)> {
    expected
        .iter()
        .filter(|group| {
            let stored: Vec<i16> = existing
                .iter()
                .find(|e| e.parameter_id == group.parameter_id && e.time == group.time)
                .map(|e| e.replicates.iter().map(|r| r.replicate_index).collect())
                .unwrap_or_default();
            let mut read = group.replicate_indices.clone();
            read.sort_unstable();
            read.dedup();
            let mut found = stored;
            found.sort_unstable();
            found.dedup();
            read != found
        })
        .map(|group| (group.parameter_id, group.time))
        .collect()
}

/// How many stored replicates an entry would move. A replace rewrites the keys the request names
/// and retracts the stored replicates it leaves out, so a save carrying every stored replicate at
/// the number it already holds moves none of them and only adds where nothing was stored.
pub(super) fn stored_values_moved(
    carried: &[(Uuid, chrono::DateTime<chrono::Utc>, i16, f64)],
    existing: &[ExistingGroup],
) -> usize {
    existing
        .iter()
        .flat_map(|group| {
            group.replicates.iter().map(move |replicate| {
                carried.iter().any(|(parameter_id, time, index, value)| {
                    *parameter_id == group.parameter_id
                        && *time == group.time
                        && *index == replicate.replicate_index
                        && *value == replicate.raw_value
                })
            })
        })
        .filter(|unchanged| !unchanged)
        .count()
}

/// Which carried rows a save enters, in the order carried: a replicate the group does not hold, or
/// one at a number other than the stored one. A stored replicate carried at its own number is the
/// grid posting the whole group, not an entry, so it keeps its own verification state.
pub(super) fn entered_rows(
    carried: &[(Uuid, chrono::DateTime<chrono::Utc>, i16, f64)],
    existing: &[ExistingGroup],
) -> Vec<bool> {
    carried
        .iter()
        .map(|(parameter_id, time, index, value)| {
            !existing.iter().any(|group| {
                group.parameter_id == *parameter_id
                    && group.time == *time
                    && group
                        .replicates
                        .iter()
                        .any(|r| r.replicate_index == *index && r.raw_value == *value)
            })
        })
        .collect()
}

pub(super) async fn fetch_existing_groups(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    groups: &[(Uuid, chrono::DateTime<chrono::Utc>)],
) -> Result<Vec<ExistingGroup>, AppError> {
    let mut out = Vec::new();
    for (parameter_id, time) in groups {
        let replicates = readings::Entity::find()
            .select_only()
            .column(readings::Column::ReplicateIndex)
            .column(readings::Column::RawValue)
            .column(readings::Column::CalibratedValue)
            .column(readings::Column::StandardCurveId)
            .filter(spot_group(site_id, *parameter_id, *time))
            .order_by_asc(readings::Column::ReplicateIndex)
            .into_model::<ExistingReplicate>()
            .all(db)
            .await?;
        if replicates.is_empty() {
            continue;
        }
        out.push(ExistingGroup {
            parameter_id: *parameter_id,
            time: *time,
            replicates,
        });
    }
    Ok(out)
}

/// Get or create a "grab_sample" stream for a given (site_id, parameter_id) pair.
pub(super) async fn get_or_create_grab_stream(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
    site_parameter_id: Option<Uuid>,
) -> Result<Uuid, AppError> {
    let source_key = format!("{site_id}:{parameter_id}");

    if let Some(stream) = data_streams::Entity::find()
        .filter(data_streams::Column::SourceSystem.eq("grab_sample"))
        .filter(data_streams::Column::SourceKey.eq(&source_key))
        .one(db)
        .await?
    {
        // Auto-pair existing unpaired stream
        if stream.site_parameter_id.is_none()
            && let Some(sp_id) = site_parameter_id
        {
            let mut active: data_streams::ActiveModel = stream.clone().into();
            active.site_parameter_id = Set(Some(sp_id));
            active.paired_at = Set(Some(chrono::Utc::now().into()));
            active.updated_at = Set(chrono::Utc::now().into());
            active.update(db).await?;
        }
        crate::routes::private::sensors::service::ensure_channel_instrument(
            db,
            &stream,
            site_id,
            parameter_id,
            "grab entry",
        )
        .await?;
        return Ok(stream.id);
    }

    let now = chrono::Utc::now();
    let id = Uuid::new_v4();
    let model = data_streams::ActiveModel {
        id: Set(id),
        source_system: Set("grab_sample".to_string()),
        source_key: Set(source_key),
        source_name: Set(Some("Grab sample".to_string())),
        source_path: Set(None),
        metadata: Set(serde_json::json!({})),
        site_parameter_id: Set(site_parameter_id),
        sensor_id: Set(None),
        measurement_type: Set(Some(GRAB_MEASUREMENT_TYPE.to_string())),
        is_active: Set(true),
        discovered_at: Set(now.into()),
        paired_at: Set(site_parameter_id.map(|_| now.into())),
        last_data_time: Set(None),
        last_window_digest: Set(None),
        pairing_plan_id: Set(None),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
    };

    data_streams::Entity::insert(model)
        .on_conflict(
            sea_orm::sea_query::OnConflict::columns([
                data_streams::Column::SourceSystem,
                data_streams::Column::SourceKey,
            ])
            .do_nothing()
            .to_owned(),
        )
        .exec_without_returning(db)
        .await
        .map_err(AppError::Database)?;

    let stream = data_streams::Entity::find()
        .filter(data_streams::Column::SourceSystem.eq("grab_sample"))
        .filter(data_streams::Column::SourceKey.eq(format!("{site_id}:{parameter_id}")))
        .one(db)
        .await?
        .ok_or_else(|| AppError::Internal("Failed to create grab sample stream".to_string()))?;

    crate::routes::private::sensors::service::ensure_channel_instrument(
        db,
        &stream,
        site_id,
        parameter_id,
        "grab entry",
    )
    .await?;

    Ok(stream.id)
}

/// What a request records about the measurement itself, as opposed to its value: who entered it,
/// what they called it, what they wrote about it, and the server-built blob saying what produced
/// the number. Every one of these is a property of the reading, so they are written onto each row
/// the request lands rather than onto the statistics row its replicates may or may not form.
pub(super) struct GrabFacts<'a> {
    pub(super) created_by: Option<&'a str>,
    pub(super) label: Option<&'a str>,
    pub(super) notes: Option<&'a str>,
    pub(super) provenance: Option<&'a serde_json::Value>,
    pub(super) kind: &'a str,
}

/// The same four facts as they are stored on a reading, owned.
#[derive(Default)]
pub(super) struct StoredFacts {
    pub(super) created_by: Option<String>,
    pub(super) label: Option<String>,
    pub(super) notes: Option<String>,
    pub(super) provenance: Option<serde_json::Value>,
    pub(super) kind: Option<String>,
}

impl StoredFacts {
    pub(super) fn is_empty(&self) -> bool {
        self.created_by.is_none()
            && self.label.is_none()
            && self.notes.is_none()
            && self.provenance.is_none()
    }
}

impl GrabFacts<'_> {
    /// This request's facts over what the group already carried, field by field: a rewrite that
    /// says nothing about the label keeps the one the group was entered under.
    pub(super) fn over(&self, prior: Option<&StoredFacts>) -> StoredFacts {
        StoredFacts {
            created_by: self
                .created_by
                .map(String::from)
                .or_else(|| prior.and_then(|p| p.created_by.clone())),
            label: self
                .label
                .map(String::from)
                .or_else(|| prior.and_then(|p| p.label.clone())),
            notes: self
                .notes
                .map(String::from)
                .or_else(|| prior.and_then(|p| p.notes.clone())),
            provenance: self
                .provenance
                .cloned()
                .or_else(|| prior.and_then(|p| p.provenance.clone())),
            // The origin follows the blob: a rewrite that carries no run of its own keeps the one
            // the group was computed under rather than reading as a hand entry.
            kind: Some(if self.provenance.is_some() {
                self.kind.to_string()
            } else {
                prior
                    .and_then(|p| p.kind.clone())
                    .unwrap_or_else(|| self.kind.to_string())
            }),
        }
    }
}

/// The statistics row for this group, created if it is not already there.
///
/// Returns the row's id and whether this call is the one that created it.
///
/// The insert yields to a concurrent one rather than testing for the row first: two field entries
/// for the same (site, parameter, time) both see nothing, both insert, and the unique index on
/// those three columns then fails one of them, losing an entire grab to a 500. `DO NOTHING` is what
/// every other writer of `samples` does, and the read below picks up whichever row won.
pub(super) async fn find_or_create_sample(
    txn: &sea_orm::DatabaseTransaction,
    site_id: Uuid,
    parameter_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
) -> Result<(Uuid, bool), AppError> {
    let candidate = samples::ActiveModel {
        id: Set(Uuid::new_v4()),
        site_id: Set(site_id),
        parameter_id: Set(parameter_id),
        collected_at: Set(time),
        created_at: Set(Some(chrono::Utc::now())),
        mean: Set(None),
        stdev: sea_orm::ActiveValue::NotSet,
        median: Set(None),
        n: Set(0),
        min_value: Set(None),
        max_value: Set(None),
        updated_at: Set(None),
    };
    let inserted = match samples::Entity::insert(candidate)
        .on_conflict(
            sea_orm::sea_query::OnConflict::columns([
                samples::Column::SiteId,
                samples::Column::ParameterId,
                samples::Column::CollectedAt,
            ])
            .do_nothing()
            .to_owned(),
        )
        .exec_without_returning(txn)
        .await
    {
        Ok(rows) => rows > 0,
        // A conflict that inserted nothing is the expected outcome of re-posting a grab, not a
        // failure.
        Err(sea_orm::DbErr::RecordNotInserted) => false,
        Err(e) => return Err(AppError::Database(e)),
    };

    let existing = samples::Entity::find()
        .filter(samples::Column::SiteId.eq(site_id))
        .filter(samples::Column::ParameterId.eq(parameter_id))
        .filter(samples::Column::CollectedAt.eq(time))
        .one(txn)
        .await?
        .ok_or_else(|| {
            AppError::Internal("Failed to record the sample for this grab".to_string())
        })?;

    Ok((existing.id, inserted))
}

/// Form the `samples` row for every group this request left with two or more stored spot readings,
/// and stamp the group's readings with it. Returns the group-to-sample map and the rows created.
///
/// Counted over what is stored rather than over the request: a second replicate entered later joins
/// the instant's group, and a single measurement re-posted alone never mints a row of one.
pub(super) async fn materialise_grab_samples(
    txn: &sea_orm::DatabaseTransaction,
    groups: &[(Uuid, chrono::DateTime<chrono::Utc>)],
    site_id: Uuid,
) -> Result<Vec<Uuid>, AppError> {
    let mut created: Vec<Uuid> = Vec::new();
    for (parameter_id, time) in groups {
        let stored = readings::Entity::find()
            .filter(spot_group(site_id, *parameter_id, *time))
            .count(txn)
            .await?;
        if !forms_sample(usize::try_from(stored).unwrap_or(0)) {
            continue;
        }
        // Re-posting the same grab must reuse its sample, not accumulate empty duplicates.
        let (sample_id, is_new) = find_or_create_sample(txn, site_id, *parameter_id, *time).await?;
        if is_new {
            created.push(sample_id);
        }
        // Scoped to spot readings: a sonde reading sharing the grab's snapped timestamp must not be
        // adopted into the sample, or the trigger folds sensor data into the grab statistics.
        readings::Entity::update_many()
            .col_expr(readings::Column::SampleId, Expr::value(sample_id))
            .filter(spot_group(site_id, *parameter_id, *time))
            .filter(readings::Column::SampleId.is_null())
            .exec(txn)
            .await?;
    }

    Ok(created)
}

/// Whether `value` is the output's value: the scalar itself, or one of the numeric leaves of a
/// replicate-shaped output. Exact equality on purpose: the numbers travelled through JSON at full
/// precision, so an edited value is a different number and the tool link it claims is not true.
pub(super) fn output_carries_value(output: &serde_json::Value, value: f64) -> bool {
    match output {
        serde_json::Value::Number(n) => n.as_f64() == Some(value),
        serde_json::Value::Array(items) => items.iter().any(|v| output_carries_value(v, value)),
        serde_json::Value::Object(map) => map.values().any(|v| output_carries_value(v, value)),
        _ => false,
    }
}

/// The server-built provenance blob for a save that names a tool run, `None` for a manual entry.
///
/// The blob is constructed from the stored `tool_runs` row, never from the request: the run's
/// inputs, constants, curves and outputs are what the engine resolved at calculate time, the
/// calculating actor and timestamp were stamped then, and the saving actor is the caller here.
/// Fail-closed on the link itself: every reading must name one of the run's outputs and carry
/// that output's value, so a save cannot claim a run it did not use. A run that consumed a
/// standard curve produced corrected outputs, so any reading carrying `standard_curve_id` is
/// refused (ADR 0003: a stored curve id means raw in, curve out, so stamping one here would apply
/// the correction twice).
/// The manifest of the tool version a run was executed under. A save reads what the run meant,
/// never the tool's current active manifest. `None` when there is no run, no matching version, or
/// a stored manifest that no longer parses (a tool-authoring problem, not this write's).
pub(super) async fn run_pinned_manifest(
    db: &DatabaseConnection,
    run_id: Uuid,
) -> Result<Option<crate::routes::private::tools::models::Manifest>, AppError> {
    // Two lookups by primary key rather than a join through a jsonb cast: the run names its
    // version inside `tool_version`, which no typed column expresses.
    let Some(version_id) = tool_run::Entity::find_by_id(run_id)
        .select_only()
        .column(tool_run::Column::ToolVersion)
        .into_tuple::<serde_json::Value>()
        .one(db)
        .await?
        .and_then(|v| {
            v.get("script_version_id")
                .and_then(serde_json::Value::as_str)
                .and_then(|s| Uuid::parse_str(s).ok())
        })
    else {
        return Ok(None);
    };
    let Some(manifest) = tool_version::Entity::find_by_id(version_id)
        .select_only()
        .column(tool_version::Column::Manifest)
        .into_tuple::<serde_json::Value>()
        .one(db)
        .await?
    else {
        return Ok(None);
    };
    Ok(crate::routes::private::tools::models::parse_manifest(&manifest).ok())
}

/// What a tool-run save may store for a named input of the run, and at which index.
///
/// A `replicates` input is an array: each position is a measurement, saved at its own index. A
/// numeric input the manifest binds to a visit parameter (`event_inputs`) is one measurement the
/// operator typed over, saved at index 0 as a correction of what the visit holds. Any other name
/// is a run-only setting with nowhere to be written. Either way the value must be what the run
/// consumed, so the stored number and the provenance cannot disagree.
fn check_saved_input(
    tool_name: &str,
    inputs: &serde_json::Value,
    event_input_params: &std::collections::HashSet<String>,
    input: &str,
    value: f64,
    replicate_index: Option<i16>,
) -> Result<(), String> {
    if let Some(values) = inputs.get(input).and_then(serde_json::Value::as_array) {
        let Some(index) = replicate_index else {
            return Err(format!(
                "Reading for input '{input}' needs the replicate_index it was entered at"
            ));
        };
        let recorded = usize::try_from(index)
            .ok()
            .and_then(|i| values.get(i))
            .and_then(serde_json::Value::as_f64);
        if recorded != Some(value) {
            return Err(format!(
                "Value {value} is not what this {tool_name} run consumed at replicate {index} of \
                 '{input}'"
            ));
        }
        return Ok(());
    }
    let scalar = inputs.get(input).and_then(serde_json::Value::as_f64);
    if let Some(recorded) = scalar.filter(|_| event_input_params.contains(input)) {
        if replicate_index.is_some_and(|i| i != 0) {
            return Err(format!(
                "'{input}' is one measurement of this {tool_name} run, so it is saved at replicate 0"
            ));
        }
        if recorded != value {
            return Err(format!(
                "Value {value} is not what this {tool_name} run consumed for '{input}'"
            ));
        }
        return Ok(());
    }
    if scalar.is_some() {
        return Err(format!(
            "'{input}' is a setting of this {tool_name} run, not a measurement of this visit; \
             only an input the calculation reads from the visit is saved"
        ));
    }
    Err(format!(
        "'{input}' is not a replicates input of this {tool_name} run"
    ))
}

/// Whether a reading saved as a run's input or output is of the parameter the pinned manifest
/// binds that name to. A name the manifest binds to nothing keeps the requested parameter, which
/// is then the only source there is.
fn check_bound_parameter(
    tool_name: &str,
    kind: &str,
    name: &str,
    bound: Option<(Uuid, &str)>,
    parameter_id: Uuid,
) -> Result<(), String> {
    match bound {
        Some((id, code)) if id != parameter_id => Err(format!(
            "This {tool_name} run's manifest binds {kind} '{name}' to {code} ({id}); a reading of \
             parameter {parameter_id} is not a measurement of it"
        )),
        _ => Ok(()),
    }
}

type Bindings = std::collections::HashMap<String, (Uuid, String)>;

/// The catalog parameter the pinned manifest binds each input name and each output key to, where
/// it binds one that resolves.
async fn manifest_bindings(
    db: &DatabaseConnection,
    pinned: Option<&crate::routes::private::tools::models::Manifest>,
) -> Result<(Bindings, Bindings), AppError> {
    use crate::routes::private::tools::service as engine;
    let Some(manifest) = pinned else {
        return Ok(Default::default());
    };
    let catalog = engine::load_parameter_catalog(db, std::iter::once(manifest)).await?;
    let mut inputs = Bindings::new();
    for p in manifest.params.iter().filter(|p| p.kind == "replicates") {
        if let Some(row) = p
            .parameter_code
            .as_deref()
            .and_then(|c| catalog.resolve_code(c))
        {
            inputs.insert(p.name.clone(), (row.id, row.code));
        }
    }
    for e in &manifest.event_inputs {
        if let Some(row) = catalog.resolve_code(&e.parameter_code) {
            inputs.insert(e.param.clone(), (row.id, row.code));
        }
    }
    let outputs = manifest
        .outputs
        .iter()
        .filter_map(|o| {
            catalog
                .resolve(o)
                .map(|row| (o.key.clone(), (row.id, row.code)))
        })
        .collect();
    Ok((inputs, outputs))
}

pub(super) async fn resolve_tool_run_provenance(
    db: &DatabaseConnection,
    tool_run_id: Option<Uuid>,
    site_id: Uuid,
    readings: &[GrabSampleReading],
    saved_by: &str,
) -> Result<Option<serde_json::Value>, AppError> {
    let Some(run_id) = tool_run_id else {
        if let Some(r) = readings
            .iter()
            .find(|r| r.output.is_some() || r.input.is_some())
        {
            return Err(AppError::BadRequest(format!(
                "Reading for parameter {} names tool {} '{}' but the request carries no \
                 tool_run_id",
                r.parameter_id,
                if r.output.is_some() {
                    "output"
                } else {
                    "input"
                },
                r.output
                    .as_deref()
                    .or(r.input.as_deref())
                    .unwrap_or_default()
            )));
        }
        return Ok(None);
    };

    let run = tool_run::Entity::find_by_id(run_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::BadRequest(format!("Tool run {run_id} does not exist")))?;

    let tool_run::Model {
        tool_name,
        source: run_source,
        context: run_context,
        tool_version,
        inputs,
        constants,
        curves,
        outputs,
        created_by: calculated_by,
        created_at,
        ..
    } = run;
    let calculated_at = created_at.with_timezone(&chrono::Utc);

    // The run resolved its station properties and same-event reads for one visit, and those
    // resolutions travel into the blob below. Saving it anywhere else would file a number computed
    // from one site's properties as another site's measurement, so the context the run recorded is
    // held to the save. A context-free run is a draft and stays saveable anywhere.
    if let Some(context) = run_context.as_ref().filter(|c| !c.is_null()) {
        if let Some(run_site) = context
            .get("site_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
            && run_site != site_id
        {
            return Err(AppError::BadRequest(format!(
                "This {tool_name} run was calculated for site {run_site}; saving it at site \
                 {site_id} would file its resolved station inputs as another station's"
            )));
        }
        if let Some(run_time) = context
            .get("collected_at")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&chrono::Utc))
            && let Some(r) = readings.iter().find(|r| r.time != run_time)
        {
            return Err(AppError::BadRequest(format!(
                "This {tool_name} run was calculated for the visit at {run_time}; a reading at \
                 {} belongs to another visit",
                r.time
            )));
        }
    }

    // An output that reduces a replicates param is a display of the group the save is about to
    // write, not a measurement of its own. Storing it would put a mean in the readings the
    // `samples` trigger then takes a mean over. The manifest is the run's own pinned version.
    let pinned = run_pinned_manifest(db, run_id).await?;
    let aggregate_outputs: std::collections::HashSet<String> = pinned
        .as_ref()
        .map(|m| {
            m.outputs
                .iter()
                .filter(|o| o.aggregate_of.is_some())
                .map(|o| o.key.clone())
                .collect()
        })
        .unwrap_or_default();
    // The params the manifest fills from the visit's own readings. A numeric one of those is a
    // measurement the operator may type over, and the save then corrects it (Q182); every other
    // numeric param is a run-only setting and has nowhere to be written.
    let event_input_params: std::collections::HashSet<String> = pinned
        .as_ref()
        .map(|m| m.event_inputs.iter().map(|e| e.param.clone()).collect())
        .unwrap_or_default();
    // The parameter each saved name is a measurement of, as the manifest binds it: a replicates
    // param's code, an event input's code, an output's catalog reference.
    let (bound_inputs, bound_outputs) = manifest_bindings(db, pinned.as_ref()).await?;

    let run_applied_curves = curves.as_array().is_some_and(|c| !c.is_empty());
    let mut saved: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    let mut saved_inputs: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    for r in readings {
        if let Some(input) = r.input.as_deref() {
            if r.output.is_some() {
                return Err(AppError::BadRequest(format!(
                    "Reading for parameter {} names both an input and an output of the run",
                    r.parameter_id
                )));
            }
            check_saved_input(
                &tool_name,
                &inputs,
                &event_input_params,
                input,
                r.value,
                r.replicate_index,
            )
            .map_err(AppError::BadRequest)?;
            let bound = bound_inputs
                .get(input)
                .map(|(id, code)| (*id, code.as_str()));
            check_bound_parameter(&tool_name, "input", input, bound, r.parameter_id)
                .map_err(AppError::BadRequest)?;
            match saved_inputs.get(input) {
                Some(existing) if existing != &serde_json::json!(r.parameter_id) => {
                    return Err(AppError::BadRequest(format!(
                        "Input '{input}' is saved to two different parameters in one request"
                    )));
                }
                _ => {
                    saved_inputs.insert(input.to_string(), serde_json::json!(r.parameter_id));
                }
            }
            continue;
        }
        let Some(output) = r.output.as_deref() else {
            return Err(AppError::BadRequest(format!(
                "Reading for parameter {} at {} names neither the tool output nor the input it \
                 stores; every reading of a tool-run save must",
                r.parameter_id, r.time
            )));
        };
        let Some(output_value) = outputs.get(output) else {
            return Err(AppError::BadRequest(format!(
                "'{output}' is not an output of this {tool_name} run"
            )));
        };
        if aggregate_outputs.contains(output) {
            return Err(AppError::BadRequest(format!(
                "'{output}' is a statistic of this {tool_name} run's replicates, not a \
                 measurement; save the replicates it reduces and the sample statistics follow"
            )));
        }
        if !output_carries_value(output_value, r.value) {
            return Err(AppError::BadRequest(format!(
                "Value {} is not what this {tool_name} run produced for '{output}'; the \
                 provenance would claim a number the tool did not compute",
                r.value
            )));
        }
        let bound = bound_outputs
            .get(output)
            .map(|(id, code)| (*id, code.as_str()));
        check_bound_parameter(&tool_name, "output", output, bound, r.parameter_id)
            .map_err(AppError::BadRequest)?;
        if run_applied_curves && r.standard_curve_id.is_some() {
            return Err(AppError::BadRequest(format!(
                "'{output}' was computed with a standard curve already applied; stamping \
                 standard_curve_id would have the correction applied twice"
            )));
        }
        match saved.get(output) {
            Some(existing) if existing != &serde_json::json!(r.parameter_id) => {
                return Err(AppError::BadRequest(format!(
                    "Output '{output}' is saved to two different parameters in one request"
                )));
            }
            _ => {
                saved.insert(output.to_string(), serde_json::json!(r.parameter_id));
            }
        }
    }

    let mut blob = serde_json::json!({
        "tool": tool_name,
        "tool_version": tool_version,
        "inputs": inputs,
        "constants": constants,
        "curves": curves,
        "outputs": outputs,
        "saved": saved,
        "saved_inputs": saved_inputs,
        "run_id": run_id,
        // D15: which path the numbers travelled. A tool linked here always actually ran; a CSV
        // that carried already-computed values gets no blob at all.
        "source": if run_source == "csv_import" { "csv_import" } else { "tool_run" },
        "calculated_by": calculated_by,
        "calculated_at": calculated_at,
        "saved_by": saved_by,
        "saved_at": chrono::Utc::now(),
    });
    // The run's resolved calculation context (station properties, same-event reads) travels into
    // the blob, so an auditor reads where each resolved value came from without the run row.
    if let Some(context) = run_context
        && !serde_json::Value::is_null(&context)
        && let Some(map) = blob.as_object_mut()
    {
        map.insert("context".to_string(), context);
    }
    Ok(Some(blob))
}

/// One review-queue row per slot instant an intern entered, so a manager sees the pending entry
/// beside every other finding. Keyed on (site, parameter, instant): a re-entry at the same slot
/// refreshes the open hold rather than filing a second one.
pub async fn open_unverified_holds<C: sea_orm::ConnectionTrait>(
    conn: &C,
    site_id: Uuid,
    groups: &[(Uuid, chrono::DateTime<chrono::Utc>)],
    actor: &str,
) -> AppResult<()> {
    for (parameter_id, at) in groups {
        let updated = crate::routes::private::sync::hold_model::Entity::update_many()
            .col_expr(
                crate::routes::private::sync::hold_model::Column::Computed,
                Expr::val(serde_json::json!({ "state": "unverified", "entered_by": actor })),
            )
            .col_expr(
                crate::routes::private::sync::hold_model::Column::CreatedAt,
                Expr::cust("NOW()"),
            )
            .filter(crate::routes::private::sync::hold_model::Column::SiteId.eq(site_id))
            .filter(crate::routes::private::sync::hold_model::Column::ParameterId.eq(*parameter_id))
            .filter(crate::routes::private::sync::hold_model::Column::GroupTime.eq(*at))
            .filter(
                crate::routes::private::sync::hold_model::Column::Kind
                    .eq(HoldKind::UnverifiedEntry.as_str()),
            )
            .filter(
                crate::routes::private::sync::hold_model::Column::Status
                    .is_in(crate::routes::private::sync::service::open_statuses()),
            )
            .exec(conn)
            .await?
            .rows_affected;
        if updated > 0 {
            continue;
        }
        // Two saves at one slot instant see no row to update and both insert; the conflict
        // clause makes the loser refresh the hold instead of failing its whole save.
        audit::upsert_hold(
            conn,
            &audit::Hold {
                key: audit::HoldKey::Slot {
                    site_id,
                    parameter_id: *parameter_id,
                    group_time: *at,
                },
                kind: HoldKind::UnverifiedEntry,
                expected: serde_json::json!({ "state": "verified" }),
                computed: serde_json::json!({ "state": "unverified", "entered_by": actor }),
                delta: serde_json::json!({}),
                status: HoldStatus::Pending,
                tool: None,
            },
        )
        .await?;
    }
    Ok(())
}

/// The `source` of a stored tool run: `interactive` | `csv_import` | `chain`. `None` when the
/// request names no run.
pub(super) async fn tool_run_source(
    db: &DatabaseConnection,
    tool_run_id: Option<Uuid>,
) -> AppResult<Option<String>> {
    let Some(run_id) = tool_run_id else {
        return Ok(None);
    };
    Ok(tool_run::Entity::find_by_id(run_id)
        .select_only()
        .column(tool_run::Column::Source)
        .into_tuple::<String>()
        .one(db)
        .await?)
}

/// The instrument a hand-entered or calculated reading names, declared and never implied.
///
/// The request's own instrument wins: it is the one the operator's chosen curve was fitted on, and
/// it applies to the rows that curve corrected. Absent that, the slot's declaration
/// (`site_parameters.instrument_sensor_id`) says what measures this parameter at this site. The
/// entry channel's own instrument is the last resort and is a marker for a slot nobody has
/// declared, so it is added at the write rather than resolving a curve or a calibration.
#[must_use]
pub fn declared_instrument(explicit: Option<Uuid>, slot: Option<Uuid>) -> Option<Uuid> {
    explicit.or(slot)
}

// --- The grab save, one step at a time ---

/// A (parameter, instant) group a grab save writes.
pub(super) type GrabGroup = (Uuid, DateTime<Utc>);

/// A save carries at least one reading.
pub(super) fn require_grab_readings(readings: &[GrabSampleReading]) -> AppResult<()> {
    if readings.is_empty() {
        return Err(AppError::BadRequest("No readings provided".to_string()));
    }
    Ok(())
}

/// Every reading is admitted as a spot value.
pub(super) fn admit_grab_readings(readings: &[GrabSampleReading]) -> AppResult<()> {
    for r in readings {
        admission::admit(r.time, r.value, Some(GRAB_MEASUREMENT_TYPE))?;
    }
    Ok(())
}

/// The site a save names.
pub(super) async fn find_grab_site(
    db: &DatabaseConnection,
    site_id: Uuid,
) -> AppResult<sites::Model> {
    sites::Entity::find_by_id(site_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Site {site_id} not found")))
}

/// The site's slots for the parameters a save names: which slot each parameter is, and what each
/// slot declares measures it.
pub(super) struct GrabSlots {
    configured: HashSet<Uuid>,
    slot_ids: HashMap<Uuid, Uuid>,
    /// A slot that declares nothing is undeclared, not a reason to borrow another row's instrument.
    instruments: HashMap<Uuid, Uuid>,
}

impl GrabSlots {
    pub(super) fn of(site_params: &[site_parameters::models::Model]) -> Self {
        Self {
            configured: site_params.iter().map(|sp| sp.parameter_id).collect(),
            slot_ids: site_params
                .iter()
                .map(|sp| (sp.parameter_id, sp.id))
                .collect(),
            instruments: site_params
                .iter()
                .filter_map(|sp| sp.instrument_sensor_id.map(|sid| (sp.parameter_id, sid)))
                .collect(),
        }
    }

    /// The instrument a reading names, its own or its slot's.
    pub(super) fn instrument_of(&self, r: &GrabSampleReading) -> Option<Uuid> {
        declared_instrument(r.sensor_id, self.instruments.get(&r.parameter_id).copied())
    }

    /// A hand save is held to the slots the site carries: one landing on a slot the site does not
    /// carry is refused rather than minting one, because a mint here would create the declaration
    /// it is meant to be checked against (Q98, kept by Q193).
    pub(super) fn require_configured(
        &self,
        site: &sites::Model,
        readings: &[GrabSampleReading],
    ) -> AppResult<()> {
        match readings
            .iter()
            .find(|r| !self.configured.contains(&r.parameter_id))
        {
            Some(r) => Err(AppError::BadRequest(format!(
                "Parameter {} is not configured for site {}; add its parameter group to the site \
                 first (POST /api/sites/{}/parameter_groups)",
                r.parameter_id, site.name, site.id
            ))),
            None => Ok(()),
        }
    }
}

/// The site's slots for the parameters a save names.
pub(super) async fn load_grab_slots(
    db: &DatabaseConnection,
    site_id: Uuid,
    readings: &[GrabSampleReading],
) -> AppResult<GrabSlots> {
    let param_ids: Vec<Uuid> = readings.iter().map(|r| r.parameter_id).collect();
    let site_params = site_parameters::Entity::find()
        .filter(site_parameters::Column::SiteId.eq(site_id))
        .filter(site_parameters::Column::ParameterId.is_in(param_ids))
        .all(db)
        .await?;
    Ok(GrabSlots::of(&site_params))
}

/// A save that names a seasonal check is held to it: every (parameter, value) pair must have been
/// screened by exactly that check.
pub(super) async fn require_checked_values(
    db: &DatabaseConnection,
    check_id: Option<Uuid>,
    site_id: Uuid,
    readings: &[GrabSampleReading],
) -> AppResult<()> {
    let Some(check_id) = check_id else {
        return Ok(());
    };
    let pairs: Vec<(Uuid, f64)> = readings.iter().map(|r| (r.parameter_id, r.value)).collect();
    validate_check_claim(db, check_id, site_id, &pairs).await
}

/// What the operator picked, held to the same rule as a slot's declaration and a deployment: a
/// bookkeeping row records that nothing was declared, and a retired instrument is not in the lab.
/// The slot's own declaration is guarded where it is set, so only the request's pick is checked.
pub(super) async fn require_picked_instruments(
    db: &DatabaseConnection,
    readings: &[GrabSampleReading],
) -> AppResult<()> {
    let picked: Vec<Uuid> = readings.iter().filter_map(|r| r.sensor_id).collect();
    sensors::service::require_measuring_instruments(
        db,
        &picked,
        "named as what measured a grab sample",
    )
    .await
}

/// The chosen standard curves, admitted by the one rule every writer of `standard_curve_id` uses.
/// A grab is spot by construction, so the only claims this path can be refused for are an unknown
/// id, a curve fitted on another instrument, and a curve on a grab that names no instrument.
pub(super) async fn admit_grab_curves(
    db: &DatabaseConnection,
    readings: &[GrabSampleReading],
    slots: &GrabSlots,
) -> AppResult<HashMap<Uuid, standard_curves::Model>> {
    let claims: Vec<CurveClaim<'_>> = readings
        .iter()
        .filter_map(|r| {
            r.standard_curve_id.map(|id| CurveClaim {
                standard_curve_id: id,
                sensor_id: slots.instrument_of(r),
                measurement_type: GRAB_MEASUREMENT_TYPE,
            })
        })
        .collect();
    admit_standard_curves(db, &claims).await
}

/// The base calibration covering each grab that names an instrument, ranked by the one resolver
/// the ingest and reprocess paths use. Resolving it here is what lets the row carry both the id
/// and the value that id produced.
pub(super) async fn resolve_grab_calibrations(
    db: &DatabaseConnection,
    readings: &[GrabSampleReading],
    slots: &GrabSlots,
) -> AppResult<HashMap<(Uuid, Option<Uuid>, DateTime<Utc>), sensor_calibrations::service::Curve>> {
    let requests: Vec<(Uuid, Option<Uuid>, DateTime<Utc>)> = readings
        .iter()
        .filter_map(|r| {
            slots
                .instrument_of(r)
                .map(|sid| (sid, Some(r.parameter_id), r.time))
        })
        .collect();
    sensor_calibrations::resolver::resolve_many(db, &requests).await
}

/// Each reading as it would be stored: its replicate index, both curves and the value they serve.
/// The same numbers serve the dry-run preview, the conflict report and the write.
pub(super) fn grab_preview(
    readings: &[GrabSampleReading],
    indices: &[i16],
    slots: &GrabSlots,
    base_curves: &HashMap<(Uuid, Option<Uuid>, DateTime<Utc>), sensor_calibrations::service::Curve>,
    standard_curves: &HashMap<Uuid, standard_curves::Model>,
) -> Vec<GrabPreview> {
    readings
        .iter()
        .zip(indices)
        .map(|(r, &replicate_index)| {
            let base = slots
                .instrument_of(r)
                .and_then(|sid| base_curves.get(&(sid, Some(r.parameter_id), r.time)))
                .copied();
            let standard = r.standard_curve_id.map(|cid| {
                let c = &standard_curves[&cid];
                sensor_calibrations::service::Curve {
                    id: c.id,
                    slope: c.slope,
                    intercept: c.intercept,
                }
            });
            // Both corrections, in the one order the arithmetic is defined in: the instrument's
            // base calibration, then the operator's standard curve on that result. A grab that
            // resolves neither is stored uncorrected, and `calibrated_value` stays NULL so a null
            // still means "no curve was applied" rather than "a curve happened to be identity".
            let calibrated_value = (base.is_some() || standard.is_some())
                .then(|| sensor_calibrations::service::apply_curves(r.value, base, standard));
            let composed_equation = match (base, standard) {
                (Some(b), Some(s)) => Some(equation(
                    s.slope * b.slope,
                    s.slope * b.intercept + s.intercept,
                )),
                _ => None,
            };
            GrabPreview {
                parameter_id: r.parameter_id,
                time: r.time,
                replicate_index,
                raw_value: r.value,
                base_calibration: base.map(|c| CurveApplication {
                    id: c.id,
                    name: None,
                    slope: c.slope,
                    intercept: c.intercept,
                    equation: equation(c.slope, c.intercept),
                }),
                standard_curve: standard.map(|c| CurveApplication {
                    id: c.id,
                    name: standard_curves[&c.id].name.clone(),
                    slope: c.slope,
                    intercept: c.intercept,
                    equation: equation(c.slope, c.intercept),
                }),
                composed_equation,
                calibrated_value,
            }
        })
        .collect()
}

/// The (parameter, instant) groups a save writes, once each, in the order they first appear.
pub(super) fn grab_groups(readings: &[GrabSampleReading]) -> Vec<GrabGroup> {
    let mut seen = HashSet::new();
    readings
        .iter()
        .filter(|r| seen.insert((r.parameter_id, r.time)))
        .map(|r| (r.parameter_id, r.time))
        .collect()
}

/// Who is writing: the chain's own save is the recompute, anything else a person.
pub(super) fn grab_writer(run_source: Option<&str>) -> flows::Writer {
    match run_source {
        Some("chain") => flows::Writer::Chain,
        _ => flows::Writer::Person,
    }
}

/// Which calculations this save feeds, known before anything is written. The chain's own save is
/// the recompute: it reports nothing.
pub(super) async fn calculations_fed_by_grab(
    db: &DatabaseConnection,
    writer: flows::Writer,
    readings: &[GrabSampleReading],
) -> AppResult<Vec<crate::routes::private::tools::models::CalculationImpact>> {
    if writer == flows::Writer::Chain {
        return Ok(Vec::new());
    }
    let mut touched: Vec<Uuid> = readings.iter().map(|r| r.parameter_id).collect();
    touched.sort_unstable();
    touched.dedup();
    crate::routes::private::tools::service::calculations_fed_by(db, &touched).await
}

/// What a dry run answers: the preview and what is stored, with nothing written.
pub(super) fn grab_dry_run(
    preview: Vec<GrabPreview>,
    existing_groups: Vec<ExistingGroup>,
    calculations: Vec<crate::routes::private::tools::models::CalculationImpact>,
) -> GrabSampleResponse {
    GrabSampleResponse {
        inserted: 0,
        samples_created: 0,
        created_sample_ids: vec![],
        dry_run: true,
        replaced: 0,
        kept_curated: 0,
        withdrawn: 0,
        preview,
        existing_groups,
        calculations,
    }
}

/// Each reading as `(parameter, instant, replicate index, value)`, the key the stored groups are
/// compared on.
pub(super) fn carried_replicates(
    readings: &[GrabSampleReading],
    preview: &[GrabPreview],
) -> Vec<(Uuid, DateTime<Utc>, i16, f64)> {
    readings
        .iter()
        .zip(preview)
        .map(|(r, p)| (r.parameter_id, r.time, p.replicate_index, r.value))
        .collect()
}

/// An intern enters measurements; a stored value is someone else's to change (Q21). A replace that
/// carries every stored replicate at the number it already holds changes none of them: the entry
/// grid posts the whole group, so a repeat typed into an empty cell arrives this way.
pub(super) fn refuse_intern_rewrite(
    role: Option<&crate::common::authz::Role>,
    mode: Option<GrabWriteMode>,
    carried: &[(Uuid, DateTime<Utc>, i16, f64)],
    existing_groups: &[ExistingGroup],
) -> AppResult<()> {
    if entry_state(role).is_none() || mode != Some(GrabWriteMode::Replace) {
        return Ok(());
    }
    let moved = stored_values_moved(carried, existing_groups);
    if moved > 0 {
        return Err(AppError::Forbidden(format!(
            "An intern's entry cannot replace stored values; a manager rewrites them \
             ({moved} stored replicate(s) would move)"
        )));
    }
    Ok(())
}

/// A save built from a stale read would retract a repeat added under it, so a client that says
/// what it read is refused when a group no longer holds that.
pub(super) fn refuse_stale_read(
    expected: Option<&[ExpectedGroup]>,
    existing_groups: &[ExistingGroup],
) -> AppResult<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let changed = groups_changed(expected, existing_groups);
    if changed.is_empty() {
        return Ok(());
    }
    let detail =
        serde_json::to_value(existing_groups).map_err(|e| AppError::Internal(e.to_string()))?;
    Err(AppError::ConflictDetail {
        message: format!(
            "{} replicate group(s) changed since they were read; re-read the visit and save again",
            changed.len()
        ),
        detail,
    })
}

/// A save landing on stored groups rewrites them only when it says so.
pub(super) fn refuse_unasked_replace(
    mode: Option<GrabWriteMode>,
    existing_groups: &[ExistingGroup],
) -> AppResult<()> {
    if existing_groups.is_empty() || mode == Some(GrabWriteMode::Replace) {
        return Ok(());
    }
    let detail =
        serde_json::to_value(existing_groups).map_err(|e| AppError::Internal(e.to_string()))?;
    Err(AppError::ConflictDetail {
        message: format!(
            "{} replicate group(s) are already stored at the requested times; pass mode \
             \"replace\" to rewrite them",
            existing_groups.len()
        ),
        detail,
    })
}

/// The grab stream each parameter's readings land on, created on first use.
pub(super) async fn grab_streams(
    db: &DatabaseConnection,
    site_id: Uuid,
    readings: &[GrabSampleReading],
    slots: &GrabSlots,
) -> AppResult<HashMap<Uuid, Uuid>> {
    let mut streams: HashMap<Uuid, Uuid> = HashMap::new();
    for r in readings {
        if let Entry::Vacant(entry) = streams.entry(r.parameter_id) {
            let sp_id = slots.slot_ids.get(&r.parameter_id).copied();
            entry.insert(get_or_create_grab_stream(db, site_id, r.parameter_id, sp_id).await?);
        }
    }
    Ok(streams)
}

/// The channel instrument each grab stream carries, which a reading naming no instrument of its
/// own is attributed to. A hand-entered value still records what produced it.
pub(super) async fn grab_stream_instruments(
    db: &DatabaseConnection,
    streams: &HashMap<Uuid, Uuid>,
) -> AppResult<HashMap<Uuid, Uuid>> {
    let ids: Vec<Uuid> = streams.values().copied().collect();
    let rows = data_streams::models::Entity::find()
        .filter(data_streams::models::Column::Id.is_in(ids))
        .filter(data_streams::models::Column::SensorId.is_not_null())
        .all(db)
        .await?;
    Ok(rows
        .into_iter()
        .filter_map(|stream| stream.sensor_id.map(|sensor_id| (stream.id, sensor_id)))
        .collect())
}

/// Window-aware attribution for grabs that name an instrument: the deployment it was on at the
/// grab time, fixed to the save's site. A grab naming no instrument keeps no deployment.
pub(super) async fn grab_deployments(
    db: &DatabaseConnection,
    site_id: Uuid,
    readings: &[GrabSampleReading],
    slots: &GrabSlots,
) -> HashMap<(Uuid, Uuid, DateTime<Utc>), sensors::models::ResolvedSlot> {
    let mut times_by_channel: HashMap<(Uuid, Uuid), Vec<DateTime<Utc>>> = HashMap::new();
    for r in readings {
        if let Some(sid) = slots.instrument_of(r) {
            times_by_channel
                .entry((sid, r.parameter_id))
                .or_default()
                .push(r.time);
        }
    }
    let mut resolved_slots = HashMap::new();
    for ((sid, pid), times) in &times_by_channel {
        let resolved =
            sensors::service::resolve_windows_for_times(db, *sid, Some(site_id), Some(*pid), times)
                .await
                .unwrap_or_default();
        for (t, slot) in resolved {
            resolved_slots.insert((*sid, *pid, t), slot);
        }
    }
    resolved_slots
}

/// The span of instants a save wrote, for the alarm episodes it reconstructs.
pub(super) fn grab_span(readings: &[GrabSampleReading]) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let lo = readings.iter().map(|r| r.time).min()?;
    let hi = readings.iter().map(|r| r.time).max()?;
    Some((lo, hi))
}

/// What a grab save's transaction writes, and what it returns.
pub(super) struct GrabWrite<'a> {
    pub(super) payload: &'a GrabSampleRequest,
    pub(super) preview: &'a [GrabPreview],
    pub(super) groups: &'a [GrabGroup],
    pub(super) existing_groups: &'a [ExistingGroup],
    pub(super) carried: &'a [(Uuid, DateTime<Utc>, i16, f64)],
    pub(super) slots: &'a GrabSlots,
    pub(super) streams: &'a HashMap<Uuid, Uuid>,
    pub(super) stream_instruments: &'a HashMap<Uuid, Uuid>,
    pub(super) deployments: &'a HashMap<(Uuid, Uuid, DateTime<Utc>), sensors::models::ResolvedSlot>,
    pub(super) writer: flows::Writer,
    pub(super) actor: &'a str,
    pub(super) provenance: Option<&'a serde_json::Value>,
    pub(super) provenance_kind: &'a str,
    pub(super) entry_state: Option<Kind>,
}

/// What a grab save's transaction did.
pub(super) struct GrabWritten {
    pub(super) inserted: usize,
    pub(super) replaced: usize,
    pub(super) kept_curated: usize,
    pub(super) withdrawn: usize,
    pub(super) created_sample_ids: Vec<Uuid>,
    pub(super) touched_events: Vec<TouchedEvent>,
}

/// What a replace does to the rows it rewrites.
pub(super) struct ReplaceOutcome {
    replaced: usize,
    kept_curated: usize,
    withdrawn: usize,
}

impl GrabWrite<'_> {
    fn replaces(&self) -> bool {
        self.payload.mode == Some(GrabWriteMode::Replace)
    }

    /// The readings of one group, with their preview.
    fn in_group(
        &self,
        (parameter_id, time): GrabGroup,
    ) -> impl Iterator<Item = (&GrabSampleReading, &GrabPreview)> {
        self.payload
            .readings
            .iter()
            .zip(self.preview)
            .filter(move |(r, _)| r.parameter_id == parameter_id && r.time == time)
    }

    /// Whether the save names a curve for this group, which decides which stored rows it may
    /// rewrite: a hand-curved row is the save's only when the save brings a curve of its own.
    fn supplies_curve(&self, group: GrabGroup) -> bool {
        self.in_group(group)
            .any(|(_, p)| p.standard_curve.is_some())
    }

    /// The save, in one guarded transaction.
    pub(super) async fn run(&self, txn: &sea_orm::DatabaseTransaction) -> AppResult<GrabWritten> {
        let prior_facts = if self.replaces() {
            self.record_replace_decisions(txn).await?;
            self.prior_facts(txn).await?
        } else {
            HashMap::new()
        };
        let outcome = if self.replaces() {
            self.hold_kept_and_withdraw_dropped(txn).await?
        } else {
            ReplaceOutcome {
                replaced: 0,
                kept_curated: 0,
                withdrawn: 0,
            }
        };
        let stored_facts = self.stored_facts(&prior_facts);
        let models = self.models(&stored_facts);
        let inserted = self.insert(txn, &models).await?;
        record_curve_claims(txn, &models, self.actor, Origin::Manual).await?;
        self.record_intern_entries(txn, &models).await?;
        self.restamp_facts(txn, &stored_facts).await?;
        let created_sample_ids =
            materialise_grab_samples(txn, self.groups, self.payload.site_id).await?;
        let touched_events = self.attach_visits(txn).await?;
        Ok(GrabWritten {
            inserted,
            replaced: outcome.replaced,
            kept_curated: outcome.kept_curated,
            withdrawn: outcome.withdrawn,
            created_sample_ids,
            touched_events,
        })
    }

    /// What the replace rewrites is decided before the rows go: a person's correction of a stored
    /// value, or the chain superseding an output with a fresh run (ADR 0008). Rows whose value does
    /// not change decide nothing, and a flagged, withdrawn or hand-curved row stays as it is (SB5)
    /// and gets a hold, not a correction.
    async fn record_replace_decisions(&self, txn: &sea_orm::DatabaseTransaction) -> AppResult<()> {
        let (kind, origin, reason) = match self.writer {
            flows::Writer::Chain => (Kind::Chain, Origin::Chain, "superseded by a recompute"),
            flows::Writer::Person => (
                Kind::ValueCorrection,
                Origin::Manual,
                "replaced by a new entry",
            ),
        };
        for &group in self.groups {
            let rows: Vec<(DateTime<Utc>, i16, serde_json::Value)> = self
                .in_group(group)
                .map(|(_, p)| {
                    let new = match (self.writer, self.payload.tool_run_id) {
                        (flows::Writer::Chain, Some(run_id)) => {
                            serde_json::json!({ "run_id": run_id })
                        }
                        _ => serde_json::json!({ "raw_value": p.raw_value }),
                    };
                    (group.1, p.replicate_index, new)
                })
                .collect();
            let guard = if self.supplies_curve(group) {
                "r.is_flagged IS NOT TRUE AND r.withdrawn_at IS NULL"
            } else {
                "r.is_flagged IS NOT TRUE AND r.withdrawn_at IS NULL \
                 AND r.standard_curve_id IS NULL"
            };
            record_keyed(
                txn,
                kind,
                self.streams[&group.0],
                &rows,
                self.actor,
                Some(reason),
                origin,
                Keyed::Changed,
                Some(guard),
                None,
            )
            .await?;
        }
        Ok(())
    }

    /// The label, notes, authorship and blob each rewritten group carries, kept on the rewritten
    /// rows wherever the request does not carry its own.
    async fn prior_facts(
        &self,
        txn: &sea_orm::DatabaseTransaction,
    ) -> AppResult<HashMap<GrabGroup, StoredFacts>> {
        let mut prior = HashMap::new();
        for &(parameter_id, time) in self.groups {
            if let Some(row) = readings::Entity::find()
                .select_only()
                .column(readings::Column::Label)
                .column(readings::Column::Notes)
                .column(readings::Column::CreatedBy)
                .column(readings::Column::Provenance)
                .column(readings::Column::ProvenanceKind)
                .filter(readings::Column::SiteId.eq(self.payload.site_id))
                .filter(readings::Column::ParameterId.eq(parameter_id))
                .filter(readings::Column::Time.eq(time))
                .filter(readings::Column::MeasurementType.eq("spot"))
                .filter(
                    Condition::any()
                        .add(readings::Column::Label.is_not_null())
                        .add(readings::Column::Notes.is_not_null())
                        .add(readings::Column::CreatedBy.is_not_null())
                        .add(readings::Column::Provenance.is_not_null()),
                )
                .order_by_asc(readings::Column::ReplicateIndex)
                .into_model::<PriorFactsRow>()
                .one(txn)
                .await?
            {
                prior.insert((parameter_id, time), row.into());
            }
        }
        Ok(prior)
    }

    /// The rewrite is scoped to the grab stream: another source's rows at the same instant are not
    /// this request's to rewrite. Curation wins, as in the windowed diff: a flagged, withdrawn or
    /// hand-curved row stays and the disagreement lands in the review queue. What the save leaves
    /// out is retracted, never deleted.
    async fn hold_kept_and_withdraw_dropped(
        &self,
        txn: &sea_orm::DatabaseTransaction,
    ) -> AppResult<ReplaceOutcome> {
        let mut removed: u64 = 0;
        let mut kept_curated: usize = 0;
        let mut withdrawn: usize = 0;
        for &group in self.groups {
            let (parameter_id, time) = group;
            let stream_id = self.streams[&parameter_id];
            let supplies_curve = self.supplies_curve(group);
            kept_curated += hold_kept_rows(txn, stream_id, time, supplies_curve).await?;
            let carried: Vec<i16> = self
                .in_group(group)
                .map(|(_, p)| p.replicate_index)
                .collect();
            withdrawn +=
                withdraw_uncarried(txn, stream_id, time, &carried, supplies_curve, self.actor)
                    .await?;
            removed += count_rewritten(txn, stream_id, time, carried, supplies_curve).await?;
        }
        Ok(ReplaceOutcome {
            replaced: usize::try_from(removed).unwrap_or(usize::MAX),
            kept_curated,
            withdrawn,
        })
    }

    /// What each row records about the measurement, request first and the rewritten group's own
    /// prior values where the request is silent.
    fn stored_facts(
        &self,
        prior_facts: &HashMap<GrabGroup, StoredFacts>,
    ) -> HashMap<GrabGroup, StoredFacts> {
        let facts = GrabFacts {
            created_by: Some(self.actor),
            label: self.payload.label.as_deref(),
            notes: self.payload.notes.as_deref(),
            provenance: self.provenance,
            kind: self.provenance_kind,
        };
        self.groups
            .iter()
            .map(|group| (*group, facts.over(prior_facts.get(group))))
            .collect()
    }

    /// The row each reading is stored as.
    fn models(&self, stored_facts: &HashMap<GrabGroup, StoredFacts>) -> Vec<readings::ActiveModel> {
        self.payload
            .readings
            .iter()
            .zip(self.preview)
            .map(|(r, p)| {
                let instrument = self.slots.instrument_of(r);
                let stream_id = self.streams[&r.parameter_id];
                let facts = &stored_facts[&(r.parameter_id, r.time)];
                readings::ActiveModel {
                    standard_curve_id: Set(p.standard_curve.as_ref().map(|c| c.id)),
                    site_id: Set(Some(self.payload.site_id)),
                    parameter_id: Set(Some(r.parameter_id)),
                    calibrated_value: Set(p.calibrated_value),
                    sensor_id: Set(
                        instrument.or_else(|| self.stream_instruments.get(&stream_id).copied())
                    ),
                    calibration_id: Set(p.base_calibration.as_ref().map(|c| c.id)),
                    deployment_id: Set(instrument.and_then(|sid| {
                        self.deployments
                            .get(&(sid, r.parameter_id, r.time))
                            .and_then(|s| s.deployment_id)
                    })),
                    measurement_type: Set(Some(GRAB_MEASUREMENT_TYPE.to_string())),
                    label: Set(facts.label.clone()),
                    notes: Set(facts.notes.clone()),
                    created_by: Set(facts.created_by.clone()),
                    provenance: Set(facts.provenance.clone()),
                    provenance_kind: Set(facts.kind.clone()),
                    ..readings::new(stream_id, r.time.into(), p.replicate_index, r.value)
                }
            })
            .collect()
    }

    /// A replace rewrites each carried row in place, guarded per group by whether the save names a
    /// curve; any other save keeps the stored row.
    async fn insert(
        &self,
        txn: &sea_orm::DatabaseTransaction,
        models: &[readings::ActiveModel],
    ) -> AppResult<usize> {
        let curved_groups: HashSet<GrabGroup> = self
            .payload
            .readings
            .iter()
            .zip(self.preview)
            .filter(|(_, p)| p.standard_curve.is_some())
            .map(|(r, _)| (r.parameter_id, r.time))
            .collect();
        let mut inserted = 0usize;
        for curved in [false, true] {
            let batch: Vec<readings::ActiveModel> = self
                .payload
                .readings
                .iter()
                .zip(models)
                .filter(|(r, _)| curved_groups.contains(&(r.parameter_id, r.time)) == curved)
                .map(|(_, m)| m.clone())
                .collect();
            if batch.is_empty() {
                continue;
            }
            let replace = if self.replaces() {
                Replace::Entry { curved }
            } else {
                Replace::Nothing
            };
            inserted += match readings::Entity::insert_many(batch)
                .on_conflict(readings_upsert(replace))
                .exec_without_returning(txn)
                .await
            {
                Ok(rows) => usize::try_from(rows).unwrap_or(usize::MAX),
                Err(e) if e.to_string().contains("None of the records") => 0,
                Err(e) => return Err(AppError::Database(e)),
            };
        }
        Ok(inserted)
    }

    /// An intern's entry lands pending: the record carries it, the columns project it and the
    /// review queue lists it until a manager verifies or rejects (Q21, M44). Only what the save
    /// entered is pending: a stored replicate the grid carried at its own number stays as a
    /// manager left it.
    async fn record_intern_entries(
        &self,
        txn: &sea_orm::DatabaseTransaction,
        models: &[readings::ActiveModel],
    ) -> AppResult<()> {
        if self.entry_state != Some(Kind::UnverifiedEntry) {
            return Ok(());
        }
        let entered = entered_rows(self.carried, self.existing_groups);
        let entries: Vec<readings::ActiveModel> = models
            .iter()
            .zip(&entered)
            .filter(|(_, entered)| **entered)
            .map(|(m, _)| m.clone())
            .collect();
        let entered_groups: Vec<GrabGroup> = self
            .groups
            .iter()
            .filter(|group| {
                self.carried
                    .iter()
                    .zip(&entered)
                    .any(|((p, t, _, _), e)| *e && (*p, *t) == **group)
            })
            .copied()
            .collect();
        record_unverified_entries(txn, &entries, self.actor, Origin::Manual).await?;
        open_unverified_holds(txn, self.payload.site_id, &entered_groups, self.actor).await
    }

    /// A re-post is the same measurement recorded again: the rows the insert skipped on conflict
    /// still take this request's story, so a second run's blob does not sit behind the value it
    /// produced. Keyed on the rows this request wrote, so a curated row a replace left in place
    /// keeps the provenance of the run that made it.
    async fn restamp_facts(
        &self,
        txn: &sea_orm::DatabaseTransaction,
        stored_facts: &HashMap<GrabGroup, StoredFacts>,
    ) -> AppResult<()> {
        for (r, p) in self.payload.readings.iter().zip(self.preview) {
            let stored = &stored_facts[&(r.parameter_id, r.time)];
            if stored.is_empty() {
                continue;
            }
            let keep = |column: readings::Column, value: sea_orm::Value| {
                Expr::expr(Func::coalesce([Expr::val(value), Expr::col(column)]))
            };
            readings::Entity::update_many()
                .col_expr(
                    readings::Column::Label,
                    keep(readings::Column::Label, stored.label.clone().into()),
                )
                .col_expr(
                    readings::Column::Notes,
                    keep(readings::Column::Notes, stored.notes.clone().into()),
                )
                .col_expr(
                    readings::Column::CreatedBy,
                    keep(
                        readings::Column::CreatedBy,
                        stored.created_by.clone().into(),
                    ),
                )
                .col_expr(
                    readings::Column::Provenance,
                    keep(
                        readings::Column::Provenance,
                        stored.provenance.clone().into(),
                    ),
                )
                .filter(readings::Column::StreamId.eq(self.streams[&r.parameter_id]))
                .filter(readings::Column::Time.eq(r.time))
                .filter(readings::Column::ReplicateIndex.eq(p.replicate_index))
                .exec(txn)
                .await?;
        }
        Ok(())
    }

    /// Every attributed spot instant this request touched belongs to a collection event (D7); a
    /// hand-entered grab is a manual visit.
    async fn attach_visits(
        &self,
        txn: &sea_orm::DatabaseTransaction,
    ) -> AppResult<Vec<TouchedEvent>> {
        let (Some(lo), Some(hi)) = (
            self.groups.iter().map(|(_, t)| *t).min(),
            self.groups.iter().map(|(_, t)| *t).max(),
        ) else {
            return Ok(Vec::new());
        };
        let site_id = self.payload.site_id;
        collection_events::service::attach_collection_events(
            txn,
            Condition::all()
                .add(flows::row(readings::Column::SiteId).eq(site_id))
                .add(flows::row(readings::Column::Time).gte(DateTimeWithTimeZone::from(lo)))
                .add(flows::row(readings::Column::Time).lte(DateTimeWithTimeZone::from(hi))),
            collection_events::service::EventSource::Manual,
        )
        .await?;
        let mut instants: Vec<DateTimeWithTimeZone> = self
            .groups
            .iter()
            .map(|(_, t)| DateTimeWithTimeZone::from(*t))
            .collect();
        instants.sort_unstable();
        instants.dedup();
        flows::touched_events(
            txn,
            Condition::all()
                .add(flows::row(readings::Column::SiteId).eq(site_id))
                .add(flows::row(readings::Column::Time).is_in(instants)),
        )
        .await
    }
}

/// Hold a group's curated rows on the grab stream in the review queue: a replace leaves them in
/// place. Returns how many it kept.
async fn hold_kept_rows(
    txn: &sea_orm::DatabaseTransaction,
    stream_id: Uuid,
    time: DateTime<Utc>,
    supplies_curve: bool,
) -> AppResult<usize> {
    let kept = readings::Entity::find()
        .select_only()
        .column(readings::Column::ReplicateIndex)
        .column_as(kept_reason(), "reason")
        .filter(readings::Column::StreamId.eq(stream_id))
        .filter(readings::Column::Time.eq(time))
        .filter(readings::Column::MeasurementType.eq("spot"))
        .filter(curated_or_curved(supplies_curve))
        .order_by_asc(readings::Column::ReplicateIndex)
        .into_model::<KeptRow>()
        .all(txn)
        .await?;
    if kept.is_empty() {
        return Ok(0);
    }
    let entries = kept
        .iter()
        .map(|r| serde_json::json!({ "replicate_index": r.replicate_index, "reason": r.reason }))
        .collect::<Vec<_>>();
    upsert_source_modified_hold(
        txn,
        stream_id,
        time,
        serde_json::json!({ "claim": "replaced", "kept": entries }),
        serde_json::json!({ "kept": true }),
        HoldStatus::Pending,
    )
    .await?;
    Ok(kept.len())
}

/// Withdraw a group's replicates the save no longer carries. A cleared cell or a narrower pasted
/// block is a person saying the replicate is not part of the measurement any more, and the stamp
/// is reversible where a delete is not. Returns how many it withdrew.
async fn withdraw_uncarried(
    txn: &sea_orm::DatabaseTransaction,
    stream_id: Uuid,
    time: DateTime<Utc>,
    carried: &[i16],
    supplies_curve: bool,
    actor: &str,
) -> AppResult<usize> {
    use crate::routes::private::readings::models::Column;
    let mut cond = Condition::all()
        .add(flows::row(Column::StreamId).eq(stream_id))
        .add(flows::row(Column::Time).eq(DateTimeWithTimeZone::from(time)))
        .add(flows::row(Column::MeasurementType).eq("spot"))
        .add(flows::row(Column::ReplicateIndex).is_not_in(carried.to_vec()))
        .add(flows::row(Column::IsFlagged).is_not(sql_true()))
        .add(flows::row(Column::WithdrawnAt).is_null());
    if !supplies_curve {
        cond = cond.add(flows::row(Column::StandardCurveId).is_null());
    }
    let dropped = record_many(
        txn,
        Kind::Withdraw,
        cond,
        NewValue::Literal(
            serde_json::json!({ "reason": "the save no longer carries this replicate" }),
        ),
        actor,
        Some("dropped by a narrower entry"),
        Origin::Manual,
        None,
    )
    .await?;
    Ok(usize::try_from(dropped.rows).unwrap_or(usize::MAX))
}

/// The carried rows the insert's conflict clause rewrites in place, under the same guard, so what
/// no entry sets stays on them.
async fn count_rewritten(
    txn: &sea_orm::DatabaseTransaction,
    stream_id: Uuid,
    time: DateTime<Utc>,
    carried: Vec<i16>,
    supplies_curve: bool,
) -> AppResult<u64> {
    use sea_orm::PaginatorTrait as _;
    let mut query = readings::Entity::find()
        .filter(readings::Column::StreamId.eq(stream_id))
        .filter(readings::Column::Time.eq(time))
        .filter(readings::Column::MeasurementType.eq("spot"))
        .filter(readings::Column::ReplicateIndex.is_in(carried))
        .filter(Expr::col(readings::Column::IsFlagged).is_not(sql_true()))
        .filter(readings::Column::WithdrawnAt.is_null());
    if !supplies_curve {
        query = query.filter(readings::Column::StandardCurveId.is_null());
    }
    Ok(query.count(txn).await?)
}

/// Cap on the warning cells an import response lists.
pub(super) const IMPORT_CHECK_FINDINGS_CAP: usize = 200;

/// Screen the cells a spot or tool file will store, and decide what the request may do with the
/// result. `dry_run` stores the check and returns its id. A commit naming a check is validated
/// against it (every screened cell must be a checked pair, the grab save's rule); a commit
/// naming none is refused when any cell warns, with the findings in the 409 body.
pub(super) async fn screen_import(
    state: &AppState,
    auth: &crate::common::middleware::AuthContext,
    req: &ImportCsvRequest,
    site_id: Uuid,
    cells: &[(usize, Uuid, chrono::DateTime<chrono::Utc>, f64)],
) -> AppResult<ImportCheck> {
    let screened = screen_cells(&state.db, site_id, cells).await?;
    let mut seen: HashSet<(Uuid, u64)> = HashSet::new();
    let pairs: Vec<crate::routes::private::readings::models::SeasonalCheckValue> = screened
        .iter()
        .filter(|c| seen.insert((c.parameter_id, c.value.to_bits())))
        .map(
            |c| crate::routes::private::readings::models::SeasonalCheckValue {
                parameter_id: c.parameter_id,
                value: c.value,
            },
        )
        .collect();
    let warnings = screened.iter().filter(|c| c.warning).count();
    let findings: Vec<crate::routes::private::readings::models::ScreenedCell> = screened
        .into_iter()
        .filter(|c| c.warning)
        .take(IMPORT_CHECK_FINDINGS_CAP)
        .collect();
    let method = method();
    let check_id = if req.dry_run {
        let anchor = cells
            .iter()
            .map(|c| c.2)
            .min()
            .unwrap_or_else(chrono::Utc::now);
        Some(
            store_check(
                &state.db,
                site_id,
                anchor,
                &pairs,
                crate::common::actor::label(auth),
            )
            .await?,
        )
    } else if let Some(check_id) = req.check_id {
        let keyed: Vec<(Uuid, f64)> = pairs.iter().map(|p| (p.parameter_id, p.value)).collect();
        validate_check_claim(&state.db, check_id, site_id, &keyed).await?;
        Some(check_id)
    } else if warnings > 0 {
        return Err(AppError::ConflictDetail {
            message: format!(
                "{warnings} value(s) fall outside the site's seasonal range; preview the file \
                 (dry_run) and pass its check_id to import it as screened"
            ),
            detail: serde_json::json!({
                "screened": cells.len(),
                "warnings": warnings,
                "findings": findings,
                "method": method,
            }),
        });
    } else {
        None
    };
    Ok(ImportCheck {
        check_id,
        screened: cells.len(),
        warnings,
        findings,
        method,
    })
}

pub(crate) const BATCH_SIZE: usize = 1000;

/// Cap on the returned error list to keep responses bounded; `error_count` reports the true total.
pub(super) const MAX_ERRORS: usize = 500;

pub(super) struct ColumnMapping {
    pub(super) idx: usize,
    pub(super) header: String,
    pub(super) parameter_id: Uuid,
    /// public_value = stored_value * factor + offset, so stored = (value - offset) / factor.
    pub(super) conversion_factor: f64,
    pub(super) conversion_offset: f64,
}

/// The sites a multi-site file may name, by the two spellings a request-level `site` accepts.
pub(super) struct SiteLookup {
    pub(super) by_id: std::collections::HashSet<Uuid>,
    pub(super) by_name: HashMap<String, Uuid>,
}

impl SiteLookup {
    /// The site a cell names, or `None` for a spelling no site answers to.
    pub(super) fn resolve(&self, cell: &str) -> Option<Uuid> {
        let cell = cell.trim();
        if let Ok(id) = Uuid::parse_str(cell) {
            return self.by_id.contains(&id).then_some(id);
        }
        self.by_name.get(&cell.to_lowercase()).copied()
    }
}

/// The column a file names its rows' sites in: the one the caller declared, else a header called
/// `site_id` or `site`. A declared column the file does not carry is an error rather than a
/// silent single-site import, which is what wrote eleven sites' rows onto the twelfth.
pub(super) fn site_column_index(
    headers: &[&str],
    declared: Option<&str>,
) -> Result<Option<usize>, String> {
    if let Some(name) = declared {
        return headers
            .iter()
            .position(|h| h.eq_ignore_ascii_case(name))
            .map(Some)
            .ok_or_else(|| format!("site_column '{name}' is not a column of this file"));
    }
    Ok(headers
        .iter()
        .position(|h| h.eq_ignore_ascii_case("site_id") || h.eq_ignore_ascii_case("site")))
}

/// The site a row belongs to: the site its own cell names, else the request's target for an empty
/// cell. An unknown spelling belongs to no site and is reported against the row.
pub(super) fn resolve_row_site(
    cell: &str,
    lookup: &SiteLookup,
    fallback: Uuid,
) -> Result<Uuid, String> {
    if cell.trim().is_empty() {
        return Ok(fallback);
    }
    lookup
        .resolve(cell)
        .ok_or_else(|| format!("No site is named '{}'", cell.trim()))
}

/// Resolve a CSV timestamp cell to an instant. `tz_offset` is the zone the operator declared for
/// the file and applies to the naive forms only: an RFC 3339 timestamp already carries its offset
/// and is a resolved instant, so applying the declared zone to it would shift it a second time.
pub(super) fn parse_datetime(
    s: &str,
    tz_offset: chrono::Duration,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let s = s.trim();
    if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Some(ndt.and_utc() - tz_offset);
    }
    if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M") {
        return Some(ndt.and_utc() - tz_offset);
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&chrono::Utc));
    }
    None
}

/// One parsed CSV reading staged for the worker job, holding only the per-row fields.
pub(super) struct StagedRow {
    pub(super) stream_id: Uuid,
    pub(super) site_id: Uuid,
    pub(super) parameter_id: Uuid,
    pub(super) time: chrono::DateTime<chrono::Utc>,
    pub(super) raw_value: f64,
    pub(super) sensor_id: Option<Uuid>,
    pub(super) calibration_id: Option<Uuid>,
    pub(super) deployment_id: Option<Uuid>,
}

/// Bulk-insert the parsed rows into `csv_import_staging` under `import_token`, chunked so the
/// parameter count per statement stays bounded. `seq` records file order so the worker job can
/// number replicate groups deterministically. The worker job reads them back by token.
pub(super) async fn stage_import_rows<C: ConnectionTrait>(
    db: &C,
    import_token: Uuid,
    rows: &[StagedRow],
) -> AppResult<()> {
    for (chunk_index, chunk) in rows.chunks(BATCH_SIZE).enumerate() {
        let staged = chunk
            .iter()
            .enumerate()
            .map(|(i, r)| import_staging::ActiveModel {
                import_token: ActiveValue::Set(import_token),
                seq: ActiveValue::Set((chunk_index * BATCH_SIZE + i) as i64),
                stream_id: ActiveValue::Set(r.stream_id),
                site_id: ActiveValue::Set(Some(r.site_id)),
                parameter_id: ActiveValue::Set(Some(r.parameter_id)),
                time: ActiveValue::Set(r.time.into()),
                raw_value: ActiveValue::Set(r.raw_value),
                sensor_id: ActiveValue::Set(r.sensor_id),
                calibration_id: ActiveValue::Set(r.calibration_id),
                deployment_id: ActiveValue::Set(r.deployment_id),
            });
        import_staging::Entity::insert_many(staged).exec(db).await?;
    }
    Ok(())
}

pub(super) struct OverlapReport {
    pub(super) identical: usize,
    pub(super) differing: usize,
    pub(super) sample: Vec<OverlapDiff>,
    /// Stream already holding readings for a (parameter, time), ie. the row an incoming value for
    /// that slot must be written onto. Absent when the slot is empty.
    pub(super) owning_stream: HashMap<(Uuid, chrono::DateTime<chrono::Utc>), Uuid>,
    /// File lines whose value is already stored: nothing to screen, nothing changes.
    pub(super) identical_lines: HashSet<usize>,
}

/// Cap on differing-overlap rows returned for UI preview.
pub(super) const OVERLAP_SAMPLE_CAP: usize = 20;

/// Tolerance for treating an incoming value as identical to the stored one.
pub(super) const OVERLAP_EPSILON: f64 = 1e-9;

/// Bucket incoming rows against existing readings into identical and differing overlaps, and
/// report which stream already owns each occupied slot. The Nth incoming row for a
/// (parameter, time) key is compared against the Nth existing replicate.
pub(super) async fn compute_overlaps(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    rows: &[(Uuid, chrono::DateTime<chrono::Utc>, f64, usize)],
    earliest: Option<chrono::DateTime<chrono::Utc>>,
    latest: Option<chrono::DateTime<chrono::Utc>>,
) -> AppResult<OverlapReport> {
    let mut identical = 0usize;
    let mut differing = 0usize;
    let mut sample = Vec::new();
    let mut owning_stream = HashMap::new();
    let mut identical_lines = HashSet::new();

    let (Some(t_min), Some(t_max)) = (earliest, latest) else {
        return Ok(OverlapReport {
            identical,
            differing,
            sample,
            owning_stream,
            identical_lines,
        });
    };
    if rows.is_empty() {
        return Ok(OverlapReport {
            identical,
            differing,
            sample,
            owning_stream,
            identical_lines,
        });
    }

    let param_ids: Vec<Uuid> = rows
        .iter()
        .map(|(pid, _, _, _)| *pid)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    let existing_rows = readings::Entity::find()
        .select_only()
        .column(readings::Column::ParameterId)
        .column(readings::Column::Time)
        .column(readings::Column::StreamId)
        .column_as(effective_value(None), "val")
        .filter(readings::Column::SiteId.eq(site_id))
        .filter(readings::Column::ParameterId.is_in(param_ids))
        .filter(readings::Column::Time.gte(t_min))
        .filter(readings::Column::Time.lte(t_max))
        .order_by_asc(readings::Column::ParameterId)
        .order_by_asc(readings::Column::Time)
        .order_by_asc(readings::Column::ReplicateIndex)
        .into_model::<StoredValueRow>()
        .all(db)
        .await?;

    let mut existing: HashMap<(Uuid, chrono::DateTime<chrono::Utc>), Vec<f64>> =
        HashMap::with_capacity(existing_rows.len());
    for row in existing_rows {
        let key = (row.parameter_id, row.time.with_timezone(&chrono::Utc));
        if let Some(stream_id) = row.stream_id {
            owning_stream.entry(key).or_insert(stream_id);
        }
        existing.entry(key).or_default().push(row.val);
    }

    let mut occurrence: HashMap<(Uuid, chrono::DateTime<chrono::Utc>), usize> = HashMap::new();
    for (pid, time, incoming, line) in rows {
        let key = (*pid, *time);
        let idx = occurrence.entry(key).or_insert(0);
        let slot = *idx;
        *idx += 1;
        let Some(stored) = existing.get(&key).and_then(|vs| vs.get(slot)) else {
            continue;
        };
        if (stored - incoming).abs() <= OVERLAP_EPSILON {
            identical += 1;
            identical_lines.insert(*line);
        } else {
            differing += 1;
            if sample.len() < OVERLAP_SAMPLE_CAP {
                sample.push(OverlapDiff {
                    time: *time,
                    parameter_id: *pid,
                    existing: *stored,
                    incoming: *incoming,
                });
            }
        }
    }

    Ok(OverlapReport {
        identical,
        differing,
        sample,
        owning_stream,
        identical_lines,
    })
}

/// A slot at one instant: the key a file's rows are deduplicated, owned and attributed by.
pub(super) type SlotInstant = (Uuid, chrono::DateTime<chrono::Utc>);

/// One stored cell of a file: parameter, instant, value as stored, and the file line it came from.
pub(super) type ImportRow = (Uuid, chrono::DateTime<chrono::Utc>, f64, usize);

/// The zone a file's naive timestamps are in, as declared by the caller.
pub(super) fn declared_offset(hours: Option<f64>) -> chrono::Duration {
    chrono::Duration::milliseconds((hours.unwrap_or(0.0) * 3_600_000.0) as i64)
}

/// `curves` fills a tool's curve slots, so a file naming no tool has none to fill.
pub(super) fn require_tool_for_curves(req: &ImportCsvRequest) -> AppResult<()> {
    if req.curves.is_some() && req.tool.is_none() {
        return Err(AppError::BadRequest(
            "curves fills a tool's curve slots; name the tool the file is entry for".into(),
        ));
    }
    Ok(())
}

/// The problems a file's rows carry: the first `MAX_ERRORS` listed, every one counted.
#[derive(Default)]
pub(super) struct RowErrors {
    pub(super) listed: Vec<RowError>,
    pub(super) count: usize,
}

impl RowErrors {
    pub(super) fn record(&mut self, row: usize, message: String) {
        self.count += 1;
        if self.listed.len() < MAX_ERRORS {
            self.listed.push(RowError { row, message });
        }
    }
}

/// What a header can name at one site: the site's slot names and aliases first, then the catalog's
/// codes and aliases. A calculation's output is recognised so it can be refused.
#[derive(Default)]
pub(super) struct ColumnResolver {
    site_names: HashMap<String, (Uuid, String)>,
    site_aliases: HashMap<String, (Uuid, String)>,
    catalog: HashMap<String, Uuid>,
    names: HashMap<Uuid, String>,
    at_site: HashSet<Uuid>,
    derived_outputs: HashSet<Uuid>,
}

impl ColumnResolver {
    /// From the site's slot columns, the catalog as `(id, code, aliases)` and every calculation
    /// output.
    pub(super) fn new(
        slots: Vec<SlotColumnRow>,
        catalog: impl IntoIterator<Item = (Uuid, String, Vec<String>)>,
        derived_outputs: HashSet<Uuid>,
    ) -> Self {
        let mut resolver = Self {
            derived_outputs,
            ..Self::default()
        };
        for row in slots {
            let pid = row.parameter_id;
            let sp_name = row.sp_name.unwrap_or_default();
            let param_name = row.param_name.unwrap_or_default();
            resolver
                .site_names
                .insert(sp_name.to_lowercase(), (pid, sp_name.clone()));
            resolver
                .site_names
                .insert(param_name.to_lowercase(), (pid, sp_name.clone()));
            resolver.names.insert(pid, sp_name.clone());
            resolver.at_site.insert(pid);
            for alias in row.aliases.unwrap_or_default() {
                resolver
                    .site_aliases
                    .insert(alias.to_lowercase(), (pid, sp_name.clone()));
            }
        }
        for (pid, code, aliases) in catalog {
            resolver.catalog.insert(code.to_lowercase(), pid);
            for alias in &aliases {
                resolver.catalog.insert(alias.to_lowercase(), pid);
            }
            resolver.names.entry(pid).or_insert(code);
        }
        resolver
    }

    /// The parameter a header names, and the name it is reported under.
    pub(super) fn resolve_header(&self, header: &str) -> Option<(Uuid, String)> {
        let key = header.to_lowercase();
        self.site_names
            .get(&key)
            .or_else(|| self.site_aliases.get(&key))
            .cloned()
            .or_else(|| self.catalog.get(&key).map(|&pid| (pid, self.name_of(pid))))
    }

    /// The parameter an explicit mapping names, by id or by any spelling a header may use.
    pub(super) fn resolve_target(&self, target: &str) -> Option<(Uuid, String)> {
        if let Ok(pid) = Uuid::parse_str(target) {
            return self.names.get(&pid).map(|name| (pid, name.clone()));
        }
        self.resolve_header(target)
    }

    fn name_of(&self, pid: Uuid) -> String {
        self.names.get(&pid).cloned().unwrap_or_default()
    }
}

/// What a file's header does with each column.
#[derive(Default)]
pub(super) struct ColumnPlan {
    pub(super) mappings: Vec<ColumnMapping>,
    pub(super) mapped_columns: HashMap<String, String>,
    pub(super) skipped_columns: Vec<String>,
    pub(super) unmapped_columns: Vec<String>,
    pub(super) warnings: Vec<String>,
}

/// Resolve every header but the timestamp: an explicit mapping first (`null` skips the column),
/// then the resolver. A calculation's output is skipped, since it is computed and never ingested,
/// and a parameter the site has no slot for is imported with a warning.
pub(super) fn plan_columns(
    headers: &[&str],
    datetime_idx: usize,
    mapping: Option<&HashMap<String, Option<String>>>,
    resolver: &ColumnResolver,
    site_name: &str,
) -> ColumnPlan {
    let mut plan = ColumnPlan::default();
    for (idx, header) in headers.iter().enumerate() {
        if idx == datetime_idx {
            continue;
        }
        let resolved = match mapping.and_then(|m| m.get(*header)) {
            Some(None) => {
                plan.skipped_columns.push((*header).to_string());
                continue;
            }
            Some(Some(target)) => {
                let Some(resolved) = resolver.resolve_target(target) else {
                    plan.unmapped_columns.push((*header).to_string());
                    plan.warnings.push(format!(
                        "Explicit mapping target '{target}' for column '{header}' is not a known parameter"
                    ));
                    continue;
                };
                Some(resolved)
            }
            None => resolver.resolve_header(header),
        };
        match resolved {
            Some((pid, _)) if resolver.derived_outputs.contains(&pid) => {
                plan.skipped_columns.push((*header).to_string());
            }
            Some((pid, resolved_name)) => {
                plan.mapped_columns
                    .insert((*header).to_string(), resolved_name);
                if !resolver.at_site.contains(&pid) {
                    plan.warnings.push(format!(
                        "Column '{header}' maps to parameter '{}', which is not assigned to site '{site_name}'; it will be stored but not exposed until you add the site parameter",
                        resolver.name_of(pid)
                    ));
                }
                plan.mappings.push(ColumnMapping {
                    idx,
                    header: (*header).to_string(),
                    parameter_id: pid,
                    conversion_factor: 1.0,
                    conversion_offset: 0.0,
                });
            }
            None => plan.unmapped_columns.push((*header).to_string()),
        }
    }
    plan
}

/// Refuse a file with no resolved column left to import.
pub(super) fn require_columns(plan: &ColumnPlan) -> AppResult<()> {
    if plan.mappings.is_empty() {
        return Err(AppError::BadRequest(
            "No CSV columns resolved to ingestible parameters for this site's project".to_string(),
        ));
    }
    Ok(())
}

/// A reader over a wide CSV: headed, trimmed, and tolerant of short rows.
pub(super) fn csv_reader(text: &str) -> csv::Reader<&[u8]> {
    csv::ReaderBuilder::new()
        .has_headers(true)
        .trim(csv::Trim::All)
        .flexible(true)
        .from_reader(text.as_bytes())
}

/// The file's header row.
pub(super) fn file_headers<R: std::io::Read>(
    reader: &mut csv::Reader<R>,
) -> AppResult<csv::StringRecord> {
    reader
        .headers()
        .cloned()
        .map_err(|e| AppError::BadRequest(format!("Failed to read CSV header: {e}")))
}

/// A file's rows as parsed: the cells to store, the span they cover and what could not be read.
pub(super) struct ParsedFile {
    pub(super) rows: Vec<ImportRow>,
    pub(super) earliest: Option<chrono::DateTime<chrono::Utc>>,
    pub(super) latest: Option<chrono::DateTime<chrono::Utc>>,
    /// Rows whose timestamp was admitted, whatever their cells held.
    pub(super) row_count: usize,
    pub(super) errors: RowErrors,
}

/// Read every row after the header. A bad row or cell is recorded against its line and skipped,
/// so a partly malformed file still imports its good rows. Every row is judged against the one
/// `now`, so two rows carrying one timestamp cannot land on opposite sides of the lead bound.
pub(super) fn parse_rows<R: std::io::Read>(
    reader: &mut csv::Reader<R>,
    datetime_idx: usize,
    tz_offset: chrono::Duration,
    mappings: &[ColumnMapping],
    now: chrono::DateTime<chrono::Utc>,
) -> ParsedFile {
    let mut file = ParsedFile {
        rows: Vec::new(),
        earliest: None,
        latest: None,
        row_count: 0,
        errors: RowErrors::default(),
    };
    // The header is line 1.
    for (record, line) in reader.records().zip(2usize..) {
        let record = match record {
            Ok(r) => r,
            Err(e) => {
                file.errors.record(line, format!("CSV parse error: {e}"));
                continue;
            }
        };
        let dt_cell = record.get(datetime_idx).unwrap_or("");
        let Some(time) = parse_datetime(dt_cell, tz_offset) else {
            file.errors
                .record(line, format!("Unparseable DateTime '{dt_cell}'"));
            continue;
        };
        if let Some(reason) = admission::time_rejection_at(now, time) {
            file.errors.record(line, reason);
            continue;
        }
        file.row_count += 1;
        file.earliest = Some(file.earliest.map_or(time, |e| Ord::min(e, time)));
        file.latest = Some(file.latest.map_or(time, |l| Ord::max(l, time)));
        for m in mappings {
            match admission::classify_cell(record.get(m.idx).unwrap_or("")) {
                admission::Cell::Missing => {}
                admission::Cell::Invalid(reason) => {
                    file.errors
                        .record(line, format!("Column '{}': {reason}", m.header));
                }
                admission::Cell::Value(raw) => {
                    let stored = (raw - m.conversion_offset) / m.conversion_factor;
                    file.rows.push((m.parameter_id, time, stored, line));
                }
            }
        }
    }
    file
}

/// The header each mapped parameter was read from, for naming it in a row error.
pub(super) fn headers_by_parameter(mappings: &[ColumnMapping]) -> HashMap<Uuid, String> {
    mappings
        .iter()
        .map(|m| (m.parameter_id, m.header.clone()))
        .collect()
}

/// Each id once, in order.
pub(super) fn distinct_ids(ids: impl IntoIterator<Item = Uuid>) -> Vec<Uuid> {
    let mut ids: Vec<Uuid> = ids.into_iter().collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Refuse the rows landing on a slot a replicate-family stream serves. A reading's
/// replicate_index is the source's column position there, so an import numbering from 0 would
/// fabricate replicates, and a row beside the family would double-serve the instant. Returns
/// whether any row was refused.
pub(super) fn refuse_family_slots(
    rows: &mut Vec<ImportRow>,
    owning_stream: &HashMap<SlotInstant, Uuid>,
    family_keys: &HashMap<Uuid, String>,
    headers: &HashMap<Uuid, String>,
    errors: &mut RowErrors,
) -> bool {
    let before = rows.len();
    rows.retain(|(pid, time, _, line)| {
        let Some(key) = owning_stream
            .get(&(*pid, *time))
            .and_then(|sid| family_keys.get(sid))
        else {
            return true;
        };
        errors.record(
            *line,
            format!(
                "Column '{}': {} is served by replicate family stream '{key}'; its replicates \
                 sync from the source and cannot be written by CSV import",
                headers.get(pid).map_or("?", String::as_str),
                time.to_rfc3339()
            ),
        );
        false
    });
    rows.len() != before
}

/// The cadence a row will be written at, resolved as the write resolves it: the request's
/// declaration, the stream that will carry the row, the deployed sensor's data_frequency, then
/// continuous.
pub(super) struct SlotCadence<'a> {
    pub(super) declared: Option<&'a str>,
    pub(super) owning_stream: &'a HashMap<SlotInstant, Uuid>,
    pub(super) api_stream_of: HashMap<Uuid, Uuid>,
    pub(super) stream_default: HashMap<Uuid, Option<String>>,
    pub(super) owners: &'a HashMap<SlotInstant, ResolvedOwner>,
    pub(super) sensor_types: HashMap<Uuid, &'static str>,
}

impl SlotCadence<'_> {
    pub(super) fn of(&self, pid: Uuid, time: chrono::DateTime<chrono::Utc>) -> String {
        let stream_id = self
            .owning_stream
            .get(&(pid, time))
            .or_else(|| self.api_stream_of.get(&pid));
        resolve_measurement_type(
            self.declared,
            stream_id
                .and_then(|id| self.stream_default.get(id))
                .and_then(Option::as_deref),
            self.owners.get(&(pid, time)).and_then(|o| o.sensor_id),
            &self.sensor_types,
        )
    }
}

/// Refuse a repeated (parameter, timestamp) on every cadence but spot. A repeat is a source defect
/// there, and absorbing it as replicate 1 would hide it from the default read and every rollup
/// while fabricating a grab sample around it; a spot file is a replicate plate, where the repeat is
/// the point. Returns whether any row was refused.
pub(super) fn refuse_repeated_slots(
    rows: &mut Vec<ImportRow>,
    cadence: impl Fn(Uuid, chrono::DateTime<chrono::Utc>) -> String,
    headers: &HashMap<Uuid, String>,
    errors: &mut RowErrors,
) -> bool {
    let mut seen: HashSet<SlotInstant> = HashSet::new();
    let before = rows.len();
    rows.retain(|(pid, time, _, line)| {
        let resolved = cadence(*pid, *time);
        if resolved == "spot" || seen.insert((*pid, *time)) {
            return true;
        }
        errors.record(
            *line,
            format!(
                "Column '{}': timestamp {} is repeated, and a '{resolved}' series holds one \
                 reading per timestamp",
                headers.get(pid).map_or("?", String::as_str),
                time.to_rfc3339()
            ),
        );
        false
    });
    rows.len() != before
}

/// The (parameter, timestamp) groups a spot file holds more than one value for.
pub(super) fn replicate_groups(measurement_type: Option<&str>, rows: &[ImportRow]) -> usize {
    if measurement_type != Some("spot") {
        return 0;
    }
    let mut sizes: HashMap<SlotInstant, usize> = HashMap::new();
    for (pid, t, _, _) in rows {
        *sizes.entry((*pid, *t)).or_default() += 1;
    }
    sizes.values().filter(|n| **n > 1).count()
}

/// The cells a spot file's seasonal screen reads. A cell already stored with the same value is
/// its own history, and importing it again changes nothing.
pub(super) fn screened_cells(
    rows: &[ImportRow],
    identical_lines: &HashSet<usize>,
) -> Vec<(usize, Uuid, chrono::DateTime<chrono::Utc>, f64)> {
    rows.iter()
        .filter(|(_, _, _, line)| !identical_lines.contains(line))
        .map(|(pid, time, value, line)| (*line, *pid, *time, *value))
        .collect()
}

/// The stream each row is written onto: the one already holding its slot, so an overwrite
/// replaces the stored reading rather than adding a second one the rollups would double-count,
/// else the importer's `api` stream for the parameter. A stream holding more than one of the
/// file's parameters is not a target, since replicates are numbered per (stream, time).
pub(super) struct WriteTargets<'a> {
    owning_stream: &'a HashMap<SlotInstant, Uuid>,
    api_streams: &'a HashMap<Uuid, Uuid>,
    shared: HashSet<Uuid>,
}

impl<'a> WriteTargets<'a> {
    pub(super) fn new(
        owning_stream: &'a HashMap<SlotInstant, Uuid>,
        api_streams: &'a HashMap<Uuid, Uuid>,
    ) -> Self {
        let mut params_per_stream: HashMap<Uuid, HashSet<Uuid>> = HashMap::new();
        for ((parameter_id, _), stream_id) in owning_stream {
            params_per_stream
                .entry(*stream_id)
                .or_default()
                .insert(*parameter_id);
        }
        let shared = params_per_stream
            .into_iter()
            .filter(|(_, params)| params.len() > 1)
            .map(|(stream_id, _)| stream_id)
            .collect();
        Self {
            owning_stream,
            api_streams,
            shared,
        }
    }

    pub(super) fn of(&self, pid: Uuid, time: chrono::DateTime<chrono::Utc>) -> Uuid {
        self.owning_stream
            .get(&(pid, time))
            .filter(|stream_id| !self.shared.contains(*stream_id))
            .copied()
            .unwrap_or(self.api_streams[&pid])
    }
}

/// The rows as staged for the worker. Sensor and deployment are physical facts about the slot at
/// that time and are stamped either way, the channel's instrument standing in where no deployment
/// names one; the calibration is a claim the value is uncorrected input, which only a raw file
/// makes.
pub(super) fn staged_rows(
    site_id: Uuid,
    rows: &[ImportRow],
    owners: &HashMap<SlotInstant, ResolvedOwner>,
    targets: &WriteTargets,
    channel_instruments: &HashMap<Uuid, Uuid>,
    values: CsvValueState,
) -> Vec<StagedRow> {
    rows.iter()
        .map(|(parameter_id, time, value, _)| {
            let owner = owners
                .get(&(*parameter_id, *time))
                .cloned()
                .unwrap_or_default();
            let stream_id = targets.of(*parameter_id, *time);
            StagedRow {
                stream_id,
                site_id,
                parameter_id: *parameter_id,
                time: *time,
                raw_value: *value,
                sensor_id: owner
                    .sensor_id
                    .or_else(|| channel_instruments.get(&stream_id).copied()),
                calibration_id: match values {
                    CsvValueState::Raw => owner.calibration_id,
                    CsvValueState::Corrected => None,
                },
                deployment_id: owner.deployment_id,
            }
        })
        .collect()
}

/// What a commit does with the file's rows against what is stored.
pub(super) struct ImportTally {
    /// Anything to write: a new row, or a differing one an overwrite replaces.
    pub(super) has_work: bool,
    pub(super) overlapping: usize,
    pub(super) inserted_total: usize,
    pub(super) duplicates: usize,
    pub(super) overwritten: usize,
}

/// How the file's rows split into new, unchanged and replaced under the conflict mode.
pub(super) fn import_tally(
    rows: usize,
    identical: usize,
    differing: usize,
    conflict: ConflictMode,
) -> ImportTally {
    let overlapping = identical + differing;
    let inserted_total = rows.saturating_sub(overlapping);
    let (duplicates, overwritten) = match conflict {
        ConflictMode::Skip => (overlapping, 0),
        ConflictMode::Overwrite => (identical, differing),
    };
    ImportTally {
        has_work: rows > overlapping || (differing > 0 && conflict == ConflictMode::Overwrite),
        overlapping,
        inserted_total,
        duplicates,
        overwritten,
    }
}

/// How many distinct timestamps the rows cover, each one a derived recompute.
pub(super) fn distinct_instants(rows: &[ImportRow]) -> usize {
    rows.iter()
        .map(|(_, t, _, _)| *t)
        .collect::<HashSet<_>>()
        .len()
}

/// Each mapped parameter paired with its importer stream, as the worker job reads them.
pub(super) fn param_streams(
    mappings: &[ColumnMapping],
    api_streams: &HashMap<Uuid, Uuid>,
) -> Vec<serde_json::Value> {
    mappings
        .iter()
        .filter_map(|m| {
            api_streams
                .get(&m.parameter_id)
                .map(|&sid| serde_json::json!([m.parameter_id, sid]))
        })
        .collect()
}

/// The `csv_import` job's params: where the staged rows are and what the worker applies them with.
pub(super) fn import_job_params(
    import_token: Uuid,
    site_id: Uuid,
    site_name: &str,
    req: &ImportCsvRequest,
    file: &ParsedFile,
    overlapping: usize,
    param_streams: Vec<serde_json::Value>,
) -> serde_json::Value {
    serde_json::json!({
        "import_token": import_token,
        "site_id": site_id,
        "site_name": site_name,
        "conflict": match req.conflict {
            ConflictMode::Skip => "skip",
            ConflictMode::Overwrite => "overwrite",
        },
        "since": file.earliest.map(|t| t.to_rfc3339()),
        "latest": file.latest.map(|t| t.to_rfc3339()),
        "overlapping": overlapping,
        "param_streams": param_streams,
        "measurement_type": req.measurement_type.as_deref(),
    })
}

/// One site's file as analysed, before anything is written.
pub(super) struct SiteAnalysis {
    pub(super) site_id: Uuid,
    pub(super) site_name: String,
    pub(super) session_id: Uuid,
    pub(super) plan: ColumnPlan,
    pub(super) parsed: ParsedFile,
    pub(super) replicate_groups: usize,
    pub(super) overlap: OverlapReport,
    pub(super) check: Option<ImportCheck>,
}

/// What a commit wrote, beyond the analysis.
pub(super) struct Committed {
    pub(super) tally: ImportTally,
    pub(super) derived_job_id: Option<Uuid>,
    pub(super) derived_timestamps: usize,
}

impl SiteAnalysis {
    /// The response for this site: a dry run's preview with no commit, else what the commit did.
    pub(super) fn response(self, commit: Option<Committed>) -> ImportCsvResponse {
        let dry_run = commit.is_none();
        let (inserted_total, duplicates, overwritten, derived_job_id, derived_timestamps) = commit
            .map_or((0, 0, 0, None, 0), |c| {
                (
                    c.tally.inserted_total,
                    c.tally.duplicates,
                    c.tally.overwritten,
                    c.derived_job_id,
                    c.derived_timestamps,
                )
            });
        ImportCsvResponse {
            site_id: self.site_id,
            site_name: self.site_name,
            dry_run,
            session_id: Some(self.session_id),
            mapped_columns: self.plan.mapped_columns,
            skipped_columns: self.plan.skipped_columns,
            unmapped_columns: self.plan.unmapped_columns,
            warnings: self.plan.warnings,
            row_count: self.parsed.row_count,
            replicate_groups: self.replicate_groups,
            inserted_total,
            earliest: self.parsed.earliest,
            latest: self.parsed.latest,
            derived_job_id,
            derived_timestamps,
            duplicates,
            overlaps_identical: self.overlap.identical,
            overlaps_differing: self.overlap.differing,
            overwritten,
            overlap_sample: self.overlap.sample,
            errors: self.parsed.errors.listed,
            error_count: self.parsed.errors.count,
            tool_runs_created: 0,
            curves: Vec::new(),
            check: self.check,
            site_imports: Vec::new(),
        }
    }
}

/// Ceiling on tool-entry rows per request: each row is one runner execution plus one grab save,
/// and a campaign result sheet is tens of rows, not thousands.
pub(super) const TOOL_IMPORT_ROW_CAP: usize = 500;

/// A header of the form `{name}_rep_{k}` or `{name}_{k}` (k from 1) naming position k-1 of a
/// `replicates` param.
pub(super) fn replicate_column(
    params: &[crate::routes::private::tools::models::ManifestParam],
    header: &str,
) -> Option<(String, Option<usize>)> {
    let lower = header.to_ascii_lowercase();
    params
        .iter()
        .filter(|p| p.kind == "replicates")
        .find_map(|p| {
            let rest = lower.strip_prefix(&format!("{}_", p.name.to_ascii_lowercase()))?;
            let rest = rest.strip_prefix("rep_").unwrap_or(rest);
            let k: usize = rest.parse().ok().filter(|k| *k >= 1)?;
            Some((p.name.clone(), Some(k - 1)))
        })
}

/// The index of the CSV's timestamp column, matched case-insensitively against the names an
/// import declares its time under. No match is the caller's error: taking a column by position
/// would parse whatever happens to be first as a time.
pub(super) fn timestamp_column(headers: &[&str]) -> Result<usize, String> {
    headers
        .iter()
        .position(|h| TIMESTAMP_HEADERS.iter().any(|n| h.eq_ignore_ascii_case(n)))
        .ok_or_else(|| {
            format!(
                "No timestamp column: expected a header named {}, found {}",
                TIMESTAMP_HEADERS.join(", "),
                headers.join(", ")
            )
        })
}

/// The header names a timestamp column may carry. `Date` is what the portals' high-frequency
/// exports head theirs.
const TIMESTAMP_HEADERS: [&str; 3] = ["DateTime", "Date", "Time"];

/// A header naming one of the tool's curve slots (case-insensitive): its cells are standard curve
/// ids, one per row.
pub(super) fn curve_column(
    curves: &[crate::routes::private::tools::models::ManifestCurve],
    header: &str,
) -> Option<String> {
    curves
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(header))
        .map(|c| c.name.clone())
}

/// The curve each slot takes for one row: the row's own cell when it names one, else the
/// request-level curve. A cell that is not a curve id is the row's error.
pub(super) fn row_curves(
    defaults: &HashMap<String, Uuid>,
    cells: &[(&str, &str)],
) -> Result<HashMap<String, Uuid>, String> {
    let mut curves = defaults.clone();
    for (slot, cell) in cells {
        let cell = cell.trim();
        if cell.is_empty() {
            continue;
        }
        let id: Uuid = cell
            .parse()
            .map_err(|_| format!("Column '{slot}': '{cell}' is not a standard curve id"))?;
        curves.insert((*slot).to_string(), id);
    }
    Ok(curves)
}

/// The curve a `replicates` param's stored readings carry: the row's curve for the slot the
/// param declares. The run applied it to what it computed; the stored replicate is raw, so the
/// reference is what makes the database apply it there too.
pub(super) fn replicate_curve(
    params: &[crate::routes::private::tools::models::ManifestParam],
    param: &str,
    curves: &HashMap<String, Uuid>,
) -> Option<Uuid> {
    let slot = params.iter().find(|p| p.name == param)?.curve.as_deref()?;
    curves.get(slot).copied()
}

/// One parsed data row of a tool-entry file: the run's typed inputs and the curve each slot takes.
pub(super) struct ToolRow {
    pub(super) line: usize,
    pub(super) time: chrono::DateTime<chrono::Utc>,
    pub(super) body: serde_json::Map<String, serde_json::Value>,
    pub(super) curves: HashMap<String, Uuid>,
}

/// The tool-entry import: one tool run per data row, outputs saved through the grab write path.
///
/// Columns map to the tool's manifest params (case-insensitive, exact otherwise); the `DateTime`
/// column is the row's collection instant and travels to the engine as `collected_at`, so
/// station and event inputs resolve exactly as they would for a typed entry. A column headed
/// with a curve slot's name carries a standard curve id per row, over the request's `curves`.
/// Each row's run is stored with `source = 'csv_import'` and its save builds the ordinary
/// server-side blob.
#[allow(clippy::too_many_arguments)]
pub(super) async fn import_tool_csv(
    state: &AppState,
    auth: &crate::common::middleware::AuthContext,
    scope: &crate::common::authz::AccessScope,
    req: &ImportCsvRequest,
    tool_name: &str,
    csv_text: &str,
    session_id: Uuid,
    site: &crate::routes::private::sites::Model,
    tz_offset: chrono::Duration,
) -> AppResult<ImportCsvResponse> {
    use super::models::GrabSampleReading;
    use super::models::GrabSampleRequest;
    use super::models::GrabWriteMode;
    use super::views::insert_grab_samples;
    use crate::routes::private::tools::flows::{execute_and_store_run, preview_run};
    use crate::routes::private::tools::service as engine;

    let tool = engine::find_active_tool(&state.db, tool_name).await?;

    let request_curves: HashMap<String, Uuid> = req.curves.clone().unwrap_or_default();
    for slot in request_curves.keys() {
        if !tool.manifest.curves.iter().any(|c| &c.name == slot) {
            let slots: Vec<&str> = tool
                .manifest
                .curves
                .iter()
                .map(|c| c.name.as_str())
                .collect();
            return Err(AppError::BadRequest(format!(
                "'{slot}' is not a curve slot of tool '{}'; its slots are [{}]",
                tool.name,
                slots.join(", ")
            )));
        }
    }

    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .trim(csv::Trim::All)
        .flexible(true)
        .from_reader(csv_text.as_bytes());
    let headers = reader
        .headers()
        .map_err(|e| AppError::BadRequest(format!("Failed to read CSV header: {e}")))?
        .clone();
    let columns: Vec<&str> = headers.iter().collect();
    let datetime_idx = timestamp_column(&columns).map_err(AppError::BadRequest)?;

    let mut mapped_columns: HashMap<String, String> = HashMap::new();
    let mut unmapped_columns: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    // (column index, param name, position within a replicates param)
    let mut column_params: Vec<(usize, String, Option<usize>)> = Vec::new();
    // (column index, curve slot)
    let mut curve_columns: Vec<(usize, String)> = Vec::new();
    for (idx, header) in headers.iter().enumerate() {
        if idx == datetime_idx {
            continue;
        }
        if let Some(slot) = curve_column(&tool.manifest.curves, header) {
            curve_columns.push((idx, slot));
            continue;
        }
        let hit = tool
            .manifest
            .params
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(header))
            .map(|p| (p.name.clone(), None))
            .or_else(|| replicate_column(&tool.manifest.params, header));
        match hit {
            Some((name, position)) => {
                mapped_columns.insert(header.to_string(), name.clone());
                column_params.push((idx, name, position));
            }
            None => {
                unmapped_columns.push(header.to_string());
                warnings.push(format!(
                    "Column '{header}' is not an input of tool '{}' and is ignored",
                    tool.name
                ));
            }
        }
    }
    if column_params.is_empty() {
        return Err(AppError::BadRequest(format!(
            "No CSV columns match inputs of tool '{}'",
            tool.name
        )));
    }

    let mut errors: Vec<RowError> = Vec::new();
    let mut error_count = 0usize;
    let record_error =
        |row: usize, message: String, errors: &mut Vec<RowError>, count: &mut usize| {
            *count += 1;
            if errors.len() < MAX_ERRORS {
                errors.push(RowError { row, message });
            }
        };

    // Parse every row up front so the plan (and dry_run) reports the whole file before anything
    // runs.
    let mut rows: Vec<ToolRow> = Vec::new();
    let mut earliest: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut latest: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut line = 1usize;
    let now = chrono::Utc::now();
    for record in reader.records() {
        line += 1;
        let record = match record {
            Ok(r) => r,
            Err(e) => {
                record_error(
                    line,
                    format!("CSV parse error: {e}"),
                    &mut errors,
                    &mut error_count,
                );
                continue;
            }
        };
        let dt_cell = record.get(datetime_idx).unwrap_or("");
        let Some(time) = parse_datetime(dt_cell, tz_offset) else {
            record_error(
                line,
                format!("Unparseable DateTime '{dt_cell}'"),
                &mut errors,
                &mut error_count,
            );
            continue;
        };
        if let Some(reason) = admission::time_rejection_at(now, time) {
            record_error(line, reason, &mut errors, &mut error_count);
            continue;
        }
        let mut body = serde_json::Map::new();
        let mut bad_cell = false;
        for (idx, param, position) in &column_params {
            match admission::classify_cell(record.get(*idx).unwrap_or("")) {
                admission::Cell::Missing => {}
                admission::Cell::Invalid(reason) => {
                    record_error(
                        line,
                        format!("Column '{param}': {reason}"),
                        &mut errors,
                        &mut error_count,
                    );
                    bad_cell = true;
                }
                admission::Cell::Value(v) => match position {
                    None => {
                        body.insert(param.clone(), serde_json::json!(v));
                    }
                    // A replicate column lands at its own position; a blank column before it
                    // stays a null so the replicate keeps its index.
                    Some(pos) => {
                        let list = body
                            .entry(param.clone())
                            .or_insert_with(|| serde_json::json!([]));
                        let list = list.as_array_mut().expect("replicate columns build a list");
                        while list.len() <= *pos {
                            list.push(serde_json::Value::Null);
                        }
                        list[*pos] = serde_json::json!(v);
                    }
                },
            }
        }
        if bad_cell || body.is_empty() {
            continue;
        }
        let cells: Vec<(&str, &str)> = curve_columns
            .iter()
            .map(|(idx, slot)| (slot.as_str(), record.get(*idx).unwrap_or("")))
            .collect();
        let curves = match row_curves(&request_curves, &cells) {
            Ok(curves) => curves,
            Err(reason) => {
                record_error(line, reason, &mut errors, &mut error_count);
                continue;
            }
        };
        earliest = Some(earliest.map_or(time, |e| Ord::min(e, time)));
        latest = Some(latest.map_or(time, |l| Ord::max(l, time)));
        rows.push(ToolRow {
            line,
            time,
            body,
            curves,
        });
    }
    if rows.len() > TOOL_IMPORT_ROW_CAP {
        return Err(AppError::BadRequest(format!(
            "Tool-entry import takes at most {TOOL_IMPORT_ROW_CAP} rows per file; this file has {}",
            rows.len()
        )));
    }

    // Every curve the file or the request names, loaded once: a request-level id nothing
    // carries refuses the plan, a row's own does the row.
    let curve_ids: Vec<Uuid> = request_curves
        .values()
        .chain(rows.iter().flat_map(|r| r.curves.values()))
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let stored_curves: HashMap<Uuid, standard_curves::Model> = standard_curves::Entity::find()
        .filter(standard_curves::Column::Id.is_in(curve_ids))
        .all(&state.db)
        .await?
        .into_iter()
        .map(|c| (c.id, c))
        .collect();
    for (slot, id) in &request_curves {
        if !stored_curves.contains_key(id) {
            return Err(AppError::BadRequest(format!(
                "curve '{slot}': standard curve {id} not found"
            )));
        }
    }
    rows.retain(|row| {
        match row
            .curves
            .iter()
            .find(|(_, id)| !stored_curves.contains_key(id))
        {
            Some((slot, id)) => {
                record_error(
                    row.line,
                    format!("Column '{slot}': standard curve {id} not found"),
                    &mut errors,
                    &mut error_count,
                );
                false
            }
            None => true,
        }
    });
    let curve_plan: Vec<ImportCurve> = tool
        .manifest
        .curves
        .iter()
        .map(|c| {
            let standard_curve_id = request_curves.get(&c.name).copied();
            ImportCurve {
                slot: c.name.clone(),
                label: c.label.clone(),
                required: c.required,
                column: curve_columns
                    .iter()
                    .find(|(_, slot)| slot == &c.name)
                    .map(|(idx, _)| headers[*idx].to_string()),
                standard_curve_id,
                name: standard_curve_id.and_then(|id| stored_curves[&id].name.clone()),
            }
        })
        .collect();

    let catalog =
        engine::load_parameter_catalog(&state.db, std::iter::once(&tool.manifest)).await?;
    let saved_outputs: Vec<(String, Uuid)> = tool
        .manifest
        .outputs
        .iter()
        .filter_map(|o| catalog.resolve(o).map(|p| (o.key.clone(), p.id)))
        .collect();
    // The replicates a row enters are readings of their own parameter, stored raw.
    let saved_inputs: Vec<(String, Uuid)> = tool
        .manifest
        .params
        .iter()
        .filter(|p| p.kind == "replicates")
        .filter_map(|p| {
            catalog
                .resolve_code(p.parameter_code.as_deref()?)
                .map(|row| (p.name.clone(), row.id))
        })
        .collect();
    if saved_outputs.is_empty() && saved_inputs.is_empty() {
        return Err(AppError::BadRequest(format!(
            "Nothing of tool '{}' resolves to a catalog parameter; nothing could be saved",
            tool.name
        )));
    }

    // --- Run ---
    // A dry run previews each row and stores nothing; a commit stores the run its save names.
    let row_count = rows.len();
    let actor = crate::common::actor::label(auth);
    let mut computed: Vec<(usize, Option<Uuid>, Vec<GrabSampleReading>)> = Vec::new();
    let mut tool_runs_created = 0usize;
    for ToolRow {
        line,
        time,
        mut body,
        curves,
    } in rows
    {
        for (slot, id) in &curves {
            body.insert(slot.clone(), serde_json::json!({ "standard_curve_id": id }));
        }
        body.insert("site_id".into(), serde_json::json!(site.id));
        body.insert(
            "collected_at".into(),
            serde_json::json!(time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        );
        let body_bytes = serde_json::to_vec(&serde_json::Value::Object(body.clone()))
            .map_err(|e| AppError::Internal(e.to_string()))?;
        let run = if req.dry_run {
            preview_run(state, &tool, &body_bytes)
                .await
                .map(|calculation| (None, calculation))
        } else {
            execute_and_store_run(state, &tool, &body_bytes, &actor, "csv_import")
                .await
                .map(|result| (Some(result.run_id), result.calculation))
        };
        let (run_id, calculation) = match run {
            Ok(run) => run,
            Err(e) => {
                record_error(line, e.to_string(), &mut errors, &mut error_count);
                continue;
            }
        };
        if run_id.is_some() {
            tool_runs_created += 1;
        }

        let mut readings: Vec<GrabSampleReading> = saved_outputs
            .iter()
            .filter_map(|(key, parameter_id)| {
                calculation
                    .results
                    .get(key)
                    .and_then(serde_json::Value::as_f64)
                    .map(|value| GrabSampleReading {
                        parameter_id: *parameter_id,
                        sensor_id: None,
                        value,
                        time,
                        replicate_index: None,
                        output: Some(key.clone()),
                        input: None,
                        standard_curve_id: None,
                    })
            })
            .collect();
        for (name, parameter_id) in &saved_inputs {
            let Some(values) = body.get(name).and_then(serde_json::Value::as_array) else {
                continue;
            };
            // A curve is fitted on one instrument, so the replicate it corrects is that
            // instrument's measurement.
            let standard_curve_id = replicate_curve(&tool.manifest.params, name, &curves);
            let sensor_id = standard_curve_id.map(|id| stored_curves[&id].sensor_id);
            for (position, cell) in values.iter().enumerate() {
                let (Some(value), Ok(replicate_index)) = (cell.as_f64(), i16::try_from(position))
                else {
                    continue;
                };
                readings.push(GrabSampleReading {
                    parameter_id: *parameter_id,
                    sensor_id,
                    value,
                    time,
                    replicate_index: Some(replicate_index),
                    output: None,
                    input: Some(name.clone()),
                    standard_curve_id,
                });
            }
        }
        if readings.is_empty() {
            record_error(
                line,
                "the run produced no savable output for this row".to_string(),
                &mut errors,
                &mut error_count,
            );
            continue;
        }
        computed.push((line, run_id, readings));
    }

    // --- Screen ---
    // Every value a row stores is screened, the replicates it entered and the outputs its run
    // published, and each save is held to the check that screened them.
    let cells: Vec<(usize, Uuid, chrono::DateTime<chrono::Utc>, f64)> = computed
        .iter()
        .flat_map(|(line, _, readings)| {
            readings
                .iter()
                .map(|r| (*line, r.parameter_id, r.time, r.value))
        })
        .collect();
    let check = screen_import(state, auth, req, site.id, &cells).await?;

    // --- Save ---
    let mut inserted_total = 0usize;
    if !req.dry_run {
        for (line, run_id, readings) in computed {
            let request = GrabSampleRequest {
                expected_replicates: None,
                pending_inputs: false,
                site_id: site.id,
                label: None,
                notes: None,
                mode: (req.conflict == ConflictMode::Overwrite).then_some(GrabWriteMode::Replace),
                dry_run: false,
                tool_run_id: run_id,
                check_id: check.check_id,
                // The tool's manifest is read by the save path itself; nothing here overrides
                // the slot's declaration.
                readings,
            };
            match insert_grab_samples(
                axum::extract::State(state.clone()),
                axum::Extension(auth.clone()),
                crate::common::middleware::ProjectScope(scope.clone()),
                axum::Json(request),
            )
            .await
            {
                Ok(axum::Json(resp)) => inserted_total += resp.inserted,
                Err(e) => record_error(line, e.to_string(), &mut errors, &mut error_count),
            }
        }
    }
    let check = Some(check);

    Ok(ImportCsvResponse {
        site_id: site.id,
        site_name: site.name.clone(),
        dry_run: req.dry_run,
        session_id: Some(session_id),
        mapped_columns,
        skipped_columns: Vec::new(),
        unmapped_columns,
        warnings,
        row_count,
        replicate_groups: 0,
        inserted_total,
        earliest,
        latest,
        derived_job_id: None,
        derived_timestamps: 0,
        duplicates: 0,
        overlaps_identical: 0,
        overlaps_differing: 0,
        overwritten: 0,
        overlap_sample: Vec::new(),
        errors,
        error_count,
        tool_runs_created,
        curves: curve_plan,
        check,
        site_imports: Vec::new(),
    })
}

/// Fraction of a window's stored rows a single pass may change or withdraw before the brake
/// holds the corrections and withdrawals (new rows always apply).
pub const RECONCILE_BRAKE_FRACTION: f64 = 0.15;

/// Fraction of a window's replicate groups that may lose one index before the brake fires, the
/// test that catches a truncated or mis-mapped member column the row-fraction test cannot see.
pub const RECONCILE_BRAKE_INDEX_FRACTION: f64 = 0.5;

/// Rows a pass must reshape before the fractions are consulted at all. The fractions were sized
/// for full-history windows (hundreds of rows); without a floor, one legitimate replicate
/// removal in a three-row window reads as a 33% reshape and brakes routine lab corrections.
pub const RECONCILE_BRAKE_MIN_ROWS: usize = 5;

pub(super) struct ReconciledRow {
    pub(super) raw_value: f64,
    pub(super) standard_curve_id: Option<Uuid>,
    pub(super) withdrawn: bool,
    /// The judgements standing on the row, as `{id, kind}`. Non-empty means a person has ruled
    /// on it, and the hold a collision raises names exactly these.
    pub(super) judgements: serde_json::Value,
    /// Whether a person has ruled on the row at all: a live judgement decision, or the curation
    /// columns of a row that predates the record (T20 gives those their decisions), or a sample
    /// an operator labelled or annotated.
    pub(super) touched: bool,
}

#[derive(Debug, Default)]
pub struct DiffOutcome {
    pub new_rows: usize,
    pub changed: usize,
    pub unchanged: usize,
    pub withdrawn: usize,
    pub retained: usize,
    pub reinstated: usize,
    pub braked: bool,
    pub holds_raised: usize,
    pub changed_keys: Vec<(DateTime<Utc>, i16)>,
    /// The keys this pass stamped withdrawn, and the keys it cleared the stamp from. Neither is a
    /// payload row (a withdrawn key is absent from the payload by construction), so a consumer
    /// working from the request alone cannot see the instants whose served value moved.
    pub withdrawn_keys: Vec<Key>,
    pub reinstated_keys: Vec<Key>,
    /// The keys the upsert should write: new rows only. A stored value the source has moved is
    /// proposed, never written (Q84), and an unchanged row re-written with identical values is
    /// WAL churn the diff exists to avoid.
    pub write_keys: HashSet<Key>,
    /// Changed keys recorded as proposals this pass, and how many of those are awaiting a
    /// decision. A pass leaving a proposal undecided is not clean, so the source keeps asserting
    /// the window and the proposal keeps its evidence.
    pub proposed: usize,
    pub proposals_awaiting: usize,
}

pub type Key = (DateTime<Utc>, i16);

/// One key the source has moved: what it now asserts, and what the store holds, each a value and
/// the curve declared with it.
pub(super) type ProposedChange = (Key, (f64, Option<Uuid>), (f64, Option<Uuid>));

/// One admitted payload row as the diff classifies it: key, raw value, declared curve.
pub type AdmittedRow = (Key, f64, Option<Uuid>);

pub(super) async fn stored_window<C: ConnectionTrait>(
    conn: &C,
    stream_id: Uuid,
    window: &SourceWindow,
) -> AppResult<HashMap<Key, ReconciledRow>> {
    let r = Alias::new("r");
    let judgements = live_judgements("r");
    let (sql, values) = Query::select()
        .column((r.clone(), readings::Column::Time))
        .column((r.clone(), readings::Column::ReplicateIndex))
        .column((r.clone(), readings::Column::RawValue))
        .column((r.clone(), readings::Column::StandardCurveId))
        .expr_as(
            Expr::col((r.clone(), readings::Column::WithdrawnAt)).is_not_null(),
            Alias::new("withdrawn"),
        )
        .expr_as(judgements.clone(), Alias::new("judgements"))
        // A row somebody has touched is one the source may not silently overwrite.
        .expr_as(
            Expr::from(
                Condition::any()
                    .add(Expr::expr(judgements).ne(Expr::cust("'[]'::jsonb")))
                    .add(Expr::cust("r.is_flagged IS TRUE"))
                    .add(Expr::col((r.clone(), readings::Column::FlagReason)).is_not_null())
                    .add(Expr::col((r.clone(), readings::Column::Label)).is_not_null())
                    .add(Expr::col((r.clone(), readings::Column::Notes)).is_not_null()),
            ),
            Alias::new("touched"),
        )
        .from_as(readings::Entity, r.clone())
        .cond_where(
            Condition::all()
                .add(Expr::col((r.clone(), readings::Column::StreamId)).eq(stream_id))
                .add(Expr::col((r.clone(), readings::Column::Time)).gte(window.from))
                .add(Expr::col((r, readings::Column::Time)).lt(window.to)),
        )
        .to_owned()
        .build(PostgresQueryBuilder);
    let rows = StoredWindowRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .all(conn)
    .await?;
    let mut out = HashMap::with_capacity(rows.len());
    for row in rows {
        out.insert(
            (row.time.with_timezone(&Utc), row.replicate_index),
            ReconciledRow {
                raw_value: row.raw_value,
                standard_curve_id: row.standard_curve_id,
                withdrawn: row.withdrawn,
                judgements: row.judgements,
                touched: row.touched,
            },
        );
    }
    Ok(out)
}

/// The two shapes with no state in which acknowledging them is correct: refused, nothing applied.
pub fn refuse_dishonest_window(
    window: &SourceWindow,
    admitted: usize,
    stored_in_window: usize,
) -> AppResult<()> {
    if stored_in_window == 0 {
        return Ok(());
    }
    if admitted == 0 && window.source_rows_read > 0 {
        return Err(AppError::BadRequest(format!(
            "The window claims {} source rows but the payload is empty while the store holds \
             {stored_in_window} readings in it; an empty payload is never read as a deletion",
            window.source_rows_read
        )));
    }
    if window.source_rows_read == 0 {
        return Err(AppError::BadRequest(format!(
            "The window claims zero source rows over a period the store holds {stored_in_window} \
             readings for; assert the real source content or narrow the window"
        )));
    }
    Ok(())
}

/// Which scale rule a pass trips, or `None` when it is within both. The floor is consulted first:
/// the fractions were sized for full-history windows, so a small window reshapes freely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Brake {
    /// The pass would change or withdraw too large a share of the window's stored rows.
    Fraction,
    /// One replicate index is withdrawn from too many of the groups that carry it, the shape a
    /// truncated or mis-mapped member column takes and the row fraction cannot see.
    IndexLoss,
}

/// `stored` is every row the window holds, `would_touch` the rows the pass would change or
/// withdraw, and `per_index` one `(withdrawn, stored)` pair per replicate index the pass withdraws
/// from.
pub fn brake_verdict(
    stored: usize,
    would_touch: usize,
    per_index: &[(usize, usize)],
) -> Option<Brake> {
    if would_touch < RECONCILE_BRAKE_MIN_ROWS {
        return None;
    }
    if stored > 0 && (would_touch as f64) / (stored as f64) > RECONCILE_BRAKE_FRACTION {
        return Some(Brake::Fraction);
    }
    if per_index.iter().any(|(withdrawn, total)| {
        *total > 0 && (*withdrawn as f64) / (*total as f64) > RECONCILE_BRAKE_INDEX_FRACTION
    }) {
        return Some(Brake::IndexLoss);
    }
    None
}

pub(crate) async fn upsert_source_modified_hold<C: ConnectionTrait>(
    conn: &C,
    stream_id: Uuid,
    group_time: DateTime<Utc>,
    expected: serde_json::Value,
    computed: serde_json::Value,
    status: HoldStatus,
) -> AppResult<()> {
    audit::upsert_hold(
        conn,
        &audit::Hold {
            key: audit::HoldKey::Stream {
                stream_id,
                group_time,
            },
            kind: HoldKind::SourceModified,
            expected,
            computed,
            delta: serde_json::json!({}),
            status,
            tool: None,
        },
    )
    .await
}

pub(super) async fn upsert_brake_hold<C: ConnectionTrait>(
    conn: &C,
    stream_id: Uuid,
    window: &SourceWindow,
    changed: usize,
    withdrawn: usize,
    stored: usize,
    status: HoldStatus,
) -> AppResult<()> {
    audit::upsert_hold(
        conn,
        &audit::Hold {
            key: audit::HoldKey::Stream {
                stream_id,
                group_time: window.from,
            },
            kind: HoldKind::BrakeFired,
            expected: serde_json::json!({
                "window": { "from": window.from, "to": window.to },
                "would_change": changed,
                "would_withdraw": withdrawn,
                "stored_in_window": stored,
                "threshold": RECONCILE_BRAKE_FRACTION,
            }),
            computed: serde_json::json!({ "held": "changed and withdrawn; new rows applied" }),
            delta: serde_json::json!({}),
            status,
            tool: None,
        },
    )
    .await
}

/// Classify the window and apply the withdrawal side. Runs inside the guarded transaction,
/// before the insert/upsert of the admitted rows.
#[allow(clippy::too_many_lines)]
pub async fn run_windowed_diff<C: ConnectionTrait>(
    conn: &C,
    stream_id: Uuid,
    window: &SourceWindow,
    admitted: &[AdmittedRow],
    rejected_keys: &HashSet<Key>,
    actor: &str,
    paired: bool,
) -> AppResult<DiffOutcome> {
    let status = audit::status_for(paired);
    let stored = stored_window(conn, stream_id, window).await?;
    refuse_dishonest_window(window, admitted.len(), stored.len())?;

    let dropped_times: HashSet<DateTime<Utc>> = window.dropped_times.iter().copied().collect();

    let mut outcome = DiffOutcome {
        new_rows: 0,
        changed: 0,
        unchanged: 0,
        withdrawn: 0,
        retained: 0,
        reinstated: 0,
        braked: false,
        holds_raised: 0,
        changed_keys: Vec::new(),
        withdrawn_keys: Vec::new(),
        reinstated_keys: Vec::new(),
        write_keys: HashSet::new(),
        proposed: 0,
        proposals_awaiting: 0,
    };

    let mut admitted_keys: HashSet<Key> = HashSet::with_capacity(admitted.len());
    let mut reinstate: Vec<Key> = Vec::new();
    // Every key the source has moved, with what it now asserts and what the store holds.
    let mut proposals: Vec<ProposedChange> = Vec::new();
    for (key, raw_value, standard_curve_id) in admitted {
        admitted_keys.insert(*key);
        // Keys outside the claimed window are plain appends and classify as new.
        match stored.get(key) {
            None => {
                outcome.new_rows += 1;
                outcome.write_keys.insert(*key);
            }
            Some(row) => {
                let equal =
                    row.raw_value == *raw_value && row.standard_curve_id == *standard_curve_id;
                if equal {
                    outcome.unchanged += 1;
                } else {
                    outcome.changed += 1;
                    if outcome.changed_keys.len() < 500 {
                        outcome.changed_keys.push(*key);
                    }
                    proposals.push((
                        *key,
                        (*raw_value, *standard_curve_id),
                        (row.raw_value, row.standard_curve_id),
                    ));
                }
                // An honest window re-asserting a row clears its retraction, equal or corrected.
                if row.withdrawn {
                    reinstate.push(*key);
                }
            }
        }
    }

    // Withdrawal, computed defensively: absent from the payload, not refused by the funnel, not
    // dropped by the backend, not already withdrawn.
    let mut to_withdraw: Vec<Key> = Vec::new();
    let mut withdraw_touched: Vec<(Key, serde_json::Value)> = Vec::new();
    for (key, row) in &stored {
        if admitted_keys.contains(key) || rejected_keys.contains(key) {
            continue;
        }
        if dropped_times.contains(&key.0) {
            outcome.retained += 1;
            continue;
        }
        if row.withdrawn {
            continue;
        }
        if row.touched {
            withdraw_touched.push((*key, row.judgements.clone()));
        } else {
            to_withdraw.push(*key);
        }
    }
    outcome.retained += rejected_keys
        .iter()
        .filter(|k| stored.contains_key(*k))
        .count();

    // The brake: a pass reshaping the stored window at scale holds its corrections and
    // withdrawals for review; new rows still apply so ingestion never stops.
    let would_touch = outcome.changed + to_withdraw.len() + withdraw_touched.len();
    let mut groups_with_index: HashMap<i16, usize> = HashMap::new();
    let mut withdrawn_with_index: HashMap<i16, usize> = HashMap::new();
    for (_, i) in stored.keys() {
        *groups_with_index.entry(*i).or_default() += 1;
    }
    for (_, i) in to_withdraw
        .iter()
        .chain(withdraw_touched.iter().map(|(key, _)| key))
    {
        *withdrawn_with_index.entry(*i).or_default() += 1;
    }
    let per_index: Vec<(usize, usize)> = withdrawn_with_index
        .iter()
        .filter_map(|(i, n)| groups_with_index.get(i).map(|total| (*n, *total)))
        .collect();
    if brake_verdict(stored.len(), would_touch, &per_index).is_some() {
        // The release path: an operator who acknowledged this stream's brake_fired hold has
        // ruled that the reshape is legitimate, so exactly one braked-scale pass applies and the
        // ruling is consumed (hold -> remediated). The source re-asserts the same window every
        // cycle, so "acknowledge, then let the next cycle through" is the whole workflow; a
        // later reshape brakes afresh with a new hold.
        let release = crate::routes::private::sync::hold_model::Entity::find()
            .filter(crate::routes::private::sync::hold_model::Column::StreamId.eq(stream_id))
            .filter(
                crate::routes::private::sync::hold_model::Column::Kind
                    .eq(HoldKind::BrakeFired.as_str()),
            )
            .filter(
                crate::routes::private::sync::hold_model::Column::Status
                    .eq(HoldStatus::Acknowledged.as_str()),
            )
            .order_by_desc(crate::routes::private::sync::hold_model::Column::CreatedAt)
            .one(conn)
            .await?;
        match release {
            Some(hold) => {
                crate::routes::private::sync::hold_model::Entity::update_many()
                    .col_expr(
                        crate::routes::private::sync::hold_model::Column::Status,
                        Expr::val(HoldStatus::Remediated.as_str()),
                    )
                    .filter(crate::routes::private::sync::hold_model::Column::Id.eq(hold.id))
                    .filter(
                        crate::routes::private::sync::hold_model::Column::Status
                            .eq(HoldStatus::Acknowledged.as_str()),
                    )
                    .exec(conn)
                    .await?;
                tracing::info!(%stream_id, changed = outcome.changed,
                    withdrawn = to_withdraw.len() + withdraw_touched.len(),
                    "acknowledged brake released; reshape applies once");
            }
            None => {
                outcome.braked = true;
                upsert_brake_hold(
                    conn,
                    stream_id,
                    window,
                    outcome.changed,
                    to_withdraw.len() + withdraw_touched.len(),
                    stored.len(),
                    status,
                )
                .await?;
                outcome.holds_raised += 1;
                return Ok(outcome);
            }
        }
    }

    // A value the source has moved since river-data stored it is proposed, never written (Q84):
    // the stored number changes when a person accepts the proposal, and the source re-asserting
    // the same number every cycle re-proposes nothing.
    for (key, proposed, stored_value) in &proposals {
        if propose(conn, stream_id, *key, *proposed, *stored_value).await? {
            outcome.proposals_awaiting += 1;
        }
        outcome.proposed += 1;
    }

    // Curated rows never change servedness without a person: the withdrawal is not stamped and
    // the disagreement lands in the review queue. A corrected value is a proposal for everyone,
    // curated or not, so a curated row needs no hold of its own for it.
    for (key, judgements) in &withdraw_touched {
        upsert_source_modified_hold(
            conn,
            stream_id,
            key.0,
            serde_json::json!({ "claim": "withdrawn", "replicate_index": key.1,
                                "judgements": judgements,
                                "window": { "from": window.from, "to": window.to } }),
            serde_json::json!({ "kept_served": true }),
            status,
        )
        .await?;
        outcome.holds_raised += 1;
    }
    // A withdrawal and a reinstatement are decisions of sync origin (ADR 0008): the source
    // retracted or re-asserted the row, and the record says so beside any operator's judgement.
    if !to_withdraw.is_empty() {
        let rows: Vec<(DateTime<Utc>, i16, serde_json::Value)> = to_withdraw
            .iter()
            .map(|(t, i)| {
                (
                    *t,
                    *i,
                    serde_json::json!({ "reason": "absent from source window" }),
                )
            })
            .collect();
        record_keyed(
            conn,
            crate::routes::private::readings::models::Kind::Withdraw,
            stream_id,
            &rows,
            actor,
            Some("absent from source window"),
            crate::routes::private::readings::models::Origin::Sync,
            Keyed::All,
            None,
            None,
        )
        .await?;
        outcome.withdrawn = to_withdraw.len();
        outcome.withdrawn_keys = to_withdraw.to_vec();
    }

    if !reinstate.is_empty() {
        let rows: Vec<(DateTime<Utc>, i16, serde_json::Value)> = reinstate
            .iter()
            .map(|(t, i)| (*t, *i, serde_json::json!({})))
            .collect();
        record_keyed(
            conn,
            crate::routes::private::readings::models::Kind::Reassert,
            stream_id,
            &rows,
            actor,
            Some("re-asserted by the source window"),
            crate::routes::private::readings::models::Origin::Sync,
            Keyed::All,
            None,
            None,
        )
        .await?;
        outcome.reinstated = reinstate.len();
        outcome.reinstated_keys = reinstate.to_vec();
    }

    Ok(outcome)
}

/// Commit the pass's receipt. The arithmetic CHECK (`submitted = new + changed + unchanged +
/// rejected_total`) makes a write path that cannot account for a submitted row unable to commit.
#[allow(clippy::too_many_arguments)]
pub async fn write_receipt<C: ConnectionTrait>(
    conn: &C,
    stream_id: Uuid,
    window: &SourceWindow,
    submitted: usize,
    outcome: &DiffOutcome,
    rejected_total: usize,
    rejected: &serde_json::Value,
) -> AppResult<()> {
    let changed_keys = if outcome.changed_keys.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::json!(
            outcome
                .changed_keys
                .iter()
                .map(|(t, i)| serde_json::json!([t, i]))
                .collect::<Vec<_>>()
        )
    };
    let count = |n: usize| i32::try_from(n).unwrap_or(i32::MAX);
    receipts::ActiveModel {
        id: Set(Uuid::new_v4()),
        stream_id: Set(stream_id),
        at: Set(chrono::Utc::now().into()),
        window_from: Set(Some(window.from.into())),
        window_to: Set(Some(window.to.into())),
        submitted: Set(count(submitted)),
        new_rows: Set(count(outcome.new_rows)),
        changed: Set(count(outcome.changed)),
        unchanged: Set(count(outcome.unchanged)),
        retained: Set(count(outcome.retained)),
        rejected_total: Set(count(rejected_total)),
        rejected: Set(rejected.clone()),
        dropped: Set(count(window.dropped_times.len())),
        withdrawn: Set(count(outcome.withdrawn)),
        changed_keys: Set(Some(changed_keys)),
        braked: Set(outcome.braked),
        brake_threshold: Set(Some(RECONCILE_BRAKE_FRACTION as f32)),
        proposed: Set(count(outcome.proposed)),
    }
    .insert(conn)
    .await?;
    Ok(())
}

#[cfg(test)]
#[path = "tests/attribution.rs"]
mod attribution_tests;

#[cfg(test)]
#[path = "tests/measurement.rs"]
mod measurement;

#[cfg(test)]
#[path = "tests/sample_groups.rs"]
mod sample_groups;

#[cfg(test)]
#[path = "tests/sample_preview.rs"]
mod sample_preview;

#[cfg(test)]
#[path = "tests/checks.rs"]
mod checks;

#[cfg(test)]
#[path = "tests/decisions.rs"]
mod decisions;

#[cfg(test)]
#[path = "tests/edits.rs"]
mod edits;

#[cfg(test)]
#[path = "tests/proposals.rs"]
mod proposals;

#[cfg(test)]
#[path = "tests/provenance.rs"]
mod provenance;

#[cfg(test)]
#[path = "tests/tail.rs"]
mod tail;

#[cfg(test)]
#[path = "tests/ingest.rs"]
mod ingest;

#[cfg(test)]
#[path = "tests/batch.rs"]
mod batch;

#[cfg(test)]
#[path = "tests/batch_correction.rs"]
mod batch_correction_tests;

// --- Statements the handlers and job bodies share ---

/// The readings whose stored curation columns disagree with what their decision record asserts,
/// newest first. A reading outside the caller's projects is not theirs to see, and an unpaired one
/// belongs to no project at all, so a scoped caller is shown neither.
#[must_use]
pub(super) fn curation_drift_query(
    scope: &crate::common::authz::AccessScope,
    limit: u32,
) -> sea_orm::sea_query::SelectStatement {
    use crate::routes::private::sites::models as sites;
    let d = Alias::new("d");
    let r = Alias::new("r");
    let st = Alias::new("st");
    Query::select()
        .column((d.clone(), Alias::new("stream_id")))
        .column((d.clone(), Alias::new("time")))
        .column((d.clone(), Alias::new("replicate_index")))
        .column((r.clone(), readings::Column::SiteId))
        .column((r.clone(), readings::Column::ParameterId))
        .expr_as(
            Expr::cust(
                "jsonb_strip_nulls(jsonb_build_object(\
                     'is_flagged', d.is_flagged, 'flag_reason', d.flag_reason, \
                     'withdrawn_at', d.withdrawn_at, 'withdrawn_reason', d.withdrawn_reason, \
                     'unverified', d.unverified, 'standard_curve_id', d.standard_curve_id, \
                     'calibration_id', d.calibration_id, 'sensor_id', d.sensor_id, \
                     'raw_value', d.raw_value))",
            ),
            Alias::new("stored"),
        )
        .expr_as(
            Expr::cust("COALESCE(d.folded, '{}'::jsonb)"),
            Alias::new("folded"),
        )
        .from_subquery(
            crate::routes::private::readings::service::inconsistent_rows(),
            d.clone(),
        )
        .join_as(
            JoinType::InnerJoin,
            readings::Entity,
            r.clone(),
            Condition::all()
                .add(
                    Expr::col((r.clone(), readings::Column::StreamId))
                        .equals((d.clone(), Alias::new("stream_id"))),
                )
                .add(
                    Expr::col((r.clone(), readings::Column::Time))
                        .equals((d.clone(), Alias::new("time"))),
                )
                .add(
                    Expr::col((r.clone(), readings::Column::ReplicateIndex))
                        .equals((d.clone(), Alias::new("replicate_index"))),
                ),
        )
        .join_as(
            JoinType::LeftJoin,
            sites::Entity,
            st.clone(),
            Expr::col((st.clone(), sites::Column::Id)).equals((r, readings::Column::SiteId)),
        )
        .cond_where(
            Condition::all().add_option(crate::common::scope::project_filter(
                scope,
                (st, sites::Column::ProjectId),
            )),
        )
        .order_by((d, Alias::new("time")), sea_orm::Order::Desc)
        .limit(u64::from(limit))
        .take()
}

/// The drift rows themselves, as the handler serves them.
pub(super) async fn curation_drift_rows<C: ConnectionTrait>(
    conn: &C,
    scope: &crate::common::authz::AccessScope,
    limit: u32,
) -> AppResult<Vec<CurationDriftRow>> {
    let (sql, values) = curation_drift_query(scope, limit).build(PostgresQueryBuilder);
    conn.query_all_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .await?
    .iter()
    .map(|row| Ok(CurationDriftRow::from_query_result(row, "")?))
    .collect()
}

/// What the source said at a replicate-statistics hold, for the preview to compare against.
/// `None` when no such hold sits on that instant.
pub(super) async fn hold_expectation<C: ConnectionTrait>(
    conn: &C,
    hold_id: Uuid,
    at: chrono::DateTime<chrono::FixedOffset>,
) -> AppResult<Option<serde_json::Value>> {
    Ok(hold_model::Entity::find()
        .filter(hold_model::Column::Id.eq(hold_id))
        .filter(hold_model::Column::Kind.eq(HoldKind::ReplicateStats.as_str()))
        .filter(hold_model::Column::GroupTime.eq(at))
        .one(conn)
        .await?
        .map(|hold| hold.expected))
}

/// Which of these streams declare a replicate family. A family's replicates sync from the source,
/// so no other writer may mint an index onto one.
pub(super) async fn replicate_family_keys<C: ConnectionTrait>(
    conn: &C,
    stream_ids: &[Uuid],
) -> AppResult<HashMap<Uuid, String>> {
    if stream_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let s = Alias::new("s");
    let (sql, values) = Query::select()
        .column((s.clone(), data_streams::models::Column::Id))
        .column((s.clone(), data_streams::models::Column::SourceKey))
        .from_as(data_streams::models::Entity, s.clone())
        .and_where(
            Expr::col((s.clone(), data_streams::models::Column::Id))
                .is_in(stream_ids.iter().copied()),
        )
        .and_where(data_streams::service::declares_replicates(&s))
        .to_owned()
        .build(PostgresQueryBuilder);
    let mut out = HashMap::new();
    for row in conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?
    {
        let row = FamilyKeyRow::from_query_result(&row, "")?;
        out.insert(row.id, row.source_key);
    }
    Ok(out)
}

/// The streams a retag would reach that declare a different classification of their own. Ingest
/// keeps writing a declared value, so the retag would drift back and the conflict is reported
/// rather than silently overwritten.
pub(super) async fn streams_declaring_other_type<C: ConnectionTrait>(
    conn: &C,
    target: &str,
    sensor_ids: &[Uuid],
    stream_ids: &[Uuid],
    source_system: Option<&str>,
) -> AppResult<Vec<(String, String)>> {
    let mut reached = Condition::any()
        .add(Expr::col(data_streams::models::Column::SensorId).is_in(sensor_ids.iter().copied()))
        .add(Expr::col(data_streams::models::Column::Id).is_in(stream_ids.iter().copied()));
    if let Some(system) = source_system {
        reached = reached.add(Expr::col(data_streams::models::Column::SourceSystem).eq(system));
    }
    let (sql, values) = Query::select()
        .column(data_streams::models::Column::SourceSystem)
        .column(data_streams::models::Column::SourceKey)
        .from(data_streams::models::Entity)
        .and_where(Expr::col(data_streams::models::Column::MeasurementType).is_not_null())
        .and_where(Expr::col(data_streams::models::Column::MeasurementType).ne(target))
        .cond_where(reached)
        .to_owned()
        .build(PostgresQueryBuilder);
    let mut out = Vec::new();
    for row in conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?
    {
        out.push((
            row.try_get::<String>("", "source_system")?,
            row.try_get::<String>("", "source_key")?,
        ));
    }
    Ok(out)
}

/// A site's parameter columns, as the importer resolves a header against them: the slot's own
/// name, the catalog code and the catalog aliases.
pub(super) async fn site_parameter_columns<C: ConnectionTrait>(
    conn: &C,
    site_id: Uuid,
) -> AppResult<Vec<SlotColumnRow>> {
    let sp = Alias::new("sp");
    let p = Alias::new("p");
    let (sql, values) = Query::select()
        .expr_as(
            Expr::col((sp.clone(), site_parameters::Column::Name)),
            Alias::new("sp_name"),
        )
        .column((sp.clone(), site_parameters::Column::ParameterId))
        .expr_as(
            Expr::col((p.clone(), parameters::Column::Code)),
            Alias::new("param_name"),
        )
        .column((p.clone(), parameters::Column::Aliases))
        .from_as(site_parameters::Entity, sp.clone())
        .join_as(
            JoinType::InnerJoin,
            parameters::Entity,
            p.clone(),
            Expr::col((p, parameters::Column::Id))
                .equals((sp.clone(), site_parameters::Column::ParameterId)),
        )
        .and_where(Expr::col((sp, site_parameters::Column::SiteId)).eq(site_id))
        .to_owned()
        .build(PostgresQueryBuilder);
    let rows = conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?;
    Ok(rows
        .iter()
        .filter_map(|row| SlotColumnRow::from_query_result(row, "").ok())
        .collect())
}

/// Every parameter a calculation writes. A derived output is computed, never ingested, so the
/// importer refuses a column naming one.
pub(super) async fn derived_output_parameter_ids<C: ConnectionTrait>(
    conn: &C,
) -> AppResult<HashSet<Uuid>> {
    Ok(calculation_formulas::Entity::find()
        .filter(calculation_formulas::Column::OutputParameterId.is_not_null())
        .select_only()
        .column(calculation_formulas::Column::OutputParameterId)
        .into_tuple::<Option<Uuid>>()
        .all(conn)
        .await?
        .into_iter()
        .flatten()
        .collect())
}

/// The `api` stream already carrying each of these slots at this site, which is where an imported
/// row lands.
pub(super) async fn api_streams_of_slots<C: ConnectionTrait>(
    conn: &C,
    site_id: Uuid,
    parameter_ids: &[Uuid],
) -> AppResult<Vec<SlotStreamRow>> {
    let ds = Alias::new("ds");
    let sp = Alias::new("sp");
    let (sql, values) = Query::select()
        .column((sp.clone(), site_parameters::Column::ParameterId))
        .column((ds.clone(), data_streams::models::Column::Id))
        .from_as(data_streams::models::Entity, ds.clone())
        .join_as(
            JoinType::InnerJoin,
            site_parameters::Entity,
            sp.clone(),
            Expr::col((sp.clone(), site_parameters::Column::Id))
                .equals((ds.clone(), data_streams::models::Column::SiteParameterId)),
        )
        .and_where(Expr::col((ds, data_streams::models::Column::SourceSystem)).eq("api"))
        .and_where(Expr::col((sp.clone(), site_parameters::Column::SiteId)).eq(site_id))
        .and_where(
            Expr::col((sp, site_parameters::Column::ParameterId))
                .is_in(parameter_ids.iter().copied()),
        )
        .to_owned()
        .build(PostgresQueryBuilder);
    let rows = conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?;
    rows.iter()
        .map(|row| Ok(SlotStreamRow::from_query_result(row, "")?))
        .collect()
}

#[cfg(test)]
#[path = "tests/grab_samples.rs"]
mod grab_samples;

#[cfg(test)]
#[path = "tests/import.rs"]
mod import;

#[cfg(test)]
#[path = "tests/reconcile.rs"]
mod reconcile;

/// Adds referenced names to reading responses through CRUD hooks.
pub struct ReadingOperations;

impl crudcrate::CRUDOperations for ReadingOperations {
    type Resource = super::models::Reading;

    async fn after_get_one<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        entity: &mut super::models::Reading,
    ) -> Result<(), crudcrate::ApiError> {
        label_readings(db, std::slice::from_mut(entity)).await
    }

    async fn after_get_all<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        entities: &mut Vec<super::models::ReadingList>,
    ) -> Result<(), crudcrate::ApiError> {
        label_readings(db, entities.as_mut_slice()).await
    }
}

trait ReadingLabelled {
    fn ids(&self) -> RowIds;
    fn label(&mut self, labels: &Labels);
}

struct RowIds {
    stream: Uuid,
    site: Option<Uuid>,
    parameter: Option<Uuid>,
    sensor: Option<Uuid>,
    calibration: Option<Uuid>,
    standard_curve: Option<Uuid>,
}

macro_rules! reading_labelled {
    ($t:ty) => {
        impl ReadingLabelled for $t {
            fn ids(&self) -> RowIds {
                RowIds {
                    stream: self.stream_id,
                    site: self.site_id,
                    parameter: self.parameter_id,
                    sensor: self.sensor_id,
                    calibration: self.calibration_id,
                    standard_curve: self.standard_curve_id,
                }
            }
            fn label(&mut self, labels: &Labels) {
                let ids = self.ids();
                if let Some((system, key)) = labels.streams.get(&ids.stream) {
                    self.source_system = Some(system.clone());
                    self.source_key = Some(key.clone());
                }
                self.site_name = ids.site.and_then(|id| labels.sites.get(&id).cloned());
                if let Some((code, units)) = ids.parameter.and_then(|id| labels.parameters.get(&id))
                {
                    self.parameter_code = Some(code.clone());
                    self.units = Some(units.clone());
                }
                self.instrument_name = ids.sensor.and_then(|id| labels.sensors.get(&id).cloned());
                self.calibration = ids
                    .calibration
                    .and_then(|id| labels.calibrations.get(&id).cloned());
                self.curve = ids
                    .standard_curve
                    .and_then(|id| labels.curves.get(&id).cloned());
            }
        }
    };
}

reading_labelled!(super::models::Reading);
reading_labelled!(super::models::ReadingList);

struct Labels {
    streams: HashMap<Uuid, (String, String)>,
    sites: HashMap<Uuid, String>,
    parameters: HashMap<Uuid, (String, String)>,
    sensors: HashMap<Uuid, String>,
    calibrations: HashMap<Uuid, ReadingCalibrationRef>,
    curves: HashMap<Uuid, ReadingCurveRef>,
}

async fn label_readings<C: ConnectionTrait, T: ReadingLabelled>(
    db: &C,
    rows: &mut [T],
) -> Result<(), crudcrate::ApiError> {
    if rows.is_empty() {
        return Ok(());
    }
    let labels = load_labels(db, rows.iter().map(ReadingLabelled::ids)).await?;
    for row in rows {
        row.label(&labels);
    }
    Ok(())
}

fn distinct(ids: impl Iterator<Item = Option<Uuid>>) -> Vec<Uuid> {
    ids.flatten().collect::<HashSet<_>>().into_iter().collect()
}

async fn load_labels<C: ConnectionTrait>(
    db: &C,
    rows: impl Iterator<Item = RowIds>,
) -> Result<Labels, sea_orm::DbErr> {
    let rows: Vec<RowIds> = rows.collect();
    let streams = load_reading_streams(db, &rows).await?;
    let sites = load_reading_sites(db, &rows).await?;
    let parameters = load_reading_parameters(db, &rows).await?;
    let sensors = load_reading_sensors(db, &rows).await?;
    let calibrations = load_reading_calibrations(db, &rows).await?;
    let curves = load_reading_curves(db, &rows).await?;
    Ok(Labels {
        streams,
        sites,
        parameters,
        sensors,
        calibrations,
        curves,
    })
}

async fn load_reading_streams<C: ConnectionTrait>(
    db: &C,
    rows: &[RowIds],
) -> Result<HashMap<Uuid, (String, String)>, sea_orm::DbErr> {
    Ok(data_streams::Entity::find()
        .filter(data_streams::Column::Id.is_in(distinct(rows.iter().map(|r| Some(r.stream)))))
        .all(db)
        .await?
        .into_iter()
        .map(|s| (s.id, (s.source_system, s.source_key)))
        .collect())
}

async fn load_reading_sites<C: ConnectionTrait>(
    db: &C,
    rows: &[RowIds],
) -> Result<HashMap<Uuid, String>, sea_orm::DbErr> {
    Ok(sites::Entity::find()
        .filter(sites::Column::Id.is_in(distinct(rows.iter().map(|r| r.site))))
        .all(db)
        .await?
        .into_iter()
        .map(|s| (s.id, s.name))
        .collect())
}

async fn load_reading_parameters<C: ConnectionTrait>(
    db: &C,
    rows: &[RowIds],
) -> Result<HashMap<Uuid, (String, String)>, sea_orm::DbErr> {
    Ok(parameters::Entity::find()
        .filter(parameters::Column::Id.is_in(distinct(rows.iter().map(|r| r.parameter))))
        .all(db)
        .await?
        .into_iter()
        .map(|p| (p.id, (p.code, p.default_units)))
        .collect())
}

async fn load_reading_sensors<C: ConnectionTrait>(
    db: &C,
    rows: &[RowIds],
) -> Result<HashMap<Uuid, String>, sea_orm::DbErr> {
    Ok(sensors::Entity::find()
        .filter(sensors::Column::Id.is_in(distinct(rows.iter().map(|r| r.sensor))))
        .all(db)
        .await?
        .into_iter()
        .map(|s| {
            let name = s
                .name
                .or(s.serial_number)
                .unwrap_or_else(|| s.id.to_string());
            (s.id, name)
        })
        .collect())
}

async fn load_reading_calibrations<C: ConnectionTrait>(
    db: &C,
    rows: &[RowIds],
) -> Result<HashMap<Uuid, ReadingCalibrationRef>, sea_orm::DbErr> {
    Ok(sensor_calibrations::Entity::find()
        .filter(sensor_calibrations::Column::Id.is_in(distinct(rows.iter().map(|r| r.calibration))))
        .all(db)
        .await?
        .into_iter()
        .map(|c| {
            (
                c.id,
                ReadingCalibrationRef {
                    id: c.id,
                    name: c.name,
                    slope: c.slope,
                    intercept: c.intercept,
                    valid_from: c.valid_from,
                    valid_until: c.valid_until,
                },
            )
        })
        .collect())
}

async fn load_reading_curves<C: ConnectionTrait>(
    db: &C,
    rows: &[RowIds],
) -> Result<HashMap<Uuid, ReadingCurveRef>, sea_orm::DbErr> {
    Ok(standard_curves::Entity::find()
        .filter(standard_curves::Column::Id.is_in(distinct(rows.iter().map(|r| r.standard_curve))))
        .all(db)
        .await?
        .into_iter()
        .map(|c| {
            (
                c.id,
                ReadingCurveRef {
                    id: c.id,
                    name: c.name,
                    slope: c.slope,
                    intercept: c.intercept,
                },
            )
        })
        .collect())
}
