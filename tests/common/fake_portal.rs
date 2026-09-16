//! A portal source the real `SyncDriver` can be pointed at, for the tests that have to run the
//! sync loop rather than write its requests by hand.
//!
//! It is a hand-made resemblance of `river-data-rshiny`'s CNET backend, not that backend: it
//! emits the same descriptor shape (`{station}:{column}`, `{station}:{column}:reps`, the
//! `station` / `parameter` / `hierarchy` metadata blocks), declares `reconciled()` so the driver
//! fetches without a cursor and windows every pass, and consumes the server's pinned replicate
//! assignments the way the real one does. The MariaDB decode is what it does not exercise, and
//! that is the accepted gap: `river-data-rshiny/tests/portal_fixture.rs` covers that half against
//! the committed fixture database.
//!
//! The content is a `Mutex` a test may edit between cycles, because a source that never changes
//! proves nothing about a source that does.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use river_data_core::chrono::{DateTime, TimeZone, Utc};
use river_data_core::client::{BackendError, SourceBackend};
use river_data_core::models::{
    ColumnAssignment, IngestReading, ReplicateSpec, SourceWindow, StandardCurveUpsert,
    StreamDescriptor, StreamFetchRequest, StreamReadings,
};
use river_data_core::serde_json::json;

/// The source system these streams register under.
pub const SOURCE_SYSTEM: &str = "fakecnet";

/// The stations the fake portal carries.
pub const STATIONS: [&str; 2] = ["S01", "S02"];

/// The single-column parameter every station reports.
pub const SINGLE_COLUMN: &str = "water_temp_degC";

/// The replicate family's mean column; its members are the `_rep_*` columns below.
pub const FAMILY_MEAN_COLUMN: &str = "DOC_avg_ppb";
pub const FAMILY_MEMBERS: [&str; 3] = ["DOC_rep_A_ppb", "DOC_rep_B_ppb", "DOC_rep_C_ppb"];

/// One visit's cells at one station: the single column, and the family's three replicates. A
/// `None` is a cell the source holds empty, which a windowed pass withdraws.
#[derive(Clone, Debug)]
pub struct Visit {
    pub at: DateTime<Utc>,
    pub single: Option<f64>,
    pub replicates: [Option<f64>; 3],
}

/// The portal's content, as the tests may edit it between cycles. A clone shares that content, so
/// a story can hand one to the driver and keep the other to edit the source through.
#[derive(Clone)]
pub struct FakePortal {
    visits: Arc<Mutex<HashMap<String, Vec<Visit>>>>,
    /// The replicate index the server pinned per source column, applied on registration. Empty
    /// until the driver hands them over, exactly as the real backend starts.
    assignments: Arc<Mutex<HashMap<String, Vec<ColumnAssignment>>>>,
}

impl FakePortal {
    /// Two stations, three visits each, with a gap in the middle of the span: the second visit
    /// has no single-column value at S02, and its third replicate is empty at both.
    #[must_use]
    pub fn seeded() -> Self {
        let at = |day: u32| {
            Utc.with_ymd_and_hms(2026, 6, day, 9, 0, 0)
                .single()
                .expect("a representable instant")
        };
        let mut visits = HashMap::new();
        visits.insert(
            "S01".to_string(),
            vec![
                Visit {
                    at: at(1),
                    single: Some(7.4),
                    replicates: [Some(310.0), Some(316.0), Some(313.0)],
                },
                Visit {
                    at: at(8),
                    single: Some(8.1),
                    replicates: [Some(402.0), Some(398.0), None],
                },
                Visit {
                    at: at(15),
                    single: Some(9.6),
                    replicates: [Some(451.0), Some(449.0), Some(450.0)],
                },
            ],
        );
        visits.insert(
            "S02".to_string(),
            vec![
                Visit {
                    at: at(1),
                    single: Some(5.2),
                    replicates: [Some(120.0), Some(124.0), Some(122.0)],
                },
                Visit {
                    at: at(8),
                    single: None,
                    replicates: [Some(133.0), Some(131.0), None],
                },
                Visit {
                    at: at(15),
                    single: Some(6.9),
                    replicates: [Some(140.0), Some(142.0), Some(141.0)],
                },
            ],
        );
        Self {
            visits: Arc::new(Mutex::new(visits)),
            assignments: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// A second view of the same content, for the stories that hand one to the driver and keep
    /// one to edit the source through.
    #[must_use]
    pub fn handle(&self) -> Self {
        Self {
            visits: Arc::clone(&self.visits),
            assignments: Arc::clone(&self.assignments),
        }
    }

    /// Edit the source the way a portal user does: replace one visit's cells, in place. The next
    /// cycle re-reads the whole window, so the change travels as a correction rather than an
    /// append.
    ///
    /// # Panics
    /// If the station or the instant is not in the seeded content.
    pub fn edit(&self, station: &str, at: DateTime<Utc>, edit: impl FnOnce(&mut Visit)) {
        let mut visits = self.visits.lock().expect("visits lock");
        let rows = visits
            .get_mut(station)
            .unwrap_or_else(|| panic!("{station} is a station of this portal"));
        let row = rows
            .iter_mut()
            .find(|v| v.at == at)
            .unwrap_or_else(|| panic!("{station} has no visit at {at}"));
        edit(row);
    }

    /// A visit added at a station the store already holds, at any instant, including one inside a
    /// window already reconciled. Kept in time order, as the source reads.
    ///
    /// # Panics
    /// If the station is not in the content.
    pub fn add_visit(&self, station: &str, visit: Visit) {
        let mut visits = self.visits.lock().expect("visits lock");
        let rows = visits
            .get_mut(station)
            .unwrap_or_else(|| panic!("{station} is a station of this portal"));
        rows.push(visit);
        rows.sort_by_key(|v| v.at);
    }

    /// A station that did not exist at enrolment, with the visits it arrives carrying.
    pub fn add_station(&self, station: &str, rows: Vec<Visit>) {
        self.visits
            .lock()
            .expect("visits lock")
            .insert(station.to_string(), rows);
    }

    /// The replicate index the server pinned for one source column, or `None` before the
    /// assignments have been applied.
    #[must_use]
    pub fn pinned_index(&self, family_key: &str, column: &str) -> Option<i16> {
        let assignments = self.assignments.lock().expect("assignments lock");
        assignments
            .get(family_key)?
            .iter()
            .find(|a| a.column == column)
            .map(|a| a.index)
    }

    fn family_key(station: &str) -> String {
        format!("{station}:{FAMILY_MEAN_COLUMN}:reps")
    }

    fn metadata(station: &str, column: &str, label: &str) -> river_data_core::serde_json::Value {
        json!({
            "station": { "name": station, "full_name": format!("Fake {station}"),
                         "catchment": "Fake", "elevation": 1200 },
            "parameter": { "column_name": column, "display_name": label,
                           "short_name": label, "section": "" },
            "hierarchy": { "project": SOURCE_SYSTEM.to_uppercase(), "site": station,
                           "parameter": column, "parameter_label": label },
            "coordinates": { "latitude": 46.2, "longitude": 7.4, "altitude_m": 1200 },
        })
    }
}

#[river_data_core::async_trait]
impl SourceBackend for FakePortal {
    fn source_system(&self) -> &str {
        SOURCE_SYSTEM
    }

    /// The portal is edited in place, so every cycle reads it whole and declares the window it
    /// read.
    fn reconciled(&self) -> bool {
        true
    }

    /// New stations and columns appear at the source without operator action, as the portals'
    /// own backend declares.
    fn rediscover_every_cycle(&self) -> bool {
        true
    }

    async fn discover_streams(&self) -> Result<Vec<StreamDescriptor>, BackendError> {
        let mut stations: Vec<String> = self
            .visits
            .lock()
            .map_err(|_| "visits lock poisoned")?
            .keys()
            .cloned()
            .collect();
        stations.sort();
        let mut out = Vec::new();
        for station in &stations {
            let station = station.as_str();
            out.push(StreamDescriptor {
                source_key: format!("{station}:{SINGLE_COLUMN}"),
                source_name: format!("{station} - {SINGLE_COLUMN}"),
                source_path: format!("{SOURCE_SYSTEM}/{station}/{SINGLE_COLUMN}"),
                metadata: Self::metadata(station, SINGLE_COLUMN, "Water temperature"),
                measurement_type: Some("spot".to_string()),
                sensor_id: None,
                replicates: None,
                decimal_places: Some(2),
                instrument_granularity: None,
            });
            let mut metadata = Self::metadata(station, FAMILY_MEAN_COLUMN, "DOC");
            metadata["replicate_family"] = json!({
                "members": FAMILY_MEMBERS,
                "portal_mean_column": FAMILY_MEAN_COLUMN,
                "portal_sd_column": "DOC_sd_ppb",
                "curve_ref_column": "DOC_curve",
                "calc": "mean",
            });
            out.push(StreamDescriptor {
                source_key: Self::family_key(station),
                source_name: format!("{station} - {FAMILY_MEAN_COLUMN}"),
                source_path: format!("{SOURCE_SYSTEM}/{station}/{FAMILY_MEAN_COLUMN}"),
                metadata,
                measurement_type: Some("spot".to_string()),
                sensor_id: None,
                replicates: Some(ReplicateSpec {
                    source_columns: FAMILY_MEMBERS.iter().map(ToString::to_string).collect(),
                    portal_mean_column: Some(FAMILY_MEAN_COLUMN.to_string()),
                    portal_sd_column: Some("DOC_sd_ppb".to_string()),
                    curve_ref_column: Some("DOC_curve".to_string()),
                    calc: Some("mean".to_string()),
                    // Undeclared, as the portals leave it: the slot declaration and the audit
                    // gate own that decision.
                    sd_estimator: None,
                }),
                decimal_places: Some(2),
                instrument_granularity: None,
            });
        }
        Ok(out)
    }

    /// The one lab curve the family's values were made with.
    async fn discover_standard_curves(&self) -> Result<Vec<StandardCurveUpsert>, BackendError> {
        Ok(vec![StandardCurveUpsert {
            source_key: "DOC-2026-06".to_string(),
            instrument_label: "DOC analyser".to_string(),
            slope: 1.04,
            intercept: -2.5,
            r_squared: Some(0.999),
            name: Some("DOC June 2026".to_string()),
            fitted_on: river_data_core::chrono::NaiveDate::from_ymd_opt(2026, 5, 30),
            notes: Some("five standards".to_string()),
        }])
    }

    /// The server pins each source column's replicate index; the backend consumes that mapping
    /// rather than deriving one, so an index is never reassigned behind a stored reading.
    async fn apply_replicate_assignments(
        &self,
        source_key: &str,
        assignments: &[ColumnAssignment],
    ) -> Result<(), BackendError> {
        self.assignments
            .lock()
            .map_err(|_| "assignments lock poisoned")?
            .insert(source_key.to_string(), assignments.to_vec());
        Ok(())
    }

    async fn fetch_readings(
        &self,
        requests: &[StreamFetchRequest],
    ) -> Result<Vec<StreamReadings>, BackendError> {
        let visits = self.visits.lock().map_err(|_| "visits lock poisoned")?;
        let assignments = self
            .assignments
            .lock()
            .map_err(|_| "assignments lock poisoned")?;
        let mut out = Vec::new();
        for req in requests {
            let (station, rest) = req
                .source_key
                .split_once(':')
                .ok_or_else(|| format!("unrecognised source_key {}", req.source_key))?;
            let rows = visits
                .get(station)
                .ok_or_else(|| format!("unknown station {station}"))?;
            let mut readings = Vec::new();
            let mut cells_read = 0u64;

            if rest == SINGLE_COLUMN {
                for visit in rows {
                    cells_read += 1;
                    if let Some(value) = visit.single {
                        readings.push(IngestReading::new(visit.at, value));
                    }
                }
            } else {
                let pinned = assignments.get(&req.source_key);
                for visit in rows {
                    for (member, value) in FAMILY_MEMBERS.iter().zip(visit.replicates) {
                        cells_read += 1;
                        let Some(value) = value else { continue };
                        // Before the first apply the source has no pinned mapping, so the
                        // column's position stands in: the server pins it on registration and
                        // every later cycle uses what it pinned.
                        let index = pinned
                            .and_then(|a| a.iter().find(|c| &c.column == member))
                            .map(|c| c.index)
                            .or_else(|| {
                                i16::try_from(FAMILY_MEMBERS.iter().position(|m| m == member)?).ok()
                            })
                            .ok_or_else(|| format!("no index for {member}"))?;
                        let mut reading = IngestReading::new(visit.at, value);
                        reading.replicate_index = index;
                        readings.push(reading);
                    }
                }
            }

            let mut stream_readings =
                StreamReadings::new(req.stream_id, req.source_key.clone(), readings);
            stream_readings.window = Some(SourceWindow {
                from: Utc
                    .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
                    .single()
                    .expect("a representable instant"),
                to: Utc::now(),
                source_rows_read: cells_read,
                dropped_times: Vec::new(),
                // Stamped by the driver.
                content_digest: None,
            });
            out.push(stream_readings);
        }
        Ok(out)
    }
}

/// Mint a credential, enrol through the real control plane, and hand back a driver pointed at the
/// served router. The three steps always travel together, and a story that needs a second driver
/// (a restart, a second service) calls it again.
pub async fn enrolled_driver(
    app: axum::Router,
    state: &river_db::common::AppState,
    portal: FakePortal,
) -> river_data_core::client::SyncDriver {
    enrolled_service(app, state, portal).await.0
}

/// The same enrolment, with the control-plane client and the service id the server assigned.
///
/// A story that drives an operator command needs all three: the command is issued against the
/// service id, collected on a heartbeat through the client, and run through the driver.
pub async fn enrolled_service(
    app: axum::Router,
    state: &river_db::common::AppState,
    portal: FakePortal,
) -> (
    river_data_core::client::SyncDriver,
    river_data_core::client::ControlPlaneClient,
    uuid::Uuid,
) {
    use axum::Json;
    use axum::extract::State;
    use river_data_core::client::{ControlPlaneClient, RiverDataClient, SyncDriver};
    use river_data_core::models::RunnerConfig;
    use river_db::routes::private::sync::models::CreateCredentialRequest;
    use river_db::routes::private::sync::views::create_credential;

    let Json(minted) = create_credential(
        State(state.clone()),
        Json(CreateCredentialRequest {
            service_type: SOURCE_SYSTEM.to_string(),
            source_system: Some(SOURCE_SYSTEM.to_string()),
        }),
    )
    .await
    .expect("mint an enrolment credential");

    let base = super::serve(app).await;
    let mut control = ControlPlaneClient::new(&base).expect("build the control-plane client");
    let enrolled = control
        .enroll(&minted.client_id, &minted.client_secret, "fake-portal")
        .await
        .expect("enroll the fake portal");

    let api = RiverDataClient::new(&base, &enrolled.session_token).expect("build the API client");
    let driver = SyncDriver::new(
        Box::new(portal),
        api,
        &RunnerConfig {
            api_base_url: base,
            client_id: minted.client_id,
            client_secret: minted.client_secret,
            instance_id: "fake-portal".to_string(),
            heartbeat_interval_secs: 30,
            sync_interval_secs: 300,
            enrollment_retry_secs: 1,
            retry_max: 1,
            retry_delay_secs: 0,
        },
    );
    (driver, control, enrolled.service_id)
}
