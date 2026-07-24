#include "hooks/windows/foreground_activation_policy.hpp"

#include <cstdint>

int main()
{
	using rml::hooks::windows::block_programmatic_activation;
	using rml::hooks::windows::block_foreground_thread_attachment;
	using rml::hooks::windows::suppress_window_position_activation;

	constexpr std::uint32_t studio_process = 100;
	constexpr std::uint32_t other_studio_process = 200;

	if (!block_programmatic_activation(studio_process, other_studio_process))
		return 1;
	if (block_programmatic_activation(studio_process, studio_process))
		return 2;
	if (block_programmatic_activation(studio_process, 0))
		return 3;
	if (block_programmatic_activation(0, other_studio_process))
		return 4;
	if (!block_foreground_thread_attachment(
	        true,
	        30,
	        30,
	        40,
	        studio_process,
	        other_studio_process))
		return 5;
	if (block_foreground_thread_attachment(
	        false,
	        30,
	        30,
	        40,
	        studio_process,
	        other_studio_process))
		return 6;
	if (block_foreground_thread_attachment(
	        true,
	        30,
	        40,
	        50,
	        studio_process,
	        other_studio_process))
		return 7;
	if (suppress_window_position_activation(0x0203, studio_process, studio_process, other_studio_process) != 0x0213)
		return 8;
	if (suppress_window_position_activation(0x0203, studio_process, studio_process, studio_process) != 0x0203)
		return 9;

	return 0;
}
