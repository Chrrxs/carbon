--!strict
local Carbon = script:FindFirstAncestor("Carbon")
local App = Carbon.App

local Types = require(Carbon.Types)
local Util = require(Carbon.Util)
local Theme = require(App.Theme)

local function hex(kind: string): string
	return "#" .. Theme.Colors.Diff[kind]:ToHex()
end

return function(changes: Types.Changes): string
	local additions = #changes.additions
	local updates = #changes.updates
	local removals = #changes.removals

	local text = "There will be "

	if additions > 0 then
		text ..= `<font color="{hex("Add")}">{additions} addition{Util.plural(additions)}</font>`

		if updates > 0 or removals > 0 then
			text ..= ", "
		end
	end

	if updates > 0 then
		text ..= `<font color="{hex("Update")}">{updates} update{Util.plural(updates)}</font>`

		if removals > 0 then
			text ..= ", "
		end
	end

	if removals > 0 then
		text ..= `<font color="{hex("Remove")}">{removals} removal{Util.plural(removals)}</font>`
	end

	text ..= " applied compared to the server"

	return text
end
