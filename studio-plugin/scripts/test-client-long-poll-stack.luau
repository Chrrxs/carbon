--!strict

local fs = require("@lune/fs")

local clientSource = fs.readFile("src/Client/init.luau")
assert(
	not string.find(clientSource, "return poll(", 1, true),
	"Capture Manifest status polling must not recursively adopt one Promise per status request"
)
assert(
	not string.find(clientSource, "return self:read()", 1, true),
	"queue-read timeouts must not recursively adopt one Promise per long-poll request"
)
local _, pollLoopUses = string.gsub(clientSource, "PollLoop.run", "")
assert(pollLoopUses == 2, "Capture Manifest and queue reads must share the iterative polling primitive")

local PollLoop = require("../src/Client/PollLoop")

local attempts = 0
local success, message = PollLoop.run(function()
	attempts += 1
	if attempts <= 100_000 then
		return false, false, "waiting"
	end
	return true, true, "managed reload"
end)

assert(success and message == "managed reload", "the first non-timeout response must escape the retry loop")
assert(attempts == 100_001, "every pending poll must retry without consuming call-stack depth")

local failedAttempts = 0
local failed, failure = PollLoop.run(function()
	failedAttempts += 1
	return true, false, "connection closed"
end)

assert(not failed and failure == "connection closed", "terminal failures must propagate immediately")
assert(failedAttempts == 1, "terminal failures must not poll again")

print("PASS: Client polling keeps managed reload and queue retries stack-flat")
