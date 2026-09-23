-- loomrt core.
--
-- Every bundle the Loom compiler emits starts with the runtime (`rt`), which
-- holds the helpers that Loom semantics need on top of plain Lua: checked
-- integer arithmetic, formatting, shuttles, unravel reporting, and so on.
-- See design/lua-backend.md for what lives here and why.
--
-- Only the skeleton exists so far; helpers arrive with the milestones that
-- need them.

local rt = {}

rt.VERSION = "0.1.0"

return rt
