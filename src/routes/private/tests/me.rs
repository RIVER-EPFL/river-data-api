use super::{highest_role, role_token};
use crate::common::authz::Role;

#[test]
fn test_the_highest_of_several_levels_is_the_callers_role() {
    assert_eq!(
        highest_role(&[Role::Intern, Role::Manager, Role::River]),
        Role::Manager
    );
    assert_eq!(
        highest_role(&[Role::River, Role::Administrator]),
        Role::Administrator
    );
}

#[test]
fn test_an_unknown_role_never_outranks_a_river_level() {
    assert_eq!(
        highest_role(&[Role::Unknown("offline_access".to_string()), Role::Intern]),
        Role::Intern
    );
}

#[test]
fn test_no_role_at_all_is_none() {
    assert_eq!(role_token(&highest_role(&[])), "none");
}

#[test]
fn test_each_level_has_its_own_token() {
    assert_eq!(role_token(&Role::Administrator), "administrator");
    assert_eq!(role_token(&Role::Manager), "manager");
    assert_eq!(role_token(&Role::River), "river");
    assert_eq!(role_token(&Role::Intern), "intern");
}
