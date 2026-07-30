#include "RobloxModLoader/roblox/instance.hpp"

#include "RobloxModLoader/internal/common.hpp"
#include "RobloxModLoader/roblox/internals_profile.hpp"

namespace RBX
{
	Instance* Instance::get_parent() const
	{
		return ::get_roblox_internals_profile().instance().parent(this);
	}

	Instances* Instance::get_children() const
	{
		return ::get_roblox_internals_profile().instance().children(this);
	}

	std::string_view Instance::get_name() const
	{
		return ::get_roblox_internals_profile().instance().name(this);
	}

	std::string Instance::get_full_name()
	{
		std::string full_name{get_name()};
		for (Instance* ancestor = get_parent(); ancestor != nullptr; ancestor = ancestor->get_parent())
			full_name = std::string{ancestor->get_name()} + "." + full_name;
		return full_name;
	}
}
