#pragma once
#include "data_model_job.hpp"
#include "script_context.hpp"


namespace rml::roblox::internals {
    class RobloxInternalsProfile;
}
namespace RBX::ScriptContextFacets {
    class WaitingHybridScriptsJob : public DataModelJob {
    public:
        [[nodiscard]] ScriptContext* get_script_context(const rml::roblox::internals::RobloxInternalsProfile* profile = nullptr) const;
    };
}
