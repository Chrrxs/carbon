local TransientMetadata = {}

local WORKSPACE_ATTRIBUTE_NAMES = {
	__StudioWorktree_CarbonEndpoint = true,
	__StudioWorktree_CarbonProject = true,
	__StudioWorktree_CarbonGeneration = true,
	__StudioWorktree_StudioChangeGeneration = true,
	__StudioWorktree_Identity = true,
	__StudioWorktree_Session = true,
}

local managedWorkspace = false
local authoredAttributes = setmetatable({}, { __mode = "k" })
local transportAttributes = {}
local pendingTransportWrites = {}
local mcpPlaceIdOwner = nil
local mcpPlaceIdTransport = nil

local STUDIO_RUNTIME_ROOT_SERVICES = {
	ControllerService = true,
	CoreGui = true,
	GamepadService = true,
	MemStorageService = true,
	PluginConnectionService = true,
	PluginGuiService = true,
	Stats = true,
	StylingService = true,
	UIDragDetectorService = true,
	UserInputService = true,
	VisualizationModeService = true,
}

local STUDIO_EDITOR_ONLY_ROOT_SERVICES = {
	CoreGui = true,
	MemStorageService = true,
	PluginConnectionService = true,
	PluginGuiService = true,
	UserInputService = true,
	VisualizationModeService = true,
}

local STUDIO_STYLING_ROOTS = {
	["StudioDesign-4"] = true,
	AvatarSettings = true,
	Explorer = true,
	ExplorerDark = true,
	ExplorerLight = true,
	AssetImporter = true,
	KnowledgeTutorials = true,
	ManageCollaborators = true,
}

function TransientMetadata.isRoot(className: string, name: string): boolean
	return (name == className and STUDIO_RUNTIME_ROOT_SERVICES[className] == true)
		or ((className == "Instance" or className == "") and name == "FilteredSelection")
end

function TransientMetadata.isEditorOnlyRoot(className: string, name: string): boolean
	return (name == className and STUDIO_EDITOR_ONLY_ROOT_SERVICES[className] == true)
		or ((className == "Instance" or className == "") and name == "FilteredSelection")
end

function TransientMetadata.configure(
	isManagedWorkspace: boolean,
	currentWorkspaceAttributes: { [string]: any }?,
	serverStorage: Instance?,
	currentServerStorageAttributes: { [string]: any }?
)
	managedWorkspace = isManagedWorkspace
	table.clear(authoredAttributes)
	table.clear(transportAttributes)
	table.clear(pendingTransportWrites)
	mcpPlaceIdOwner = nil
	mcpPlaceIdTransport = nil
	if isManagedWorkspace then
		for attributeName in WORKSPACE_ATTRIBUTE_NAMES do
			transportAttributes[attributeName] = (currentWorkspaceAttributes or {})[attributeName]
		end
		local currentMcpPlaceId = (currentServerStorageAttributes or {}).__MCPPlaceId
		mcpPlaceIdOwner = serverStorage
		if type(currentMcpPlaceId) == "string" and currentMcpPlaceId ~= "" then
			mcpPlaceIdTransport = currentMcpPlaceId
			transportAttributes.__MCPPlaceId = currentMcpPlaceId
		end
	end
end

function TransientMetadata.claimMcpPlaceIdTransport(serverStorage: Instance?, instanceId: any): boolean
	if not managedWorkspace or serverStorage == nil or type(instanceId) ~= "string" then
		return false
	end
	local anonymousId = string.match(instanceId, "^anon:(.+)$")
	if anonymousId == nil or serverStorage:GetAttribute("__MCPPlaceId") ~= anonymousId then
		return false
	end
	mcpPlaceIdOwner = serverStorage
	mcpPlaceIdTransport = anonymousId
	transportAttributes.__MCPPlaceId = anonymousId
	return true
end

function TransientMetadata.isAttribute(instance: Instance, attributeName: string): boolean
	return managedWorkspace
		and (
			(instance == workspace and WORKSPACE_ATTRIBUTE_NAMES[attributeName] == true)
			or (instance == mcpPlaceIdOwner and attributeName == "__MCPPlaceId")
		)
end

function TransientMetadata.hasAttributeOverlay(instance: Instance): boolean
	return managedWorkspace and (instance == workspace or instance == mcpPlaceIdOwner)
end

function TransientMetadata.rememberAuthoredAttributes(instance: Instance, attributes: { [string]: any })
	local remembered = {}
	for attributeName, value in attributes do
		if TransientMetadata.isAttribute(instance, attributeName) then
			remembered[attributeName] = value
		end
	end
	authoredAttributes[instance] = remembered
end

function TransientMetadata.consumeAttributeChange(instance: Instance, attributeName: string): boolean
	if not TransientMetadata.isAttribute(instance, attributeName) then
		return true
	end
	if instance == mcpPlaceIdOwner and attributeName == "__MCPPlaceId" and mcpPlaceIdTransport == nil then
		local value = instance:GetAttribute(attributeName)
		if
			type(value) == "string"
			and string.match(value, "^[0-9a-fA-F]+%-[0-9a-fA-F]+%-[0-9a-fA-F]+%-[0-9a-fA-F]+%-[0-9a-fA-F]+$")
		then
			mcpPlaceIdTransport = value
			transportAttributes.__MCPPlaceId = value
			return false
		end
	end
	if (pendingTransportWrites[attributeName] or 0) > 0 then
		return false
	end
	local value = instance:GetAttribute(attributeName)
	if value == transportAttributes[attributeName] then
		return false
	end
	local remembered = authoredAttributes[instance] or {}
	remembered[attributeName] = value
	authoredAttributes[instance] = remembered
	TransientMetadata.setTransportAttribute(instance, attributeName, transportAttributes[attributeName])
	return true
end

function TransientMetadata.setTransportAttribute(instance: Instance, attributeName: string, value: any)
	assert(
		TransientMetadata.isAttribute(instance, attributeName),
		"transport attribute is outside managed Workspace scope"
	)
	transportAttributes[attributeName] = value
	pendingTransportWrites[attributeName] = (pendingTransportWrites[attributeName] or 0) + 1
	instance:SetAttribute(attributeName, value)
	pendingTransportWrites[attributeName] -= 1
end

function TransientMetadata.isInstance(
	className: string,
	name: string,
	parentClassName: string,
	archivable: boolean?
): boolean
	-- FilteredSelection is an internal Studio root. Depending on which Studio
	-- reflection path observed it, Roblox reports its class as either Instance
	-- or the empty string, so its stable editor-only name is the discriminator.
	if
		parentClassName == "DataModel"
		and name == "FilteredSelection"
		and (className == "Instance" or className == "")
	then
		return true
	end

	-- CoreGui and PluginGuiService are editor/Roblox UI ownership domains. Their
	-- descendants cannot participate in an experience's authored gameplay state.
	if parentClassName == "CoreGui" or parentClassName == "PluginGuiService" then
		return true
	end
	if parentClassName == "StylingService" and STUDIO_STYLING_ROOTS[name] then
		return true
	end
	if
		parentClassName == "GuiService"
		and (
			(className == "ScreenshotHud" and name == "ScreenshotHud")
			or (className == "LocalizationTable" and name == "FoundationLocalization" and archivable == false)
		)
	then
		return true
	end
	if parentClassName == "Terrain" then
		return archivable == false
			and (
				(className == "Folder" and name == "DO_NOT_REMOVE_ForgeSharedPartCache")
				or (className == "Part" and name == "DO_NOT_REMOVE_ForgeTextureCache")
			)
	end

	-- Studio creates another internal controller every time a place containing
	-- ControllerService is opened. These children are runtime controller state,
	-- not authored instances, and persisting them grows the place on every launch.
	return archivable == false
		and parentClassName == "ControllerService"
		and className == "HumanoidController"
		and name == "Instance"
end

function TransientMetadata.stripAttributes(instance: Instance, attributes: { [string]: any }): { [string]: any }
	if
		instance == mcpPlaceIdOwner
		and attributes.__MCPPlaceId ~= nil
		and attributes.__MCPPlaceId ~= mcpPlaceIdTransport
	then
		local remembered = authoredAttributes[instance] or {}
		remembered.__MCPPlaceId = attributes.__MCPPlaceId
		authoredAttributes[instance] = remembered
	end
	for attributeName in attributes do
		if
			(managedWorkspace and instance == workspace and WORKSPACE_ATTRIBUTE_NAMES[attributeName] == true)
			or (
				instance == mcpPlaceIdOwner
				and attributeName == "__MCPPlaceId"
				and attributes[attributeName] == mcpPlaceIdTransport
			)
		then
			attributes[attributeName] = nil
		end
	end
	for attributeName, value in authoredAttributes[instance] or {} do
		attributes[attributeName] = value
	end
	return attributes
end

return TransientMetadata
