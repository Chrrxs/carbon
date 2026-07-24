#pragma once

#include <cstdint>

namespace rml::hooks::windows
{
	[[nodiscard]] constexpr bool block_programmatic_activation(
	    const std::uint32_t current_process_id,
	    const std::uint32_t foreground_process_id) noexcept
	{
		return current_process_id != 0 && foreground_process_id != 0 && foreground_process_id != current_process_id;
	}

	[[nodiscard]] constexpr bool block_foreground_thread_attachment(
	    const bool attach,
	    const std::uint32_t foreground_thread_id,
	    const std::uint32_t first_thread_id,
	    const std::uint32_t second_thread_id,
	    const std::uint32_t current_process_id,
	    const std::uint32_t foreground_process_id) noexcept
	{
		return attach && foreground_thread_id != 0 &&
		       (first_thread_id == foreground_thread_id || second_thread_id == foreground_thread_id) &&
		       block_programmatic_activation(current_process_id, foreground_process_id);
	}

	[[nodiscard]] constexpr std::uint32_t suppress_window_position_activation(
	    const std::uint32_t flags,
	    const std::uint32_t current_process_id,
	    const std::uint32_t target_process_id,
	    const std::uint32_t foreground_process_id) noexcept
	{
		constexpr std::uint32_t no_activate = 0x0010;
		return target_process_id == current_process_id &&
		               block_programmatic_activation(current_process_id, foreground_process_id)
		           ? flags | no_activate
		           : flags;
	}
}
