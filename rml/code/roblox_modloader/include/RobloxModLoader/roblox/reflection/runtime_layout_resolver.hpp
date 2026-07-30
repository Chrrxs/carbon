#pragma once

#include <array>
#include <cstddef>
#include <cstdint>
#include <expected>
#include <span>
#include <string_view>
#include <vector>

namespace rml::roblox::internals
{
	enum class CompatibilityFailure
	{
		missing_signature,
		insufficient_evidence,
		ambiguous_evidence,
		unsupported_instruction_form,
		invalid_address_range,
	};

	struct CompatibilityError
	{
		std::string_view capability;
		CompatibilityFailure failure;
		std::size_t matched_calls{};
		std::size_t decoded_candidates{};
	};

	struct ReflectionLayoutEvidence
	{
		std::ptrdiff_t name_offset{-1};
		std::array<std::ptrdiff_t, 5> descriptor_container_offsets{-1, -1, -1, -1, -1};
		std::ptrdiff_t base_class_offset{-1};
		std::ptrdiff_t functionality_offset{-1};
		std::ptrdiff_t owner_offset{-1};
		std::ptrdiff_t security_offset{-1};
		std::ptrdiff_t property_type_offset{-1};
		std::ptrdiff_t property_functionality_offset{-1};
		std::ptrdiff_t signature_offset{-1};
		std::ptrdiff_t function_kind_offset{-1};
		std::ptrdiff_t function_invoke_func_ptr_offset{-1};
		std::ptrdiff_t function_bound_this_delta_offset{-1};
		std::ptrdiff_t callback_signature_offset{-1};
		std::ptrdiff_t callback_async_flag_offset{-1};
		std::ptrdiff_t event_signal_offset{-1};
		std::size_t supporting_calls{0};
		std::size_t matched_calls{0};
	};

	struct ReflectionVftSets
	{
		std::span<const std::uintptr_t> descriptor_vfts;
		std::span<const std::uintptr_t> member_vfts;
		std::span<const std::uintptr_t> property_vfts;
		std::span<const std::uintptr_t> function_vfts;
		std::span<const std::uintptr_t> yield_function_vfts;
		std::span<const std::uintptr_t> event_vfts;
		std::span<const std::uintptr_t> callback_vfts;
		std::span<const std::uintptr_t> class_descriptor_vfts;
	};

	[[nodiscard]] std::expected<ReflectionLayoutEvidence, CompatibilityError> resolve_reflection_layout(
		std::span<const std::byte> executable_code,
		std::uintptr_t code_address,
		std::uintptr_t get_string_atom_address,
		std::span<const std::byte> runtime_function_table,
		std::uintptr_t module_address,
		const ReflectionVftSets& vft_sets,
		std::vector<CompatibilityError>* diagnostics = nullptr) noexcept;
	struct InstanceLayoutEvidence
	{
		std::ptrdiff_t parent_offset{-1};
		std::ptrdiff_t children_offset{-1};
		std::ptrdiff_t name_offset{-1};
		std::size_t supporting_calls{};
		std::size_t matched_calls{};
	};

	[[nodiscard]] std::expected<InstanceLayoutEvidence, CompatibilityError> resolve_instance_layout(
		std::span<const std::byte> executable_code,
		std::uintptr_t code_address,
		std::span<const std::byte> runtime_function_table,
		std::uintptr_t module_address,
		std::span<const std::uintptr_t> instance_vft_addresses,
		std::span<const std::uintptr_t> instance_vft_entry_addresses = {},
		std::vector<CompatibilityError>* diagnostics = nullptr) noexcept;

	struct SignalLayoutEvidence
	{
		std::ptrdiff_t signal_head_offset{-1};
		std::ptrdiff_t slot_strong_offset{-1};
		std::ptrdiff_t slot_weak_offset{-1};
		std::ptrdiff_t slot_next_offset{-1};
		std::ptrdiff_t slot_source_offset{-1};
		std::ptrdiff_t slot_wrapper_ptr_offset{-1};
		std::size_t supporting_calls{};
		std::size_t matched_calls{};
	};

	[[nodiscard]] std::expected<SignalLayoutEvidence, CompatibilityError> resolve_signal_layout(
		std::span<const std::byte> executable_code,
		std::uintptr_t code_address,
		std::span<const std::byte> runtime_function_table,
		std::uintptr_t module_address,
		std::uintptr_t signal_disconnect_address = 0,
		std::uintptr_t signal_slot_free_address = 0,
		std::vector<CompatibilityError>* diagnostics = nullptr) noexcept;

	struct JobLayoutEvidence
	{
		std::ptrdiff_t waiting_scripts_job_script_context_offset{-1};
		std::size_t supporting_calls{};
		std::size_t matched_calls{};
	};

	[[nodiscard]] std::expected<JobLayoutEvidence, CompatibilityError> resolve_job_layout(
		std::span<const std::byte> executable_code,
		std::uintptr_t code_address,
		std::span<const std::byte> runtime_function_table,
		std::uintptr_t module_address,
		std::span<const std::uintptr_t> waiting_scripts_job_vft_addresses,
		std::vector<CompatibilityError>* diagnostics = nullptr) noexcept;
}
