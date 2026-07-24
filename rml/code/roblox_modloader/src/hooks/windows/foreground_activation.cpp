#include "foreground_activation.hpp"
#include "foreground_activation_policy.hpp"

#if defined(_WIN32)

#include "RobloxModLoader/hooking/hooking.hpp"
#include "RobloxModLoader/internal/common.hpp"
#include "RobloxModLoader/internal/hooking/engine_hooks.hpp"

#include <algorithm>
#include <array>
#include <cstddef>
#include <cstring>
#include <string_view>
#include <vector>

RML_LOG_SCOPE("ForegroundActivation");

namespace
{
	struct ImportPatch
	{
		IMAGE_THUNK_DATA* slot{};
		ULONG_PTR original{};
		ULONG_PTR replacement{};
	};

	struct CbtHook
	{
		DWORD thread_id{};
		HHOOK handle{};
	};

	struct ForegroundOwner
	{
		DWORD thread_id{};
		DWORD process_id{};
	};

	[[nodiscard]] ForegroundOwner foreground_owner()
	{
		ForegroundOwner owner;
		owner.thread_id = GetWindowThreadProcessId(GetForegroundWindow(), &owner.process_id);
		return owner;
	}

	std::vector<ImportPatch> g_qwindows_import_patches;
	std::vector<CbtHook> g_cbt_hooks;

	[[nodiscard]] bool keyboard_task_switch_in_progress()
	{
		constexpr SHORT key_down = static_cast<SHORT>(0x8000);
		return (GetAsyncKeyState(VK_MENU) & key_down) != 0 ||
		       (GetAsyncKeyState(VK_LWIN) & key_down) != 0 ||
		       (GetAsyncKeyState(VK_RWIN) & key_down) != 0;
	}

	LRESULT CALLBACK foreground_cbt_hook(const int code, const WPARAM w_param, const LPARAM l_param)
	{
		if (code == HCBT_ACTIVATE)
		{
			const auto target = reinterpret_cast<HWND>(w_param);
			DWORD target_process_id{};
			GetWindowThreadProcessId(target, &target_process_id);
			const auto foreground = foreground_owner();
			const auto activation = reinterpret_cast<const CBTACTIVATESTRUCT*>(l_param);
			const auto mouse_activation = activation && activation->fMouse != FALSE;
			if (target_process_id == GetCurrentProcessId() && !mouse_activation &&
			    !keyboard_task_switch_in_progress() &&
			    rml::hooks::windows::block_programmatic_activation(GetCurrentProcessId(), foreground.process_id))
			{
				RML_DEBUG("Blocked a non-interactive Studio activation before Windows changed the foreground");
				return 1;
			}
		}

		return CallNextHookEx(nullptr, code, w_param, l_param);
	}

	BOOL CALLBACK install_cbt_hook_for_studio_window(const HWND window, const LPARAM)
	{
		DWORD process_id{};
		const auto thread_id = GetWindowThreadProcessId(window, &process_id);
		if (process_id != GetCurrentProcessId() || thread_id == 0)
			return TRUE;

		if (std::ranges::any_of(g_cbt_hooks, [thread_id](const CbtHook& hook) { return hook.thread_id == thread_id; }))
			return TRUE;

		if (const auto hook = SetWindowsHookExW(WH_CBT, foreground_cbt_hook, nullptr, thread_id))
			g_cbt_hooks.push_back({.thread_id = thread_id, .handle = hook});

		return TRUE;
	}

	[[nodiscard]] bool write_import_slot(IMAGE_THUNK_DATA* const slot, const ULONG_PTR value)
	{
		DWORD old_protection{};
		if (!VirtualProtect(&slot->u1.Function, sizeof(slot->u1.Function), PAGE_READWRITE, &old_protection))
			return false;

		slot->u1.Function = value;

		DWORD ignored{};
		if (!VirtualProtect(&slot->u1.Function, sizeof(slot->u1.Function), old_protection, &ignored))
			RML_WARN("A foreground import hook was written but its original page protection could not be restored");
		return true;
	}

	[[nodiscard]] bool patch_import(
	    const HMODULE module,
	    const std::string_view imported_name,
	    void* const replacement)
	{
		const auto base = reinterpret_cast<std::byte*>(module);
		const auto dos = reinterpret_cast<const IMAGE_DOS_HEADER*>(base);
		if (!dos || dos->e_magic != IMAGE_DOS_SIGNATURE || dos->e_lfanew <= 0)
			return false;

		const auto nt = reinterpret_cast<const IMAGE_NT_HEADERS*>(base + dos->e_lfanew);
		if (nt->Signature != IMAGE_NT_SIGNATURE)
			return false;

		const auto& import_directory = nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT];
		if (import_directory.VirtualAddress == 0 || import_directory.Size < sizeof(IMAGE_IMPORT_DESCRIPTOR))
			return false;

		const auto descriptor_count = import_directory.Size / sizeof(IMAGE_IMPORT_DESCRIPTOR);
		auto descriptors = reinterpret_cast<const IMAGE_IMPORT_DESCRIPTOR*>(base + import_directory.VirtualAddress);
		for (std::size_t descriptor_index = 0; descriptor_index < descriptor_count; ++descriptor_index)
		{
			const auto& descriptor = descriptors[descriptor_index];
			if (descriptor.Name == 0)
				break;
			if (descriptor.OriginalFirstThunk == 0 || descriptor.FirstThunk == 0)
				continue;

			auto lookup = reinterpret_cast<const IMAGE_THUNK_DATA*>(base + descriptor.OriginalFirstThunk);
			auto slots = reinterpret_cast<IMAGE_THUNK_DATA*>(base + descriptor.FirstThunk);
			for (std::size_t thunk_index = 0; lookup[thunk_index].u1.AddressOfData != 0; ++thunk_index)
			{
				if (IMAGE_SNAP_BY_ORDINAL(lookup[thunk_index].u1.Ordinal))
					continue;

				const auto import = reinterpret_cast<const IMAGE_IMPORT_BY_NAME*>(base + lookup[thunk_index].u1.AddressOfData);
				if (std::strcmp(reinterpret_cast<const char*>(import->Name), imported_name.data()) != 0)
					continue;

				auto* const slot = &slots[thunk_index];
				const ImportPatch patch{
				    .slot = slot,
				    .original = slot->u1.Function,
				    .replacement = reinterpret_cast<ULONG_PTR>(replacement),
				};
				if (!write_import_slot(slot, patch.replacement))
					return false;

				g_qwindows_import_patches.push_back(patch);
				return true;
			}
		}

		return false;
	}
}

bool rml::hooks::windows::install_foreground_activation_hooks()
{
	if (!g_qwindows_import_patches.empty() && !g_cbt_hooks.empty())
		return true;

	if (const auto qwindows = GetModuleHandleW(L"qwindows.dll"))
	{
		const std::array imports{
		    std::pair{"AttachThreadInput", reinterpret_cast<void*>(&Hooks::attach_thread_input)},
		    std::pair{"SetForegroundWindow", reinterpret_cast<void*>(&Hooks::set_foreground_window)},
		    std::pair{"SetFocus", reinterpret_cast<void*>(&Hooks::set_focus)},
		    std::pair{"SetWindowPos", reinterpret_cast<void*>(&Hooks::set_window_pos)},
		};

		for (const auto& [name, replacement] : imports)
		{
			if (!patch_import(qwindows, name, replacement))
				RML_WARN("qwindows.dll does not expose a patchable {} import; the activation veto remains active", name);
		}
	}
	else
	{
		RML_WARN("qwindows.dll is not loaded; the activation veto remains active");
	}

	EnumWindows(install_cbt_hook_for_studio_window, 0);
	if (g_cbt_hooks.empty())
	{
		RML_ERROR("Cannot install foreground guard: Studio has no hookable UI thread");
		uninstall_foreground_activation_hooks();
		return false;
	}

	RML_INFO(
	    "Installed qwindows foreground guard on {} Win32 imports and {} Studio UI threads",
	    g_qwindows_import_patches.size(),
	    g_cbt_hooks.size());
	return true;
}

void rml::hooks::windows::uninstall_foreground_activation_hooks()
{
	for (const auto& hook : g_cbt_hooks)
	{
		if (!UnhookWindowsHookEx(hook.handle))
			RML_WARN("Failed to remove a Studio foreground CBT hook during shutdown");
	}
	g_cbt_hooks.clear();

	for (auto patch = g_qwindows_import_patches.rbegin(); patch != g_qwindows_import_patches.rend(); ++patch)
	{
		if (patch->slot->u1.Function == patch->replacement && !write_import_slot(patch->slot, patch->original))
			RML_WARN("Failed to restore a qwindows foreground import hook during shutdown");
	}
	g_qwindows_import_patches.clear();
}

void rml::Hooks::q_window_request_activate(void* const window)
{
	// QWindow::requestActivate is the stable semantic boundary before Qt's
	// platform plugin issues SetWindowPos, SetForegroundWindow, or SetFocus.
	if (hooks::windows::block_programmatic_activation(GetCurrentProcessId(), foreground_owner().process_id))
		return;

	Hooking::get_original<&Hooks::q_window_request_activate>()(window);
}

BOOL WINAPI rml::Hooks::attach_thread_input(
	const DWORD attach_thread,
	const DWORD attach_to_thread,
	const BOOL attach)
{
	const auto foreground = foreground_owner();
	if (hooks::windows::block_foreground_thread_attachment(
	        attach != FALSE,
	        foreground.thread_id,
	        attach_thread,
	        attach_to_thread,
	        GetCurrentProcessId(),
	        foreground.process_id))
	{
		RML_DEBUG("Blocked qwindows from attaching Studio input to an external foreground thread");
		return FALSE;
	}

	return Hooking::get_original<&Hooks::attach_thread_input>()(attach_thread, attach_to_thread, attach);
}

BOOL WINAPI rml::Hooks::set_foreground_window(const HWND window)
{
	DWORD target_process_id{};
	GetWindowThreadProcessId(window, &target_process_id);

	const auto current_process_id = GetCurrentProcessId();
	if (target_process_id == current_process_id &&
	    hooks::windows::block_programmatic_activation(current_process_id, foreground_owner().process_id))
	{
		RML_DEBUG("Blocked qwindows SetForegroundWindow while another process owns the foreground");
		return TRUE;
	}

	return Hooking::get_original<&Hooks::set_foreground_window>()(window);
}

HWND WINAPI rml::Hooks::set_focus(const HWND window)
{
	DWORD target_process_id{};
	GetWindowThreadProcessId(window, &target_process_id);

	const auto current_process_id = GetCurrentProcessId();
	if (target_process_id == current_process_id &&
	    hooks::windows::block_programmatic_activation(current_process_id, foreground_owner().process_id))
	{
		RML_DEBUG("Blocked qwindows SetFocus while another process owns the foreground");
		return GetFocus();
	}

	return Hooking::get_original<&Hooks::set_focus>()(window);
}

BOOL WINAPI rml::Hooks::set_window_pos(
	const HWND window,
	const HWND insert_after,
	const int x,
	const int y,
	const int width,
	const int height,
	const UINT flags)
{
	DWORD target_process_id{};
	GetWindowThreadProcessId(window, &target_process_id);
	const auto foreground = foreground_owner();
	const auto guarded_flags = hooks::windows::suppress_window_position_activation(
	    flags,
	    GetCurrentProcessId(),
	    target_process_id,
	    foreground.process_id);

	return Hooking::get_original<&Hooks::set_window_pos>()(
	    window,
	    insert_after,
	    x,
	    y,
	    width,
	    height,
	    guarded_flags);
}

#endif
