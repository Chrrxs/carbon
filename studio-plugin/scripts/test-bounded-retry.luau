--!strict

local BoundedRetry = require("../src/Core/BoundedRetry")

local attempts = 0
local waits = {}
local success, capabilities = BoundedRetry.run(function()
	attempts += 1
	if attempts < 5 then
		error("managed dependency is not visible yet")
	end
	return { identity = "connected" }
end, function(delay)
	table.insert(waits, delay)
end)

assert(success, "a dependency that appears after managed startup must be accepted")
assert(capabilities.identity == "connected", "retry must return the successful result")
assert(attempts == 5, "retry must stop immediately after the dependency becomes available")
assert(
	#waits == 4 and waits[1] == 0.25 and waits[2] == 0.5 and waits[3] == 1 and waits[4] == 2,
	"managed startup retries must use capped exponential backoff"
)

local failures = 0
local unavailable, reason = BoundedRetry.run(function()
	failures += 1
	error("dependency absent")
end, function() end)
assert(not unavailable and tostring(reason):find("dependency absent", 1, true), "an absent dependency must fail closed")
assert(failures == 10, "manual attach retry must remain bounded")

local slowAttempts = 0
local slowWaits = {}
local slowSuccess, slowCapabilities = BoundedRetry.runUntil(function()
	slowAttempts += 1
	if slowAttempts < 25 then
		error("bridge still launching")
	end
	return { identity = "late-bridge" }
end, function(delay)
	table.insert(slowWaits, delay)
end, function()
	return true
end)
assert(slowSuccess and slowCapabilities.identity == "late-bridge", "managed startup must accept a slow bridge")
assert(slowAttempts == 25 and #slowWaits == 24, "managed startup must not inherit the bounded retry limit")
assert(slowWaits[#slowWaits] == 2, "managed startup retry delay must remain capped")

local cancellationChecks = 0
local cancelled, cancellationReason = BoundedRetry.runUntil(function()
	error("bridge absent")
end, function() end, function()
	cancellationChecks += 1
	return cancellationChecks <= 3
end)
assert(
	not cancelled and tostring(cancellationReason):find("bridge absent", 1, true),
	"managed startup cancellation must return the last bridge error"
)

local boundedAttempts = 0
local bounded, boundedReason = BoundedRetry.runUntil(function()
	boundedAttempts += 1
	error("bridge never attached")
end, function() end, function()
	return boundedAttempts < 61
end)
assert(
	not bounded and tostring(boundedReason):find("bridge never attached", 1, true),
	"managed startup exhaustion must return the last bridge error"
)
assert(boundedAttempts == 60, "managed startup must fail after its generous bounded retry window")

print("PASS: retry policy tolerates slow managed launches and bounds manual attach")
