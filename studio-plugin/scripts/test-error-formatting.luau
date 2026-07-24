--!strict
-- selene: allow(incorrect_standard_library_use)

local formatError = require("../src/ErrorFormat")

local formatted = formatError("Failed: $1 ($2)", { "100% complete", "%0%1%q" }, tostring)
assert(formatted == "Failed: 100% complete (%0%1%q)", `unexpected formatted error: {formatted}`)

print("PASS: error interpolation preserves literal percent characters")
