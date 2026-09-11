use super::JanitorRun;
use crate::routes::private::alarms::flows::AlarmSweep;
use crate::routes::private::reprocessing_jobs::service::{Job, TunableKind};

fn janitor() -> JanitorRun {
    JanitorRun {
        interval_seconds: 300,
        full_refresh_seconds: 3600,
        maintenance_retention_days: 7,
        operator_retention_days: 90,
        maintenance_max_rows: 100_000,
    }
}

#[test]
fn a_misspelt_tunable_is_refused_naming_it() {
    let err = janitor()
        .validate(&serde_json::json!({ "retention_dayz": 7 }))
        .unwrap_err();
    assert!(err.contains("retention_dayz"), "{err}");
    assert!(err.contains("retention_days"), "{err}");
}

#[test]
fn the_janitor_still_takes_its_one_key() {
    assert!(
        janitor()
            .validate(&serde_json::json!({ "retention_days": 7 }))
            .is_ok()
    );
    assert!(janitor().validate(&serde_json::json!({})).is_ok());
    assert!(janitor().validate(&serde_json::Value::Null).is_ok());
    assert!(
        janitor()
            .validate(&serde_json::json!({ "retention_days": 0 }))
            .is_err()
    );
}

#[test]
fn a_job_with_no_tunables_refuses_every_key() {
    let sweep = AlarmSweep {
        interval_seconds: 60,
    };
    assert!(sweep.validate(&serde_json::json!({})).is_ok());
    let err = sweep
        .validate(&serde_json::json!({ "retention_days": 7 }))
        .unwrap_err();
    assert!(err.contains("no tunables"), "{err}");
    assert!(err.contains("retention_days"), "{err}");
}

/// Every job accepts a tunables object built from its own declared defaults, and refuses a
/// value outside a spec's range.
#[test]
fn every_job_accepts_its_own_defaults_and_refuses_an_out_of_range_value() {
    let registry = crate::routes::private::reprocessing_jobs::service::build_registry();
    for name in registry.names() {
        let handler = registry.get(name).expect("a listed name is registered");
        let specs = handler.tunables();
        let defaults: serde_json::Map<String, serde_json::Value> = specs
            .iter()
            .map(|s| (s.key.to_string(), s.default.clone()))
            .collect();
        handler
            .validate(&serde_json::Value::Object(defaults))
            .unwrap_or_else(|e| panic!("{name} refuses its own defaults: {e}"));

        for spec in &specs {
            let Some(min) = spec.min else { continue };
            if !matches!(spec.kind, TunableKind::Integer | TunableKind::Duration) {
                continue;
            }
            let below = serde_json::json!({ &spec.key: min - 1 });
            assert!(
                handler.validate(&below).is_err(),
                "{name} accepts {} below its minimum",
                spec.key
            );
        }
    }
}
