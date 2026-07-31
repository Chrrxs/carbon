#include "RobloxModLoader/roblox/internals_profile.hpp"

#include "RobloxModLoader/roblox/reflection/callback_descriptor.hpp"
#include "RobloxModLoader/roblox/reflection/event_descriptor.hpp"
#include "RobloxModLoader/roblox/reflection/function_descriptor.hpp"
#include "RobloxModLoader/roblox/reflection/property_descriptor.hpp"
#include "RobloxModLoader/roblox/reflection/type.hpp"
#include "RobloxModLoader/roblox/reflection/yield_function_descriptor.hpp"

namespace RBX::Reflection
{
	const Name* Descriptor::name() const noexcept
	{
		const auto* profile = ::try_get_roblox_internals_profile();
		return profile ? profile->reflection().descriptor_name(this) : nullptr;
	}
	const ClassDescriptor* MemberDescriptor::owner() const noexcept
	{
		const auto* profile = ::try_get_roblox_internals_profile();
		return profile ? profile->reflection().member_owner(this) : nullptr;
	}
	Security::Permissions MemberDescriptor::security() const noexcept
	{
		const auto* profile = ::try_get_roblox_internals_profile();
		return profile ? profile->reflection().member_security(this) : Security::Permissions::None;
	}
	const Type* PropertyDescriptor::type() const noexcept
	{
		const auto* profile = ::try_get_roblox_internals_profile();
		return profile ? profile->reflection().property_type(this) : nullptr;
	}
	const Name* Type::tag() const noexcept
	{
		const auto* profile = ::try_get_roblox_internals_profile();
		return profile ? profile->reflection().type_tag(this) : nullptr;
	}
	int Type::type_id() const noexcept
	{
		const auto* profile = ::try_get_roblox_internals_profile();
		return profile ? profile->reflection().type_id(this) : 0;
	}
	bool Type::is_float() const noexcept
	{
		const auto* profile = ::try_get_roblox_internals_profile();
		return profile && profile->reflection().type_is_float(this);
	}
	bool Type::is_number() const noexcept
	{
		const auto* profile = ::try_get_roblox_internals_profile();
		return profile && profile->reflection().type_is_number(this);
	}
	bool Type::is_enum() const noexcept
	{
		const auto* profile = ::try_get_roblox_internals_profile();
		return profile && profile->reflection().type_is_enum(this);
	}
	bool PropertyDescriptor::is_public() const noexcept
	{
		const auto* profile = ::try_get_roblox_internals_profile();
		return profile && profile->reflection().property_is_public(this);
	}
	bool PropertyDescriptor::is_editable() const noexcept
	{
		const auto* profile = ::try_get_roblox_internals_profile();
		return profile && profile->reflection().property_is_editable(this);
	}
	bool PropertyDescriptor::can_replicate() const noexcept
	{
		const auto* profile = ::try_get_roblox_internals_profile();
		return profile && profile->reflection().property_can_replicate(this);
	}
	bool PropertyDescriptor::can_xml_read() const noexcept
	{
		const auto* profile = ::try_get_roblox_internals_profile();
		return profile && profile->reflection().property_can_xml_read(this);
	}
	bool PropertyDescriptor::can_xml_write() const noexcept
	{
		const auto* profile = ::try_get_roblox_internals_profile();
		return profile && profile->reflection().property_can_xml_write(this);
	}
	bool PropertyDescriptor::is_scriptable() const noexcept
	{
		const auto* profile = ::try_get_roblox_internals_profile();
		return profile && profile->reflection().property_is_scriptable(this);
	}
	bool PropertyDescriptor::always_clone() const noexcept
	{
		const auto* profile = ::try_get_roblox_internals_profile();
		return profile && profile->reflection().property_always_clone(this);
	}
	const SignatureDescriptor* FunctionDescriptor::get_signature() const noexcept
	{
		const auto* profile = ::try_get_roblox_internals_profile();
		return profile ? profile->reflection().function_signature(this) : nullptr;
	}
	FunctionDescriptor::Kind FunctionDescriptor::get_kind() const noexcept
	{
		const auto* profile = ::try_get_roblox_internals_profile();
		return profile ? static_cast<Kind>(profile->reflection().function_kind(this)) : Kind::Default;
	}
	void* FunctionDescriptor::invoke_func_ptr_raw() const noexcept
	{
		const auto* profile = ::try_get_roblox_internals_profile();
		return profile ? profile->reflection().function_invoke_func_ptr(this) : nullptr;
	}
	const SignatureDescriptor* YieldFunctionDescriptor::get_signature() const noexcept
	{
		const auto* profile = ::try_get_roblox_internals_profile();
		return profile ? profile->reflection().yield_signature(this) : nullptr;
	}
	const SignatureDescriptor* CallbackDescriptor::get_signature() const noexcept
	{
		const auto* profile = ::try_get_roblox_internals_profile();
		return profile ? profile->reflection().callback_signature(this) : nullptr;
	}
	bool CallbackDescriptor::is_async() const noexcept
	{
		const auto* profile = ::try_get_roblox_internals_profile();
		return profile && profile->reflection().callback_is_async(this);
	}
	const SignatureDescriptor* EventDescriptor::get_signature() const noexcept
	{
		const auto* profile = ::try_get_roblox_internals_profile();
		return profile ? profile->reflection().event_signature(this) : nullptr;
	}
}
