local ManagedConnectionMarker = {}

ManagedConnectionMarker.Name = "__StudioWorktree_CarbonConnection"

function ManagedConnectionMarker.create(parent: Instance, sessionToken: string): StringValue
	local marker = Instance.new("StringValue")
	marker.Name = ManagedConnectionMarker.Name
	marker.Value = sessionToken
	marker.Archivable = false
	marker.Parent = parent
	return marker
end

function ManagedConnectionMarker.destroy(marker: StringValue?)
	if marker ~= nil then
		marker:Destroy()
	end
end

function ManagedConnectionMarker.is(instance: Instance): boolean
	return instance.Name == ManagedConnectionMarker.Name
		and instance:IsA("StringValue")
		and instance.Archivable == false
end

return ManagedConnectionMarker
