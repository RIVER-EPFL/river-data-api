use super::*;

fn parameter() -> Uuid {
    Uuid::from_u128(0x5741_0001)
}

fn other() -> Uuid {
    Uuid::from_u128(0x5741_0002)
}

/// A visit nobody has typed into touches nothing, so every family reads from the store.
#[test]
fn test_empty_visit_touches_nothing() {
    let staged = StagedVisit::default();
    assert!(!staged.touches(parameter()));
    assert!(!staged.is_retracted(parameter()));
    assert_eq!(staged.cells_of(parameter()).count(), 0);
}

/// An absent cell and a cleared cell are different states: the first is not in the overlay at all,
/// the second is in it with no value (Q227).
#[test]
fn test_cleared_cell_is_not_an_absent_one() {
    let mut staged = StagedVisit::default();
    staged.cells.insert((parameter(), 0), None);
    assert!(staged.touches(parameter()));
    assert_eq!(
        staged.cells_of(parameter()).collect::<Vec<_>>(),
        vec![(0, None)]
    );
    assert!(!staged.touches(other()));
}

/// The cells of one parameter come back lowest index first, and a neighbouring parameter's cells
/// are not among them.
#[test]
fn test_cells_of_is_by_index_and_by_parameter() {
    let mut staged = StagedVisit::default();
    staged.cells.insert((parameter(), 2), Some(3.0));
    staged.cells.insert((parameter(), 0), Some(1.0));
    staged.cells.insert((other(), 1), Some(9.0));
    assert_eq!(
        staged.cells_of(parameter()).collect::<Vec<_>>(),
        vec![(0, Some(1.0)), (2, Some(3.0))]
    );
    assert_eq!(
        staged.cells_of(other()).collect::<Vec<_>>(),
        vec![(1, Some(9.0))]
    );
}

/// An output the chain produced replaces the family the store holds, index for index: the save
/// writes its outputs as a replace, so a preview that merged them with the stored ones would show
/// repeats the save is about to drop.
#[test]
fn test_taking_an_output_replaces_the_stored_family() {
    let mut staged = StagedVisit::default();
    staged.take_output(parameter(), &[(0, 1.5), (2, 2.5)]);
    assert!(staged.is_retracted(parameter()));
    assert_eq!(
        staged.cells_of(parameter()).collect::<Vec<_>>(),
        vec![(0, Some(1.5)), (2, Some(2.5))]
    );
}

/// A cleared output empties the slot downstream, as the save's withdrawal leaves it.
#[test]
fn test_retracting_an_output_empties_the_slot() {
    let mut staged = StagedVisit::default();
    staged.cells.insert((parameter(), 0), Some(7.0));
    staged.cells.insert((other(), 0), Some(8.0));
    staged.retract(parameter());
    assert!(staged.is_retracted(parameter()));
    assert_eq!(staged.cells_of(parameter()).count(), 0);
    assert_eq!(
        staged.cells_of(other()).collect::<Vec<_>>(),
        vec![(0, Some(8.0))]
    );
}

/// A slot that has never held a grab has no stream to name, and the preview does not mint one.
#[test]
fn test_stream_of_is_absent_until_the_slot_has_one() {
    let mut staged = StagedVisit::default();
    assert_eq!(staged.stream_of(parameter()), None);
    let stream = Uuid::from_u128(0x5741_0003);
    staged.streams.insert(parameter(), stream);
    assert_eq!(staged.stream_of(parameter()), Some(stream));
}
