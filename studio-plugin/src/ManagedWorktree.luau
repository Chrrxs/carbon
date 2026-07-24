local ManagedWorktree = {}

function ManagedWorktree.resolve(endpoint: any, project: any, worktreeId: any, sessionToken: any, sourceGeneration: any)
	if type(endpoint) ~= "string" then
		return nil
	end
	local host, rawPort = endpoint:match("^http://([^:/]+):(%d+)$")
	local port = tonumber(rawPort)
	if (host ~= "127.0.0.1" and host ~= "localhost") or port == nil then
		return nil
	end
	if
		type(project) ~= "string"
		or project == ""
		or type(worktreeId) ~= "string"
		or worktreeId == ""
		or type(sessionToken) ~= "string"
		or sessionToken == ""
	then
		return nil
	end
	return {
		host = host,
		port = port,
		project = {
			name = project,
			worktreeId = worktreeId,
			sessionToken = sessionToken,
			sourceGeneration = if type(sourceGeneration) == "string" then sourceGeneration else nil,
		},
	}
end

return ManagedWorktree
