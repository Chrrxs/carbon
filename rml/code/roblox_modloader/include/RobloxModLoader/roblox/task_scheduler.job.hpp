#pragma once

#include "RobloxModLoader/util/layout_assert.hpp"

namespace RBX {
    class DataModel;

    struct Stats {
        double now;
        double last_step;
        double delta_time;
    };

    struct Error {
        double error;
    };

    class TaskSchedulerJob {
    protected:
        virtual ~TaskSchedulerJob() = default;

        virtual void unknown1() = 0;

        virtual void unknown2() = 0;

        virtual void unknown3() = 0;

        virtual void unknown4() = 0;

    public:
        virtual void destroy(bool delete_after) = 0;
    };
}
