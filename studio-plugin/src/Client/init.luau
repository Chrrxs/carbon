local Carbon = script:FindFirstAncestor("Carbon")
local RuntimeServices = require(Carbon.RuntimeServices)
local HttpService = RuntimeServices.get("HttpService") :: HttpService
local CoreGui = RuntimeServices.get("CoreGui") :: CoreGui

local Promise = require(Carbon.Packages.Promise)
local MsgPack = require(Carbon.Lib.MsgPack)

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

function Client:getPrivilegedCapabilities(): Promise.Promise
	return Http.post(self:getUrl("privileged/capabilities"), {
		clientId = self.clientId,
	}):andThen(function(response)
		return response:decode()
	end)
end

function Client:attachManagedHierarchy(): Promise.Promise
	return Http.post(self:getUrl("privileged/managed/attach"), {
		clientId = self.clientId,
	}):andThen(function(response)
		return response:decode()
	end)
end

function Client:bootstrapManifestIdentities(): Promise.Promise
	return Http.post(self:getUrl("privileged/manifest-identities/bootstrap"), {
		clientId = self.clientId,
	}):andThen(function(response)
		return response:decode()
	end)
end

function Client:resolveManagedIdentities(sourceIds: { Types.Ref }, debugIds: { string }?): Promise.Promise
	local requestId = HttpService:GenerateGUID(false)
	local function decode(response)
		return response:decode()
	end
	local function poll(response)
		if response.pending ~= true then
			return response
		end
		-- The bridge must be able to run its queued engine task outside the
		-- originating Luau HTTP request. Schedule polling on a fresh Promise turn
		-- instead of yielding inside this continuation.
		return Promise.delay(0):andThen(function()
			return Http.post(self:getUrl("privileged/managed/resolve/poll"), {
				clientId = self.clientId,
				requestId = requestId,
			})
				:andThen(decode)
				:andThen(poll)
		end)
	end
	return Http.post(self:getUrl("privileged/managed/resolve"), {
		clientId = self.clientId,
		requestId = requestId,
		sourceIds = sourceIds,
		debugIds = debugIds or {},
	})
		:andThen(decode)
		:andThen(poll)
end

function Client:readPrivilegedProperty(debugId: string, property: string): Promise.Promise
	return Http.post(self:getUrl("privileged/property/read"), {
		clientId = self.clientId,
		debugId = debugId,
		property = property,
	}):andThen(function(response)
		return response:decode()
	end)
end

function Client:readPrivilegedProperties(requests: { { debugId: string, property: string } }): Promise.Promise
	return Http.post(self:getUrl("privileged/properties/read"), {
		clientId = self.clientId,
		requests = requests,
	}):andThen(function(response)
		return response:decode()
	end)
end

function Client:readPrivilegedReferences(requests: { { debugId: string, property: string } }): Promise.Promise
	return Http.post(self:getUrl("privileged/references/read"), {
		clientId = self.clientId,
		requests = requests,
	}):andThen(function(response)
		return response:decode()
	end)
end

function Client:readPrivilegedDefaults(className: string, properties: { string }): Promise.Promise
	return Http.post(self:getUrl("privileged/defaults/read"), {
		clientId = self.clientId,
		className = className,
		properties = properties,
	}):andThen(function(response)
		return response:decode()
	end)
end

function Client:writePrivilegedProperty(debugId: string, property: string, value: string): Promise.Promise
	return Http.post(self:getUrl("privileged/property/write"), {
		clientId = self.clientId,
		debugId = debugId,
		property = property,
		value = value,
	})
end

function Client:writePrivilegedReference(debugId: string, property: string, targetDebugId: string?): Promise.Promise
	return Http.post(self:getUrl("privileged/reference/write"), {
		clientId = self.clientId,
		debugId = debugId,
		property = property,
		targetDebugId = targetDebugId,
	})
end

function Client:copyPrivilegedProperty(sourceDebugId: string, targetDebugId: string, property: string): Promise.Promise
	return Http.post(self:getUrl("privileged/property/copy"), {
		clientId = self.clientId,
		sourceDebugId = sourceDebugId,
		targetDebugId = targetDebugId,
		property = property,
	})
end

function Client:materializePrivilegedProperty(
	className: string,
	property: string,
	dataType: string,
	value: buffer
): Promise.Promise
	return Http.post(self:getUrl("privileged/property/materialize"), {
		clientId = self.clientId,
		className = className,
		property = property,
		dataType = dataType,
		value = value,
	}):andThen(function(response)
		return response:decode()
	end)
end

function Client:writeMaterializedPrivilegedProperty(
	debugId: string,
	className: string,
	property: string,
	dataType: string,
	value: buffer
): Promise.Promise
	return Http.post(self:getUrl("privileged/property/materialized-write"), {
		clientId = self.clientId,
		debugId = debugId,
		className = className,
		property = property,
		dataType = dataType,
		value = value,
	})
end

function Client:createPrivilegedInstance(className: string, parentDebugId: string, name: string): Promise.Promise
	return Http.post(self:getUrl("privileged/instance/create"), {
		clientId = self.clientId,
		className = className,
		parentDebugId = parentDebugId,
		name = name,
	}):andThen(function(response)
		return response:decode()
	end)
end

function Client:getPrivilegedRoots(): Promise.Promise
	return Http.post(self:getUrl("privileged/roots"), {
		clientId = self.clientId,
	}):andThen(function(response)
		return response:decode()
	end)
end

function Client:getPrivilegedRootSnapshots(
	debugIds: { string },
	parent: Types.Ref,
	sourceGeneration: string,
	rootSourceIds: { [string]: Types.Ref }?,
	sourceInstanceIds: { [string]: Types.Ref }?
): Promise.Promise
	return Http.post(self:getUrl("privileged/roots/snapshots"), {
		clientId = self.clientId,
		sourceGeneration = sourceGeneration,
		debugIds = debugIds,
		parent = parent,
		rootSourceIds = MsgPack.Map.new(table.clone(rootSourceIds or {})),
		sourceInstanceIds = MsgPack.Map.new(table.clone(sourceInstanceIds or {})),
	}):andThen(function(response)
		return response:decode()
	end)
end

function Client:applyPrivilegedRoots(
	rootSourceGeneration: string,
	roots,
	knownSourceInstances,
	externalReferences,
	preflight: boolean?,
	preflightToken: string?
): Promise.Promise
	return Http.post(self:getUrl("privileged/roots/apply"), {
		clientId = self.clientId,
		sourceGeneration = rootSourceGeneration,
		preflight = preflight == true,
		preflightToken = preflightToken,
		roots = roots,
		knownSourceInstances = knownSourceInstances or {},
		externalReferences = externalReferences or {},
	}):andThen(function(response)
		return response:decode()
	end)
end

function Client:readPrivilegedChanges(after: number): Promise.Promise
	return Http.post(self:getUrl("privileged/changes"), {
		clientId = self.clientId,
		after = after,
	}):andThen(function(response)
		return response:decode()
	end)
end

function Client:requestManifestCapture(
	onStatus: ((status: Types.ManifestCaptureStatus) -> ())?,
	forceFull: boolean?
): Promise.TypedPromise<Types.ManifestCaptureStatus>
	Log.trace("Client request: Capture Manifest")
	local endpoint = if forceFull == true then "capture/request?forceFull=true" else "capture/request"
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

return Client
