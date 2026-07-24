local Carbon = script:FindFirstAncestor("Carbon")
local RuntimeServices = require(Carbon.RuntimeServices)
local HttpService = RuntimeServices.get("HttpService") :: HttpService

local Promise = require(Carbon.Packages.Promise)
local MsgPack = require(Carbon.Lib.MsgPack)

local Error = require(script.Parent.Error)
local RequestPolicy = require(script.Parent.RequestPolicy)

type Response = {
	Body: string,
	Headers: { [string]: string },
	StatusCode: number,
	StatusMessage: string,
	Success: boolean,
	decode: () -> { [string]: any },
}

local function methodify(response: { [string]: any }): any
	return setmetatable(response, {
		__index = {
			decode = function(self)
				return MsgPack.decode(self.Body)
			end,
		},
	})
end

local function request(
	method: string,
	url: string,
	body: { [string]: any }?,
	attempt: number?
): Promise.TypedPromise<Response>
	return Promise.new(function(resolve, reject)
		local success, response = pcall(function()
			return HttpService:RequestAsync({
				Url = url,
				Method = method,
				Headers = {
					["Content-Type"] = "application/msgpack",
				},
				Body = body and MsgPack.encode(body) or nil,
			})
		end)

		if success then
			if response.Success then
				resolve(methodify(response))
			else
				reject(Error.fromResponse(response))
			end
		else
			local retryDelay = RequestPolicy.retryDelay(response, attempt or 0)
			if retryDelay then
				task.wait(retryDelay)
				request(method, url, body, (attempt or 0) + 1):andThen(resolve, reject)
			else
				reject(Error.fromMessage(response))
			end
		end
	end)
end

local Http = {}

function Http.get(url: string)
	return request("GET", url)
end

function Http.post(url: string, body: { [string]: any }?)
	return request("POST", url, body)
end

return Http
