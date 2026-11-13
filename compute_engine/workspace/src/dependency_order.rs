use std::collections::{BTreeMap, BTreeSet};

use measure::MeasureGlobalId;

// Not the order of the declaration: a formula that reads a later formula would calculate from its old cells.
pub(crate) fn dependency_order(
    sources: &BTreeMap<MeasureGlobalId, BTreeSet<MeasureGlobalId>>,
) -> Result<Vec<MeasureGlobalId>, Vec<MeasureGlobalId>> {
    let mut waiting: BTreeMap<MeasureGlobalId, BTreeSet<MeasureGlobalId>> = sources
        .iter()
        .map(|(&result, read)| {
            let formulas = read
                .iter()
                .copied()
                .filter(|source| sources.contains_key(source))
                .collect();
            (result, formulas)
        })
        .collect();
    let mut order = Vec::with_capacity(waiting.len());
    // shortcut: quadratic in the number of formulas, count the open sources of each formula when a workspace has tens of thousands.
    // Not any ready formula: the smallest ID first gives the same order for every order of the declaration.
    while let Some(ready) = waiting
        .iter()
        .find(|(_, formulas)| formulas.is_empty())
        .map(|(&result, _)| result)
    {
        waiting.remove(&ready);
        for formulas in waiting.values_mut() {
            formulas.remove(&ready);
        }
        order.push(ready);
    }
    if waiting.is_empty() {
        Ok(order)
    } else {
        Err(waiting.into_keys().collect())
    }
}

#[cfg(test)]
mod tests {
    use ulid::Ulid;

    use super::*;

    fn order(sources: &[(u128, &[u128])]) -> Result<Vec<u128>, Vec<u128>> {
        let id = |id: u128| MeasureGlobalId(Ulid(id));
        let sources = sources
            .iter()
            .map(|&(result, read)| (id(result), read.iter().copied().map(id).collect()))
            .collect();
        let plain = |ids: Vec<MeasureGlobalId>| ids.into_iter().map(|id| id.0.0).collect();
        dependency_order(&sources).map(plain).map_err(plain)
    }

    #[test]
    fn a_formula_comes_after_every_formula_that_it_reads() {
        assert_eq!(order(&[(1, &[3]), (2, &[1]), (3, &[])]), Ok(vec![3, 1, 2]));
    }

    #[test]
    fn formulas_that_do_not_read_each_other_come_in_the_order_of_their_ids() {
        assert_eq!(order(&[(3, &[9]), (1, &[9]), (2, &[])]), Ok(vec![1, 2, 3]));
    }

    #[test]
    fn a_measure_that_is_not_a_formula_holds_no_formula_back() {
        assert_eq!(order(&[(2, &[7]), (1, &[7])]), Ok(vec![1, 2]));
    }

    #[test]
    fn the_formulas_of_a_cycle_and_the_formulas_behind_it_are_the_error() {
        assert_eq!(
            order(&[(1, &[2]), (2, &[1]), (3, &[2]), (4, &[])]),
            Err(vec![1, 2, 3])
        );
    }

    #[test]
    fn a_formula_that_reads_itself_is_a_cycle() {
        assert_eq!(order(&[(1, &[1])]), Err(vec![1]));
    }
}
