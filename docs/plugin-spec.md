# flwrs plugin specification

How a flwrs plugin is packaged, described and started: the **package** (a ZIP), its **manifest**, the **node settings** a plugin takes (its launch-args schema), and the **launch contract** (what a local plugin gets when the server starts it). It applies to plugins in any language.

What a plugin says to the hub once it runs (framing, messages, lifecycle, delivery) is the [plugin protocol](protocol.md). The two documents together are the whole contract; the Rust SDK ([`flwrs-plugin`](https://crates.io/crates/flwrs-plugin)) is one implementation of it.

The key words MUST, MUST NOT, SHOULD and MAY are used as in RFC 2119.

## 1. Packages

A plugin is installed as a ZIP with `manifest.json` at its root, the executable it names, and the files the manifest and the launch-args schema reference. Anything else (libraries, models, assets) is optional.

```
script-transform-0.1.0-linux-x86_64.zip
├── manifest.json
├── bin/flwrs-script-transform
├── schema/launch_args.json     JSON Schema of a node's settings
├── schema/out.json             payload schema(s), by plugin type
├── types/script.d.lua          definitions for a code field's editor (optional)
├── templates/script.lua        a template an editor fills an empty field with (optional)
├── ui/script.svg               icon (optional)
└── README.md                   help text (optional)
```

The server refuses a package that has:

- an absolute path, a `..` component, a Windows drive letter or a backslash in an entry name;
- a symbolic link, or anything but regular files and folders;
- two entries with the same path;
- a top-level entry named **`config`** (the name of a node's folder for the files the server provides, [section 5.3](#53-the-node-folder));
- more entries, more unpacked bytes or a higher compression ratio than the server allows (by default 10 000 entries, 1 GiB, 200:1 for entries over 1 MiB);
- a manifest that fails the rules below, or a referenced file that's missing.

The executable is made executable when the package is unpacked; other permissions aren't kept. `flwrs-trellis package` builds a package and runs these checks.

## 2. The manifest

`manifest.json`, version 2:

```json
{
  "manifest_version": 2,
  "id": "script-transform",
  "name": "Script",
  "version": "0.1.0",
  "type": "TRANSFORM",
  "description": "Runs a Lua function on each event.",
  "executable": "bin/flwrs-script-transform",
  "platforms": ["linux-x86_64"],
  "launch_args_schema": "schema/launch_args.json",
  "launch_args_style": "flags",
  "payload_schemas": {
    "in":  { "from_launch_args": "/schemas/in" },
    "out": { "from_launch_args": "/schemas/out", "default": "schema/out.json" }
  },
  "runtime_defaults": { "init_timeout_ms": 20000 },
  "ui": { "icon": "ui/script.svg", "categories": ["Transform", "Scripting"], "color": "#4f7cff", "docs": "README.md" }
}
```

| Field | Required | Meaning |
|---|---|---|
| `manifest_version` | yes | `2` (this document). Version 1 manifests are still read: they have no `ui`, no per-node payload schemas, and their launch-args schema's annotations ([section 3](#3-node-settings-the-launch-args-schema)) are ignored. |
| `id` | yes | `^[a-z0-9][a-z0-9_-]{0,63}$`. Unique on a server. |
| `name`, `version` | yes | A display name, and a semantic version (`1.2.3`, optionally `-pre` / `+build`). The plugin MUST report this version in `Initialize`. |
| `type` | yes | `SOURCE`, `SINK` or `TRANSFORM`. |
| `description` | no | Free text. |
| `launch` | no | `remote`: a **remote plugin definition** ([section 4.2](#42-remote-plugin-definitions)), a plugin that runs elsewhere and that the server only describes. Absent: the package has an executable, and its nodes may be local or remote. Version 2 only. |
| `executable` | yes, unless `launch` is `remote` | A path in the package: a native binary, or a script with a shebang. A remote plugin definition has none. |
| `platforms` | no | `<os>-<arch>` targets (`linux-x86_64`, `linux-aarch64`, `macos-aarch64`, `windows-x86_64`, …). The upload fails on another platform. Omit it for packages that run anywhere. |
| `launch_args_schema` | no | A JSON Schema (draft 2020-12) of a node's settings. External `$ref`s aren't resolved. |
| `launch_args_style` | no | `flags` (default; a node's settings are a JSON object) or `positional` (a list of strings, passed as they are; no annotations). |
| `payload_schemas` | by type | `in` for a SINK, `out` for a SOURCE, both for a TRANSFORM. Each is either a path to a `SchemaDefinition` in proto3 JSON (`{"fields": [{"key": "url", "type": "STRING"}]}`), or `{"from_launch_args": "<JSON pointer>", "default": "<path>"?}`: the node's own schema, from its settings ([section 3.5](#35-per-node-payload-schemas)). The plugin MUST declare exactly the schemas in effect in `Initialize`. |
| `runtime_defaults` | no | Defaults for the node's runtime settings (timeouts, restarts, queue, delivery, `is_critical_path`); same keys as a node's `config.runtime`. `is_critical_path` says whether a node of this plugin is critical unless the user changes it (default `false`). When a critical node fails for good, the scene goes to `FAILED`. Any other failed node is marked `FAILED` and taken offline: producers stop routing to it, what's queued for it is kept, the scene runs on, and the next reload relaunches it. |
| `threads` | no | How the plugin takes a **core count**: `{"via": "arg" \| "env", "name", "value"?}`, e.g. `{"via": "arg", "name": "threads"}` (the Rust SDK's `--threads`), `{"via": "env", "name": "GOMAXPROCS"}`, or `{"via": "env", "name": "JAVA_TOOL_OPTIONS", "value": "-XX:ActiveProcessorCount={n}"}` (`value` is a template, default `{n}`). See [section 5.5](#55-the-core-count). A name a launch arg also uses, a bootstrap argument, or an `FLWRS_*` variable is refused. |
| `configurator` | no | `{"executable", "fields"}`: a program in the package that sets some of a node's settings from its others, e.g. an out schema derived from an OpenAPI document ([section 3.6](#36-configured-settings-the-configurator)). A remote plugin definition gives only `fields` (its nodes' endpoints run it, [section 4.1](#41-configuring-a-remote-node)). Version 2 only. |
| `ui` | no | How a UI presents the plugin: `icon` (an `.svg` or `.png` in the package), `categories` (free tags, 1 to 64 characters, no duplicates), `color` (`#RRGGBB`), `docs` (a Markdown file in the package). |
| `x-…` | no | Top-level fields starting with `x-` are ignored, with a warning at upload. Any other unknown field is an error. |

## 3. Node settings: the launch-args schema

A node's settings (`config.launch_args` in the scene API) are a JSON object checked against `launch_args_schema`. In manifest version 2 the schema's **top-level properties** also say how each setting reaches the plugin, and a few keywords mark secrets, code and presentation.

```json
{
  "type": "object",
  "additionalProperties": false,
  "required": ["schemas", "script"],
  "properties": {
    "schemas": { "type": "object", "properties": { "in": { "type": "object" }, "out": { "type": "object" } } },
    "script": {
      "type": "string", "contentMediaType": "text/x-lua",
      "x-flwrs-launch": { "via": "file" },
      "x-flwrs-code": { "language": "lua", "params": ["payload", "meta"], "definitions": "types/script.d.lua",
                        "signature": "fun(payload: In, meta: Meta): Out|nil" }
    },
    "model": { "type": "string", "x-flwrs-launch": { "via": "file", "source": "upload" } },
    "api_key": { "type": "string", "writeOnly": true, "x-flwrs-launch": { "via": "env", "alias": "SCRIPT_API_KEY" } },
    "timeout_ms": { "type": "integer", "minimum": 1, "default": 100, "x-flwrs-group": "Limits", "x-flwrs-advanced": true }
  }
}
```

### 3.1 Delivery: `x-flwrs-launch`

On a top-level property only: `{"via": "arg" | "env" | "file", "alias"?, "file_name"?, "source"?: "upload"}`.

| `via` | The plugin gets | Default for |
|---|---|---|
| `arg` | `--<name> <value>`; `true` → `--<name>`; `false` and `null` → nothing. | strings, numbers, booleans |
| `env` | An environment variable. | (never a default) |
| `file` | `--<name> <absolute path>` of a file in the node's `config/` folder ([section 5.3](#53-the-node-folder)). | objects and arrays |

- **Names.** `alias` replaces the default name: `--<property with _ as ->` for arguments and files, `<PROPERTY_IN_UPPER_SNAKE>` for environment variables. Flags may use letters, digits and `-`, and can't be a bootstrap argument (`id`, `host`, `port`, `log-level`, `token`). Environment variable names must match `^[A-Z_][A-Z0-9_]*$` and can't start with `FLWRS_`, `LD_` or `DYLD_` or be one that changes how programs run (`PATH`, `HOME`, `PYTHONPATH`, `NODE_OPTIONS`, `JAVA_TOOL_OPTIONS`, …). No two settings may use the same flag, variable or file name.
- **Values.** Strings are passed as they are, numbers and booleans as their JSON text. Objects and arrays MUST be delivered as files (`"via": "file"`), as JSON.
- **Files.** A string is written as its UTF-8 bytes, anything else as JSON. The file is named `file_name`, else the property name plus an extension from `contentMediaType` (`text/x-lua` → `.lua`, `application/json` → `.json`, `application/yaml` → `.yaml`, `application/x-pem-file` → `.pem`, `text/plain` → `.txt`, …), else `.json` for objects and arrays, else no extension. File names: letters, digits, `.`, `_` and `-`, 1 to 100 characters, not starting with `.`.
- **Uploads.** With `"source": "upload"` (a string property, `"via": "file"`), the value is the name of a file uploaded to the node through the scene API; the plugin gets the path of that file in `config/`. On an array of strings (`"type": "array", "items": {"type": "string"}`), the value is a list of such names: each file is linked into `config/` under its name, and the plugin gets the flag once per file, in the list's order (`--model config/a.onnx --model config/b.onnx`; nothing for an empty list). Names are unique within `config/`: the same name twice, in one list or in two fields, is refused.
- Properties the schema doesn't declare (if it allows them) are delivered by the defaults.

**Limits**, checked when a node is saved and the same on every platform, so a scene runs anywhere: the whole command line (as Windows quotes it) under 30 000 characters, and each environment variable's value under 30 000. Long values belong in files.

### 3.2 Secrets: `writeOnly`

`"writeOnly": true` on a top-level property marks a secret. It MUST be delivered by `env` or `file` (command lines are visible to other users of the machine). The server never returns a secret's value: the API lists which secrets a node has set, an update that leaves one out keeps it, and `null` clears it. Scene exports leave secrets out unless asked for them.

### 3.3 Code: `x-flwrs-code`

On a string property: `{"language": "lua", "params": [names], "definitions"?: "<file in the package>", "signature"?: "<text>"}`. The value is code the plugin runs, as the body of a function with `params`. The server checks its syntax when the node is saved (a server built without Lua accepts it with a warning). `definitions` is a LuaCATS `---@meta` file for an editor (completion, signature help), and `signature` the function's type in the same notation; `In` and `Out` there stand for the node's payload schemas.

### 3.4 Presentation hints

On any property, all optional: `x-flwrs-group` (a section name), `x-flwrs-order` (an integer), `x-flwrs-advanced` (collapsed by default), `x-flwrs-widget` (`duration`, `cron`, `file-path` or `code`), `x-flwrs-platforms`, `x-flwrs-template` and `x-flwrs-graph` (below). Standard keywords (`title`, `description`, `default`, `enum`, `minimum`, `format`, …) are used as they are. An unknown `x-flwrs-*` keyword is a warning at upload, and ignored.

**Platforms:** `x-flwrs-platforms: ["macos", "linux-aarch64"]` on any property, nested ones included, lists the platforms the setting applies to: an OS, or `<os>-<arch>`, in the manifest's notation. An editor hides the setting on other platforms for local nodes (a value already set stays visible, so it can be cleared), and never for remote nodes, which run elsewhere. The server doesn't refuse a value set on another platform, since scenes move between servers: its scene check warns about it, and the plugin decides what it means. A server that doesn't know the keyword warns about it at upload, and ignores it.

**Templates:** `x-flwrs-template: "<file in the package>"` on a property names a file that an editor puts in the field while it's empty: a starting point the user edits, such as a sample API document, a script, or the payload schemas a script expects. For a property whose value may be a string the file is that text (UTF-8); for any other (an object, a list, a number) it's the value as JSON, and a package whose JSON template doesn't parse is refused. Give the fields that work together templates that fit each other (a script and the schemas it reads and writes). It's only a suggestion: the server never applies it (a node that leaves the field out gets the schema's `default`, or nothing), and the plugin never sees it unless the user saves it as the field's value. Write it so it works as it is, if it's saved unchanged. The server serves only the files the schema names this way (`GET /api/plugins/by-id/{id}/templates/{path}`), as text. A server that doesn't know the keyword warns about it at upload, and ignores it.

**Graphs:** `x-flwrs-graph: {"catalog": "<file in the package>"}` on a top-level object property says its value is a graph of typed nodes, and an editor shows it on a canvas instead of as a form. The value keeps one shape, `{"nodes": {"<id>": {"type": "<node type>", …}}, "links": [{"from": "<node>.<port>", "to": "<node>.<port>"}], "groups": […]}`, and stays a normal launch arg (delivered, exported, diffed and checked as any other). The catalog is JSON, checked at upload against the launch-args schema; unknown keys and broken references refuse the package:

```json
{
  "format": 1,
  "port_types": { "frame": { "title": "Frame" }, "regions": { "title": "Regions" }, "boxes": { "title": "Boxes" } },
  "files_field": "files",
  "connects": [{ "from": "regions", "to": "frame", "badge": "for each" }],
  "node_types": {
    "model": {
      "title": "Model",
      "category": "Models",
      "settings": "#/$defs/model",
      "max": 8,
      "defaults": {},
      "inputs": [{ "name": "frame", "type": "frame", "when": { "pointer": "/embedding", "present": false } }],
      "outputs": [{ "type_from": { "pointer": "/decode/type", "map": { "yolo": "boxes" } } }],
      "file_settings": ["model"],
      "proposal": { "field": "model_info", "file_setting": "model", "at": "/proposal" },
      "link_badges": [{ "label": "every {value} frames", "when": { "pointer": "/sampling/every_n", "present": true } }]
    }
  },
  "groups": { "node_types": ["model"], "families": ["execution"], "settings": "#/$defs/group" }
}
```

- `settings` is a reference `#/$defs/<name>` into the launch-args schema (there's no second copy), whose `type` property is the constant node type id.
- A port has a fixed `type`, or `type_from` (a value in the node's settings, by JSON pointer, mapped to a port type). `name` defaults to the type's id. `when` shows a port only under a condition: `pointer` with exactly one of `present`, `equals`, `not_equals`, and optionally the `default` meant when the setting is left out.
- A port always connects to its own type; `connects` adds links between different types, with an optional `badge` for such a link. An input takes one link, and graphs have no cycles.
- `file_settings` are the node's settings naming one of the node's uploaded files. `files_field` is the top-level list upload setting they must be in; an editor adds a picked file to it.
- `proposal` says where settings for a picked file come from: the configured `field` holds an object by file name, each entry its proposal at the pointer `at`. An editor asks with the configurator's describe mode (§3.6) and fills only settings that are empty.
- `max` limits a type per graph; `defaults` start a new node; `link_badges` label the links into a node while their condition holds.
- `live_setting` (optional) names the top-level setting that turns the plugin's live channels (`LIVE_FEED`, `docs/protocol.md`) on. While a running node announces none and no boolean in that setting is `true`, an editor says to turn it on there.
- **Live stats on the canvas:** an editor shows a running node's first JSON live channel on its graph when the items have this shape: `{"interval_s": <seconds>, "nodes": {"<graph node id>": {"fps": …, "latency_ms": {"mean": …, "p95": …}, "errors": …, "runs": …, …}}, "<section>": {…}}`. Every number in a node's entry is listed for it (one level of `{mean, p95}`-style nesting flattened; `null` = no value); `fps`, `latency_ms` and `errors` make its short line, and `runs` or `fps` above 0 marks the links into it active (or out of it, into a node without an entry). A node without an entry is shown as not measured, never as zero. Every other top-level object is a summary section. Its first image channel is the preview.

The server serves only the catalogs the schema names (`GET /api/plugins/by-id/{id}/graph-catalogs/{path}`), as text. Where the nodes sit on the canvas is layout, stored beside the node and not in its settings. A server that doesn't know the keyword warns about it at upload, and ignores it.

**Listen addresses:** `x-flwrs-listen: true` on a top-level string property says its value is an address the plugin listens on, as `host:port` (its `default` is what the plugin binds when a node leaves it out). The server's scene check then warns when two local nodes would bind the same address (the same port, on the same address or a wildcard one: `0.0.0.0`, `::`; port 0 never clashes), within a scene or with the server's other enabled scenes. Only one of them could start listening. A server that doesn't know the keyword warns about it at upload, and ignores it.

**Device claims:** `x-flwrs-device` on a top-level property says the setting names a device its node opens exclusively, such as a camera. The server's scene check then reports a **problem** (enabling is refused) when two local nodes claim the same device, within a scene or against the server's other enabled scenes.

```json
"input": {
  "$ref": "#/$defs/input",
  "x-flwrs-device": { "kind": "camera", "field": "device", "when": { "type": "camera" }, "default": "default" }
}
```

- `kind` (required): the sort of device. Claims are only compared within a kind.
- `field`: for an object setting, the key holding the device's name. Left out, the property's own string value is the name.
- `when` (needs `field`): values the object's other keys must have for it to claim anything. Here, an `input` of another `type` (a folder of images) claims no device.
- `default`: what a node claims when it names no device (here, "the default camera"). Without one, it claims nothing. An object setting that's left out entirely claims nothing either way.

Devices are **compared by the string each node names**, exactly. The server doesn't know that `0`, `/dev/video0` and a camera's name may be one device, so two nodes naming it differently aren't reported: name a shared device the same way everywhere. Remote nodes claim nothing here, since their devices are on another machine. A server that doesn't know the keyword warns about it at upload, and ignores it.

### 3.5 Per-node payload schemas

`payload_schemas.in` / `out` may be `{"from_launch_args": "/schemas/in", "default": "schema/in.json"?}`: the JSON pointer leads (through `properties`) to an `object` property of the launch-args schema, and a node's value there is its `SchemaDefinition`. The server checks it, compiles edge mappings against it, and expects it in `Initialize`; without one, `default` applies, and without a default the node is invalid. The plugin MUST read the schema from its settings and declare it.

### 3.6 Configured settings: the configurator

Some settings follow from others: an HTTP source's out schema from the OpenAPI document it serves, a gRPC source's from its `.proto` files. A plugin can derive them with a **configurator**, a second program in its package that the server runs when a node's settings change, so the server knows the result (to check mappings and the plugin's `Initialize`) before the plugin ever runs:

```json
"configurator": { "executable": "bin/my-plugin-config", "fields": ["out_schema"] }
```

- `fields` are top-level properties of the launch-args schema, typed and validated like any other setting. They can't be secrets (`writeOnly`), and the plugin must use the `flags` style. With per-node payload schemas ([section 3.5](#35-per-node-payload-schemas)), point `from_launch_args` at a configured field.
- On a local node, the configured fields belong to the configurator: a request that sends another value for one is refused (sending the stored value back is fine). A remote node's configurator runs at an endpoint on the remote side, if the node names one ([section 4.1](#41-configuring-a-remote-node)); otherwise its configured fields are set by hand.

**When it runs.** When a local node is saved and its other settings (or the plugin's version) changed; when an upload its settings name is replaced (a failure refuses the upload); for every local node when the plugin is upgraded (a failure refuses the upgrade); and on request (`POST /api/scenes/by-id/{id}/nodes/by-id/{node_id}/configure`, e.g. after importing a scene configured by another version). While an upload its settings name is missing (a new node can't have uploads yet), it waits: the node is saved without its configured settings, and the server's check lists the node until the file is uploaded.

**How it runs** (contract version 1):

1. The server makes a fresh temporary folder, the configurator's working directory, and deletes it afterwards. It holds `config/`, laid out as the node's folder would have it ([section 5.3](#53-the-node-folder)): the setting files and uploads, as copies.
2. It writes `request.json` there:
   ```json
   { "contract": 1, "plugin": { "id": "http-source", "version": "1.2.0" }, "node_id": "01J…",
     "launch_args": { "listen": "0.0.0.0:8080", "openapi": "…" }, "fields": ["out_schema"] }
   ```
   `launch_args` are the node's settings without secrets and without the configured fields.
3. It runs the configurator with exactly `--request request.json --response response.json`, in that folder. The environment holds only the variables the plugin would get from its settings (secrets delivered by `env` included), the `PATH` (and `SystemRoot` on Windows), and `TMPDIR` / `TEMP` / `TMP` pointing into the folder.
4. The configurator writes `response.json` and exits with status 0:
   ```json
   { "values": { "out_schema": { "fields": [ … ] } },
     "files": { "descriptor": "out/api.pb" },
     "warnings": ["paths./legacy: no requestBody schema; its body is passed as bytes"],
     "errors": [] }
   ```
   - `values`: the configured fields' values. Only fields from `fields`, and not those delivered as uploaded files.
   - `files`: for configured fields that are uploads (`"source": "upload"`): a path inside the folder to a file it wrote, or for a list field a list of paths (file names unique). The server stores each file as one of the node's uploads, under its file name, and sets the field to that name (a list field to the list of names).
   - `warnings` are kept with the node and shown by the scene's check (as warnings: they don't stop a scene), for as long as the node has the settings that run produced; a later run replaces them. Any `errors` (or a non-zero exit, or no `response.json`) refuse the change, and the user sees the `errors` (or the program's last lines of stderr).
5. The server checks the result: only the requested fields, files inside the folder, regular, and within `[scenes] max_upload_bytes`, and the node's settings with the configured values in place against the launch-args schema and the delivery rules. A configurator that takes longer than `[plugins] configure_timeout_ms` (30 s) is stopped, and `response.json` may hold at most `[plugins] configure_max_response_bytes` (1 MiB).

A configurator must be deterministic for the same input and must not depend on the network: it runs whenever the server needs the result, possibly on another day. It's code from the package, like the plugin, run by the server with the same trust. `flwrs-trellis configure` runs one the way the server does, and `*.configure.toml` test files check it ([trellis](../trellis/README.md)).

#### Pre-flight checks

A configurator can also answer a narrower question, read-only: **can this node run on this machine right now?** (the camera exists, the GPU is there, the binary's libraries load). A plugin opts in:

```json
"configurator": { "executable": "bin/my-plugin-config", "fields": ["out_schema"], "preflight": true }
```

Only plugins declaring `preflight` are ever run in this mode, so an existing configurator never sees its arguments. A remote plugin definition can't declare it, and remote nodes are skipped: they run elsewhere, so this machine says nothing about them.

**When it runs.** For every local node before a scene is **enabled** (in parallel, without holding up other changes to the server's scenes); for a draft's added and changed nodes **before it's committed** to an enabled scene; by the director before a scene **starts**, and on a **reload** for the nodes added or changed; and on request, starting nothing (`POST /api/scenes/by-id/{id}/preflight`, `…/nodes/by-id/{node_id}/preflight`, `?draft=true` for the scene's draft).

**How it runs** is the contract above, with these differences (contract version 2):

- The arguments are `--mode preflight --request request.json --response response.json`.
- `request.json` has `"contract": 2` and `"mode": "preflight"`, and `launch_args` are the node's whole settings, **with** the configured fields (still without secrets, which arrive as they do for the plugin). `config/` holds their files.
- `request.json` also has `releasing`: a list of `{"node_id", "launch_args"}` for the nodes of the **same plugin** in the **same scene** that are running now and that this change will stop (changed or removed nodes), secrets left out. It's empty for a scene that isn't running. Nodes that keep running aren't listed, and neither are other scenes' nodes, so a real clash with one of them still shows.
- `response.json` is:
  ```json
  { "problems": [ { "message": "no camera at /dev/video0", "retryable": true } ],
    "warnings": ["the GPU driver is older than recommended"] }
  ```
  No problems means the node can run. `retryable` (default `false`) says the problem may go away by itself, like a device still starting up. A check that finds problems may exit non-zero, as long as it writes them.
- The run is read-only: a response with `values` or `files` in it is refused, as is any other unknown key. It must not change anything outside its temporary folder.

**Resources held by `releasing` nodes.** The old version of a node may hold what the new one needs, such as a camera opened exclusively. Whatever a node in `releasing` holds **will be free when the new version starts** (the old process has fully stopped first), so the configurator MUST NOT report it as busy. It should check what it can without taking it (the device still exists, it supports the settings), and it MUST NEVER try to open it exclusively: that could disturb the running version. A configurator that ignores `releasing` still works, but may report its own running version as busy.

A program that can't start, crashes, or writes no valid answer is a **permanent** problem carrying the end of its stderr (e.g. `GLIBC_2.38 not found`). One that exceeds `[plugins] configure_timeout_ms` is stopped, and that's a **retryable** problem.

**What the server does with the answer.**

- **Enabling a scene:** a permanent problem refuses it (`409`, the problems in `details`, as for the scene's check). Retryable problems don't: the scene goes to `WAITING`, with the problems in its status, and the nodes still failing are checked again with exponential backoff until they pass (the scene starts) or a limit is reached (`FAILED`).
- **Submitting a draft to an enabled scene:** the checks run first, and live and the running scene aren't touched until they pass. If they pass, the draft is committed and the scene reloads. A permanent problem commits nothing: the submit answers `409`, and the draft is kept, marked `FAILED` with the problems. Retryable problems only: the submit answers `202`, the draft is marked `WAITING`, and the server retries; on a pass it commits, if the draft is unchanged and live hasn't moved on (otherwise the draft is marked `FAILED`, saying why). At the limit the draft is marked `FAILED`. Editing, rebasing, undoing or discarding the draft, submitting it again, or disabling or deleting the scene ends the wait and kills a check in progress. A wait doesn't survive a server restart: the draft is then `FAILED`, and live never changed. A submit that disables the scene, or a submit to a disabled scene, commits without a check (it runs at enable).
- **Changes that bypass drafts** (direct node and edge edits, `graph/batch`, plugin upgrades) are committed first and reach the director afterwards. While a **running** scene waits for such a reload it keeps its previous graph: unchanged nodes aren't touched, and a changed node keeps its old settings until the check passes. If the check fails or reaches the limit, the scene **keeps running its previous graph** and stays `RUNNING`, with the failed update and its problems in its status (`failed_update`), until a later reload succeeds. A scene that isn't running goes to `FAILED`.

The limits are `[director] preflight_retry` (`max_attempts` 10, `max_wait` `5m`, `initial_delay` `1s`, `max_delay` `30s`; whichever comes first), and a scene can override each in its `defaults.preflight_retry`.

**What pre-flight can't prove.** For an exclusive resource held by the running version, a check can only say that the device exists and fits the settings: it can't take the lock while the old process has it. So a change can pass and the new version still fail after the switch-over, for example because the old process didn't fully exit or didn't release the device. The server stops the old process completely before launching the new one, and then doesn't try to recover: there's **no rollback**, and it doesn't kill or chase whatever holds the device beyond the normal shutdown. A node started by a reload that never reaches `Running` gets up to its `max_restarts`, or 5 restarts if it sets none (as does a critical node at any start), and then fails; `max_restarts: 0` fails it on the first failed launch and `-1` means try forever. If it's critical (`is_critical_path`), the **scene** goes to `FAILED`, naming the node and its last error, and the change stays committed. A person takes over: fix the cause, then disable and enable the scene, or submit a fix. If it isn't critical, only the node goes offline.

**In a draft.** A local node's settings that its configurator refuses are still saved to a scene's draft, so unfinished work isn't lost. The configured fields are then left out (values from earlier settings would look valid), and the scene's check lists the configurator's errors as problems, which block enabling and submitting to an enabled scene. A save to the live graph is refused as before, and so is a value of the node's own for a configured field. A disabled scene may take such a node from its draft; a plugin upgrade then skips it (its configurator already refused these settings, none of its configured fields are set): it isn't run or checked again, takes the new version with its settings unchanged, and the check keeps reporting it. A node that was configured and that the new version's configurator refuses still refuses the upgrade.

**Describe.** A configurator may declare `"describe": true` next to `fields`: a read-only mode for editors, asking what its configured fields would say about the node's uploaded files while the other settings may still be incomplete (a model picked for a node that has no decoder yet). It runs as above with `--mode describe`, `"contract": 3`, `"mode": "describe"`, and only the node's upload settings in `launch_args`. It answers `{"values": {"<configured field>": …}}`, or `{"errors": […]}`; other fields and returned files are refused, and nothing is stored. Only plugins declaring it are run this way (`POST /api/scenes/by-id/{id}/nodes/by-id/{node_id}/describe`), and a remote plugin definition can't declare it.

**Saying where a problem is.** A configurator's error, warning or pre-flight message may start with `[at <JSON pointer>] `, the place in the launch args it's about (`[at /graph/nodes/det] the input isn't linked`). An editor marks that place and leaves the tag out; to everything else it's part of the text. For a graph setting, the pointers it understands are `/<setting>/nodes/<id>` and `/<setting>/links/<index>`.

## 4. Remote plugins

A node with `launch = "remote"` runs a plugin someone else starts, possibly on another machine. It gets no node folder, and its launch args are never delivered; a remote node MAY carry launch args only for its per-node payload schemas, which the hub then expects. Its plugin's configurator ([section 3.6](#36-configured-settings-the-configurator)) can't run on the server for it (the package may be built for the remote machine's platform): either the remote side offers it as an endpoint ([section 4.1](#41-configuring-a-remote-node)), or the remote node sets the configured fields itself, so its out schema is fixed up front.

### 4.1 Configuring a remote node

A remote node whose plugin has a configurator can name an **endpoint** on the remote side that runs it: the same contract ([section 3.6](#36-configured-settings-the-configurator)), as a JSON `POST`. In the node's `config`:

```json
"configurator": {
  "url": "https://plugins.example.com/configure",
  "auth": { "type": "bearer", "token": "…" },
  "ca": "remote-ca.pem",
  "send_secrets": false
}
```

- `url`: `https://…`, or `http://…` **on a trusted network only** (settings and credentials then cross the network unencrypted; the server's check warns about it). The server must allow it: `[plugins] remote_configurator_urls` lists URL prefixes (a prefix matches itself and what follows it after a `/` or `?`); empty, the default, turns remote configurators off.
- `auth`: `{"type": "basic", "username", "password"}`, `{"type": "bearer", "token"}`, or `{"type": "mtls", "cert", "key"}` (the node's uploaded PEM files: a client certificate and its key). Passwords, tokens and the key are secrets: never shown by the API, kept when an update leaves them out, and left out of exports unless secrets are included.
- `ca`: the node's uploaded PEM file with the CA(s) of the endpoint's certificate (default: the bundled Mozilla roots).
- `send_secrets`: also send the node's secret settings (default: they're left out).

**The request** is the contract's `request.json`, plus `files`: the files `config/` would hold (setting files and uploads), by name, base64-encoded:

```json
{ "contract": 1, "plugin": { "id": "http-source", "version": "1.2.0" }, "node_id": "01J…",
  "launch_args": { "openapi": "…" }, "fields": ["out_schema"],
  "files": { "openapi.json": "eyJvcGVuYXBpIjo…" } }
```

**The answer** is `200` (any 2xx) with the contract's `response.json`, except that a returned file is a URL the server downloads (with the same credentials and TLS settings), `{"name", "url"}`, or a list of them for a list field:

```json
{ "values": { "out_schema": { "fields": [ … ] } },
  "files": { "descriptor": { "name": "api.pb", "url": "https://plugins.example.com/files/api.pb" } },
  "warnings": [], "errors": [] }
```

Any other status, `errors`, or a file that can't be downloaded refuses the change; the user sees the errors (or the status and the start of the body). The server sends nothing else to the endpoint and follows no redirects; the request has `[plugins] configure_timeout_ms` to finish, the answer may hold at most `configure_max_response_bytes`, each file `[scenes] max_upload_bytes`, and every URL (the endpoint's and the files') must be within `remote_configurator_urls`.

**When it runs:** as for a local node: when the node's settings, its endpoint (`url`, `send_secrets`) or the plugin's version change (a changed password or token alone doesn't), when a file its settings name is replaced, on a plugin upgrade, and on `POST …/configure`. While a file it needs is missing (a named upload, or the endpoint's CA, certificate or key: a new node has no uploads yet), it waits, and the server's check lists the node. An endpoint that can't be reached refuses the change like any other failure: it likely means the remote node can't be reached either. The plugin is started with the bootstrap arguments of [section 5.1](#51-the-command-line) pointing at the hub's reachable address, and authenticates with the node's pre-shared token or a client certificate ([protocol section 14](protocol.md#14-security)).

### 4.2 Remote plugin definitions

A plugin that only ever runs elsewhere doesn't need its program on the server: a **remote plugin definition** describes it, so its nodes can be placed, wired, mapped and checked, and the hub can check its `Initialize`. It's a package whose manifest says `"launch": "remote"` and has no `executable`, `platforms`, `threads` or configurator program (a `configurator` gives only its `fields`); everything else (type, payload schemas, the launch-args schema, `runtime_defaults`, `ui`) is as for any package, with the files it references. Nothing in it runs on the server, and nothing is deployed to the remote side.

- Its nodes must be remote (`launch = "remote"`): a local one is refused when saved and listed by the server's check.
- It's registered like a package (`POST /api/plugins` with a ZIP, which needs `[plugins] allow_upload` as any upload does; `flwrs-trellis package` builds one without a build step) or from JSON, the manifest and its files without a ZIP (`POST /api/plugins/remote`, which the web UI's remote plugin wizard uses). Upgrades and deletes are as for packages; `GET /api/plugins/by-id/{id}/export` gives a registered definition back as a package ZIP, to install on another server.
- `flwrs-trellis audit`, `test` and `bench` check the plugin that will connect: `[remote]` in `trellis.toml` gives the commands that make it start or connect to trellis's hub, and its configurator endpoint ([trellis](../trellis/README.md#remote-plugins)).

## 5. Launching a local plugin

### 5.1 The command line

```
<install folder>/<executable> --id <instance id> --host <hub host> --port <hub port> --log-level <trace|debug|info|warn|error> [the node's arg and file settings, by name]
```

- The executable is started directly (no shell), by its absolute path.
- The bootstrap arguments come first, then the node's settings delivered as arguments, ordered by name.
- A plugin MUST accept the bootstrap arguments in this long form and SHOULD ignore their order. It MUST NOT expect other arguments from the server than these and its own settings.

### 5.2 The environment

- `FLWRS_TOKEN`: a fresh random token for this launch, which the plugin sends in `Initialize`.
- The node's settings delivered by `env`.
- Otherwise the server's own environment. Child processes a plugin starts inherit all of it, secrets included: a plugin that starts other programs SHOULD give them only what they need.
- Standard input is closed. Standard output and standard error go to the server log (one entry per line, cut at 8 KiB).

### 5.3 The node folder

The working directory is the node's own folder, `<nodes dir>/<scene id>/<node id>/` on the server:

```
<node folder>/
├── bin/, schema/, …      a link to each top-level entry of the package
├── config/               the server's: the node's file settings and uploads (read-only)
└── …                     the plugin's own files
```

- **Package files** are reachable by relative path through the links (symbolic links on Unix; on Windows, junctions for folders and hard links or copies for files). Links to a newer version replace them when the plugin is upgraded.
- **`config/`** holds exactly the node's `file` settings and links to its uploads, brought up to date before each launch and never changed while the plugin runs. It's read-only by permission; a plugin MUST NOT change it.
- **Everything else is the plugin's**: state, caches, downloads. It's kept across restarts, reloads, disabling and enabling the scene, and plugin upgrades, and deleted with the node or its scene (or on request, while the scene is disabled). A plugin SHOULD keep its state here rather than in its install folder, which is shared by every node and replaced on upgrade.
- The folder can hold secrets (`config/`), so only the server's user can enter it on Unix.

### 5.4 Stopping

To stop a local plugin the hub sends `Shutdown` and waits for the process to exit (10 s by default). Then it asks the OS to stop the process and everything it started (SIGTERM to its process group on Unix, Ctrl-Break to its console group on Windows), waits again (2 s by default), and kills it. A plugin SHOULD exit on `Shutdown`, and SHOULD treat SIGTERM or Ctrl-Break as a last request to stop. An exit the hub didn't ask for makes it relaunch the plugin after a delay.

### 5.5 The thread count

A node can say how many threads its plugin may use: `runtime.threads` in its config, else its share of a scene **core budget** (a `resources` group's `limits.cpu`, split among the members that set no value of their own), else the manifest's `runtime_defaults.threads`, else the server's `[director] plugin_threads`. The number `n` means threads: `1` single-threaded, `2` two, and so on; `0` or nothing means the plugin decides (typically one thread per core). It lets a device or server keep cores reserved for other work.

The server delivers it only to a plugin whose manifest declares `threads`, the way it declares: as `--<name> <value>` after the bootstrap arguments (`arg`), or as the environment variable `<name>=<value>` (`env`), with `{n}` in `value` replaced by the number. Without the declaration, or for `0`, nothing is passed (the server's check warns about a node that sets a count its plugin wouldn't get). A plugin SHOULD honour it, but MAY treat it as a hint: what a "thread" is depends on the runtime (e.g., the Rust SDK maps `n` to tokio's main thread plus `n - 1` workers, and `1` to a single-threaded runtime; Go's `GOMAXPROCS` caps the threads running Go code; the JVM's `-XX:ActiveProcessorCount` sizes its pools).

## 6. Guidance

These aren't requirements, but plugins that follow them are easier to run.

- **Read settings at start.** Read `file` settings and uploads when the plugin starts; they don't change while it runs (a change restarts the plugin).
- **Report bad settings early.** Settings the schema can't express (a regex that doesn't compile, a URL that doesn't resolve) are best checked at start: log the problem and exit with a non-zero status, which the server shows as a failed launch.
- **Describe the settings well.** The launch-args schema is all a UI has to build a form: give properties a `title` and `description`, `default`s, `enum`s and ranges, and group them with `x-flwrs-group`.
- **Keep secrets out of logs** and out of child processes' environments.
- **Test with trellis.** `flwrs-trellis audit` checks the protocol, `test` the plugin's behaviour, and `bench` its performance. They run the plugin the way the server does: in a node folder, with the settings delivered as described here (`[uploads]` in `trellis.toml` for uploaded files).

## 7. Versions

This document describes manifest version 2. A server reads every manifest version up to the newest it knows, and refuses newer ones outright rather than half-understanding them. Additions that older servers can safely ignore (presentation hints, `x-` fields) don't need a new manifest version.
