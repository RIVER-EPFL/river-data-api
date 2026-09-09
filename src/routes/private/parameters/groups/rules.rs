//! The reshape rules for a parameter group, as pure decisions over a group's members and the
//! calculations attached to it. The database holds the unique membership and the role CHECK; these
//! are the rules SQL cannot state, and the CRUD hooks are their only callers.

use std::fmt;

use uuid::Uuid;

/// What a parameter is doing inside its group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Entered at a visit and read by a calculation.
    Measured,
    /// Entered at a visit and read by nothing.
    EntryOnly,
    /// Produced by the group's calculation.
    Output,
}

impl Role {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "measured" => Some(Self::Measured),
            "entry_only" => Some(Self::EntryOnly),
            "output" => Some(Self::Output),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Measured => "measured",
            Self::EntryOnly => "entry_only",
            Self::Output => "output",
        }
    }
}

/// One membership row, reduced to what the rules read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Member {
    pub group_id: Uuid,
    pub parameter_id: Uuid,
    pub role: Role,
}

/// A calculation attached to a group, naming the parameters it consumes and produces.
#[derive(Clone, Debug)]
pub struct Calculation {
    pub group_id: Uuid,
    pub name: String,
    pub inputs: Vec<Uuid>,
    pub outputs: Vec<Uuid>,
}

/// Why a reshape was refused. Each carries what the caller has to say back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The parameter already belongs to another group.
    AlreadyGrouped { group_id: Uuid },
    /// An `output` member cannot leave while a calculation in its group produces it.
    ProducedHere { calculation: String },
    /// A group with members is not deleted; its members are moved first.
    GroupHasMembers { count: usize },
    /// The parameter is the mean or sd of a replicated member of the same group.
    StatisticOfMember { member_code: String },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyGrouped { group_id } => {
                write!(f, "parameter already belongs to group {group_id}")
            }
            Self::ProducedHere { calculation } => write!(
                f,
                "calculation {calculation} produces this parameter; retire it from the calculation first"
            ),
            Self::GroupHasMembers { count } => {
                write!(f, "group still has {count} members; move them out first")
            }
            Self::StatisticOfMember { member_code } => write!(
                f,
                "this is the mean or sd of member {member_code}, which the samples trigger computes; it renders beside that member and is not one"
            ),
        }
    }
}

/// A parameter belongs to at most one group, so adding it anywhere else is refused.
pub fn may_add(parameter_id: Uuid, members: &[Member]) -> Result<(), Refusal> {
    match members.iter().find(|m| m.parameter_id == parameter_id) {
        Some(existing) => Err(Refusal::AlreadyGrouped {
            group_id: existing.group_id,
        }),
        None => Ok(()),
    }
}

/// The segments a portal statistics column is named with, between the analyte and its units:
/// `DOC_avg_ppb` and `DOC_sd_ppb` are the mean and sd of `DOC_ppb`.
const STATISTIC_SEGMENTS: [&str; 5] = ["avg", "mean", "sd", "stdev", "std"];

/// A replicated member's statistics are computed by the `samples` trigger and shown beside it, so
/// the group holds no member for them. The portals stored each as a parameter of its own, and one
/// taken in as a member renders a second mean nothing maintains and an operator can type into.
///
/// `replicated` is the code of every member of the group that is entered several times.
pub fn may_add_code(code: &str, replicated: &[&str]) -> Result<(), Refusal> {
    let segments: Vec<&str> = code.split('_').collect();
    for (index, segment) in segments.iter().enumerate() {
        if !STATISTIC_SEGMENTS.contains(&segment.to_lowercase().as_str()) {
            continue;
        }
        let mut rest = segments.clone();
        rest.remove(index);
        let stripped = rest.join("_");
        if let Some(member) = replicated
            .iter()
            .find(|m| m.eq_ignore_ascii_case(&stripped))
        {
            return Err(Refusal::StatisticOfMember {
                member_code: (*member).to_string(),
            });
        }
    }
    Ok(())
}

/// A member leaves its group only when nothing in that group produces it. A move within the same
/// group is a reorder, not a move, and is always allowed.
pub fn may_move(
    member: Member,
    to_group: Uuid,
    calculations: &[Calculation],
) -> Result<(), Refusal> {
    if member.group_id == to_group {
        return Ok(());
    }
    if member.role != Role::Output {
        return Ok(());
    }
    match calculations
        .iter()
        .find(|c| c.group_id == member.group_id && c.outputs.contains(&member.parameter_id))
    {
        Some(producer) => Err(Refusal::ProducedHere {
            calculation: producer.name.clone(),
        }),
        None => Ok(()),
    }
}

/// A group is deleted only once it is empty; a split is a new group plus moves.
pub fn may_delete(group_id: Uuid, members: &[Member]) -> Result<(), Refusal> {
    let count = members.iter().filter(|m| m.group_id == group_id).count();
    if count > 0 {
        return Err(Refusal::GroupHasMembers { count });
    }
    Ok(())
}

/// What a parameter is to the calculations, which is what its role says (Q135).
///
/// The role is read off the calculations, never set by hand: a parameter one writes is an output,
/// one a calculation reads is measured, and a parameter no calculation touches is entered and read
/// by nothing. A calculation may read any catalog parameter, so a group is a way to list many
/// parameters together, not a boundary a calculation is confined to.
#[must_use]
pub fn derive_role(parameter_id: Uuid, calculations: &[Calculation]) -> Role {
    if calculations.iter().any(|c| c.outputs.contains(&parameter_id)) {
        return Role::Output;
    }
    if calculations.iter().any(|c| c.inputs.contains(&parameter_id)) {
        return Role::Measured;
    }
    Role::EntryOnly
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn member(group: u128, parameter: u128, role: Role) -> Member {
        Member {
            group_id: id(group),
            parameter_id: id(parameter),
            role,
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
        let members = [member(1, 10, Role::Measured)];
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
        let output = member(1, 20, Role::Output);
        assert_eq!(
            may_move(output, id(2), &[dom_calculation()]),
            Err(Refusal::ProducedHere {
                calculation: "dom".to_string()
            })
        );
    }

    #[test]
    fn test_may_move_allows_an_output_nothing_produces() {
        let orphan = member(1, 21, Role::Output);
        assert_eq!(may_move(orphan, id(2), &[dom_calculation()]), Ok(()));
    }

    #[test]
    fn test_may_move_allows_a_measured_member_a_calculation_reads() {
        let input = member(1, 10, Role::Measured);
        assert_eq!(may_move(input, id(2), &[dom_calculation()]), Ok(()));
    }

    #[test]
    fn test_may_move_within_the_group_is_a_reorder() {
        let output = member(1, 20, Role::Output);
        assert_eq!(may_move(output, id(1), &[dom_calculation()]), Ok(()));
    }

    #[test]
    fn test_may_move_ignores_a_calculation_in_another_group() {
        let output = member(2, 20, Role::Output);
        assert_eq!(may_move(output, id(3), &[dom_calculation()]), Ok(()));
    }

    #[test]
    fn test_may_delete_refuses_a_group_with_members() {
        let members = [
            member(1, 10, Role::Measured),
            member(1, 11, Role::EntryOnly),
        ];
        assert_eq!(
            may_delete(id(1), &members),
            Err(Refusal::GroupHasMembers { count: 2 })
        );
    }

    #[test]
    fn test_may_delete_accepts_an_empty_group() {
        let members = [member(2, 10, Role::Measured)];
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
}
