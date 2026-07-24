--!strict

local BootstrapRoots = require("../src/BootstrapRoots")
local InitialRootBaseline = require("../src/Core/InitialRootBaseline")
local fs = require("@lune/fs")

local runtimeServicesSource = fs.readFile("src/RuntimeServices.luau")
local appSource = fs.readFile("src/App/init.luau")
local processorSource = fs.readFile("src/Core/Processor/init.luau")
local rootCapture = string.find(runtimeServicesSource, "BootstrapRoots.index(game:GetChildren()", 1, true)
assert(rootCapture ~= nil, "the pre-App baseline must freeze authored roots before App loads")
assert(
	not string.find(runtimeServicesSource, "game:IsLoaded()", 1, true)
		and not string.find(runtimeServicesSource, "game.Loaded:Wait()", 1, true),
	"local Studio edit bootstrap must not wait for gameplay load state"
)
local finishStream = string.find(processorSource, "function Processor:finishStream(", 1, true)
local hydratePage = string.find(processorSource, "function Processor:hydratePage(", 1, true)
local eligibleHydrationRoots =
	string.find(processorSource, "InitialRootBaseline.eligibleCurrentChildren(parent:GetChildren()", hydratePage, true)
assert(
	hydratePage ~= nil and eligibleHydrationRoots ~= nil and eligibleHydrationRoots < finishStream,
	"hydratePage must exclude Carbon-materialized roots from source identity matching"
)
local finishMaterializedFilter =
	string.find(processorSource, "InitialRootBaseline.shouldAdoptFrozen(", finishStream, true)
assert(
	finishStream ~= nil and finishMaterializedFilter ~= nil,
	"Processor finishStream must independently reject a bootstrap-materialized frozen root"
)
assert(
	string.find(appSource, 'game:FindFirstChildOfClass("ServerStorage")', 1, true)
		and not string.find(appSource, 'RuntimeServices.get("ServerStorage")', 1, true),
	"a connection marker must not materialize an absent persistent ServerStorage service"
)
local authoredLighting = { debugId = "late-file-lighting", archivable = true }
local authoredFolder = { debugId = "late-file-folder", archivable = true }
-- Plugin entry runs after Studio finishes loading the file, but before App or
-- any bundled package can call GetService.
local initial = BootstrapRoots.index({ authoredLighting, authoredFolder }, function(instance)
	return instance.debugId
end)

local carbonHttpService = { debugId = "carbon-http-service", archivable = true }
local materialized = BootstrapRoots.materialized(
	initial,
	{ authoredLighting, authoredFolder, carbonHttpService },
	function(instance)
		return instance.debugId
	end,
	function(instance)
		instance.archivable = false
	end
)

assert(initial[authoredLighting.debugId] and initial[authoredFolder.debugId])
assert(not materialized[authoredLighting.debugId] and not materialized[authoredFolder.debugId])
assert(materialized[carbonHttpService.debugId], "a service created while requiring App must be runtime state")
assert(not carbonHttpService.archivable, "Carbon-materialized services must not persist in place saves")
assert(
	not InitialRootBaseline.shouldAdoptFrozen(true, false, true, true, false),
	"finishStream eligibility must reject App-materialized HttpService even when endpoint/frozen checks pass"
)
assert(
	InitialRootBaseline.shouldAdoptFrozen(true, false, true, false, false),
	"finishStream eligibility must retain an authored late-loaded root"
)
local rootCandidates = InitialRootBaseline.eligibleCurrentChildren(
	{ authoredLighting, carbonHttpService },
	function(instance)
		return materialized[instance.debugId] == true
	end
)
assert(
	#rootCandidates == 1 and rootCandidates[1] == authoredLighting,
	"hydratePage must not match a stale source HttpService to Carbon's post-bootstrap runtime service"
)
assert(
	not InitialRootBaseline.shouldPreserveUnavailableClass(true),
	"an absent known Folder or renamed service is a place-authoritative deletion"
)
assert(
	InitialRootBaseline.shouldPreserveUnavailableClass(false),
	"only a genuinely unknown reflection class may be preserved as Studio-unavailable"
)

print("PASS: plugin-entry baseline isolates services materialized while loading Carbon")
