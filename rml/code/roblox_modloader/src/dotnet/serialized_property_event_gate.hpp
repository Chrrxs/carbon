#pragma once

#include "RobloxModLoader/roblox/reflection/type.hpp"

#include <utility>

namespace RBX::Reflection
{
	class PropertyDescriptor;
}

namespace rml::dotnet::detail
{
	template<typename Arguments, typename Visitor>
	bool visit_serialized_property_descriptor_argument(
	    const Arguments& args,
	    const RBX::Reflection::Type* expected_type,
	    Visitor&& visitor)
	{
		if (!expected_type || args.size() < 2 || args[1].type_ptr() != expected_type)
			return false;

		const auto* descriptor_slot = args[1].try_cast<const RBX::Reflection::PropertyDescriptor*>();
		if (!descriptor_slot)
			return false;

		std::forward<Visitor>(visitor)(*descriptor_slot);
		return true;
	}
}
