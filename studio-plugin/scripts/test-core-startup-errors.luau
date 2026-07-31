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

local commitProjectName = assert(string.find(core, "self:__commitManagedProjectName(project.name)", 1, true))
local connectedBoundary = assert(string.find(core, "self.status = Core.Status.Connected", commitProjectName, true))
assert(
	commitProjectName < connectedBoundary,
	"the replacement project transport name must commit at the successful reconnect boundary"
)

print("PASS: Core startup preserves structured failures and commits project names only when ready")
