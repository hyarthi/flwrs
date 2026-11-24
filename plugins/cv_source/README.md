# Vision source

A flwrs source plugin that runs computer-vision models on a camera or video and emits what they see as events: objects with their boxes, keypoints and classes. One node is one camera.

**Status: early.** At start, the plugin checks its model graph and loads every model on its device, then connects to the hub. It reads frames from a camera or files, runs the graph on them, follows objects across frames and evaluates rules. It sends rule events, and optionally the objects it sees, to the hub.


## Input

The `input` setting says where frames come from. Without it, the plugin loads its graph and reads nothing. A slow graph always gets the newest frame: older unread ones are dropped and counted.

```json
{ "type": "camera", "device": "FaceTime", "width": 1280, "height": 720, "fps": 30 }
```

| `type` | Settings | Notes |
|---|---|---|
| `camera` | `device`, `width`, `height`, `fps`, `format` (`auto`, `mjpeg`, `yuyv`, `uyvy`, `nv12`), `backend` (`auto`, `native`, `nokhwa`) | Any camera the OS exposes: built-in, USB, CSI through a video node, virtual. `device` is an index, a device id or path, or a unique part of the name; left out, the first camera. |
| `images` | `path`, `fps`, `loop` | A folder of JPEG / PNG files, in name order. |
| `mjpeg` | `path`, `fps`, `loop` | A file of concatenated JPEG frames. |

File inputs without `fps` deliver every frame and wait for the graph, so runs are deterministic. A camera that disappears is reopened with a backoff (1 s, doubling to 30 s).

| OS | Capture | Notes |
|---|---|---|
| macOS | AVFoundation, `nokhwa` as a fallback | `auto` falls back when the native backend can't open, gives no first frame, or killed the process during start-up (then for a day). `nokhwa` always reads YUYV. |
| Linux | V4L2 (the `v4l` crate) | Building needs libclang and the Linux headers. Raspberry Pi CSI cameras will need `libcamera`, which isn't in yet. |
| Windows | Media Foundation | Type-checked only so far. |
| Others | none yet | Use a file input. |

**macOS camera permission.** The plugin is a child of the server, so macOS asks on behalf of the app that started it (your terminal, for example). If it's denied, allow that app under System Settings → Privacy & Security → Camera and restart it.

## Tracking and rules

A `tracker` node follows objects across frames (ByteTrack-style: a Kalman filter per track, optimal matching by box overlap, weak detections keeping a partly hidden object's track alive). Objects get a `tracker_id` once seen on `min_hits` frames (default 3), and a track survives `max_lost_s` (default 1 s) without a detection. `smooth_keypoints` applies a One-Euro filter to tracked keypoints. A model run on a tracker's regions can use `"sampling": {"per_track": {"refresh_frames": 30}}` to run once per track instead of on every frame.

**Control frames.** A heavy detector doesn't have to run on every frame. With `"control": {"tracker_frames": 5}` on a tracker, the model feeding it runs on one frame in six, and the tracker carries the objects in between while the models on their crops keep running on every frame. A top-down pose model suits this: its small input is what makes it cheap, and the pose it finds on each frame moves the box for the next one (only moves it: the box's size comes from the detector, eased in over a few frames so its corrections don't show as jumps). `box_smoothing` (default 3, at least 1) sets how gently detections correct the box: higher is steadier, lower hugs the person more closely. A detector also runs early when a new track waits to be confirmed, or a pose turns weak; a track control frames keep missing ends, so a crop model can't keep a ghost alive. `background` (default on) runs the detector on its own thread, so a control frame doesn't delay the others (it needs at least 2 cores). Limit the crops with the `crop` node's `max`, since cost grows with each person. `examples/graphs/pose_tracked_nanodet.json` and `pose_tracked_yolov10n.json` are complete graphs.

`rule` nodes turn objects into events. Lines and zones are in normalised frame coordinates (0..1, y down), judged at the box's `anchor` (`bottom_center` by default).

```json
{ "type": "rule", "name": "door", "classes": ["person"], "debounce_s": 0.15, "hysteresis": 0.015,
  "rule": { "type": "line_crossing", "line": [[0.5, 0.0], [0.5, 1.0]] } }
```

| `rule.type` | Fires | Settings |
|---|---|---|
| `line_crossing` | `in` (to the right of the line's direction) / `out` | `line`, `direction` |
| `zone` | `enter` / `exit` | `zone`, `on` |
| `loitering` | `exceeded`, once per stay | `zone`, `seconds` |
| `counting` | `above` / `below` (no tracker needed) | `zone` (optional), `above`, `below` |
| `track` | `started` / `ended` | `on`, `min_age_s` |
| `label_changed` | `changed` | `from` (a model run on the object) |
| `score_above` | `above`, on the rise | `threshold`, `from`, `class` |

Every rule takes `debounce_s` (a change must hold this long), `cooldown_s` (the least time between one track's events) and `hysteresis` (the margin to change back: a distance for lines and zones, objects for counts, a score for scores). Without them, a real camera produces flickering events. `label_changed` and `score_above` can also read a whole-frame classifier (`"input": "classes"`). An object already inside a zone when it's first seen doesn't count as entering.

### Lua rules

A `lua` node's `script` is the body of `function(frame, objects, tracks, state)`, and returns nothing, an event, or a list of events (`event`, `action`, `tracker_id`, `class_name`, `label`, `previous`, `score`, `count`, `duration`, `data`). `examples/graphs/pose_hand_raised.json` is a complete one.

- `frame`: `seq`, `time` (seconds), `width`, `height`.
- `objects`: `class_id`, `class_name`, `confidence`, `tracker_id`, `box` (`x1` … `y2`), `keypoints`, `kp` (by name, with the model's `keypoint_layout`), `children` (what models run on it found, by node). In frame pixels.
- `tracks`: `id`, `class_id`, `class_name`, `box`, `vx`, `vy` (pixels per second), `age`, `lost`.
- `state`: the rule's own table across frames; `state.tracks[id]` is dropped when that track ends.
- Helpers: `held(key, condition, seconds)` (true once it's been true that long), `once(key, condition, cooldown?)` (true on the frame it becomes true), `pose.angle(a, b, c)`, `pose.dist(a, b)`, `pose.torso(object)`, `pose.visible(point, min?)`.

Durations use the stream's clock: real time for a camera, frame number / `fps` for a file (30 without `fps`), so a file gives the same events every run.

## Debug mode

`flwrs-cv-source debug` runs a node's graph with no hub and shows what it sees, using the same settings as a node, so what you tune is what you deploy. In this repository, `cargo cv-debug` runs it optimised, with its window:

```sh
cargo cv-debug --settings node.json --files models/ --camera
cargo cv-debug --node .data/nodes/<scene>/<node> --video clip.mjpeg --loop
```

`--settings` is a JSON file with the node's launch args (`graph`, optional `input` and `execution`), and `--node` reads a node folder's `config/`. `--camera [device]`, `--video <mjpeg>` and `--images <folder>` replace the node's input.

The window draws each detection as an outline (a stable colour per track), labelled with its track, class, confidence and everything the models run on it found, with its keypoints and skeleton, plus the rules' lines and zones and the latest events. Results without a box, the frame rate and each model's time and device are in a panel. Press `h` (or `?`) for the keyboard guide in the bottom-right corner: `space` pause, `n` step one frame, `b` boxes, `l` labels, `k` keypoints, `m` segmentation or depth map (when the graph has a model that produces one), `p` panel, `f` mirror the display, `z` rule lines and zones, `r` start or stop recording, `q` quit. Mirroring (off by default) only flips what's shown: the models always get the true image, so a person's left and right stay correct. While it's hidden, the corner shows how to open it.

Without a display, with `--no-window`, or in a build without the `debug-window` feature, the same facts go to the console, a block per frame. `--on-change` prints only when what's seen changes, `--json` writes JSON lines, `--no-timing` makes runs comparable, and `--max-frames <n>` stops early.

Recording saves every frame the camera delivers, without the overlay, as an MJPEG file under `.data/recordings/` (`--record-dir` to change it, `--record` to start at once). When it stops, the plugin prints the file and the command to replay it with `--video`, so the same movement can be tried under different settings.

`--compare-cpu` also runs every model on the CPU and reports where the chosen device's results differ. Use it to check a `gpu` or `npu` setting.

## Testing a Lua rule

`flwrs-cv-source rule-test` runs one Lua rule node of a graph on frames written as JSON lines, with no camera, no models and no hub, and prints the events it returns as JSON lines. The rule runs as it does in a node: the same sandbox and helpers, `state` across frames, and `held` / `once` on the frames' own clock, so a test of timing is exact.

```sh
flwrs-cv-source rule-test --graph graph.json --node my_rule --frames frames.jsonl
```

`--graph` takes the graph itself, a node's launch args (`{"graph": …}`) or an example file; `--node` can be left out when the graph has one Lua node; without `--frames` the frames come from standard input. A frame:

```json
{"time": 1.5, "width": 1280, "height": 720,
 "objects": [{"tracker_id": 1, "class_name": "person", "box": [400, 100, 700, 700],
              "kp": {"left_wrist": [520, 300, 0.9]},
              "children": {"hand": [{"box": [480, 220, 560, 300], "kp": {"index_finger_tip": [530, 230]}}],
                           "gesture": [{"class_name": "thumb_up", "confidence": 0.9}]}}]}
```

Only `time` (seconds) is required; `width` and `height` default to 1280 x 720. Keypoints are `keypoints` (a list) or `kp` (by name, using the `keypoint_layout` of the model that feeds the rule, or for a child, of that child's model node); a point is `[x, y]`, `[x, y, confidence]` or `{x, y, confidence}`, with confidence 1 when left out. A child is a list of objects (items with a `box`), a list of classes, or a string. `tracks` and `ended` (track ids) may be given; without them every object with a `tracker_id` is a track, and a track missing from a frame ends on it. The command exits with 1 if the rule failed on any frame (each failure on standard error), 2 on a problem with its input.

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

### Execution groups

A graph may list `groups`, and each model node names the ones it's in. The graph says what is computed; groups say how it runs:

```json
{
  "groups": [
    {"id": "detector", "family": "execution", "policy": {"threads": 4, "priority": 10}},
    {"id": "crops", "family": "execution", "policy": {"mode": "parallel", "threads": 2}},
    {"id": "budget", "family": "resources", "policy": {"limits": {"cpu": 6}}}
  ],
  "nodes": {
    "det": {"type": "model", "groups": ["detector", "budget"], "…": "…"},
    "pose": {"type": "model", "groups": ["crops", "budget"], "…": "…"}
  }
}
```

Only model nodes have `groups`. `placement` policies are refused here, since a node's models all run in one process on one machine. Groups of one family nest or stay apart. A model follows its **innermost** execution group, whole: an outer group doesn't fill in what an inner one leaves out. Two execution groups with exactly the same members are refused, since neither would be innermost.

| Execution key | |
|---|---|
| `device`, `provider`, `allow_fallback` | As in `execution`. The order is the model's own `execution`, then its group, then the plugin's. A group that names another device or provider doesn't inherit the plugin's provider or its options. |
| `mode` | How a for-each's items run: `sequential` (the default), `parallel` or `batched`. |
| `threads` | Cores for the group. The groups' sum must fit the node's `--threads` (a node that won't fit doesn't start). With no group, a model keeps the default split. |
| `priority` | Which models give way when a live frame runs late (below). |

`limits.cpu` of a `resources` group is a budget over the threads of the execution groups with a member inside it (each counted in full); more is refused. A model in the budget without a thread count of its own is capped to it.

**Modes never change results.** Items are put back in their order whichever finishes first, and the graph's results match a sequential run within float tolerance.

- `parallel`: the items run side by side, on `min(threads, the crop's max, 8)` workers. Each worker has **its own session of the model** with its share of the threads, so memory grows with the workers, and loading takes that much longer. A frame with a single item runs on a session with all the group's threads. This pays off when frames often have several items; on mostly single-item streams it costs memory and buys nothing.
- `batched`: a frame's items go through one run, at most 16 at a time. It needs a model whose first input dimension is dynamic, on ONNX Runtime. At start the plugin compares a batch of two with the same two inputs run alone. A model that fails this, has a **fixed batch size**, or runs on tract gets a warning and runs its items **one by one**, with the same results. If a batch fails on a frame, that batch's items run one by one too.

`mode` only matters for a for-each: a model on a crop's regions, or on objects (`tensor_input`). A model that runs once per frame uses the group's device and threads.

**`threads` left out** is measured at start, for groups that aren't `parallel`: the group's largest model (up to 16 MiB) is loaded with 1, 2 and 4 threads within what the other groups leave, timed over three runs, and the fewest threads within 10 % of the fastest win. The choice is in the log. All of this takes at most about 300 ms, and a model that's too large keeps the default. A `parallel` group without `threads` takes an even share of the cores the other groups leave.

**`priority` is the one setting that can change results**, only on live streams, and only by reusing results sampling already keeps. A frame's time budget is the gap between its capture and the previous frame's. Once a frame is past it, a model in a group **below the highest priority** that has a sampling policy may skip a run that's due and reuse its last results: `every_n_frames` for one due run in a row, `per_track` for up to `refresh_frames` frames beyond its refresh. A model without sampling, or without results to reuse, always runs, and detectors on control frames are never skipped. Runs on a file never skip, so they repeat exactly. Leave `priority` out, or give all groups the same one, and nothing is ever skipped.

## The model graph

Models are chained inside the plugin, so a frame and its crops never travel through the hub. The graph has **nodes** with typed ports, and **links** from an output port to an input port, written `<node>.<port>`. Every input is linked exactly once, and there are no cycles. Node ids are lowercase letters, digits and `_`, starting with a letter.

```json
{
  "nodes": {
    "camera": { "type": "source" },
    "palm":   { "type": "model", "model": "mp_palm.onnx", "pre": { … }, "decode": { "type": "ssd_anchors", … } },
    "roi":    { "type": "crop", "rotate": { "from": 0, "to": 2, "angle": 90 }, "scale": 2.6, "shift_y": -0.5, "square": true },
    "hand":   { "type": "model", "model": "mp_hand.onnx", "pre": { … }, "decode": { "type": "landmarks", … } }
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

### Node types

| Type | Inputs | Output | |
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
| `tensor_input` | Or: the input built from an object's keypoints, linked from an `objects` output: a flat `[1, n × dims]` vector, `x y` (`dims` 2) or `x y confidence` (3) per keypoint (all, or the indexes in `keypoints`), relative to: nothing (`normalize: none`, frame pixels), the object's box (`box`: 0..1), or the first keypoint scaled by the farthest one (`wrist`). An object without the keypoints it needs gets no result. For MediaPipe's gesture classifier, `{"embedder": "gesture_embedder.onnx"}` instead: the node first runs that embedder on a hand from a `landmarks` decoder with `world` and `handedness` set (ONNX Runtime only, since it takes three inputs). |
| `decode` | The decoder (below). |
| `min_confidence` | The confidence cutoff, 0 to 1: a result below it is dropped as soon as the decoder knows its confidence (before NMS and keypoints), so no node downstream (crops, models run on objects) and no event sees it. Default 0.25 for detectors (`yolo`, `detections_e2e`, `detr`, `gfl`, `ssd_anchors`, `yunet`, `movenet`, `rtmo`), 0 for the others. It applies to objects, classes (`classify`: the top-k below it are dropped), poses and landmarks (`simcc`, `heatmap`: the mean keypoint confidence; `landmarks`: the presence score), text regions (`ocr_det`) and text (`ctc`). Decoders whose results have no confidence (`semantic`, `dense_map`, `embedding`, `raw`) don't take it. |
| `labels` | A list, or `{"builtin": "coco80" \| "imagenet1k"}`, `{"file": "<name>"}`, `{"model_metadata": "names"}` (Ultralytics exports), `{"hf_config": "<name>"}` (a Hugging Face `config.json`); names are of the node's `files`. |
| `charset` | For `ctc`: `{"file": "<name>"}` or `{"model_metadata": "<key>"}`, one entry per line. |
| `sampling` | How often it runs: `{"every_n_frames": n}` (the frames in between reuse its last results; only for models that run once per frame). Default: every frame. (`per_track` needs object tracking, which isn't there yet.) |
| `execution` | Where it runs (as the plugin's `execution` setting): each field set here overrides its execution group's and the plugin's. |
| `groups` | The [groups](#execution-groups) it's in, by id. |
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

## The configurator

The package has a configurator (`flwrs-cv-source-config`). It runs on the server's machine.

**When a node is saved** it sets two read-only settings:

- `out_schema`: the event payload, without the fields this graph can never fill in (rule fields without a rule, `data` without a Lua rule, `keypoints` when the `emit` node turns them off).
- `model_info`: what each model file says about itself, read without loading it: inputs, outputs, class count, and either a `proposal` (`decode`, plus `pre` and `labels` for Ultralytics exports) or `candidates` (decoders whose output shape it could be). A proposal is a starting point and is never applied by itself. Also `file_bytes`, `parameters` (from the weights' dimensions), `dynamic_batch`, `flops` (multiply-adds per run, in builds with the `tract` feature, for a model of at most 16 MiB with a fixed input size), and a `recommendation` for its execution group (`mode`, `threads`, `reason`): small models (at most 0.5 GFLOP, or 3 M parameters without a count) get 2 threads, `batched` with a dynamic batch and `parallel` without; large ones `sequential` with most threads. These depend only on the file and the build, never on the machine; a group asking for more threads than this machine has cores is a warning.

A graph that's invalid is refused. Everything else is a warning, because a scene may be configured here and run on another machine: a missing file, a camera that isn't there, a model that won't load on its device, a decoder output the model doesn't have, a `pre` size that differs from the model's input, or a graph that would send nothing.

**Before a node starts** (pre-flight) the same checks are binding. A camera that isn't listed is retryable. Missing files, models that won't load and ambiguous camera names are permanent. A model that fell back to the CPU under `allow_fallback` is a warning. Models get 20 s to load here; past that they're checked when the node starts.

The check **never opens a camera**, so it can't disturb a running node. It also means it can't tell that a camera is busy: that shows up when the node starts, and the node's restart limit decides what happens next.

**Describe** (`--mode describe`) answers an editor's question about the node's uploaded files while the graph may still be unfinished: their `model_info` entries, nothing stored. The web UI uses it to prefill a model node's empty settings from a picked file.

Problems and warnings about a part of the graph start with where it is, a JSON pointer into the launch args (for example `[at /graph/nodes/det] nodes.det: input frame isn't linked`), so editors can mark the node or link.

## In the web UI

The `graph` setting is edited on a canvas in the node's Graph tab: the plugin's catalog (`schema/graph_catalog.json`, named by the setting's `x-flwrs-graph` annotation) lists the node types, their settings and ports, and which ports connect. The tab type-checks links, shows for-each, per-track and sampling badges on links, marks problems on nodes, edits execution groups with frames, and keeps a JSON tab. Node positions are layout, stored beside the graph: moving one is never a change to the scene. A graph the configurator refuses can still be saved to a draft, but not to a running scene.

A node of this plugin is **critical by default** (`runtime.is_critical_path`): if it fails, its scene stops. Turn it off in the node's run-time settings to keep the rest of the scene running without the camera.

## Events

What leaves the plugin is set by an optional `emit` node (at most one per graph):

| `mode` | Sends |
|---|---|
| `rules` (default, also without the node) | Rule events only. |
| `changes` | Rule events, and the objects when who's there or a label changes. Link the objects to its `objects` input. |
| `frames` | Rule events, and the objects at most every `min_interval_ms` (default 200). Link the objects to its `objects` input. |

`max_objects` (default 32) caps a frame's objects, keeping the most confident, and `keypoints` (default on) can leave keypoints out.

The default payload (`schema/out.json`), named as in [supervision](https://supervision.roboflow.com/): `event` (`detections`, or a rule's type), `frame_time`, `frame_seq`, `width`, `height`, and `objects`, each with `class_id`, `class_name`, `confidence`, `tracker_id` (0 when untracked), `source` (the graph node), `box` (`x1`, `y1`, `x2`, `y2`), `keypoints` (`x`, `y`, `confidence`; from a model run on the object's crop too) and `classes` (classifier results on the object). Boxes and keypoints are normalised to the frame (0..1), so they don't depend on the camera's resolution. A rule event also has `rule`, `action`, `tracker_id`, `label`, `previous`, `score`, `count`, `duration_s` and `data` (a Lua rule's values, as JSON text), and its `objects` hold the object it's about. Rule types follow the ONVIF analytics rules where they exist (`line_crossing`, `zone`, `loitering`, `counting`).

The frame loop never waits for the hub. Rule events queue (up to 256; the oldest are dropped beyond that, and counted) and go first, each sent until the hub confirms it, so they survive a reconnect. Frame results replace each other, so a slow workflow gets the newest.

## Live view

The `live` setting lets a node show what it's doing while someone watches it, through the protocol's live feed. Both parts are off by default and switch on independently:

```json
{ "stats": true, "preview": { "enabled": true, "fps": 5, "width": 640 } }
```

| Setting | Default | Range | What it does |
|---|---|---|---|
| `stats` | `false` | | The `stats` channel (`application/json`), about once a second |
| `preview.enabled` | `false` | | The `preview` channel (`image/jpeg`): the frame with boxes, labels, skeletons, map overlays and rule shapes |
| `preview.fps` | `5` | 0.5 to 30 | At most this many preview frames per second |
| `preview.width` | `640` | 160 to 1920 | Width in pixels; a smaller frame isn't enlarged |

With neither on, the plugin doesn't offer the capability. With a channel on but nobody watching it, the frame loop only asks whether it's watched: nothing is counted, drawn, encoded or serialized. Changing the setting restarts the node, like any launch argument.

Drawing and JPEG encoding run on their own thread. It takes one frame at a time and a newer frame replaces a waiting one, so a slow encoder can't delay inference or events. Preview frames stay in memory: they're never written to disk, logs, events or telemetry. A frame is kept under 2 MiB by lowering its JPEG quality, and left out if it still doesn't fit.

A stats message:

```json
{
  "version": 1,
  "interval_s": 1.0,
  "frames": {
    "captured_fps": 30.0,
    "processed_fps": 28.9,
    "dropped": 1,
    "errors": 0,
    "process_ms": { "mean": 13.2, "p95": 21.0 },
    "latency_ms": { "mean": 15.1, "p95": 24.3 }
  },
  "nodes": {
    "pose": {
      "fps": 14.5,
      "runs": 29,
      "skipped": 14,
      "yielded": 2,
      "errors": 0,
      "latency_ms": { "mean": 9.8, "p95": 14.0 }
    }
  }
}
```

- `frames`: `captured_fps` is what the input delivered, `processed_fps` what the graph ran on, `dropped` the frames replaced by a newer one in the interval. `process_ms` is the graph's time on a frame, `latency_ms` the time from capture to results. Either is `null` with nothing to measure.
- `nodes` is keyed by the graph's node ids. It lists every model node, and any other node that reported an error.
- A node's `fps` counts frames it ran on, `runs` its runs (a per-region node runs once per region), and `skipped` frames it didn't run on: sampling, a late frame, or nothing to run on. `yielded` is the part of `skipped` where it gave way to a higher-priority group. `latency_ms` is its time on a frame, all its runs together, or `null` if it didn't run.

## Runtimes

Cargo features choose the inference runtimes: `ort` (ONNX Runtime, the default) and `tract` (pure Rust, for devices ONNX Runtime doesn't support). At least one is needed. The `ort` build downloads ONNX Runtime's library once (it's cached); a build without network access needs it in the cache already.

- **ONNX Runtime** runs a model on the device its `execution` names, picked explicitly from ONNX Runtime's device list. A session left to ONNX Runtime's defaults puts parts of a graph on other devices (CoreML on macOS), which changed results: a YOLO26 export lost its best detection that way.
- **tract** runs on the CPU, on one thread. It compiles each model for its input's shape, symbolic dimensions (`batch_size`, `height`) made concrete. Of the example graphs, it can't run `depth_anything_s`, `rfdetr_nano` and `rtmo_s` (operators its analysis doesn't support); the others give the same results as ONNX Runtime.

## Tests

Tiny ONNX models generated by the tests check each decoder through each runtime, and whole graphs (crops, models on regions and on keypoints, filters, sampling, devices and fallback). `tests/real_models.rs` (ignored) runs the example graphs on real models and compares their results, what ran on each object included, with reference results; see the file for the folders it needs.
