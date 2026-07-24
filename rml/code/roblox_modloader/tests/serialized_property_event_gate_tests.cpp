#include "dotnet/serialized_property_event_gate.hpp"

#include <cstdint>
#include <cstring>

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

	return 0;
}
