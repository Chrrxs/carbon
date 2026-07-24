--!strict

local created = {}
Instance = {
	new = function(className: string)
		local instance = {
			ClassName = className,
			Name = className,
			Value = "",
			Archivable = true,
			destroyed = false,
		}
		function instance:IsA(candidate: string): boolean
			return self.ClassName == candidate
		end
		function instance:Destroy()
			self.destroyed = true
		end
		table.insert(created, instance)
		return instance
	end,
}

local Marker = require("../src/ManagedConnectionMarker")
local parent = {}
local authoredString = {
	Name = Marker.Name,
	ClassName = "StringValue",
	Value = "authored",
	Archivable = true,
	destroyed = false,
}
function authoredString:IsA(candidate: string): boolean
	return self.ClassName == candidate
end
local authoredFolder = {
	Name = Marker.Name,
	ClassName = "Folder",
	Archivable = true,
	destroyed = false,
}
function authoredFolder:IsA(candidate: string): boolean
	return self.ClassName == candidate
end

local owned = Marker.create(parent, "session-token")
assert(#created == 1, "managed connection must always create its own exact marker")
assert(owned ~= authoredString and owned ~= authoredFolder, "managed connection must never adopt an authored collision")
assert(owned.Name == Marker.Name and owned.Value == "session-token", "managed marker identity is wrong")
assert(owned.Archivable == false and owned.Parent == parent, "managed marker must be transient")
assert(not Marker.is(authoredString), "an authored StringValue collision must remain canonical")
assert(not Marker.is(authoredFolder), "an authored non-StringValue collision must remain canonical")
assert(Marker.is(owned), "the exact owned non-archivable StringValue must be transient")

Marker.destroy(owned)
assert(owned.destroyed, "disconnect must destroy the exact owned marker")
assert(
	not authoredString.destroyed and authoredString.Value == "authored",
	"authored StringValue collision was mutated"
)
assert(not authoredFolder.destroyed, "authored non-StringValue collision was destroyed")

print("PASS: managed connection marker ownership preserves authored name collisions")
