--!strict

local Tree = require("../src/Core/Tree")

local firstId = buffer.fromstring(string.rep("a", 16))
local secondId = buffer.fromstring(string.rep("b", 16))
local oldInstance = {} :: any
local replacement = {} :: any

local tree = Tree.new()
tree:insertInstance(oldInstance, firstId)
tree:insertInstance(replacement, firstId)

assert(tree:getId(oldInstance) == nil, "replacing an ID must retire the old object's reverse binding")
assert(tree:getInstance(firstId) == replacement, "the replacement must own the stable ID")
assert(tree:removeByInstance(oldInstance) == nil, "a stale removal must not resolve to the replacement's ID")
assert(tree:getInstance(firstId) == replacement, "a stale removal must not erase the replacement's binding")

tree:insertInstance(replacement, secondId)
assert(tree:getInstance(firstId) == nil, "moving an object to a new ID must retire its previous forward binding")
assert(tree:getInstance(secondId) == replacement, "the object must resolve through its newest ID")

tree:removeById(secondId)
assert(tree:getId(replacement) == nil, "removing the current ID must clear the reverse binding")

print("PASS: tree identity replacement remains one-to-one across delayed stale removals")
