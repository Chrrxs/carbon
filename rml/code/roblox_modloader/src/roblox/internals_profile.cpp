#include "RobloxModLoader/roblox/internals_profile.hpp"
#include "RobloxModLoader/roblox/datamodel_layout_resolver.hpp"
#include "RobloxModLoader/roblox/reflection/object.hpp"
#include "RobloxModLoader/roblox/job_types.hpp"

#include "RobloxModLoader/internal/memory/pe_parser.hpp"
#include "RobloxModLoader/memory/module.hpp"
#include "RobloxModLoader/internal/memory/rtti_scanner.hpp"

#include <future>
#include <algorithm>
#include <cstring>
#include <optional>
#include <span>
#include <vector>

namespace rml::roblox::internals
{
	RobloxInternalsProfile::RobloxInternalsProfile(
		ReflectionCapabilities reflection,
		DataModelCapabilities datamodel,
		InstanceCapabilities instance,
		SignalCapabilities signal,
		JobCapabilities job) noexcept :
		m_reflection(reflection),
		m_datamodel(datamodel),
		m_instance(instance),
		m_signal(signal),
		m_job(job)
	{
	}
	std::expected<RobloxInternalsProfile, CompatibilityError> RobloxInternalsProfile::resolve_bootstrap(
		const memory::module& studio_module,
		const functions::get_string_atom get_string_atom) noexcept
	{
		if (!get_string_atom)
		{
			return std::unexpected(CompatibilityError{
				.capability = "Reflection.GetStringAtom",
				.failure = CompatibilityFailure::missing_signature,
			});
		}
		memory::pe::Parser parser;
		if (!parser.parse())
		{
			return std::unexpected(CompatibilityError{
				.capability = "Executable.TextSection",
				.failure = CompatibilityFailure::invalid_address_range,
			});
		}
		const auto* text_sections = parser.get_sections_with_name(".text");
		if (!text_sections || text_sections->size() != 1 || !text_sections->front())
		{
			return std::unexpected(CompatibilityError{
				.capability = "Executable.TextSection",
				.failure = text_sections && text_sections->size() > 1 ? CompatibilityFailure::ambiguous_evidence
				                                                        : CompatibilityFailure::missing_signature,
			});
		}
		const auto* runtime_function_sections = parser.get_sections_with_name(".pdata");
		if (!runtime_function_sections || runtime_function_sections->size() != 1 ||
			!runtime_function_sections->front())
		{
			return std::unexpected(CompatibilityError{
				.capability = "Executable.RuntimeFunctions",
				.failure = runtime_function_sections && runtime_function_sections->size() > 1
					? CompatibilityFailure::ambiguous_evidence
					: CompatibilityFailure::missing_signature,
			});
		}

		const auto& text = *text_sections->front();
		const auto section_offset = text.start.value();
		if (section_offset < 0 || text.size == 0 || static_cast<std::size_t>(section_offset) > studio_module.size() ||
			text.size > studio_module.size() - static_cast<std::size_t>(section_offset))
		{
			return std::unexpected(CompatibilityError{
				.capability = "Executable.TextSection",
				.failure = CompatibilityFailure::invalid_address_range,
			});
		}
		const auto& runtime_functions = *runtime_function_sections->front();
		const auto runtime_function_offset = runtime_functions.start.value();
		if (runtime_function_offset < 0 ||
			runtime_functions.size == 0 ||
			static_cast<std::size_t>(runtime_function_offset) > studio_module.size() ||
			runtime_functions.size >
				studio_module.size() - static_cast<std::size_t>(runtime_function_offset))
		{
			return std::unexpected(CompatibilityError{
				.capability = "Executable.RuntimeFunctions",
				.failure = CompatibilityFailure::invalid_address_range,
			});
		}

		const auto module_base = studio_module.begin().as<std::uintptr_t>();
		const auto module_contains = [&](const std::uintptr_t address, const std::size_t size) {
			if (address < module_base)
				return false;
			const auto offset = address - module_base;
			return offset <= studio_module.size() && size <= studio_module.size() - offset;
		};
		const auto module_rva_address =
			[&](const std::int32_t rva, const std::size_t size) -> std::optional<std::uintptr_t> {
				if (rva < 0)
					return std::nullopt;
				const auto offset = static_cast<std::size_t>(rva);
				if (offset > studio_module.size() || size > studio_module.size() - offset)
					return std::nullopt;
				return module_base + offset;
			};
		const auto text_address = module_base + static_cast<std::size_t>(section_offset);

		const auto code = std::span{
			reinterpret_cast<const std::byte*>(text_address),
			text.size,
		};
		const auto runtime_function_table = std::span{
			reinterpret_cast<const std::byte*>(
				module_base + static_cast<std::size_t>(runtime_function_offset)),
			runtime_functions.size,
		};

		auto gather_family_vfts = [&](const char* name1, const char* name2) -> std::vector<std::uintptr_t> {
			auto candidates = memory::rtti::Scanner::get_class_rtti_candidates(name1);
			if (candidates.empty() && name2)
			{
				candidates = memory::rtti::Scanner::get_class_rtti_candidates(name2);
			}
			std::vector<std::uintptr_t> vfts;
			vfts.reserve(candidates.size());
			for (const auto& candidate : candidates)
			{
				const auto vft = reinterpret_cast<std::uintptr_t>(candidate->get_virtual_function_table());
				if (vft != 0 && vft >= module_base && vft < module_base + studio_module.size())
				{
					vfts.push_back(vft);
				}
			}
			return vfts;
		};

		const auto descriptor_vfts = gather_family_vfts("RBX::Reflection::Descriptor", "class RBX::Reflection::Descriptor");
		if (descriptor_vfts.empty())
		{
			return std::unexpected(CompatibilityError{
				.capability = "Reflection.DescriptorRTTI",
				.failure = CompatibilityFailure::missing_signature,
			});
		}

		const auto member_vfts = gather_family_vfts("RBX::Reflection::MemberDescriptor", "class RBX::Reflection::MemberDescriptor");
		if (member_vfts.empty())
		{
			return std::unexpected(CompatibilityError{
				.capability = "Reflection.MemberDescriptorRTTI",
				.failure = CompatibilityFailure::missing_signature,
			});
		}

		const auto property_vfts = gather_family_vfts("RBX::Reflection::PropertyDescriptor", "class RBX::Reflection::PropertyDescriptor");
		if (property_vfts.empty())
		{
			return std::unexpected(CompatibilityError{
				.capability = "Reflection.PropertyDescriptorRTTI",
				.failure = CompatibilityFailure::missing_signature,
			});
		}

		const auto function_vfts = gather_family_vfts("RBX::Reflection::FunctionDescriptor", "class RBX::Reflection::FunctionDescriptor");
		if (function_vfts.empty())
		{
			return std::unexpected(CompatibilityError{
				.capability = "Reflection.FunctionDescriptorRTTI",
				.failure = CompatibilityFailure::missing_signature,
			});
		}

		const auto yield_function_vfts = gather_family_vfts("RBX::Reflection::YieldFunctionDescriptor", "class RBX::Reflection::YieldFunctionDescriptor");
		if (yield_function_vfts.empty())
		{
			return std::unexpected(CompatibilityError{
				.capability = "Reflection.YieldFunctionDescriptorRTTI",
				.failure = CompatibilityFailure::missing_signature,
			});
		}

		auto gather_descendant_family_vfts =
			[&](const char* name1, const char* name2) -> std::vector<std::uintptr_t> {
			auto target_candidates = memory::rtti::Scanner::get_class_rtti_candidates(name1);
			if (target_candidates.empty())
			{
				target_candidates = memory::rtti::Scanner::get_class_rtti_candidates(name2);
			}
			if (target_candidates.empty())
			{
				return {};
			}

			std::vector<const memory::rtti::TypeDescriptor*> target_tds;
			for (const auto& candidate : target_candidates)
			{
				const auto vft = reinterpret_cast<std::uintptr_t>(candidate->get_virtual_function_table());
				if (vft != 0 && vft >= module_base && vft < module_base + studio_module.size())
				{
					auto* td = candidate->get_type_descriptor();
					if (td != nullptr)
					{
						const auto td_addr = reinterpret_cast<std::uintptr_t>(td);
						if (td_addr >= module_base && td_addr < module_base + studio_module.size())
						{
							target_tds.push_back(td);
						}
					}
				}
			}
			if (target_tds.empty())
			{
				return {};
			}

			const auto module_size = studio_module.size();
			std::vector<std::uintptr_t> family_vfts;

			for (const auto& [class_name, candidates] : memory::rtti::Scanner::get_all_classes())
			{
				for (const auto& info : candidates)
				{
					if (!info)
						continue;

					const auto vft = reinterpret_cast<std::uintptr_t>(info->get_virtual_function_table());
					if (vft == 0 || vft < module_base || vft >= module_base + module_size)
						continue;

					const auto* col = info->get_complete_object_locator();
					const auto col_addr = reinterpret_cast<std::uintptr_t>(col);
					if (col_addr < module_base || col_addr + sizeof(memory::rtti::CompleteObjectLocator) > module_base + module_size)
						continue;

					if (col->signature != 0 && col->signature != 1)
						continue;

					const auto chd_addr = module_base + static_cast<std::uint32_t>(
						col->class_hierarchy_offset.value());
					if (chd_addr < module_base || chd_addr + sizeof(memory::rtti::ClassHierarchyDescriptor) > module_base + module_size)
						continue;

					const auto* chd = reinterpret_cast<const memory::rtti::ClassHierarchyDescriptor*>(chd_addr);
					if (chd->signature != 0 && chd->signature != 1)
						continue;
					if (chd->num_base_classes == 0 || chd->num_base_classes > 100)
						continue;

					const auto bca_addr = module_base + static_cast<std::uint32_t>(
						chd->base_class_array_offset.value());
					if (bca_addr < module_base || bca_addr + chd->num_base_classes * sizeof(std::int32_t) > module_base + module_size)
						continue;

					const auto* bca = reinterpret_cast<const memory::pe::IBO32*>(bca_addr);
					bool contains_target = false;
					for (std::uint32_t i = 0; i < chd->num_base_classes; ++i)
					{
						const auto bcd_rva = static_cast<std::uint32_t>(bca[i].value());
						const auto bcd_addr = module_base + bcd_rva;
						if (bcd_addr < module_base || bcd_addr + sizeof(memory::rtti::BaseClassDescriptor) > module_base + module_size)
						{
							contains_target = false;
							break;
						}
						const auto* bcd = reinterpret_cast<const memory::rtti::BaseClassDescriptor*>(bcd_addr);
						const auto td_addr = module_base + static_cast<std::uint32_t>(
							bcd->type_descriptor_offset);
						if (td_addr < module_base || td_addr + sizeof(memory::rtti::TypeDescriptor) > module_base + module_size)
						{
							contains_target = false;
							break;
						}
						const auto* td = reinterpret_cast<const memory::rtti::TypeDescriptor*>(td_addr);
						for (const auto* target_td : target_tds)
						{
							if (td == target_td || td_addr == reinterpret_cast<std::uintptr_t>(target_td))
							{
								contains_target = true;
								break;
							}
						}
						if (contains_target)
							break;
					}

					if (contains_target)
					{
						family_vfts.push_back(vft);
					}
				}
			}

			std::sort(family_vfts.begin(), family_vfts.end());
			family_vfts.erase(std::unique(family_vfts.begin(), family_vfts.end()), family_vfts.end());
			return family_vfts;
		};

		LOG_INFO("Resolving Roblox internals: Event RTTI");
		const auto event_vfts = gather_descendant_family_vfts(
			"RBX::Reflection::EventDescriptor", "class RBX::Reflection::EventDescriptor");
		if (event_vfts.empty())
		{
			return std::unexpected(CompatibilityError{
				.capability = "Reflection.EventDescriptorRTTI",
				.failure = CompatibilityFailure::missing_signature,
			});
		}


		auto callback_vfts = gather_family_vfts("RBX::Reflection::SyncCallbackDescriptor", "class RBX::Reflection::SyncCallbackDescriptor");
		auto async_callback_vfts = gather_family_vfts("RBX::Reflection::AsyncCallbackDescriptor", "class RBX::Reflection::AsyncCallbackDescriptor");
		callback_vfts.insert(callback_vfts.end(), async_callback_vfts.begin(), async_callback_vfts.end());
		if (callback_vfts.empty())
		{
			return std::unexpected(CompatibilityError{
				.capability = "Reflection.CallbackDescriptorRTTI",
				.failure = CompatibilityFailure::missing_signature,
			});
		}

		const auto class_vfts = gather_family_vfts("RBX::Reflection::ClassDescriptor", "class RBX::Reflection::ClassDescriptor");
		if (class_vfts.empty())
		{
			return std::unexpected(CompatibilityError{
				.capability = "Reflection.ClassDescriptorRTTI",
				.failure = CompatibilityFailure::missing_signature,
			});
		}

		const ReflectionVftSets vft_sets{
			.descriptor_vfts = descriptor_vfts,
			.member_vfts = member_vfts,
			.property_vfts = property_vfts,
			.function_vfts = function_vfts,
			.yield_function_vfts = yield_function_vfts,
			.event_vfts = event_vfts,
			.callback_vfts = callback_vfts,
			.class_descriptor_vfts = class_vfts,
		};

		LOG_INFO("Resolving Roblox internals: reflection layout");
		const auto get_string_atom_addr = reinterpret_cast<std::uintptr_t>(get_string_atom);
		using ReflectionResolution = std::expected<ReflectionLayoutEvidence, CompatibilityError>;
		std::optional<std::future<ReflectionResolution>> reflection_future;
		try
		{
			reflection_future.emplace(std::async(std::launch::async, [&] {
				return resolve_reflection_layout(
					code,
					text_address,
					get_string_atom_addr,
					runtime_function_table,
					module_base,
					vft_sets);
			}));
		}
		catch (...)
		{
			// Thread creation can fail under host resource pressure. Preserve
			// compatibility by resolving synchronously at the join point.
		}

		auto datamodel_candidates =
			memory::rtti::Scanner::get_class_rtti_candidates("RBX::DataModel");
		if (datamodel_candidates.empty())
		{
			datamodel_candidates =
				memory::rtti::Scanner::get_class_rtti_candidates("class RBX::DataModel");
		}
		if (datamodel_candidates.empty())
		{
			return std::unexpected(CompatibilityError{
				.capability = "DataModel.RTTI",
				.failure = CompatibilityFailure::missing_signature,
			});
		}

		auto instance_candidates =
			memory::rtti::Scanner::get_class_rtti_candidates("RBX::Instance");
		if (instance_candidates.empty())
		{
			instance_candidates =
				memory::rtti::Scanner::get_class_rtti_candidates("class RBX::Instance");
		}
		if (instance_candidates.empty())
		{
			return std::unexpected(CompatibilityError{
				.capability = "Instance.RTTI",
				.failure = CompatibilityFailure::missing_signature,
			});
		}

		std::vector<std::uintptr_t> datamodel_vfts;
		datamodel_vfts.reserve(datamodel_candidates.size());
		for (const auto& candidate : datamodel_candidates)
		{
			const auto datamodel_vft =
				reinterpret_cast<std::uintptr_t>(candidate->get_virtual_function_table());
			if (datamodel_vft != 0 &&
				datamodel_vft >= module_base &&
				datamodel_vft < module_base + studio_module.size())
			{
				datamodel_vfts.push_back(datamodel_vft);
			}
		}
		if (datamodel_vfts.empty())
		{
			return std::unexpected(CompatibilityError{
				.capability = "DataModel.RTTI",
				.failure = CompatibilityFailure::invalid_address_range,
			});
		}

		std::vector<const memory::rtti::TypeDescriptor*> instance_type_descriptors;
		for (const auto& candidate : instance_candidates)
		{
			const auto* type_descriptor = candidate->get_type_descriptor();
			const auto address = reinterpret_cast<std::uintptr_t>(type_descriptor);
			if (module_contains(address, sizeof(memory::rtti::TypeDescriptor)))
				instance_type_descriptors.push_back(type_descriptor);
		}

		std::vector<std::ptrdiff_t> datamodel_instance_base_offsets;
		for (const auto& candidate : datamodel_candidates)
		{
			const auto* hierarchy = candidate->get_class_hierarchy_descriptor();
			const auto hierarchy_address = reinterpret_cast<std::uintptr_t>(hierarchy);
			if (!module_contains(
					hierarchy_address,
					sizeof(memory::rtti::ClassHierarchyDescriptor)) ||
				hierarchy->num_base_classes == 0 || hierarchy->num_base_classes > 100)
			{
				continue;
			}

			const auto base_array_address = module_rva_address(
				hierarchy->base_class_array_offset.value(),
				hierarchy->num_base_classes * sizeof(memory::pe::IBO32));
			if (!base_array_address)
				continue;
			const auto* base_array =
				reinterpret_cast<const memory::pe::IBO32*>(*base_array_address);
			for (std::uint32_t index = 0; index < hierarchy->num_base_classes; ++index)
			{
				const auto descriptor_address = module_rva_address(
					base_array[index].value(),
					sizeof(memory::rtti::BaseClassDescriptor));
				if (!descriptor_address)
					continue;
				const auto* descriptor =
					reinterpret_cast<const memory::rtti::BaseClassDescriptor*>(
						*descriptor_address);
				const auto type_descriptor_address = module_rva_address(
					descriptor->type_descriptor_offset,
					sizeof(memory::rtti::TypeDescriptor));
				if (!type_descriptor_address)
					continue;
				const auto* type_descriptor =
					reinterpret_cast<const memory::rtti::TypeDescriptor*>(
						*type_descriptor_address);
				if (std::ranges::find(instance_type_descriptors, type_descriptor) ==
						instance_type_descriptors.end() ||
					descriptor->member_displacement[1] != -1 ||
					descriptor->member_displacement[0] < 0)
				{
					continue;
				}
				datamodel_instance_base_offsets.push_back(
					descriptor->member_displacement[0]);
			}
		}
		std::ranges::sort(datamodel_instance_base_offsets);
		datamodel_instance_base_offsets.erase(
			std::ranges::unique(datamodel_instance_base_offsets).begin(),
			datamodel_instance_base_offsets.end());
		if (datamodel_instance_base_offsets.size() != 1)
		{
			return std::unexpected(CompatibilityError{
				.capability = "DataModel.RTTI",
				.failure = datamodel_instance_base_offsets.empty()
					? CompatibilityFailure::missing_signature
					: CompatibilityFailure::ambiguous_evidence,
			});
		}

		LOG_INFO("Resolving Roblox internals: DataModel layout");
		auto datamodel_layout = resolve_datamodel_layout(
			code,
			text_address,
			runtime_function_table,
			module_base,
			datamodel_vfts);
		if (!datamodel_layout)
			return std::unexpected(datamodel_layout.error());
		LOG_INFO("Resolved Roblox internals: DataModel layout");

		DataModelCapabilities datamodel_caps(module_base, studio_module.size(), datamodel_layout->type_offset);

		std::vector<std::uintptr_t> instance_vfts;
		std::vector<std::uintptr_t> instance_vft_entries;
		for (const auto& candidate : instance_candidates)
		{
			const auto vft = reinterpret_cast<std::uintptr_t>(candidate->get_virtual_function_table());
			if (vft != 0 && vft >= module_base && vft < module_base + studio_module.size())
			{
				instance_vfts.push_back(vft);
				const auto* vft_table = reinterpret_cast<const std::uintptr_t*>(vft);
				for (std::size_t i = 0; i < 64; ++i)
				{
					std::uintptr_t fn_addr = 0;
					std::memcpy(&fn_addr, vft_table + i, sizeof(fn_addr));
					if (fn_addr >= text_address && fn_addr < text_address + code.size())
					{
						instance_vft_entries.push_back(fn_addr);
					}
					else if (fn_addr == 0)
					{
						break;
					}
				}
			}
		}
		if (instance_vfts.empty() || instance_vft_entries.empty())
		{
			return std::unexpected(CompatibilityError{
				.capability = "Instance.RTTI",
				.failure = CompatibilityFailure::invalid_address_range,
			});
		}
		LOG_INFO("Resolving Roblox internals: Instance layout");
		auto instance_layout = resolve_instance_layout(
			code, text_address, runtime_function_table, module_base, instance_vfts, instance_vft_entries);
		if (!instance_layout)
			return std::unexpected(instance_layout.error());
		LOG_INFO("Resolved Roblox internals: Instance layout");
		InstanceCapabilities instance_caps(
			instance_layout->parent_offset,
			instance_layout->children_offset,
			instance_layout->name_offset);



		LOG_INFO("Resolving Roblox internals: Signal layout");
		auto signal_layout = resolve_signal_layout(
			code, text_address, runtime_function_table, module_base);
		if (!signal_layout)
			return std::unexpected(signal_layout.error());
		LOG_INFO("Resolved Roblox internals: Signal layout");
		SignalCapabilities signal_caps(
			signal_layout->signal_head_offset,
			signal_layout->slot_strong_offset,
			signal_layout->slot_weak_offset,
			signal_layout->slot_next_offset,
			signal_layout->slot_source_offset,
			signal_layout->slot_wrapper_ptr_offset);
		auto dm_job_candidates =
			memory::rtti::Scanner::get_class_rtti_candidates("RBX::DataModelJob");
		if (dm_job_candidates.empty())
		{
			dm_job_candidates =
				memory::rtti::Scanner::get_class_rtti_candidates("class RBX::DataModelJob");
		}
		if (dm_job_candidates.empty())
		{
			return std::unexpected(CompatibilityError{
				.capability = "Job.RTTI",
				.failure = CompatibilityFailure::missing_signature,
			});
		}
		std::vector<std::uintptr_t> dm_job_vfts;
		for (const auto& candidate : dm_job_candidates)
		{
			const auto vft = reinterpret_cast<std::uintptr_t>(candidate->get_virtual_function_table());
			if (vft != 0 && vft >= module_base && vft < module_base + studio_module.size())
				dm_job_vfts.push_back(vft);
		}
		if (dm_job_vfts.empty())
		{
			return std::unexpected(CompatibilityError{
				.capability = "Job.RTTI",
				.failure = CompatibilityFailure::invalid_address_range,
			});
		}
		auto job_candidates =
			memory::rtti::Scanner::get_class_rtti_candidates("RBX::ScriptContextFacets::WaitingHybridScriptsJob");
		if (job_candidates.empty())
		{
			job_candidates =
				memory::rtti::Scanner::get_class_rtti_candidates("class RBX::ScriptContextFacets::WaitingHybridScriptsJob");
		}
		if (job_candidates.empty())
		{
			return std::unexpected(CompatibilityError{
				.capability = "Job.RTTI",
				.failure = CompatibilityFailure::missing_signature,
			});
		}
		std::vector<std::uintptr_t> job_vfts;
		for (const auto& candidate : job_candidates)
		{
			const auto vft = reinterpret_cast<std::uintptr_t>(candidate->get_virtual_function_table());
			if (vft != 0 && vft >= module_base && vft < module_base + studio_module.size())
				job_vfts.push_back(vft);
		}
		if (job_vfts.empty())
		{
			return std::unexpected(CompatibilityError{
				.capability = "Job.RTTI",
				.failure = CompatibilityFailure::invalid_address_range,
			});
		}
		std::ranges::sort(job_vfts);
		job_vfts.erase(std::ranges::unique(job_vfts).begin(), job_vfts.end());
		std::vector<std::uintptr_t> waiting_job_step_addresses;
		for (const auto vft : job_vfts)
		{
			const auto entry_address =
				vft + rml::JobVtable::kWaitingHybridScriptsExecutionStepIndex *
					sizeof(std::uintptr_t);
			if (!module_contains(entry_address, sizeof(std::uintptr_t)))
				continue;
			std::uintptr_t step_address = 0;
			std::memcpy(
				&step_address,
				reinterpret_cast<const void*>(entry_address),
				sizeof(step_address));
			if (step_address >= text_address && step_address < text_address + code.size())
				waiting_job_step_addresses.push_back(step_address);
		}
		std::ranges::sort(waiting_job_step_addresses);
		waiting_job_step_addresses.erase(
			std::ranges::unique(waiting_job_step_addresses).begin(),
			waiting_job_step_addresses.end());
		if (waiting_job_step_addresses.size() != 1)
		{
			return std::unexpected(CompatibilityError{
				.capability = "Job.DataModelAccessor",
				.failure = waiting_job_step_addresses.empty()
					? CompatibilityFailure::missing_signature
					: CompatibilityFailure::ambiguous_evidence,
			});
		}
		LOG_INFO("Resolving Roblox internals: job layout");
		auto job_layout = resolve_job_layout(
			code,
			text_address,
			runtime_function_table,
			module_base,
			job_vfts,
			waiting_job_step_addresses.front(),
			datamodel_instance_base_offsets.front());
		if (!job_layout)
			return std::unexpected(job_layout.error());
		LOG_INFO("Resolved Roblox internals: job layout");
		JobCapabilities job_caps(
			job_layout->waiting_scripts_job_script_context_offset,
			reinterpret_cast<JobCapabilities::DataModelAccessor>(
				job_layout->waiting_scripts_job_data_model_accessor));

		auto reflection_layout = reflection_future
			? reflection_future->get()
			: resolve_reflection_layout(
				code,
				text_address,
				get_string_atom_addr,
				runtime_function_table,
				module_base,
				vft_sets);
		if (!reflection_layout)
			return std::unexpected(reflection_layout.error());
		LOG_INFO("Resolved Roblox internals: reflection layout");

		return RobloxInternalsProfile(
			ReflectionCapabilities{
				get_string_atom,
				reflection_layout->descriptor_container_offsets,
				reflection_layout->base_class_offset,
				reflection_layout->functionality_offset,
				reflection_layout->name_offset,
				reflection_layout->owner_offset,
				reflection_layout->security_offset,
				reflection_layout->property_type_offset,
				reflection_layout->property_functionality_offset,
				reflection_layout->signature_offset,
				reflection_layout->function_kind_offset,
				reflection_layout->function_invoke_func_ptr_offset,
				reflection_layout->function_bound_this_delta_offset,
				reflection_layout->callback_signature_offset,
				reflection_layout->callback_async_flag_offset,
				reflection_layout->event_signal_offset,
			},
			datamodel_caps,
			instance_caps,
			signal_caps,
			job_caps);
	}
}
