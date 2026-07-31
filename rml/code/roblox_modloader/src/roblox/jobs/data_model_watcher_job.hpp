#pragma once
#include "RobloxModLoader/roblox/job_base.hpp"
#include "RobloxModLoader/roblox/task_scheduler.hpp"
#include <chrono>
#include <cstdint>
#include <string_view>
#include <unordered_map>



namespace RBX {
    class DataModel;
    class ScriptContext;
    enum class DataModelType;
}

namespace rml::jobs {
    class DataModelWatcherJob final : public JobBase {
    public:
        DataModelWatcherJob() noexcept;

        ~DataModelWatcherJob() override = default;

        DataModelWatcherJob(const DataModelWatcherJob &) = delete;

        DataModelWatcherJob &operator=(const DataModelWatcherJob &) = delete;

        DataModelWatcherJob(DataModelWatcherJob &&) noexcept = delete;

        DataModelWatcherJob &operator=(DataModelWatcherJob &&) noexcept = delete;

    private:
        bool should_execute_impl(const JobExecutionContext &context) noexcept override;

        void execute_impl(const JobExecutionContext &context) override;

        void destroy_impl() noexcept override;

        static constexpr std::string_view JOB_NAME = "DataModelWatcher";
        static constexpr auto CHANGE_CHECK_INTERVAL = std::chrono::milliseconds{250};
        static constexpr auto STALE_DATA_MODEL_THRESHOLD = std::chrono::seconds{5};



        [[nodiscard]] static std::uint8_t studio_marker_priority(
            const RBX::DataModel *data_model) noexcept;

        static void on_data_model_changed(const RBX::DataModel *old_data_model,
                                          RBX::DataModel *new_data_model,
                                          RBX::ScriptContext *script_context);

        void check_and_cleanup_stale_data_models();

        std::unordered_map<RBX::DataModelType, RBX::DataModel *> m_data_models;
        std::unordered_map<RBX::DataModelType, std::chrono::steady_clock::time_point> m_data_model_last_time_stepped;
        std::chrono::steady_clock::time_point m_last_check = std::chrono::steady_clock::now();
    };
}
