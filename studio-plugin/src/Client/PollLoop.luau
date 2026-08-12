--!strict

export type Step = () -> (boolean, boolean, any)

local PollLoop = {}

function PollLoop.run(step: Step, shouldContinue: (() -> boolean)?): (boolean, any)
	while shouldContinue == nil or shouldContinue() do
		local complete, success, result = step()
		if complete then
			return success, result
		end
	end
	return false, nil
end

return PollLoop
