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
#[path = "tests/dependency.rs"]
mod tests;
