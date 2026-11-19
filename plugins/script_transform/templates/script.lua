-- Runs once per event: `payload` follows the in schema, `meta` is the
-- event's metadata. Return a table shaped like the out schema (one output),
-- nil (no output) or emit_all({ ... }) (several). Fields the out schema
-- doesn't declare are dropped.
--
-- This example converts a reading to Fahrenheit, flags hot ones, and counts
-- each device's readings in the node's state (load / store: kept with the
-- event's outputs, exactly once).
local readings = (load(payload.device) or 0) + 1
store(payload.device, readings)

return {
  device = payload.device,
  temp_f = payload.temp_c * 9 / 5 + 32,
  alert = payload.temp_c > 30,
  readings = readings,
}
