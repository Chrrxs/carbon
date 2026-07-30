#pragma once

#include "../util/name.hpp"
#include "RobloxModLoader/roblox/memory/noncopyable.hpp"

#include <memory>

namespace RBX::Reflection
{
	class Descriptor : public rml::memory::Noncopyable
	{
	public:
		struct Attributes
		{
			bool is_deprecated;
			Attributes() :
			    is_deprecated(false)
			{
			}

			static Attributes deprecated()
			{
				Attributes result;
				result.is_deprecated = true;
				return result;
			}
		};

		static bool locked_down;

		virtual ~Descriptor() = default;

		[[nodiscard]] const Name* name() const noexcept;
	};
}