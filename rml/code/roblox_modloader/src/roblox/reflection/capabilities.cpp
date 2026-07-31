#include "RobloxModLoader/roblox/internals_profile.hpp"

#include "RobloxModLoader/roblox/data_model_job.hpp"
#include "RobloxModLoader/roblox/instance.hpp"
#include "RobloxModLoader/roblox/reflection/callback_descriptor.hpp"
#include "RobloxModLoader/roblox/reflection/event_descriptor.hpp"
#include "RobloxModLoader/roblox/reflection/function_descriptor.hpp"
#include "RobloxModLoader/roblox/reflection/object.hpp"
#include "RobloxModLoader/roblox/reflection/property_descriptor.hpp"
#include "RobloxModLoader/roblox/reflection/yield_function_descriptor.hpp"
#include "RobloxModLoader/roblox/signals.hpp"
#include "RobloxModLoader/roblox/util/rbx_str.hpp"
#include "pointers.hpp"

#ifdef _WIN32
#include <Windows.h>
#endif

#include <array>
#include <cstring>
#include <limits>
#include <optional>
#include <string_view>
#include <utility>

namespace
{
#ifdef _WIN32
	bool is_readable(const void* address, const std::size_t size) noexcept
	{
		if (!address || size == 0)
			return false;
		MEMORY_BASIC_INFORMATION region{};
		if (VirtualQuery(address, &region, sizeof(region)) != sizeof(region))
			return false;
		if (region.State != MEM_COMMIT || (region.Protect & (PAGE_GUARD | PAGE_NOACCESS)) != 0)
			return false;
		const auto begin = reinterpret_cast<std::uintptr_t>(address);
		if (begin > std::numeric_limits<std::uintptr_t>::max() - size)
			return false;
		const auto end = begin + size;
		const auto region_begin = reinterpret_cast<std::uintptr_t>(region.BaseAddress);
		if (region_begin > std::numeric_limits<std::uintptr_t>::max() - region.RegionSize)
			return false;
		return begin >= region_begin && end <= region_begin + region.RegionSize;
	}

	bool is_executable(const void* address) noexcept
	{
		if (!address)
			return false;
		MEMORY_BASIC_INFORMATION region{};
		if (VirtualQuery(address, &region, sizeof(region)) != sizeof(region) ||
			region.State != MEM_COMMIT || (region.Protect & (PAGE_GUARD | PAGE_NOACCESS)) != 0)
		{
			return false;
		}
		const auto protection = region.Protect & 0xff;
		return protection == PAGE_EXECUTE || protection == PAGE_EXECUTE_READ ||
			protection == PAGE_EXECUTE_READWRITE || protection == PAGE_EXECUTE_WRITECOPY;
	}
#else
	bool is_readable(const void* address, const std::size_t size) noexcept
	{
		return address != nullptr && size > 0;
	}

	bool is_executable(const void* address) noexcept
	{
		return address != nullptr;
	}
#endif

	std::optional<std::uintptr_t> field_address(
		const void* object,
		const std::ptrdiff_t offset,
		const std::size_t size) noexcept
	{
		if (!object || offset < 0 || size == 0)
			return std::nullopt;
		const auto base = reinterpret_cast<std::uintptr_t>(object);
		const auto displacement = static_cast<std::uintptr_t>(offset);
		if (base > std::numeric_limits<std::uintptr_t>::max() - displacement)
			return std::nullopt;
		const auto address = base + displacement;
		if (!is_readable(reinterpret_cast<const void*>(address), size))
			return std::nullopt;
		return address;
	}

	std::optional<std::uintptr_t> signed_field_address(
		const void* object,
		const std::ptrdiff_t offset,
		const std::size_t size) noexcept
	{
		if (!object || size == 0)
			return std::nullopt;
		const auto base = reinterpret_cast<std::uintptr_t>(object);
		std::uintptr_t address{};
		if (offset >= 0)
		{
			const auto displacement = static_cast<std::uintptr_t>(offset);
			if (base > std::numeric_limits<std::uintptr_t>::max() - displacement)
				return std::nullopt;
			address = base + displacement;
		}
		else
		{
			const auto magnitude = static_cast<std::uintptr_t>(-(offset + 1)) + 1;
			if (magnitude > base)
				return std::nullopt;
			address = base - magnitude;
		}
		if (!is_readable(reinterpret_cast<const void*>(address), size))
			return std::nullopt;
		return address;
	}

	template<typename T>
	std::optional<T> read_field(const void* object, const std::ptrdiff_t offset) noexcept
	{
		const auto address = field_address(object, offset, sizeof(T));
		if (!address)
			return std::nullopt;
		T value{};
		std::memcpy(&value, reinterpret_cast<const void*>(*address), sizeof(value));
		return value;
	}

	template<typename T>
	T* read_pointer_field(const void* object, const std::ptrdiff_t offset) noexcept
	{
		const auto pointer = read_field<std::uintptr_t>(object, offset);
		if (!pointer || *pointer == 0 || (*pointer % alignof(void*)) != 0)
			return nullptr;
		return reinterpret_cast<T*>(*pointer);
	}

	struct RobloxStringLayout
	{
		union
		{
			const char* heap;
			char sso[RBX::rbx_str::sso_capacity + 1];
		};
		std::uint64_t size;
		std::uint64_t capacity;
	};
	static_assert(sizeof(RobloxStringLayout) == sizeof(RBX::rbx_str));

	std::optional<std::string_view> read_roblox_string(
		const void* object,
		const std::ptrdiff_t offset) noexcept
	{
		constexpr std::size_t maximum_name_length = 256;
		const auto address = field_address(object, offset, sizeof(RobloxStringLayout));
		if (!address)
			return std::nullopt;
		RobloxStringLayout value{};
		std::memcpy(&value, reinterpret_cast<const void*>(*address), sizeof(value));
		if (value.size > maximum_name_length)
			return std::nullopt;

		const char* data = nullptr;
		if (value.capacity == RBX::rbx_str::sso_capacity)
		{
			if (value.size > RBX::rbx_str::sso_capacity)
				return std::nullopt;
			data = reinterpret_cast<const RobloxStringLayout*>(*address)->sso;
		}
		else
		{
			if (value.capacity < value.size || value.capacity > maximum_name_length || !value.heap)
				return std::nullopt;
			data = value.heap;
			if (!is_readable(data, static_cast<std::size_t>(value.size) + 1))
				return std::nullopt;
		}
		if (data[value.size] != '\0')
			return std::nullopt;
		return std::string_view{data, static_cast<std::size_t>(value.size)};
	}

	constexpr std::size_t descriptor_collection_entry_size = sizeof(std::uintptr_t) * 2;
	constexpr std::size_t maximum_descriptor_collection_entries = 100000;

	struct DescriptorCollectionView
	{
		std::uintptr_t entries;
		std::size_t count;
	};

	std::optional<DescriptorCollectionView> descriptor_collection(
		const void* descriptor,
		const std::ptrdiff_t storage_offset) noexcept
	{
		const auto storage = field_address(
			descriptor,
			storage_offset,
			sizeof(std::uintptr_t) * 3);
		if (!storage)
			return std::nullopt;

		std::array<std::uintptr_t, 3> words{};
		std::memcpy(words.data(), reinterpret_cast<const void*>(*storage), sizeof(words));
		const auto entries = words[0];
		const auto count = words[1];
		const auto capacity = words[2];
		if (count > capacity || count > maximum_descriptor_collection_entries)
			return std::nullopt;
		if (count == 0)
			return DescriptorCollectionView{entries, 0};
		if (!entries || (entries % alignof(void*)) != 0)
			return std::nullopt;
		const auto bytes = static_cast<std::size_t>(count) * descriptor_collection_entry_size;
		if (!is_readable(reinterpret_cast<const void*>(entries), bytes))
			return std::nullopt;
		return DescriptorCollectionView{entries, static_cast<std::size_t>(count)};
	}
}

namespace rml::roblox::internals
{
	ReflectionCapabilities::ReflectionCapabilities(
		const functions::get_string_atom get_string_atom,
		const std::array<std::ptrdiff_t, 5> descriptor_container_offsets,
		const std::ptrdiff_t base_class_offset,
		const std::ptrdiff_t functionality_offset,
		const std::ptrdiff_t name_offset,
		const std::ptrdiff_t owner_offset,
		const std::ptrdiff_t security_offset,
		const std::ptrdiff_t property_type_offset,
		const std::ptrdiff_t property_functionality_offset,
		const std::ptrdiff_t type_tag_offset,
		const std::ptrdiff_t type_id_offset,
		const std::ptrdiff_t type_is_float_offset,
		const std::ptrdiff_t type_is_number_offset,
		const std::ptrdiff_t type_is_enum_offset,
		const std::ptrdiff_t signature_offset,
		const std::ptrdiff_t function_kind_offset,
		const std::ptrdiff_t function_invoke_func_ptr_offset,
		const std::ptrdiff_t function_bound_this_delta_offset,
		const std::ptrdiff_t callback_signature_offset,
		const std::ptrdiff_t callback_async_flag_offset,
		const std::ptrdiff_t event_signal_offset) noexcept :
		m_get_string_atom(get_string_atom),
		m_descriptor_container_offsets(descriptor_container_offsets),
		m_base_class_offset(base_class_offset),
		m_functionality_offset(functionality_offset),
		m_name_offset(name_offset),
		m_owner_offset(owner_offset),
		m_security_offset(security_offset),
		m_property_type_offset(property_type_offset),
		m_property_functionality_offset(property_functionality_offset),
		m_type_tag_offset(type_tag_offset),
		m_type_id_offset(type_id_offset),
		m_type_is_float_offset(type_is_float_offset),
		m_type_is_number_offset(type_is_number_offset),
		m_type_is_enum_offset(type_is_enum_offset),
		m_signature_offset(signature_offset),
		m_function_kind_offset(function_kind_offset),
		m_function_invoke_func_ptr_offset(function_invoke_func_ptr_offset),
		m_function_bound_this_delta_offset(function_bound_this_delta_offset),
		m_callback_signature_offset(callback_signature_offset),
		m_callback_async_flag_offset(callback_async_flag_offset),
		m_event_signal_offset(event_signal_offset)
	{
	}

	const RBX::Name* ReflectionCapabilities::descriptor_name(
		const RBX::Reflection::Descriptor* descriptor) const noexcept
	{
		const auto* name = read_pointer_field<const RBX::Name>(descriptor, m_name_offset);
		return name && is_readable(name, sizeof(void*)) ? name : nullptr;
	}

	std::expected<const RBX::Reflection::ClassDescriptor*, CompatibilityError> ReflectionCapabilities::base_class(
		const RBX::Reflection::ClassDescriptor* descriptor) const noexcept
	{
		if (!descriptor)
			return nullptr;
		if (m_base_class_offset <= 0)
			return std::unexpected(CompatibilityError{"Reflection.BaseClass", CompatibilityFailure::insufficient_evidence});
		const auto pointer = read_field<std::uintptr_t>(descriptor, m_base_class_offset);
		if (!pointer)
			return std::unexpected(CompatibilityError{"Reflection.BaseClass", CompatibilityFailure::invalid_address_range});
		if (*pointer == 0)
			return nullptr;
		if (*pointer < 0x10000 || (*pointer % alignof(void*)) != 0 ||
			!is_readable(reinterpret_cast<const void*>(*pointer), sizeof(void*) * 2))
		{
			return std::unexpected(CompatibilityError{"Reflection.BaseClass", CompatibilityFailure::invalid_address_range});
		}
		return reinterpret_cast<const RBX::Reflection::ClassDescriptor*>(*pointer);
	}

	bool ReflectionCapabilities::is_a(
		const RBX::Reflection::ClassDescriptor* descriptor,
		const RBX::Reflection::ClassDescriptor* target) const noexcept
	{
		if (!descriptor || !target)
			return false;
		const auto* target_atom = descriptor_name(target);
		std::array<const RBX::Reflection::ClassDescriptor*, 64> visited{};
		std::size_t depth = 0;
		for (auto* current = descriptor; current && depth < visited.size();)
		{
			if (current == target || (target_atom && descriptor_name(current) == target_atom))
				return true;
			for (std::size_t index = 0; index < depth; ++index)
				if (visited[index] == current)
					return false;
			visited[depth++] = current;
			const auto base = base_class(current);
			if (!base)
				return false;
			current = *base;
		}
		return false;
	}

	bool ReflectionCapabilities::is_a(
		const RBX::Reflection::ClassDescriptor* descriptor,
		const char* target_name) const noexcept
	{
		if (!descriptor || !target_name || !m_get_string_atom)
			return false;
		const auto atom_address = m_get_string_atom(target_name);
		if (!atom_address)
			return false;
		const auto* target_atom = reinterpret_cast<const RBX::Name*>(atom_address);
		std::array<const RBX::Reflection::ClassDescriptor*, 64> visited{};
		std::size_t depth = 0;
		for (auto* current = descriptor; current && depth < visited.size();)
		{
			if (descriptor_name(current) == target_atom)
				return true;
			for (std::size_t index = 0; index < depth; ++index)
				if (visited[index] == current)
					return false;
			visited[depth++] = current;
			const auto base = base_class(current);
			if (!base)
				return false;
			current = *base;
		}
		return false;
	}

	bool ReflectionCapabilities::is_serializable(
		const RBX::Reflection::ClassDescriptor* descriptor) const noexcept
	{
		const auto flags = read_field<std::uint32_t>(descriptor, m_functionality_offset);
		return flags && ((*flags & 0x8u) != 0);
	}

	const RBX::Reflection::ClassDescriptor* ReflectionCapabilities::member_owner(
		const RBX::Reflection::MemberDescriptor* member) const noexcept
	{
		auto* owner = read_pointer_field<const RBX::Reflection::ClassDescriptor>(member, m_owner_offset);
		return owner && is_readable(owner, sizeof(void*)) ? owner : nullptr;
	}

	RBX::Security::Permissions ReflectionCapabilities::member_security(
		const RBX::Reflection::MemberDescriptor* member) const noexcept
	{
		const auto value = read_field<std::uint32_t>(member, m_security_offset);
		return value ? static_cast<RBX::Security::Permissions>(*value) : RBX::Security::Permissions::None;
	}

	const RBX::Reflection::Type* ReflectionCapabilities::property_type(
		const RBX::Reflection::PropertyDescriptor* property) const noexcept
	{
		auto* type = read_pointer_field<const RBX::Reflection::Type>(property, m_property_type_offset);
		return type && is_readable(type, sizeof(void*)) ? type : nullptr;
	}
	const RBX::Name* ReflectionCapabilities::type_tag(
		const RBX::Reflection::Type* type) const noexcept
	{
		const auto* tag = read_pointer_field<const RBX::Name>(type, m_type_tag_offset);
		return tag && is_readable(tag, sizeof(void*)) ? tag : nullptr;
	}

	int ReflectionCapabilities::type_id(const RBX::Reflection::Type* type) const noexcept
	{
		return read_field<std::int32_t>(type, m_type_id_offset).value_or(0);
	}

	bool ReflectionCapabilities::type_is_float(const RBX::Reflection::Type* type) const noexcept
	{
		return read_field<bool>(type, m_type_is_float_offset).value_or(false);
	}

	bool ReflectionCapabilities::type_is_number(const RBX::Reflection::Type* type) const noexcept
	{
		return read_field<bool>(type, m_type_is_number_offset).value_or(false);
	}

	bool ReflectionCapabilities::type_is_enum(const RBX::Reflection::Type* type) const noexcept
	{
		return read_field<bool>(type, m_type_is_enum_offset).value_or(false);
	}

	std::uint32_t ReflectionCapabilities::property_functionality(
		const RBX::Reflection::PropertyDescriptor* property) const noexcept
	{
		return read_field<std::uint32_t>(property, m_property_functionality_offset).value_or(0);
	}

	bool ReflectionCapabilities::property_is_public(const RBX::Reflection::PropertyDescriptor* property) const noexcept
	{
		return (property_functionality(property) & (1u << 0)) != 0;
	}
	bool ReflectionCapabilities::property_is_editable(const RBX::Reflection::PropertyDescriptor* property) const noexcept
	{
		return (property_functionality(property) & (1u << 1)) != 0;
	}
	bool ReflectionCapabilities::property_can_replicate(const RBX::Reflection::PropertyDescriptor* property) const noexcept
	{
		return (property_functionality(property) & (1u << 2)) != 0;
	}
	bool ReflectionCapabilities::property_can_xml_read(const RBX::Reflection::PropertyDescriptor* property) const noexcept
	{
		return (property_functionality(property) & (1u << 3)) != 0;
	}
	bool ReflectionCapabilities::property_can_xml_write(const RBX::Reflection::PropertyDescriptor* property) const noexcept
	{
		return (property_functionality(property) & (1u << 4)) != 0;
	}
	bool ReflectionCapabilities::property_is_scriptable(const RBX::Reflection::PropertyDescriptor* property) const noexcept
	{
		return (property_functionality(property) & (1u << 5)) != 0;
	}
	bool ReflectionCapabilities::property_always_clone(const RBX::Reflection::PropertyDescriptor* property) const noexcept
	{
		return (property_functionality(property) & (1u << 6)) != 0;
	}

	const RBX::Reflection::SignatureDescriptor* ReflectionCapabilities::function_signature(
		const RBX::Reflection::FunctionDescriptor* function) const noexcept
	{
		const auto address = field_address(function, m_signature_offset, sizeof(RBX::Reflection::SignatureDescriptor));
		return address ? reinterpret_cast<const RBX::Reflection::SignatureDescriptor*>(*address) : nullptr;
	}
	std::uint32_t ReflectionCapabilities::function_kind(
		const RBX::Reflection::FunctionDescriptor* function) const noexcept
	{
		return read_field<std::uint32_t>(function, m_function_kind_offset).value_or(0);
	}
	void* ReflectionCapabilities::function_invoke_func_ptr(
		const RBX::Reflection::FunctionDescriptor* function) const noexcept
	{
		auto* pointer = read_pointer_field<void>(function, m_function_invoke_func_ptr_offset);
		return is_executable(pointer) ? pointer : nullptr;
	}
	std::intptr_t ReflectionCapabilities::function_bound_this_delta(
		const RBX::Reflection::FunctionDescriptor* function) const noexcept
	{
		return read_field<std::int32_t>(function, m_function_bound_this_delta_offset).value_or(0);
	}
	const RBX::Reflection::SignatureDescriptor* ReflectionCapabilities::yield_signature(
		const RBX::Reflection::YieldFunctionDescriptor* function) const noexcept
	{
		const auto address = field_address(function, m_signature_offset, sizeof(RBX::Reflection::SignatureDescriptor));
		return address ? reinterpret_cast<const RBX::Reflection::SignatureDescriptor*>(*address) : nullptr;
	}
	const RBX::Reflection::SignatureDescriptor* ReflectionCapabilities::callback_signature(
		const RBX::Reflection::CallbackDescriptor* callback) const noexcept
	{
		const auto address = field_address(callback, m_callback_signature_offset, sizeof(RBX::Reflection::SignatureDescriptor));
		return address ? reinterpret_cast<const RBX::Reflection::SignatureDescriptor*>(*address) : nullptr;
	}
	bool ReflectionCapabilities::callback_is_async(
		const RBX::Reflection::CallbackDescriptor* callback) const noexcept
	{
		return read_field<std::uint8_t>(callback, m_callback_async_flag_offset).value_or(0) != 0;
	}
	const RBX::Reflection::SignatureDescriptor* ReflectionCapabilities::event_signature(
		const RBX::Reflection::EventDescriptor* event) const noexcept
	{
		const auto address = field_address(event, m_signature_offset, sizeof(RBX::Reflection::SignatureDescriptor));
		return address ? reinterpret_cast<const RBX::Reflection::SignatureDescriptor*>(*address) : nullptr;
	}

	RBX::Signals::Signal* ReflectionCapabilities::event_signal(
		const RBX::Reflection::EventDescriptor* event,
		const void* event_source) const noexcept
	{
		const auto displacement = read_field<std::int32_t>(event, m_event_signal_offset);
		if (!displacement)
			return nullptr;
		const auto address = signed_field_address(event_source, *displacement, sizeof(void*));
		if (!address)
			return nullptr;
		std::uintptr_t pointer{};
		std::memcpy(&pointer, reinterpret_cast<const void*>(*address), sizeof(pointer));
		return pointer ? reinterpret_cast<RBX::Signals::Signal*>(pointer) : nullptr;
	}

	void* ReflectionCapabilities::find_member_in_family(
		const RBX::Reflection::ClassDescriptor* descriptor,
		const char* name,
		const std::size_t family_index) const noexcept
	{
		if (!descriptor || !name || family_index >= m_descriptor_container_offsets.size() || !m_get_string_atom)
			return nullptr;
		const auto atom_address = m_get_string_atom(name);
		if (!atom_address)
			return nullptr;
		const auto* target_atom = reinterpret_cast<const RBX::Name*>(atom_address);

		std::array<const RBX::Reflection::ClassDescriptor*, 64> visited{};
		std::size_t depth = 0;
		for (auto* current = descriptor; current && depth < visited.size();)
		{
			for (std::size_t index = 0; index < depth; ++index)
				if (visited[index] == current)
					return nullptr;
			visited[depth++] = current;

			const auto collection = descriptor_collection(
				current,
				m_descriptor_container_offsets[family_index]);
			if (collection)
			{
				for (std::size_t index = 0; index < collection->count; ++index)
				{
					std::uintptr_t member{};
					std::memcpy(
						&member,
						reinterpret_cast<const void*>(
							collection->entries + index * descriptor_collection_entry_size),
						sizeof(member));
					if (member && (member % alignof(void*)) == 0 &&
						descriptor_name(reinterpret_cast<const RBX::Reflection::Descriptor*>(member)) == target_atom)
					{
						return reinterpret_cast<void*>(member);
					}
				}
			}
			const auto base = base_class(current);
			if (!base)
				return nullptr;
			current = *base;
		}
		return nullptr;
	}

	RBX::Reflection::PropertyDescriptor* ReflectionCapabilities::find_property(
		const RBX::Reflection::ClassDescriptor* descriptor, const char* name) const noexcept
	{
		return static_cast<RBX::Reflection::PropertyDescriptor*>(find_member_in_family(descriptor, name, 0));
	}
	RBX::Reflection::EventDescriptor* ReflectionCapabilities::find_event(
		const RBX::Reflection::ClassDescriptor* descriptor, const char* name) const noexcept
	{
		return static_cast<RBX::Reflection::EventDescriptor*>(find_member_in_family(descriptor, name, 1));
	}
	RBX::Reflection::FunctionDescriptor* ReflectionCapabilities::find_function(
		const RBX::Reflection::ClassDescriptor* descriptor, const char* name) const noexcept
	{
		return static_cast<RBX::Reflection::FunctionDescriptor*>(find_member_in_family(descriptor, name, 2));
	}
	RBX::Reflection::YieldFunctionDescriptor* ReflectionCapabilities::find_yield_function(
		const RBX::Reflection::ClassDescriptor* descriptor, const char* name) const noexcept
	{
		return static_cast<RBX::Reflection::YieldFunctionDescriptor*>(find_member_in_family(descriptor, name, 3));
	}
	RBX::Reflection::CallbackDescriptor* ReflectionCapabilities::find_callback(
		const RBX::Reflection::ClassDescriptor* descriptor, const char* name) const noexcept
	{
		return static_cast<RBX::Reflection::CallbackDescriptor*>(find_member_in_family(descriptor, name, 4));
	}

	std::optional<PropertyDescriptorSpanView> ReflectionCapabilities::property_descriptors(
		const RBX::Reflection::ClassDescriptor* descriptor) const noexcept
	{
		const auto collection = descriptor_collection(
			descriptor,
			m_descriptor_container_offsets[0]);
		if (!collection)
			return std::nullopt;
		return PropertyDescriptorSpanView{
			reinterpret_cast<const PropertyDescriptorCollectionEntry*>(collection->entries),
			collection->count};
	}

}
