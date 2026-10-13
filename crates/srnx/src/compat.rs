//! Stand-ins for standard-library helpers stabilized after the crate's MSRV.

/// Equivalent of `<[T]>::split_first_chunk` (stable in Rust 1.77).
pub(crate) fn split_first_chunk<const N: usize>(s: &[u8]) -> Option<(&[u8; N], &[u8])> {
    if s.len() < N {
        return None;
    }
    let (head, tail) = s.split_at(N);
    Some((head.try_into().ok()?, tail))
}

/// Equivalent of `u64::cast_signed` (stable in Rust 1.87).
#[allow(clippy::cast_possible_wrap)]
pub(crate) const fn cast_signed(v: u64) -> i64 {
    v as i64
}

/// Equivalent of `i64::cast_unsigned` (stable in Rust 1.87).
#[cfg(test)]
#[allow(clippy::cast_sign_loss)]
pub(crate) const fn cast_unsigned(v: i64) -> u64 {
    v as u64
}
