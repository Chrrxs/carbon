#pragma once
#include "RobloxModLoader/roblox/job_base.hpp"
#include "RobloxModLoader/roblox/task_scheduler.hpp"
#include "data_model_watcher_policy.hpp"

#include <chrono>
#include <cstdint>
#include <optional>
#include <string_view>
#include <unordered_map>


namespace RBX
{
	class DataModel;
	class ScriptContext;
	enum class DataModelType;
}

namespace rml::jobs
{
	class DataModelWatcherJob final : public JobBase
	{
	public:
		DataModelWatcherJob() noexcept;

		~DataModelWatcherJob() override = default;

		DataModelWatcherJob(const DataModelWatcherJob&) = delete;

		DataModelWatcherJob& operator=(const DataModelWatcherJob&) = delete;

		DataModelWatcherJob(DataModelWatcherJob&&) noexcept = delete;

		DataModelWatcherJob& operator=(DataModelWatcherJob&&) noexcept = delete;

	private:
		bool should_execute_impl(const JobExecutionContext& context) noexcept override;

		void execute_impl(const JobExecutionContext& context) override;

		void destroy_impl() noexcept override;

		static constexpr std::string_view JOB_NAME = "DataModelWatcher";
		static constexpr auto CHANGE_CHECK_INTERVAL = std::chrono::milliseconds{250};
		static constexpr auto PROVISIONAL_REPLACEMENT_WINDOW = std::chrono::seconds{5};
		static constexpr auto PROVISIONAL_ABSENCE_RESET = std::chrono::seconds{1};
		static constexpr auto STALE_DATA_MODEL_THRESHOLD = std::chrono::seconds{5};
		static constexpr auto STALE_CLEANUP_INTERVAL = std::chrono::seconds{1};
		static constexpr auto JOB_CACHE_RETENTION = std::chrono::seconds{10};


		[[nodiscard]] static std::uint8_t studio_marker_priority(const RBX::DataModel* data_model) noexcept;

		static void on_data_model_changed(const RBX::DataModel* old_data_model, RBX::DataModel* new_data_model, RBX::DataModelType data_model_type, RBX::ScriptContext* script_context);

		void check_and_cleanup_stale_data_models(std::chrono::steady_clock::time_point now);

		struct JobResolution
		{
			RBX::DataModel* data_model{};
			std::optional<RBX::DataModelType> type;
			std::uint8_t marker_priority{};
			std::uint64_t candidate_sequence{};
		};

		detail::PerJobCadence m_job_cadence;
		std::unordered_map<const void*, JobResolution> m_job_resolutions;
		std::unordered_map<RBX::DataModel*, std::uint64_t> m_data_model_candidate_sequences;
		std::unordered_map<RBX::DataModelType, RBX::DataModel*> m_data_models;
		std::unordered_map<RBX::DataModelType, std::chrono::steady_clock::time_point> m_data_model_last_time_stepped;
		std::unordered_map<RBX::DataModelType, std::chrono::steady_clock::time_point> m_provisional_window_started_at;
		std::unordered_map<RBX::DataModelType, std::chrono::steady_clock::time_point> m_data_model_absent_since;
		std::unordered_map<RBX::DataModelType, std::uint64_t> m_current_candidate_sequences;
		std::unordered_map<RBX::DataModelType, std::uint8_t> m_data_model_marker_priorities;
		std::uint64_t m_next_candidate_sequence{};
		std::chrono::steady_clock::time_point m_next_stale_cleanup{};
	};
}
