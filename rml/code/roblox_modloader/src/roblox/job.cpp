#include "RobloxModLoader/internal/common.hpp"
#include "RobloxModLoader/roblox/task_scheduler.hpp"
#include "RobloxModLoader/roblox/task_scheduler.job.hpp"
#include "RobloxModLoader/roblox/job.hpp"
#include "RobloxModLoader/roblox/internals_profile.hpp"
#include "RobloxModLoader/roblox/waiting_hybrid_scripts_job.hpp"

namespace RBX::ScriptContextFacets
{
	ScriptContext* WaitingHybridScriptsJob::get_script_context(const rml::roblox::internals::RobloxInternalsProfile* profile) const
	{
		if (profile == nullptr)
			profile = ::try_get_roblox_internals_profile();
		if (profile == nullptr)
			return nullptr;

		return profile->job().get_script_context(this);
	}
}
