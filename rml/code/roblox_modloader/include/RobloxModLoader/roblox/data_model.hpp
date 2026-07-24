#pragma once

#include <memory>

#include "instance.hpp"
#include "job_types.hpp"

#include "RobloxModLoader/util/layout_assert.hpp"

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
        char m_pad_0[0x26C];
        DataModelType m_type;
        char m_pad_1[0x299];
        bool m_initialized;

    public:
        DataModelType get_type() const;

        bool is_initialized() const;

        // Roblox's scheduler owns the reflection-visible DataModel Instance as
        // a subobject. Native task submission takes the owning context rather
        // than this Instance address.
        void *get_task_context() noexcept;

        static DataModel *from_job(const DataModelJob *job);

    private:
        RML_LAYOUT_GUARD_BEGIN()
            RML_ASSERT_LAYOUT_OFFSET(DataModel, m_type, 0x324);
            RML_ASSERT_LAYOUT_OFFSET(DataModel, m_initialized, 0x5C1);
        RML_LAYOUT_GUARD_END()
    };
}
