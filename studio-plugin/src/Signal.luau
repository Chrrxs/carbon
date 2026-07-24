--!strict

export type Connection = {
	connected: boolean,
	signal: Signal,
	callback: (...any) -> (),
	Unbind: (self: Connection) -> (),
}

export type Signal = {
	yieldable: boolean,
	keepOrder: boolean,
	connections: { [Connection]: boolean },
	Bind: (self: Signal, callback: (...any) -> ()) -> Connection,
	Once: (self: Signal, callback: (...any) -> ()) -> Connection,
	Fire: (self: Signal, ...any) -> (),
	Unbind: (self: Signal, connection: Connection) -> (),
	UnbindAll: (self: Signal) -> (),
	Wait: (self: Signal) -> ...any,
}

local freeThread: thread? = nil

local function acquireThreadAndRunCallback(callback, ...)
	local acquiredThread = freeThread
	freeThread = nil
	callback(...)
	freeThread = acquiredThread
end

local function runCallback()
	while true do
		acquireThreadAndRunCallback(coroutine.yield())
	end
end

local Connection = {}
Connection.__index = Connection

function Connection.new(signal: Signal, callback: (...any) -> ()): Connection
	return setmetatable({
		connected = true,
		signal = signal,
		callback = callback,
	}, Connection) :: any
end

function Connection:Unbind()
	self.connected = false
	self.signal:Unbind(self)
end

local Signal = {}
Signal.__index = Signal

function Signal.new(yieldable: boolean?, keepOrder: boolean?): Signal
	return setmetatable({
		yieldable = if type(yieldable) == "boolean" then yieldable else true,
		keepOrder = if type(keepOrder) == "boolean" then keepOrder else false,
		connections = {},
	}, Signal) :: any
end

function Signal:Bind(callback: (...any) -> ()): Connection
	local connection = Connection.new(self, callback)
	self.connections[connection] = true
	if self.keepOrder then
		local connections = {}
		for current in self.connections do
			connections[current] = true
		end
		self.connections = connections
	end
	return connection
end

function Signal:Once(callback: (...any) -> ()): Connection
	local connection: Connection
	connection = Connection.new(self, function(...)
		if connection.connected then
			connection:Unbind()
		end
		callback(...)
	end)
	self.connections[connection] = true
	return connection
end

function Signal:Fire(...: any)
	for connection in self.connections do
		if self.yieldable then
			if not freeThread then
				freeThread = coroutine.create(runCallback)
				coroutine.resume(freeThread)
			end
			task.spawn(freeThread, connection.callback, ...)
		else
			connection.callback(...)
		end
	end
end

function Signal:Unbind(connection: Connection)
	self.connections[connection] = nil
end

function Signal:UnbindAll()
	self.connections = {}
end

function Signal:Wait(): ...any
	local thread = coroutine.running()
	self:Once(function(...)
		task.spawn(thread, ...)
	end)
	return coroutine.yield()
end

return Signal
