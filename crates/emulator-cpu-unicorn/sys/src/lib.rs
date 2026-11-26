//! Links the static Unicorn archive that the build script compiles from
//! `../vendor/unicorn`. The C bindings live in `fujimod-emulator-cpu-unicorn`.
//!
//! The archive build has no features, so every feature set, test and binary of
//! the CPU crate shares one Unicorn compilation per profile and target.
#![no_std]

// GCC's AArch64 soft-MMU helpers use 128-bit host atomics on Linux x86-64.
// Keep their runtime with the static archive for every native consumer.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[link(name = "atomic")]
unsafe extern "C" {}
