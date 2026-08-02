--!strict

local InstanceIdentity = require("../src/Client/InstanceIdentity")

local stored = nil
local writes = 0
local serverStorage = {}
local attributeChanged = nil
function serverStorage:GetAttribute(name)
	assert(name == "__MCPPlaceId", "identity read used an unexpected attribute")
	return stored
end
function serverStorage:SetAttribute(_name, _value)
	writes += 1
end
function serverStorage:GetAttributeChangedSignal(name)
	assert(name == "__MCPPlaceId", "identity observation used an unexpected attribute")
	return {
		Connect = function(_self, callback)
			attributeChanged = callback
			return {
				Disconnect = function()
					attributeChanged = nil
				end,
			}
		end,
	}
end

local httpService = {}
local generated = 0
function httpService:GenerateGUID(_wrap)
	generated += 1
	return `ephemeral-guid-{generated}`
end

local anonymous = InstanceIdentity.compute(0, serverStorage, httpService)
assert(anonymous == "anon:ephemeral-guid-1", "unpublished places need a session-local routing identity")
assert(stored == nil and writes == 0, "live-session attach must not persist __MCPPlaceId")

local refreshed = {}
local connection = InstanceIdentity.observeChanges(0, serverStorage, anonymous, function(instanceId)
	table.insert(refreshed, instanceId)
end)
assert(#refreshed == 0, "an absent MCP identity must not generate a false route refresh")
stored = "mcp-reported-id"
assert(attributeChanged ~= nil, "instance identity must observe MCP route publication")
attributeChanged()
attributeChanged()
assert(#refreshed == 1 and refreshed[1] == "anon:mcp-reported-id", "MCP route refresh must publish exactly once")
connection:Disconnect()
assert(attributeChanged == nil, "route identity observation must be disconnectable")

local absentService = InstanceIdentity.compute(0, nil, httpService)
assert(absentService == "anon:ephemeral-guid-2", "identity must not require materializing ServerStorage")
assert(writes == 0, "an absent ServerStorage must remain absent")

stored = "existing-authored-id"
local existing = InstanceIdentity.compute(0, serverStorage, httpService)
assert(existing == "anon:existing-authored-id", "an existing authored/MCP identity should remain readable")
assert(writes == 0, "reading an existing identity must not rewrite it")

local published = InstanceIdentity.compute(12345, serverStorage, httpService)
assert(published == "place:12345", "published places retain their stable PlaceId route")
assert(writes == 0, "published routing must not touch ServerStorage metadata")

print("PASS: live-session instance identity is non-persistent")
