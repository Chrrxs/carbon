--!strict

local fs = require("@lune/fs")

local core = fs.readFile("src/Core/init.luau")
local errors = fs.readFile("src/Core/Error.luau")

assert(
	not string.find(core, "Error.fromMessage", 1, true),
	"Core startup failures must use the Core error module's structured constructors"
)
assert(
	string.find(errors, "PrivilegedUnavailable", 1, true)
		and string.find(core, "Error.new(Error.PrivilegedUnavailable, privilegedResult)", 1, true),
	"failed RML negotiation must preserve its last failure in a structured Core error"
)
assert(
	string.find(errors, "ManifestIdentityUnsupported", 1, true)
		and string.find(core, "Error.new(Error.ManifestIdentityUnsupported)", 1, true)
		and string.find(errors, "ManifestIdentityRejected", 1, true)
		and string.find(core, "Error.new(Error.ManifestIdentityRejected)", 1, true),
	"manifest capability failures must reject with structured Core errors"
)

local managedReloadStart = assert(string.find(core, 'elseif kind == "ManagedReload"', 1, true))
local managedReloadEnd = assert(
	string.find(core, "return reject(Error.new(Error.ManagedReload, data.transitionId))", managedReloadStart, true)
)
local managedReloadHandler = string.sub(core, managedReloadStart, managedReloadEnd)
assert(
	string.find(managedReloadHandler, "self.expectedWorktree.name = data.projectName", 1, true),
	"managed reload must retain the replacement project name for reconnect verification"
)
assert(
	string.find(managedReloadHandler, "self.expectedWorktree.sourceGeneration = data.sourceGeneration", 1, true)
		and not string.find(managedReloadHandler, "self.expectedWorktree.sourceGeneration = nil", 1, true),
	"managed reload must trust the authenticated generation captured before the transition"
)
assert(
	string.find(managedReloadHandler, "self.expectedWorktree.managedReloadTransitionId = data.transitionId", 1, true),
	"managed reload must carry its transition identity across the reconnect"
)
assert(
	not string.find(managedReloadHandler, "__commitManagedProjectName", 1, true)
		and not string.find(managedReloadHandler, "__StudioWorktree_CarbonProject", 1, true),
	"managed reload must not advertise the replacement project before reconnect succeeds"
)

local commitProjectName = assert(string.find(core, "self:__commitManagedProjectName(project.name)", 1, true))
local connectedBoundary = assert(string.find(core, "self.status = Core.Status.Connected", commitProjectName, true))
assert(
	commitProjectName < connectedBoundary,
	"the replacement project transport name must commit at the successful reconnect boundary"
)

local transitionStartup = assert(
	string.find(core, "local managedReloadTransitionId =", 1, true),
	"managed reconnect startup must detect the pending transition"
)
local transitionReconcile = assert(
	string.find(core, "self:__reconcileMappings()", transitionStartup, true),
	"managed reconnect must reconcile the replacement filesystem mappings"
)
local transitionBootstrap = assert(
	string.find(core, "bootstrapManifestIdentities(managedReloadTransitionId)", transitionReconcile, true),
	"managed reconnect must recover native manifest identity authority when it was lost"
)
local transitionCapture = assert(
	string.find(core, "requestManifestCapture(nil, true, managedReloadTransitionId)", transitionBootstrap, true),
	"managed reconnect must prove the candidate contract with a transition-bound capture"
)
assert(
	transitionReconcile < transitionBootstrap and transitionBootstrap < transitionCapture,
	"managed reload must materialize mappings, refresh native identity authority, and then capture the candidate"
)
assert(
	(
		string.find(
			core,
			"if privilegedCapabilities.manifestIdentitiesAuthoritative ~= true then",
			transitionReconcile,
			true
		) or math.huge
	) > transitionCapture,
	"managed reload must refresh an authoritative ledger because the replacement contract can add identities"
)

print("PASS: Core startup preserves failures and proves managed reload topology before commit")
