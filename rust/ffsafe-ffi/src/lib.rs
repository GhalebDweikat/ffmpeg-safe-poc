//! C entry points for the safe decoder ports.
//!
//! This crate is the only Rust code in the project that uses `unsafe`: it
//! turns the (pointer, length) pairs handed over by the C shims into
//! slices, and catches panics so they never unwind into C. Everything after
//! that runs in the `*-core` crates, which forbid `unsafe`.

#[cfg(feature = "aic")]
mod aic;
#[cfg(feature = "cinepak")]
mod cinepak;
#[cfg(feature = "flac")]
mod flac;
#[cfg(feature = "loco")]
mod loco;
#[cfg(feature = "magicyuv")]
mod magicyuv;
#[cfg(feature = "msrle")]
mod msrle;
#[cfg(feature = "msvideo1")]
mod msvideo1;
#[cfg(feature = "prores")]
mod prores;
#[cfg(feature = "qtrle")]
mod qtrle;
#[cfg(feature = "rpza")]
mod rpza;
#[cfg(feature = "targa")]
mod targa;
#[cfg(feature = "tta")]
mod tta;
#[cfg(feature = "v210")]
mod v210;
#[cfg(feature = "vble")]
mod vble;

/// A decoder panicked. This is a bug in a port, never a property of the
/// input; it is reported to C instead of unwinding.
pub const FFSAFE_PANIC: i32 = -100;

/// # Safety
/// `ptr` must be valid for reads of `len` bytes, or `len` must be 0.
pub(crate) unsafe fn bytes<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if ptr.is_null() || len == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(ptr, len)
    }
}
