#include "RobloxModLoader/roblox/internals_profile.hpp"

#ifdef _WIN32
#include <Windows.h>
#endif

#include <cstring>
#include <limits>

namespace
{
	bool read_pointer_field(const void* object, const std::ptrdiff_t offset, void*& value) noexcept
	{
		value = nullptr;
		if (!object || offset < 0)
			return false;
		const auto base = reinterpret_cast<std::uintptr_t>(object);
		const auto displacement = static_cast<std::uintptr_t>(offset);
		if (base > (std::numeric_limits<std::uintptr_t>::max)() - displacement)
			return false;
		const auto address = base + displacement;
#ifdef _WIN32
		MEMORY_BASIC_INFORMATION region{};
		if (VirtualQuery(reinterpret_cast<const void*>(address), &region, sizeof(region)) != sizeof(region) ||
			region.State != MEM_COMMIT || (region.Protect & (PAGE_GUARD | PAGE_NOACCESS)) != 0 ||
			address + sizeof(value) > reinterpret_cast<std::uintptr_t>(region.BaseAddress) + region.RegionSize)
			return false;
#endif
		std::memcpy(&value, reinterpret_cast<const void*>(address), sizeof(value));
		return value == nullptr || (reinterpret_cast<std::uintptr_t>(value) % alignof(void*)) == 0;
	}
}

namespace rml::roblox::internals
{
	JobCapabilities::JobCapabilities(
		const std::ptrdiff_t waiting_scripts_job_script_context_offset,
		const DataModelAccessor waiting_scripts_job_data_model_accessor) noexcept :
		m_waiting_scripts_job_script_context_offset(waiting_scripts_job_script_context_offset),
		m_waiting_scripts_job_data_model_accessor(waiting_scripts_job_data_model_accessor)
	{
	}
	RBX::ScriptContext* JobCapabilities::get_script_context(
		const RBX::ScriptContextFacets::WaitingHybridScriptsJob* job) const noexcept
	{
		void* value{};
		return read_pointer_field(job, m_waiting_scripts_job_script_context_offset, value)
			? static_cast<RBX::ScriptContext*>(value) : nullptr;
	}
	RBX::DataModel* JobCapabilities::get_data_model(
		const RBX::ScriptContextFacets::WaitingHybridScriptsJob* job) const noexcept
	{
		const auto script_context = get_script_context(job);
		return script_context != nullptr && m_waiting_scripts_job_data_model_accessor != nullptr
			? m_waiting_scripts_job_data_model_accessor(script_context)
			: nullptr;
	}
}
