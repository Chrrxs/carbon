--!strict

local PrivilegedNegotiation = require("../src/Core/PrivilegedNegotiation")

local attempts = 0
local waits = {}
local success, capabilities = PrivilegedNegotiation.run(function()
	attempts += 1
	if attempts < 3 then
		error("bridge route is not visible yet")
	end
	return { bridgeId = "exact-studio-bridge" }
end, function(delay)
	table.insert(waits, delay)
end)

assert(success, "a bridge that appears after plugin startup must activate elevation")
assert(capabilities.bridgeId == "exact-studio-bridge", "negotiation must return the matching bridge")
assert(attempts == 3, "negotiation must stop immediately after the bridge becomes available")
assert(#waits == 2 and waits[1] == 0.25 and waits[2] == 0.5, "bridge retries must use bounded backoff")

local failures = 0
local unavailable, reason = PrivilegedNegotiation.run(function()
	failures += 1
	error("bridge absent")
end, function() end)
assert(not unavailable and tostring(reason):find("bridge absent", 1, true), "an absent bridge must fail closed")
assert(failures == 4, "bridge negotiation must remain bounded")

print("PASS: privileged elevation tolerates bridge startup races and fails closed")
