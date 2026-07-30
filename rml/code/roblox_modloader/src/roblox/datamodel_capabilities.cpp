#include "RobloxModLoader/roblox/internals_profile.hpp"
#include "RobloxModLoader/roblox/data_model.hpp"

#ifdef _WIN32
#include <Windows.h>
#endif
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <limits>
#include <string_view>

namespace
{
	constexpr std::string_view capability_name = "DataModel.RuntimeIdentity";
	constexpr std::string_view instance_type_name = ".?AVInstance@RBX@@";
	constexpr std::size_t maximum_base_classes = 100;

#ifdef _WIN32
	bool is_readable(const void* address, const std::size_t size) noexcept
	{
		if (address == nullptr || size == 0)
			return false;
		MEMORY_BASIC_INFORMATION region{};
		if (VirtualQuery(address, &region, sizeof(region)) != sizeof(region))
			return false;
		if (region.State != MEM_COMMIT || (region.Protect & (PAGE_GUARD | PAGE_NOACCESS)) != 0)
			return false;
		const auto begin = reinterpret_cast<std::uintptr_t>(address);
		if (size > std::numeric_limits<std::uintptr_t>::max() - begin)
			return false;
		const auto end = begin + size;
		const auto region_begin = reinterpret_cast<std::uintptr_t>(region.BaseAddress);
		if (region.RegionSize > std::numeric_limits<std::uintptr_t>::max() - region_begin)
			return false;
		return end <= region_begin + region.RegionSize;
	}
#else
	bool is_readable(const void* address, const std::size_t size) noexcept
	{
		return address != nullptr && size > 0;
	}
#endif

	bool checked_add_signed(
		const std::uintptr_t base,
		const std::int32_t displacement,
		std::uintptr_t& result) noexcept
	{
		if (displacement >= 0)
		{
			const auto positive = static_cast<std::uintptr_t>(displacement);
			if (positive > std::numeric_limits<std::uintptr_t>::max() - base)
				return false;
			result = base + positive;
			return true;
		}

		const auto magnitude = static_cast<std::uint64_t>(-static_cast<std::int64_t>(displacement));
		if (magnitude > base)
			return false;
		result = base - static_cast<std::uintptr_t>(magnitude);
		return true;
	}

	bool is_in_module(
		const std::uintptr_t address,
		const std::size_t size,
		const std::uintptr_t module_base,
		const std::size_t module_size) noexcept
	{
		if (module_base == 0 || module_size == 0 || address < module_base)
			return false;
		if (module_size > std::numeric_limits<std::uintptr_t>::max() - module_base)
			return false;
		if (size > std::numeric_limits<std::uintptr_t>::max() - address)
			return false;
		return address + size <= module_base + module_size;
	}

	bool resolve_module_rva(
		const std::uintptr_t module_base,
		const std::size_t module_size,
		const std::int32_t rva,
		const std::size_t size,
		std::uintptr_t& result) noexcept
	{
		return checked_add_signed(module_base, rva, result) &&
			is_in_module(result, size, module_base, module_size) &&
			is_readable(reinterpret_cast<const void*>(result), size);
	}

#pragma pack(push, 1)
	struct TypeDescriptor
	{
		const void* type_info_vft;
		void* spare;
		char name[1];
	};

	struct CompleteObjectLocator
	{
		std::uint32_t signature;
		std::uint32_t offset;
		std::uint32_t constructor_displacement;
		std::int32_t type_descriptor_offset;
		std::int32_t class_hierarchy_offset;
		std::int32_t self_offset;
	};

	struct ClassHierarchyDescriptor
	{
		std::uint32_t signature;
		std::uint32_t attributes;
		std::uint32_t num_base_classes;
		std::int32_t base_class_array_offset;
	};

	struct BaseClassDescriptor
	{
		std::int32_t type_descriptor_offset;
		std::uint32_t num_contained_bases;
		std::int32_t member_displacement[3];
		std::uint32_t attributes;
		std::int32_t class_hierarchy_offset;
	};
#pragma pack(pop)

	std::unexpected<rml::roblox::internals::CompatibilityError> failure(
		const rml::roblox::internals::CompatibilityFailure reason) noexcept
	{
		return std::unexpected(rml::roblox::internals::CompatibilityError{
			.capability = capability_name,
			.failure = reason,
		});
	}

	std::expected<const CompleteObjectLocator*, rml::roblox::internals::CompatibilityError> resolve_col(
		const void* subobject,
		const std::uintptr_t module_base,
		const std::size_t module_size) noexcept
	{
		using rml::roblox::internals::CompatibilityFailure;

		if (subobject == nullptr)
			return static_cast<const CompleteObjectLocator*>(nullptr);
		if (!is_readable(subobject, sizeof(void*)))
			return failure(CompatibilityFailure::invalid_address_range);

		const auto vtable = *reinterpret_cast<void* const*>(subobject);
		const auto vtable_address = reinterpret_cast<std::uintptr_t>(vtable);
		if (vtable_address < sizeof(void*))
			return failure(CompatibilityFailure::invalid_address_range);
		const auto col_slot = vtable_address - sizeof(void*);
		if (!is_in_module(col_slot, sizeof(void*), module_base, module_size) ||
			!is_readable(reinterpret_cast<const void*>(col_slot), sizeof(void*)))
		{
			return failure(CompatibilityFailure::invalid_address_range);
		}

		const auto col_address = *reinterpret_cast<const std::uintptr_t*>(col_slot);
		if (!is_in_module(col_address, sizeof(CompleteObjectLocator), module_base, module_size) ||
			!is_readable(reinterpret_cast<const void*>(col_address), sizeof(CompleteObjectLocator)))
		{
			return failure(CompatibilityFailure::invalid_address_range);
		}

		const auto* col = reinterpret_cast<const CompleteObjectLocator*>(col_address);
		if (col->signature != 1)
			return failure(CompatibilityFailure::unsupported_instruction_form);
		if (col->self_offset < 0 || col_address < static_cast<std::uintptr_t>(col->self_offset))
			return failure(CompatibilityFailure::invalid_address_range);
		if (col_address - static_cast<std::uintptr_t>(col->self_offset) != module_base)
			return failure(CompatibilityFailure::insufficient_evidence);
		if (col->constructor_displacement != 0)
			return failure(CompatibilityFailure::unsupported_instruction_form);

		return col;
	}
}

namespace rml::roblox::internals
{
	DataModelCapabilities::DataModelCapabilities(
		const std::uintptr_t module_base,
		const std::size_t module_size,
		const std::ptrdiff_t type_offset) noexcept :
		m_module_base(module_base),
		m_module_size(module_size),
		m_type_offset(type_offset)
	{
	}

	std::expected<RBX::DataModel*, CompatibilityError> DataModelCapabilities::job_subobject_to_data_model(
		const void* job_subobject) const noexcept
	{
		if (job_subobject == nullptr)
			return nullptr;

		const auto col_result = resolve_col(job_subobject, m_module_base, m_module_size);
		if (!col_result)
			return std::unexpected(col_result.error());
		const auto* col = *col_result;

		const auto subobject_address = reinterpret_cast<std::uintptr_t>(job_subobject);
		if (subobject_address < col->offset)
			return failure(CompatibilityFailure::invalid_address_range);
		const auto complete_object_address = subobject_address - col->offset;

		std::uintptr_t chd_address = 0;
		if (!resolve_module_rva(
				m_module_base,
				m_module_size,
				col->class_hierarchy_offset,
				sizeof(ClassHierarchyDescriptor),
				chd_address))
		{
			return failure(CompatibilityFailure::invalid_address_range);
		}

		const auto* chd = reinterpret_cast<const ClassHierarchyDescriptor*>(chd_address);
		if (chd->num_base_classes == 0 || chd->num_base_classes > maximum_base_classes)
			return failure(CompatibilityFailure::insufficient_evidence);

		const auto array_size = sizeof(std::int32_t) * static_cast<std::size_t>(chd->num_base_classes);
		std::uintptr_t array_address = 0;
		if (!resolve_module_rva(
				m_module_base,
				m_module_size,
				chd->base_class_array_offset,
				array_size,
				array_address))
		{
			return failure(CompatibilityFailure::invalid_address_range);
		}

		const auto* rva_array = reinterpret_cast<const std::int32_t*>(array_address);
		std::size_t match_count = 0;
		std::int32_t target_mdisp = 0;
		constexpr auto required_type_descriptor_size =
			offsetof(TypeDescriptor, name) + instance_type_name.size() + 1;

		for (std::uint32_t i = 0; i < chd->num_base_classes; ++i)
		{
			std::uintptr_t bcd_address = 0;
			if (!resolve_module_rva(
					m_module_base,
					m_module_size,
					rva_array[i],
					sizeof(BaseClassDescriptor),
					bcd_address))
			{
				return failure(CompatibilityFailure::invalid_address_range);
			}
			const auto* bcd = reinterpret_cast<const BaseClassDescriptor*>(bcd_address);

			std::uintptr_t td_address = 0;
			if (!resolve_module_rva(
					m_module_base,
					m_module_size,
					bcd->type_descriptor_offset,
					required_type_descriptor_size,
					td_address))
			{
				return failure(CompatibilityFailure::invalid_address_range);
			}
			const auto* td = reinterpret_cast<const TypeDescriptor*>(td_address);
			if (std::memcmp(td->name, instance_type_name.data(), instance_type_name.size()) != 0 ||
				td->name[instance_type_name.size()] != '\0')
			{
				continue;
			}

			if (bcd->member_displacement[1] != -1)
				return failure(CompatibilityFailure::unsupported_instruction_form);
			target_mdisp = bcd->member_displacement[0];
			++match_count;
		}

		if (match_count == 0)
			return failure(CompatibilityFailure::missing_signature);
		if (match_count > 1)
			return failure(CompatibilityFailure::ambiguous_evidence);

		std::uintptr_t instance_address = 0;
		if (!checked_add_signed(complete_object_address, target_mdisp, instance_address) ||
			(instance_address % alignof(void*)) != 0 ||
			!is_readable(reinterpret_cast<const void*>(instance_address), sizeof(void*)))
		{
			return failure(CompatibilityFailure::invalid_address_range);
		}

		return reinterpret_cast<RBX::DataModel*>(instance_address);
	}

	std::expected<void*, CompatibilityError> DataModelCapabilities::data_model_to_task_context(
		const RBX::DataModel* instance) const noexcept
	{
		if (instance == nullptr)
			return nullptr;

		const auto col_result = resolve_col(instance, m_module_base, m_module_size);
		if (!col_result)
			return std::unexpected(col_result.error());
		const auto* col = *col_result;

		const auto instance_address = reinterpret_cast<std::uintptr_t>(instance);
		if (instance_address < col->offset)
			return failure(CompatibilityFailure::invalid_address_range);
		const auto complete_object_address = instance_address - col->offset;
		if ((complete_object_address % alignof(void*)) != 0 ||
			!is_readable(reinterpret_cast<const void*>(complete_object_address), sizeof(void*)))
		{
			return failure(CompatibilityFailure::invalid_address_range);
		}

		return reinterpret_cast<void*>(complete_object_address);
	}

	std::expected<RBX::DataModelType, CompatibilityError> DataModelCapabilities::resolve_type(
		const RBX::DataModel* instance) const noexcept
	{
		if (instance == nullptr)
			return RBX::DataModelType::Null;
		if (m_type_offset <= 0)
			return failure(CompatibilityFailure::insufficient_evidence);

		const auto owner_result = data_model_to_task_context(instance);
		if (!owner_result)
			return std::unexpected(owner_result.error());
		const auto owner_address = reinterpret_cast<std::uintptr_t>(*owner_result);

		std::uintptr_t type_address = 0;
		if (m_type_offset > std::numeric_limits<std::int32_t>::max() ||
			!checked_add_signed(owner_address, static_cast<std::int32_t>(m_type_offset), type_address) ||
			!is_readable(reinterpret_cast<const void*>(type_address), sizeof(std::int32_t)))
		{
			return failure(CompatibilityFailure::invalid_address_range);
		}

		const auto raw_type = *reinterpret_cast<const std::int32_t*>(type_address);
		if (raw_type < static_cast<std::int32_t>(RBX::DataModelType::Edit) ||
			raw_type > static_cast<std::int32_t>(RBX::DataModelType::Null))
		{
			return failure(CompatibilityFailure::insufficient_evidence);
		}

		return static_cast<RBX::DataModelType>(raw_type);
	}
}
