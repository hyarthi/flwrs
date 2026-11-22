# Vision source

A flwrs source plugin that runs computer-vision models on a camera or video and emits what they see as events: objects with their boxes, keypoints and classes. One node is one camera.

**Status: early.** At start, the plugin checks its model graph and loads every model on its device, then connects to the hub. The engine runs whole graphs on frames, but there's no camera input yet, so it emits no events.

## Settings

| Setting | | |
|---|---|---|
| `graph` | required | The model graph (below). |
| `files` | | The files the graph names: models, label lists, charsets. Upload them to the node and list them here; the graph refers to each by its file name, and a name it uses that isn't listed is refused. |
| `execution` | optional | Where the models run (below). A model node's own `execution` overrides it, field by field. |
| `out_schema` | optional | The events' payload schema. Default: the detections schema (`schema/out.json`). |

### Execution

| Field | |
|---|---|
| `runtime` | `ort` (ONNX Runtime) or `tract`. Default: the build's first (`ort` when it has both). |
| `device` | `cpu` (the default), `gpu`, or `npu` (a neural engine). Parts of a model a device's provider can't take run on the CPU. |
| `provider` | ONNX Runtime's execution provider for the device: `coreml`, `cuda`, `tensorrt`, `directml`, `openvino`, `webgpu`, `xnnpack`, `qnn`, or a full `…ExecutionProvider` name. Default: the first the device offers. Which exist depends on the build. |
| `provider_options` | The provider's options, e.g. `{"ModelFormat": "MLProgram"}` for CoreML. |
| `allow_fallback` | Default off: a model that won't load or run on its device stops the node from starting. On: it runs on the CPU instead, with a warning in the node's log. |

Loading a model includes one run on a zero input, so a model that compiles for a device but fails on it is caught at start. A device other than the CPU may change results: on an Apple M2, CoreML (GPU or Neural Engine) with its default model format computes in float16, and a sixth to a third of the example graphs' runs differed from the CPU's; with `"ModelFormat": "MLProgram"` the results matched, but some models didn't compile. Check a model's results before choosing another device.

## The model graph

Models are chained inside the plugin, so a frame and its crops never travel through the hub. The graph has **nodes** with typed ports, and **links** from an output port to an input port, written `<node>.<port>`. Every input is linked exactly once, and there are no cycles. Node ids are lowercase letters, digits and `_`, starting with a letter.

```json
{
  "nodes": {
    "camera": { "kind": "source" },
    "palm":   { "kind": "model", "model": "mp_palm.onnx", "pre": { … }, "decode": { "type": "ssd_anchors", … } },
    "roi":    { "kind": "crop", "rotate": { "from": 0, "to": 2, "angle": 90 }, "scale": 2.6, "shift_y": -0.5, "square": true },
    "hand":   { "kind": "model", "model": "mp_hand.onnx", "pre": { … }, "decode": { "type": "landmarks", … } }
  },
  "links": [
    { "from": "camera.frame", "to": "palm.frame" },
    { "from": "camera.frame", "to": "roi.frame" },
    { "from": "palm.objects", "to": "roi.objects" },
    { "from": "roi.regions",  "to": "hand.frame" }
  ]
}
```

`examples/graphs/` has 22 graphs: detection, pose, faces, hands, segmentation, depth, OCR and re-id models, and a gesture classifier on hand landmarks.

### Node kinds

| Kind | Inputs | Output | |
|---|---|---|---|
| `source` | none | `frame` | The camera frame. Exactly one per graph. |
| `model` | `frame` (or `objects` with `tensor_input`) | named after its decoder's result: `objects`, `classes`, `map`, `text`, `vector` or `tensor` | Pre-processing, inference, decoding. |
| `crop` | `frame`, `objects` | `regions` | A region of the frame per object: the box, or a region from keypoints (centre, size, rotation), scaled, shifted, squared or set to an aspect ratio. `classes` and `max` choose the objects. |
| `filter` | `objects` | `objects` | Objects kept by `classes`, `min_confidence` and `max`. |

### Port types

| Type | What it carries |
|---|---|
| `frame` | the frame, or a region of it |
| `regions` | regions of the frame, one per object |
| `objects` | class, score, box, keypoints, and the results of models run on them |
| `classes` | top-k classes with scores |
| `map` | a per-pixel map (segmentation, depth) and its summary |
| `text` | text and its score (OCR) |
| `vector` | an embedding |
| `tensor` | raw output tensors |

Coordinates are always frame pixels: each model input remembers how it was cut and scaled from the frame, and its results are mapped back.

A link may join two ports of the same type. One exception, **for each:** `regions` linked to a `frame` input runs that node once per region, and everything downstream of it as well (e.g. a gesture classifier on each hand's landmarks). A node can't take inputs per region of two different crop nodes, and regions can't be cropped again.

### How a frame runs

The nodes that run once per frame go first, in an order where each node follows its inputs; then each crop node's per-region nodes, once per region. **Results go with the object they're about:** a model run on a region adds its result to the object the region was cut from, under the model's node id (an object's `children`); a model on keypoints (`tensor_input`) runs once per object of its input and adds each result to its object. So a palm detector's palms carry their hand landmarks, and those carry their gesture. A crop takes the objects of its `classes`, the highest confidence first, at most `max` (8). A model that fails on a frame gives nothing for it, and the node's log says why; the others run on.

### Model nodes

| Field | |
|---|---|
| `model` | The ONNX file: the name of one of the node's `files`. |
| `input_name` | The input tensor's name (default: the model's first input). |
| `pre` | How a frame becomes the input: `width`, `height`, `resize` (`stretch`, `letterbox`, `cover`), `pad`, `color` (`rgb`, `bgr`), `dtype` (`f32`, `i32`, `u8`), `scale`, `mean`, `std` (value = (pixel × scale − mean) / std), `layout` (`nchw`, `nhwc`). |
| `tensor_input` | Or: the input built from an object's keypoints, linked from an `objects` output: a flat `[1, n × dims]` vector, `x y` (`dims` 2) or `x y confidence` (3) per keypoint (all, or the indexes in `keypoints`), relative to: nothing (`normalize: none`, frame pixels), the object's box (`box`: 0..1), or the first keypoint scaled by the farthest one (`wrist`). An object without the keypoints it needs gets no result. |
| `decode` | The decoder (below). |
| `min_confidence` | The confidence cutoff, 0 to 1: a result below it is dropped as soon as the decoder knows its confidence (before NMS and keypoints), so no node downstream (crops, models run on objects) and no event sees it. Default 0.25 for detectors (`yolo`, `detections_e2e`, `detr`, `gfl`, `ssd_anchors`, `yunet`, `movenet`, `rtmo`), 0 for the others. It applies to objects, classes (`classify`: the top-k below it are dropped), poses and landmarks (`simcc`, `heatmap`: the mean keypoint confidence; `landmarks`: the presence score), text regions (`ocr_det`) and text (`ctc`). Decoders whose results have no confidence (`semantic`, `dense_map`, `embedding`, `raw`) don't take it. |
| `labels` | A list, or `{"builtin": "coco80" \| "imagenet1k"}`, `{"file": "<name>"}`, `{"model_metadata": "names"}` (Ultralytics exports), `{"hf_config": "<name>"}` (a Hugging Face `config.json`); names are of the node's `files`. |
| `charset` | For `ctc`: `{"file": "<name>"}` or `{"model_metadata": "<key>"}`, one entry per line. |
| `sampling` | How often it runs: `{"every_n_frames": n}` (the frames in between reuse its last results; only for models that run once per frame). Default: every frame. (`per_track` needs object tracking, which isn't there yet.) |
| `execution` | Where it runs (as the plugin's `execution` setting): each field set here overrides the plugin's. |
| `keypoint_layout` | What the keypoints are (point names and skeleton): `coco17`, `blazepose33`, `hand21`, `wholebody133` (COCO-WholeBody), `face5` (YuNet: eyes, nose, mouth corners), `face6` (BlazeFace: eyes, nose, mouth, ears). Where the decoder's settings fix the count, it must match. |

### Decoders

The decoder belongs to the model's *export*, not to its architecture: the same YOLO26n exports as end-to-end rows from Ultralytics and as DETR-style queries from transformers.js. Output names are the model's own tensor names.

| `type` | Result | For |
|---|---|---|
| `classify` | `classes` | single-label classifiers (`softmax`, `topk`) |
| `yolo` | `objects` | YOLO raw outputs: v5 / v7 (`objectness`), v8 / 11 / 26 raw (`channels_first`), YOLOX (`grid`), YOLO-pose (`keypoints`); NMS |
| `detections_e2e` | `objects` | rows of `x1 y1 x2 y2 score class`: YOLOv10, YOLO26 end-to-end, exports with NMS inside |
| `detr` | `objects` | DETR family: RT-DETR, RF-DETR, D-FINE (`activation`) |
| `gfl` | `objects` | NanoDet-Plus |
| `ssd_anchors` | `objects` | MediaPipe-style SSD with generated anchors: palm, BlazePose, BlazeFace detectors |
| `yunet` | `objects` | YuNet faces |
| `movenet` | `objects` | MoveNet single / multi-pose |
| `rtmo` | `objects` | RTMO one-stage pose |
| `simcc` | `objects` | RTMPose, RTMW (per person crop) |
| `heatmap` | `objects` | HRNet, ViTPose (per person crop) |
| `landmarks` | `objects` | MediaPipe hand, pose and face landmarks |
| `semantic` | `map` | segmentation logits → class ids (SegFormer, DeepLab) |
| `dense_map` | `map` | one-channel maps: alpha mattes, depth |
| `ocr_det` | `objects` | DB text detection (PaddleOCR) |
| `ctc` | `text` | CTC text recognition (needs `charset`) |
| `embedding` | `vector` | embeddings (re-id) |
| `raw` | `tensor` | selected outputs as they are |

Decoders that run NMS take `iou` (0.45); at most 300 objects come out of one run. Whatever a model outputs (the wrong shapes, NaN, infinities), a decoder returns an error or results with confidences between 0 and 1 and finite coordinates, never a crash. Outputs of any numeric type (integers, quantised `u8` / `i8`, `f64`) are read as floats.

Built-in label lists: `coco80`, `imagenet1k`.

## Events

The default payload (`schema/out.json`), named as in [supervision](https://supervision.roboflow.com/): `event`, `frame_time`, `frame_seq`, `width`, `height`, and `objects`, each with `class_id`, `class_name`, `confidence`, `tracker_id`, `source` (the graph node), `box` (`x1`, `y1`, `x2`, `y2`), `keypoints` (`x`, `y`, `confidence`) and `classes` (classifier results on the object). Boxes and keypoints are normalised to the frame (0..1), so they don't depend on the camera's resolution. Rule events (`line_crossing`, `zone`, `loitering`, `counting`, after the ONVIF analytics rules) will use the same object shape.

## Runtimes

Cargo features choose the inference runtimes: `ort` (ONNX Runtime, the default) and `tract` (pure Rust, for devices ONNX Runtime doesn't support). At least one is needed. The `ort` build downloads ONNX Runtime's library once (it's cached); a build without network access needs it in the cache already.

- **ONNX Runtime** runs a model on the device its `execution` names, picked explicitly from ONNX Runtime's device list. A session left to ONNX Runtime's defaults puts parts of a graph on other devices (CoreML on macOS), which changed results: a YOLO26 export lost its best detection that way.
- **tract** runs on the CPU, on one thread. It compiles each model for its input's shape, symbolic dimensions (`batch_size`, `height`) made concrete. Of the example graphs, it can't run `depth_anything_s`, `rfdetr_nano` and `rtmo_s` (operators its analysis doesn't support); the others give the same results as ONNX Runtime.

## Tests

Tiny ONNX models generated by the tests check each decoder through each runtime, and whole graphs (crops, models on regions and on keypoints, filters, sampling, devices and fallback). `tests/real_models.rs` (ignored) runs the example graphs on real models and compares their results, what ran on each object included, with reference results; see the file for the folders it needs.
