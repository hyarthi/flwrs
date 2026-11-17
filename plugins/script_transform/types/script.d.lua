---@meta
-- Definitions for the script transform's editor (LuaCATS, lua-language-server).

---@class Meta
---@field event_id string
---@field trace_id string
---@field hops integer
---@field origin_node_id string
---@field create_time integer Epoch milliseconds (UTC).

---@class Many
-- Several outputs from one input; see emit_all.

---A value kept in this node's state, or nil.
---@param key string 1 to 256 bytes.
---@return any|nil
function load(key) end

---Keeps a value in this node's state (committed with the event's outputs,
---exactly once). nil isn't a value: use delete.
---@param key string 1 to 256 bytes.
---@param value any Numbers, strings (bytes too), booleans, tables.
---@param opts? { ttl?: string } e.g. { ttl = "10m" }: forgotten after that long.
function store(key, value, opts) end

---Removes a key from this node's state.
---@param key string
function delete(key) end

---Several outputs from one input: `return emit_all({ a, b })`.
---@param outputs table[]
---@return Many
function emit_all(outputs) end

json = {}
---@param value any
---@return string
function json.encode(value) end
---@param text string
---@return any
function json.decode(text) end

time = {}
---The current time, epoch milliseconds (UTC).
---@return integer
function time.now() end

log = {}
---To the plugin's log (rate-limited).
---@param ... any
function log.info(...) end
---@param ... any
function log.warn(...) end
