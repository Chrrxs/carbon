--!strict

local LiveSessionMarker = require("../src/LiveSessionMarker")

local encodedPayload = nil
local decodedPayload = {
	protocolVersion = 1,
	endpoint = "http://127.0.0.1:41234",
	project = "Captured Place",
	session = "secret-session",
	generation = "initial-generation",
}
local httpService = {
	JSONDecode = function(_, _value)
		return decodedPayload
	end,
	JSONEncode = function(_, value)
		encodedPayload = value
		return "updated-json"
	end,
}
local coreGui = { children = {} }
function coreGui:GetChildren()
	return self.children
end
local marker = {
	Parent = coreGui,
	Name = LiveSessionMarker.Name,
	ClassName = "StringValue",
	Archivable = false,
	Value = "initial-json",
}
function marker:IsA(className)
	return self.ClassName == className
end
table.insert(coreGui.children, marker)

local resolved = LiveSessionMarker.find(coreGui, httpService)
assert(resolved ~= nil, "exact RML live-session marker must resolve")
assert(resolved.marker == marker, "marker ownership")
assert(resolved.host == "127.0.0.1" and resolved.port == 41234, "explicit endpoint")
assert(resolved.project.name == "Captured Place", "project")
assert(resolved.project.sessionToken == "secret-session", "session authentication")
assert(resolved.project.sourceGeneration == "initial-generation", "initial generation")

resolved.project.updateGeneration("committed-generation")
assert(marker.Value == "updated-json", "generation update must stay on the transient marker")
assert(encodedPayload.generation == "committed-generation", "committed generation was not encoded")

local authoredCollision = {
	Parent = coreGui,
	Name = LiveSessionMarker.Name,
	ClassName = "StringValue",
	Archivable = true,
	Value = "authored",
}
function authoredCollision:IsA(className)
	return self.ClassName == className
end
assert(
	LiveSessionMarker.resolve(authoredCollision, coreGui, httpService) == nil,
	"authored marker-name collision must not auto-connect"
)

decodedPayload.endpoint = "http://example.com:41234"
assert(
	LiveSessionMarker.resolve(marker, coreGui, httpService) == nil,
	"non-loopback live-session marker must be rejected"
)
decodedPayload.endpoint = "http://127.0.0.1:41234"
decodedPayload.session = ""
assert(LiveSessionMarker.resolve(marker, coreGui, httpService) == nil, "empty session token must be rejected")

print("PASS: authenticated transient CoreGui live-session marker")
