#pragma once

#include <chrono>
#include <cstddef>
#include <cstdint>
#include <unordered_map>
#include <utility>

namespace rml::jobs::detail
{
	[[nodiscard]] constexpr std::uint8_t studio_marker_priority(const std::size_t route_markers, const bool baseline_ready) noexcept
	{
		if (route_markers != 1)
			return 0;
		return baseline_ready ? 2 : 1;
	}

	[[nodiscard]] constexpr bool should_cleanup_stale_data_model(const void* const current_data_model, const void* const tracked_data_model) noexcept
	{
		return tracked_data_model && (!current_data_model || current_data_model == tracked_data_model);
	}

	[[nodiscard]] constexpr bool should_prefer_data_model_candidate(const bool current_is_stale, const std::uint8_t candidate_priority, const std::uint8_t current_priority) noexcept
	{
		return current_is_stale || candidate_priority > current_priority;
	}

	class PerJobCadence final
	{
	public:
		using Key = const void*;
		using TimePoint = std::chrono::steady_clock::time_point;
		using Duration = std::chrono::steady_clock::duration;

		[[nodiscard]] bool should_check(const Key key, const TimePoint now, const Duration interval)
		{
			auto [it, inserted] = m_entries.try_emplace(key, Entry{.next_check = now + interval, .last_seen = now});
			if (inserted)
				return true;

			auto& entry = it->second;
			entry.last_seen = now;
			if (now < entry.next_check)
				return false;
			entry.next_check = now + interval;
			return true;
		}

		void make_due(const Key key, const TimePoint now) noexcept
		{
			if (const auto it = m_entries.find(key); it != m_entries.end())
				it->second.next_check = now;
		}

		template<typename OnPruned>
		void prune(const TimePoint now, const Duration retention, OnPruned&& on_pruned)
		{
			for (auto it = m_entries.begin(); it != m_entries.end();)
			{
				if (now - it->second.last_seen <= retention)
				{
					++it;
					continue;
				}
				std::forward<OnPruned>(on_pruned)(it->first);
				it = m_entries.erase(it);
			}
		}

		void clear() noexcept
		{
			m_entries.clear();
		}

	private:
		struct Entry
		{
			TimePoint next_check;
			TimePoint last_seen;
		};

		std::unordered_map<Key, Entry> m_entries;
	};
}
