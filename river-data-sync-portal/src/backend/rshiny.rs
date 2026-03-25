use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use sqlx::mysql::MySqlRow;
use sqlx::{MySqlPool, Row};

use super::{PortalBackend, ReadingValue, StreamDescriptor, StreamFetchRequest, StreamReadings};
use crate::config::PortalType;
use crate::error::SyncError;

/// A discovered parameter column from grab_params_plotting / sensor_params_plotting.
#[derive(Debug, Clone)]
struct ParamColumn {
    /// The column name in the `data` table (e.g., "WTW_DO_mgL_1")
    column_name: String,
    /// Human-readable name (e.g., "Dissolved Oxygen - Field [mg/L]")
    display_name: String,
    /// Units (e.g., "mg/L")
    units: String,
    /// Section/category (e.g., "Water physicochemistry")
    section: String,
}

/// Shared backend for CNET and METALP portals (identical schema).
pub struct RshinyBackend {
    portal_type: PortalType,
}

impl RshinyBackend {
    pub fn new(portal_type: PortalType) -> Self {
        Self { portal_type }
    }

    /// Query the portal's plotting metadata tables to discover parameter columns.
    async fn discover_param_columns(&self, pool: &MySqlPool) -> Result<Vec<ParamColumn>, SyncError> {
        let mut columns = Vec::new();

        // Grab sample parameters
        let grab_rows = sqlx::query(
            "SELECT option_name, data, units, section_name FROM grab_params_plotting WHERE active = 1"
        )
        .fetch_all(pool)
        .await?;

        for row in &grab_rows {
            let data_col: String = row.get("data");
            let display_name: String = row.get("option_name");
            let units: String = row.get("units");
            let section: String = row.get("section_name");

            // The `data` field can be comma-separated for multi-plot params (e.g., "Reach_depth_avg_cm,Gage_obl_cm,Gage_vert_cm").
            // Each component is a separate column to sync.
            for col in data_col.split(',') {
                let col = col.trim();
                if col.is_empty() {
                    continue;
                }
                columns.push(ParamColumn {
                    column_name: col.to_string(),
                    display_name: if data_col.contains(',') {
                        format!("{display_name} [{col}]")
                    } else {
                        display_name.clone()
                    },
                    units: units.clone(),
                    section: section.clone(),
                });
            }
        }

        // Sensor parameters (continuous logger data, if present)
        let sensor_rows = sqlx::query(
            "SELECT option_name, data, units, section_name FROM sensor_params_plotting WHERE active = 1"
        )
        .fetch_all(pool)
        .await;

        if let Ok(rows) = sensor_rows {
            for row in &rows {
                let data_col: String = row.get("data");
                let display_name: String = row.get("option_name");
                let units: String = row.get("units");
                let section: String = row.get("section_name");

                for col in data_col.split(',') {
                    let col = col.trim();
                    if col.is_empty() {
                        continue;
                    }
                    // Avoid duplicates (same column in both grab and sensor plotting)
                    if columns.iter().any(|c| c.column_name == col) {
                        continue;
                    }
                    columns.push(ParamColumn {
                        column_name: col.to_string(),
                        display_name: if data_col.contains(',') {
                            format!("{display_name} [{col}]")
                        } else {
                            display_name.clone()
                        },
                        units: units.clone(),
                        section: section.clone(),
                    });
                }
            }
        }

        tracing::info!(count = columns.len(), "Discovered parameter columns from plotting tables");
        Ok(columns)
    }

    /// Query all station names.
    async fn discover_stations(&self, pool: &MySqlPool) -> Result<Vec<StationInfo>, SyncError> {
        let rows = sqlx::query("SELECT name, full_name, catchment, elevation FROM stations")
            .fetch_all(pool)
            .await?;

        let stations: Vec<StationInfo> = rows
            .iter()
            .map(|row| StationInfo {
                name: row.get("name"),
                full_name: row.get("full_name"),
                catchment: row.get("catchment"),
                elevation: row.get::<Option<i32>, _>("elevation").map(|v| v as f64),
            })
            .collect();

        tracing::info!(count = stations.len(), "Discovered stations");
        Ok(stations)
    }
}

#[derive(Debug, Clone)]
struct StationInfo {
    name: String,
    full_name: Option<String>,
    catchment: Option<String>,
    elevation: Option<f64>,
}

#[async_trait::async_trait]
impl PortalBackend for RshinyBackend {
    fn source_system(&self) -> &str {
        self.portal_type.source_system()
    }

    async fn discover_stream_descriptors(
        &self,
        pool: &MySqlPool,
    ) -> Result<Vec<StreamDescriptor>, SyncError> {
        let stations = self.discover_stations(pool).await?;
        let columns = self.discover_param_columns(pool).await?;

        let source_system = self.source_system();
        let mut descriptors = Vec::with_capacity(stations.len() * columns.len());

        for station in &stations {
            for col in &columns {
                descriptors.push(StreamDescriptor {
                    source_key: format!("{}:{}", station.name, col.column_name),
                    source_name: format!("{} - {}", station.name, col.display_name),
                    source_path: format!("{}/{}/{}", source_system, station.name, col.column_name),
                    metadata: serde_json::json!({
                        "station": {
                            "name": station.name,
                            "full_name": station.full_name,
                            "catchment": station.catchment,
                            "elevation": station.elevation,
                        },
                        "parameter": {
                            "column_name": col.column_name,
                            "display_name": col.display_name,
                            "section": col.section,
                        },
                        "units": col.units,
                    }),
                });
            }
        }

        tracing::info!(
            count = descriptors.len(),
            stations = stations.len(),
            columns = columns.len(),
            "Built stream descriptors"
        );
        Ok(descriptors)
    }

    async fn fetch_readings(
        &self,
        pool: &MySqlPool,
        streams: &[StreamFetchRequest],
    ) -> Result<Vec<StreamReadings>, SyncError> {
        if streams.is_empty() {
            return Ok(Vec::new());
        }

        // Group streams by station
        let mut station_streams: std::collections::HashMap<String, Vec<&StreamFetchRequest>> =
            std::collections::HashMap::new();
        let mut column_set: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        for req in streams {
            if let Some((station, col)) = req.source_key.split_once(':') {
                station_streams
                    .entry(station.to_string())
                    .or_default()
                    .push(req);
                column_set.insert(col.to_string());
            }
        }

        // Build the list of columns to fetch (backtick-quoted for safety)
        let columns: Vec<String> = column_set.iter().cloned().collect();
        let columns_sql = columns
            .iter()
            .map(|c| format!("`{c}`"))
            .collect::<Vec<_>>()
            .join(", ");

        let mut all_results: Vec<StreamReadings> = Vec::new();

        for (station, station_reqs) in &station_streams {
            // Find the earliest `since` for this station
            let earliest_since = station_reqs
                .iter()
                .filter_map(|r| r.since)
                .min();

            let query = if earliest_since.is_some() {
                format!(
                    "SELECT station, DATE_reading, TIME_reading_GMT, {} FROM data \
                     WHERE station = ? AND TIMESTAMP(DATE_reading, TIME_reading_GMT) > ? \
                     ORDER BY DATE_reading, TIME_reading_GMT",
                    columns_sql
                )
            } else {
                format!(
                    "SELECT station, DATE_reading, TIME_reading_GMT, {} FROM data \
                     WHERE station = ? \
                     ORDER BY DATE_reading, TIME_reading_GMT",
                    columns_sql
                )
            };

            let rows: Vec<MySqlRow> = if let Some(since) = earliest_since {
                sqlx::query(&query)
                    .bind(station)
                    .bind(since.naive_utc())
                    .fetch_all(pool)
                    .await?
            } else {
                sqlx::query(&query)
                    .bind(station)
                    .fetch_all(pool)
                    .await?
            };

            // Build a lookup: source_key -> (stream_id, since)
            let stream_lookup: std::collections::HashMap<String, (uuid::Uuid, Option<DateTime<Utc>>)> =
                station_reqs
                    .iter()
                    .map(|r| (r.source_key.clone(), (r.stream_id, r.since)))
                    .collect();

            // Unpivot: for each row, iterate each parameter column and emit readings
            let mut per_stream: std::collections::HashMap<String, Vec<ReadingValue>> =
                std::collections::HashMap::new();

            for row in &rows {
                let date: NaiveDate = row.get("DATE_reading");
                let time: NaiveTime = row.get("TIME_reading_GMT");
                let timestamp = date.and_time(time).and_utc();

                for col_name in &columns {
                    let source_key = format!("{station}:{col_name}");

                    // Check if this stream was requested and if this reading is new
                    if let Some((_, since)) = stream_lookup.get(&source_key) {
                        if let Some(s) = since {
                            if timestamp <= *s {
                                continue;
                            }
                        }
                    } else {
                        continue;
                    }

                    // Try to read as f64 — columns can be float or int
                    let value: Option<f64> = row.try_get::<Option<f64>, _>(col_name.as_str())
                        .or_else(|_| {
                            row.try_get::<Option<i32>, _>(col_name.as_str())
                                .map(|v| v.map(|i| i as f64))
                        })
                        .unwrap_or(None);

                    if let Some(val) = value {
                        per_stream
                            .entry(source_key)
                            .or_default()
                            .push(ReadingValue {
                                time: timestamp,
                                value: val,
                                replicate_index: 0,
                            });
                    }
                }
            }

            // Convert to StreamReadings
            for (source_key, readings) in per_stream {
                if let Some((stream_id, _)) = stream_lookup.get(&source_key) {
                    all_results.push(StreamReadings {
                        source_key,
                        stream_id: *stream_id,
                        readings,
                    });
                }
            }

            tracing::debug!(
                station = %station,
                rows = rows.len(),
                "Fetched readings for station"
            );
        }

        Ok(all_results)
    }
}
