#pragma once

#include "RobloxModLoader/roblox/reflection/runtime_layout_resolver.hpp"

#include <cstddef>
#include <cstdint>
#include <expected>
#include <span>

namespace rml::roblox::internals
{
	struct DataModelLayoutEvidence
	{
		std::ptrdiff_t type_offset{-1};
		std::size_t supporting_calls{};
		std::size_t matched_calls{};
	};

	[[nodiscard]] std::expected<DataModelLayoutEvidence, CompatibilityError> resolve_datamodel_layout(
		std::span<const std::byte> executable_code,
		std::uintptr_t code_address,
		std::span<const std::byte> runtime_function_table,
		std::uintptr_t module_address,
		std::span<const std::uintptr_t> datamodel_vft_addresses,
		std::vector<CompatibilityError>* diagnostics = nullptr) noexcept;
}
