//! Plugin packages (feature `package`): the manifest model, its validation,
//! the rules for a node's `launch_args` and how they reach the plugin
//! (`launch`), the checks on a package ZIP, and the configurator contract
//! (`configure`).
//!
//! This is the format the server installs (`POST /api/plugins`) and
//! `flwrs-trellis package` produces; both use this module, so they agree on
//! what a valid package is.

pub mod archive;
pub mod configure;
pub mod launch;
pub mod manifest;

pub use archive::{ExtraCheck, Limits, Package, PackageError, extract_and_validate, validate};
pub use launch::{
    CONFIG_DIR, CodeField, ConfigContent, ConfigFile, LaunchField, LaunchPlan, LaunchSpec,
    ListenField, SchemaReport, Via, off_platform_values, validate_launch_args,
};
pub use manifest::{
    Configurator, FromLaunchArgs, LaunchArgsStyle, MANIFEST_FILE, MANIFEST_VERSION, Manifest,
    PayloadSchemas, PluginType, RESERVED_ARGS, RESERVED_ENTRY, SchemaSource, Threads, ThreadsVia,
    Ui, check_relative_path, current_platform, is_semver, is_valid_id, launch_args_to_argv,
    parse_pointer,
};
