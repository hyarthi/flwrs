use std::fmt::{Display, Formatter};
use rust_embed::EmbeddedFile;
use std::str;
use crate::db::DbError;
#[derive(Clone)]
pub struct MigrationDef {
    pub file: EmbeddedFile,
    pub file_name: String,
    pub file_hash: String,
    pub version_major: u32,
    pub version_minor: u32,
    pub version_patch: u32,
    pub build_number: u32,
}

impl MigrationDef {
    pub fn new(file_name: String, file: EmbeddedFile) -> Result<Self, DbError> {
        let parts: Vec<&str> = file_name.split('-').collect();
        if parts.len() != 2 {
            return Err(DbError::MigrationMalformed(
                format!("migration file name corrupted [{}]: no build version", file_name).to_string()
            ));
        }
        let version_parts: Vec<&str> = parts[0].split('.').collect();
        if version_parts.len() != 3 {
            return Err(DbError::MigrationMalformed(
                format!("migration file name corrupted [{}]: version is mangled", file_name).to_string(),
            ));
        }
        let version_major = match u32::from_str_radix(version_parts[0], 10) {
            Ok(u) => u,
            Err(e) => return Err(DbError::MigrationMalformed(
                format!("migration file name corrupted [{}]: major version [{}] is not a number: {}", file_name, version_parts[0], e).to_string()
            ))
        };
        let version_minor = match u32::from_str_radix(version_parts[1], 10) {
            Ok(u) => u,
            Err(e) => return Err(DbError::MigrationMalformed(
                format!("migration file name corrupted [{}]: minor version [{}] is not a number: {}", file_name, version_parts[1], e).to_string()
            ))
        };
        let version_patch = match u32::from_str_radix(version_parts[2], 10) {
            Ok(u) => u,
            Err(e) => return Err(DbError::MigrationMalformed(
                format!("migration file name corrupted [{}]: patch version [{}] is not a number: {}", file_name, version_parts[2], e).to_string()
            ))
        };
        let trimmed_build = parts[1].split('.').next().unwrap();
        let build_number = match u32::from_str_radix(trimmed_build, 10) {
            Ok(u) => u,
            Err(e) => return Err(DbError::MigrationMalformed(
                format!("migration file name corrupted [{}]: build number [{}] is not a number: {}", file_name, trimmed_build, e).to_string()
            ))
        };
        let file_hash = sha256::digest(file.data.as_ref());

        Ok(Self {
            file,
            version_major,
            version_minor,
            version_patch,
            build_number,
            file_name,
            file_hash,
        })
    }

    pub fn version(&self) -> Version {
        Version {
            major: self.version_major,
            minor: self.version_minor,
            patch: self.version_patch,
            build_number: self.build_number,
        }
    }
}

#[derive(Eq, PartialEq, Copy, Clone)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub build_number: u32,
}

impl Version {
    pub fn new(major: u32, minor: u32, patch: u32, build_number: u32) -> Self {
        Self {
            major,
            minor,
            patch,
            build_number,
        }
    }
}

impl Display for Version {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}-{}", self.major, self.minor, self.patch, self.build_number)
    }
}

impl From<&MigrationDef> for Version {
    fn from(value: &MigrationDef) -> Self {
        Self {
            major: value.version_major,
            minor: value.version_minor,
            patch: value.version_patch,
            build_number: value.build_number,
        }
    }
}

impl Version {
    pub fn is_after(&self, other: &Version) -> bool {
        if self.major > other.major {
            true
        } else if self.minor > other.minor {
            true
        } else if self.patch > other.patch {
            true
        } else if self.build_number > other.build_number {
            true
        } else {
            false
        }
    }

    pub fn is_before(&self, other: &Version) -> bool {
        if self.major < other.major {
            true
        } else if self.minor < other.minor {
            true
        } else if self.patch < other.patch {
            true
        } else if self.build_number < other.build_number {
            true
        } else {
            false
        }
    }
}