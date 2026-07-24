--!strict

local fs = require("@lune/fs")

local MsgPack = require("../src/Lib/MsgPack")

local database = MsgPack.decode(fs.readFile("src/Lib/Dom/database.msgpack"))

local function hasTag(tags, expected: string): boolean
	for _, tag in tags do
		if tag == expected then
			return true
		end
	end
	return false
end

local rows = {}
for className, class in database.Classes do
	if hasTag(class.Tags, "Service") or hasTag(class.Tags, "NotCreatable") then
		local current = class
		while current ~= nil do
			for propertyName, descriptor in current.Properties do
				local dataType = descriptor.DataType
				if dataType.Value == "Content" then
					local canonical = descriptor.Kind.Canonical
					if canonical ~= nil and canonical.Serialization ~= "DoesNotSerialize" then
						table.insert(
							rows,
							`{className}.{propertyName}\t{descriptor.Scriptability}\t{canonical.Serialization}`
						)
					end
				end
			end
			current = if current.Superclass ~= nil then database.Classes[current.Superclass] else nil
		end
	end
end

table.sort(rows)
for _, row in rows do
	print(row)
end
