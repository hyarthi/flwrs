---@meta
-- Definitions for the vision source's Lua rules (LuaCATS, lua-language-server):
-- the body of `function(frame, objects, tracks, state)`. Coordinates are frame
-- pixels; times are seconds on the stream's clock.

---@class Frame
---@field seq integer The frame's number.
---@field time number Seconds on the stream's clock (real time for cameras, frame number / fps for files).
---@field width integer Pixels.
---@field height integer Pixels.

---@class Box
---@field x1 number Left, frame pixels.
---@field y1 number Top.
---@field x2 number Right.
---@field y2 number Bottom.

---@class Point
---@field x number Frame pixels.
---@field y number Frame pixels.
---@field confidence number 0..1.

---@class Class
---@field class_id integer
---@field class_name string|nil
---@field confidence number 0..1.

---@class Object
---@field class_id integer
---@field class_name string|nil
---@field confidence number 0..1.
---@field tracker_id integer|nil Its track, if a tracker confirmed one.
---@field box Box
---@field keypoints Point[] In the model's order.
---@field kp table<string, Point> By name, when the model's keypoint layout names them (e.g. kp.left_wrist).
---@field children table<string, any> What ran on this object, by graph node id: a list of Object, a list of Class, or text.

---@class Track
---@field id integer
---@field class_id integer
---@field class_name string|nil
---@field box Box
---@field vx number Velocity, pixels per second.
---@field vy number Velocity, pixels per second.
---@field age number Seconds since the track started.
---@field lost boolean Not seen on this frame (kept a while).

---@class RuleEvent
---@field event string|nil The event's name (default "custom").
---@field action string|nil What happened (default "fired").
---@field tracker_id integer|nil
---@field class_name string|nil
---@field count integer|nil
---@field duration number|nil Seconds.
---@field score number|nil
---@field label string|nil
---@field previous string|nil
---@field data table|nil Any JSON-like value, sent with the event.

---True once `condition` has been true for `seconds` in a row (false while it isn't).
---@param key string|number Names this timer; use a track id for one per person.
---@param condition boolean
---@param seconds number
---@return boolean
function held(key, condition, seconds) end

---True on the frame `condition` becomes true, then not again until it has been false; `cooldown` keeps it quiet that many seconds after firing.
---@param key string|number Names this trigger.
---@param condition boolean
---@param cooldown? number Seconds.
---@return boolean
function once(key, condition, cooldown) end

pose = {}
---The angle at `b` between `a` and `c`, degrees.
---@param a Point
---@param b Point
---@param c Point
---@return number|nil
function pose.angle(a, b, c) end
---The distance between two points, pixels.
---@param a Point
---@param b Point
---@return number
function pose.dist(a, b) end
---Whether a keypoint is confident enough to use.
---@param p Point|nil
---@param min? number Least confidence (default 0.3).
---@return boolean
function pose.visible(p, min) end
---The person's torso length, pixels (shoulders to hips), or nil when they aren't all visible.
---@param o Object
---@return number|nil
function pose.torso(o) end
