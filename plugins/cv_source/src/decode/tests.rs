use super::fixtures;
use super::util::MAX_DETECTIONS;
use crate::results::{MapSummary, Results};
use crate::tensor::{Output, Outputs};
use crate::testing::Rng;

#[test]
fn every_decoder_decodes_its_fixture() {
    let all = fixtures::all();
    for f in &all {
        let results = f
            .decode(&f.outputs())
            .unwrap_or_else(|e| panic!("{}: {e}", f.name));
        assert_eq!(results.port_type(), f.decoder.result_type(), "{}", f.name);
        (f.check)(&results);
    }
    // Every decoder type has one.
    let types: std::collections::BTreeSet<String> = all
        .iter()
        .map(|f| {
            serde_json::to_value(&f.decoder).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(types.len(), 18, "{types:?}");
}

/// Below the cutoff, nothing comes out: objects, classes, poses and text.
#[test]
fn the_cutoff_drops_results_in_the_decoder() {
    let count = |r: &Results| match r {
        Results::Objects(o) => o.len(),
        Results::Classes(c) => c.len(),
        Results::Text(t) => t.iter().count(),
        _ => usize::MAX,
    };
    for mut f in fixtures::all() {
        if f.decoder.default_min_confidence().is_none() {
            continue;
        }
        let kept = count(&f.decode(&f.outputs()).unwrap());
        assert!(kept > 0 && kept != usize::MAX, "{}", f.name);
        // Every fixture's confidences are below 1.
        f.min_confidence = Some(1.0);
        assert_eq!(count(&f.decode(&f.outputs()).unwrap()), 0, "{}", f.name);
        f.min_confidence = Some(0.0);
        assert!(
            count(&f.decode(&f.outputs()).unwrap()) >= kept,
            "{}",
            f.name
        );
    }
    let by_name = |name: &str| {
        fixtures::all()
            .into_iter()
            .find(|f| f.name == name)
            .unwrap()
    };
    // Between the two detections (0.9 and 0.6): the weaker goes.
    let mut yolo = by_name("yolo");
    yolo.min_confidence = Some(0.7);
    let Results::Objects(o) = yolo.decode(&yolo.outputs()).unwrap() else {
        panic!()
    };
    assert_eq!(o.iter().map(|o| o.class_id).collect::<Vec<_>>(), [1]);
    // The top-2 classes are 0.64 and 0.24: only the first passes 0.3.
    let mut classify = by_name("classify");
    classify.min_confidence = Some(0.3);
    let Results::Classes(c) = classify.decode(&classify.outputs()).unwrap() else {
        panic!()
    };
    assert_eq!(c.iter().map(|c| c.class_id).collect::<Vec<_>>(), [1]);
}

#[test]
fn wrong_outputs_are_errors() {
    for f in fixtures::all() {
        // A missing output names what's there.
        let e = f.decode(&Outputs::new()).err();
        assert!(
            e.as_deref().is_some_and(|e| e.contains("has no output")),
            "{}: {e:?}",
            f.name
        );
    }
    let f = fixtures::all()
        .into_iter()
        .find(|f| f.name == "yolo")
        .unwrap();
    let small: Outputs = [(
        "out".to_string(),
        Output::new(vec![1, 5, 3], vec![0.0; 15]).unwrap(),
    )]
    .into();
    assert!(
        f.decode(&small)
            .unwrap_err()
            .contains("the settings need 6")
    );
    let batch: Outputs = [(
        "out".to_string(),
        Output::new(vec![2, 6, 3], vec![0.0; 36]).unwrap(),
    )]
    .into();
    assert!(f.decode(&batch).unwrap_err().contains("expected [1, C, N]"));
}

/// Results anyone downstream can rely on.
fn check_invariants(name: &str, r: &Results) {
    let unit = |v: f32| v.is_finite() && (0.0..=1.0).contains(&v);
    match r {
        Results::Objects(objects) => {
            assert!(
                objects.len() <= MAX_DETECTIONS,
                "{name}: {} objects",
                objects.len()
            );
            for o in objects {
                assert!(unit(o.confidence), "{name}: confidence {}", o.confidence);
                assert!(
                    o.bbox.iter().all(|v| v.is_finite())
                        && o.bbox[0] <= o.bbox[2]
                        && o.bbox[1] <= o.bbox[3],
                    "{name}: box {:?}",
                    o.bbox
                );
                for k in &o.keypoints {
                    assert!(
                        k.x.is_finite() && k.y.is_finite() && unit(k.confidence),
                        "{name}: keypoint {k:?}"
                    );
                }
            }
        }
        Results::Classes(classes) => assert!(classes.iter().all(|c| unit(c.confidence)), "{name}"),
        Results::Map(m) => {
            assert!(m.values.iter().all(|v| v.is_finite()), "{name}");
            if let MapSummary::Classes(c) = &m.summary {
                assert!(
                    c.iter()
                        .all(|(c, share)| unit(c.confidence) && unit(*share)),
                    "{name}"
                );
            }
        }
        Results::Text(t) => assert!(t.as_ref().is_none_or(|t| unit(t.confidence)), "{name}"),
        Results::Vector(v) => assert!(v.iter().all(|x| x.is_finite()), "{name}"),
        Results::Tensor(_) => {}
    }
}

/// Random tensors (random values with NaN and infinities; the fixture's
/// shapes or random ones) never make a decoder panic, and what it returns
/// keeps the invariants.
#[test]
fn random_outputs_never_panic() {
    let mut rng = Rng::new(0x5eed);
    for f in fixtures::all() {
        for round in 0..300 {
            let outputs: Outputs = f
                .outputs
                .iter()
                .map(|(name, shape, _)| {
                    let shape: Vec<usize> = if round % 3 == 0 {
                        (0..1 + rng.below(4)).map(|_| rng.below(9)).collect()
                    } else {
                        shape.clone()
                    };
                    let n = shape.iter().product();
                    let data = (0..n).map(|_| rng.value()).collect();
                    (name.to_string(), Output::new(shape, data).unwrap())
                })
                .collect();
            if let Ok(r) = f.decode(&outputs) {
                check_invariants(f.name, &r);
            }
        }
    }
}

/// A detector that scores everything stays within its limits.
#[test]
fn everything_detected_is_capped() {
    let f = fixtures::all()
        .into_iter()
        .find(|f| f.name == "yolo")
        .unwrap();
    let n = 20_000;
    let mut data = vec![0.0; 6 * n];
    for i in 0..n {
        let (x, y) = ((i % 200) as f32 * 10.0, (i / 200) as f32 * 10.0);
        for (ch, v) in [x, y, 4.0, 4.0, 0.9, 0.1].into_iter().enumerate() {
            data[ch * n + i] = v;
        }
    }
    let outputs: Outputs = [("out".to_string(), Output::new(vec![1, 6, n], data).unwrap())].into();
    let Results::Objects(o) = f.decode(&outputs).unwrap() else {
        panic!()
    };
    assert_eq!(o.len(), MAX_DETECTIONS);
}
