#pragma once

#include "RobloxModLoader/roblox/security/script_permissions.hpp"
#include "RobloxModLoader/util/layout_assert.hpp"
#include "descriptor.hpp"

namespace RBX::Reflection
{
	class ClassDescriptor;

	class MemberDescriptor : public Descriptor
	{
	public:
		static void (*member_hiding_hook)(MemberDescriptor*, MemberDescriptor*);

		[[nodiscard]] const ClassDescriptor* owner() const noexcept;
		[[nodiscard]] Security::Permissions security() const noexcept;

	protected:
		virtual ~MemberDescriptor() = default;
	};
}