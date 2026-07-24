local Generator = {}

Generator.Attributes = {
	OutputName = "GeneratedProbe",
}

Generator.OnGenerate = function(parameters, targetContainer)
	local output = Instance.new("Part")
	output.Name = parameters.Attributes.OutputName
	output.Anchored = true
	output.CanCollide = false
	output.Size = parameters.Size
	output.Parent = targetContainer
end

return Generator
