--!strict

local fs = require("@lune/fs")

local core = fs.readFile("src/Core/init.luau")
local client = fs.readFile("src/Client/init.luau")

assert(
	not string.find(core, "Error.fromMessage", 1, true),
	"Core startup failures must use the Core error module's structured constructors"
)
assert(not string.find(client, "forceFull", 1, true), "recovery capture must not retain obsolete full-scan modes")

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
local automaticCapture = assert(
	string.find(core, "self.client:startAutomaticManifestCapture():expect()", connectedBoundary, true),
	"successful startup must enable continuous Studio auto-recovery capture"
)
local syncLoop = assert(string.find(core, "return self:__startSyncLoop():expect()", automaticCapture, true))
assert(
	connectedBoundary < automaticCapture and automaticCapture < syncLoop,
	"automatic capture must start only after initial or replacement mapping reconciliation completes"
)

local syncDetailsStart = assert(string.find(core, 'elseif kind == "SyncDetails"', 1, true))
local syncDetailsEnd = assert(string.find(core, "self.__sync(kind, data)", syncDetailsStart, true))
local syncDetails = string.sub(core, syncDetailsStart, syncDetailsEnd)
local advanceGeneration = assert(string.find(syncDetails, "self:__advanceSourceGeneration", 1, true))
local verifyDetails = assert(string.find(syncDetails, "self:__verifyProject(data):expect()", 1, true))
assert(
	advanceGeneration < verifyDetails,
	"automatic capture details must advance the authenticated generation before contract verification"
)

local transitionStartup = assert(
	string.find(core, "local managedReloadTransitionId =", 1, true),
	"managed reconnect startup must detect the pending transition"
)
local transitionReconcile = assert(
	string.find(core, "self:__reconcileMappings()", transitionStartup, true),
	"managed reconnect must reconcile the replacement filesystem mappings"
)
local transitionCapture = assert(
	string.find(core, "requestManifestCapture(nil, managedReloadTransitionId)", transitionReconcile, true),
	"managed reconnect must prove the candidate contract with a transition-bound capture"
)
assert(
	transitionReconcile < transitionCapture,
	"managed reload must materialize mappings and then wait for the recovery-backed capture"
)

print("PASS: Core startup preserves failures and proves managed reload topology before commit")
