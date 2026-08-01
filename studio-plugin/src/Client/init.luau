local Carbon = script:FindFirstAncestor("Carbon")
local RuntimeServices = require(Carbon.RuntimeServices)
local HttpService = RuntimeServices.get("HttpService") :: HttpService
local CoreGui = RuntimeServices.get("CoreGui") :: CoreGui

local Promise = require(Carbon.Packages.Promise)

local Types = require(Carbon.Types)
local Place = require(Carbon.Place)
local Log = require(Carbon.Log)

local Http = require(script.Http)
local Error = require(script.Error)
local RequestPolicy = require(script.RequestPolicy)
local InstanceIdentity = require(script.InstanceIdentity)

local STUDIO_ROUTE_MARKER = "__CarbonStudioRoute"
local ENGINE_PUMP_MARKER = "__CarbonEnginePump"
local MANAGED_BASELINE_READY_MARKER = "__CarbonManagedBaselineReady"

local function generateId(): number
	return math.floor(math.random() * 10 ^ 9)
end

local function computeInstanceId(): string
	return InstanceIdentity.compute(game.PlaceId, game:FindFirstChildOfClass("ServerStorage"), HttpService)
end

local Client = {}
Client.__index = Client

function Client.new(host: string, port: number)
	local self = setmetatable({}, Client)

	self.isSubscribed = false
	self.clientId = generateId()
	self.studioSessionId = HttpService:GenerateGUID(false)
	self.instanceId = computeInstanceId()
	self.placeName = Place.Name
	self.host = host
	self.port = port
	self.enginePumpGeneration = 0

	return self
end

function Client:startEnginePump()
	self.enginePumpGeneration += 1
	local generation = self.enginePumpGeneration
	local previousReadyMarker = CoreGui:FindFirstChild(MANAGED_BASELINE_READY_MARKER)
	if previousReadyMarker then
		previousReadyMarker:Destroy()
	end
	local marker = CoreGui:FindFirstChild(ENGINE_PUMP_MARKER)
	if marker and (not marker:IsA("BoolValue") or marker.Archivable) then
		marker:Destroy()
		marker = nil
	end
	if not marker then
		marker = Instance.new("BoolValue")
		marker.Name = ENGINE_PUMP_MARKER
		marker.Archivable = false
		marker.Parent = CoreGui
	end
	task.defer(function()
		if self.enginePumpGeneration ~= generation or marker.Parent ~= CoreGui then
			return
		end
		local readyMarker = Instance.new("BoolValue")
		readyMarker.Name = MANAGED_BASELINE_READY_MARKER
		readyMarker.Archivable = false
		readyMarker.Parent = CoreGui
	end)
end

function Client:stopEnginePump()
	self.enginePumpGeneration += 1
	local marker = CoreGui:FindFirstChild(ENGINE_PUMP_MARKER)
	if marker and marker:IsA("BoolValue") and not marker.Archivable then
		marker:Destroy()
	end
	local readyMarker = CoreGui:FindFirstChild(MANAGED_BASELINE_READY_MARKER)
	if readyMarker and readyMarker:IsA("BoolValue") and not readyMarker.Archivable then
		readyMarker:Destroy()
	end
end

function Client:refreshStudioRoute()
	self.instanceId = computeInstanceId()
	local marker = CoreGui:FindFirstChild(STUDIO_ROUTE_MARKER)
	if marker and (not marker:IsA("StringValue") or marker.Archivable) then
		marker:Destroy()
		marker = nil
	end
	if not marker then
		marker = Instance.new("StringValue")
		marker.Name = STUDIO_ROUTE_MARKER
		marker.Archivable = false
		(marker :: StringValue).Value = `{self.studioSessionId}\n{self.instanceId}`
		marker.Parent = CoreGui
	else
		(marker :: StringValue).Value = `{self.studioSessionId}\n{self.instanceId}`
	end
end

function Client:getUrl(endpoint: string?): string
	return `http://{self.host}:{self.port}/{endpoint or ""}`
end

function Client:fetchReflection(): Promise.TypedPromise<{ version: { number }, apiDump: string }>
	Log.trace("Client request: reflection")

	local url = self:getUrl("reflection")

	return Http.get(url):andThen(function(response)
		return response:decode()
	end)
end

function Client:fetchDetails(): Promise.TypedPromise<Types.Project>
	Log.trace("Client request: details")

	local url = self:getUrl("details")

	return Http.get(url):andThen(function(response)
		return response:decode()
	end)
end

function Client:subscribe(): Promise.Promise
	Log.trace("Client request: subscribe")
	self:refreshStudioRoute()
	self:startEnginePump()

	local url = self:getUrl("subscribe")

	return Http.post(url, {
		clientId = self.clientId,
		name = self.placeName,
		studioSessionId = self.studioSessionId,
		instanceId = self.instanceId,
	})
		:andThen(function()
			self.isSubscribed = true
		end)
		:catch(function(err)
			self:stopEnginePump()
			if err == Error.Unknown then
				return Promise.reject(Error.new(Error.AlreadySubscribed, self.clientId))
			else
				return Promise.reject(err)
			end
		end)
end

function Client:unsubscribe(): Promise.Promise
	Log.trace("Client request: unsubscribe")

	local url = self:getUrl("unsubscribe")

	return Http.post(url, {
		clientId = self.clientId,
	})
		:andThen(function()
			self.isSubscribed = false
			self:stopEnginePump()
		end)
		:catch(function(err)
			self:stopEnginePump()
			if err == Error.Unknown then
				return Promise.reject(Error.new(Error.NotSubscribed, self.clientId))
			else
				return Promise.reject(err)
			end
		end)
end

function Client:getSnapshotPage(cursor: { Types.Ref }?): Promise.TypedPromise<Types.SnapshotPage>
	Log.trace("Client request: snapshot page")

	local url = self:getUrl("snapshot/page")

	return Http.post(url, {
		instance = buffer.create(16),
		cursor = cursor or {},
		maxInstances = 32768,
		maxBytes = 8 * 1024 * 1024,
	}):andThen(function(response)
		return response:decode()
	end)
end

function Client:getSourcePage(
	cursor: Types.SourceCursor?,
	metadataOnly: boolean?,
	mappingValues: boolean?
): Promise.TypedPromise<Types.SourcePage>
	Log.trace("Client request: source page")
	return Http.post(self:getUrl("snapshot/source-page"), {
		cursor = cursor,
		maxInstances = RequestPolicy.sourcePageInstances(metadataOnly),
		maxBytes = 8 * 1024 * 1024,
		metadataOnly = metadataOnly == true,
		mappingValues = mappingValues == true,
	}):andThen(function(response)
		return response:decode()
	end)
end

function Client:read(): Promise.TypedPromise<Types.Message>
	Log.trace("Client request: read")

	local url = self:getUrl("read")

	return Http.post(url, {
		clientId = self.clientId,
	})
		:andThen(function(response)
			return response:decode()
		end)
		:catch(function(err)
			-- We don't need to handle this as Rust queue uses `recv_timeout`
			if err == Error.Timedout then
				return self:read()
			else
				return Promise.reject(err)
			end
		end)
end

function Client:reportWarnings(warnings: { string }): Promise.Promise
	if #warnings == 0 then
		return Promise.resolve()
	end
	return Http.post(self:getUrl("diagnostics/warnings"), {
		clientId = self.clientId,
		warnings = warnings,
	})
end

function Client:requestManifestCapture(
	onStatus: ((status: Types.ManifestCaptureStatus) -> ())?,
	managedReloadTransitionId: string?
): Promise.TypedPromise<Types.ManifestCaptureStatus>
	Log.trace("Client request: Capture Manifest")
	local query = {}
	if managedReloadTransitionId ~= nil then
		table.insert(query, `managedReloadTransitionId={HttpService:UrlEncode(managedReloadTransitionId)}`)
	end
	local endpoint = "capture/request"
	if #query > 0 then
		endpoint ..= `?{table.concat(query, "&")}`
	end
	return Http.post(self:getUrl(endpoint)):andThen(function(response)
		local started = HttpService:JSONDecode(response.Body) :: Types.ManifestCaptureStatus
		local lastMessage = nil
		local function poll(status: Types.ManifestCaptureStatus)
			if onStatus then
				onStatus(status)
			end
			if status.message ~= nil and status.message ~= lastMessage then
				lastMessage = status.message
				Log.info("Capture Manifest:", status.message)
			end
			if status.state == "complete" then
				return status
			elseif status.state == "failed" then
				return Promise.reject(Error.fromMessage(status.message or "Capture Manifest failed without a reason"))
			elseif status.state ~= "running" then
				return Promise.reject(Error.fromMessage(`Capture Manifest returned unknown state {status.state}`))
			end
			return Promise.delay(0.1)
				:andThen(function()
					return Http.get(self:getUrl(`capture/status/{started.requestId}`))
				end)
				:andThen(function(nextResponse)
					return poll(HttpService:JSONDecode(nextResponse.Body) :: Types.ManifestCaptureStatus)
				end)
		end
		return poll(started)
	end)
end

function Client:startAutomaticManifestCapture(): Promise.Promise
	Log.trace("Client request: start automatic Studio auto-recovery capture")
	return Http.post(self:getUrl("capture/automatic"))
end

return Client
