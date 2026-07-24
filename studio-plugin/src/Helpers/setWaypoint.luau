local Carbon = script:FindFirstAncestor("Carbon")
local RuntimeServices = require(Carbon.RuntimeServices)
local ChangeHistoryService = RuntimeServices.get("ChangeHistoryService") :: ChangeHistoryService

return function(action: string, name: string?)
	name = name and ` "{name}"` or ""
	ChangeHistoryService:SetWaypoint(`Carbon: {action}{name} [{DateTime.now():FormatLocalTime("LTS", "en-us")}]`)
end
