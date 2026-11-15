//! Plugin packages (feature `package`): the manifest model, its validation,
//! the rules for a node's `launch_args`, and the checks on a package ZIP.
//!
//! This is the format the server installs (`POST /api/plugins`) and
//! `flwrs-trellis package` produces; both use this module, so they agree on
//! what a valid package is.

pub mod archive;
pub mod manifest;

pub use archive::{ExtraCheck, Limits, Package, PackageError, extract_and_validate, validate};
pub use manifest::{
    LaunchArgsStyle, MANIFEST_FILE, MANIFEST_VERSION, Manifest, PayloadSchemaPaths, PluginType,
    RESERVED_ARGS, check_relative_path, current_platform, is_semver, is_valid_id,
    launch_args_to_argv, validate_launch_args,
};
