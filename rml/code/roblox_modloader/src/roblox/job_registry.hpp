#pragma once
#include "RobloxModLoader/roblox/i_task_scheduler.hpp"
#include "RobloxModLoader/roblox/job.hpp"

#include <array>
#include <atomic>
#include <condition_variable>
#include <expected>
#include <memory>
#include <mutex>
#include <optional>
#include <shared_mutex>
#include <string>
#include <unordered_map>
#include <vector>

namespace rml
{
	class JobRegistry final
	{
	public:
		using JobId = ITaskScheduler::JobId;
		using JobPtr = ITaskScheduler::JobPtr;
		using JobHandle = ITaskScheduler::JobHandle;
		using JobStats = ITaskScheduler::JobStats;

		JobRegistry() noexcept;

		~JobRegistry() = default;

		JobRegistry(const JobRegistry&) = delete;

		JobRegistry& operator=(const JobRegistry&) = delete;

		JobRegistry(JobRegistry&&) noexcept = delete;

		JobRegistry& operator=(JobRegistry&&) noexcept = delete;

		std::expected<JobId, std::string> register_job(JobPtr job) noexcept;

		bool unregister_job(JobId job_id) noexcept;

		bool unregister_job(std::string_view job_name) noexcept;

		void execute_jobs_for_kind(const JobExecutionContext& context) noexcept;

		[[nodiscard]] bool has_jobs_for_kind(JobKind kind) const noexcept;

		JobHandle get_job(JobId job_id) const noexcept;

		JobHandle get_job(std::string_view job_name) const noexcept;

		std::vector<JobId> get_jobs_by_kind(JobKind kind) const noexcept;

		std::size_t get_job_count() const noexcept;

		std::optional<JobStats> get_job_stats(JobId job_id) const noexcept;

		void reset_stats() noexcept;

		void cleanup_destroyed_jobs() noexcept;

		void shutdown() noexcept;

		bool is_shutdown() const noexcept;

		void register_job_kind_vtable(JobKind kind, void** vtable) noexcept;

		std::optional<JobKind> get_job_kind_from_vtable(void** vtable) const noexcept;

		std::optional<void**> get_vtable_for_job_kind(JobKind kind) const noexcept;

	private:
		struct JobEntry
		{
			JobHandle job;
			std::atomic<std::uint64_t> executions{0};
			std::atomic<std::uint64_t> failures{0};
			std::atomic<std::int64_t> total_execution_nanoseconds{0};
			std::atomic<std::chrono::steady_clock::time_point> last_execution;
			std::atomic<bool> removal_started{false};
			std::atomic<std::uint32_t> active_executions{0};
			std::atomic<bool> destroy_when_quiescent{false};
			std::mutex quiescence_mutex;
			std::condition_variable quiescence_cv;
			std::mutex execution_mutex;

			explicit JobEntry(JobHandle job_handle) noexcept :
			    job(std::move(job_handle)),
			    last_execution(std::chrono::steady_clock::now())
			{
			}

			[[nodiscard]] JobStats stats() const noexcept;

			void reset_stats() noexcept;

			void finish_execution() noexcept;
			void request_destroy_when_quiescent() noexcept;

			void wait_for_quiescence() noexcept;
		};

		using EntryHandle = std::shared_ptr<JobEntry>;
		using DispatchSnapshot = std::vector<EntryHandle>;

		[[nodiscard]] static std::optional<std::size_t> dispatch_index(JobKind kind) noexcept;

		void rebuild_dispatch_snapshots_locked();

		static void try_execute_job(const EntryHandle& entry, const JobExecutionContext& context) noexcept;

		static void execute_job_with_stats(JobEntry& entry, const JobExecutionContext& context) noexcept;

		JobId generate_job_id() noexcept;

		mutable std::shared_mutex m_jobs_mutex;
		std::unordered_map<JobId, EntryHandle> m_jobs;
		std::unordered_map<std::string, JobId> m_name_to_id;
		std::array<std::atomic<std::shared_ptr<const DispatchSnapshot>>, 4> m_dispatch_snapshots;
		std::atomic<JobId> m_next_job_id{1};
		std::atomic<bool> m_shutdown_requested{false};

		std::unordered_map<void**, JobKind> m_vtable_to_kind;
		std::unordered_map<JobKind, void**> m_kind_to_vtable;
	};
}
