use super::{BatchAttribution, batch_attribution, batch_instrument, ingest_instrument};
use crate::routes::private::sensors::models::ResolvedOwner;
use uuid::Uuid;

const ROW: Option<Uuid> = Some(Uuid::from_u128(1));
const STREAM: Option<Uuid> = Some(Uuid::from_u128(2));
const SLOT: Option<Uuid> = Some(Uuid::from_u128(3));

/// Three candidate ids in the order the writer names them, and the one expected to win.
type Case = (Option<Uuid>, Option<Uuid>, Option<Uuid>, Option<Uuid>);

/// Every combination of the three candidates, so the precedence is pinned rather than the one
/// happy path. The two orders differ only in the middle rung, which is the thing a swap changes
/// and the thing a route test cannot see.
#[test]
fn test_batch_prefers_the_row_then_the_slot_then_the_stream() {
    let cases: [Case; 8] = [
        (ROW, SLOT, STREAM, ROW),
        (ROW, SLOT, None, ROW),
        (ROW, None, STREAM, ROW),
        (ROW, None, None, ROW),
        (None, SLOT, STREAM, SLOT),
        (None, SLOT, None, SLOT),
        (None, None, STREAM, STREAM),
        (None, None, None, None),
    ];
    for (row, slot, stream, expected) in cases {
        assert_eq!(
            batch_instrument(row, slot, stream),
            expected,
            "row={row:?} slot={slot:?} stream={stream:?}"
        );
    }
}

#[test]
fn test_ingest_prefers_the_row_then_the_stream_then_the_slot() {
    let cases: [Case; 8] = [
        (ROW, STREAM, SLOT, ROW),
        (ROW, STREAM, None, ROW),
        (ROW, None, SLOT, ROW),
        (ROW, None, None, ROW),
        (None, STREAM, SLOT, STREAM),
        (None, STREAM, None, STREAM),
        (None, None, SLOT, SLOT),
        (None, None, None, None),
    ];
    for (row, stream, slot, expected) in cases {
        assert_eq!(
            ingest_instrument(row, stream, slot),
            expected,
            "row={row:?} stream={stream:?} slot={slot:?}"
        );
    }
}

/// The one case the two paths answer differently, stated on its own: with no instrument on the
/// row, a batch takes the slot's deployment and an ingest takes the stream's frozen instrument.
/// Making the two agree would be a change to what is stored, not a tidy-up.
#[test]
fn test_the_two_orders_disagree_only_when_the_row_names_nothing() {
    assert_eq!(batch_instrument(None, SLOT, STREAM), SLOT);
    assert_eq!(ingest_instrument(None, STREAM, SLOT), STREAM);
    assert_ne!(
        batch_instrument(None, SLOT, STREAM),
        ingest_instrument(None, STREAM, SLOT),
        "the middle rung is the whole difference between the two write paths"
    );
}

fn slot_owner() -> ResolvedOwner {
    ResolvedOwner {
        sensor_id: Some(Uuid::from_u128(10)),
        deployment_id: Some(Uuid::from_u128(11)),
        calibration_id: Some(Uuid::from_u128(12)),
    }
}

const CHANNEL: Option<Uuid> = Some(Uuid::from_u128(20));

#[test]
fn test_batch_attribution_paired_derives_what_the_row_leaves_out() {
    let owner = slot_owner();
    assert_eq!(
        batch_attribution(BatchAttribution::default(), &owner, CHANNEL, true),
        BatchAttribution {
            sensor_id: owner.sensor_id,
            deployment_id: owner.deployment_id,
            calibration_id: owner.calibration_id,
        }
    );
    assert_eq!(
        batch_attribution(
            BatchAttribution::default(),
            &ResolvedOwner::default(),
            CHANNEL,
            true
        ),
        BatchAttribution {
            sensor_id: CHANNEL,
            ..BatchAttribution::default()
        },
        "no deployment covers the time: the channel's instrument stands in"
    );
}

#[test]
fn test_batch_attribution_paired_keeps_what_the_row_declares() {
    let declared = BatchAttribution {
        sensor_id: ROW,
        deployment_id: Some(Uuid::from_u128(30)),
        calibration_id: Some(Uuid::from_u128(31)),
    };
    assert_eq!(
        batch_attribution(declared, &slot_owner(), CHANNEL, true),
        declared
    );
}

#[test]
fn test_batch_attribution_unpaired_derives_nothing() {
    assert_eq!(
        batch_attribution(BatchAttribution::default(), &slot_owner(), CHANNEL, false),
        BatchAttribution::default(),
        "no instrument, deployment or curve until the pairing stamps them"
    );
}

#[test]
fn test_batch_attribution_unpaired_keeps_only_what_the_row_declares() {
    let declared = BatchAttribution {
        sensor_id: ROW,
        deployment_id: None,
        calibration_id: Some(Uuid::from_u128(31)),
    };
    assert_eq!(
        batch_attribution(declared, &slot_owner(), CHANNEL, false),
        declared
    );
}

/// The instrument a staged row's curve claim is judged against is the one it is stored with, so a
/// deployment at a slot the site does not carry neither admits a curve nor classifies the row.
#[test]
fn test_batch_attribution_unpaired_cadence_reads_the_declared_then_the_channel() {
    let owner = slot_owner();
    let staged = batch_attribution(BatchAttribution::default(), &owner, None, false);
    assert_eq!(staged.sensor_id, None, "a curve claim names no instrument");
    assert_eq!(staged.cadence_instrument(None), None);
    assert_eq!(
        staged.cadence_instrument(CHANNEL),
        CHANNEL,
        "which device a feed comes through is a fact about the channel"
    );
    let declared = batch_attribution(
        BatchAttribution {
            sensor_id: ROW,
            ..BatchAttribution::default()
        },
        &owner,
        CHANNEL,
        false,
    );
    assert_eq!(declared.sensor_id, ROW);
    assert_eq!(declared.cadence_instrument(CHANNEL), ROW);
}

#[test]
fn test_batch_attribution_paired_cadence_reads_the_stored_instrument() {
    let owner = slot_owner();
    let deployed = batch_attribution(BatchAttribution::default(), &owner, CHANNEL, true);
    assert_eq!(deployed.cadence_instrument(CHANNEL), owner.sensor_id);
    let undeployed = batch_attribution(
        BatchAttribution::default(),
        &ResolvedOwner::default(),
        CHANNEL,
        true,
    );
    assert_eq!(
        undeployed.cadence_instrument(CHANNEL),
        CHANNEL,
        "the channel's instrument the row is stored with"
    );
}
