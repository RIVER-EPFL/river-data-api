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
    if calculations
        .iter()
        .any(|c| c.outputs.contains(&parameter_id))
    {
        return Role::Output;
    }
    if calculations
        .iter()
        .any(|c| c.inputs.contains(&parameter_id))
    {
        return Role::Measured;
    }
    Role::EntryOnly
}

#[cfg(test)]
#[path = "tests/rules.rs"]
mod tests;
