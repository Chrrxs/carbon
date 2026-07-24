local Carbon = script:FindFirstAncestor("Carbon")
local RuntimeServices = require(Carbon.RuntimeServices)
local CollectionService = RuntimeServices.get("CollectionService") :: CollectionService
local ScriptEditorService = RuntimeServices.get("ScriptEditorService") :: ScriptEditorService

local Error = require(script.Parent.Error)
local AwaitedSourceWrite = require(script.Parent.AwaitedSourceWrite)
local StyleRuleProperties = require(script.Parent.StyleRuleProperties)
local TransientMetadata = require(Carbon.TransientMetadata)

--- A list of `Enum.Material` values that are used for Terrain.MaterialColors
local TERRAIN_MATERIAL_COLORS = {
	Enum.Material.Grass,
	Enum.Material.Slate,
	Enum.Material.Concrete,
	Enum.Material.Brick,
	Enum.Material.Sand,
	Enum.Material.WoodPlanks,
	Enum.Material.Rock,
	Enum.Material.Glacier,
	Enum.Material.Snow,
	Enum.Material.Sandstone,
	Enum.Material.Mud,
	Enum.Material.Basalt,
	Enum.Material.Ground,
	Enum.Material.CrackedLava,
	Enum.Material.Asphalt,
	Enum.Material.Cobblestone,
	Enum.Material.Ice,
	Enum.Material.LeafyGrass,
	Enum.Material.Salt,
	Enum.Material.Limestone,
	Enum.Material.Pavement,
}

local function isAttributeNameValid(attributeName)
	-- For SetAttribute to succeed, the attribute name must be less than or
	-- equal to 100 characters...
	return #attributeName <= 100
		-- ...and must only contain alphanumeric characters, periods, hyphens,
		-- underscores, or forward slashes.
		and attributeName:match("[^%w%.%-_/]") == nil
end

local function isAttributeNameReserved(attributeName)
	-- For SetAttribute to succeed, attribute names must not use the RBX
	-- prefix, which is reserved by Roblox.
	return attributeName:sub(1, 3) == "RBX"
end

local SOURCE_INTERFACE = {
	read = function(instance)
		return true, ScriptEditorService:GetEditorSource(instance)
	end,
	write = function(instance, _, value: string)
		if type(value) ~= "string" then
			return false, Error.new(Error.Kind.Roblox, "script source must be a string")
		end
		local success, reason = AwaitedSourceWrite.run(function(transform)
			-- UpdateSourceAsync yields. Do not detach it: the encompassing
			-- SyncChanges transaction is successful only after Studio commits.
			ScriptEditorService:UpdateSourceAsync(instance, transform)
		end, value)
		return success, if success then nil else Error.new(Error.Kind.Roblox, reason)
	end,
}

-- Defines how to read and write properties that aren't directly scriptable.
--
-- The reflection database refers to these as having scriptability = "Custom"
return {
	Instance = {
		Attributes = {
			read = function(instance)
				return true, TransientMetadata.stripAttributes(instance, instance:GetAttributes())
			end,
			write = function(instance, _, value)
				if typeof(value) ~= "table" then
					return false, Error.new(Error.Kind.CannotParseBinaryString)
				end

				local existing = instance:GetAttributes()
				TransientMetadata.rememberAuthoredAttributes(instance, value)
				local didAllWritesSucceed = true

				for attributeName, attributeValue in pairs(value) do
					if TransientMetadata.isAttribute(instance, attributeName) then
						continue
					end

					if isAttributeNameReserved(attributeName) then
						-- If the attribute name is reserved, then we don't
						-- really care about reporting any failures about
						-- it.
						continue
					end

					if not isAttributeNameValid(attributeName) then
						didAllWritesSucceed = false
						continue
					end

					instance:SetAttribute(attributeName, attributeValue)
				end

				for existingAttributeName in pairs(existing) do
					if TransientMetadata.isAttribute(instance, existingAttributeName) then
						continue
					end

					if isAttributeNameReserved(existingAttributeName) then
						continue
					end

					if not isAttributeNameValid(existingAttributeName) then
						didAllWritesSucceed = false
						continue
					end

					if value[existingAttributeName] == nil then
						instance:SetAttribute(existingAttributeName, nil)
					end
				end

				return didAllWritesSucceed
			end,
		},
		Tags = {
			read = function(instance)
				return true, CollectionService:GetTags(instance)
			end,
			write = function(instance, _, value)
				local existingTags = CollectionService:GetTags(instance)

				local unseenTags = {}
				for _, tag in ipairs(existingTags) do
					unseenTags[tag] = true
				end

				for _, tag in ipairs(value) do
					unseenTags[tag] = nil
					CollectionService:AddTag(instance, tag)
				end

				for tag in pairs(unseenTags) do
					CollectionService:RemoveTag(instance, tag)
				end

				return true
			end,
		},
	},
	LocalizationTable = {
		Contents = {
			read = function(instance, _)
				return true, instance:GetContents()
			end,
			write = function(instance, _, value)
				instance:SetContents(value)
				return true
			end,
		},
	},
	Model = {
		Scale = {
			read = function(instance, _, _)
				return true, instance:GetScale()
			end,
			write = function(instance, _, value)
				return true, instance:ScaleTo(value)
			end,
		},
		WorldPivotData = {
			read = function(instance)
				return true, instance.WorldPivot
			end,
			write = function(instance, _, value)
				if value == nil then
					return true, nil
				else
					instance.WorldPivot = value
					return true
				end
			end,
		},
	},
	Terrain = {
		MaterialColors = {
			read = function(instance: Terrain)
				-- There's no way to get a list of every color, so we have to
				-- make one.
				local colors = {}
				for _, material in TERRAIN_MATERIAL_COLORS do
					colors[material] = instance:GetMaterialColor(material)
				end

				return true, colors
			end,
			write = function(instance: Terrain, _, value: { [Enum.Material]: Color3 })
				if typeof(value) ~= "table" then
					return false, Error.new(Error.Kind.CannotParseBinaryString)
				end

				for material, color in value do
					instance:SetMaterialColor(material, color)
				end

				return true
			end,
		},
	},
	Script = {
		Source = SOURCE_INTERFACE,
	},
	LocalScript = {
		Source = SOURCE_INTERFACE,
	},
	ModuleScript = {
		Source = SOURCE_INTERFACE,
	},
	StyleRule = {
		-- The descriptor lookup is canonical. Roblox serializes this property as
		-- PropertiesSerialize, but the custom interface name is Properties.
		Properties = StyleRuleProperties,
	},
	MeshPart = {
		MeshContent = {
			read = function(instance: MeshPart)
				return true, instance.MeshContent
			end,
			write = function()
				-- Carbon handles this in a custom way
				return true
			end,
		},
		InitialSize = {
			read = function(instance: MeshPart)
				return true, instance.MeshSize
			end,
			write = function()
				-- Carbon handles this in a custom way
				return true
			end,
		},
	},
}
