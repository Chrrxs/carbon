#include "dotnet/serialized_property_event_gate.hpp"
#include "RobloxModLoader/roblox/reflection/event_descriptor.hpp"

#include <cstdint>
#include <cstring>

namespace
{
	class SlotProbe final : public RBX::Reflection::GenericSlotWrapper
	{
	public:
		void deliver_owned(const RBX::Reflection::EventArguments& args) override
		{
			++owned_calls;
			owned_size = args.size();
		}

		void deliver_view(const RBX::Reflection::EventArgumentsView& args) override
		{
			++view_calls;
			view_size = args.size();
		}

		int owned_calls{};
		int view_calls{};
		std::size_t owned_size{};
		std::size_t view_size{};
	};
}

int main()
{
	using namespace RBX::Reflection;
	using rml::dotnet::detail::visit_serialized_property_descriptor_argument;

	const auto* expected_type = reinterpret_cast<const Type*>(std::uintptr_t{0x1000});
	const auto* wrong_type = reinterpret_cast<const Type*>(std::uintptr_t{0x2000});
	const auto* fake_descriptor = reinterpret_cast<const PropertyDescriptor*>(std::uintptr_t{0x12345678});

	EventArguments args(2);
	args[1].set_type_and_ops(wrong_type, nullptr);
	std::memcpy(args[1].storage(), &fake_descriptor, sizeof(fake_descriptor));

	int callback_count = 0;
	const PropertyDescriptor* observed = nullptr;
	auto visit = [&](const PropertyDescriptor* descriptor) {
		++callback_count;
		observed = descriptor;
	};

	if (visit_serialized_property_descriptor_argument(args, expected_type, visit) || callback_count != 0 || observed)
		return 1;

	args[1].set_type_and_ops(expected_type, nullptr);
	if (!visit_serialized_property_descriptor_argument(args, expected_type, visit) || callback_count != 1 ||
	    observed != fake_descriptor)
	{
		return 2;
	}

	EventArgumentsView args_view(args.data(), args.size());
	if (!visit_serialized_property_descriptor_argument(args_view, expected_type, visit) || callback_count != 2 ||
	    observed != fake_descriptor)
	{
		return 3;
	}

	SlotProbe slot;
	GenericSlotWrapper& wrapper = slot;
	wrapper.deliver_owned(args);
	wrapper.deliver_view(args_view);
	if (slot.owned_calls != 1 || slot.view_calls != 1 || slot.owned_size != 2 || slot.view_size != 2)
		return 4;

#ifdef _MSC_VER
	const auto vtable = *reinterpret_cast<void***>(&slot);
	using OwnedDeliver = void(__fastcall*)(GenericSlotWrapper*, const EventArguments&);
	using ViewDeliver = void(__fastcall*)(GenericSlotWrapper*, const EventArgumentsView&);
	reinterpret_cast<OwnedDeliver>(vtable[2])(&slot, args);
	reinterpret_cast<ViewDeliver>(vtable[3])(&slot, args_view);
	if (slot.owned_calls != 2 || slot.view_calls != 2 || slot.owned_size != 2 || slot.view_size != 2)
		return 5;
#endif

	return 0;
}
