local function identity(...)
	return ...
end

local function unpackDecoder(f)
	return function(value)
		return f(unpack(value))
	end
end

local ALL_AXES = { "X", "Y", "Z" }
local ALL_FACES = { "Right", "Top", "Back", "Left", "Bottom", "Front" }

local EncodedValue = {}

function EncodedValue.uniqueIdFromHex(hex: string): buffer
	assert(
		type(hex) == "string" and #hex == 32 and string.match(hex, "^[0-9a-fA-F]+$") ~= nil,
		"UniqueId reflection default must be 32 hexadecimal characters"
	)
	local value = buffer.create(16)
	for offset = 0, 15 do
		buffer.writeu8(value, offset, tonumber(string.sub(hex, offset * 2 + 1, offset * 2 + 2), 16))
	end
	return value
end

local types
types = {
	Attributes = {
		fromPod = function(pod)
			local output = {}

			for key, value in pairs(pod) do
				local ok, result = EncodedValue.decode(value)

				if ok then
					output[key] = result
				else
					local warning = ("Could not decode attribute value of type %q: %s"):format(
						typeof(value),
						tostring(result)
					)
					warn(warning)
				end
			end

			return output
		end,
		toPod = function(roblox)
			local output = {}

			for key, value in pairs(roblox) do
				local ok, result = EncodedValue.encodeNaive(value)

				if ok then
					output[key] = result
				else
					local warning = ("Could not encode attribute value of type %q: %s"):format(
						typeof(value),
						tostring(result)
					)
					warn(warning)
				end
			end

			return output
		end,
	},

	Axes = {
		fromPod = function(pod)
			local axes = {}

			for index, axisName in ipairs(pod) do
				axes[index] = Enum.Axis[axisName]
			end

			return Axes.new(unpack(axes))
		end,

		toPod = function(roblox)
			local json = {}

			for _, axis in ipairs(ALL_AXES) do
				if roblox[axis] then
					table.insert(json, axis)
				end
			end

			return json
		end,
	},

	BinaryString = {
		fromPod = function(pod)
			assert(typeof(pod) == "buffer", "BinaryString wire value must be a buffer")
			return buffer.tostring(pod)
		end,
		toPod = function(roblox)
			return buffer.fromstring(roblox)
		end,
	},

	Bool = {
		fromPod = identity,
		toPod = identity,
	},

	BrickColor = {
		fromPod = function(pod)
			return BrickColor.new(pod)
		end,

		toPod = function(roblox)
			return roblox.Number
		end,
	},

	CFrame = {
		fromPod = function(pod)
			local pos = pod.position
			local orient = pod.orientation

			--stylua: ignore
			return CFrame.new(
				pos[1], pos[2], pos[3],
				orient[1][1], orient[1][2], orient[1][3],
				orient[2][1], orient[2][2], orient[2][3],
				orient[3][1], orient[3][2], orient[3][3]
			)
		end,

		toPod = function(roblox)
			local x, y, z, r00, r01, r02, r10, r11, r12, r20, r21, r22 = roblox:GetComponents()

			return {
				position = { x, y, z },
				orientation = {
					{ r00, r01, r02 },
					{ r10, r11, r12 },
					{ r20, r21, r22 },
				},
			}
		end,
	},

	Color3 = {
		fromPod = unpackDecoder(Color3.new),

		toPod = function(roblox)
			return { roblox.r, roblox.g, roblox.b }
		end,
	},

	Color3uint8 = {
		fromPod = unpackDecoder(Color3.fromRGB),

		toPod = function(roblox)
			return {
				math.round(roblox.R * 255),
				math.round(roblox.G * 255),
				math.round(roblox.B * 255),
			}
		end,
	},

	ColorSequence = {
		fromPod = function(pod)
			local keypoints = {}

			for index, keypoint in ipairs(pod.keypoints) do
				keypoints[index] = ColorSequenceKeypoint.new(keypoint.time, types.Color3.fromPod(keypoint.color))
			end

			return ColorSequence.new(keypoints)
		end,

		toPod = function(roblox)
			local keypoints = {}

			for index, keypoint in ipairs(roblox.Keypoints) do
				keypoints[index] = {
					time = keypoint.Time,
					color = types.Color3.toPod(keypoint.Value),
				}
			end

			return {
				keypoints = keypoints,
			}
		end,
	},

	Content = {
		fromPod = function(pod, tree): Content | { __carbonContentObject: Instance }
			if type(pod) == "string" then
				if pod == "None" then
					return Content.none
				else
					-- error(`unexpected Content value '{pod}'`)
					return Content.fromUri(pod) -- Temporary
				end
			else
				local ty, value = next(pod)
				if ty == "Uri" then
					return Content.fromUri(value)
				elseif ty == "Object" then
					local instance = tree and tree:getInstance(value)
					assert(instance ~= nil, "Content.Object target is missing")
					-- Roblox can deserialize legacy Instance-backed Content.Object values,
					-- but Content.fromObject rejects Instances. Preserve a comparison
					-- sentinel so initial Studio-authoritative reconciliation can still
					-- compare and sync the exact object reference back to source.
					return { __carbonContentObject = instance }
				else
					error(`Unknown Content type '{ty}' (could not deserialize)`)
				end
			end
		end,
		toPod = function(roblox: Content, tree)
			if roblox.SourceType == Enum.ContentSourceType.None then
				return "None"
			elseif roblox.SourceType == Enum.ContentSourceType.Uri then
				return { Uri = roblox.Uri }
			elseif roblox.SourceType == Enum.ContentSourceType.Object then
				local object = roblox.Object
				local id = tree and tree:getId(object)
				assert(id ~= nil, "Content.Object target is not tracked")
				return { Object = id }
			else
				error(`Unknown Content type '{roblox.SourceType} (could not serialize)`)
			end
		end,
	},

	ContentId = {
		fromPod = identity,
		toPod = identity,
	},

	Enum = {
		fromPod = identity,

		toPod = function(roblox)
			-- FIXME: More robust handling of enums
			if typeof(roblox) == "number" then
				return roblox
			else
				return roblox.Value
			end
		end,
	},

	EnumItem = {
		fromPod = function(pod)
			return Enum[pod.type]:FromValue(pod.value)
		end,

		toPod = function(roblox)
			return {
				type = tostring(roblox.EnumType),
				value = roblox.Value,
			}
		end,
	},

	Faces = {
		fromPod = function(pod)
			local faces = {}

			for index, faceName in ipairs(pod) do
				faces[index] = Enum.NormalId[faceName]
			end

			return Faces.new(unpack(faces))
		end,

		toPod = function(roblox)
			local pod = {}

			for _, face in ipairs(ALL_FACES) do
				if roblox[face] then
					table.insert(pod, face)
				end
			end

			return pod
		end,
	},

	Float32 = {
		fromPod = identity,
		toPod = identity,
	},

	Float64 = {
		fromPod = identity,
		toPod = identity,
	},

	Font = {
		fromPod = function(pod)
			return Font.new(
				pod.family,
				if pod.weight ~= nil then Enum.FontWeight[pod.weight] else nil,
				if pod.style ~= nil then Enum.FontStyle[pod.style] else nil
			)
		end,
		toPod = function(roblox)
			return {
				family = roblox.Family,
				weight = roblox.Weight.Name,
				style = roblox.Style.Name,
			}
		end,
	},

	Int32 = {
		fromPod = identity,
		toPod = identity,
	},

	Int64 = {
		fromPod = identity,
		toPod = identity,
	},

	UniqueId = {
		fromPod = function(pod)
			assert(typeof(pod) == "buffer" and buffer.len(pod) == 16, "UniqueId wire value must be 16 bytes")
			local canonical = buffer.tostring(pod)
			-- Carbon's canonical UniqueId layout matches rbx_types' textual/wire
			-- representation: random (8 bytes), time (4), index (4). Roblox's
			-- Luau property value exposes the same components in its serialized
			-- engine order: index (4), time (4), random (8).
			return string.sub(canonical, 13, 16) .. string.sub(canonical, 9, 12) .. string.sub(canonical, 1, 8)
		end,
		toPod = function(roblox)
			assert(type(roblox) == "string" and #roblox == 16, "UniqueId exact value must be 16 bytes")
			local canonical = string.sub(roblox, 9, 16) .. string.sub(roblox, 5, 8) .. string.sub(roblox, 1, 4)
			return buffer.fromstring(canonical)
		end,
	},

	SecurityCapabilities = {
		fromPod = identity,
		toPod = identity,
	},

	NetAssetRef = {
		-- Roblox represents NetAssetRef values as an opaque byte payload. They use
		-- the same wire representation as SharedString, but
		-- retain a distinct variant tag in the canonical Carbon data.
		fromPod = function(pod)
			if typeof(pod) == "buffer" then
				return buffer.tostring(pod)
			end

			return pod
		end,

		toPod = function(roblox)
			if typeof(roblox) == "string" then
				return buffer.fromstring(roblox)
			end

			return roblox
		end,
	},

	MaterialColors = {
		fromPod = function(pod: { [string]: { number } })
			local real = {}
			for name, color in pod do
				real[Enum.Material[name]] = Color3.fromRGB(color[1], color[2], color[3])
			end
			return real
		end,
		toPod = function(roblox: { [Enum.Material]: Color3 })
			local pod = {}
			for material, color in roblox do
				pod[material.Name] = {
					math.round(math.clamp(color.R, 0, 1) * 255),
					math.round(math.clamp(color.G, 0, 1) * 255),
					math.round(math.clamp(color.B, 0, 1) * 255),
				}
			end
			return pod
		end,
	},

	NumberRange = {
		fromPod = unpackDecoder(NumberRange.new),

		toPod = function(roblox)
			return { roblox.Min, roblox.Max }
		end,
	},

	NumberSequence = {
		fromPod = function(pod)
			local keypoints = {}

			for index, keypoint in ipairs(pod.keypoints) do
				-- TODO: Add a test for NaN or Infinity values and envelopes
				-- Right now it isn't possible because it'd fail the roundtrip.
				-- It's more important that it works right now, though.
				local value = keypoint.value or 0
				local envelope = keypoint.envelope or 0
				keypoints[index] = NumberSequenceKeypoint.new(keypoint.time, value, envelope)
			end

			return NumberSequence.new(keypoints)
		end,

		toPod = function(roblox)
			local keypoints = {}

			for index, keypoint in ipairs(roblox.Keypoints) do
				keypoints[index] = {
					time = keypoint.Time,
					value = keypoint.Value,
					envelope = keypoint.Envelope,
				}
			end

			return {
				keypoints = keypoints,
			}
		end,
	},

	PhysicalProperties = {
		fromPod = function(pod)
			if pod == "Default" then
				return nil
			else
				local custom = pod.Custom
				assert(custom ~= nil, "custom PhysicalProperties must use the canonical { Custom = ... } wire shape")

				-- Passing `nil` instead of not passing anything gives
				-- different results, so we have to branch here.
				if custom.acousticAbsorption then
					return (PhysicalProperties.new :: any)(
						custom.density,
						custom.friction,
						custom.elasticity,
						custom.frictionWeight,
						custom.elasticityWeight,
						custom.acousticAbsorption
					)
				else
					return PhysicalProperties.new(
						custom.density,
						custom.friction,
						custom.elasticity,
						custom.frictionWeight,
						custom.elasticityWeight
					)
				end
			end
		end,

		toPod = function(roblox)
			if roblox == nil then
				return "Default"
			else
				return {
					Custom = {
						density = roblox.Density,
						friction = roblox.Friction,
						elasticity = roblox.Elasticity,
						frictionWeight = roblox.FrictionWeight,
						elasticityWeight = roblox.ElasticityWeight,
						acousticAbsorption = roblox.AcousticAbsorption,
					},
				}
			end
		end,
	},

	Ray = {
		fromPod = function(pod)
			return Ray.new(types.Vector3.fromPod(pod.origin), types.Vector3.fromPod(pod.direction))
		end,

		toPod = function(roblox)
			return {
				origin = types.Vector3.toPod(roblox.Origin),
				direction = types.Vector3.toPod(roblox.Direction),
			}
		end,
	},

	Rect = {
		fromPod = function(pod)
			return Rect.new(types.Vector2.fromPod(pod[1]), types.Vector2.fromPod(pod[2]))
		end,

		toPod = function(roblox)
			return {
				types.Vector2.toPod(roblox.Min),
				types.Vector2.toPod(roblox.Max),
			}
		end,
	},

	Ref = {
		fromPod = function(pod, tree)
			if tree == nil then
				return nil
			end

			return tree:getInstance(pod)
		end,

		toPod = function(roblox, tree)
			if roblox == nil then
				return buffer.create(16) -- represents Ref::none()
			end
			if type(roblox) == "table" and typeof(roblox.__carbonSourceRef) == "buffer" then
				return roblox.__carbonSourceRef
			end

			if tree == nil then
				return nil
			end

			return tree:getId(roblox)
		end,
	},

	Region3 = {
		fromPod = function(_)
			error("Region3 is not implemented")
		end,

		toPod = function(_)
			error("Region3 is not implemented")
		end,
	},

	Region3int16 = {
		fromPod = function(pod)
			return Region3int16.new(types.Vector3int16.fromPod(pod[1]), types.Vector3int16.fromPod(pod[2]))
		end,

		toPod = function(roblox)
			return {
				types.Vector3int16.toPod(roblox.Min),
				types.Vector3int16.toPod(roblox.Max),
			}
		end,
	},

	SharedString = {
		fromPod = function(pod)
			if typeof(pod) == "buffer" then
				return buffer.tostring(pod)
			end

			return pod
		end,

		toPod = function(roblox)
			if typeof(roblox) == "string" then
				return buffer.fromstring(roblox)
			end

			return roblox
		end,
	},

	String = {
		fromPod = identity,
		toPod = identity,
	},

	UDim = {
		fromPod = unpackDecoder(UDim.new),

		toPod = function(roblox)
			return { roblox.Scale, roblox.Offset }
		end,
	},

	UDim2 = {
		fromPod = function(pod)
			return UDim2.new(types.UDim.fromPod(pod[1]), types.UDim.fromPod(pod[2]))
		end,

		toPod = function(roblox)
			return {
				types.UDim.toPod(roblox.X),
				types.UDim.toPod(roblox.Y),
			}
		end,
	},

	Tags = {
		fromPod = identity,
		toPod = identity,
	},

	Vector2 = {
		fromPod = unpackDecoder(Vector2.new),

		toPod = function(roblox)
			return {
				identity(roblox.X),
				identity(roblox.Y),
			}
		end,
	},

	Vector2int16 = {
		fromPod = unpackDecoder(Vector2int16.new),

		toPod = function(roblox)
			return { roblox.X, roblox.Y }
		end,
	},

	Vector3 = {
		fromPod = unpackDecoder(Vector3.new),

		toPod = function(roblox)
			return {
				identity(roblox.X),
				identity(roblox.Y),
				identity(roblox.Z),
			}
		end,
	},

	Vector3int16 = {
		fromPod = unpackDecoder(Vector3int16.new),

		toPod = function(roblox)
			return { roblox.X, roblox.Y, roblox.Z }
		end,
	},
}

types.OptionalCFrame = {
	fromPod = function(pod)
		if pod == nil then
			return nil
		else
			return types.CFrame.fromPod(pod)
		end
	end,

	toPod = function(roblox)
		if roblox == nil then
			return nil
		else
			return types.CFrame.toPod(roblox)
		end
	end,
}

function EncodedValue.decode(encodedValue, tree)
	local ty, value = next(encodedValue)

	if ty == nil then
		-- If the encoded pair is empty, assume it is an unoccupied optional value
		return true, nil
	end

	local typeImpl = types[ty]
	if typeImpl == nil then
		return false, "Couldn't decode value " .. tostring(ty)
	end

	return true, typeImpl.fromPod(value, tree)
end

function EncodedValue.encode(rbxValue, propertyType, tree)
	assert(propertyType ~= nil, "Property type descriptor is required")

	local typeImpl = types[propertyType]
	if typeImpl == nil then
		return false, ("Missing encoder for property type %q"):format(propertyType)
	end

	return true, {
		[propertyType] = typeImpl.toPod(rbxValue, tree),
	}
end

local propertyTypeRenames = {
	number = "Float64",
	boolean = "Bool",
	string = "String",
}

function EncodedValue.encodeNaive(rbxValue)
	local propertyType = typeof(rbxValue)
	if propertyTypeRenames[propertyType] ~= nil then
		propertyType = propertyTypeRenames[propertyType]
	end

	return EncodedValue.encode(rbxValue, propertyType)
end

return EncodedValue
