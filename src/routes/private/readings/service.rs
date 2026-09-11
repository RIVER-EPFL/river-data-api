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
use sea_orm::entity::prelude::*;
use sea_orm::sea_query::Alias;
use sea_orm::sea_query::CommonTableExpression;
use sea_orm::sea_query::Expr;
use sea_orm::sea_query::Func;
use sea_orm::sea_query::JoinType;
use sea_orm::sea_query::Order;
use sea_orm::sea_query::PostgresQueryBuilder;
use sea_orm::sea_query::Query;
use sea_orm::sea_query::WithClause;
use sea_orm::sea_query::extension::postgres::PgBinOper;
use utoipa::ToSchema;
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
use crate::routes::private::collection_events;
use crate::routes::private::collection_events::flows;
use crate::routes::private::collection_events::flows::TouchedEvent;
use crate::routes::private::data_streams;
use crate::routes::private::data_streams::models::receipts;
use crate::routes::private::derived_parameters::models::definition as calculation_formulas;
use crate::routes::private::derived_parameters::models::version as derived_versions;
use crate::routes::private::readings;
use crate::routes::private::readings::models::ConflictMode;
use crate::routes::private::readings::models::Kind;
use crate::routes::private::readings::models::Origin;
use crate::routes::private::readings::models::Owner;
use crate::routes::private::readings::models::ProvenanceQuery;
use crate::routes::private::readings::models::Selection;
use crate::routes::private::readings::samples;
use crate::routes::private::reprocessing_jobs::job::Job;
use crate::routes::private::sensors;
use crate::routes::private::sensor_calibrations;
use crate::routes::private::sensor_deployments as deployments;
use crate::routes::private::standard_curves;
use crate::routes::private::sites;
use crate::routes::private::site_parameters;
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
        Some(other) => MeasurementType::from_str(other).is_none().then(|| {
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
    (MeasurementType::from_str(value).is_none() && value != RETAG_DECLARED).then(|| {
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

pub const SAMPLE: &str = "sample";

pub const POPULATION: &str = "population";

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

/// A readings select narrowed to the columns a [`PreviewRow`] decodes: the slot, the replicate's
/// served value, and whether it is currently excluded from the statistics.
pub(super) fn preview_rows() -> sea_orm::Select<readings::Entity> {
    readings::Entity::find()
        .select_only()
        .column(readings::Column::SiteId)
        .column(readings::Column::ParameterId)
        .column(readings::Column::ReplicateIndex)
        .column_as(effective_value(None), "value")
        .column_as(Expr::cust("is_flagged IS TRUE"), "flagged")
        .column_as(
            Expr::col(readings::Column::WithdrawnAt).is_not_null(),
            "withdrawn",
        )
}

/// Why a replicate survived a grab replace, in the order the reasons are checked.
pub(super) fn kept_reason() -> Expr {
    sea_orm::sea_query::CaseStatement::new()
        .case(Expr::cust("is_flagged IS TRUE"), "flagged")
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
        .add(Expr::cust("is_flagged IS TRUE"))
        .add(readings::Column::WithdrawnAt.is_not_null());
    if !supplies_curve {
        kept = kept.add(readings::Column::StandardCurveId.is_not_null());
    }
    kept
}

/// Where a sample's estimator came from, most specific first. Stored on the row beside the value
/// it chose, so "computed under no declaration" stays distinguishable from "declared sample".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A person chose it for this one collection group (an audit resolution scoped to the instant).
    Sample,
    /// A tool's manifest fixed it, or its operator chose it for this run.
    Tool,
    /// The stream's registered replicate spec declares it.
    Stream,
    /// The slot declares it: `site_parameters.sd_estimator`.
    Slot,
    /// Nothing declared one. The fallback applied and this row is undeclared.
    Default,
}

impl Source {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sample => "sample",
            Self::Tool => "tool",
            Self::Stream => "stream",
            Self::Slot => "slot",
            Self::Default => "default",
        }
    }
}

/// A resolved estimator: the value a sample is computed with, and what chose it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolved {
    pub estimator: &'static str,
    pub source: Source,
}

impl Resolved {
    /// The undeclared fallback: a sample sd, recorded as chosen by nothing.
    #[must_use]
    pub const fn undeclared() -> Self {
        Self {
            estimator: SAMPLE,
            source: Source::Default,
        }
    }

    #[must_use]
    pub const fn is_declared(&self) -> bool {
        !matches!(self.source, Source::Default)
    }
}

/// Reject anything outside the two divisors. A stored estimator is a specification, so an unknown
/// value is refused at the edge rather than falling back to one of them.
pub fn parse(value: &str) -> AppResult<&'static str> {
    match value {
        SAMPLE => Ok(SAMPLE),
        POPULATION => Ok(POPULATION),
        other => Err(AppError::BadRequest(format!(
            "unknown sd estimator '{other}'; expected 'sample' or 'population'"
        ))),
    }
}

/// The same check for an optional field.
pub fn parse_opt(value: Option<&str>) -> AppResult<Option<&'static str>> {
    value.map(parse).transpose()
}

/// The slot's declaration, or None when the slot has not declared one.
pub async fn slot_declaration<C: ConnectionTrait>(
    conn: &C,
    site_id: Uuid,
    parameter_id: Uuid,
) -> AppResult<Option<&'static str>> {
    let row = site_parameters::Entity::find()
        .filter(site_parameters::Column::SiteId.eq(site_id))
        .filter(site_parameters::Column::ParameterId.eq(parameter_id))
        .filter(site_parameters::Column::SdEstimator.is_not_null())
        .one(conn)
        .await?;
    let Some(row) = row else { return Ok(None) };
    let stored = row.sd_estimator;
    // A value outside the two is not reachable through the CHECK constraint; treat it as
    // undeclared rather than failing a read.
    Ok(stored.as_deref().and_then(|v| match v {
        SAMPLE => Some(SAMPLE),
        POPULATION => Some(POPULATION),
        _ => None,
    }))
}

/// The estimator a stored collection group already carries because a person chose it there: an
/// audit resolution scoped to the instant (`sd_estimator_source = 'sample'`). It belongs to the
/// group, not the slot, so it is read by instant and outranks every slot-level declaration.
pub async fn instant_declaration<C: ConnectionTrait>(
    conn: &C,
    site_id: Uuid,
    parameter_id: Uuid,
    collected_at: chrono::DateTime<chrono::Utc>,
) -> AppResult<Option<&'static str>> {
    let stored = samples::Entity::find()
        .select_only()
        .column(samples::Column::SdEstimator)
        .filter(samples::Column::SiteId.eq(site_id))
        .filter(samples::Column::ParameterId.eq(parameter_id))
        .filter(samples::Column::CollectedAt.eq(collected_at))
        .filter(samples::Column::SdEstimatorSource.eq(SAMPLE))
        .into_tuple::<String>()
        .one(conn)
        .await?;
    Ok(stored.as_deref().and_then(|v| match v {
        SAMPLE => Some(SAMPLE),
        POPULATION => Some(POPULATION),
        _ => None,
    }))
}

/// Resolve one slot's estimator, most specific wins: an explicit request value, then the stream's
/// spec, then the slot's declaration, then the undeclared fallback.
///
/// `explicit` carries its own [`Source`] because the two callers that supply one mean different
/// things by it (a tool run versus an operator's decision about one instant).
pub async fn resolve<C: ConnectionTrait>(
    conn: &C,
    site_id: Uuid,
    parameter_id: Uuid,
    explicit: Option<(&'static str, Source)>,
    stream_spec: Option<&'static str>,
) -> AppResult<Resolved> {
    if let Some((estimator, source)) = explicit {
        return Ok(Resolved { estimator, source });
    }
    if let Some(estimator) = stream_spec {
        return Ok(Resolved {
            estimator,
            source: Source::Stream,
        });
    }
    if let Some(estimator) = slot_declaration(conn, site_id, parameter_id).await? {
        return Ok(Resolved {
            estimator,
            source: Source::Slot,
        });
    }
    Ok(Resolved::undeclared())
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
/// A spot instant with no sample row is by definition a single measurement, which is why serving,
/// the visits grid and every export derive n = 1 from the reading rather than from a row here.
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
    materialise_samples_with_estimator(conn, rows, None).await
}

/// [`materialise_samples`] for a caller that knows the stream's declared sd estimator.
///
/// The estimator is resolved per group rather than per call: one predicate can span several slots,
/// and each carries its own declaration. `stream_spec` is the stream's own declaration, which wins
/// over the slot's; absent it, the slot decides, and absent that the group is recorded undeclared.
pub async fn materialise_samples_with_estimator<C: ConnectionTrait>(
    conn: &C,
    rows: Condition,
    stream_spec: Option<&str>,
) -> AppResult<()> {
    let g = Alias::new("g");
    let sp = Alias::new("sp");

    // The estimator each new row is computed with, and what chose it, decided in the insert so a
    // group can never exist without both recorded. A stream declaration outranks the slot's; with
    // neither, the row is stamped `default`, which is the undeclared state the report lists and
    // the audit gate reads.
    let declared_by_stream = parse_opt(stream_spec)?;
    let (estimator, source) = match declared_by_stream {
        Some(declared) => (Expr::val(declared), Expr::val("stream")),
        None => (
            sea_orm::sea_query::Func::coalesce([
                Expr::col((sp.clone(), site_parameters::Column::SdEstimator)),
                Expr::val(SAMPLE),
            ])
            .into(),
            sea_orm::sea_query::CaseStatement::new()
                .case(
                    Expr::col((sp.clone(), site_parameters::Column::SdEstimator)).is_null(),
                    "default",
                )
                .finally("slot")
                .into(),
        ),
    };
    let groups = Query::select()
        .column((g.clone(), samples::Column::SiteId))
        .column((g.clone(), samples::Column::ParameterId))
        .column((g.clone(), readings::Column::Time))
        .expr(estimator)
        .expr(source)
        .from_subquery(group_select(rows.clone()), g.clone())
        .join_as(
            JoinType::LeftJoin,
            site_parameters::Entity,
            sp.clone(),
            Expr::from(
                Condition::all()
                    .add(
                        Expr::col((sp.clone(), site_parameters::Column::SiteId))
                            .equals((g.clone(), samples::Column::SiteId)),
                    )
                    .add(
                        Expr::col((sp, site_parameters::Column::ParameterId))
                            .equals((g.clone(), samples::Column::ParameterId)),
                    ),
            ),
        )
        .to_owned();
    let mut insert = Query::insert();
    insert
        .into_table(samples::Entity)
        .columns([
            samples::Column::SiteId,
            samples::Column::ParameterId,
            samples::Column::CollectedAt,
            samples::Column::SdEstimator,
            samples::Column::SdEstimatorSource,
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
}

/// The change being previewed.
#[derive(Debug, Clone, Default)]
pub struct Change<'a> {
    pub exclude: &'a [i16],
    pub include: &'a [i16],
    pub estimator: Option<&'static str>,
}

pub(super) fn stats_over(values: &[f64], estimator: &'static str) -> PreviewStats {
    let s = audit::group_stats(values).under(estimator);
    PreviewStats {
        n: s.n,
        mean: s.mean,
        sd: s.sd,
        sd_estimator: estimator,
    }
}

/// The statistics now and after the change, over the same rule the samples trigger applies:
/// unflagged, unwithdrawn replicates only. A withdrawn replicate is outside every count and no
/// flag change brings it back.
#[must_use]
pub fn preview_statistics(
    replicates: &[Replicate],
    current_estimator: &'static str,
    change: &Change<'_>,
) -> (
    PreviewStats,
    PreviewStats,
    PreviewDelta,
    Vec<PreviewReplicate>,
) {
    let proposed_estimator = change.estimator.unwrap_or(current_estimator);
    let mut rows = Vec::with_capacity(replicates.len());
    let mut now = Vec::new();
    let mut after = Vec::new();
    for r in replicates {
        let included_now = !r.flagged && !r.withdrawn;
        let included_after = !r.withdrawn
            && !change.exclude.contains(&r.index)
            && (!r.flagged || change.include.contains(&r.index));
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
    let current = stats_over(&now, current_estimator);
    let proposed = stats_over(&after, proposed_estimator);
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

/// Cap on the per-parameter distribution sample returned for plotting.
pub(super) const DISTRIBUTION_CAP: i64 = 500;

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
                .add(Expr::cust("is_flagged IS NOT TRUE"))
                .add(readings::Column::WithdrawnAt.is_null())
                .add(Expr::cust("unverified IS NOT TRUE"))
                // Cyclic month distance, so December is two months from February.
                .add(Expr::cust_with_values(
                    "LEAST(\
                       (EXTRACT(MONTH FROM time)::int \
                        - EXTRACT(MONTH FROM $1::timestamptz)::int + 12) % 12, \
                       (EXTRACT(MONTH FROM $1::timestamptz)::int \
                        - EXTRACT(MONTH FROM time)::int + 12) % 12) <= $2",
                    [
                        sea_orm::Value::from(at),
                        sea_orm::Value::from(WINDOW_MONTHS),
                    ],
                )),
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
        pooled: "Spot (grab) readings only. Flagged and withdrawn readings are excluded. \
                 Replicates enter as individual values, not as their sample mean.",
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
    SdEstimatorRetag,
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
            | Self::MeasurementRetag
            | Self::SdEstimatorRetag => None,
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
            "migration" => Origin::Migration,
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
    }
    .insert(conn)
    .await?;
    if d.kind == Kind::ValueCorrection {
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
            crate::routes::private::collection_events::flows::touched_events(
                conn,
                crate::routes::private::collection_events::flows::rows_matching(
                    "r.stream_id = $1 AND r.time = $2 \
                     AND ($3::smallint IS NULL OR r.replicate_index = $3)",
                    key_binds(&key),
                ),
            )
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
    if d.kind == Kind::ValueCorrection {
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
        .expr(Expr::cust_with_values(
            "(SELECT d.id FROM reading_decisions d \
               WHERE d.stream_id = t.stream_id AND d.time = t.time \
                 AND d.replicate_index IS NOT DISTINCT FROM t.replicate_index \
                 AND d.kind = ANY($1) AND d.rolled_back_by IS NULL \
               ORDER BY d.at DESC, d.id DESC LIMIT 1)",
            [sea_orm::Value::from(family_kinds(kind))],
        ))
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
    times: Vec<String>,
    indices: Vec<i32>,
    news: Vec<String>,
) -> sea_orm::sea_query::SelectStatement {
    let unnest = |array: Expr, of: &'static str| {
        Expr::from(
            sea_orm::sea_query::Func::cust(Alias::new("unnest")).arg(array.cast_as(Alias::new(of))),
        )
    };
    Query::select()
        .expr_as(unnest(Expr::val(times), "timestamptz[]"), Alias::new("t"))
        .expr_as(unnest(Expr::val(indices), "smallint[]"), Alias::new("ri"))
        .expr_as(unnest(Expr::val(news), "jsonb[]"), Alias::new("n"))
        .to_owned()
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
    let times: Vec<String> = rows.iter().map(|(t, _, _)| t.to_rfc3339()).collect();
    let indices: Vec<i32> = rows.iter().map(|(_, i, _)| i32::from(*i)).collect();
    let news: Vec<String> = rows.iter().map(|(_, _, n)| n.to_string()).collect();
    let cols: Vec<String> = kind
        .recorded_columns()
        .iter()
        .map(|c| (*c).to_string())
        .collect();

    let r = Alias::new("r");
    let k = Alias::new("k");
    let t = Alias::new("t");
    let state = state_object(Some("r"));
    let keys = key_set(times.clone(), indices.clone(), news);
    let already_decided = Query::select()
        .expr(Expr::val(1))
        .from_as(decision_model::Entity, Alias::new("d"))
        .cond_where(Expr::cust_with_values(
            "d.stream_id = r.stream_id AND d.time = r.time \
             AND d.replicate_index IS NOT DISTINCT FROM r.replicate_index \
             AND d.kind = $1 AND d.rolled_back_by IS NULL AND d.new @> k.n",
            [sea_orm::Value::from(kind.as_str())],
        ))
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
        .expr(Expr::cust_with_values(
            "(SELECT d.id FROM reading_decisions d \
               WHERE d.stream_id = t.stream_id AND d.time = t.time \
                 AND d.replicate_index IS NOT DISTINCT FROM t.replicate_index \
                 AND d.kind = ANY($1) AND d.rolled_back_by IS NULL \
               ORDER BY d.at DESC, d.id DESC LIMIT 1)",
            [sea_orm::Value::from(family_kinds(kind))],
        ))
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
        recorded.touched_events =
            crate::routes::private::collection_events::flows::touched_events(
                conn,
                crate::routes::private::collection_events::flows::rows_matching(
                    "r.stream_id = $1 AND (r.time, r.replicate_index) IN \
                     (SELECT t, ri FROM unnest($2::text[]::timestamptz[], $3::int[]::smallint[]) AS k(t, ri))",
                    vec![stream_id.into(), times.into(), indices.into()],
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

/// SQL over `alias` (a `readings` row) that is true when no live pin of `kind` covers the row.
/// Every derivation that would rewrite the pinned column carries this, so a pin outlives reprocess.
#[must_use]
pub fn not_pinned_sql(alias: &str, kind: Kind) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM reading_decisions d \
             WHERE d.stream_id = {alias}.stream_id AND d.time = {alias}.time \
               AND (d.replicate_index IS NULL OR d.replicate_index = {alias}.replicate_index) \
               AND d.kind = '{}' AND d.rolled_back_by IS NULL)",
        kind.as_str()
    )
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

/// The same fold in SQL, anti-joined against the readings: every key whose folded columns are not
/// what its live decisions say they should be, with the columns the reading holds and the map its
/// decisions fold to (`folded`), so a caller can show a person which side says what. Report-only, because which side is wrong is a
/// decision (a rollback, or a fresh decision), never something a sweep may pick.
///
/// It folds every column a decision's own assertion determines. The one it cannot is
/// `calibrated_value`, which is recomposed from the row's own curves rather than asserted, so it
/// cannot be predicted from `new`; a decision records it as `old` for a rollback to restore. The
/// columns only some kinds assert are compared only where a decision asserted one, so a row
/// carrying an instrument nothing pinned is not drift.
#[must_use]
pub fn inconsistent_rows_sql() -> String {
    "WITH candidate AS (
         SELECT r.stream_id, r.time, r.replicate_index,
                COALESCE(r.is_flagged, false) AS is_flagged, r.flag_reason,
                r.withdrawn_at, r.withdrawn_reason, COALESCE(r.unverified, false) AS unverified,
                r.standard_curve_id, r.calibration_id, r.sensor_id, r.raw_value
         FROM readings r
         WHERE r.is_flagged IS TRUE OR r.flag_reason IS NOT NULL
            OR r.withdrawn_at IS NOT NULL OR r.withdrawn_reason IS NOT NULL
            OR r.unverified IS TRUE
            OR EXISTS (SELECT 1 FROM reading_decisions d
                        WHERE d.stream_id = r.stream_id AND d.time = r.time
                          AND (d.replicate_index IS NULL
                               OR d.replicate_index = r.replicate_index)
                          AND d.rolled_back_by IS NULL)
     )
     SELECT c.*, e.m AS folded
     FROM candidate c
     LEFT JOIN LATERAL (
         SELECT jsonb_object_agg(a.col, a.val) AS m
         FROM (
             SELECT DISTINCT ON (v.col) v.col, v.val
             FROM reading_decisions d
             CROSS JOIN LATERAL (VALUES
                 ('is_flagged', CASE d.kind
                      WHEN 'flag' THEN 'true'::jsonb
                      WHEN 'unflag' THEN 'false'::jsonb
                      WHEN 'rollback' THEN d.new -> 'columns' -> 'is_flagged' END),
                 ('flag_reason', CASE d.kind
                      WHEN 'flag' THEN COALESCE(d.new -> 'reason', 'null'::jsonb)
                      WHEN 'unflag' THEN 'null'::jsonb
                      WHEN 'rollback' THEN d.new -> 'columns' -> 'flag_reason' END),
                 ('withdrawn_at', CASE
                      WHEN d.kind IN ('withdraw', 'reject')
                          THEN to_jsonb(COALESCE((d.new ->> 'withdrawn_at')::timestamptz, d.at))
                      WHEN d.kind = 'reassert' THEN 'null'::jsonb
                      WHEN d.kind = 'rollback' THEN d.new -> 'columns' -> 'withdrawn_at' END),
                 ('withdrawn_reason', CASE
                      WHEN d.kind IN ('withdraw', 'reject')
                          THEN COALESCE(d.new -> 'reason', 'null'::jsonb)
                      WHEN d.kind = 'reassert' THEN 'null'::jsonb
                      WHEN d.kind = 'rollback' THEN d.new -> 'columns' -> 'withdrawn_reason' END),
                 ('unverified', CASE
                      WHEN d.kind = 'unverified_entry' THEN 'true'::jsonb
                      WHEN d.kind IN ('verify', 'reject') THEN 'false'::jsonb
                      WHEN d.kind = 'rollback' THEN d.new -> 'columns' -> 'unverified' END),
                 ('standard_curve_id', CASE d.kind
                      WHEN 'curve' THEN d.new -> 'standard_curve_id'
                      WHEN 'rollback' THEN d.new -> 'columns' -> 'standard_curve_id' END),
                 ('calibration_id', CASE d.kind
                      WHEN 'calibration_pin' THEN d.new -> 'calibration_id'
                      WHEN 'rollback' THEN d.new -> 'columns' -> 'calibration_id' END),
                 ('sensor_id', CASE d.kind
                      WHEN 'instrument_pin' THEN d.new -> 'sensor_id'
                      WHEN 'rollback' THEN d.new -> 'columns' -> 'sensor_id' END),
                 ('raw_value', CASE d.kind
                      WHEN 'value_correction' THEN d.new -> 'raw_value'
                      WHEN 'rollback' THEN d.new -> 'columns' -> 'raw_value' END)
             ) AS v(col, val)
             WHERE d.stream_id = c.stream_id AND d.time = c.time
               AND (d.replicate_index IS NULL OR d.replicate_index = c.replicate_index)
               AND d.rolled_back_by IS NULL AND v.val IS NOT NULL
             ORDER BY v.col, d.at DESC, d.id DESC
         ) a
     ) e ON TRUE
     WHERE c.is_flagged IS DISTINCT FROM COALESCE((e.m ->> 'is_flagged')::boolean, false)
        OR c.flag_reason IS DISTINCT FROM (e.m ->> 'flag_reason')
        OR c.withdrawn_at IS DISTINCT FROM (e.m ->> 'withdrawn_at')::timestamptz
        OR c.withdrawn_reason IS DISTINCT FROM (e.m ->> 'withdrawn_reason')
        OR c.unverified IS DISTINCT FROM COALESCE((e.m ->> 'unverified')::boolean, false)
        OR (e.m ? 'standard_curve_id'
            AND c.standard_curve_id IS DISTINCT FROM (e.m ->> 'standard_curve_id')::uuid)
        OR (e.m ? 'calibration_id'
            AND c.calibration_id IS DISTINCT FROM (e.m ->> 'calibration_id')::uuid)
        OR (e.m ? 'sensor_id' AND c.sensor_id IS DISTINCT FROM (e.m ->> 'sensor_id')::uuid)
        OR (e.m ? 'raw_value'
            AND c.raw_value IS DISTINCT FROM (e.m ->> 'raw_value')::double precision)"
        .to_string()
}

/// How many readings the record and the columns disagree about. The janitor reports it; nothing
/// repairs it.
pub async fn curation_drift_count<C: ConnectionTrait>(conn: &C) -> AppResult<i64> {
    let sql = format!(
        "SELECT count(*)::bigint AS n FROM ({}) drift",
        inconsistent_rows_sql()
    );
    let row = conn
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            sql,
        ))
        .await?
        .ok_or_else(|| AppError::Internal("counting curation drift returned no row".to_string()))?;
    Ok(row.try_get("", "n")?)
}

/// Record one decision per reading a selection covers, as one set. Returns the set id and what
/// was recorded.
/// Put the corrected value back under the rows a value correction touched.
///
/// The projection writes `raw_value` and leaves `calibrated_value` NULL, because a corrected value
/// is a claim about the curves the row itself names rather than a number a decision may record.
/// This recomposes it from exactly those curves, so value and provenance move together.
pub(super) async fn recompose_corrected<C: ConnectionTrait>(
    conn: &C,
    scope_sql: &str,
    params: Vec<sea_orm::Value>,
) -> AppResult<()> {
    crate::routes::private::sensor_calibrations::service::recompose_from_own_curves(
        conn,
        &crate::routes::private::sensor_calibrations::service::corrected_rows("r"),
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
    if kind == Kind::ValueCorrection && recorded.rows > 0 {
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
    let rows = conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id FROM reading_decisions WHERE set_id = $1 AND rolled_back_by IS NULL",
            [set_id.into()],
        ))
        .await?;
    let mut n = 0usize;
    let mut recorded = Recorded::default();
    for row in &rows {
        let id: Uuid = row.try_get("", "id")?;
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
        if let Some(job) = crate::routes::private::reprocessing_jobs::worker::enqueue(
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
    let rows = ReceiptRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT id, at, submitted, new_rows, changed, unchanged, withdrawn, rejected_total, \
                braked \
           FROM ingest_receipts \
          WHERE stream_id = ANY($1) AND window_from <= $2 AND window_to >= $2 \
          ORDER BY at DESC",
        [streams.to_vec().into(), time.into()],
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
        return hold_rows(conn, "stream_id = ANY($1)", vec![streams.to_vec().into()]).await;
    };
    hold_rows(
        conn,
        "stream_id = ANY($1) OR (site_id = $2 AND parameter_id = $3)",
        vec![streams.to_vec().into(), site_id.into(), parameter_id.into()],
    )
    .await
}

pub(super) async fn hold_rows<C: ConnectionTrait>(
    conn: &C,
    predicate: &str,
    binds: Vec<sea_orm::Value>,
) -> AppResult<Vec<LedgerEntry>> {
    let rows = LedgerHoldRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "SELECT id, kind, status, created_at, tool FROM replicate_audit_holds \
              WHERE {predicate} ORDER BY created_at DESC"
        ),
        binds,
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
        .from_raw_sql(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT * FROM tool_runs \
              WHERE id = ANY($1) OR context->>'collection_event_id' = ANY($2) \
              ORDER BY created_at DESC",
            [runs.to_vec().into(), event_keys.into()],
        ))
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
    let rows = JobRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT id, trigger_type, status, error_message, created_at, completed_at, \
                readings_updated \
           FROM reprocessing_jobs \
          WHERE (params->>'site_id' = $1 AND params->>'parameter_id' = $2) \
             OR params->>'collection_event_id' = ANY($3) \
          ORDER BY created_at DESC",
        [
            site_id.to_string().into(),
            parameter_id.to_string().into(),
            event_keys.into(),
        ],
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
    let rows = JobLogRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT job_id, level, message, ts FROM reprocessing_job_logs \
          WHERE job_id = ANY($1) AND level <> 'info' ORDER BY ts DESC",
        [jobs.to_vec().into()],
    ))
    .all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| LedgerEntry {
            at: r.ts,
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
    let rows = ChangeRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT c.id, c.change, c.old_value, c.new_value, c.changed_by, c.changed_at \
           FROM change_audit c \
          WHERE c.subject = 'parameter:' || $2::text \
             OR c.subject IN (SELECT 'site_parameter:' || sp.id::text FROM site_parameters sp \
                               WHERE sp.site_id = $1 AND sp.parameter_id = $2) \
          ORDER BY c.changed_at DESC",
        [site_id.into(), parameter_id.into()],
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
    let rows = AlarmRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT id, severity, max_severity, started_at, resolved_at, acknowledged_by, \
                measurement_type \
           FROM alarm_events \
          WHERE site_id = $1 AND parameter_id = $2 \
            AND started_at <= $3 AND COALESCE(resolved_at, last_seen_at) >= $3",
        [site_id.into(), parameter_id.into(), time.into()],
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
        // either way rather than falling silent where a pin used to be offered.
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
) -> AppResult<Vec<InspectedRow>> {
    let (sql, values) = stored_rows()
        .cond_where(selection.condition()?)
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
    for row in inspect_rows(conn, selection).await? {
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

/// What every edit owes after its transaction commits, forward or inverted: the rollups over the
/// span it moved, and the calculations at the visits it touched.
pub(super) async fn propagate(state: &AppState, recorded: &Recorded, actor: &str) -> AppResult<()> {
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
pub async fn propose<C: ConnectionTrait>(
    conn: &C,
    stream_id: Uuid,
    key: (DateTime<Utc>, i16),
    proposed: (f64, Option<Uuid>),
    stored: (f64, Option<Uuid>),
) -> AppResult<bool> {
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            // A row whose proposed value is unchanged keeps its status and its decision, and only
            // its `last_seen_at` moves: the source is still asserting the number that was ruled on.
            r"INSERT INTO reading_change_proposals
                  (stream_id, time, replicate_index, proposed_raw_value, proposed_standard_curve_id,
                   stored_raw_value, stored_standard_curve_id)
              VALUES ($1, $2, $3, $4, $5, $6, $7)
              ON CONFLICT (stream_id, time, replicate_index) DO UPDATE
                 SET last_seen_at = now(),
                     stored_raw_value = EXCLUDED.stored_raw_value,
                     stored_standard_curve_id = EXCLUDED.stored_standard_curve_id,
                     proposed_raw_value = EXCLUDED.proposed_raw_value,
                     proposed_standard_curve_id = EXCLUDED.proposed_standard_curve_id,
                     status = CASE
                         WHEN reading_change_proposals.proposed_raw_value IS DISTINCT FROM EXCLUDED.proposed_raw_value
                           OR reading_change_proposals.proposed_standard_curve_id IS DISTINCT FROM EXCLUDED.proposed_standard_curve_id
                         THEN 'pending' ELSE reading_change_proposals.status END,
                     decided_by = CASE
                         WHEN reading_change_proposals.proposed_raw_value IS DISTINCT FROM EXCLUDED.proposed_raw_value
                           OR reading_change_proposals.proposed_standard_curve_id IS DISTINCT FROM EXCLUDED.proposed_standard_curve_id
                         THEN NULL ELSE reading_change_proposals.decided_by END,
                     decided_at = CASE
                         WHEN reading_change_proposals.proposed_raw_value IS DISTINCT FROM EXCLUDED.proposed_raw_value
                           OR reading_change_proposals.proposed_standard_curve_id IS DISTINCT FROM EXCLUDED.proposed_standard_curve_id
                         THEN NULL ELSE reading_change_proposals.decided_at END
              RETURNING status = 'pending' AND decided_at IS NULL AS awaiting",
            [
                stream_id.into(),
                key.0.into(),
                key.1.into(),
                proposed.0.into(),
                proposed.1.into(),
                stored.0.into(),
                stored.1.into(),
            ],
        ))
        .await?;
    Ok(row
        .map(|r| r.try_get::<bool>("", "awaiting"))
        .transpose()?
        .unwrap_or(false))
}

pub(super) const SELECT: &str = r"SELECT p.id, p.stream_id, ds.source_system, ds.source_key,
       sp.site_id, s.name AS site_name, sp.parameter_id, par.code AS parameter_code,
       p.time, p.replicate_index, p.stored_raw_value, p.proposed_raw_value,
       p.stored_standard_curve_id, p.proposed_standard_curve_id,
       p.status, p.first_seen_at, p.last_seen_at, p.decided_by, p.decided_at
  FROM reading_change_proposals p
  JOIN data_streams ds ON ds.id = p.stream_id
  LEFT JOIN site_parameters sp ON sp.id = ds.site_parameter_id
  LEFT JOIN sites s ON s.id = sp.site_id
  LEFT JOIN parameters par ON par.id = sp.parameter_id";

/// Every proposal matching the filter, newest source assertion first.
pub async fn list(
    db: &DatabaseConnection,
    status: Option<&str>,
    stream_id: Option<Uuid>,
    projects: Option<sea_orm::Value>,
) -> AppResult<Vec<Proposal>> {
    let mut sql = SELECT.to_string();
    let mut binds: Vec<sea_orm::Value> = Vec::new();
    let mut clauses: Vec<String> = Vec::new();
    if let Some(status) = status {
        binds.push(status.into());
        clauses.push(format!("p.status = ${}", binds.len()));
    }
    if let Some(stream_id) = stream_id {
        binds.push(stream_id.into());
        clauses.push(format!("p.stream_id = ${}", binds.len()));
    }
    if let Some(projects) = projects {
        // A scoped caller sees a proposal only where the pairing places it in one of its projects;
        // an unpaired stream belongs to no project and is not theirs to rule on.
        binds.push(projects);
        clauses.push(format!("s.project_id = ANY(${})", binds.len()));
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY p.last_seen_at DESC, p.time DESC LIMIT 1000");
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            binds,
        ))
        .await?;
    rows.iter()
        .map(|r| Proposal::from_query_result(r, ""))
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

/// The proposals a caller may decide, out of the ids they named. A restricted caller reaches only
/// what their projects hold, so an id outside them is simply not found and comes back refused: the
/// same confinement the listing applies, at the write.
pub(super) async fn load_undecided<C: ConnectionTrait>(
    conn: &C,
    ids: &[Uuid],
    projects: Option<sea_orm::Value>,
) -> AppResult<Vec<Pending>> {
    let mut binds: Vec<sea_orm::Value> = vec![ids.to_vec().into()];
    let scope = if let Some(projects) = projects {
        binds.push(projects);
        format!(
            " AND EXISTS (SELECT 1 FROM data_streams ds \
                 JOIN site_parameters sp ON sp.id = ds.site_parameter_id \
                 JOIN sites s ON s.id = sp.site_id \
                WHERE ds.id = p.stream_id AND s.project_id = ANY(${}))",
            binds.len()
        )
    } else {
        String::new()
    };
    let rows = conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT p.id, p.stream_id, p.time, p.replicate_index, p.proposed_raw_value,
                        p.proposed_standard_curve_id, p.stored_standard_curve_id
                   FROM reading_change_proposals p
                  WHERE p.id = ANY($1) AND p.status <> 'accepted'{scope}
                  FOR UPDATE OF p"
            ),
            binds,
        ))
        .await?;
    rows.iter()
        .map(|r| Pending::from_query_result(r, ""))
        .collect::<Result<Vec<_>, _>>()
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
    projects: Option<sea_orm::Value>,
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
        response.accepted = pending.len();
    } else {
        response.rejected = pending.len();
    }
    if !found.is_empty() {
        conn.execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE reading_change_proposals
                SET status = $2, decided_by = $3, decided_at = now()
              WHERE id = ANY($1)",
            [
                found.clone().into(),
                if accept { "accepted" } else { "rejected" }.into(),
                actor.into(),
            ],
        ))
        .await?;
    }
    Ok((response, written))
}

/// How many proposals are awaiting a decision, per source system. The notification's subject.
pub async fn pending_by_source(db: &DatabaseConnection) -> AppResult<Vec<(String, i64)>> {
    let rows = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT ds.source_system AS source_system, count(*) AS n
               FROM reading_change_proposals p
               JOIN data_streams ds ON ds.id = p.stream_id
              WHERE p.status = 'pending'
              GROUP BY ds.source_system"
                .to_string(),
        ))
        .await?;
    rows.iter()
        .map(|r| SourceCount::from_query_result(r, "").map(|c| (c.source_system, c.n)))
        .collect::<Result<Vec<_>, _>>()
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
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT stream_id, replicate_index, MAX(at) AS at
             FROM reading_decisions
             WHERE stream_id = ANY($1) AND time = $2
               AND kind = 'value_correction' AND rolled_back_by IS NULL
               AND replicate_index IS NOT NULL
             GROUP BY stream_id, replicate_index",
            [
                stream_ids.to_vec().into(),
                sea_orm::prelude::DateTimeWithTimeZone::from(at).into(),
            ],
        ))
        .await?;
    let mut out = HashMap::new();
    for r in rows.iter().map(|r| ArrivalRow::from_query_result(r, "")) {
        let r = r?;
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
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT stream_id, id, kind, replicate_index, new, actor, at, reason, set_id
             FROM reading_decisions
             WHERE stream_id = ANY($1) AND time = $2
               AND kind IN ('instrument_pin', 'calibration_pin') AND rolled_back_by IS NULL
             ORDER BY at DESC",
            [
                stream_ids.to_vec().into(),
                sea_orm::prelude::DateTimeWithTimeZone::from(at).into(),
            ],
        ))
        .await?;
    let mut out: HashMap<Uuid, Vec<PinRef>> = HashMap::new();
    for r in rows.iter().map(|r| PinRow::from_query_result(r, "")) {
        let r = r?;
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
pub const PROVENANCE_KINDS: [&str; 8] = [
    "tool_run",
    "chain",
    "csv_import",
    "manual",
    "batch",
    "sync",
    "derived",
    "migration",
];

/// The kind a writer with no better evidence stamps, from the row's own classification and the
/// stream it arrived on. Mirrors `readings_default_provenance_kind`, the trigger that holds the
/// column total for a writer that names none.
#[must_use]
pub fn provenance_kind_for_stream(
    measurement_type: Option<&str>,
    source_system: Option<&str>,
) -> &'static str {
    if measurement_type == Some("derived") {
        return "derived";
    }
    match source_system {
        Some("grab_sample") => "manual",
        Some("api") => "batch",
        Some(_) => "sync",
        None => "migration",
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
    let calibration_map: HashMap<Uuid, sensor_calibrations::Model> = sensor_calibrations::Entity::find()
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
    let (calculations, formula_versions) = fetch_calculations(db, rows).await?;
    let links = fetch_formula_links(db, rows).await?;
    let served = fetch_served_values(db, rows, &links, time).await?;

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
        // still has one; the sample adds the estimator its stored sd was computed with.
        let sample = group
            .iter()
            .find_map(|r| r.sample_id)
            .and_then(|id| sample_map.get(&id));
        let blob = group.iter().find_map(|r| r.provenance.clone());
        let entered_by = group.iter().find_map(|r| r.created_by.clone());
        let computation = if blob.is_some() || entered_by.is_some() || sample.is_some() {
            let run_source = run_id_of(blob.as_ref()).and_then(|id| run_sources.get(&id).cloned());
            Some(ComputationInfo {
                sample_id: sample.map(|s| s.id),
                created_by: entered_by,
                label: group.iter().find_map(|r| r.label.clone()),
                notes: group.iter().find_map(|r| r.notes.clone()),
                provenance: blob,
                run_source,
                sd_estimator: sample.map(|s| s.sd_estimator.clone()),
                sd_estimator_source: sample.map(|s| s.sd_estimator_source.clone()),
                n: sample.map(|s| s.n),
                mean: sample.and_then(|s| s.mean),
                stdev: sample.and_then(|s| s.stdev),
                stdev_sample: sample.and_then(|s| s.stdev_sample),
                stdev_population: sample.and_then(|s| s.stdev_population),
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
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT DISTINCT ON (stream_id) stream_id, id, at, window_from, window_to, \
                    submitted, new_rows, changed, unchanged, withdrawn, rejected_total, braked \
             FROM ingest_receipts \
             WHERE stream_id = ANY($1) AND window_from <= $2 AND window_to >= $2 \
             ORDER BY stream_id, at DESC",
            [stream_ids.to_vec().into(), time.into()],
        ))
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
fn live_hold_statuses() -> String {
    HoldStatus::sql_list(&[
        HoldStatus::Pending,
        HoldStatus::Deferred,
        HoldStatus::Acknowledged,
    ])
}

/// Replicate-statistics holds keyed by stream at the instant. Terminal holds are left out.
pub(super) async fn fetch_stream_holds(
    db: &sea_orm::DatabaseConnection,
    stream_ids: &[Uuid],
    time: DateTime<Utc>,
) -> AppResult<HashMap<Uuid, Vec<HoldRef>>> {
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT stream_id, NULL::uuid AS parameter_id, id, kind, status, created_at \
                 FROM replicate_audit_holds \
                 WHERE stream_id = ANY($1) AND group_time = $2 \
                   AND status IN {live} \
                 ORDER BY created_at DESC",
                live = live_hold_statuses()
            ),
            [stream_ids.to_vec().into(), time.into()],
        ))
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
            .query_all_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    "SELECT NULL::uuid AS stream_id, parameter_id, id, kind, status, created_at \
                     FROM replicate_audit_holds \
                     WHERE stream_id IS NULL AND site_id = $1 AND parameter_id = ANY($2) \
                       AND group_time = $3 AND status IN {live} \
                     ORDER BY created_at DESC",
                    live = live_hold_statuses()
                ),
                [
                    site_id.into(),
                    parameter_ids.into_iter().collect::<Vec<_>>().into(),
                    time.into(),
                ],
            ))
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

/// The minting path of every tool run the rows' provenance blobs name.
pub(super) async fn fetch_run_sources(
    db: &sea_orm::DatabaseConnection,
    rows: &[RawRow],
) -> AppResult<HashMap<Uuid, String>> {
    let run_ids: Vec<Uuid> = rows
        .iter()
        .filter_map(|r| run_id_of(r.provenance.as_ref()))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    if run_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let found: Vec<(Uuid, String)> = tool_run::Entity::find()
        .filter(tool_run::Column::Id.is_in(run_ids))
        .select_only()
        .column(tool_run::Column::Id)
        .column(tool_run::Column::Source)
        .into_tuple()
        .all(db)
        .await?;
    Ok(found.into_iter().collect())
}

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
    let definitions: Vec<(Uuid, String, String, Option<Uuid>)> =
        calculation_formulas::Entity::find()
            .filter(calculation_formulas::Column::OutputParameterId.is_in(parameter_ids))
            .select_only()
            .column(calculation_formulas::Column::Id)
            .column(calculation_formulas::Column::Code)
            .column(calculation_formulas::Column::Name)
            .column(calculation_formulas::Column::OutputParameterId)
            .into_tuple()
            .all(db)
            .await?;
    // The newest version each of them has, which is what "active" means for a standalone formula.
    let mut active_version: HashMap<Uuid, i32> = HashMap::new();
    for (definition_id, version_no) in derived_versions::Entity::find()
        .filter(
            derived_versions::Column::DefinitionId
                .is_in(definitions.iter().map(|(id, ..)| *id).collect::<Vec<_>>()),
        )
        .select_only()
        .column(derived_versions::Column::DefinitionId)
        .column_as(derived_versions::Column::VersionNo.max(), "version_no")
        .group_by(derived_versions::Column::DefinitionId)
        .into_tuple::<(Uuid, Option<i32>)>()
        .all(db)
        .await?
    {
        if let Some(version_no) = version_no {
            active_version.insert(definition_id, version_no);
        }
    }
    let mut by_parameter: HashMap<Uuid, CalculationInfo> = HashMap::new();
    for (id, code, name, output_parameter_id) in definitions {
        let row = DefinitionRow {
            active_version_no: active_version.get(&id).copied(),
            id,
            code,
            name,
            output_parameter_id,
        };
        let Some(output) = row.output_parameter_id else {
            continue;
        };
        by_parameter.insert(
            output,
            CalculationInfo {
                definition_id: row.id,
                code: row.code,
                name: row.name,
                version_id: None,
                version_no: None,
                formula: None,
                content_hash: None,
                active_version_no: row.active_version_no,
            },
        );
    }

    let version_ids: Vec<Uuid> = rows
        .iter()
        .filter_map(|r| r.derived_version_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    if version_ids.is_empty() {
        return Ok((by_parameter, HashMap::new()));
    }
    let found: Vec<(Uuid, i32, String, String)> = derived_versions::Entity::find()
        .filter(derived_versions::Column::Id.is_in(version_ids))
        .select_only()
        .column(derived_versions::Column::Id)
        .column(derived_versions::Column::VersionNo)
        .column(derived_versions::Column::Formula)
        .column(derived_versions::Column::ContentHash)
        .into_tuple()
        .all(db)
        .await?;
    let mut versions = HashMap::new();
    for (id, version_no, formula, content_hash) in found {
        versions.insert(id, (version_no, formula, content_hash));
    }
    Ok((by_parameter, versions))
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
    pub(super) mean: Option<f64>,
    pub(super) scalar: Option<f64>,
}

/// Served values keyed by `(site_id, parameter_id)`, plus the site rows' numeric columns.
#[derive(Debug, Default)]
pub(super) struct ServedValues {
    pub(super) slots: HashMap<(Uuid, Uuid), ServedSlot>,
    pub(super) site_properties: HashMap<Uuid, HashMap<String, f64>>,
}

impl ServedValues {
    pub(super) fn scalar(&self, site_id: Uuid, parameter_id: Uuid) -> (Option<f64>, &'static str) {
        match self.slots.get(&(site_id, parameter_id)) {
            Some(slot) if slot.mean.is_some() => (slot.mean, "mean"),
            Some(slot) if slot.scalar.is_some() => (slot.scalar, "reading"),
            _ => (None, "missing"),
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
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT d.id, d.code, d.name, d.per_replicate, d.output_parameter_id, \
                    p.code AS output_parameter_code, s.name AS calculation, \
                    COALESCE(s.enabled, true) AS enabled \
               FROM calculation_formulas d \
               LEFT JOIN tool_scripts s ON s.id = d.tool_script_id \
               LEFT JOIN parameters p ON p.id = d.output_parameter_id \
              WHERE d.output_parameter_id = ANY($1) \
                 OR d.id IN (SELECT derived_definition_id FROM derived_parameter_sources \
                              WHERE parameter_id = ANY($1)) \
              ORDER BY d.ordinal, d.code",
            [parameter_ids.into()],
        ))
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
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT ds.derived_definition_id, ds.variable_name, ds.parameter_id, \
                    ds.site_property, p.code AS parameter_code \
               FROM derived_parameter_sources ds \
               LEFT JOIN parameters p ON p.id = ds.parameter_id \
              WHERE ds.derived_definition_id = ANY($1) \
              ORDER BY ds.variable_name",
            [definition_ids.into()],
        ))
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
        .column((sample.clone(), samples::Column::Mean))
        .expr_as(
            Expr::cust("r.is_flagged IS NOT TRUE AND r.withdrawn_at IS NULL"),
            Alias::new("live"),
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
        slot.mean = slot.mean.or(row.mean);
        if row.live && slot.scalar.is_none() {
            slot.scalar = Some(row.value);
        }
    }

    let columns = links.site_properties_to_serve();
    if columns.is_empty() {
        return Ok(served);
    }
    let sites = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id, to_jsonb(sites) AS row FROM sites WHERE id = ANY($1)",
            [site_ids.into()],
        ))
        .await?;
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
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT p.code, p.name, COALESCE(sp.display_units, p.default_units) AS units, \
                    sp.decimal_places \
             FROM parameters p \
             LEFT JOIN site_parameters sp ON sp.parameter_id = p.id AND sp.site_id = $2 \
             WHERE p.id = $1 LIMIT 1",
            [parameter_id.into(), site_id.into()],
        ))
        .await?;
    let Some(row) = row else { return Ok(None) };
    let row = SlotRow::from_query_result(&row, "")?;
    Ok(Some((row.code, row.name, row.units, row.decimal_places)))
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
            Ok(()) => {}
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
        flows::enqueue_for(sink.db, &written.touched_events, actor, axes.writer).await?;
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
                crate::routes::private::reprocessing_jobs::worker::enqueue(
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
        crate::routes::private::reprocessing_jobs::worker::enqueue(
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
    mod tests {
        use super::*;

        #[test]
        fn declared_missing_markers_carry_no_value_and_no_error() {
            for cell in ["", "   ", "NaN", "nan", "NA", "na"] {
                assert_eq!(classify_cell(cell), Cell::Missing, "cell {cell:?}");
            }
        }

        #[test]
        fn every_spelling_of_the_sentinel_is_the_same_marker() {
            for cell in ["-9999", "-9999.0", "-9999.00", " -9999.000 ", "-9.999e3"] {
                assert_eq!(classify_cell(cell), Cell::Missing, "cell {cell:?}");
            }
            assert!(is_missing_sentinel(-9999.0));
            assert!(!is_missing_sentinel(-9998.9));
            assert!(!is_missing_sentinel(9999.0));
        }

        #[test]
        fn a_value_next_to_the_sentinel_is_a_measurement() {
            assert_eq!(classify_cell("-9999.5"), Cell::Value(-9999.5));
            assert_eq!(classify_cell("-999.9"), Cell::Value(-999.9));
        }

        #[test]
        fn non_finite_cells_are_errors_rather_than_missing_values() {
            for cell in ["Inf", "inf", "-inf", "Infinity", "-Infinity"] {
                assert!(
                    matches!(classify_cell(cell), Cell::Invalid(_)),
                    "cell {cell:?} must be a row error"
                );
            }
        }

        #[test]
        fn unparseable_cells_are_errors() {
            assert!(matches!(classify_cell("n/a"), Cell::Invalid(_)));
            assert!(matches!(classify_cell("12,5"), Cell::Invalid(_)));
        }

        #[test]
        fn ordinary_cells_parse() {
            assert_eq!(classify_cell(" 12.5 "), Cell::Value(12.5));
            assert_eq!(classify_cell("0"), Cell::Value(0.0));
            assert_eq!(classify_cell("1e3"), Cell::Value(1000.0));
        }

        #[test]
        fn non_finite_values_are_refused_on_every_path() {
            assert!(admit_value(f64::NAN).is_err());
            assert!(admit_value(f64::INFINITY).is_err());
            assert!(admit_value(f64::NEG_INFINITY).is_err());
            assert!(admit_value(0.0).is_ok());
            assert!(admit_value(MISSING_SENTINEL).is_ok());
        }

        #[test]
        fn the_timestamp_window_holds_at_its_edges_and_refuses_beyond_them() {
            let now = Utc::now();
            let (min_time, max_time) = window(now);
            assert!(time_rejection_at(now, now).is_none());
            assert!(time_rejection_at(now, min_time + Duration::minutes(1)).is_none());
            assert!(time_rejection_at(now, max_time - Duration::minutes(1)).is_none());
            assert!(time_rejection_at(now, min_time - Duration::days(1)).is_some());
            assert!(time_rejection_at(now, max_time + Duration::days(1)).is_some());
        }

        /// Expected behaviour: the lead bound moves with the clock, so a file's rows are judged
        /// against one reading of it. Judged a millisecond later, this timestamp changes answer.
        #[test]
        fn a_timestamp_just_past_the_lead_bound_turns_on_which_clock_read_judges_it() {
            let now = Utc::now();
            let just_past = now + Duration::days(MAX_LEAD_DAYS) + Duration::microseconds(500);
            assert!(time_rejection_at(now, just_past).is_some());
            assert!(time_rejection_at(now + Duration::milliseconds(1), just_past).is_none());
        }

        /// Expected behaviour: the floor is a fixed date, so a decade-old archive series stays
        /// ingestible indefinitely. A relative floor would make the same reading admissible today
        /// and refused later, which is what stalls a portal backfill.
        #[test]
        fn the_backward_bound_does_not_move_with_the_clock() {
            let (early, _) = window("2020-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap());
            let (late, _) = window("2099-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap());
            assert_eq!(early, late);

            let archive = "2016-08-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
            assert!(time_rejection_at(Utc::now(), archive).is_none());
        }

        /// Expected behaviour: `rejection` and `admit` are one implementation, so the reason a
        /// reading is skipped on `/ingest` is the reason it would be refused elsewhere.
        #[test]
        fn rejection_reports_what_admit_raises() {
            let now = Utc::now();
            let (_, max_time) = window(now);

            assert_eq!(rejection(now, 1.0, None), None);
            assert_eq!(rejection(now, MISSING_SENTINEL, Some("spot")), None);

            for (time, value, declared) in [
                (max_time + Duration::days(2), 1.0, None),
                (now, f64::NAN, None),
                (now, 1.0, Some("grab")),
            ] {
                let reason = rejection(time, value, declared);
                assert!(reason.is_some(), "expected a reason for {declared:?}");
                assert!(admit(time, value, declared).is_err());
            }
        }

        #[test]
        fn the_classification_vocabulary_is_closed() {
            let now = Utc::now();
            for declared in [None, Some("continuous"), Some("spot"), Some("derived")] {
                assert!(admit(now, 1.0, declared).is_ok(), "declared {declared:?}");
            }
            assert!(admit(now, 1.0, Some("grab")).is_err());
        }
    }
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
/// beside it, so raising this hold supersedes a live `replicate_stats` hold there. That precedence
/// used to fall out of the two sharing one upsert key; the key now carries `kind`, so it is stated
/// here instead of happening by accident.
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
    conn.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "UPDATE replicate_audit_holds SET status = '{superseded}'
             WHERE stream_id = $1 AND group_time = $2 AND kind = '{kind}'
               AND status IN {open}",
            superseded = HoldStatus::Superseded.as_str(),
            open = *crate::routes::private::sync::service::OPEN,
            kind = HoldKind::ReplicateStats.as_str()
        ),
        [
            stream_id.into(),
            sea_orm::prelude::DateTimeWithTimeZone::from(group_time).into(),
        ],
    ))
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
        held: 0,
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
pub(super) async fn resolve_stream_slot(
    db: &sea_orm::DatabaseConnection,
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
    estimator: Option<&str>,
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
        // Compared under the divisor the slot publishes. An undeclared slot compares as sample,
        // which is what makes its population-shaped groups disagree and surface for a decision
        // rather than being quietly reconciled under a convention nobody chose.
        let estimator = estimator.unwrap_or("sample");
        let stats = audit::group_stats(&numbers).under(estimator);
        let agree = audit::agrees(a, &stats);
        let mismatch = audit::GroupMismatch {
            time: a.time,
            expected_mean: a.expected_mean,
            expected_sd: a.expected_sd,
            expected_n: a.expected_n,
            computed_mean: stats.mean,
            computed_sd: stats.sd,
            n: stats.n,
            sd_estimator: estimator.to_string(),
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
    estimator: Resolved,
) -> Result<(Uuid, bool), AppError> {
    let candidate = samples::ActiveModel {
        id: Set(Uuid::new_v4()),
        site_id: Set(site_id),
        parameter_id: Set(parameter_id),
        collected_at: Set(time),
        created_at: Set(Some(chrono::Utc::now())),
        mean: Set(None),
        stdev: sea_orm::ActiveValue::NotSet,
        stdev_sample: Set(None),
        stdev_population: Set(None),
        median: Set(None),
        n: Set(0),
        min_value: Set(None),
        max_value: Set(None),
        updated_at: Set(None),
        sd_estimator: Set(estimator.estimator.to_string()),
        sd_estimator_source: Set(estimator.source.as_str().to_string()),
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
    requested_estimator: Option<&'static str>,
    fixed_estimators: &HashMap<Uuid, &'static str>,
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
        // Each group resolves its own estimator: one request can span several parameters, and the
        // declaration is per slot. A manifest that fixes one for this output outranks the
        // request's, which is the operator's choice for a `selectable` output; both are stamped
        // `tool`, and neither is invented here.
        let explicit = fixed_estimators
            .get(parameter_id)
            .copied()
            .or(requested_estimator);
        let estimator = resolve(
            txn,
            site_id,
            *parameter_id,
            explicit.map(|e| (e, Source::Tool)),
            None,
        )
        .await?;
        // Re-posting the same grab must reuse its sample, not accumulate empty duplicates.
        let (sample_id, is_new) =
            find_or_create_sample(txn, site_id, *parameter_id, *time, estimator).await?;
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
/// refused (ADR 0003: a stored curve id means raw in, curve out — stamping one here would apply
/// the correction twice).
/// The estimator each output of a run's tool fixes, keyed by the parameter its readings land on.
///
/// A manifest that names `sample` or `population` is stating what that output means, so the server
/// applies it rather than trusting a client to repeat it. `selectable` is the operator's choice and
/// arrives on the request instead; an output that declares neither takes the slot's declaration.
///
/// The manifest read is the run's own pinned version, not the tool's active one: a save records
/// what the run that produced it meant.
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

pub(super) async fn tool_run_fixed_estimators(
    db: &DatabaseConnection,
    tool_run_id: Option<Uuid>,
    readings: &[GrabSampleReading],
) -> Result<HashMap<Uuid, &'static str>, AppError> {
    let Some(run_id) = tool_run_id else {
        return Ok(HashMap::new());
    };
    let Some(manifest) = run_pinned_manifest(db, run_id).await? else {
        return Ok(HashMap::new());
    };
    let by_key: HashMap<&str, &'static str> = manifest
        .outputs
        .iter()
        .filter_map(|o| Some((o.key.as_str(), o.fixed_sd_estimator()?)))
        .map(|(k, e)| {
            (
                k,
                if e == "population" {
                    POPULATION
                } else {
                    SAMPLE
                },
            )
        })
        .collect();
    if by_key.is_empty() {
        return Ok(HashMap::new());
    }
    let mut by_parameter: HashMap<Uuid, &'static str> = HashMap::new();
    for r in readings {
        if let Some(output) = r.output.as_deref()
            && let Some(estimator) = by_key.get(output)
        {
            by_parameter.insert(r.parameter_id, estimator);
        }
    }
    Ok(by_parameter)
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
    let aggregate_outputs: std::collections::HashSet<String> = run_pinned_manifest(db, run_id)
        .await?
        .map(|m| {
            m.outputs
                .iter()
                .filter(|o| o.aggregate_of.is_some())
                .map(|o| o.key.clone())
                .collect()
        })
        .unwrap_or_default();

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
            let Some(index) = r.replicate_index else {
                return Err(AppError::BadRequest(format!(
                    "Reading for input '{input}' needs the replicate_index it was entered at"
                )));
            };
            let Some(values) = inputs.get(input).and_then(serde_json::Value::as_array) else {
                return Err(AppError::BadRequest(format!(
                    "'{input}' is not a replicates input of this {tool_name} run"
                )));
            };
            let recorded = usize::try_from(index)
                .ok()
                .and_then(|i| values.get(i))
                .and_then(serde_json::Value::as_f64);
            if recorded != Some(r.value) {
                return Err(AppError::BadRequest(format!(
                    "Value {} is not what this {tool_name} run consumed at replicate {index} of \
                     '{input}'",
                    r.value
                )));
            }
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
        let binds = [
            site_id.into(),
            (*parameter_id).into(),
            sea_orm::prelude::DateTimeWithTimeZone::from(*at).into(),
            serde_json::json!({ "state": "unverified", "entered_by": actor }).into(),
        ];
        let updated = conn
            .execute_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    "UPDATE replicate_audit_holds SET computed = $4, created_at = NOW() \
                     WHERE site_id = $1 AND parameter_id = $2 AND group_time = $3 \
                       AND kind = '{kind}' AND status IN {open}",
                    kind = HoldKind::UnverifiedEntry.as_str(),
                    open = *crate::routes::private::sync::service::OPEN
                ),
                binds,
            ))
            .await?
            .rows_affected();
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
pub(super) async fn stage_import_rows(
    db: &sea_orm::DatabaseConnection,
    import_token: Uuid,
    rows: &[StagedRow],
) -> AppResult<()> {
    let mut seq: i64 = 0;
    for chunk in rows.chunks(BATCH_SIZE) {
        let mut sql = String::from(
            "INSERT INTO csv_import_staging \
             (import_token, stream_id, site_id, parameter_id, time, raw_value, \
              sensor_id, calibration_id, deployment_id, seq) VALUES ",
        );
        let mut values: Vec<sea_orm::Value> = Vec::with_capacity(chunk.len() * 10);
        for (i, r) in chunk.iter().enumerate() {
            let base = i * 10;
            if i > 0 {
                sql.push(',');
            }
            sql.push_str(&format!(
                "(${},${},${},${},${},${},${},${},${},${})",
                base + 1,
                base + 2,
                base + 3,
                base + 4,
                base + 5,
                base + 6,
                base + 7,
                base + 8,
                base + 9,
                base + 10,
            ));
            values.push(import_token.into());
            values.push(r.stream_id.into());
            values.push(r.site_id.into());
            values.push(r.parameter_id.into());
            values.push(sea_orm::prelude::DateTimeWithTimeZone::from(r.time).into());
            values.push(r.raw_value.into());
            values.push(r.sensor_id.into());
            values.push(r.calibration_id.into());
            values.push(r.deployment_id.into());
            values.push(seq.into());
            seq += 1;
        }
        db.execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?;
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
    use crate::routes::private::tools::flows::execute_and_store_run;
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
    let datetime_idx = headers
        .iter()
        .position(|h| h.eq_ignore_ascii_case("datetime") || h.eq_ignore_ascii_case("time"))
        .unwrap_or(0);

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

    // The cells the file stores as readings are the replicate inputs; outputs exist only once
    // the tool has run, so they are screened by the grab save's own gate, not here.
    let mut cells: Vec<(usize, Uuid, chrono::DateTime<chrono::Utc>, f64)> = Vec::new();
    for row in &rows {
        for (name, parameter_id) in &saved_inputs {
            let values: Vec<f64> = match row.body.get(name) {
                Some(serde_json::Value::Array(list)) => {
                    list.iter().filter_map(serde_json::Value::as_f64).collect()
                }
                Some(v) => v.as_f64().into_iter().collect(),
                None => Vec::new(),
            };
            cells.extend(
                values
                    .into_iter()
                    .map(|v| (row.line, *parameter_id, row.time, v)),
            );
        }
    }
    let check = Some(screen_import(state, auth, req, site.id, &cells).await?);

    let row_count = rows.len();
    let mut inserted_total = 0usize;
    let mut tool_runs_created = 0usize;
    if !req.dry_run {
        let actor = crate::common::actor::label(auth);
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
            let body = serde_json::Value::Object(body);
            let body_bytes =
                serde_json::to_vec(&body).map_err(|e| AppError::Internal(e.to_string()))?;
            let result = match execute_and_store_run(
                state,
                &tool,
                &body_bytes,
                &actor,
                "csv_import",
            )
            .await
            {
                Ok(result) => result,
                Err(e) => {
                    record_error(line, e.to_string(), &mut errors, &mut error_count);
                    continue;
                }
            };
            tool_runs_created += 1;

            let mut readings: Vec<GrabSampleReading> = saved_outputs
                .iter()
                .filter_map(|(key, parameter_id)| {
                    result
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
                    let (Some(value), Ok(replicate_index)) =
                        (cell.as_f64(), i16::try_from(position))
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
            let request = GrabSampleRequest {
                pending_inputs: false,
                site_id: site.id,
                created_by: Some(actor.clone()),
                label: None,
                notes: None,
                mode: (req.conflict == ConflictMode::Overwrite).then_some(GrabWriteMode::Replace),
                dry_run: false,
                tool_run_id: Some(result.run_id),
                check_id: None,
                // The tool's manifest is read by the save path itself; nothing here overrides
                // the slot's declaration.
                sd_estimator: None,
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
        let release = conn
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    "SELECT id FROM replicate_audit_holds
                     WHERE stream_id = $1 AND kind = '{kind}' AND status = '{acknowledged}'
                     ORDER BY created_at DESC LIMIT 1",
                    acknowledged = HoldStatus::Acknowledged.as_str(),
                    kind = HoldKind::BrakeFired.as_str()
                ),
                [stream_id.into()],
            ))
            .await?;
        match release {
            Some(row) => {
                let hold_id: Uuid = row.try_get("", "id")?;
                conn.execute_raw(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    format!(
                        "UPDATE replicate_audit_holds SET status = '{remediated}'
                         WHERE id = $1 AND status = '{acknowledged}'",
                        remediated = HoldStatus::Remediated.as_str(),
                        acknowledged = HoldStatus::Acknowledged.as_str()
                    ),
                    [hold_id.into()],
                ))
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
        outcome.withdrawn_keys = to_withdraw.iter().copied().collect();
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
        outcome.reinstated_keys = reinstate.iter().copied().collect();
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
#[path = "tests/sd_estimator.rs"]
mod sd_estimator;

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
#[path = "tests/batch.rs"]
mod batch;

#[cfg(test)]
#[path = "tests/grab_samples.rs"]
mod grab_samples;

#[cfg(test)]
#[path = "tests/import.rs"]
mod import;

#[cfg(test)]
#[path = "tests/reconcile.rs"]
mod reconcile;
