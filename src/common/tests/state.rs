use uuid::Uuid;

use super::{AppEvent, SlotTally};

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn announced(tally: &SlotTally) -> Vec<(Option<Uuid>, Option<Uuid>, usize)> {
    tally
        .events()
        .into_iter()
        .map(|event| match event {
            AppEvent::DataIngested {
                site_id,
                parameter_id,
                stream_id,
                count,
            } => {
                assert_eq!(stream_id, None, "a rewrite arrives on no channel");
                (site_id, parameter_id, count)
            }
            other => panic!("a tally announces only DataIngested, got {other:?}"),
        })
        .collect()
}

#[test]
fn test_slot_tally_empty_announces_nothing() {
    assert!(SlotTally::default().events().is_empty());
}

#[test]
fn test_slot_tally_counts_rows_per_slot() {
    let mut tally = SlotTally::default();
    tally.add(Some(id(1)), Some(id(10)), 2);
    tally.add(Some(id(1)), Some(id(10)), 3);
    tally.add(Some(id(1)), Some(id(11)), 1);
    tally.add(Some(id(2)), None, 4);
    assert_eq!(
        announced(&tally),
        vec![
            (Some(id(1)), Some(id(10)), 5), // 2 + 3
            (Some(id(1)), Some(id(11)), 1),
            (Some(id(2)), None, 4),
        ]
    );
}

#[test]
fn test_slot_tally_skips_rows_without_a_site_and_zero_counts() {
    let mut tally = SlotTally::default();
    tally.add(None, Some(id(10)), 7);
    tally.add(Some(id(1)), Some(id(10)), 0);
    assert!(tally.events().is_empty());
}

#[test]
fn test_slot_tally_merge_sums_shared_slots() {
    let mut first = SlotTally::default();
    first.add(Some(id(1)), Some(id(10)), 2);
    let mut second = SlotTally::default();
    second.add(Some(id(1)), Some(id(10)), 1);
    second.add(Some(id(2)), Some(id(10)), 1);
    first.merge(second);
    assert_eq!(
        announced(&first),
        vec![
            (Some(id(1)), Some(id(10)), 3), // 2 + 1
            (Some(id(2)), Some(id(10)), 1),
        ]
    );
}

#[test]
fn test_slot_tally_announce_sends_one_event_per_slot() {
    let (sender, mut receiver) = tokio::sync::broadcast::channel(8);
    let mut tally = SlotTally::default();
    tally.add(Some(id(1)), Some(id(10)), 2);
    tally.add(Some(id(2)), Some(id(10)), 1);
    tally.announce(&sender);
    let mut sites = Vec::new();
    while let Ok(AppEvent::DataIngested { site_id, .. }) = receiver.try_recv() {
        sites.push(site_id);
    }
    assert_eq!(sites, vec![Some(id(1)), Some(id(2))]);
}
