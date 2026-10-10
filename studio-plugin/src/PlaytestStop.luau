--!strict

-- A quick save needs Studio in edit mode, but only a playtest's server can call
-- StudioTestService:EndTest. A play server copied from a Carbon-served place
-- long-polls serve and ends its playtest when the edit plugin asks for a save.

local PlaytestStop = {}

PlaytestStop.END_RESULT = "carbon-quick-save"
local RETRY_DELAY = 2

export type Target = {
	url: string,
	sessionToken: string,
}

export type MonitorOptions = {
	target: Target,
	httpService: any,
	studioTestService: any,
	isRunning: () -> boolean,
	wait: ((seconds: number) -> ())?,
}

-- The served session's Workspace attributes are copied into the play server.
function PlaytestStop.target(workspace: any): Target?
	local readable, endpoint, sessionToken = pcall(function()
		return workspace:GetAttribute("__StudioWorktree_CarbonEndpoint"),
			workspace:GetAttribute("__StudioWorktree_Session")
	end)
	if not readable or type(endpoint) ~= "string" or type(sessionToken) ~= "string" or sessionToken == "" then
		return nil
	end
	local host, port = endpoint:match("^http://([^:/]+):(%d+)$")
	if (host ~= "127.0.0.1" and host ~= "localhost") or port == nil then
		return nil
	end
	return {
		url = `{endpoint}/studio/playtest/await-stop`,
		sessionToken = sessionToken,
	}
end

-- Poll until serve asks for a stop, then end the playtest. Returns whether it
-- ended the playtest; it returns false once the playtest ends on its own.
function PlaytestStop.monitor(options: MonitorOptions): boolean
	local wait = options.wait or task.wait
	local httpService = options.httpService
	while options.isRunning() do
		local polled, response = pcall(function()
			return httpService:RequestAsync({
				Url = options.target.url,
				Method = "POST",
				Headers = { ["Content-Type"] = "application/json" },
				Body = httpService:JSONEncode({ sessionToken = options.target.sessionToken }),
			})
		end)
		local stop = false
		if polled and response.Success then
			local decoded, body = pcall(function()
				return httpService:JSONDecode(response.Body)
			end)
			if decoded and type(body) == "table" and body.stop == true then
				stop = true
			end
		else
			-- Serve restarted, refused this session, or predates this endpoint.
			wait(RETRY_DELAY)
		end
		if stop then
			pcall(function()
				options.studioTestService:EndTest(PlaytestStop.END_RESULT)
			end)
			return true
		end
	end
	return false
end

return PlaytestStop
