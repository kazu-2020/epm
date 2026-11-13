use rayon::prelude::*;

use crate::error::MeasureError;

pub(crate) fn check_sorted(packed_coordinates: &[u64]) -> Result<(), MeasureError> {
    match first_unsorted(packed_coordinates) {
        None => Ok(()),
        Some(row) if packed_coordinates[row - 1] == packed_coordinates[row] => {
            Err(MeasureError::DuplicateCoordinate(row))
        }
        Some(row) => Err(MeasureError::UnsortedCoordinate(row)),
    }
}

const CHECKED_ROWS: usize = 1 << 16;

pub(crate) fn first_row<T: Sync>(items: &[T], bad: impl Fn(&T) -> bool + Sync) -> Option<usize> {
    items
        .par_chunks(CHECKED_ROWS)
        .enumerate()
        .filter_map(|(piece, chunk)| {
            chunk
                .iter()
                .position(&bad)
                .map(|row| piece * CHECKED_ROWS + row)
        })
        .min()
}

pub(crate) fn first_unsorted(packed_coordinates: &[u64]) -> Option<usize> {
    (1..packed_coordinates.len())
        .step_by(CHECKED_ROWS)
        .collect::<Vec<usize>>()
        .into_par_iter()
        .filter_map(|start| {
            let end = (start + CHECKED_ROWS).min(packed_coordinates.len());
            packed_coordinates[start - 1..end]
                .windows(2)
                .position(|pair| pair[0] >= pair[1])
                .map(|row| start + row)
        })
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_bad_row_is_found_across_the_border_of_two_pieces() {
        let packed_coordinates: Vec<u64> = (0..=CHECKED_ROWS as u64).chain([0]).collect();

        assert_eq!(first_unsorted(&packed_coordinates), Some(CHECKED_ROWS + 1));
        let last_of_the_first_piece: Vec<u64> = (0..CHECKED_ROWS as u64).chain([0]).collect();
        assert_eq!(first_unsorted(&last_of_the_first_piece), Some(CHECKED_ROWS));
        assert_eq!(
            first_row(&packed_coordinates, |&packed| packed == CHECKED_ROWS as u64),
            Some(CHECKED_ROWS)
        );
    }

    #[test]
    fn short_inputs_have_no_bad_row_unless_one_is_there() {
        assert_eq!(first_unsorted(&[]), None);
        assert_eq!(first_unsorted(&[7]), None);
        assert_eq!(first_unsorted(&[7, 7]), Some(1));
        assert_eq!(first_row(&[] as &[u64], |_| true), None);
        assert_eq!(first_row(&[7u64], |&packed| packed == 7), Some(0));
    }

    #[test]
    fn a_bad_row_at_the_end_of_two_full_pieces_is_found() {
        let sorted: Vec<u64> = (0..2 * CHECKED_ROWS as u64).collect();
        assert_eq!(first_unsorted(&sorted), None);

        let mut last_repeated = sorted.clone();
        last_repeated[2 * CHECKED_ROWS - 1] = last_repeated[2 * CHECKED_ROWS - 2];
        assert_eq!(first_unsorted(&last_repeated), Some(2 * CHECKED_ROWS - 1));
        assert_eq!(
            first_row(&sorted, |&packed| packed + 1 == 2 * CHECKED_ROWS as u64),
            Some(2 * CHECKED_ROWS - 1)
        );
    }
}
