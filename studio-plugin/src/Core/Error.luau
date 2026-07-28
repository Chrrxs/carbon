local Carbon = script:FindFirstAncestor("Carbon")

local Util = require(Carbon.Util)
local formatError = require(Carbon.ErrorFormat)

export type Error = {
	message: string,
	kind: string,
}

local Error = {
	Unknown = "Unknown Core error: $1",
	-- Initial prompts
	ProtocolVersion = "Client protocol version: $1 does not match server protocol version: $2",
	Version = "Client version: $1 is not compatible with the server version: $2",
	WorktreeContract = "Managed worktree contract mismatch: $1",
	TooManyChanges = "There are $1 additions, $2 updates, $3 deletions compared to the server",
	Terminated = "Terminated connection by the user",
	-- Core errors
	UnknownEvent = "Received an unknown event from the server: $1, with data: $2",
	Disconnected = "Disconnected from the server: $1",
	RestartRequired = "Carbon serve requires a hard restart: $1",
	ManagedReload = "Managed carbon reload requested: $1",
	PrivilegedUnavailable = "The exact RML bridge required for Capture Manifest is unavailable: $1",
	ManifestIdentityUnsupported = "RML does not support Carbon manifest identity GUIDs",
	ManifestIdentityRejected = "RML rejected the managed-place manifest identity contract",
	-- Process errors
	DecodeFailed = "Failed to decode snapshot property: $1 with value: $2",
	EncodeFailed = "Failed to encode snapshot property: $1 with value: $2",
	ReadFailed = "Failed to read property: $1 from instance: $2, reason: $3",
	WriteFailed = "Failed to write property: $1 for instance: $2, reason: $3",
	NoInstanceAdd = "Tried to add an instance whose parent that does not exist in the tree: $1",
	NoInstanceUpdate = "Tried to update an instance that does not exist in the tree: $1",
	NoInstanceMove = "Tried to move an instance whose parent does not exist in the tree: $1",
	NoInstanceRemove = "Tried to remove an instance that does not exist in the tree: $1",
	InvalidUnavailableRoot = "Server marked a non-root instance as Studio-unavailable: $1",
	NotCreatable = "Tried to create an instance of class: $1 that is not creatable",
	NotRemovable = "Tried to remove an instance of class: $1 that is not removable",
	HydrationFailed = "Failed to hydrate root service, snapshot: $1",
	ValidationFailed = "Skipped property: $1 from instance: $2, because it contains malformed data: $3",
	UnsafeAddition = "Refused to sync added subtree $1 because $2 contains protected serialized properties that Studio plugins cannot read: $3. Import it through Carbon's full binary extract/build path instead.",
}

function Error.__new(message: string, kind: string): Error
	local err = setmetatable({
		message = message,
		kind = kind,
	}, Error)

	return err
end

function Error.new(err: Error, ...): Error
	err = table.clone(err)
	err.message = formatError(err.message, { ... }, Util.stringify)

	return err
end

function Error:__eq(other: Error): boolean
	return self.kind == other.kind
end

function Error:__tostring(): string
	return self.message
end

-- Convert all strings to Error objects
for kind, message in Error do
	if type(message) == "string" then
		Error[kind] = Error.__new(message, kind)
	end
end

return Error
