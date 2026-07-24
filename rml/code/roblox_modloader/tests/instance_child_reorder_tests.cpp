#include "dotnet/instance_child_reorder.hpp"

#include <array>
#include <cstdint>
#include <initializer_list>

namespace
{
	using rml::dotnet::detail::ChildReorderSwap;
	using rml::dotnet::detail::plan_exact_child_reorder;

	template<size_t Size>
	bool swaps_to(
	    std::array<uintptr_t, Size> current,
	    const std::array<uintptr_t, Size>& desired,
	    const std::initializer_list<ChildReorderSwap> expected_swaps)
	{
		const auto plan = plan_exact_child_reorder(current, desired);
		if (!plan || *plan != std::vector<ChildReorderSwap>(expected_swaps))
			return false;

		for (const auto [left, right] : *plan)
			std::swap(current[left], current[right]);
		return current == desired;
	}
}

int main()
{
	if (!swaps_to<4>({1, 2, 3, 4}, {3, 1, 4, 2}, {{0, 2}, {1, 2}, {2, 3}}))
		return 1;
	if (!swaps_to<3>({1, 2, 3}, {1, 2, 3}, {}))
		return 2;

	if (plan_exact_child_reorder(
	        std::array<uintptr_t, 2>{1, 2},
	        std::array<uintptr_t, 1>{1}))
	{
		return 3;
	}
	if (plan_exact_child_reorder(
	        std::array<uintptr_t, 3>{1, 2, 3},
	        std::array<uintptr_t, 3>{1, 2, 2}))
	{
		return 4;
	}
	if (plan_exact_child_reorder(
	        std::array<uintptr_t, 3>{1, 1, 3},
	        std::array<uintptr_t, 3>{1, 2, 3}))
	{
		return 5;
	}
	if (plan_exact_child_reorder(
	        std::array<uintptr_t, 3>{1, 2, 3},
	        std::array<uintptr_t, 3>{1, 2, 4}))
	{
		return 6;
	}
	if (plan_exact_child_reorder(
	        std::array<uintptr_t, 3>{1, 2, 3},
	        std::array<uintptr_t, 3>{1, 0, 3}))
	{
		return 7;
	}

	return 0;
}
