-- Runs for each event: `state` is the group's state (from Init), `payload`
-- follows the in schema. Return the new state and, optionally, an output
-- shaped like the out schema: here a running mean, for each event.
-- With windows, Finish returns each window's output when it closes.
state.count = state.count + 1
state.sum = state.sum + payload.value
return state, { key = key, count = state.count, sum = state.sum, mean = state.sum / state.count }
