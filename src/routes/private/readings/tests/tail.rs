use super::*;
use chrono::TimeZone;

fn slot(site: u128, parameter: u128) -> Slot {
    Slot::paired(Uuid::from_u128(site), Uuid::from_u128(parameter))
}

fn at(hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, hour, 0, 0).unwrap()
}

fn axes() -> Axes {
    Axes {
        cache: Cache::Sites,
        refresh: Refresh::Range { fatal: false },
        announce: true,
        reconcile_alarms: true,
        episodes: Episodes::Inline,
        recompute_derived: false,
        writer: crate::routes::private::collection_events::flows::Writer::Person,
    }
}

fn written() -> Written {
    Written::new(3)
        .over(Some((at(1), at(5))))
        .at(vec![slot(1, 10), slot(1, 11), slot(2, 10)])
}

#[test]
fn a_write_that_moved_nothing_runs_no_step() {
    let plan = plan(&Written::new(0).over(Some((at(1), at(5)))), &axes());
    assert_eq!(plan, Plan::nothing());
}

#[test]
fn the_refresh_window_is_the_span_the_write_covers() {
    let plan = plan(&written(), &axes());
    assert!(matches!(plan.refresh, Some(Window::Range(lo, hi)) if lo == at(1) && hi == at(5)));
    assert!(!plan.refresh_fatal);
}

#[test]
fn a_since_refresh_runs_from_the_earliest_instant_touched() {
    let axes = Axes {
        refresh: Refresh::Since { fatal: true },
        ..axes()
    };
    let plan = plan(&written(), &axes);
    assert!(matches!(plan.refresh, Some(Window::Since(lo)) if lo == at(1)));
    assert!(plan.refresh_fatal);
}

#[test]
fn a_write_reporting_no_span_refreshes_nothing_and_rebuilds_no_episode() {
    let plan = plan(&Written::new(3).at(vec![slot(1, 10)]), &axes());
    assert!(plan.refresh.is_none());
    assert_eq!(plan.episodes, Episodes::None);
    assert!(plan.episode_span.is_none());
    // The slot still had rows written to it, so it is announced and reconciled.
    assert_eq!(plan.announce.len(), 1);
    assert_eq!(plan.reconcile.len(), 1);
}

#[test]
fn per_site_invalidation_names_each_site_once() {
    let plan = plan(&written(), &axes());
    assert!(!plan.invalidate_all);
    assert_eq!(
        plan.invalidate_sites,
        vec![Uuid::from_u128(1), Uuid::from_u128(2)]
    );
}

#[test]
fn a_write_that_rewrites_history_invalidates_everything_and_names_no_site() {
    let plan = plan(
        &written(),
        &Axes {
            cache: Cache::All,
            ..axes()
        },
    );
    assert!(plan.invalidate_all);
    assert!(plan.invalidate_sites.is_empty());
}

#[test]
fn alarm_reconciliation_is_one_entry_per_slot_and_is_skipped_when_the_axis_says_so() {
    let plan = plan(&written(), &axes());
    assert_eq!(plan.reconcile.len(), 3);
    let plan = plan_without_reconcile();
    assert!(plan.reconcile.is_empty());
}

fn plan_without_reconcile() -> Plan {
    plan(
        &written(),
        &Axes {
            reconcile_alarms: false,
            ..axes()
        },
    )
}

#[test]
fn a_write_nothing_subscribes_to_announces_no_slot() {
    let plan = plan(
        &written(),
        &Axes {
            announce: false,
            ..axes()
        },
    );
    assert!(plan.announce.is_empty());
}

#[test]
fn an_unpaired_stream_is_announced_and_nothing_else() {
    let unpaired = Slot {
        site_id: None,
        parameter_id: None,
        stream_id: Some(Uuid::from_u128(9)),
    };
    let plan = plan(
        &Written::new(2)
            .over(Some((at(1), at(2))))
            .at(vec![unpaired]),
        &axes(),
    );
    assert_eq!(plan.announce, vec![unpaired]);
    assert!(plan.invalidate_sites.is_empty());
    assert!(plan.reconcile.is_empty());
}

#[test]
fn the_chain_never_asks_for_its_own_recompute() {
    let touched = vec![TouchedEvent {
        id: Uuid::from_u128(7),
        source: "manual".to_string(),
        parameter_ids: vec![Uuid::from_u128(10)],
    }];
    let write = written().touching(touched);
    assert_eq!(plan(&write, &axes()).recompute_events.len(), 1);
    let plan = plan(
        &write,
        &Axes {
            recompute_derived: false,
            writer: crate::routes::private::collection_events::flows::Writer::Chain,
            ..axes()
        },
    );
    assert!(plan.recompute_events.is_empty());
}
