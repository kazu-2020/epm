use std::iter;
use std::ops::Range;

use arrow_buffer::ScalarBuffer;
use rayon::prelude::*;

use super::PARALLEL_ROWS;

pub(super) fn pieces(len: usize) -> Vec<Range<usize>> {
    (0..len)
        .step_by(PARALLEL_ROWS)
        .map(|start| start..(start + PARALLEL_ROWS).min(len))
        .collect()
}

pub(super) fn gather<T: Copy + Send + Sync>(rows: &[usize], from: &[T]) -> Vec<T> {
    rows.par_iter()
        .with_min_len(PARALLEL_ROWS)
        .map(|&row| from[row])
        .collect()
}

pub(super) fn copy_rows<Rows: Iterator<Item = usize>>(
    from_packed: &[u64],
    from_values: &[f64],
    counts: &[usize],
    piece_rows: impl Fn(usize) -> Rows + Sync,
) -> (Vec<u64>, Vec<f64>) {
    let total = counts.iter().sum();
    let mut packed_coordinates = vec![0; total];
    let mut values = vec![0.0; total];
    split_by_counts(&mut packed_coordinates, counts)
        .into_par_iter()
        .zip(split_by_counts(&mut values, counts))
        .enumerate()
        .for_each(|(piece, (piece_packed, piece_values))| {
            let cells = piece_packed
                .iter_mut()
                .zip(piece_values)
                .zip(piece_rows(piece));
            for ((packed, value), row) in cells {
                *packed = from_packed[row];
                *value = from_values[row];
            }
        });
    (packed_coordinates, values)
}

fn split_by_counts<'b, T>(mut buffer: &'b mut [T], counts: &[usize]) -> Vec<&'b mut [T]> {
    let mut parts = Vec::with_capacity(counts.len());
    for &count in counts {
        let (part, rest) = buffer.split_at_mut(count);
        parts.push(part);
        buffer = rest;
    }
    parts
}

pub(super) fn union(left: ScalarBuffer<u64>, right: &ScalarBuffer<u64>) -> ScalarBuffer<u64> {
    if left.ptr_eq(right) {
        return left;
    }
    // Not split at `left` as given: a short `left` gives one range, and the merge runs on one thread again.
    let (longer, shorter): (&[u64], &[u64]) = if left.len() >= right.len() {
        (&left[..], &right[..])
    } else {
        (&right[..], &left[..])
    };
    let bounds: Vec<(usize, usize)> = iter::once((0, 0))
        .chain(
            (PARALLEL_ROWS..longer.len())
                .step_by(PARALLEL_ROWS)
                .map(|longer_bound| {
                    (
                        longer_bound,
                        shorter.partition_point(|&packed| packed < longer[longer_bound]),
                    )
                }),
        )
        .chain(iter::once((longer.len(), shorter.len())))
        .collect();
    let ranges: Vec<(&[u64], &[u64])> = bounds
        .windows(2)
        .map(|pair| {
            let [(longer_start, shorter_start), (longer_end, shorter_end)] = [pair[0], pair[1]];
            (
                &longer[longer_start..longer_end],
                &shorter[shorter_start..shorter_end],
            )
        })
        .collect();
    let counts: Vec<usize> = ranges
        .par_iter()
        .map(|&(longer_range, shorter_range)| {
            let mut count = 0;
            merge(longer_range, shorter_range, |_| count += 1);
            count
        })
        .collect();
    let mut packed_coordinates = vec![0; counts.iter().sum()];
    split_by_counts(&mut packed_coordinates, &counts)
        .into_par_iter()
        .zip(&ranges)
        .for_each(|(part, &(longer_range, shorter_range))| {
            let mut written = 0;
            merge(longer_range, shorter_range, |packed| {
                part[written] = packed;
                written += 1;
            });
        });
    packed_coordinates.into()
}

fn merge(left: &[u64], right: &[u64], mut emit: impl FnMut(u64)) {
    let (mut left_position, mut right_position) = (0, 0);
    while left_position < left.len() || right_position < right.len() {
        let next = match (left.get(left_position), right.get(right_position)) {
            (Some(&left_packed), Some(&right_packed)) if left_packed == right_packed => {
                left_position += 1;
                right_position += 1;
                left_packed
            }
            (Some(&left_packed), Some(&right_packed)) if left_packed < right_packed => {
                left_position += 1;
                left_packed
            }
            (Some(&left_packed), None) => {
                left_position += 1;
                left_packed
            }
            (_, Some(&right_packed)) => {
                right_position += 1;
                right_packed
            }
            (None, None) => unreachable!("the loop stops when both are done"),
        };
        emit(next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn union_across_many_ranges_equals_the_set_union() {
        let left: Vec<u64> = (5..5 + 6 * PARALLEL_ROWS as u64).step_by(2).collect();
        let right: Vec<u64> = (0..5)
            .chain((5..8 * PARALLEL_ROWS as u64).step_by(4))
            .collect();
        let expected: std::collections::BTreeSet<u64> =
            left.iter().chain(&right).copied().collect();

        let expected: Vec<u64> = expected.into_iter().collect();

        assert_eq!(
            &union(left.clone().into(), &right.clone().into())[..],
            expected
        );
        assert_eq!(&union(right.into(), &left.into())[..], expected);
    }
}
