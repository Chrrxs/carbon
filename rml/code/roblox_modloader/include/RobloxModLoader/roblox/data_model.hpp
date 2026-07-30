#pragma once

#include <expected>
#include "RobloxModLoader/roblox/reflection/runtime_layout_resolver.hpp"
#include <memory>

#include "instance.hpp"
#include "job_types.hpp"


namespace RBX {
    class DataModelJob;
}

namespace RBX {
    enum class DataModelType : std::int32_t {
        Edit = 0,
        Client = 1,
        Server = 2,
        Standalone = 3,
        Null = 4,
    };

    class DataModel : public Instance {
    public:
        std::expected<DataModelType, rml::roblox::internals::CompatibilityError> get_type() const;

        // Roblox's scheduler owns the reflection-visible DataModel Instance as
        // a subobject. Native task submission takes the owning context rather
        // than this Instance address.
        std::expected<void*, rml::roblox::internals::CompatibilityError> get_task_context() const noexcept;

        static DataModel *from_job(const DataModelJob *job, const rml::roblox::internals::RobloxInternalsProfile* profile = nullptr);
    };
}
