//! One dependency relation, for every engine that has one.
//!
//! "A produces what B reads" is the same edge whether the producers are tools at a visit, derived
//! definitions at a site, or formulas inside one calculation, and the two questions asked of it are
//! the same too: what order do these run in, and does adding this edge close a loop. Both are
//! answered here, over indices, so a caller keeps its own names and its own message and the rule
//! itself lives in one place (I67, Q96).

/// The order in which items may run, each after every item it depends on.
///
/// `deps[i]` holds the indices `i` waits for. On success every index appears exactly once. On
/// failure the members of the cycle come back, which is what a caller names in its message:
/// dropping them and running the rest would leave their values permanently absent with nothing
/// said.
pub fn order(deps: &[Vec<usize>]) -> Result<Vec<usize>, Vec<usize>> {
    let mut done = vec![false; deps.len()];
    let mut ordered: Vec<usize> = Vec::with_capacity(deps.len());
    let mut remaining: Vec<usize> = (0..deps.len()).collect();

    while !remaining.is_empty() {
        let mut progress = false;
        remaining.retain(|&i| {
            if deps[i].iter().all(|&d| done[d]) {
                done[i] = true;
                ordered.push(i);
                progress = true;
                false
            } else {
                true
            }
        });
        if !progress {
            return Err(remaining);
        }
    }
    Ok(ordered)
}

/// The members of a cycle in this graph, or `None` when there is none. The authoring guards ask
/// this of the graph they are about to create, which is the same question [`order`] answers.
#[must_use]
pub fn cycle(deps: &[Vec<usize>]) -> Option<Vec<usize>> {
    order(deps).err()
}

#[cfg(test)]
mod tests {
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
}
