#pragma once

#include "RobloxModLoader/roblox/internals_profile.hpp"

#include <expected>
#include <string>

namespace rml::roblox::internals
{
	[[nodiscard]] std::expected<RobloxInternalsProfile, std::string> load_pinned_internals_profile(
		const memory::module& studio_module,
		functions::get_string_atom get_string_atom) noexcept;
}
