use crate::db::DbError;
use rust_embed::EmbeddedFile;
use std::borrow::Cow;
use std::fmt::{Display, Formatter};

/// One migration script. File names follow `MAJOR.MINOR.PATCH-BUILD.<up|down>.sql`.
#[derive(Clone)]
pub struct MigrationDef {
    pub data: Cow<'static, [u8]>,
    pub file_name: String,
    pub file_hash: String,
    pub version: Version,
}

impl MigrationDef {
    pub fn new(file_name: String, file: EmbeddedFile) -> Result<Self, DbError> {
        Self::from_bytes(file_name, file.data)
    }

    pub fn from_bytes(
        file_name: String,
        data: impl Into<Cow<'static, [u8]>>,
    ) -> Result<Self, DbError> {
        let version = Version::parse_file_name(&file_name)?;
        let data = data.into();
        let file_hash = sha256::digest(data.as_ref());
        Ok(Self {
            data,
            file_name,
            file_hash,
            version,
        })
    }

    pub fn version(&self) -> Version {
        self.version
    }
}

/// Schema version. Ordering compares `major`, `minor`, `patch`, then
/// `build_number`, in that order.
#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Copy, Clone, Default)]
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

    fn parse_file_name(file_name: &str) -> Result<Self, DbError> {
        let malformed = |reason: String| {
            DbError::MigrationMalformed(format!(
                "migration file name corrupted [{file_name}]: {reason}"
            ))
        };

        let (version, rest) = file_name
            .split_once('-')
            .ok_or_else(|| malformed("no build version".into()))?;
        let parts: Vec<&str> = version.split('.').collect();
        let [major, minor, patch] = parts.as_slice() else {
            return Err(malformed("version is mangled".into()));
        };
        let build = rest.split('.').next().unwrap_or_default();

        let number = |name: &str, value: &str| {
            value
                .parse::<u32>()
                .map_err(|e| malformed(format!("{name} [{value}] is not a number: {e}")))
        };
        Ok(Self {
            major: number("major version", major)?,
            minor: number("minor version", minor)?,
            patch: number("patch version", patch)?,
            build_number: number("build number", build)?,
        })
    }
}

impl Display for Version {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}.{}.{}-{}",
            self.major, self.minor, self.patch, self.build_number
        )
    }
}

impl From<&MigrationDef> for Version {
    fn from(value: &MigrationDef) -> Self {
        value.version
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordering_is_lexicographic() {
        let v = Version::new;
        assert!(v(1, 0, 0, 0) > v(0, 9, 0, 0));
        assert!(v(0, 9, 0, 0) < v(1, 0, 0, 0));
        assert!(v(1, 2, 0, 0) > v(1, 1, 9, 9));
        assert!(v(1, 1, 1, 2) > v(1, 1, 1, 1));
        assert!(v(2, 0, 0, 0) > v(1, 9, 9, 9));
        assert_eq!(v(1, 2, 3, 4), v(1, 2, 3, 4));

        let mut versions = vec![v(1, 0, 0, 2), v(0, 9, 1, 0), v(1, 0, 0, 1), v(0, 10, 0, 0)];
        versions.sort();
        assert_eq!(
            versions,
            vec![v(0, 9, 1, 0), v(0, 10, 0, 0), v(1, 0, 0, 1), v(1, 0, 0, 2)]
        );
    }

    #[test]
    fn parses_file_names() {
        let def =
            MigrationDef::from_bytes("1.2.3-4.up.sql".into(), b"SELECT 1;".as_slice()).unwrap();
        assert_eq!(def.version(), Version::new(1, 2, 3, 4));
        assert_eq!(def.file_hash, sha256::digest("SELECT 1;"));
        assert_eq!(
            Version::parse_file_name("10.0.0-12.down.sql").unwrap(),
            Version::new(10, 0, 0, 12)
        );
    }

    #[test]
    fn rejects_malformed_file_names() {
        for name in [
            "1.0.0.up.sql",
            "1.0-1.up.sql",
            "1.x.0-1.up.sql",
            "1.0.0-b.up.sql",
            "1.0.0.0-1.up.sql",
        ] {
            assert!(
                Version::parse_file_name(name).is_err(),
                "{name} should be rejected"
            );
        }
    }
}
