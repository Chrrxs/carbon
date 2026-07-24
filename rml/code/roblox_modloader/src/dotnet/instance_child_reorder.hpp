#pragma once

#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <optional>
#include <span>
#include <utility>
#include <vector>

namespace rml::dotnet::detail
{
	struct ChildReorderSwap
	{
		size_t left;
		size_t right;

		constexpr bool operator==(const ChildReorderSwap&) const = default;
	};

	// Produces the swaps needed to turn one complete child ordering into another.
	// Validation is deliberately performed against raw identities before any
	// engine-owned shared_ptr is touched by the interop implementation.
	[[nodiscard]] inline std::optional<std::vector<ChildReorderSwap>> plan_exact_child_reorder(
	    const std::span<const uintptr_t> current,
	    const std::span<const uintptr_t> desired)
	{
		if (current.size() != desired.size())
			return std::nullopt;

		for (size_t index = 0; index < current.size(); ++index)
		{
			if (current[index] == 0 ||
			    std::find(current.begin(), current.begin() + index, current[index]) != current.begin() + index)
			{
				return std::nullopt;
			}

			if (desired[index] == 0 ||
			    std::find(desired.begin(), desired.begin() + index, desired[index]) != desired.begin() + index)
			{
				return std::nullopt;
			}
		}

		std::vector<uintptr_t> working(current.begin(), current.end());
		std::vector<ChildReorderSwap> swaps;
		swaps.reserve(current.size());

		for (size_t index = 0; index < desired.size(); ++index)
		{
			if (working[index] == desired[index])
				continue;

			const auto match = std::find(working.begin() + index + 1, working.end(), desired[index]);
			if (match == working.end())
				return std::nullopt;

			const auto match_index = static_cast<size_t>(match - working.begin());
			swaps.push_back({index, match_index});
			std::swap(working[index], working[match_index]);
		}

		return swaps;
	}
} // namespace rml::dotnet::detail
