--!strict

local BootstrapRoots = require(script.Parent.BootstrapRoots)

-- User plugins run after Studio installs the edit DataModel, while
-- DataModel:IsLoaded() remains false for local edit sessions. Capture the
-- authored roots now; waiting for the gameplay load signal blocks forever.

local function debugId(instance: Instance): string?
	local success, value = pcall(instance.GetDebugId, instance, 128)
	return if success then value else nil
end

-- This module is required before init.server touches any service. The captured
-- roots therefore separate authored file state from services loaded by Carbon or
-- one of its bundled packages during their module initialization.
local initialDebugIds = BootstrapRoots.index(game:GetChildren(), debugId)
local materializedDebugIds = {}
local initializingDebugIds = {}
local initializationGenerations = {}
local pendingClasses = {}
local pendingArchivableWrites = setmetatable({}, { __mode = "k" })
local UNMANAGED_SERVICE_INITIALIZATION_SECONDS = 2

local function writeRuntimeArchivable(instance: Instance, value: boolean)
	pendingArchivableWrites[instance] = true
	pcall(function()
		instance.Archivable = value
	end)
	task.defer(function()
		pendingArchivableWrites[instance] = nil
	end)
end

local function markMaterialized(instance: Instance)
	local id = debugId(instance)
	if id == nil or initialDebugIds[id] then
		return
	end
	materializedDebugIds[id] = true
	-- A tool-created service must never enter an AutoRecovery/manual save even if
	-- the plugin is disconnected before its initial source audit.
	writeRuntimeArchivable(instance, false)
end

local RuntimeServices = {}

function RuntimeServices.get(className: string): Instance
	local existing = game:FindFirstChildOfClass(className)
	if existing ~= nil then
		return existing
	end
	pendingClasses[className] = (pendingClasses[className] or 0) + 1
	local success, service = pcall(game.GetService, game, className)
	pendingClasses[className] -= 1
	if pendingClasses[className] == 0 then
		pendingClasses[className] = nil
	end
	if not success then
		error(service, 2)
	end
	markMaterialized(service)
	return service
end

function RuntimeServices.sealBootstrap()
	local discovered = BootstrapRoots.materialized(initialDebugIds, game:GetChildren(), debugId, markMaterialized)
	for id in discovered do
		materializedDebugIds[id] = true
	end
end

function RuntimeServices.observeUnmanagedService(instance: Instance)
	if instance.Parent ~= game then
		return
	end
	local id = debugId(instance)
	if id == nil then
		return
	end
	materializedDebugIds[id] = true
	initializingDebugIds[id] = true
	initializationGenerations[id] = (initializationGenerations[id] or 0) + 1
	local generation = initializationGenerations[id]
	writeRuntimeArchivable(instance, false)
	task.delay(UNMANAGED_SERVICE_INITIALIZATION_SECONDS, function()
		if initializationGenerations[id] == generation then
			initializingDebugIds[id] = nil
		end
	end)
end

function RuntimeServices.isMaterialized(instance: Instance): boolean
	local id = debugId(instance)
	return (id ~= nil and materializedDebugIds[id] == true)
		or (instance.Parent == game and (pendingClasses[instance.ClassName] or 0) > 0)
end

function RuntimeServices.isArchivableWritePending(instance: Instance): boolean
	return pendingArchivableWrites[instance] == true
end

function RuntimeServices.isInitializing(instance: Instance): boolean
	local id = debugId(instance)
	return id ~= nil and initializingDebugIds[id] == true
end

function RuntimeServices.promote(instance: Instance)
	local id = debugId(instance)
	if id == nil or not materializedDebugIds[id] then
		return
	end
	materializedDebugIds[id] = nil
	initializingDebugIds[id] = nil
	initializationGenerations[id] = nil
	-- Archivable=false was Carbon's save-isolation overlay, not user state. Restore
	-- Roblox's service default before Read:onAdd captures a genuinely authored
	-- child/property edit. A later explicit user edit remains an ordinary Change.
	writeRuntimeArchivable(instance, BootstrapRoots.promotedArchivable(true, instance.Archivable))
end

function RuntimeServices.initialDebugIds(): { [string]: boolean }
	return table.clone(initialDebugIds)
end

return RuntimeServices
