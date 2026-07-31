#include "serialized_property_access.hpp"

#include "../dotnet/type_marshaler.hpp"
#include "RobloxModLoader/roblox/reflection/object.hpp"
#include "RobloxModLoader/roblox/reflection/property_descriptor.hpp"
#include <array>
#include <charconv>
#include <cstdlib>
#include <cstring>
#include <limits>
#include <string>
#include <string_view>

namespace rml::carbon
{
	namespace
	{
		struct SharedStringValue
		{
			std::array<std::byte, 16> hash;
			std::string value;
		};

		class VariantCleanup
		{
		public:
			explicit VariantCleanup(RBX::Reflection::Variant& variant) noexcept :
			    m_variant(variant)
			{
			}

			~VariantCleanup()
			{
				const auto* ops = static_cast<const void* const*>(m_variant.value_ops());
				if (ops && ops[2])
					reinterpret_cast<void (*)(void*)>(const_cast<void*>(ops[2]))(m_variant.storage());
			}

		private:
			RBX::Reflection::Variant& m_variant;
		};

		constexpr std::array<std::string_view, 7> supported_types{
		    "BinaryString",
		    "SharedString",
		    "ContentId",
		    "NetAssetRef",
		    "OptionalCFrame",
		    "OptionalCoordinateFrame",
		    "UniqueId",
		};
		constexpr std::array<std::string_view, 4> model_serialized_types{
		    "NetAssetRef",
		    "PhysicalProperties",
		    "Region3",
		    "ColorSequence",
		};

		// TriangleCount is continuously recomputed edit-mode state. CollisionGroups
		// is an inaccessible synthetic alias; CollisionGroupData is the actual
		// persisted BinaryString field and remains eligible through the normal path.
		constexpr std::array<std::string_view, 2> excluded_runtime_properties{
		    "PartOperation.TriangleCount",
		    "Workspace.CollisionGroups",
		};

		[[nodiscard]] bool contains(const auto& values, const std::string_view candidate) noexcept
		{
			for (const auto value : values)
			{
				if (value == candidate)
					return true;
			}
			return false;
		}

		[[nodiscard]] bool has_type(
		    const RBX::Reflection::PropertyDescriptor& descriptor,
		    const std::string_view candidate) noexcept
		{
			const auto* t = descriptor.type();
			if (!t) return false;
			const auto* n = t->name();
			const std::string_view name_str = n ? n->to_string() : "";
			const auto* tag = t->tag();
			const std::string_view tag_str = tag ? tag->to_string() : "";
			return name_str == candidate || tag_str == candidate;
		}

		[[nodiscard]] bool is_valid_base_hierarchy(const RBX::Reflection::ClassDescriptor* descriptor) noexcept
		{
			if (!descriptor)
				return false;
			const auto* current = descriptor;
			constexpr std::size_t max_depth = 64;
			std::array<const RBX::Reflection::ClassDescriptor*, max_depth> visited{};
			std::size_t depth = 0;
			while (current != nullptr && depth < max_depth)
			{
				for (std::size_t i = 0; i < depth; ++i)
					if (visited[i] == current)
						return false;
				visited[depth++] = current;

				const auto base_res = ::get_roblox_internals_profile().reflection().base_class(current);
				if (!base_res)
					return false;
				current = *base_res;
			}
			return current == nullptr;
		}
	}

	bool SerializedPropertyAccess::is_binary_type(const RBX::Reflection::PropertyDescriptor& descriptor) noexcept
	{
		const auto* t = descriptor.type();
		if (!t) return false;
		const auto* n = t->name();
		const std::string_view name_str = n ? n->to_string() : "";
		const auto* tag = t->tag();
		const std::string_view tag_str = tag ? tag->to_string() : "";
		return contains(supported_types, name_str) || contains(supported_types, tag_str);
	}

	bool SerializedPropertyAccess::read_content_source_type(
	    const RBX::Reflection::PropertyDescriptor& descriptor,
	    const RBX::Reflection::DescribedBase& instance,
	    ContentSourceType& out_source_type)
	{
		// ContentId shares the "Content" reflection tag but stores a string.
		// The SourceType discriminator is valid only for the concrete Content
		// descriptor/Variant type.
		const auto* t = descriptor.type();
		const auto* n = t ? t->name() : nullptr;
		if (!n || n->to_string() != "Content" ||
		    !is_valid_base_hierarchy(instance.try_get_descriptor()) ||
		    !is_valid_base_hierarchy(descriptor.owner()))
			return false;

		RBX::Reflection::Variant variant;
		descriptor.get_variant(&instance, variant);
		VariantCleanup cleanup(variant);
		if (variant.is_void())
			return false;
		const auto* variant_name = variant.type().name();
		if (!variant_name || variant_name->to_string() != "Content")
			return false;

		std::uint32_t source_type{};
		std::memcpy(&source_type, variant.storage(), sizeof(source_type));
		switch (source_type)
		{
		case static_cast<std::uint32_t>(ContentSourceType::None):
		case static_cast<std::uint32_t>(ContentSourceType::Uri):
		case static_cast<std::uint32_t>(ContentSourceType::Object):
			out_source_type = static_cast<ContentSourceType>(source_type);
			return true;
		default: return false;
		}
	}

	bool SerializedPropertyAccess::is_supported_type(const RBX::Reflection::PropertyDescriptor& descriptor) noexcept
	{
		const auto* t = descriptor.type();
		if (!t) return false;
		const auto* n = t->name();
		const std::string_view name_str = n ? n->to_string() : "";
		const auto* tag = t->tag();
		const std::string_view tag_str = tag ? tag->to_string() : "";
		if (is_binary_type(descriptor) || t->is_enum() || name_str == "SecurityCapabilities" || tag_str == "SecurityCapabilities")
			return true;
		const auto marshal_kind = dotnet::TypeMarshaler::classify(*t).kind;
		if (marshal_kind == dotnet::MarshalKind::Blittable || marshal_kind == dotnet::MarshalKind::Sequence)
			return true;

		using namespace RBX::Reflection;
		switch (t->type_id())
		{
		case TypeId::Bool:
		case TypeId::Int:
		case TypeId::Int64:
		case TypeId::Integer:
		case TypeId::Float:
		case TypeId::Double:
		case TypeId::String: return true;
		default: return false;
		}
	}

	bool SerializedPropertyAccess::is_explicitly_excluded(const RBX::Reflection::PropertyDescriptor& descriptor) noexcept
	{
		const auto* o = descriptor.owner();
		const auto* o_name = o ? o->name() : nullptr;
		const auto* d_name = descriptor.name();
		if (!o_name || !d_name) return false;
		const std::string_view owner = o_name->to_string();
		const std::string_view name = d_name->to_string();
		const std::string qualified = std::string(owner) + "." + std::string(name);
		return contains(excluded_runtime_properties, qualified);
	}

	bool SerializedPropertyAccess::is_accessible(const RBX::Reflection::PropertyDescriptor& descriptor) noexcept
	{
		// The current engine no longer exposes reliable XML/scriptability bits for
		// hidden binary descriptors. Carbon's reflection database selects canonical,
		// serialized, non-scriptable properties before a request reaches this seam;
		// native code independently narrows that request to transport-safe engine
		// types and the explicit identity/runtime exclusions above.
		return is_supported_type(descriptor) && !is_explicitly_excluded(descriptor);
	}

	bool SerializedPropertyAccess::is_copyable(const RBX::Reflection::PropertyDescriptor& descriptor) noexcept
	{
		const auto* t = descriptor.type();
		const auto* n = t ? t->name() : nullptr;
		const std::string_view name_str = n ? n->to_string() : "";
		const auto* tag = t ? t->tag() : nullptr;
		const std::string_view tag_str = tag ? tag->to_string() : "";
		return !is_explicitly_excluded(descriptor) &&
		       (is_accessible(descriptor) || contains(model_serialized_types, name_str) ||
		        contains(model_serialized_types, tag_str));
	}

	bool SerializedPropertyAccess::describe(
	    const RBX::Reflection::PropertyDescriptor& descriptor,
	    dotnet::SerializedPropertyInfo& out_info)
	{
		if (!is_valid_base_hierarchy(descriptor.owner()))
			return false;
		out_info = {};
		if (descriptor.can_xml_read())
			out_info.flags |= dotnet::SerializedPropertyXmlRead;
		if (descriptor.can_xml_write())
			out_info.flags |= dotnet::SerializedPropertyXmlWrite;
		if (descriptor.is_scriptable())
			out_info.flags |= dotnet::SerializedPropertyScriptable;
		if (descriptor.is_read_only())
			out_info.flags |= dotnet::SerializedPropertyReadOnly;
		if (descriptor.is_write_only())
			out_info.flags |= dotnet::SerializedPropertyWriteOnly;
		if (is_binary_type(descriptor))
			out_info.flags |= dotnet::SerializedPropertyBinary;
		if (is_explicitly_excluded(descriptor))
			out_info.flags |= dotnet::SerializedPropertyExcluded;
		if (is_accessible(descriptor))
			out_info.flags |= dotnet::SerializedPropertyAccessible;
		const auto* t = descriptor.type();
		if (!t) return false;
		if (dotnet::TypeMarshaler::classify(*t).kind == dotnet::MarshalKind::RefInstance)
			out_info.flags |= dotnet::SerializedPropertyReference;

		const auto* n = t->name();
		const std::string_view name = n ? n->to_string() : "";
		const auto* tag_ptr = t->tag();
		const std::string_view tag = tag_ptr ? tag_ptr->to_string() : "";
		std::string_view type_name = name;
		if (contains(supported_types, tag))
			type_name = tag;
		out_info.type_name = static_cast<char*>(std::malloc(type_name.size() + 1));
		if (!out_info.type_name)
			return false;
		std::memcpy(out_info.type_name, type_name.data(), type_name.size());
		out_info.type_name[type_name.size()] = '\0';
		return true;
	}

	bool SerializedPropertyAccess::read(
	    const RBX::Reflection::PropertyDescriptor& descriptor,
	    const RBX::Reflection::DescribedBase& instance,
	    std::vector<std::byte>& out_value)
	{
		if (!is_accessible(descriptor) ||
		    !is_valid_base_hierarchy(instance.try_get_descriptor()) ||
		    !is_valid_base_hierarchy(descriptor.owner()))
			return false;

		std::string value;
		if (has_type(descriptor, "BinaryString"))
		{
			RBX::Reflection::Variant variant;
			descriptor.get_variant(&instance, variant);
			VariantCleanup cleanup(variant);
			if (variant.is_void())
				return false;
			value = *variant.try_cast<std::string>();
		}
		else if (has_type(descriptor, "SharedString"))
		{
			RBX::Reflection::Variant variant;
			descriptor.get_variant(&instance, variant);
			VariantCleanup cleanup(variant);
			if (variant.is_void())
				return false;
			const auto* shared = *variant.try_cast<const SharedStringValue*>();
			if (!shared)
			{
				// NetAssetRef uses a null interned pointer for its canonical empty
				// value. That is valid authored data, not a failed property read.
				out_value.clear();
				return true;
			}
			value = shared->value;
		}
		else if (has_type(descriptor, "UniqueId"))
		{
			RBX::Reflection::Variant variant;
			descriptor.get_variant(&instance, variant);
			VariantCleanup cleanup(variant);
			if (variant.is_void())
				return false;
			// Roblox stores UniqueId as index, timestamp, and random components in
			// memory. Carbon's exact wire form is the canonical 16-byte serde form:
			// random (i64), timestamp (u32), index (u32), all big-endian.
			const auto* storage = variant.try_cast<std::byte>();
			if (!storage)
				return false;
			uint32_t index{};
			uint32_t time{};
			int64_t random{};
			std::memcpy(&index, storage, sizeof(index));
			std::memcpy(&time, storage + 4, sizeof(time));
			std::memcpy(&random, storage + 8, sizeof(random));
			out_value.resize(16);
			for (size_t i = 0; i < 8; ++i)
				out_value[i] = static_cast<std::byte>((static_cast<uint64_t>(random) >> (56 - i * 8)) & 0xffu);
			for (size_t i = 0; i < 4; ++i)
			{
				out_value[8 + i] = static_cast<std::byte>((time >> (24 - i * 8)) & 0xffu);
				out_value[12 + i] = static_cast<std::byte>((index >> (24 - i * 8)) & 0xffu);
			}
			return true;
		}
		else if (has_type(descriptor, "NetAssetRef"))
		{
			// NetAssetRef is an opaque engine object, not SharedString's direct
			// interned payload layout. The managed bridge batches these reads
			// through SerializationService and Carbon extracts their exact RBXM
			// bytes; never guess at the wrapper's private memory representation.
			return false;
		}
		else if (has_type(descriptor, "OptionalCFrame") || has_type(descriptor, "OptionalCoordinateFrame"))
		{
			RBX::Reflection::Variant variant;
			descriptor.get_variant(&instance, variant);
			VariantCleanup cleanup(variant);
			if (variant.is_void())
				return false;
			// MSVC's optional stores the 48-byte CoordinateFrame first and its
			// engaged flag immediately after it. The wire format is one presence
			// byte followed by those exact CFrame bits.
			const auto* storage = variant.try_cast<std::byte>();
			const bool engaged = storage[48] != std::byte{};
			out_value.resize(engaged ? 49 : 1);
			out_value[0] = engaged ? std::byte{1} : std::byte{0};
			if (engaged)
				std::memcpy(out_value.data() + 1, storage, 48);
			return true;
		}
		else if (has_type(descriptor, "SecurityCapabilities"))
		{
			RBX::Reflection::Variant variant;
			descriptor.get_variant(&instance, variant);
			VariantCleanup cleanup(variant);
			if (variant.is_void())
				return false;
			const auto* bytes = variant.try_cast<std::byte>();
			if (!bytes)
				return false;
			uint64_t capabilities{};
			std::memcpy(&capabilities, bytes, sizeof(capabilities));
			value = std::to_string(capabilities);
		}
		else if ((descriptor.type() && descriptor.type()->is_enum()) || (descriptor.type() && descriptor.type()->type_id() == RBX::Reflection::TypeId::Integer))
		{
			RBX::Reflection::Variant variant;
			descriptor.get_variant(&instance, variant);
			VariantCleanup cleanup(variant);
			if (variant.is_void())
				return false;
			value = std::to_string(*variant.try_cast<int>());
		}
		else if ((descriptor.type() && descriptor.type()->type_id() == RBX::Reflection::TypeId::Int64) ||
		         (descriptor.type() && descriptor.type()->type_id() == RBX::Reflection::TypeId::Float) ||
		         (descriptor.type() && descriptor.type()->type_id() == RBX::Reflection::TypeId::Double))
		{
			RBX::Reflection::Variant variant;
			descriptor.get_variant(&instance, variant);
			VariantCleanup cleanup(variant);
			if (variant.is_void())
				return false;
			const size_t size = descriptor.type()->type_id() == RBX::Reflection::TypeId::Int64
			                        ? sizeof(int64_t)
			                        : descriptor.type()->type_id() == RBX::Reflection::TypeId::Float ? sizeof(float)
			                                                                                   : sizeof(double);
			const auto* bytes = variant.try_cast<std::byte>();
			if (!bytes)
				return false;
			out_value.assign(bytes, bytes + size);
			return true;
		}
		else if (const auto plan = descriptor.type() ? dotnet::TypeMarshaler::classify(*descriptor.type()) : dotnet::MarshalPlan{};
		         plan.kind == dotnet::MarshalKind::Blittable || plan.kind == dotnet::MarshalKind::Sequence)
		{
			RBX::Reflection::Variant variant;
			descriptor.get_variant(&instance, variant);
			VariantCleanup cleanup(variant);
			if (variant.is_void())
				return false;
			const auto* bytes = variant.try_cast<std::byte>();
			if (!bytes)
				return false;
			if (plan.kind == dotnet::MarshalKind::Sequence)
			{
				const auto* header = reinterpret_cast<const dotnet::engine_vector_header*>(bytes);
				const auto begin = reinterpret_cast<uintptr_t>(header->begin);
				const auto end = reinterpret_cast<uintptr_t>(header->end);
				if (plan.byte_size == 0 || end < begin)
					return false;

				const auto byte_count = end - begin;
				if (byte_count % plan.byte_size != 0 ||
				    byte_count / plan.byte_size > static_cast<size_t>(std::numeric_limits<int32_t>::max()) ||
				    byte_count > out_value.max_size() - sizeof(int32_t))
				{
					return false;
				}

				const auto count = static_cast<int32_t>(byte_count / plan.byte_size);
				out_value.resize(sizeof(count) + byte_count);
				std::memcpy(out_value.data(), &count, sizeof(count));
				if (byte_count != 0)
					std::memcpy(out_value.data() + sizeof(count), header->begin, byte_count);
				return true;
			}
			out_value.assign(bytes, bytes + plan.byte_size);
			return true;
		}
		else
		{
			// Scalars and ContentId retain the engine's
			// string converter.
			value = descriptor.get_string_value(&instance).to_string();
		}
		out_value.resize(value.size());
		if (!value.empty())
			std::memcpy(out_value.data(), value.data(), value.size());
		return true;
	}

	bool SerializedPropertyAccess::write(
	    const RBX::Reflection::PropertyDescriptor& descriptor,
	    RBX::Reflection::DescribedBase& instance,
	    const std::span<const std::byte> value)
	{
		if (!is_accessible(descriptor) ||
		    !is_valid_base_hierarchy(instance.try_get_descriptor()) ||
		    !is_valid_base_hierarchy(descriptor.owner()))
			return false;

		const std::string text{reinterpret_cast<const char*>(value.data()), value.size()};
		if (has_type(descriptor, "BinaryString"))
		{
			RBX::Reflection::Variant variant;
			descriptor.get_variant(&instance, variant);
			VariantCleanup cleanup(variant);
			if (variant.is_void())
				return false;
			*variant.try_cast<std::string>() = text;
			descriptor.set_variant(&instance, variant);
			return true;
		}
		if (has_type(descriptor, "SharedString") || has_type(descriptor, "NetAssetRef"))
		{
			// SharedString and NetAssetRef values are interned engine objects. They are materialized
			// through SerializationService and copied by descriptor in the bridge's
			// dedicated copy path rather than forged here.
			return false;
		}
		if (has_type(descriptor, "OptionalCFrame") || has_type(descriptor, "OptionalCoordinateFrame"))
		{
			if (value.size() != 1 && value.size() != 49)
				return false;
			const bool engaged = value[0] != std::byte{};
			if (engaged != (value.size() == 49))
				return false;
			RBX::Reflection::Variant variant;
			descriptor.get_variant(&instance, variant);
			VariantCleanup cleanup(variant);
			if (variant.is_void())
				return false;
			auto* storage = variant.try_cast<std::byte>();
			if (engaged)
				std::memcpy(storage, value.data() + 1, 48);
			storage[48] = engaged ? std::byte{1} : std::byte{0};
			descriptor.set_variant(&instance, variant);
			return true;
		}
		if (has_type(descriptor, "SecurityCapabilities"))
		{
			uint64_t capabilities{};
			const auto [end, error] = std::from_chars(text.data(), text.data() + text.size(), capabilities);
			if (error != std::errc{} || end != text.data() + text.size())
				return false;
			RBX::Reflection::Variant variant;
			descriptor.get_variant(&instance, variant);
			VariantCleanup cleanup(variant);
			if (variant.is_void())
				return false;
			auto* destination = variant.try_cast<std::byte>();
			if (!destination)
				return false;
			std::memcpy(destination, &capabilities, sizeof(capabilities));
			descriptor.set_variant(&instance, variant);
			return true;
		}
		if (const auto* t = descriptor.type(); t != nullptr)
		{
			const auto plan = dotnet::TypeMarshaler::classify(*t);
			if (plan.kind == dotnet::MarshalKind::Blittable || plan.kind == dotnet::MarshalKind::Sequence)
			{
				if (plan.kind == dotnet::MarshalKind::Blittable)
				{
					if (value.size() != plan.byte_size)
						return false;
				}
				else
				{
					if (value.size() < sizeof(int32_t) || plan.byte_size == 0)
						return false;
					int32_t count{};
					std::memcpy(&count, value.data(), sizeof(count));
					if (count < 0 ||
					    static_cast<size_t>(count) >
					        (std::numeric_limits<size_t>::max() - sizeof(count)) / plan.byte_size ||
					    value.size() != sizeof(count) + static_cast<size_t>(count) * plan.byte_size)
					{
						return false;
					}
				}
				dotnet::InteropVariant encoded{};
				encoded.tag = dotnet::InteropValueTag::Blittable;
				encoded.as_instance = reinterpret_cast<uintptr_t>(value.data());
				return dotnet::TypeMarshaler::decode_property(&descriptor, &instance, encoded);
			}
		}
		const auto* t = descriptor.type();
		if (!t)
			return false;
		const auto type_id = t->type_id();
		if (t->is_enum() || type_id == RBX::Reflection::TypeId::Integer)
		{
			int parsed{};
			const auto [end, error] = std::from_chars(text.data(), text.data() + text.size(), parsed);
			if (error != std::errc{} || end != text.data() + text.size())
				return false;
			RBX::Reflection::Variant variant;
			descriptor.get_variant(&instance, variant);
			VariantCleanup cleanup(variant);
			if (variant.is_void())
				return false;
			*variant.try_cast<int>() = parsed;
			descriptor.set_variant(&instance, variant);
			return true;
		}
		if (type_id == RBX::Reflection::TypeId::Int64 ||
		    type_id == RBX::Reflection::TypeId::Float ||
		    type_id == RBX::Reflection::TypeId::Double)
		{
			const size_t size = type_id == RBX::Reflection::TypeId::Int64
			                        ? sizeof(int64_t)
			                        : type_id == RBX::Reflection::TypeId::Float ? sizeof(float)
			                                                                  : sizeof(double);
			if (value.size() != size)
				return false;
			RBX::Reflection::Variant variant;
			descriptor.get_variant(&instance, variant);
			VariantCleanup cleanup(variant);
			if (variant.is_void())
				return false;
			auto* destination = variant.try_cast<std::byte>();
			if (!destination)
				return false;
			std::memcpy(destination, value.data(), size);
			descriptor.set_variant(&instance, variant);
			return true;
		}
		return descriptor.set_string_value(&instance, text);
	}

	bool SerializedPropertyAccess::copy(
	    const RBX::Reflection::PropertyDescriptor& source_descriptor,
	    const RBX::Reflection::DescribedBase& source,
	    const RBX::Reflection::PropertyDescriptor& destination_descriptor,
	    RBX::Reflection::DescribedBase& destination)
	{
		// A carrier can intentionally have a different class from its source. In
		// particular, MaterialVariant.PhysicalProperties is serialized through a
		// parentable Part. Calling the Part descriptor's copy_value with a
		// MaterialVariant source applies BasePart offsets to an unrelated object and
		// crashes Studio. Read with the source descriptor and write with the
		// destination descriptor so Roblox still owns the private value's lifetime
		// and type-safe assignment without assuming a shared owner layout.
		const auto* src_t = source_descriptor.type();
		const auto* dst_t = destination_descriptor.type();
		const auto* src_n = src_t ? src_t->name() : nullptr;
		const auto* dst_n = dst_t ? dst_t->name() : nullptr;
		if (is_explicitly_excluded(source_descriptor) || is_explicitly_excluded(destination_descriptor) ||
		    !src_n || !dst_n || src_n->to_string() != dst_n->to_string() ||
		    !is_valid_base_hierarchy(source.try_get_descriptor()) ||
		    !is_valid_base_hierarchy(source_descriptor.owner()) ||
		    !is_valid_base_hierarchy(destination.try_get_descriptor()) ||
		    !is_valid_base_hierarchy(destination_descriptor.owner()))
			return false;

		RBX::Reflection::Variant variant;
		source_descriptor.get_variant(&source, variant);
		VariantCleanup cleanup(variant);
		if (variant.is_void())
			return false;
		destination_descriptor.set_variant(&destination, variant);
		return true;
	}

}
