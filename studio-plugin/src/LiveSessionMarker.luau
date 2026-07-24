--!strict

local LiveSessionMarker = {}

LiveSessionMarker.Name = "__CarbonLiveSession"
LiveSessionMarker.ProtocolVersion = 1

function LiveSessionMarker.is(instance: Instance, coreGui: Instance): boolean
	return instance.Parent == coreGui
		and instance.Name == LiveSessionMarker.Name
		and instance:IsA("StringValue")
		and instance.Archivable == false
end

local function endpoint(value: any)
	if type(value) ~= "string" then
		return nil
	end
	local host, rawPort = value:match("^http://([^:/]+):(%d+)$")
	local port = tonumber(rawPort)
	if (host ~= "127.0.0.1" and host ~= "localhost") or port == nil or port < 1 or port > 65535 then
		return nil
	end
	return host, port
end

function LiveSessionMarker.resolve(instance: Instance, coreGui: Instance, httpService: any)
	if not LiveSessionMarker.is(instance, coreGui) then
		return nil
	end
	local success, payload = pcall(httpService.JSONDecode, httpService, (instance :: StringValue).Value)
	if not success or type(payload) ~= "table" or payload.protocolVersion ~= LiveSessionMarker.ProtocolVersion then
		return nil
	end
	local host, port = endpoint(payload.endpoint)
	if
		host == nil
		or type(payload.project) ~= "string"
		or payload.project == ""
		or type(payload.session) ~= "string"
		or payload.session == ""
		or type(payload.generation) ~= "string"
		or payload.generation == ""
	then
		return nil
	end

	local project = {
		name = payload.project,
		sessionToken = payload.session,
		sourceGeneration = payload.generation,
	}
	project.updateGeneration = function(generation: string)
		if not LiveSessionMarker.is(instance, coreGui) or generation == "" then
			return
		end
		payload.generation = generation
		project.sourceGeneration = generation
		local value = instance :: StringValue
		value.Value = httpService:JSONEncode(payload)
	end

	return {
		marker = instance,
		host = host,
		port = port,
		project = project,
	}
end

function LiveSessionMarker.find(coreGui: Instance, httpService: any)
	for _, child in coreGui:GetChildren() do
		local resolved = LiveSessionMarker.resolve(child, coreGui, httpService)
		if resolved ~= nil then
			return resolved
		end
	end
	return nil
end

return LiveSessionMarker
