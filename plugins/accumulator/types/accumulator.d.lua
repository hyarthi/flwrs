---@meta
-- Definitions for the accumulator's editor (LuaCATS, lua-language-server).

---@class Meta
---@field event_id string
---@field trace_id string
---@field hops integer
---@field origin_node_id string
---@field create_time integer Epoch milliseconds (UTC).

---@class Window
---@field key string
---@field start_time integer Epoch milliseconds (UTC), inclusive.
---@field end_time integer Epoch milliseconds (UTC), exclusive.

---@class Many
-- Several outputs; see emit_all.

---Several outputs at once: `return state, emit_all({ a, b })`.
---@param outputs table[]
---@return Many
function emit_all(outputs) end

---Other state of this node (beyond the per-window state step gets).
---@param key string
---@return any|nil
function load(key) end
---@param key string
---@param value any
---@param opts? { ttl?: string }
function store(key, value, opts) end
---@param key string
function delete(key) end

json = {}
---@param value any
---@return string
function json.encode(value) end
---@param text string
---@return any
function json.decode(text) end

time = {}
---@return integer Epoch milliseconds (UTC).
function time.now() end

log = {}
---@param ... any
function log.info(...) end
---@param ... any
function log.warn(...) end
