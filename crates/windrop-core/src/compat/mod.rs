//! Windows compatibility: reading executables and deciding how to run them.
//!
//! * [`pe`] reads PE headers and import tables.
//! * [`profile`] models compatibility recipes and builds fallback chains.
//! * [`engine`] resolves a dropped file to a profile.

pub mod engine;
pub mod pe;
pub mod profile;
pub mod seed;

pub use engine::{describe_requirements, CompatibilityEngine, InputKind, ResolvedProfile};
pub use pe::{inspect, Arch, ImportedLibrary, PeInspection};
pub use profile::{
    generic_profile, AppProfile, DependencySpec, GraphicsApi, ProfileSource, Requirements,
    RuntimeEnv, WindowsVersion,
};
pub use seed::{bundled_count, bundled_profiles, SeedReport};
