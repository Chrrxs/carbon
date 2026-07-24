#pragma once

namespace rml::hooks::windows
{
	[[nodiscard]] bool install_foreground_activation_hooks();
	void uninstall_foreground_activation_hooks();
}
