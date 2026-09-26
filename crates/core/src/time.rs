//! Time, in two types that cannot be mixed up: [`Timestamp`], the recording clock (ms since the
//! Unix epoch, as rrweb stamps events), and [`Millis`], a span of time. Trace times are spans from
//! the recording's first event. Subtracting timestamps gives a span; there is no other way across.

use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    iter::Sum,
    ops::{Add, AddAssign, Neg, Sub},
};

/// A span of time in milliseconds. Fractional: browsers report sub-millisecond timings.
#[derive(Clone, Copy, Debug, Default, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Millis(pub f64);

/// A moment on the recording clock, in ms since the Unix epoch.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(pub f64);

impl Millis {
    pub const ZERO: Self = Self(0.0);
    /// Before any time: an unbounded lower limit.
    pub const NEG_INFINITY: Self = Self(f64::NEG_INFINITY);

    pub fn min(self, other: Self) -> Self {
        Self(self.0.min(other.0))
    }

    pub fn max(self, other: Self) -> Self {
        Self(self.0.max(other.0))
    }

    pub fn is_finite(self) -> bool {
        self.0.is_finite()
    }
}

impl Timestamp {
    /// Earlier than any recorded moment.
    pub const NEG_INFINITY: Self = Self(f64::NEG_INFINITY);

    pub fn min(self, other: Self) -> Self {
        Self(self.0.min(other.0))
    }

    pub fn max(self, other: Self) -> Self {
        Self(self.0.max(other.0))
    }
}

impl Add for Millis {
    type Output = Self;
    fn add(self, other: Self) -> Self {
        Self(self.0 + other.0)
    }
}

impl AddAssign for Millis {
    fn add_assign(&mut self, other: Self) {
        self.0 += other.0;
    }
}

impl Sub for Millis {
    type Output = Self;
    fn sub(self, other: Self) -> Self {
        Self(self.0 - other.0)
    }
}

impl Neg for Millis {
    type Output = Self;
    fn neg(self) -> Self {
        Self(-self.0)
    }
}

impl Sum for Millis {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::ZERO, Add::add)
    }
}

impl Sub for Timestamp {
    type Output = Millis;
    fn sub(self, other: Self) -> Millis {
        Millis(self.0 - other.0)
    }
}

impl Add<Millis> for Timestamp {
    type Output = Self;
    fn add(self, span: Millis) -> Self {
        Self(self.0 + span.0)
    }
}

impl Sub<Millis> for Timestamp {
    type Output = Self;
    fn sub(self, span: Millis) -> Self {
        Self(self.0 - span.0)
    }
}

/// Stable ascending order for sorting by time, treating `-0` and `0` as equal (and NaN as equal
/// to everything, so a corrupt time cannot panic a sort).
pub(crate) fn chronological<T: PartialOrd>(a: &T, b: &T) -> Ordering {
    a.partial_cmp(b).unwrap_or(Ordering::Equal)
}
