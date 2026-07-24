--!strict

local InstanceIdentity = require("../src/Client/InstanceIdentity")

local stored = nil
local writes = 0
local serverStorage = {}
function serverStorage:GetAttribute(name)
	assert(name == "__MCPPlaceId")
	return stored
end
function serverStorage:SetAttribute(_name, _value)
	writes += 1
end

local httpService = {}
function httpService:GenerateGUID(_wrap)
	return "ephemeral-guid"
end

local anonymous = InstanceIdentity.compute(0, serverStorage, httpService)
assert(anonymous == "anon:ephemeral-guid", "unpublished places need a session-local routing identity")
assert(stored == nil and writes == 0, "live-session attach must not persist __MCPPlaceId")

local absentService = InstanceIdentity.compute(0, nil, httpService)
assert(absentService == "anon:ephemeral-guid", "identity must not require materializing ServerStorage")
assert(writes == 0, "an absent ServerStorage must remain absent")

stored = "existing-authored-id"
local existing = InstanceIdentity.compute(0, serverStorage, httpService)
assert(existing == "anon:existing-authored-id", "an existing authored/MCP identity should remain readable")
assert(writes == 0, "reading an existing identity must not rewrite it")

local published = InstanceIdentity.compute(12345, serverStorage, httpService)
assert(published == "place:12345", "published places retain their stable PlaceId route")
assert(writes == 0, "published routing must not touch ServerStorage metadata")

print("PASS: live-session instance identity is non-persistent")
