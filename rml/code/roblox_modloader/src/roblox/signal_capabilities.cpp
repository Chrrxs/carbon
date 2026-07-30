#include "RobloxModLoader/roblox/internals_profile.hpp"

#include "RobloxModLoader/roblox/data_model_job.hpp"
#include "RobloxModLoader/roblox/instance.hpp"
#include "RobloxModLoader/roblox/signals.hpp"
#include "RobloxModLoader/roblox/util/rbx_str.hpp"
#include "pointers.hpp"

#ifdef _WIN32
#include <Windows.h>
#endif

#include <cstring>
#include <limits>
#include <optional>

namespace
{
#ifdef _WIN32
	bool is_readable(const void* address, const std::size_t size) noexcept
	{
		if (!address || size == 0)
			return false;
		MEMORY_BASIC_INFORMATION region{};
		if (VirtualQuery(address, &region, sizeof(region)) != sizeof(region) || region.State != MEM_COMMIT ||
			(region.Protect & (PAGE_GUARD | PAGE_NOACCESS)) != 0)
			return false;
		const auto begin = reinterpret_cast<std::uintptr_t>(address);
		if (begin > std::numeric_limits<std::uintptr_t>::max() - size)
			return false;
		const auto region_begin = reinterpret_cast<std::uintptr_t>(region.BaseAddress);
		return begin >= region_begin && begin + size <= region_begin + region.RegionSize;
	}
#else
	bool is_readable(const void* address, const std::size_t size) noexcept { return address && size; }
#endif

	std::optional<std::uintptr_t> field_address(const void* object, const std::ptrdiff_t offset, const std::size_t size) noexcept
	{
		if (!object || offset < 0 || size == 0)
			return std::nullopt;
		const auto base = reinterpret_cast<std::uintptr_t>(object);
		const auto displacement = static_cast<std::uintptr_t>(offset);
		if (base > std::numeric_limits<std::uintptr_t>::max() - displacement)
			return std::nullopt;
		const auto address = base + displacement;
		return is_readable(reinterpret_cast<const void*>(address), size) ? std::optional{address} : std::nullopt;
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

	std::optional<std::string_view> read_roblox_string(const void* object, const std::ptrdiff_t offset) noexcept
	{
		constexpr std::size_t maximum_name_length = 256;
		const auto address = field_address(object, offset, sizeof(RobloxStringLayout));
		if (!address)
			return std::nullopt;
		RobloxStringLayout value{};
		std::memcpy(&value, reinterpret_cast<const void*>(*address), sizeof(value));
		if (value.size > maximum_name_length)
			return std::nullopt;
		const char* data{};
		if (value.capacity == RBX::rbx_str::sso_capacity)
		{
			if (value.size > RBX::rbx_str::sso_capacity)
				return std::nullopt;
			data = reinterpret_cast<const RobloxStringLayout*>(*address)->sso;
		}
		else
		{
			if (!value.heap || value.capacity < value.size || value.capacity > maximum_name_length ||
				!is_readable(value.heap, static_cast<std::size_t>(value.size) + 1))
				return std::nullopt;
			data = value.heap;
		}
		return data[value.size] == '\0' ? std::optional{std::string_view{data, static_cast<std::size_t>(value.size)}} : std::nullopt;
	}
}

namespace rml::roblox::internals
{
	SignalCapabilities::SignalCapabilities(
		const std::ptrdiff_t signal_head_offset,
		const std::ptrdiff_t slot_strong_offset,
		const std::ptrdiff_t slot_weak_offset,
		const std::ptrdiff_t slot_next_offset,
		const std::ptrdiff_t slot_source_offset,
		const std::ptrdiff_t slot_wrapper_ptr_offset) noexcept :
		m_signal_head_offset(signal_head_offset), m_slot_strong_offset(slot_strong_offset),
		m_slot_weak_offset(slot_weak_offset), m_slot_next_offset(slot_next_offset),
		m_slot_source_offset(slot_source_offset), m_slot_wrapper_ptr_offset(slot_wrapper_ptr_offset)
	{
	}

	RBX::Signals::Signal* SignalCapabilities::get_signal(
		const RBX::Reflection::EventDescriptor* descriptor, const void* event_source) const noexcept
	{
		const auto* profile = ::try_get_roblox_internals_profile();
		return profile ? profile->reflection().event_signal(descriptor, event_source) : nullptr;
	}
	RBX::Signals::Slot* SignalCapabilities::get_head(const RBX::Signals::Signal* signal) const noexcept
	{
		return read_pointer_field<RBX::Signals::Slot>(signal, m_signal_head_offset);
	}
	RBX::Signals::Slot* SignalCapabilities::get_next(const RBX::Signals::Slot* slot) const noexcept
	{
		return read_pointer_field<RBX::Signals::Slot>(slot, m_slot_next_offset);
	}
	void* SignalCapabilities::get_source(const RBX::Signals::Slot* slot) const noexcept
	{
		return read_pointer_field<void>(slot, m_slot_source_offset);
	}
	void* SignalCapabilities::get_wrapper_ptr(const RBX::Signals::Slot* slot) const noexcept
	{
		return read_pointer_field<void>(slot, m_slot_wrapper_ptr_offset);
	}
	bool SignalCapabilities::is_connected(const RBX::Signals::Slot* slot) const noexcept
	{
		return read_field<long>(slot, m_slot_strong_offset).value_or(0) > 0;
	}
	void SignalCapabilities::observe_slot(RBX::Signals::Slot* slot) const noexcept
	{
		if (const auto address = field_address(slot, m_slot_weak_offset, sizeof(long)))
			_InterlockedIncrement(reinterpret_cast<volatile long*>(*address));
	}
	void SignalCapabilities::release_slot(RBX::Signals::Slot* slot) const noexcept
	{
		const auto address = field_address(slot, m_slot_weak_offset, sizeof(long));
		if (address && _InterlockedExchangeAdd(reinterpret_cast<volatile long*>(*address), -1) == 1 &&
			g_pointers && g_pointers->m_roblox_pointers.signal_slot_free)
			g_pointers->m_roblox_pointers.signal_slot_free(slot);
	}
	void SignalCapabilities::disconnect_slot(RBX::Signals::Slot* slot) const noexcept
	{
		if (slot && g_pointers && g_pointers->m_roblox_pointers.signal_disconnect)
			g_pointers->m_roblox_pointers.signal_disconnect(slot);
	}
	std::vector<RBX::Signals::Connection> SignalCapabilities::snapshot_connections(
		const RBX::Reflection::EventDescriptor* descriptor, void* event_source) const noexcept
	{
		std::vector<RBX::Signals::Connection> result;
		auto* signal = get_signal(descriptor, event_source);
		if (!signal)
			return result;
		void* mutex = g_pointers && g_pointers->m_roblox_pointers.signal_mutex_get
			? g_pointers->m_roblox_pointers.signal_mutex_get() : nullptr;
		if (mutex)
			_Mtx_lock(static_cast<_Mtx_t>(mutex));
		for (auto* slot = get_head(signal); slot; slot = get_next(slot))
			if (get_source(slot))
				result.push_back(RBX::Signals::Connection::observe(slot));
		if (mutex)
			_Mtx_unlock(static_cast<_Mtx_t>(mutex));
		return result;
	}

}
