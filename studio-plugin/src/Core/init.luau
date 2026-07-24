local Carbon = script:FindFirstAncestor("Carbon")

local Promise = require(Carbon.Packages.Promise)

local Client = require(Carbon.Client)
local Config = require(Carbon.Config)
local Dom = require(Carbon.Lib.Dom)
local Executor = require(Carbon.Executor)
local Log = require(Carbon.Log)
local TransientMetadata = require(Carbon.TransientMetadata)
local Types = require(Carbon.Types)
local manifest = require(Carbon.manifest)

local Error = require(script.Error)
local InitialReconciliation = require(script.InitialReconciliation)
local MappingReconciler = require(script.MappingReconciler)
local PrivilegedNegotiation = require(script.PrivilegedNegotiation)

local PROTOCOL_VERSION = 4

local Core = {
	Status = {
		Disconnected = 0,
		Connecting = 1,
		Connected = 2,
		Disconnecting = 3,
	},
}
Core.__index = Core

function Core.new(host: string?, port: number?, expectedWorktree: Types.Project?)
	local self = setmetatable({}, Core)
	self.project = nil
	self.status = Core.Status.Disconnected
	self.expectedWorktree = expectedWorktree
	self.client = Client.new(host or Config.HOST, port or Config:get("Port"))
	self.executor = Executor.new()
	self.reconciler = MappingReconciler.new()
	self.tree = self.reconciler.tree
	self.privilegedCapabilities = nil

	local managedWorkspace = expectedWorktree ~= nil and expectedWorktree.worktreeId ~= nil
	local serverStorage = game:FindFirstChildOfClass("ServerStorage")
	TransientMetadata.configure(
		managedWorkspace,
		if managedWorkspace then workspace:GetAttributes() else nil,
		serverStorage,
		if managedWorkspace and serverStorage then serverStorage:GetAttributes() else nil
	)

	self.__prompt = function(_message: string, _changes: Types.Changes?, _tree: any): boolean
		return true
	end
	self.__ready = function(_project: Types.Project) end
	self.__sync = function(_kind: Types.MessageKind, _data: any) end
	return self
end

function Core:run(): Promise.Promise
	return Promise.new(function(_, reject)
		self.status = Core.Status.Connecting
		-- selene: allow(undefined_variable)
		Dom.assertReflectionVersion(version())

		Log.trace("Fetching server details..")
		local project = self.client:fetchDetails():expect()
		self:__verifyProject(project):expect()
		self.project = project

		local trustedBuiltPlace = InitialReconciliation.isTrustedBuiltPlace(project, self.expectedWorktree)
		if not self.expectedWorktree then
			local confirmed = self.__prompt(
				`Attach this open Studio place to Carbon project "{project.name}"? Filesystem mappings will be applied authoritatively.`,
				nil
			)
			if not confirmed then
				return reject(Error.new(Error.Terminated))
			end
		end

		Log.trace("Subscribing to the server queue..")
		self.client:subscribe():expect()
		local serverStorage = game:FindFirstChildOfClass("ServerStorage")
		TransientMetadata.claimMcpPlaceIdTransport(serverStorage, self.client.instanceId)

		-- Negotiation binds this Studio session to its RML bridge for explicit
		-- native capture. It does not attach, verify, or watch the hierarchy.
		local privilegedSuccess, privilegedResult = PrivilegedNegotiation.run(function()
			local capabilities = self.client:getPrivilegedCapabilities():expect()
			if
				capabilities.studioSessionId ~= self.client.studioSessionId
				or capabilities.instanceId ~= self.client.instanceId
			then
				error("RML bridge routing identity mismatch", 0)
			end
			return capabilities
		end, function(delay)
			task.wait(delay)
		end)
		if not privilegedSuccess then
			Dom.configurePrivileged(nil, nil)
			return reject(Error.new(Error.PrivilegedUnavailable, privilegedResult))
		end
		local privilegedCapabilities = privilegedResult
		Dom.configurePrivileged(self.client, privilegedCapabilities)
		self.privilegedCapabilities = privilegedCapabilities
		if privilegedCapabilities.manifestIdentityLedger ~= true then
			return reject(Error.new(Error.ManifestIdentityUnsupported))
		end

		if trustedBuiltPlace and privilegedCapabilities.manifestIdentitiesAuthoritative ~= true then
			local identityBootstrap = self.client:bootstrapManifestIdentities():expect()
			if identityBootstrap.authoritative ~= true then
				return reject(Error.new(Error.ManifestIdentityRejected))
			end
			Log.trace("Binding generation-matched mapped source identities without hierarchy verification..")
			local report = self.reconciler:prime(self.client, self.project)
			self.tree = self.reconciler.tree
			Log.debug("Attached built mapping identities:", report.instances, "of", report.expected, "instances")
		else
			Log.trace("Reconciling frozen filesystem mappings into the confirmed open place..")
			local report = self:__reconcileMappings()
			self.tree = self.reconciler.tree
			Log.debug("Initial mapping reconciliation:", report.instances, "instances")
		end

		if self.status ~= Core.Status.Connecting then
			return reject(Error.new(Error.Terminated))
		end
		self.status = Core.Status.Connected
		self.__ready(project)
		Log.trace("Starting filesystem sync loop..")
		return self:__startSyncLoop():expect()
	end)
end

function Core:stop()
	if self.status == Core.Status.Disconnected or self.status == Core.Status.Disconnecting then
		return
	end
	Log.trace("Stopping Carbon core..")
	self.status = Core.Status.Disconnecting
	Dom.configurePrivileged(nil, nil)
	if self.client.isSubscribed then
		task.spawn(function()
			self.client:unsubscribe():catch(function(err)
				Log.debug("Failed to unsubscribe from the server", err)
			end)
		end)
	end
end

function Core:onPrompt(callback: (message: string, changes: Types.Changes?, tree: any) -> boolean)
	self.__prompt = function(message, changes)
		if self.status == Core.Status.Disconnecting then
			return false
		end
		return callback(message, changes, self.tree)
	end
end

function Core:onReady(callback: (project: Types.Project) -> ())
	self.__ready = function(project)
		if self.status ~= Core.Status.Disconnecting then
			return callback(project)
		end
	end
end

function Core:onSync(callback: (kind: Types.MessageKind, data: any) -> ())
	self.__sync = function(kind, data)
		if self.status ~= Core.Status.Disconnecting then
			return callback(kind, data)
		end
	end
end

function Core:captureManifest(
	onStatus: ((status: Types.ManifestCaptureStatus) -> ())?
): Promise.TypedPromise<Types.ManifestCaptureStatus>
	if self.status ~= Core.Status.Connected then
		return Promise.reject(Error.new(Error.Terminated))
	end
	return self.client:requestManifestCapture(onStatus):andThen(function(status)
		self:__advanceSourceGeneration(status.sourceGeneration)
		return self.client:fetchDetails():andThen(function(project)
			self:__verifyProject(project):expect()
			self.project = project
			self.__sync("SyncDetails", project)
			return status
		end)
	end)
end

function Core:__reconcileMappings()
	-- Filesystem mappings only contain folders and scripts. Their live-sync
	-- surface must not depend on native manifest access, bridge attachment, or
	-- protected instance properties. Temporarily expose exactly the ordinary
	-- Studio property surface while producing and applying the mapped diff.
	Dom.configurePrivileged(nil, nil)
	local success, result = pcall(function()
		return self.reconciler:reconcile(self.client, self.project)
	end)
	Dom.configurePrivileged(self.client, self.privilegedCapabilities)
	if not success then
		error(result, 0)
	end
	if #result.warnings > 0 then
		-- The Studio console remains the immediate local diagnostic, while this
		-- authenticated report makes the same warning explicit in the owning
		-- `carbon serve` terminal and in terminal-based harnesses.
		self.client:reportWarnings(result.warnings):expect()
	end
	return result
end

function Core.wasExitGraceful(err: Error.Error)
	return err == Error.TooManyChanges or err == Error.Terminated or err.kind == "ConnectionClosed"
end

function Core:__verifyProject(project: Types.Project): Promise.Promise
	if project.protocolVersion ~= PROTOCOL_VERSION then
		local serverVersion = project.protocolVersion == nil and "missing" or project.protocolVersion
		return Promise.reject(Error.new(Error.ProtocolVersion, PROTOCOL_VERSION, serverVersion))
	end
	if type(project.mappedRootRefs) ~= "table" then
		return Promise.reject(Error.new(Error.WorktreeContract, "server omitted mapped filesystem roots"))
	end
	for _, id in project.mappedRootRefs do
		if typeof(id) ~= "buffer" then
			return Promise.reject(Error.new(Error.WorktreeContract, "server returned an invalid mapped root identity"))
		end
	end
	if project.version ~= manifest.package.version then
		return Promise.reject(Error.new(Error.Version, manifest.package.version, project.version))
	end
	if self.expectedWorktree then
		local expected = self.expectedWorktree
		if project.name ~= expected.name then
			return Promise.reject(Error.new(Error.WorktreeContract, `project {project.name} != {expected.name}`))
		end
		if project.worktreeId ~= expected.worktreeId then
			return Promise.reject(Error.new(Error.WorktreeContract, "worktree identity does not match"))
		end
		if project.sessionToken ~= expected.sessionToken then
			return Promise.reject(Error.new(Error.WorktreeContract, "session token does not match"))
		end
		if expected.sourceGeneration ~= nil and project.sourceGeneration ~= expected.sourceGeneration then
			return Promise.reject(
				Error.new(Error.WorktreeContract, "source generation does not match; rebuild the place")
			)
		end
	end
	return Promise.resolve()
end

function Core:__advanceSourceGeneration(sourceGeneration: string)
	assert(type(sourceGeneration) == "string" and sourceGeneration ~= "", "SyncChanges omitted sourceGeneration")
	if self.project then
		self.project.sourceGeneration = sourceGeneration
	end
	if not self.expectedWorktree then
		return
	end
	self.expectedWorktree.sourceGeneration = sourceGeneration
	if type(self.expectedWorktree.updateGeneration) == "function" then
		self.expectedWorktree.updateGeneration(sourceGeneration)
	elseif self.expectedWorktree.worktreeId ~= nil then
		TransientMetadata.setTransportAttribute(workspace, "__StudioWorktree_CarbonGeneration", sourceGeneration)
	end
end

function Core:__startSyncLoop()
	return Promise.new(function(resolve, reject)
		while self.status == Core.Status.Connected do
			local message = self.client:read():expect() :: Types.Message?
			if not message then
				continue
			end
			local kind = next(message) :: Types.MessageKind
			local data = message[kind] :: any
			Log.trace("Received message:", kind)

			if kind == "SyncChanges" then
				-- The sparse server diff is a damage notification. Studio application
				-- always reconciles from fresh full source pages, so pre-existing drift
				-- cannot survive merely because the changed property was elsewhere.
				local report = self:__reconcileMappings()
				self.tree = self.reconciler.tree
				self:__advanceSourceGeneration(data.sourceGeneration)
				self.__sync(kind, report)
			elseif kind == "SyncDetails" then
				self:__verifyProject(data):expect()
				self.project = data
				self.__sync(kind, data)
			elseif kind == "RestartRequired" then
				self.status = Core.Status.Disconnecting
				return reject(Error.new(Error.RestartRequired, data.message))
			elseif kind == "ExecuteCode" then
				self.executor:execute(data.code)
			elseif kind == "Disconnect" then
				return reject(Error.new(Error.Disconnected, data.message))
			else
				return reject(Error.new(Error.UnknownEvent, kind, data))
			end
		end
		resolve()
	end)
end

return Core
