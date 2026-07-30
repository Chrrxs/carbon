#include "RobloxModLoader/roblox/reflection/event_descriptor.hpp"

#include "RobloxModLoader/internal/common.hpp"
#include "RobloxModLoader/roblox/internals_profile.hpp"
#include "RobloxModLoader/roblox/reflection/event.hpp"
#include "pointers.hpp"

namespace RBX::Reflection
{
	Signals::Signal* EventDescriptor::get_signal(EventSource* source) const
	{
		if (!source)
			return nullptr;

		return ::get_roblox_internals_profile().signal().get_signal(this, source);
	}

	std::vector<Signals::Connection> EventDescriptor::snapshot_connections(EventSource* source) const
	{
		if (!source)
			return {};

		return ::get_roblox_internals_profile().signal().snapshot_connections(this, source);
	}
}
