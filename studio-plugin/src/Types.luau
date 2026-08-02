-- General structs

export type Ref = buffer
export type ClassName = string

export type Properties = {
	[string]: any,
}

-- Snapshots

export type Snapshot = {
	id: Ref,
	name: string,
	rawName: buffer?,
	class: ClassName,
	properties: Properties,
	children: { Snapshot },
}

export type AddedSnapshot = {
	id: Ref,
	parent: Ref,
	name: string,
	rawName: buffer?,
	class: ClassName,
	properties: Properties,
	children: { Snapshot },
}

export type HierarchySnapshot = {
	id: Ref,
	parent: Ref,
	name: string,
	rawName: buffer?,
	class: ClassName,
	unavailable: boolean?,
}

export type PropertySnapshot = {
	id: Ref,
	properties: Properties,
}

export type SnapshotPage = {
	instances: { HierarchySnapshot },
	cursor: { Ref },
	done: boolean,
	encodedBytes: number,
}

export type SourceCursor = {
	generation: string,
	index: number,
}

export type SourcePage = {
	instances: { PropertySnapshot },
	cursor: SourceCursor?,
	done: boolean,
	encodedBytes: number,
}

export type InitialChildren = { [Instance]: { Instance } }
export type InitialNames = { [Instance]: string }

export type UpdatedSnapshot = {
	id: Ref,
	parent: Ref?,
	name: string?,
	rawName: buffer?,
	class: ClassName?,
	properties: Properties?,
	removedProperties: { string }?,
}

-- Other structs

export type Changes = {
	additions: { AddedSnapshot },
	updates: { UpdatedSnapshot },
	removals: { Ref | Instance },
}

export type Project = {
	protocolVersion: number,
	name: string,
	version: string,
	rootRefs: { Ref },
	mappedRootRefs: { Ref },
	isPlace: boolean,
	sourceRootRef: Ref?,
	sourceGeneration: string?,
	worktreeId: string?,
	sessionToken: string?,
	managedReloadTransitionId: string?,
}

export type ManifestCaptureStatus = {
	requestId: string,
	state: "running" | "complete" | "failed",
	sourceGeneration: string,
	message: string?,
}

-- Messages

export type Message =
	SyncChanges
	| SyncDetails
	| RestartRequired
	| ExecuteCode
	| Disconnect
	| ManagedReload
	| StudioChangeProbe
export type MessageKind =
	"SyncChanges"
	| "SyncDetails"
	| "RestartRequired"
	| "ExecuteCode"
	| "Disconnect"
	| "ManagedReload"
	| "StudioChangeProbe"

export type SyncChanges = {
	SyncChanges: {
		changes: Changes,
		sourceGeneration: string,
	},
}

export type SyncDetails = {
	SyncDetails: Project,
}

export type RestartRequired = {
	RestartRequired: {
		message: string,
	},
}

export type ExecuteCode = {
	code: string,
}

export type Disconnect = {
	message: string,
}

export type ManagedReloadData = {
	transitionId: string,
	projectName: string,
	sourceGeneration: string,
	worktreeId: string,
	sessionToken: string,
}

export type ManagedReload = {
	ManagedReload: ManagedReloadData,
}

export type StudioChangeProbe = {
	StudioChangeProbe: {
		requestId: string,
	},
}

return nil
