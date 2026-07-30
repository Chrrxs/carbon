#include "RobloxModLoader/roblox/internals_profile.hpp"
#include "RobloxModLoader/roblox/data_model.hpp"
#include "RobloxModLoader/roblox/datamodel_layout_resolver.hpp"
#include "datamodel_constructor_fixtures.hpp"

#include <cstdlib>
#include <cstdint>
#include <cstring>
#include <iostream>
#include <span>
#include <vector>

namespace
{
	using rml::roblox::internals::CompatibilityFailure;
	using rml::roblox::internals::DataModelCapabilities;
	using rml::roblox::internals::resolve_datamodel_layout;

#pragma pack(push, 1)
	struct TypeDescriptorRaw
	{
		const void* type_info_vft{nullptr};
		void* spare{nullptr};
		char name[32]{};
	};

	struct BaseClassDescriptorRaw
	{
		std::int32_t type_descriptor_offset{0};
		std::uint32_t num_contained_bases{1};
		std::int32_t mdisp{0};
		std::int32_t pdisp{-1};
		std::int32_t vdisp{0};
		std::uint32_t attributes{0};
		std::int32_t class_hierarchy_offset{0};
	};

	struct ClassHierarchyDescriptorRaw
	{
		std::uint32_t signature{0};
		std::uint32_t attributes{0};
		std::uint32_t num_base_classes{1};
		std::int32_t base_class_array_offset{0};
	};

	struct CompleteObjectLocatorRaw
	{
		std::uint32_t signature{1};
		std::uint32_t offset{0};
		std::uint32_t constructor_displacement{0};
		std::int32_t type_descriptor_offset{0};
		std::int32_t class_hierarchy_offset{0};
		std::int32_t self_offset{0};
	};

	struct RuntimeFunctionEntryRaw
	{
		std::uint32_t begin_address;
		std::uint32_t end_address;
		std::uint32_t unwind_info_address;
	};
#pragma pack(pop)

	struct SyntheticModule
	{
		alignas(16) std::vector<std::uint8_t> buffer;
		std::uintptr_t base{};
		std::size_t size{};

		explicit SyntheticModule(const std::size_t total_size = 0x20000) :
			buffer(total_size, 0),
			base(reinterpret_cast<std::uintptr_t>(buffer.data())),
			size(total_size)
		{
		}

		std::int32_t rva(const void* pointer) const
		{
			return static_cast<std::int32_t>(reinterpret_cast<std::uintptr_t>(pointer) - base);
		}
	};

	struct RttiFixture
	{
		static constexpr std::size_t complete_offset = 0x2000;
		static constexpr std::size_t job_offset = 0x8;
		static constexpr std::size_t instance_offset = 0x1C8;

		SyntheticModule module{0x10000};
		DataModelCapabilities capabilities{module.base, module.size, 0x908};
		std::uint8_t* complete_object{module.buffer.data() + complete_offset};
		void* job_subobject{complete_object + job_offset};
		RBX::DataModel* instance{reinterpret_cast<RBX::DataModel*>(complete_object + instance_offset)};
		CompleteObjectLocatorRaw* job_col{reinterpret_cast<CompleteObjectLocatorRaw*>(module.buffer.data() + 0x3000)};
		void** job_vtable{reinterpret_cast<void**>(module.buffer.data() + 0x3100)};
		TypeDescriptorRaw* job_type{reinterpret_cast<TypeDescriptorRaw*>(module.buffer.data() + 0x3200)};
		TypeDescriptorRaw* instance_type{reinterpret_cast<TypeDescriptorRaw*>(module.buffer.data() + 0x3300)};
		BaseClassDescriptorRaw* instance_base{reinterpret_cast<BaseClassDescriptorRaw*>(module.buffer.data() + 0x3400)};
		std::int32_t* base_array{reinterpret_cast<std::int32_t*>(module.buffer.data() + 0x3500)};
		ClassHierarchyDescriptorRaw* hierarchy{reinterpret_cast<ClassHierarchyDescriptorRaw*>(module.buffer.data() + 0x3600)};
		CompleteObjectLocatorRaw* instance_col{reinterpret_cast<CompleteObjectLocatorRaw*>(module.buffer.data() + 0x3700)};
		void** instance_vtable{reinterpret_cast<void**>(module.buffer.data() + 0x3800)};

		RttiFixture()
		{
			job_vtable[-1] = job_col;
			job_vtable[0] = reinterpret_cast<void*>(0xdeadbeef);
			*reinterpret_cast<void**>(job_subobject) = job_vtable;

			std::strcpy(job_type->name, ".?AVJobSubobject@@");
			std::strcpy(instance_type->name, ".?AVInstance@RBX@@");
			instance_base->type_descriptor_offset = module.rva(instance_type);
			instance_base->mdisp = static_cast<std::int32_t>(instance_offset);
			instance_base->pdisp = -1;
			base_array[0] = module.rva(instance_base);
			hierarchy->num_base_classes = 1;
			hierarchy->base_class_array_offset = module.rva(base_array);

			job_col->signature = 1;
			job_col->offset = static_cast<std::uint32_t>(job_offset);
			job_col->type_descriptor_offset = module.rva(job_type);
			job_col->class_hierarchy_offset = module.rva(hierarchy);
			job_col->self_offset = module.rva(job_col);

			instance_vtable[-1] = instance_col;
			instance_vtable[0] = module.buffer.data();
			*reinterpret_cast<void**>(instance) = instance_vtable;
			instance_col->signature = 1;
			instance_col->offset = static_cast<std::uint32_t>(instance_offset);
			instance_col->type_descriptor_offset = module.rva(instance_type);
			instance_col->class_hierarchy_offset = module.rva(hierarchy);
			instance_col->self_offset = module.rva(instance_col);
		}
	};

	void emit_lea_rip(
		std::vector<std::uint8_t>& code,
		const bool use_r8,
		const std::uintptr_t instruction_address,
		const std::uintptr_t target_address)
	{
		const auto displacement = static_cast<std::int32_t>(target_address - (instruction_address + 7));
		if (use_r8)
			code.insert(code.end(), {0x4C, 0x8D, 0x05});
		else
			code.insert(code.end(), {0x48, 0x8D, 0x05});
		const auto* bytes = reinterpret_cast<const std::uint8_t*>(&displacement);
		code.insert(code.end(), bytes, bytes + sizeof(displacement));
	}

	void copy_code(SyntheticModule& module, const std::vector<std::uint8_t>& code)
	{
		std::memcpy(module.buffer.data() + 0x1000, code.data(), code.size());
	}

	auto resolve_with_runtime_functions(
		const std::span<const std::byte> code,
		const std::uintptr_t code_address,
		const std::span<const RuntimeFunctionEntryRaw> runtime_functions,
		const std::uintptr_t module_address,
		const std::uintptr_t vft_address)
	{
		return resolve_datamodel_layout(
			code,
			code_address,
			std::as_bytes(runtime_functions),
			module_address,
			std::span{&vft_address, 1});
	}

	auto resolve_captured_layout(
		const std::span<const std::byte> code,
		const std::uintptr_t code_address,
		const std::uintptr_t module_address,
		const std::uint32_t function_begin,
		const std::uint32_t function_end,
		const std::uintptr_t vft_address)
	{
		const RuntimeFunctionEntryRaw runtime_function{
			.begin_address = function_begin,
			.end_address = function_end,
			.unwind_info_address = 1,
		};
		return resolve_with_runtime_functions(
			code,
			code_address,
			std::span{&runtime_function, 1},
			module_address,
			vft_address);
	}

	auto resolve_layout(
		SyntheticModule& module,
		const std::vector<std::uint8_t>& code,
		const std::uintptr_t vft_address)
	{
		copy_code(module, code);
		return resolve_captured_layout(
			std::span{reinterpret_cast<const std::byte*>(module.buffer.data() + 0x1000), code.size()},
			module.base + 0x1000,
			module.base,
			0x1000,
			static_cast<std::uint32_t>(0x1000 + code.size()),
			vft_address);
	}

	template<typename T>
	bool failed_with(
		const std::expected<T, rml::roblox::internals::CompatibilityError>& result,
		const CompatibilityFailure expected)
	{
		return !result && result.error().failure == expected;
	}
}

const rml::roblox::internals::RobloxInternalsProfile& get_roblox_internals_profile()
{
	std::abort();
}

const rml::roblox::internals::RobloxInternalsProfile* try_get_roblox_internals_profile() noexcept
{
	return nullptr;
}

namespace RBX::Reflection
{
	void EventSource::process_remote_event(const EventDescriptor&, const EventArguments&, const SystemAddress&)
	{
	}

	void EventSource::raise_event_invocation(const EventDescriptor&, const EventArguments&, const SystemAddress*)
	{
	}
}

int main()
{
	// Captured exact 0.731 and 0.732 constructors must derive the same runtime type field.
	{
		using namespace rml::tests::fixtures;
		const auto studio_0731_layout = resolve_captured_layout(
			studio_0731_datamodel_constructor,
			studio_0731_code_address,
			studio_0731_image_base,
			studio_0731_function_begin_rva,
			studio_0731_function_end_rva,
			studio_0731_vft_address);
		const auto studio_0732_layout = resolve_captured_layout(
			studio_0732_datamodel_constructor,
			studio_0732_code_address,
			studio_0732_image_base,
			studio_0732_function_begin_rva,
			studio_0732_function_end_rva,
			studio_0732_vft_address);
		if (!studio_0731_layout || studio_0731_layout->type_offset != 0x908 ||
			!studio_0732_layout || studio_0732_layout->type_offset != 0x908)
		{
			std::cerr << "Captured DataModel constructors did not resolve\n";
			return 15;
		}
	}
	// Captured evidence without the enum-use guard is insufficient.
	{
		using namespace rml::tests::fixtures;
		auto missing_guard = std::vector<std::byte>(
			studio_0732_datamodel_constructor.begin(),
			studio_0732_datamodel_constructor.end());
		missing_guard[studio_0732_type_guard_sub_immediate_offset] = std::byte{0x03};
		const auto layout = resolve_captured_layout(
			missing_guard,
			studio_0732_code_address,
			studio_0732_image_base,
			studio_0732_function_begin_rva,
			studio_0732_function_end_rva,
			studio_0732_vft_address);
		if (!failed_with(layout, CompatibilityFailure::missing_signature))
		{
			std::cerr << "Captured constructor without semantic guard did not fail closed\n";
			return 16;
		}
	}

	// Malformed and overlapping runtime-function ranges fail closed.
	{
		using namespace rml::tests::fixtures;
		const std::array malformed_runtime_functions{
			RuntimeFunctionEntryRaw{
				.begin_address = studio_0732_function_end_rva,
				.end_address = studio_0732_function_begin_rva,
				.unwind_info_address = 1,
			},
		};
		const auto malformed = resolve_with_runtime_functions(
			studio_0732_datamodel_constructor,
			studio_0732_code_address,
			malformed_runtime_functions,
			studio_0732_image_base,
			studio_0732_vft_address);

		const std::array overlapping_runtime_functions{
			RuntimeFunctionEntryRaw{
				.begin_address = studio_0732_function_begin_rva,
				.end_address = studio_0732_function_end_rva,
				.unwind_info_address = 1,
			},
			RuntimeFunctionEntryRaw{
				.begin_address = studio_0732_function_begin_rva + 0x10,
				.end_address = studio_0732_function_end_rva,
				.unwind_info_address = 2,
			},
		};
		const auto overlapping = resolve_with_runtime_functions(
			studio_0732_datamodel_constructor,
			studio_0732_code_address,
			overlapping_runtime_functions,
			studio_0732_image_base,
			studio_0732_vft_address);
		if (!failed_with(malformed, CompatibilityFailure::invalid_address_range) ||
			!failed_with(overlapping, CompatibilityFailure::ambiguous_evidence))
		{
			std::cerr << "Runtime-function evidence did not fail closed\n";
			return 17;
		}
	}


	// 1. 0.731 form: copied owner/type registers, interior NOP, Instance subobject VFT, and distant read.
	{
		SyntheticModule module;
		const auto vft = module.base + 0x15000;
		const auto code_address = module.base + 0x1000;
		std::vector<std::uint8_t> code{0xCC, 0xCC, 0x45, 0x89, 0xCD, 0x49, 0x89, 0xCE, 0x90,
			0x49, 0x8D, 0x9E, 0xC8, 0x01, 0x00, 0x00};
		emit_lea_rip(code, false, code_address + code.size(), vft);
		code.insert(code.end(), {0x48, 0x89, 0x03, 0x45, 0x89, 0xAE, 0x08, 0x09, 0x00, 0x00});
		code.insert(code.end(), 0xB00, 0x90);
		code.insert(code.end(), {0x41, 0x8B, 0x86, 0x08, 0x09, 0x00, 0x00,
			0x83, 0xE8, 0x02, 0x83, 0xF8, 0x01, 0x77, 0x00, 0xC3});
		const auto layout = resolve_layout(module, code, vft);
		if (!layout || layout->type_offset != 0x908)
		{
			std::cerr << "Test 1 failed: 0.731 type layout\n";
			return 1;
		}

		RttiFixture fixture;
		*reinterpret_cast<std::int32_t*>(fixture.complete_object + 0x908) = 1;
		*reinterpret_cast<std::int32_t*>(reinterpret_cast<std::uint8_t*>(fixture.instance) + 0x908) = 99;
		const auto data_model = fixture.capabilities.job_subobject_to_data_model(fixture.job_subobject);
		const auto context = fixture.capabilities.data_model_to_task_context(fixture.instance);
		const auto type = fixture.capabilities.resolve_type(fixture.instance);
		if (!data_model || *data_model != fixture.instance || !context || *context != fixture.complete_object ||
			!type || *type != RBX::DataModelType::Client)
		{
			std::cerr << "Test 1 failed: RTTI owner/Instance/type conversion\n";
			return 1;
		}
	}

	// 2. 0.732 form: direct owner VFT store and copied type register.
	{
		SyntheticModule module;
		const auto vft = module.base + 0x15000;
		const auto code_address = module.base + 0x1000;
		std::vector<std::uint8_t> code{0xCC, 0xCC, 0x48, 0x89, 0xCF, 0x44, 0x89, 0xCE};
		emit_lea_rip(code, true, code_address + code.size(), vft);
		code.insert(code.end(), {0x4C, 0x89, 0x07, 0x89, 0xB7, 0x08, 0x09, 0x00, 0x00,
			0x8B, 0x87, 0x08, 0x09, 0x00, 0x00,
			0x83, 0xE8, 0x02, 0x83, 0xF8, 0x01, 0x77, 0x00, 0xC3});
		const auto layout = resolve_layout(module, code, vft);
		if (!layout || layout->type_offset != 0x908)
		{
			std::cerr << "Test 2 failed: 0.732 type layout\n";
			return 2;
		}
		RttiFixture fixture;
		*reinterpret_cast<std::int32_t*>(fixture.complete_object + 0x908) = 0;
		const auto type = fixture.capabilities.resolve_type(fixture.instance);
		if (!type || *type != RBX::DataModelType::Edit)
		{
			std::cerr << "Test 2 failed: type resolution\n";
			return 2;
		}
	}

	// 3. A structurally compatible moved type field remains derivable.
	{
		SyntheticModule module;
		const auto vft = module.base + 0x15000;
		const auto code_address = module.base + 0x1000;
		std::vector<std::uint8_t> code{0xCC};
		emit_lea_rip(code, false, code_address + code.size(), vft);
		code.insert(code.end(), {0x48, 0x89, 0x01, 0x44, 0x89, 0x89, 0x80, 0x09, 0x00, 0x00,
			0x8B, 0x81, 0x80, 0x09, 0x00, 0x00,
			0x83, 0xE8, 0x02, 0x83, 0xF8, 0x01, 0x77, 0x00, 0xC3});
		const auto layout = resolve_layout(module, code, vft);
		if (!layout || layout->type_offset != 0x980)
		{
			std::cerr << "Test 3 failed: moved type layout\n";
			return 3;
		}
	}

	// 4. Missing VFT evidence fails closed.
	{
		SyntheticModule module{0x10000};
		const auto code = std::span{reinterpret_cast<const std::byte*>(module.buffer.data() + 0x1000), 0x100};
		const auto layout = resolve_captured_layout(
			code,
			module.base + 0x1000,
			module.base,
			0x1000,
			0x1100,
			module.base + 0x5000);
		if (!failed_with(layout, CompatibilityFailure::missing_signature))
		{
			std::cerr << "Test 4 failed: missing VFT evidence\n";
			return 4;
		}
	}

	// 5. A VFT address load without a store to the owner/subobject is insufficient.
	{
		SyntheticModule module{0x10000};
		const auto vft = module.base + 0x5000;
		const auto code_address = module.base + 0x1000;
		std::vector<std::uint8_t> code{0xCC};
		emit_lea_rip(code, false, code_address + code.size(), vft);
		code.insert(code.end(), {0x44, 0x89, 0x89, 0x08, 0x09, 0x00, 0x00,
			0x8B, 0x81, 0x08, 0x09, 0x00, 0x00,
			0x83, 0xE8, 0x02, 0x83, 0xF8, 0x01, 0x77, 0x00, 0xC3});
		const auto layout = resolve_layout(module, code, vft);
		if (!failed_with(layout, CompatibilityFailure::missing_signature))
		{
			std::cerr << "Test 5 failed: unverified VFT store\n";
			return 5;
		}
	}

	// 6. Runtime enum reads reject values outside DataModelType.
	{
		RttiFixture fixture;
		*reinterpret_cast<std::int32_t*>(fixture.complete_object + 0x908) = 99;
		const auto type = fixture.capabilities.resolve_type(fixture.instance);
		if (!failed_with(type, CompatibilityFailure::insufficient_evidence))
		{
			std::cerr << "Test 6 failed: invalid DataModelType\n";
			return 6;
		}
	}

	// 7. Conflicting constructor evidence is ambiguous.
	{
		SyntheticModule module;
		const auto vft = module.base + 0x15000;
		const auto code_address = module.base + 0x1000;
		std::vector<std::uint8_t> code{0xCC};
		emit_lea_rip(code, false, code_address + code.size(), vft);
		code.insert(code.end(), {0x48, 0x89, 0x01, 0x44, 0x89, 0x89, 0x08, 0x09, 0x00, 0x00,
			0x8B, 0x81, 0x08, 0x09, 0x00, 0x00,
			0x83, 0xE8, 0x02, 0x83, 0xF8, 0x01, 0x77, 0x00, 0xC3, 0xCC});
		emit_lea_rip(code, false, code_address + code.size(), vft);
		code.insert(code.end(), {0x48, 0x89, 0x01, 0x44, 0x89, 0x89, 0x80, 0x09, 0x00, 0x00,
			0x8B, 0x81, 0x80, 0x09, 0x00, 0x00,
			0x83, 0xE8, 0x02, 0x83, 0xF8, 0x01, 0x77, 0x00, 0xC3});
		const auto layout = resolve_layout(module, code, vft);
		if (!failed_with(layout, CompatibilityFailure::ambiguous_evidence))
		{
			std::cerr << "Test 7 failed: ambiguous layout\n";
			return 7;
		}
	}

	// 8. A copied type argument loses provenance when overwritten.
	{
		SyntheticModule module{0x10000};
		const auto vft = module.base + 0x5000;
		const auto code_address = module.base + 0x1000;
		std::vector<std::uint8_t> code{0xCC};
		emit_lea_rip(code, false, code_address + code.size(), vft);
		code.insert(code.end(), {0x48, 0x89, 0x01, 0x45, 0x89, 0xCA, 0x44, 0x8B, 0xD0,
			0x44, 0x89, 0x91, 0x08, 0x09, 0x00, 0x00,
			0x8B, 0x81, 0x08, 0x09, 0x00, 0x00,
			0x83, 0xE8, 0x02, 0x83, 0xF8, 0x01, 0x77, 0x00, 0xC3});
		const auto layout = resolve_layout(module, code, vft);
		if (!failed_with(layout, CompatibilityFailure::missing_signature))
		{
			std::cerr << "Test 8 failed: overwritten type provenance\n";
			return 8;
		}
	}

	// 9. Every nested module-relative RTTI record is range checked before use.
	{
		auto rejects_invalid_range = [](RttiFixture& fixture) {
			const auto result = fixture.capabilities.job_subobject_to_data_model(fixture.job_subobject);
			return failed_with(result, CompatibilityFailure::invalid_address_range);
		};
		RttiFixture bad_hierarchy;
		bad_hierarchy.job_col->class_hierarchy_offset = static_cast<std::int32_t>(bad_hierarchy.module.size + 0x100);
		RttiFixture bad_array;
		bad_array.hierarchy->base_class_array_offset = static_cast<std::int32_t>(bad_array.module.size + 0x100);
		RttiFixture bad_bcd;
		bad_bcd.base_array[0] = static_cast<std::int32_t>(bad_bcd.module.size + 0x100);
		RttiFixture bad_type;
		bad_type.instance_base->type_descriptor_offset = static_cast<std::int32_t>(bad_type.module.size + 0x100);
		if (!rejects_invalid_range(bad_hierarchy) || !rejects_invalid_range(bad_array) ||
			!rejects_invalid_range(bad_bcd) || !rejects_invalid_range(bad_type))
		{
			std::cerr << "Test 9 failed: nested RTTI module bounds\n";
			return 9;
		}
	}

	// 10. COL self-RVA must recover the configured module base exactly.
	{
		RttiFixture fixture;
		fixture.job_col->self_offset += 8;
		const auto result = fixture.capabilities.job_subobject_to_data_model(fixture.job_subobject);
		if (!failed_with(result, CompatibilityFailure::insufficient_evidence))
		{
			std::cerr << "Test 10 failed: COL owner mismatch\n";
			return 10;
		}
	}

	// 11. Instance must appear exactly once in the hierarchy.
	{
		RttiFixture missing;
		std::strcpy(missing.instance_type->name, ".?AVOther@RBX@@");
		const auto missing_result = missing.capabilities.job_subobject_to_data_model(missing.job_subobject);

		RttiFixture ambiguous;
		auto* second_base = reinterpret_cast<BaseClassDescriptorRaw*>(ambiguous.module.buffer.data() + 0x3440);
		*second_base = *ambiguous.instance_base;
		ambiguous.base_array[1] = ambiguous.module.rva(second_base);
		ambiguous.hierarchy->num_base_classes = 2;
		const auto ambiguous_result = ambiguous.capabilities.job_subobject_to_data_model(ambiguous.job_subobject);
		if (!failed_with(missing_result, CompatibilityFailure::missing_signature) ||
			!failed_with(ambiguous_result, CompatibilityFailure::ambiguous_evidence))
		{
			std::cerr << "Test 11 failed: missing/ambiguous Instance base\n";
			return 11;
		}
	}

	// 12. Virtual-base PMDs require vbtable evaluation and are unsupported.
	{
		RttiFixture fixture;
		fixture.instance_base->pdisp = 0;
		const auto result = fixture.capabilities.job_subobject_to_data_model(fixture.job_subobject);
		if (!failed_with(result, CompatibilityFailure::unsupported_instruction_form))
		{
			std::cerr << "Test 12 failed: virtual Instance PMD\n";
			return 12;
		}
	}

	// 13. Hierarchy cardinality is bounded before computing/reading its RVA array.
	{
		RttiFixture fixture;
		fixture.hierarchy->num_base_classes = 101;
		const auto result = fixture.capabilities.job_subobject_to_data_model(fixture.job_subobject);
		if (!failed_with(result, CompatibilityFailure::insufficient_evidence))
		{
			std::cerr << "Test 13 failed: hierarchy bound\n";
			return 13;
		}
	}


	// 14. Missing profile returns a typed error instead of throwing through noexcept.
	{
		struct DummyDataModel final : RBX::DataModel
		{
			const RBX::Name& get_class_name() const override
			{
				std::abort();
			}
		};

		DummyDataModel dummy_instance;
		const auto context = dummy_instance.get_task_context();
		if (!failed_with(context, CompatibilityFailure::missing_signature) || context.error().capability != "DataModel.RTTI")
		{
			std::cerr << "Test 14 failed: uninitialized profile did not fail closed\n";
			return 14;
		}
	}
	// 15. JobCapabilities only owns WaitingHybridScriptsJob context access.
	{
		rml::roblox::internals::JobCapabilities job_caps(0x1F8);

		alignas(16) std::uint8_t dummy_job_buffer[0x200]{};
		*reinterpret_cast<void**>(dummy_job_buffer + 0x1F8) = dummy_job_buffer + 0x20;

		const auto* waiting_job = reinterpret_cast<const RBX::ScriptContextFacets::WaitingHybridScriptsJob*>(dummy_job_buffer);
		const auto derived_ctx = job_caps.get_script_context(waiting_job);
		if (derived_ctx != reinterpret_cast<RBX::ScriptContext*>(dummy_job_buffer + 0x20))
		{
			std::cerr << "Test 15 failed: WaitingHybridScriptsJob context access\n";
			return 18;
		}

		if (job_caps.get_script_context(nullptr) != nullptr)
		{
			std::cerr << "Test 15 failed: null WaitingHybridScriptsJob did not return nullptr\n";
			return 18;
		}
	}

	// 16. InstanceCapabilities: typed parent, children, and name accessors.
	{
		rml::roblox::internals::InstanceCapabilities instance_caps(0x60, 0x68, 0x98);
		if (instance_caps.parent(nullptr) != nullptr || instance_caps.children(nullptr) != nullptr || !instance_caps.name(nullptr).empty())
		{
			std::cerr << "Test 16 failed: null instance parameters did not return safe defaults\n";
			return 19;
		}
	}

	std::cout << "All DataModel capability synthetic tests passed successfully!\n";
	return 0;
}
