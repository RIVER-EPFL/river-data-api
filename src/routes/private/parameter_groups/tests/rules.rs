use super::rules::*;
use uuid::Uuid;

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn member(group: u128, parameter: u128) -> Member {
    Member {
        group_id: id(group),
        parameter_id: id(parameter),
    }
}

fn dom_calculation() -> Calculation {
    Calculation {
        group_id: id(1),
        name: "dom".to_string(),
        inputs: vec![id(10), id(11)],
        outputs: vec![id(20)],
    }
}

#[test]
fn test_may_add_refuses_a_parameter_another_group_holds() {
    let members = [member(1, 10)];
    assert_eq!(
        may_add(id(10), &members),
        Err(Refusal::AlreadyGrouped { group_id: id(1) })
    );
    assert_eq!(may_add(id(11), &members), Ok(()));
}

#[test]
fn test_may_add_accepts_into_an_empty_membership_table() {
    assert_eq!(may_add(id(10), &[]), Ok(()));
}

#[test]
fn test_may_add_code_refuses_the_mean_and_sd_of_a_replicated_member() {
    for code in ["DOC_avg_ppb", "DOC_sd_ppb", "doc_mean_ppb"] {
        assert_eq!(
            may_add_code(code, &["DOC_ppb"]),
            Err(Refusal::StatisticOfMember {
                member_code: "DOC_ppb".to_string()
            }),
            "{code}"
        );
    }
}

#[test]
fn test_may_add_code_accepts_an_analyte_whose_name_carries_no_statistic() {
    assert_eq!(may_add_code("DOC_ppb", &["TSS_mgL"]), Ok(()));
    assert_eq!(may_add_code("Std_Curve", &["DOC_ppb"]), Ok(()));
}

#[test]
fn test_may_add_code_accepts_a_statistic_of_a_member_no_group_replicates() {
    assert_eq!(may_add_code("TSS_avg_mgL", &["DOC_ppb"]), Ok(()));
}

#[test]
fn test_may_move_refuses_an_output_its_group_produces() {
    let output = member(1, 20);
    assert_eq!(
        may_move(output, id(2), &[dom_calculation()]),
        Err(Refusal::ProducedHere {
            calculation: "dom".to_string()
        })
    );
}

#[test]
fn test_may_move_allows_an_output_nothing_produces() {
    let orphan = member(1, 21);
    assert_eq!(may_move(orphan, id(2), &[dom_calculation()]), Ok(()));
}

#[test]
fn test_may_move_allows_a_measured_member_a_calculation_reads() {
    let input = member(1, 10);
    assert_eq!(may_move(input, id(2), &[dom_calculation()]), Ok(()));
}

#[test]
fn test_may_move_within_the_group_is_a_reorder() {
    let output = member(1, 20);
    assert_eq!(may_move(output, id(1), &[dom_calculation()]), Ok(()));
}

#[test]
fn test_may_move_ignores_a_calculation_in_another_group() {
    let output = member(2, 20);
    assert_eq!(may_move(output, id(3), &[dom_calculation()]), Ok(()));
}

#[test]
fn test_may_delete_refuses_a_group_with_members() {
    let members = [member(1, 10), member(1, 11)];
    assert_eq!(
        may_delete(id(1), &members),
        Err(Refusal::GroupHasMembers { count: 2 })
    );
}

#[test]
fn test_may_delete_accepts_an_empty_group() {
    let members = [member(2, 10)];
    assert_eq!(may_delete(id(1), &members), Ok(()));
    assert_eq!(may_delete(id(1), &[]), Ok(()));
}

/// Scenario: pCO2, which reads a field-data parameter of another group and writes its own,
/// and whose stage 2 reads what its stage 1 wrote.
///
/// Expected behaviour: the role follows from the calculations. Nothing is refused for crossing
/// a group, because a group is a filter and not a boundary (Q135).
#[test]
fn test_derive_role_reads_the_calculations_never_a_declaration() {
    let pco2 = Calculation {
        group_id: id(1),
        name: "pco2".to_string(),
        // 11 is a field_data member; 20 is written by stage 1 and read by stage 2.
        inputs: vec![id(11), id(20)],
        outputs: vec![id(20), id(21)],
    };
    let calculations = [pco2];
    assert_eq!(derive_role(id(11), &calculations), Role::Measured);
    assert_eq!(derive_role(id(21), &calculations), Role::Output);
    // Written and read by the same calculation: what it writes is what it is.
    assert_eq!(derive_role(id(20), &calculations), Role::Output);
    // Touched by no calculation: entered at a visit and read by nothing.
    assert_eq!(derive_role(id(99), &calculations), Role::EntryOnly);
    assert_eq!(derive_role(id(11), &[]), Role::EntryOnly);
}

#[test]
fn test_role_round_trips_through_its_stored_form() {
    for role in [Role::Measured, Role::EntryOnly, Role::Output] {
        assert_eq!(Role::parse(role.as_str()), Some(role));
    }
    assert_eq!(Role::parse("derived"), None);
}
