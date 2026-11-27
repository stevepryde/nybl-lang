//! Tiny `f64` math facade that works on both the default (std)
//! build and the `no_std` opt-in. `core::f64` doesn't expose
//! `sqrt` / `sin` / `cos` / etc. — those methods live in
//! `std::f64`'s platform-dependent math library — so the no_std
//! build has to forward to the pure-Rust `libm` crate instead.
//! The std build stays dep-free and calls the native `f64`
//! methods directly.
//!
//! Every nybl math builtin and `ops::div` / `ops::rem`'s float
//! path goes through here rather than calling `x.sqrt()` /
//! `libm::sqrt(x)` at the point of use, so the rest of the
//! codebase stays `#[cfg]`-free.
//!
//! ## Feature
//!
//! The `std` feature uses `std::f64`'s native math. Enable
//! `no_std` with `default-features = false` when targeting
//! bare-metal / embedded / edge wasm; that pulls in the tiny
//! pure-Rust `libm` crate. If Cargo unifies both features, the
//! std implementation wins.

#![allow(dead_code)]

/// `√x`.
#[inline]
pub fn sqrt(x: f64) -> f64 {
    #[cfg(any(feature = "std", not(feature = "no_std")))]
    {
        x.sqrt()
    }
    #[cfg(all(feature = "no_std", not(feature = "std")))]
    {
        libm::sqrt(x)
    }
}

/// `sin(x)` (radians).
#[inline]
pub fn sin(x: f64) -> f64 {
    #[cfg(any(feature = "std", not(feature = "no_std")))]
    {
        x.sin()
    }
    #[cfg(all(feature = "no_std", not(feature = "std")))]
    {
        libm::sin(x)
    }
}

/// `cos(x)` (radians).
#[inline]
pub fn cos(x: f64) -> f64 {
    #[cfg(any(feature = "std", not(feature = "no_std")))]
    {
        x.cos()
    }
    #[cfg(all(feature = "no_std", not(feature = "std")))]
    {
        libm::cos(x)
    }
}

/// `tan(x)` (radians).
#[inline]
pub fn tan(x: f64) -> f64 {
    #[cfg(any(feature = "std", not(feature = "no_std")))]
    {
        x.tan()
    }
    #[cfg(all(feature = "no_std", not(feature = "std")))]
    {
        libm::tan(x)
    }
}

/// `asin(x)` (radians, in `[-π/2, π/2]`, `NaN` for `|x| > 1`).
#[inline]
pub fn asin(x: f64) -> f64 {
    #[cfg(any(feature = "std", not(feature = "no_std")))]
    {
        x.asin()
    }
    #[cfg(all(feature = "no_std", not(feature = "std")))]
    {
        libm::asin(x)
    }
}

/// `acos(x)` (radians, in `[0, π]`, `NaN` for `|x| > 1`).
#[inline]
pub fn acos(x: f64) -> f64 {
    #[cfg(any(feature = "std", not(feature = "no_std")))]
    {
        x.acos()
    }
    #[cfg(all(feature = "no_std", not(feature = "std")))]
    {
        libm::acos(x)
    }
}

/// `atan(x)` (radians, in `[-π/2, π/2]`).
#[inline]
pub fn atan(x: f64) -> f64 {
    #[cfg(any(feature = "std", not(feature = "no_std")))]
    {
        x.atan()
    }
    #[cfg(all(feature = "no_std", not(feature = "std")))]
    {
        libm::atan(x)
    }
}

/// Four-quadrant arctangent `atan2(y, x)` (radians, in `[-π, π]`).
#[inline]
pub fn atan2(y: f64, x: f64) -> f64 {
    #[cfg(any(feature = "std", not(feature = "no_std")))]
    {
        y.atan2(x)
    }
    #[cfg(all(feature = "no_std", not(feature = "std")))]
    {
        libm::atan2(y, x)
    }
}

/// Euclidean hypotenuse `√(x² + y²)` without undue overflow or underflow.
#[inline]
pub fn hypot(x: f64, y: f64) -> f64 {
    #[cfg(any(feature = "std", not(feature = "no_std")))]
    {
        x.hypot(y)
    }
    #[cfg(all(feature = "no_std", not(feature = "std")))]
    {
        libm::hypot(x, y)
    }
}

/// `⌊x⌋` — largest integer ≤ x, as a float.
#[inline]
pub fn floor(x: f64) -> f64 {
    #[cfg(any(feature = "std", not(feature = "no_std")))]
    {
        x.floor()
    }
    #[cfg(all(feature = "no_std", not(feature = "std")))]
    {
        libm::floor(x)
    }
}

/// `⌈x⌉` — smallest integer ≥ x, as a float.
#[inline]
pub fn ceil(x: f64) -> f64 {
    #[cfg(any(feature = "std", not(feature = "no_std")))]
    {
        x.ceil()
    }
    #[cfg(all(feature = "no_std", not(feature = "std")))]
    {
        libm::ceil(x)
    }
}

/// Round half-away-from-zero (matches `f64::round`).
#[inline]
pub fn round(x: f64) -> f64 {
    #[cfg(any(feature = "std", not(feature = "no_std")))]
    {
        x.round()
    }
    #[cfg(all(feature = "no_std", not(feature = "std")))]
    {
        libm::round(x)
    }
}

/// Truncate toward zero — drop the fractional part.
#[inline]
pub fn trunc(x: f64) -> f64 {
    #[cfg(any(feature = "std", not(feature = "no_std")))]
    {
        x.trunc()
    }
    #[cfg(all(feature = "no_std", not(feature = "std")))]
    {
        libm::trunc(x)
    }
}

/// `base ** exp` for floats.
#[inline]
pub fn powf(base: f64, exp: f64) -> f64 {
    #[cfg(any(feature = "std", not(feature = "no_std")))]
    {
        base.powf(exp)
    }
    #[cfg(all(feature = "no_std", not(feature = "std")))]
    {
        libm::pow(base, exp)
    }
}

/// Natural log.
#[inline]
pub fn ln(x: f64) -> f64 {
    #[cfg(any(feature = "std", not(feature = "no_std")))]
    {
        x.ln()
    }
    #[cfg(all(feature = "no_std", not(feature = "std")))]
    {
        libm::log(x)
    }
}

/// `e^x`.
#[inline]
pub fn exp(x: f64) -> f64 {
    #[cfg(any(feature = "std", not(feature = "no_std")))]
    {
        x.exp()
    }
    #[cfg(all(feature = "no_std", not(feature = "std")))]
    {
        libm::exp(x)
    }
}
