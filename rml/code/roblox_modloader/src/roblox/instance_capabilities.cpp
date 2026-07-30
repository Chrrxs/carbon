#include "RobloxModLoader/roblox/internals_profile.hpp"
#include "RobloxModLoader/roblox/instance.hpp"
#include "RobloxModLoader/roblox/util/rbx_str.hpp"

#ifdef _WIN32
#include <Windows.h>
#endif

#include <cstring>
#include <limits>
#include <optional>

namespace
{
	bool is_readable(const void* address, const std::size_t size) noexcept
	{
		if (!address || size == 0)
			return false;
#ifdef _WIN32
		MEMORY_BASIC_INFORMATION region{};
		if (VirtualQuery(address, &region, sizeof(region)) != sizeof(region) || region.State != MEM_COMMIT ||
			(region.Protect & (PAGE_GUARD | PAGE_NOACCESS)) != 0)
			return false;
		const auto begin = reinterpret_cast<std::uintptr_t>(address);
		return begin <= std::numeric_limits<std::uintptr_t>::max() - size &&
			begin + size <= reinterpret_cast<std::uintptr_t>(region.BaseAddress) + region.RegionSize;
#else
		return true;
#endif
	}
	std::optional<std::uintptr_t> field_address(const void* object, const std::ptrdiff_t offset, const std::size_t size) noexcept
	{
		if (!object || offset < 0)
			return std::nullopt;
		const auto base = reinterpret_cast<std::uintptr_t>(object);
		const auto displacement = static_cast<std::uintptr_t>(offset);
		if (base > std::numeric_limits<std::uintptr_t>::max() - displacement)
			return std::nullopt;
		const auto address = base + displacement;
		return is_readable(reinterpret_cast<const void*>(address), size) ? std::optional{address} : std::nullopt;
	}
	template<typename T> T* read_pointer(const void* object, const std::ptrdiff_t offset) noexcept
	{
		const auto address = field_address(object, offset, sizeof(void*));
		std::uintptr_t value{};
		if (!address)
			return nullptr;
		std::memcpy(&value, reinterpret_cast<const void*>(*address), sizeof(value));
		return value && (value % alignof(void*)) == 0 ? reinterpret_cast<T*>(value) : nullptr;
	}
	struct RobloxStringLayout
	{
		union { const char* heap; char sso[RBX::rbx_str::sso_capacity + 1]; };
		std::uint64_t size;
		std::uint64_t capacity;
	};
	static_assert(sizeof(RobloxStringLayout) == sizeof(RBX::rbx_str));
}

namespace rml::roblox::internals
{
	InstanceCapabilities::InstanceCapabilities(
		const std::ptrdiff_t parent_offset,
		const std::ptrdiff_t children_offset,
		const std::ptrdiff_t name_offset) noexcept :
		m_parent_offset(parent_offset), m_children_offset(children_offset), m_name_offset(name_offset)
	{
	}
	RBX::Instance* InstanceCapabilities::parent(const RBX::Instance* instance) const noexcept
	{
		return read_pointer<RBX::Instance>(instance, m_parent_offset);
	}
	std::vector<std::shared_ptr<RBX::Instance>>* InstanceCapabilities::children(const RBX::Instance* instance) const noexcept
	{
		auto* result = read_pointer<std::vector<std::shared_ptr<RBX::Instance>>>(instance, m_children_offset);
		return result && is_readable(result, sizeof(*result)) ? result : nullptr;
	}
	std::string_view InstanceCapabilities::name(const RBX::Instance* instance) const noexcept
	{
		constexpr std::size_t maximum_name_length = 256;
		const auto address = field_address(instance, m_name_offset, sizeof(RobloxStringLayout));
		if (!address)
			return {};
		RobloxStringLayout value{};
		std::memcpy(&value, reinterpret_cast<const void*>(*address), sizeof(value));
		if (value.size > maximum_name_length)
			return {};
		const char* data{};
		if (value.capacity == RBX::rbx_str::sso_capacity)
		{
			if (value.size > RBX::rbx_str::sso_capacity)
				return {};
			data = reinterpret_cast<const RobloxStringLayout*>(*address)->sso;
		}
		else
		{
			if (!value.heap || value.capacity < value.size || value.capacity > maximum_name_length ||
				!is_readable(value.heap, static_cast<std::size_t>(value.size) + 1))
				return {};
			data = value.heap;
		}
		return data[value.size] == '\0' ? std::string_view{data, static_cast<std::size_t>(value.size)} : std::string_view{};
	}
}
