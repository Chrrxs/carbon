--!strict

-- selene: allow(incorrect_standard_library_use)
local Changes = require("../src/Core/Changes")

local function id(value: number): buffer
	local result = buffer.create(4)
	buffer.writeu32(result, 0, value)
	return result
end

local childCount = 40_000
local children = table.create(childCount)
for index = 1, childCount do
	children[index] = {
		id = id(index),
		name = "Child" .. index,
		class = "StringValue",
		properties = {},
		children = {},
	}
end

local changes = Changes.new()
changes:add({
	id = id(0),
	name = "Root",
	class = "Folder",
	properties = {},
	children = children,
})

local started = os.clock()
for index = 1, childCount do
	changes:upsertUpdate({
		id = id(index),
		properties = { Value = { String = tostring(index) } },
		removedProperties = {},
	})
end
local elapsed = os.clock() - started

assert(#changes.additions == 1, "indexed nested updates must remain folded into their addition")
assert(#changes.updates == 0, "indexed nested updates must not create detached updates")
assert(elapsed < 2, string.format("40k indexed nested updates took %.3f seconds", elapsed))

assert(changes:removePendingAddition(id(20_000)), "indexed pending child should be removable")
changes:upsertUpdate({ id = id(20_000), name = "Detached", removedProperties = {} })
assert(#changes.updates == 1, "removed addition identity must be removed from the index")

print(string.format("PASS: Changes indexed 40k nested updates in %.3f seconds", elapsed))
