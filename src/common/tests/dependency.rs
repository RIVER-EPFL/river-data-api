use super::{cycle, order};

#[test]
fn a_chain_runs_producers_first() {
    // 0 waits on 1, 1 waits on 2.
    let deps = vec![vec![1], vec![2], vec![]];
    assert_eq!(order(&deps).unwrap(), vec![2, 1, 0]);
}

#[test]
fn a_diamond_is_not_a_cycle_and_has_a_depth_no_rule_caps() {
    // 3 → 1 and 3 → 2, both → 0. Four deep is as orderable as two.
    let deps = vec![vec![], vec![0], vec![0], vec![1, 2]];
    let ordered = order(&deps).unwrap();
    assert_eq!(ordered.len(), 4);
    assert!(ordered.iter().position(|&i| i == 0) < ordered.iter().position(|&i| i == 3));
    assert!(cycle(&deps).is_none());
}

#[test]
fn a_self_edge_is_a_cycle_naming_itself() {
    assert_eq!(cycle(&[vec![0]]).unwrap(), vec![0]);
}

#[test]
fn a_two_item_loop_names_both_and_leaves_the_rest_out_of_it() {
    // 0 and 1 feed each other; 2 stands alone and is not a member.
    let deps = vec![vec![1], vec![0], vec![]];
    let members = cycle(&deps).unwrap();
    assert_eq!(members, vec![0, 1]);
}

#[test]
fn nothing_orders_to_nothing() {
    assert!(order(&[]).unwrap().is_empty());
    assert!(cycle(&[]).is_none());
}
