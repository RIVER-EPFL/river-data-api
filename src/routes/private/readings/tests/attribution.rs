use super::{batch_instrument, ingest_instrument};
use uuid::Uuid;

const ROW: Option<Uuid> = Some(Uuid::from_u128(1));
const STREAM: Option<Uuid> = Some(Uuid::from_u128(2));
const SLOT: Option<Uuid> = Some(Uuid::from_u128(3));

/// Every combination of the three candidates, so the precedence is pinned rather than the one
/// happy path. The two orders differ only in the middle rung, which is the thing a swap changes
/// and the thing a route test cannot see.
#[test]
fn test_batch_prefers_the_row_then_the_slot_then_the_stream() {
    let cases: [(Option<Uuid>, Option<Uuid>, Option<Uuid>, Option<Uuid>); 8] = [
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
    let cases: [(Option<Uuid>, Option<Uuid>, Option<Uuid>, Option<Uuid>); 8] = [
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
