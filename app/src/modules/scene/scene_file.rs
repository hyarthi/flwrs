//! The `.flwr` scene export file:
//! `"FLWR" | u32 LE version length | version | protobuf SceneFile`
//! (schema: `app/schema/scene_file.proto`).

use prost::Message;

#[allow(clippy::all, dead_code)] // generated code
pub(crate) mod pb {
    include!(concat!(env!("OUT_DIR"), "/flwrs.scene_file.rs"));
}

/// Marks the file type, so a wrong file fails with a clear error.
pub(crate) const MAGIC: &[u8; 4] = b"FLWR";
/// The format version written.
pub(crate) const VERSION: &str = "1.0.0";
/// The format versions this server reads.
const READABLE: &[&str] = &["1.0.0"];
/// Larger files are refused, before and after decoding.
pub(crate) const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const EXTENSION: &str = "flwr";
const MAX_VERSION_LEN: usize = 64;

pub(crate) fn encode(file: &pb::SceneFile) -> Vec<u8> {
    let body = file.encode_to_vec();
    let mut out = Vec::with_capacity(8 + VERSION.len() + body.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(VERSION.len() as u32).to_le_bytes());
    out.extend_from_slice(VERSION.as_bytes());
    out.extend_from_slice(&body);
    out
}

pub(crate) fn decode(bytes: &[u8]) -> Result<pb::SceneFile, String> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err(format!("file is larger than {MAX_FILE_BYTES} bytes"));
    }
    let rest = bytes
        .strip_prefix(MAGIC.as_slice())
        .ok_or("not a flwrs scene file (.flwr)")?;
    let (len, rest) = rest
        .split_first_chunk::<4>()
        .ok_or("truncated file header")?;
    let len = u32::from_le_bytes(*len) as usize;
    if len > MAX_VERSION_LEN || len > rest.len() {
        return Err("invalid file header".into());
    }
    let (version, body) = rest.split_at(len);
    let version = std::str::from_utf8(version).map_err(|_| "invalid file header")?;
    if !READABLE.contains(&version) {
        return Err(format!(
            "unsupported scene file version {version:?} (this server reads {})",
            READABLE.join(", ")
        ));
    }
    pb::SceneFile::decode(body).map_err(|e| format!("invalid scene file: {e}"))
}

/// A file name for `scene_name`: letters, digits, `.`, `_` and `-` kept,
/// anything else replaced, with the `.flwr` extension.
pub(crate) fn file_name(scene_name: &str) -> String {
    let stem: String = scene_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(100)
        .collect();
    let stem = stem.trim_matches(|c| c == '_' || c == '.');
    let stem = if stem.is_empty() { "scene" } else { stem };
    format!("{stem}.{EXTENSION}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> pb::SceneFile {
        pb::SceneFile {
            name: "demo".into(),
            max_hops: Some(64),
            nodes: vec![pb::Node {
                key: "n1".into(),
                r#type: pb::plugin_type::Enum::Sink as i32,
                plugin_id: "http-sink".into(),
                config_json: "{}".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn round_trip_with_header() {
        let bytes = encode(&sample());
        assert_eq!(&bytes[..4], b"FLWR");
        assert_eq!(&bytes[4..8], &5u32.to_le_bytes());
        assert_eq!(&bytes[8..13], b"1.0.0");
        assert_eq!(decode(&bytes).unwrap(), sample());
    }

    #[test]
    fn rejects_bad_files() {
        let good = encode(&sample());
        let cases: Vec<(Vec<u8>, &str)> = vec![
            (b"PK\x03\x04 a zip".to_vec(), "not a flwrs scene file"),
            (b"FLWR\x05".to_vec(), "truncated"),
            (
                [b"FLWR".as_slice(), &200u32.to_le_bytes(), b"1.0.0"].concat(),
                "invalid file header",
            ),
            (
                [b"FLWR".as_slice(), &5u32.to_le_bytes(), b"9.0.0"].concat(),
                "unsupported scene file version \"9.0.0\"",
            ),
            (
                [&good[..13], &[0xff, 0xff, 0xff]].concat(),
                "invalid scene file",
            ),
            (vec![0; MAX_FILE_BYTES + 1], "larger than"),
        ];
        for (bytes, expected) in cases {
            let err = decode(&bytes).unwrap_err();
            assert!(err.contains(expected), "{expected:?} not in {err:?}");
        }
    }

    #[test]
    fn file_names_are_safe() {
        assert_eq!(file_name("My scene / v2"), "My_scene___v2.flwr");
        assert_eq!(file_name("../../etc"), "etc.flwr");
        assert_eq!(file_name("   "), "scene.flwr");
        assert_eq!(file_name("ok-name_1.2"), "ok-name_1.2.flwr");
    }
}
