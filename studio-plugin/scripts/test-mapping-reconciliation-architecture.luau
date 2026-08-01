--!strict

local fs = require("@lune/fs")

local core = fs.readFile("src/Core/init.luau")
local reconciler = fs.readFile("src/Core/MappingReconciler.luau")
local client = fs.readFile("src/Client/init.luau")
local app = fs.readFile("src/App/init.luau")

assert(
	not string.find(core, "Watcher", 1, true)
		and not string.find(core, "Syncback", 1, true)
		and not string.find(core, "attachManagedHierarchy", 1, true),
	"the live Core must not install Studio watchers, sync back, or verify a managed hierarchy"
)
assert(
	not string.find(client, 'getUrl("write/page")', 1, true)
		and not string.find(client, 'getUrl("write/abort")', 1, true),
	"the plugin must expose no Studio-to-server source write endpoint"
)
assert(
	string.find(core, "self.reconciler:reconcile(self.client, self.project)", 1, true)
		and string.find(reconciler, "client:getSnapshotPage(cursor)", 1, true)
		and string.find(reconciler, "client:getSourcePage(cursor, false, true)", 1, true),
	"each filesystem damage notification must reconcile from fresh authoritative hierarchy and property pages"
)
assert(
	string.find(reconciler, "root:GetDescendants()", 1, true)
		and string.find(reconciler, "sibling.Name == snapshotName(snapshot)", 1, true)
		and string.find(reconciler, "Removed Studio-only instance from filesystem mapping", 1, true),
	"reconciliation must remove and explicitly warn for invalid descendants and mapping-root collisions"
)
assert(
	string.find(core, "self.client:reportWarnings(result.warnings):expect()", 1, true)
		and string.find(client, 'self:getUrl("diagnostics/warnings")', 1, true),
	"mapped-removal warnings must be forwarded to the owning serve terminal"
)
assert(
	string.find(core, "self.reconciler:prime(self.client, self.project)", 1, true)
		and string.find(reconciler, "function MappingReconciler:prime(client, project)", 1, true),
	"managed connection must bind the disposable build's mapped identities before the first filesystem update"
)
assert(
	string.find(core, "self.reconciler = reconciler or MappingReconciler.new()", 1, true)
		and string.find(app, "self.managedReloadReconciler = self.core.reconciler", 1, true)
		and string.find(app, "Core.new(self.host, self.port, expectedWorktree, self.managedReloadReconciler)", 1, true),
	"managed reload must preserve mapped identity state so removed mappings are deleted from Studio"
)

local startupCapture = assert(string.find(core, "self.client:requestManifestCapture(nil):expect()", 1, true))
local startupReconcile = assert(string.find(core, "local report = self:__reconcileMappings()", startupCapture, true))
assert(
	startupCapture < startupReconcile and not string.find(client, "forceFull", 1, true),
	"an ordinary untrusted attach must capture Studio through auto-recovery before mapped reconciliation"
)

local primeStart = assert(string.find(reconciler, "function MappingReconciler:prime", 1, true))
local reconcileStart = assert(string.find(reconciler, "function MappingReconciler:__bindHierarchy", primeStart, true))
local prime = string.sub(reconciler, primeStart, reconcileStart - 1)
assert(
	string.find(prime, "self.tree:insertInstance(instance, snapshot.id)", 1, true)
		and not string.find(prime, "applyChanges", 1, true)
		and not string.find(prime, "createInstance", 1, true)
		and not string.find(prime, ":Destroy", 1, true)
		and not string.find(prime, "getSourcePage", 1, true),
	"startup identity binding must remain non-mutating and property-free"
)

print("PASS: filesystem mappings use one-way snapshot reconciliation with no Studio instance watcher")
