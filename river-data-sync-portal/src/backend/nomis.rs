use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use sqlx::mysql::MySqlRow;
use sqlx::{MySqlPool, Row};

use super::{PortalBackend, ReadingValue, StreamDescriptor, StreamFetchRequest, StreamReadings};
use crate::error::SyncError;

/// NOMIS location water quality columns (from the `location` table).
const LOCATION_PARAMS: &[(&str, &str, &str)] = &[
    ("water_temp", "Water Temperature", "°C"),
    ("ph", "pH", "-"),
    ("do", "Dissolved Oxygen", "mg/L"),
    ("do_sat", "Dissolved Oxygen Saturation", "%"),
    ("w_co2", "Water CO2", "ppm"),
    ("conductivity", "Conductivity", "µS/cm"),
    ("turb", "Turbidity", "NTU"),
];

/// NOMIS biogeo_1 columns (ion chemistry + nutrients).
const BIOGEO1_PARAMS: &[(&str, &str, &str)] = &[
    ("i1_na", "Sodium", "mg/L"),
    ("i2_k", "Potassium", "mg/L"),
    ("i3_mg", "Magnesium", "mg/L"),
    ("i4_ca", "Calcium", "mg/L"),
    ("i5_cl", "Chloride", "mg/L"),
    ("i6_so4", "Sulfate", "mg/L"),
    ("n1_tn", "Total Nitrogen", "mg/L"),
    ("n2_tp", "Total Phosphorus", "mg/L"),
    ("n3_srp", "Soluble Reactive Phosphorus", "µg/L"),
    ("n4_nh4", "Ammonium", "µg/L"),
    ("n5_no3", "Nitrate", "mg/L"),
    ("n6_no2", "Nitrite", "mg/L"),
    ("hydro_isotope", "Hydrogen Isotope (δD)", "‰"),
    ("oxy_isotope", "Oxygen Isotope (δ18O)", "‰"),
];

/// NOMIS biogeo_3u columns (DOM/DOC optical properties).
const BIOGEO3U_PARAMS: &[(&str, &str, &str)] = &[
    ("doc", "Dissolved Organic Carbon", "ppb"),
    ("abs254", "Absorption at 254nm", "-"),
    ("abs300", "Absorption at 300nm", "-"),
    ("suva", "SUVA", "L/mg-m"),
    ("e2e3", "E2/E3 Ratio", "-"),
    ("e4e6", "E4/E6 Ratio", "-"),
    ("s275295", "Spectral Slope S275-295", "-"),
    ("s350400", "Spectral Slope S350-400", "-"),
    ("s300700", "Spectral Slope S300-700", "-"),
    ("sr", "Slope Ratio SR", "-"),
    ("bix", "Biological Index BIX", "-"),
    ("fi", "Fluorescence Index FI", "-"),
    ("hix", "Humification Index HIX", "-"),
    ("coble_b", "Peak B (Protein)", "-"),
    ("coble_t", "Peak T (Protein)", "-"),
    ("coble_a", "Peak A (Humic)", "-"),
    ("coble_m", "Peak M (Marine Humic)", "-"),
    ("coble_c", "Peak C (Humic)", "-"),
    ("coble_r", "Peak R", "-"),
];

/// NOMIS biogeo_1u columns (total suspended solids).
const BIOGEO1U_PARAMS: &[(&str, &str, &str)] = &[
    ("tss_1", "TSS Filter 1", "mg/L"),
    ("tss_2", "TSS Filter 2", "mg/L"),
    ("tss_3", "TSS Filter 3", "mg/L"),
    ("tss_4", "TSS Filter 4", "mg/L"),
    ("tss_5", "TSS Filter 5", "mg/L"),
    ("tss_5a", "TSS Filter 5a", "mg/L"),
    ("tss_5b", "TSS Filter 5b", "mg/L"),
    ("tss_5c", "TSS Filter 5c", "mg/L"),
    ("tss_5d", "TSS Filter 5d", "mg/L"),
];

pub struct NomisBackend;

impl NomisBackend {
    pub fn new() -> Self {
        Self
    }
}

/// Parse NOMIS date strings which can be "dd.mm.yyyy" or "dd.mm.yy".
fn parse_nomis_date(s: &str) -> Option<NaiveDate> {
    let trimmed = s.trim();
    // Try 4-digit year first, but only if the year part actually has 4 digits
    let parts: Vec<&str> = trimmed.split('.').collect();
    if parts.len() == 3 {
        let year_part = parts[2];
        if year_part.len() == 4 {
            if let Ok(d) = NaiveDate::parse_from_str(trimmed, "%d.%m.%Y") {
                return Some(d);
            }
        }
        // 2-digit year: map to 2000+
        if year_part.len() <= 2 {
            if let Ok(year) = year_part.parse::<i32>() {
                let full_year = 2000 + year;
                if let (Ok(day), Ok(month)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                    return NaiveDate::from_ymd_opt(full_year, month, day);
                }
            }
        }
    }
    // Fallback
    NaiveDate::parse_from_str(trimmed, "%d.%m.%Y")
        .or_else(|_| NaiveDate::parse_from_str(trimmed, "%d.%m.%y"))
        .ok()
}

/// Parse NOMIS time strings like "15:00" or "12:17".
fn parse_nomis_time(s: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(s.trim(), "%H:%M")
        .or_else(|_| NaiveTime::parse_from_str(s.trim(), "%H:%M:%S"))
        .ok()
}

fn parse_nomis_datetime(date_str: &str, time_str: Option<&str>) -> Option<DateTime<Utc>> {
    let date = parse_nomis_date(date_str)?;
    let time = time_str
        .and_then(|t| parse_nomis_time(t))
        .unwrap_or(NaiveTime::from_hms_opt(12, 0, 0).unwrap());
    Some(NaiveDateTime::new(date, time).and_utc())
}

/// Map replicate letter (A, B, C, ...) to replicate_index (0, 1, 2, ...).
fn replicate_to_index(rep: &str) -> i32 {
    match rep.trim().to_uppercase().as_str() {
        "A" => 0,
        "B" => 1,
        "C" => 2,
        "D" => 3,
        _ => 0,
    }
}

#[async_trait::async_trait]
impl PortalBackend for NomisBackend {
    fn source_system(&self) -> &str {
        "nomis"
    }

    async fn discover_stream_descriptors(
        &self,
        pool: &MySqlPool,
    ) -> Result<Vec<StreamDescriptor>, SyncError> {
        // Fetch locations with glacier info and coordinates
        let locations = sqlx::query(
            "SELECT l.id_location, l.id_glacier, l.type, g.gl_name, g.rgi_v6, \
                    gu.lat_sp, gu.lon_sp, gu.ele_sp \
             FROM location l \
             JOIN glacier g ON l.id_glacier = g.id_glacier \
             LEFT JOIN glacier_ud gu ON l.id_glacier = gu.id_glacier AND l.type = gu.site"
        )
        .fetch_all(pool)
        .await?;

        let mut descriptors = Vec::new();

        for loc in &locations {
            let id_location: String = loc.get("id_location");
            let id_glacier: String = loc.get("id_glacier");
            let loc_type: Option<String> = loc.get("type");
            let gl_name: Option<String> = loc.get("gl_name");
            let rgi: Option<String> = loc.get("rgi_v6");
            let lat: Option<f64> = loc.try_get("lat_sp").ok();
            let lon: Option<f64> = loc.try_get("lon_sp").ok();
            let ele: Option<i32> = loc.try_get("ele_sp").ok();

            let base_metadata = serde_json::json!({
                "glacier": {
                    "id": id_glacier,
                    "name": gl_name,
                    "rgi_v6": rgi,
                },
                "location": {
                    "id": id_location,
                    "type": loc_type,
                },
                "coordinates": {
                    "latitude": lat,
                    "longitude": lon,
                    "altitude_m": ele,
                },
            });

            // Helper to build descriptors with hierarchy metadata
            let mut add_params = |params: &[(&str, &str, &str)], table: &str, key_prefix: &str, path_prefix: &str| {
                for (col, name, units) in params {
                    let source_key = if key_prefix.is_empty() {
                        format!("{id_location}:{col}")
                    } else {
                        format!("{id_location}:{key_prefix}:{col}")
                    };
                    let mut meta = base_metadata.clone();
                    meta["units"] = serde_json::json!(units);
                    meta["table"] = serde_json::json!(table);
                    meta["hierarchy"] = serde_json::json!({
                        "project": "NOMIS",
                        "site": id_location,
                        "parameter": name,
                    });

                    let source_path = if path_prefix.is_empty() {
                        format!("nomis/{id_glacier}/{id_location}/{col}")
                    } else {
                        format!("nomis/{id_glacier}/{id_location}/{path_prefix}/{col}")
                    };

                    descriptors.push(StreamDescriptor {
                        source_key,
                        source_name: format!("{id_location} - {name}"),
                        source_path,
                        metadata: meta,
                    });
                }
            };

            add_params(LOCATION_PARAMS, "location", "", "");
            add_params(BIOGEO1_PARAMS, "biogeo_1", "biogeo1", "biogeo1");
            add_params(BIOGEO1U_PARAMS, "biogeo_1u", "biogeo1u", "biogeo1u");
            add_params(BIOGEO3U_PARAMS, "biogeo_3u", "biogeo3u", "biogeo3u");
        }

        tracing::info!(
            count = descriptors.len(),
            locations = locations.len(),
            "Built NOMIS stream descriptors"
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

        // Build lookup: source_key -> (stream_id, since)
        let stream_lookup: std::collections::HashMap<String, (uuid::Uuid, Option<DateTime<Utc>>)> =
            streams
                .iter()
                .map(|r| (r.source_key.clone(), (r.stream_id, r.since)))
                .collect();

        let mut all_results: Vec<StreamReadings> = Vec::new();
        let mut per_stream: std::collections::HashMap<String, Vec<ReadingValue>> =
            std::collections::HashMap::new();

        // ── Location-level readings ──
        let location_rows: Vec<MySqlRow> = sqlx::query(
            "SELECT id_location, date, time, water_temp, ph, `do`, do_sat, w_co2, conductivity, turb \
             FROM location"
        )
        .fetch_all(pool)
        .await?;

        for row in &location_rows {
            let id_location: String = row.get("id_location");
            let date_str: Option<String> = row.get("date");
            let time_str: Option<String> = row.get("time");

            let Some(date_s) = date_str.as_deref() else { continue };
            let Some(timestamp) = parse_nomis_datetime(date_s, time_str.as_deref()) else {
                tracing::debug!(location = %id_location, date = ?date_s, "Skipping unparseable date");
                continue;
            };

            for (col, _, _) in LOCATION_PARAMS {
                let source_key = format!("{id_location}:{col}");
                if let Some((_, since)) = stream_lookup.get(&source_key) {
                    if let Some(s) = since {
                        if timestamp <= *s {
                            continue;
                        }
                    }

                    let value: Option<f64> = row.try_get::<Option<f64>, _>(*col).unwrap_or(None);
                    if let Some(val) = value.filter(|&v| v > -9000.0) {
                        per_stream.entry(source_key).or_default().push(ReadingValue {
                            time: timestamp,
                            value: val,
                            replicate_index: 0,
                        });
                    }
                }
            }
        }

        // ── biogeo_1 readings (with replicates) ──
        let biogeo1_rows: Vec<MySqlRow> = sqlx::query(
            "SELECT b.id_location, b.replicate, \
                    b.i1_na, b.i2_k, b.i3_mg, b.i4_ca, b.i5_cl, b.i6_so4, \
                    b.n1_tn, b.n2_tp, b.n3_srp, b.n4_nh4, b.n5_no3, b.n6_no2, \
                    b.hydro_isotope, b.oxy_isotope, \
                    l.date, l.time \
             FROM biogeo_1 b \
             JOIN location l ON b.id_location = l.id_location"
        )
        .fetch_all(pool)
        .await?;

        for row in &biogeo1_rows {
            let id_location: String = row.get("id_location");
            let replicate: String = row.get("replicate");
            let date_str: Option<String> = row.get("date");
            let time_str: Option<String> = row.get("time");

            let Some(date_s) = date_str.as_deref() else { continue };
            let Some(timestamp) = parse_nomis_datetime(date_s, time_str.as_deref()) else {
                continue;
            };

            let rep_idx = replicate_to_index(&replicate);

            for (col, _, _) in BIOGEO1_PARAMS {
                let source_key = format!("{id_location}:biogeo1:{col}");
                if let Some((_, since)) = stream_lookup.get(&source_key) {
                    if let Some(s) = since {
                        if timestamp <= *s {
                            continue;
                        }
                    }

                    let value: Option<f64> = row.try_get::<Option<f64>, _>(*col).unwrap_or(None);
                    if let Some(val) = value.filter(|&v| v > -9000.0) {
                        per_stream.entry(source_key).or_default().push(ReadingValue {
                            time: timestamp,
                            value: val,
                            replicate_index: rep_idx,
                        });
                    }
                }
            }
        }

        // ── biogeo_1u readings (TSS, with replicates) ──
        let biogeo1u_rows: Vec<MySqlRow> = sqlx::query(
            "SELECT b.id_location, b.replicate, \
                    b.tss_1, b.tss_2, b.tss_3, b.tss_4, b.tss_5, \
                    b.tss_5a, b.tss_5b, b.tss_5c, b.tss_5d, \
                    l.date, l.time \
             FROM biogeo_1u b \
             JOIN location l ON b.id_location = l.id_location"
        )
        .fetch_all(pool)
        .await?;

        for row in &biogeo1u_rows {
            let id_location: String = row.get("id_location");
            let replicate: String = row.get("replicate");
            let date_str: Option<String> = row.get("date");
            let time_str: Option<String> = row.get("time");

            let Some(date_s) = date_str.as_deref() else { continue };
            let Some(timestamp) = parse_nomis_datetime(date_s, time_str.as_deref()) else {
                continue;
            };

            let rep_idx = replicate_to_index(&replicate);

            for (col, _, _) in BIOGEO1U_PARAMS {
                let source_key = format!("{id_location}:biogeo1u:{col}");
                if let Some((_, since)) = stream_lookup.get(&source_key) {
                    if let Some(s) = since {
                        if timestamp <= *s {
                            continue;
                        }
                    }

                    let value: Option<f64> = row.try_get::<Option<f64>, _>(*col).unwrap_or(None);
                    if let Some(val) = value.filter(|&v| v > -9000.0) {
                        per_stream.entry(source_key).or_default().push(ReadingValue {
                            time: timestamp,
                            value: val,
                            replicate_index: rep_idx,
                        });
                    }
                }
            }
        }

        // ── biogeo_3u readings (with replicates) ──
        let biogeo3u_rows: Vec<MySqlRow> = sqlx::query(
            "SELECT b.id_location, b.replicate, \
                    b.doc, b.abs254, b.abs300, b.suva, b.e2e3, b.e4e6, \
                    b.s275295, b.s350400, b.s300700, b.sr, \
                    b.bix, b.fi, b.hix, \
                    b.coble_b, b.coble_t, b.coble_a, b.coble_m, b.coble_c, b.coble_r, \
                    l.date, l.time \
             FROM biogeo_3u b \
             JOIN location l ON b.id_location = l.id_location"
        )
        .fetch_all(pool)
        .await?;

        for row in &biogeo3u_rows {
            let id_location: String = row.get("id_location");
            let replicate: String = row.get("replicate");
            let date_str: Option<String> = row.get("date");
            let time_str: Option<String> = row.get("time");

            let Some(date_s) = date_str.as_deref() else { continue };
            let Some(timestamp) = parse_nomis_datetime(date_s, time_str.as_deref()) else {
                continue;
            };

            let rep_idx = replicate_to_index(&replicate);

            for (col, _, _) in BIOGEO3U_PARAMS {
                let source_key = format!("{id_location}:biogeo3u:{col}");
                if let Some((_, since)) = stream_lookup.get(&source_key) {
                    if let Some(s) = since {
                        if timestamp <= *s {
                            continue;
                        }
                    }

                    let value: Option<f64> = row.try_get::<Option<f64>, _>(*col).unwrap_or(None);
                    if let Some(val) = value.filter(|&v| v > -9000.0) {
                        per_stream.entry(source_key).or_default().push(ReadingValue {
                            time: timestamp,
                            value: val,
                            replicate_index: rep_idx,
                        });
                    }
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

        tracing::info!(
            streams = all_results.len(),
            total_readings = all_results.iter().map(|r| r.readings.len()).sum::<usize>(),
            "Fetched NOMIS readings"
        );
        Ok(all_results)
    }
}
