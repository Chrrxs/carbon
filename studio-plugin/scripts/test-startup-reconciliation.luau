--!strict

local InitialReconciliation = require("../src/Core/InitialReconciliation")

local generation = "built-source-generation"
assert(
	InitialReconciliation.isTrustedBuiltPlace(
		{ isPlace = true, sourceGeneration = generation },
		{ sourceGeneration = generation }
	),
	"a generation-matched managed place must use the built-source fast path"
)
assert(
	not InitialReconciliation.isTrustedBuiltPlace({ isPlace = true, sourceGeneration = generation }, nil)
		and not InitialReconciliation.isTrustedBuiltPlace(
			{ isPlace = true, sourceGeneration = generation },
			{ sourceGeneration = nil }
		),
	"unversioned arbitrary places must retain one-time Studio adoption"
)

local renamedRoot = { Name = "New" }
local lateRoot = { Name = "Lazy" }
local initialNames = { [renamedRoot] = "Old" }
assert(
	InitialReconciliation.identityName(initialNames, renamedRoot) == "Old",
	"a root renamed during hierarchy paging must retain its frozen source identity"
)
assert(
	InitialReconciliation.identityName(initialNames, lateRoot) == "Lazy",
	"a root materialized after the frozen hierarchy must fall back to its current name"
)
lateRoot.Name = "Old"
local identityBucket = {}
InitialReconciliation.appendIdentityCandidate(identityBucket, initialNames, lateRoot)
InitialReconciliation.appendIdentityCandidate(identityBucket, initialNames, renamedRoot)
assert(
	identityBucket[1] == renamedRoot and identityBucket[2] == lateRoot,
	"a later same-class/name root must never steal a frozen root's source identity"
)

local retained = {}
local deleted = {}
local alive = { [retained] = true, [deleted] = true }
local attempts = 0
local success, reason = InitialReconciliation.beginStableBatch({ retained, deleted }, function(instance)
	return alive[instance] == true
end, function(instances)
	attempts += 1
	if attempts == 1 then
		assert(#instances == 2)
		alive[deleted] = false
		return false, "native identity disappeared"
	end
	assert(#instances == 1 and instances[1] == retained, "retry must omit only the mapped instance deleted mid-page")
	return true
end)
assert(success and reason == nil and attempts == 2, "a deletion during exact batching must retry the surviving set")

print("PASS: built-place startup bypass and arbitrary-place adoption remain explicitly separated")
