--!strict

local ReferenceTransport = require("../src/Lib/Dom/PrivilegedReferenceTransport")
local ProtectedProperties = require("../src/Lib/Dom/ProtectedProperties")

local descriptor = {
	dataType = "Ref",
	serializes = true,
	scriptability = "None",
	privilegedExcluded = false,
}
assert(ReferenceTransport.supportsDescriptor(true, descriptor), "capability must enable canonical protected refs")
assert(
	ReferenceTransport.supportsDescriptor(true, {
		dataType = "Ref",
		serializes = true,
		scriptability = "ReadWrite",
		privilegedExcluded = false,
	}),
	"the exact transport must cover ordinary serialized refs that target hidden instances"
)
assert(not ReferenceTransport.supportsDescriptor(false, descriptor), "missing capability must fail closed")
assert(not ReferenceTransport.supportsDescriptor(true, {
	dataType = "Ref",
	serializes = true,
	scriptability = "None",
	privilegedExcluded = true,
}), "excluded refs must stay outside the privileged seam")
assert(not ReferenceTransport.supportsDescriptor(true, {
	dataType = "String",
	serializes = true,
	scriptability = "None",
	privilegedExcluded = false,
}), "the reference seam must reject non-Ref descriptors")

local knownTarget = {}
local ok, target = ReferenceTransport.resolveTarget(nil, {})
assert(ok and target == nil, "an explicit null target must remain null")

ok, target = ReferenceTransport.resolveTarget("known", { known = knownTarget })
assert(ok and target == knownTarget, "an in-tree target must retain identity")

local reason
ok, reason = ReferenceTransport.resolveTarget("outside", {})
assert(not ok and tostring(reason):find("outside", 1, true), "an out-of-tree target must fail closed")

local referenceRequests = {
	{ debugId = "owner-null", property = "Value" },
	{ debugId = "owner-target", property = "Value" },
	{ debugId = "owner-hidden", property = "Value" },
}
local cached, referenceCache = ReferenceTransport.beginBatch(referenceRequests, function(requests)
	return { requests }
end, function()
	return { values = { {}, { targetDebugId = "known" }, { targetDebugId = "hidden" } } }
end, function(request)
	return request.debugId .. "\0" .. request.property
end)
assert(cached, tostring(referenceCache))
assert(
	referenceCache["owner-null\0Value"].targetDebugId == nil,
	"a batched null reference must not require a byte value"
)
assert(
	referenceCache["owner-target\0Value"].targetDebugId == "known",
	"a batched target reference must retain its debug ID"
)
local hiddenSourceId = buffer.fromstring(string.rep("h", 16))
local nullResolved, nullTarget = ReferenceTransport.resolveResponse(referenceCache["owner-null\0Value"], {}, {})
assert(nullResolved and nullTarget == nil, "the production response resolver must preserve a batched null")
local visibleResolved, visibleTarget = ReferenceTransport.resolveResponse(
	referenceCache["owner-target\0Value"],
	{ known = knownTarget },
	{}
)
assert(
	visibleResolved and visibleTarget == knownTarget,
	"the production response resolver must preserve visible identity"
)
local hiddenResolved, hiddenTarget = ReferenceTransport.resolveResponse(
	referenceCache["owner-hidden\0Value"],
	{},
	{ hidden = hiddenSourceId }
)
assert(
	hiddenResolved
		and buffer.tostring(ReferenceTransport.sourceId(hiddenTarget) :: buffer) == buffer.tostring(hiddenSourceId),
	"the production response resolver must convert a batched hidden target to its canonical source Ref"
)
local mappedSourceId = buffer.fromstring(string.rep("m", 16))
local mappedResolved, mappedTarget = ReferenceTransport.resolveResponse({
	targetDebugId = "mapped-runtime",
	sourceId = string.rep("6d", 16),
}, { ["mapped-runtime"] = knownTarget }, {})
assert(
	mappedResolved
		and buffer.tostring(ReferenceTransport.sourceId(mappedTarget) :: buffer) == buffer.tostring(mappedSourceId),
	"an exact bridge-provided mapped identity must win over a visible target that is not yet bound in the Luau tree"
)
local hiddenContent = ReferenceTransport.contentObjectValue("hidden", { hidden = hiddenSourceId })
assert(
	hiddenContent ~= nil and buffer.tostring(hiddenContent.Content.Object) == buffer.tostring(hiddenSourceId),
	"a visible Content.Object retargeted to a hidden instance must encode the hidden source identity"
)
assert(
	ReferenceTransport.contentObjectValue("outside", { hidden = hiddenSourceId }) == nil,
	"an unmanaged Content.Object target must not acquire a fabricated source identity"
)
local malformedBatch = ReferenceTransport.beginBatch(referenceRequests, function(requests)
	return { requests }
end, function()
	return "not a batch envelope"
end, function(request)
	return request.debugId .. "\0" .. request.property
end)
assert(not malformedBatch, "a malformed successful batch envelope must fail closed instead of throwing")
local invalidTarget = ReferenceTransport.cacheResponses(
	{},
	{ referenceRequests[1] },
	{ { targetDebugId = 12 } },
	function()
		return "unused"
	end
)
assert(not invalidTarget, "a non-string target debug ID must fail closed")
local invalidSource = ReferenceTransport.cacheResponses(
	{},
	{ referenceRequests[1] },
	{ { targetDebugId = "mapped", sourceId = "not-an-id" } },
	function()
		return "unused"
	end
)
assert(not invalidSource, "a malformed mapped source identity must fail closed")

for _, qualifiedName in
	{
		"AuroraScriptObject.BehaviorWeak",
		"AuroraScriptObject.BoundInstanceWeak",
		"CoreGui.SelectionImageObject",
		"StarterGui.StudioDefaultStyleSheet",
		"StarterGui.StudioInsertWidgetLayerCollectorAutoLinkStyleSheet",
	}
do
	local separator = qualifiedName:find(".", 1, true) :: number
	local className = qualifiedName:sub(1, separator - 1)
	local propertyName = qualifiedName:sub(separator + 1)
	assert(
		ProtectedProperties.exclusionReason(className, propertyName, { DataType = { Value = "Ref" } }) == nil,
		`authored protected Ref must not be excluded: {qualifiedName}`
	)
end

print("PASS: protected reference capability, null, in-tree, and out-of-tree semantics")
