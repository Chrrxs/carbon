#pragma once

#include "RobloxModLoader/util/layout_assert.hpp"
#include "member.hpp"
#include "type.hpp"

namespace RBX::Reflection
{
	class Callback;

	class CallbackDescriptor : public MemberDescriptor
	{
	public:
		typedef Callback ConstMember;
		typedef Callback Member;

	public:
		[[nodiscard]] const SignatureDescriptor* get_signature() const noexcept;
		[[nodiscard]] bool is_async() const noexcept;
	};
}