--!strict

local InitialRootBaseline = require("../src/Core/InitialRootBaseline")

local roots = {
	{ debugId = "workspace", initiallyPresent = true },
	-- These model authored roots parented after RML's OnDataModelLoaded callback.
	{ debugId = "late-lighting", initiallyPresent = false },
	{ debugId = "late-folder", initiallyPresent = false },
	{ debugId = "hidden-authored", initiallyPresent = false },
	{ debugId = "carbon-materialized", initiallyPresent = false },
}
local current = InitialRootBaseline.indexCurrent(roots)
assert(current["late-lighting"] and current["late-folder"], "late file-loader roots must remain baseline roots")

local hidden = InitialRootBaseline.hiddenCurrent(roots, {
	workspace = true,
	["late-lighting"] = true,
	["late-folder"] = true,
	["carbon-materialized"] = true,
})
assert(#hidden == 1 and hidden[1].debugId == "hidden-authored")

-- A root created after the plugin freezes the hierarchy can appear in a later
-- bridge enumeration, but it is absent from the frozen visible set.
local managedVisible = InitialRootBaseline.managedFrozenVisible(
	{ "workspace", "late-lighting", "late-folder" },
	current,
	function(debugId)
		return debugId, true
	end
)
assert(managedVisible["late-lighting"] and managedVisible["late-folder"])
assert(
	not managedVisible["carbon-materialized"],
	"a service materialized after Carbon freezes the hierarchy must not become authored state"
)

print("PASS: late serialized roots survive the endpoint-time initial baseline")
