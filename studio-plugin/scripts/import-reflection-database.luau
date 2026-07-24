--!strict

local fs = require("@lune/fs")
local process = require("@lune/process")
local serde = require("@lune/serde")

local MsgPack = require("../src/Lib/MsgPack")

local PINNED_VERSION = { 0, 729, 0, 7290838 }
local OUTPUT = "src/Lib/Dom/database.msgpack"

local input = process.args[1]
assert(input ~= nil and input ~= "--", "usage: lune run scripts/import-reflection-database.luau <database.json>")

local database = serde.decode("json", fs.readFile(input))
assert(
	serde.encode("json", database.Version) == serde.encode("json", PINNED_VERSION),
	`expected reflection version {serde.encode("json", PINNED_VERSION)}, got {serde.encode("json", database.Version)}`
)

fs.writeFile(OUTPUT, MsgPack.encode(database))
print(`Imported reflection database into {OUTPUT}`)
