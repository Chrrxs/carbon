--!strict

local PlaytestStop = require("../src/PlaytestStop")

local function newWorkspace(attributes: { [string]: any })
	local workspace = {}
	function workspace:GetAttribute(name: string)
		return attributes[name]
	end
	return workspace
end

-- Only a play server copied from a Carbon-served loopback session waits for stops.
local served = PlaytestStop.target(newWorkspace({
	__StudioWorktree_CarbonEndpoint = "http://127.0.0.1:48311",
	__StudioWorktree_Session = "session-1",
}))
assert(served ~= nil, "a served play server must find its Carbon endpoint")
assert(served.url == "http://127.0.0.1:48311/studio/playtest/await-stop")
assert(served.sessionToken == "session-1")
assert(PlaytestStop.target(newWorkspace({})) == nil, "an unserved place never waits")
assert(PlaytestStop.target(newWorkspace({
	__StudioWorktree_CarbonEndpoint = "http://example.com:48311",
	__StudioWorktree_Session = "session-1",
})) == nil, "only a loopback Carbon endpoint may end a playtest")
assert(PlaytestStop.target(newWorkspace({
	__StudioWorktree_CarbonEndpoint = "http://127.0.0.1:48311",
	__StudioWorktree_Session = "",
})) == nil)

local function newHttpService(responses: { any })
	local service = { requests = {} }
	function service:JSONEncode(value: any): string
		return "session=" .. tostring(value.sessionToken)
	end
	function service:JSONDecode(body: string): any
		return { stop = body == "stop" }
	end
	function service:RequestAsync(request: any): any
		table.insert(self.requests, request)
		local response = table.remove(responses, 1)
		if response == "error" then
			error("HttpError: ConnectFail", 0)
		end
		return response
	end
	return service
end

local function newStudioTestService()
	local service = { ended = {} }
	function service:EndTest(value: any)
		table.insert(self.ended, value)
	end
	return service
end

-- The play server keeps waiting through idle polls and lost connections, then
-- ends the playtest as soon as serve asks.
local http = newHttpService({
	{ Success = true, StatusCode = 200, Body = "wait" },
	"error",
	{ Success = true, StatusCode = 200, Body = "stop" },
})
local studioTestService = newStudioTestService()
local waits = {}
assert(
	PlaytestStop.monitor({
		target = served,
		httpService = http,
		studioTestService = studioTestService,
		isRunning = function()
			return true
		end,
		wait = function(seconds: number)
			table.insert(waits, seconds)
		end,
	}),
	"the requested stop must end the playtest"
)
assert(#http.requests == 3)
assert(http.requests[1].Url == served.url and http.requests[1].Method == "POST")
assert(http.requests[1].Body == "session=session-1", "the poll must prove the served session")
assert(#waits == 1, "only a failed poll backs off")
assert(studioTestService.ended[1] == PlaytestStop.END_RESULT, "the playtest must end with Carbon's result")

-- A playtest that ends on its own stops the monitor without ending anything.
local running = true
local idleHttp = newHttpService({ { Success = true, StatusCode = 200, Body = "wait" } })
local idleService = newStudioTestService()
assert(not PlaytestStop.monitor({
	target = served,
	httpService = idleHttp,
	studioTestService = idleService,
	isRunning = function()
		local current = running
		running = false
		return current
	end,
	wait = function() end,
}))
assert(#idleService.ended == 0, "an unrequested playtest must never end")

-- A rejected poll, such as another session's token, never ends the playtest.
local rejected = newHttpService({ { Success = false, StatusCode = 403, Body = "stop" } })
local rejectedService = newStudioTestService()
local polls = 0
PlaytestStop.monitor({
	target = served,
	httpService = rejected,
	studioTestService = rejectedService,
	isRunning = function()
		polls += 1
		return polls == 1
	end,
	wait = function() end,
})
assert(#rejectedService.ended == 0, "a rejected poll must not end the playtest")

print("PASS: a served play server ends its playtest only when its Carbon session asks")
