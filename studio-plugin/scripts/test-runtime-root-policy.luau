--!strict

local RuntimeRootPolicy = require("../src/Core/RuntimeRootPolicy")
local TransientMetadata = require("../src/TransientMetadata")

assert(
	TransientMetadata.isInstance("", "FilteredSelection", "DataModel", true),
	"Studio's blank-class FilteredSelection root must never enter a capture"
)
assert(
	TransientMetadata.isInstance("Instance", "FilteredSelection", "DataModel", true),
	"Studio's reflected Instance-class FilteredSelection root must never enter a capture"
)

assert(
	not RuntimeRootPolicy.shouldAdoptInitial(false, true, false),
	"an unmapped empty tool-materialized root must not enter source during initial adoption"
)
assert(
	not RuntimeRootPolicy.shouldAdoptInitial(true, false, false),
	"an unmapped service materialized by another plugin must not enter source during initial adoption"
)
assert(
	RuntimeRootPolicy.shouldAdoptInitial(true, false, true),
	"a source-mapped runtime-name collision is authored and must be retained"
)
assert(
	RuntimeRootPolicy.shouldRefreshHidden(false, true, true, true, false, false),
	"an authored hidden runtime-name collision must be captured"
)
assert(
	RuntimeRootPolicy.shouldRefreshHidden(true, false, false, false, false, true),
	"an edited runtime root must be promoted and captured"
)
assert(
	RuntimeRootPolicy.shouldRefreshHidden(true, false, false, false, true, false),
	"an already tracked runtime root must remain observable"
)
assert(
	not RuntimeRootPolicy.shouldRefreshHidden(true, false, false, false, false, false),
	"an unmapped unchanged tool root remains transient"
)
assert(
	not RuntimeRootPolicy.shouldRefreshHidden(true, true, false, false, false, true),
	"native activity under an unmapped nominal runtime root must remain transient"
)
assert(
	not TransientMetadata.isInstance("CoreGui", "CoreGui", "DataModel"),
	"mapped roots must pass ordinary change, ordering, and removal handling"
)
assert(
	TransientMetadata.isRoot("UIDragDetectorService", "UIDragDetectorService")
		and not TransientMetadata.isEditorOnlyRoot("UIDragDetectorService", "UIDragDetectorService"),
	"UIDragDetectorService must ignore native wakeups without swallowing visible authored edits"
)
assert(
	TransientMetadata.isRoot("ControllerService", "ControllerService")
		and TransientMetadata.isRoot("GamepadService", "GamepadService")
		and not TransientMetadata.isEditorOnlyRoot("ControllerService", "ControllerService")
		and not TransientMetadata.isEditorOnlyRoot("GamepadService", "GamepadService"),
	"controller roots must ignore native wakeups without swallowing visible authored edits"
)
assert(
	TransientMetadata.isRoot("UserInputService", "UserInputService")
		and TransientMetadata.isEditorOnlyRoot("UserInputService", "UserInputService"),
	"UserInputService is runtime input state and can never become authored place source"
)
assert(
	RuntimeRootPolicy.shouldPromoteEdit(false, true, "Change", true)
		and RuntimeRootPolicy.shouldPromoteEdit(false, true, "Add", false),
	"editing or adding a child to a runtime-named collision must promote its authored subtree"
)
assert(
	not RuntimeRootPolicy.shouldPromoteEdit(false, true, "Add", true),
	"the initial materialized root event alone must not promote an unchanged tool root"
)

print("PASS: runtime-root policy distinguishes unmapped tool state from authored collisions")
