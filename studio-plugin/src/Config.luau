local plugin = script:FindFirstAncestorWhichIsA("Plugin")

local Carbon = script:FindFirstAncestor("Carbon")

local Util = require(Carbon.Util)
local Log = require(Carbon.Log)

export type Level = "Place" | "Game" | "Global"
export type Setting = "Port" | "AutoReconnect" | "LogLevel"

type Config = {
	HOST: string,
	__configs: { [Level]: { [Setting]: any } },
	__callbacks: { [Setting]: { (value: any) -> () } },
	load: () -> (),
	get: (self: Config, setting: Setting, level: Level?, exact: boolean?) -> any,
	getDefault: (self: Config, setting: Setting) -> any,
	set: (self: Config, setting: Setting, value: any, level: Level) -> (),
	restoreDefaults: (self: Config, level: Level) -> (),
	onChanged: (self: Config, setting: Setting, callback: (value: any) -> ()) -> () -> (),
}

local DEFAULTS = {
	Port = 8000,
	AutoReconnect = false,
	LogLevel = "Warn",
}

local CONFIGS_ORDERED = { "Place", "Game", "Global" }
local CONFIGS = {
	Place = "CarbonConfigPlace_" .. game.PlaceId,
	Game = "CarbonConfigGame_" .. game.GameId,
	Global = "CarbonConfigGlobal",
}

local Config: Config = {
	HOST = "127.0.0.1",
	__configs = {},
	__callbacks = {},
} :: Config

function Config.load()
	for level, configKey in CONFIGS do
		local stored = plugin:GetSetting(configKey)
		local config = {}
		local hadObsoleteSetting = false

		if stored and type(stored) == "table" then
			for setting, value in stored do
				if DEFAULTS[setting] ~= nil then
					config[setting] = value
				else
					hadObsoleteSetting = true
				end
			end
		end

		Config.__configs[level] = config

		-- Older Carbon builds exposed Rojo-derived controls that no longer
		-- participate in the loopback, filesystem-authoritative product model.
		-- Rewrite the setting once so those stale keys do not linger forever.
		if hadObsoleteSetting then
			plugin:SetSetting(configKey, if next(config) then config else nil)
		end
	end
end

function Config:get(setting, level, exact)
	local default = DEFAULTS[setting]
	local relevant = not level

	if default == nil then
		Log.error(`Setting '{setting}' does not exist!`)
	end

	if exact then
		return self.__configs[level][setting]
	end

	if level and level ~= "Global" then
		local index = table.find(CONFIGS_ORDERED, level)

		if not index then
			Log.error(`Config level '{level}' does not exist!`)
		end

		level = CONFIGS_ORDERED[index + 1]
	end

	for _, config in CONFIGS_ORDERED do
		if not relevant then
			if config == level then
				relevant = true
			else
				continue
			end
		end

		local value = self.__configs[config][setting]

		if value ~= nil then
			return value
		end
	end

	return default
end

function Config:getDefault(setting)
	local default = DEFAULTS[setting]

	if default == nil then
		Log.error(`Setting '{setting}' does not exist!`)
	end

	return default
end

function Config:set(setting, value, level)
	local default = DEFAULTS[setting]
	local config = self.__configs[level]

	if default == nil then
		Log.error(`Setting '{setting}' does not exist!`)
	end

	if value == config[setting] then
		return
	end

	value = Util.cast(value, type(default))
	config[setting] = if value == default then nil else value

	if self.__callbacks[setting] then
		for _, callback in self.__callbacks[setting] do
			callback(self:get(setting))
		end
	end

	if not next(config) then
		plugin:SetSetting(CONFIGS[level], nil)
	else
		plugin:SetSetting(CONFIGS[level], config)
	end
end

function Config:restoreDefaults(level)
	local config = self.__configs[level]
	self.__configs[level] = {}

	for setting in config do
		if self.__callbacks[setting] then
			for _, callback in self.__callbacks[setting] do
				callback(self:get(setting))
			end
		end
	end

	plugin:SetSetting(CONFIGS[level], nil)
end

function Config:onChanged(setting, callback)
	if not self.__callbacks[setting] then
		self.__callbacks[setting] = {}
	end

	local id = Util.generateGUID()
	self.__callbacks[setting][id] = callback

	return function()
		self.__callbacks[setting][id] = nil
	end
end

return Config
