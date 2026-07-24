local ManagedWorktree = require("../src/ManagedWorktree")
local TransientMetadata = require("../src/TransientMetadata")

assert(
	ManagedWorktree.resolve("http://127.0.0.1:8000", "Project", "worktree", "token") ~= nil,
	"the default serve port must auto-connect managed launches"
)
assert(
	ManagedWorktree.resolve("http://example.com:41000", "Project", "worktree", "token") == nil,
	"non-loopback endpoint must be rejected"
)
assert(
	ManagedWorktree.resolve("http://127.0.0.1:41000", "", "worktree", "token") == nil,
	"incomplete worktree contract must be rejected"
)

local managed = ManagedWorktree.resolve("http://127.0.0.1:41000", "Project", "worktree", "token", "source-generation")
assert(managed ~= nil, "managed endpoint should resolve")
assert(managed.host == "127.0.0.1", "host")
assert(managed.port == 41000, "port")
assert(managed.project.name == "Project", "project")
assert(managed.project.worktreeId == "worktree", "identity")
assert(managed.project.sessionToken == "token", "token")
assert(managed.project.sourceGeneration == "source-generation", "source generation")

local workspaceAttributes = {
	__StudioWorktree_CarbonEndpoint = "http://127.0.0.1:41000",
	__StudioWorktree_CarbonProject = "Project",
	__StudioWorktree_CarbonGeneration = "source-generation",
	__StudioWorktree_Identity = "worktree",
	__StudioWorktree_Session = "token",
}
local lastTransportEcho = nil
local deferAttributeCallbacks = false
local deferredAttribute = nil
local workspaceInstance
workspaceInstance = {
	GetAttribute = function(_, name)
		return workspaceAttributes[name]
	end,
	SetAttribute = function(_, name, value)
		workspaceAttributes[name] = value
		if deferAttributeCallbacks then
			deferredAttribute = name
		else
			lastTransportEcho = TransientMetadata.consumeAttributeChange(workspaceInstance, name)
		end
	end,
}
local serverStorage = {}
workspace = workspaceInstance
game = {
	GetService = function()
		error("transient metadata must not materialize a service")
	end,
}

TransientMetadata.configure(true, workspaceAttributes)
TransientMetadata.rememberAuthoredAttributes(workspaceInstance, {
	__StudioWorktree_CarbonEndpoint = "authored-endpoint",
	__StudioWorktree_CarbonProject = "authored-project",
})

local attributes = TransientMetadata.stripAttributes(workspaceInstance, {
	Canonical = "keep",
	__MCPPlaceId = "session-id",
	__StudioWorktree_CarbonEndpoint = "http://127.0.0.1:41000",
	__StudioWorktree_CarbonProject = "Project",
	__StudioWorktree_CarbonGeneration = "source-generation",
	__StudioWorktree_Identity = "worktree",
	__StudioWorktree_Session = "token",
})
assert(attributes.Canonical == "keep", "canonical attributes must remain")
assert(attributes.__MCPPlaceId == "session-id", "ServerStorage metadata names must remain authored on Workspace")
assert(
	attributes.__StudioWorktree_CarbonEndpoint == "authored-endpoint",
	"managed Workspace metadata must expose the authored source value during syncback"
)
assert(
	attributes.__StudioWorktree_CarbonProject == "authored-project",
	"every colliding authored Workspace value must survive the managed session"
)
assert(attributes.__StudioWorktree_CarbonGeneration == nil, "unowned managed metadata must be stripped")

workspaceAttributes.__StudioWorktree_CarbonEndpoint = "user-authored-endpoint"
assert(
	TransientMetadata.consumeAttributeChange(workspaceInstance, "__StudioWorktree_CarbonEndpoint"),
	"a user edit colliding with managed routing metadata must emit an authored Attributes change"
)
assert(
	workspaceAttributes.__StudioWorktree_CarbonEndpoint == "http://127.0.0.1:41000",
	"the routing marker must be restored independently of the authored collision"
)
local editedAttributes = TransientMetadata.stripAttributes(workspaceInstance, workspaceAttributes)
assert(
	editedAttributes.__StudioWorktree_CarbonEndpoint == "user-authored-endpoint",
	"syncback must expose the latest authored collision rather than the launch-time value"
)
TransientMetadata.setTransportAttribute(workspaceInstance, "__StudioWorktree_CarbonGeneration", "next-generation")
assert(lastTransportEcho == false, "Carbon's own marker advance must not echo into authored source")
deferAttributeCallbacks = true
TransientMetadata.setTransportAttribute(workspaceInstance, "__StudioWorktree_CarbonGeneration", "deferred-generation")
deferAttributeCallbacks = false
assert(
	deferredAttribute == "__StudioWorktree_CarbonGeneration"
		and not TransientMetadata.consumeAttributeChange(workspaceInstance, deferredAttribute),
	"deferred Workspace signals must still recognize Carbon's current transport value"
)

local currentMcpPlaceId = "mcp-transport-id"
serverStorage.GetAttribute = function(_, name)
	assert(name == "__MCPPlaceId")
	return currentMcpPlaceId
end

TransientMetadata.configure(true, workspaceAttributes, serverStorage, {})
currentMcpPlaceId = "ab28d6f5-27fe-4d7a-a34e-c1dcfe0671a3"
assert(
	not TransientMetadata.consumeAttributeChange(serverStorage, "__MCPPlaceId"),
	"the first UUID installed by MCP after subscription must be claimed as transport"
)
local lateMcpServerAttributes = TransientMetadata.stripAttributes(serverStorage, {
	__MCPPlaceId = currentMcpPlaceId,
})
assert(lateMcpServerAttributes.__MCPPlaceId == nil, "the late MCP UUID must never enter canonical source")

currentMcpPlaceId = "mcp-transport-id"
TransientMetadata.configure(true, workspaceAttributes)
assert(
	TransientMetadata.claimMcpPlaceIdTransport(serverStorage, `anon:{currentMcpPlaceId}`),
	"a managed anonymous MCP identity that arrives after Core construction must be claimed"
)
assert(
	not TransientMetadata.consumeAttributeChange(serverStorage, "__MCPPlaceId"),
	"the delayed launch identity must not enter canonical source"
)
local delayedTransportServerAttributes = TransientMetadata.stripAttributes(serverStorage, {
	Canonical = "keep",
	__MCPPlaceId = currentMcpPlaceId,
})
assert(
	delayedTransportServerAttributes.Canonical == "keep" and delayedTransportServerAttributes.__MCPPlaceId == nil,
	"the delayed launch identity must be stripped after the exact client route claims it"
)

TransientMetadata.configure(true, workspaceAttributes)
currentMcpPlaceId = "authored-before-claim"
assert(
	not TransientMetadata.claimMcpPlaceIdTransport(serverStorage, "anon:different-route"),
	"a value that does not match the subscribed client route must remain authored"
)
local unclaimedServerAttributes = TransientMetadata.stripAttributes(serverStorage, {
	__MCPPlaceId = currentMcpPlaceId,
})
assert(
	unclaimedServerAttributes.__MCPPlaceId == currentMcpPlaceId,
	"route claiming must not strip a mismatched authored value"
)

currentMcpPlaceId = "mcp-transport-id"
TransientMetadata.configure(true, workspaceAttributes, serverStorage, {
	__MCPPlaceId = currentMcpPlaceId,
})
assert(
	not TransientMetadata.consumeAttributeChange(serverStorage, "__MCPPlaceId"),
	"the launch tool's current ServerStorage identity must not enter canonical source"
)
local transportServerAttributes = TransientMetadata.stripAttributes(serverStorage, {
	Canonical = "keep",
	__MCPPlaceId = currentMcpPlaceId,
})
assert(
	transportServerAttributes.Canonical == "keep" and transportServerAttributes.__MCPPlaceId == nil,
	"the launch identity must be stripped only from its owning ServerStorage"
)

TransientMetadata.rememberAuthoredAttributes(serverStorage, {
	__MCPPlaceId = "authored-place-id",
})
currentMcpPlaceId = "live-authored-edit"
local serverAttributes = TransientMetadata.stripAttributes(serverStorage, {
	Canonical = "keep",
	__MCPPlaceId = "live-authored-edit",
	__StudioWorktree_CarbonEndpoint = "authored-collision",
})
assert(serverAttributes.Canonical == "keep", "canonical ServerStorage attributes must remain")
assert(
	serverAttributes.__MCPPlaceId == "live-authored-edit",
	"an authored __MCPPlaceId edit must not be swallowed as transient metadata"
)
assert(
	serverAttributes.__StudioWorktree_CarbonEndpoint == "authored-collision",
	"Workspace-only metadata names must remain authored on other instances"
)

local otherAttributes = TransientMetadata.stripAttributes({}, {
	__MCPPlaceId = "authored-collision",
	__StudioWorktree_CarbonEndpoint = "authored-collision",
})
assert(otherAttributes.__MCPPlaceId ~= nil, "transient names must not be filtered globally")
assert(otherAttributes.__StudioWorktree_CarbonEndpoint ~= nil, "transient names must be owner-scoped")

TransientMetadata.configure(false)
local unmanagedWorkspaceAttributes = TransientMetadata.stripAttributes(workspaceInstance, {
	__StudioWorktree_CarbonEndpoint = "authored-endpoint",
	__StudioWorktree_Session = "authored-session",
})
assert(
	unmanagedWorkspaceAttributes.__StudioWorktree_CarbonEndpoint == "authored-endpoint"
		and unmanagedWorkspaceAttributes.__StudioWorktree_Session == "authored-session",
	"unmanaged places must preserve every authored tool-name collision"
)

assert(
	TransientMetadata.isInstance("HumanoidController", "Instance", "ControllerService", false),
	"Studio-generated controller state must not enter source"
)
assert(
	not TransientMetadata.isInstance("HumanoidController", "AuthoredController", "ControllerService", true),
	"explicitly named controller content must remain eligible for source"
)
assert(
	not TransientMetadata.isInstance("HumanoidController", "Instance", "ControllerService", true),
	"an authored controller named Instance must remain eligible for source"
)
assert(
	not TransientMetadata.isInstance("Folder", "DO_NOT_REMOVE_ForgeSharedPartCache", "Terrain", true),
	"an authored Terrain child that collides with a cache name must remain eligible for source"
)
assert(
	TransientMetadata.isInstance("Folder", "DO_NOT_REMOVE_ForgeSharedPartCache", "Terrain", false),
	"the non-archivable Studio Terrain cache must remain transient"
)
assert(
	not TransientMetadata.isInstance("UIDragDetectorService", "UIDragDetectorService", "DataModel"),
	"a mapped/authored UIDragDetectorService must participate in ordinary instance sync"
)
assert(
	not TransientMetadata.isInstance("GamepadService", "GamepadService", "DataModel"),
	"a mapped/authored GamepadService must participate in ordinary instance sync"
)
assert(
	TransientMetadata.isRoot("CoreGui", "CoreGui") and TransientMetadata.isRoot("Instance", "FilteredSelection"),
	"nominal runtime roots must remain classifiable only for unmapped-root reconciliation"
)
assert(
	TransientMetadata.isInstance("Folder", "ManageCollaborators", "StylingService", true)
		and not TransientMetadata.isInstance("StyleSheet", "GameplayTheme", "StylingService", true),
	"known Studio styling roots must be transient without hiding authored gameplay styles"
)
assert(
	TransientMetadata.isInstance("DockWidgetPluginGui", "AuthoredNameCollision", "PluginGuiService", true),
	"PluginGuiService descendants are editor-only and must never enter gameplay source"
)
assert(
	TransientMetadata.isInstance("ScreenshotHud", "ScreenshotHud", "GuiService", true)
		and TransientMetadata.isInstance("LocalizationTable", "FoundationLocalization", "GuiService", false)
		and not TransientMetadata.isInstance("LocalizationTable", "GameplayLocalization", "GuiService", true),
	"GuiService's generated screenshot UI must remain transient without hiding authored localization"
)
assert(
	TransientMetadata.isInstance("ScreenGui", "RobloxGui", "CoreGui", true),
	"CoreGui descendants are engine/editor UI and must never enter gameplay source"
)
assert(
	not TransientMetadata.isRoot("AvatarSettings", "AvatarSettings"),
	"authored native-only services must remain eligible"
)
print("managed worktree endpoint policy passed")
