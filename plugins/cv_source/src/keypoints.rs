//! Keypoint layouts: what a model's keypoints are, by index, and which
//! pairs a skeleton joins.

/// A layout: point names (by index) and skeleton edges.
#[derive(Debug, PartialEq, Eq)]
pub struct KeypointLayout {
    pub name: &'static str,
    pub points: Vec<String>,
    pub skeleton: Vec<(usize, usize)>,
}

/// The layouts, by name: `coco17` (COCO body), `blazepose33` (MediaPipe
/// pose), `hand21` (MediaPipe and COCO-WholeBody hands), `wholebody133`
/// (COCO-WholeBody: body, feet, 68 face points, both hands), `face5` (YuNet,
/// SCRFD: eyes, nose, mouth corners), `face6` (BlazeFace: eyes, nose, mouth,
/// ears).
pub const NAMES: &[&str] = &[
    "coco17",
    "blazepose33",
    "hand21",
    "wholebody133",
    "face5",
    "face6",
];

const COCO17: [&str; 17] = [
    "nose",
    "left_eye",
    "right_eye",
    "left_ear",
    "right_ear",
    "left_shoulder",
    "right_shoulder",
    "left_elbow",
    "right_elbow",
    "left_wrist",
    "right_wrist",
    "left_hip",
    "right_hip",
    "left_knee",
    "right_knee",
    "left_ankle",
    "right_ankle",
];

const COCO17_SKELETON: [(usize, usize); 19] = [
    (15, 13),
    (13, 11),
    (16, 14),
    (14, 12),
    (11, 12),
    (5, 11),
    (6, 12),
    (5, 6),
    (5, 7),
    (6, 8),
    (7, 9),
    (8, 10),
    (1, 2),
    (0, 1),
    (0, 2),
    (1, 3),
    (2, 4),
    (3, 5),
    (4, 6),
];

const BLAZEPOSE33: [&str; 33] = [
    "nose",
    "left_eye_inner",
    "left_eye",
    "left_eye_outer",
    "right_eye_inner",
    "right_eye",
    "right_eye_outer",
    "left_ear",
    "right_ear",
    "mouth_left",
    "mouth_right",
    "left_shoulder",
    "right_shoulder",
    "left_elbow",
    "right_elbow",
    "left_wrist",
    "right_wrist",
    "left_pinky",
    "right_pinky",
    "left_index",
    "right_index",
    "left_thumb",
    "right_thumb",
    "left_hip",
    "right_hip",
    "left_knee",
    "right_knee",
    "left_ankle",
    "right_ankle",
    "left_heel",
    "right_heel",
    "left_foot_index",
    "right_foot_index",
];

const BLAZEPOSE33_SKELETON: [(usize, usize); 35] = [
    (0, 1),
    (1, 2),
    (2, 3),
    (3, 7),
    (0, 4),
    (4, 5),
    (5, 6),
    (6, 8),
    (9, 10),
    (11, 12),
    (11, 13),
    (13, 15),
    (15, 17),
    (15, 19),
    (15, 21),
    (17, 19),
    (12, 14),
    (14, 16),
    (16, 18),
    (16, 20),
    (16, 22),
    (18, 20),
    (11, 23),
    (12, 24),
    (23, 24),
    (23, 25),
    (24, 26),
    (25, 27),
    (26, 28),
    (27, 29),
    (28, 30),
    (29, 31),
    (30, 32),
    (27, 31),
    (28, 32),
];

const HAND21: [&str; 21] = [
    "wrist",
    "thumb_cmc",
    "thumb_mcp",
    "thumb_ip",
    "thumb_tip",
    "index_finger_mcp",
    "index_finger_pip",
    "index_finger_dip",
    "index_finger_tip",
    "middle_finger_mcp",
    "middle_finger_pip",
    "middle_finger_dip",
    "middle_finger_tip",
    "ring_finger_mcp",
    "ring_finger_pip",
    "ring_finger_dip",
    "ring_finger_tip",
    "pinky_mcp",
    "pinky_pip",
    "pinky_dip",
    "pinky_tip",
];

const HAND21_SKELETON: [(usize, usize); 21] = [
    (0, 1),
    (1, 2),
    (2, 3),
    (3, 4),
    (0, 5),
    (5, 6),
    (6, 7),
    (7, 8),
    (5, 9),
    (9, 10),
    (10, 11),
    (11, 12),
    (9, 13),
    (13, 14),
    (14, 15),
    (15, 16),
    (13, 17),
    (0, 17),
    (17, 18),
    (18, 19),
    (19, 20),
];

const FEET6: [&str; 6] = [
    "left_big_toe",
    "left_small_toe",
    "left_heel",
    "right_big_toe",
    "right_small_toe",
    "right_heel",
];

const FACE5: [&str; 5] = [
    "right_eye",
    "left_eye",
    "nose_tip",
    "mouth_right",
    "mouth_left",
];

const FACE6: [&str; 6] = [
    "right_eye",
    "left_eye",
    "nose_tip",
    "mouth_center",
    "right_ear_tragion",
    "left_ear_tragion",
];

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

/// A layout by name.
pub fn layout(name: &str) -> Option<KeypointLayout> {
    let (points, skeleton) = match name {
        "coco17" => (names(&COCO17), COCO17_SKELETON.to_vec()),
        "blazepose33" => (names(&BLAZEPOSE33), BLAZEPOSE33_SKELETON.to_vec()),
        "hand21" => (names(&HAND21), HAND21_SKELETON.to_vec()),
        "wholebody133" => {
            let mut points = names(&COCO17);
            points.extend(names(&FEET6));
            points.extend((0..68).map(|i| format!("face_{i}")));
            for side in ["left", "right"] {
                points.extend(HAND21.iter().map(|n| format!("{side}_hand_{n}")));
            }
            let mut skeleton = COCO17_SKELETON.to_vec();
            // Ankles to the feet.
            skeleton.extend([(15, 17), (15, 18), (15, 19), (16, 20), (16, 21), (16, 22)]);
            // Each hand, joined at its wrist to the body's.
            for (start, wrist) in [(91, 9), (112, 10)] {
                skeleton.push((wrist, start));
                skeleton.extend(HAND21_SKELETON.iter().map(|(a, b)| (a + start, b + start)));
            }
            (points, skeleton)
        }
        "face5" => (names(&FACE5), vec![]),
        "face6" => (names(&FACE6), vec![]),
        _ => return None,
    };
    let name = NAMES.iter().find(|n| **n == name)?;
    Some(KeypointLayout {
        name,
        points,
        skeleton,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_layout_is_consistent() {
        for name in NAMES {
            let l = layout(name).unwrap();
            let n = l.points.len();
            assert!(
                l.skeleton.iter().all(|&(a, b)| a < n && b < n && a != b),
                "{name}"
            );
            let unique: std::collections::BTreeSet<_> = l.points.iter().collect();
            assert_eq!(unique.len(), n, "{name}: names are unique");
        }
        let sizes: Vec<usize> = NAMES
            .iter()
            .map(|n| layout(n).unwrap().points.len())
            .collect();
        assert_eq!(sizes, [17, 33, 21, 133, 5, 6]);
        let wb = layout("wholebody133").unwrap();
        assert_eq!(
            (wb.points[91].as_str(), wb.points[132].as_str()),
            ("left_hand_wrist", "right_hand_pinky_tip")
        );
        assert!(layout("coco18").is_none());
    }
}
