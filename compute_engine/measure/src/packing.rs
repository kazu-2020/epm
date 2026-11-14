#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoordinatePacking {
    shifts: Vec<u32>,
    masks: Vec<u64>,
    width: u32,
}

impl CoordinatePacking {
    pub fn new(member_counts: &[u32]) -> Option<CoordinatePacking> {
        let widths: Vec<u32> = member_counts
            .iter()
            .map(|&member_count| u32::BITS - member_count.saturating_sub(1).leading_zeros())
            .collect();
        let width = widths.iter().sum::<u32>();
        if width > u64::BITS {
            return None;
        }
        let mut shifts = vec![0; widths.len()];
        let mut shift = 0;
        for (slot, &width) in shifts.iter_mut().zip(&widths).rev() {
            // Not 64 for a zero-width axis in front of 64 bits: a u64 shifted by 64 overflows.
            *slot = shift.min(u64::BITS - 1);
            shift += width;
        }
        Some(CoordinatePacking {
            shifts,
            masks: widths.iter().map(|&width| (1u64 << width) - 1).collect(),
            width,
        })
    }

    pub fn pack(&self, members: impl IntoIterator<Item = u32>) -> u64 {
        self.shifts.iter().zip(&self.masks).zip(members).fold(
            0,
            |packed, ((&shift, &mask), member)| {
                debug_assert!(u64::from(member) <= mask);
                packed | (u64::from(member) << shift)
            },
        )
    }

    pub fn place(&self, axis: usize, member: u32) -> u64 {
        debug_assert!(self.fits(axis, member));
        u64::from(member) << self.shifts[axis]
    }

    pub fn fits(&self, axis: usize, member: u32) -> bool {
        u64::from(member) <= self.masks[axis]
    }

    pub fn axis_mask(&self, axis: usize) -> u64 {
        self.masks[axis] << self.shifts[axis]
    }

    pub fn member(&self, packed: u64, axis: usize) -> u32 {
        ((packed >> self.shifts[axis]) & self.masks[axis]) as u32
    }

    pub(crate) fn holds(&self, packed: u64) -> bool {
        packed.checked_shr(self.width).unwrap_or(0) == 0
    }

    pub fn unpack(&self, packed: u64) -> impl Iterator<Item = u32> + '_ {
        (0..self.shifts.len()).map(move |axis| self.member(packed, axis))
    }
}
