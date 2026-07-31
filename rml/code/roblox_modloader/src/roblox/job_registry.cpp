#include "job_registry.hpp"

#include "RobloxModLoader/internal/common.hpp"

RML_LOG_SCOPE("JobRegistry");

namespace
{
	thread_local const void* executing_job_entry{};
}

namespace rml
{
	JobRegistry::JobRegistry() noexcept
	{
		const auto empty_snapshot = std::make_shared<const DispatchSnapshot>();
		for (auto& snapshot : m_dispatch_snapshots)
		{
			snapshot.store(empty_snapshot, std::memory_order_relaxed);
		}
	}

	JobRegistry::JobStats JobRegistry::JobEntry::stats() const noexcept
	{
		JobStats result{};
		result.executions = executions.load(std::memory_order_relaxed);
		result.failures = failures.load(std::memory_order_relaxed);
		result.total_execution_time = std::chrono::nanoseconds{total_execution_nanoseconds.load(std::memory_order_relaxed)};
		if (result.executions != 0)
		{
			result.average_execution_time = result.total_execution_time / static_cast<std::int64_t>(result.executions);
		}
		return result;
	}

	void JobRegistry::JobEntry::reset_stats() noexcept
	{
		executions.store(0, std::memory_order_relaxed);
		failures.store(0, std::memory_order_relaxed);
		total_execution_nanoseconds.store(0, std::memory_order_relaxed);
	}
	void JobRegistry::JobEntry::finish_execution() noexcept
	{
		if (active_executions.fetch_sub(1, std::memory_order_acq_rel) != 1 || !removal_started.load(std::memory_order_acquire))
		{
			return;
		}
		quiescence_cv.notify_all();
		if (destroy_when_quiescent.exchange(false, std::memory_order_acq_rel))
		{
			job->destroy();
		}
	}

	void JobRegistry::JobEntry::request_destroy_when_quiescent() noexcept
	{
		destroy_when_quiescent.store(true, std::memory_order_release);
		if (active_executions.load(std::memory_order_acquire) == 0 && destroy_when_quiescent.exchange(false, std::memory_order_acq_rel))
		{
			job->destroy();
		}
	}

	void JobRegistry::JobEntry::wait_for_quiescence() noexcept
	{
		std::unique_lock lock(quiescence_mutex);
		quiescence_cv.wait(lock, [this] {
			return active_executions.load(std::memory_order_acquire) == 0;
		});
	}


	std::expected<JobRegistry::JobId, std::string> JobRegistry::register_job(JobPtr job) noexcept
	{
		if (!job)
		{
			return std::unexpected("Cannot register null job");
		}
		if (m_shutdown_requested.load(std::memory_order_acquire))
		{
			return std::unexpected("TaskScheduler is shutting down");
		}

		const std::string job_name{job->get_name()};
		const auto job_id = generate_job_id();
		auto entry = std::make_shared<JobEntry>(JobHandle{std::move(job)});

		std::unique_lock lock(m_jobs_mutex);
		if (m_shutdown_requested.load(std::memory_order_acquire))
		{
			return std::unexpected("TaskScheduler is shutting down");
		}
		if (m_name_to_id.contains(job_name))
		{
			return std::unexpected(std::format("Job with name '{}' already exists", job_name));
		}

		try
		{
			m_jobs.emplace(job_id, entry);
			m_name_to_id.emplace(job_name, job_id);
			rebuild_dispatch_snapshots_locked();
		}
		catch (const std::exception& e)
		{
			m_name_to_id.erase(job_name);
			m_jobs.erase(job_id);
			return std::unexpected(std::format("Failed to register job '{}': {}", job_name, e.what()));
		}

		RML_DEBUG("Registered job '{}' with ID {}", job_name, job_id);
		return job_id;
	}

	bool JobRegistry::unregister_job(const JobId job_id) noexcept
	{
		EntryHandle entry;
		{
			std::shared_lock lock(m_jobs_mutex);
			const auto it = m_jobs.find(job_id);
			if (it == m_jobs.end())
			{
				return false;
			}
			entry = it->second;
		}

		if (entry->removal_started.exchange(true, std::memory_order_acq_rel))
		{
			return false;
		}

		const std::string job_name{entry->job->get_name()};

		{
			std::unique_lock lock(m_jobs_mutex);
			const auto it = m_jobs.find(job_id);
			if (it == m_jobs.end() || it->second != entry)
			{
				return false;
			}
			m_name_to_id.erase(job_name);
			m_jobs.erase(it);
			try
			{
				rebuild_dispatch_snapshots_locked();
			}
			catch (const std::exception& e)
			{
				RML_ERROR("Failed to rebuild job dispatch snapshots after unregistering '{}': {}", job_name, e.what());
			}
		}
		if (executing_job_entry)
		{
			entry->request_destroy_when_quiescent();
		}
		else
		{
			entry->wait_for_quiescence();
			entry->job->destroy();
		}


		RML_DEBUG("Unregistered job '{}' (ID: {})", job_name, job_id);
		return true;
	}

	bool JobRegistry::unregister_job(const std::string_view job_name) noexcept
	{
		JobId job_id{};
		{
			std::shared_lock lock(m_jobs_mutex);
			const auto name_it = m_name_to_id.find(std::string(job_name));
			if (name_it == m_name_to_id.end())
			{
				return false;
			}
			job_id = name_it->second;
		}
		return unregister_job(job_id);
	}

	void JobRegistry::execute_jobs_for_kind(const JobExecutionContext& context) noexcept
	{
		if (m_shutdown_requested.load(std::memory_order_acquire))
		{
			return;
		}
		const auto index = dispatch_index(context.kind);
		if (!index)
		{
			return;
		}
		const auto snapshot = m_dispatch_snapshots[*index].load(std::memory_order_acquire);
		if (!snapshot)
		{
			return;
		}
		for (const auto& entry : *snapshot)
		{
			try_execute_job(entry, context);
		}
	}

	bool JobRegistry::has_jobs_for_kind(const JobKind kind) const noexcept
	{
		const auto index = dispatch_index(kind);
		if (!index)
		{
			return false;
		}
		const auto snapshot = m_dispatch_snapshots[*index].load(std::memory_order_acquire);
		return snapshot && !snapshot->empty();
	}

	JobRegistry::JobHandle JobRegistry::get_job(const JobId job_id) const noexcept
	{
		std::shared_lock lock(m_jobs_mutex);
		if (const auto it = m_jobs.find(job_id); it != m_jobs.end())
		{
			return it->second->job;
		}
		return {};
	}

	JobRegistry::JobHandle JobRegistry::get_job(const std::string_view job_name) const noexcept
	{
		std::shared_lock lock(m_jobs_mutex);
		const auto name_it = m_name_to_id.find(std::string(job_name));
		if (name_it == m_name_to_id.end())
		{
			return {};
		}
		if (const auto job_it = m_jobs.find(name_it->second); job_it != m_jobs.end())
		{
			return job_it->second->job;
		}
		return {};
	}

	std::vector<JobRegistry::JobId> JobRegistry::get_jobs_by_kind(const JobKind kind) const noexcept
	{
		std::vector<JobId> result;
		std::shared_lock lock(m_jobs_mutex);
		for (const auto& [job_id, entry] : m_jobs)
		{
			if (kind == JobKind::Custom || entry->job->get_target_kind() == JobKind::Custom || has_job_kind(entry->job->get_target_kind(), kind))
			{
				result.push_back(job_id);
			}
		}
		return result;
	}

	std::size_t JobRegistry::get_job_count() const noexcept
	{
		std::shared_lock lock(m_jobs_mutex);
		return m_jobs.size();
	}

	std::optional<JobRegistry::JobStats> JobRegistry::get_job_stats(const JobId job_id) const noexcept
	{
		EntryHandle entry;
		{
			std::shared_lock lock(m_jobs_mutex);
			const auto it = m_jobs.find(job_id);
			if (it == m_jobs.end())
			{
				return std::nullopt;
			}
			entry = it->second;
		}
		return entry->stats();
	}

	void JobRegistry::reset_stats() noexcept
	{
		std::shared_lock lock(m_jobs_mutex);
		for (const auto& entry : m_jobs | std::views::values)
		{
			entry->reset_stats();
		}
	}

	void JobRegistry::cleanup_destroyed_jobs() noexcept
	{
		std::unique_lock lock(m_jobs_mutex);
		bool changed = false;
		for (auto it = m_jobs.begin(); it != m_jobs.end();)
		{
			const auto& entry = it->second;
			if (entry->job->get_state() != JobState::Destroyed)
			{
				++it;
				continue;
			}
			const std::string job_name{entry->job->get_name()};
			entry->removal_started.store(true, std::memory_order_release);
			m_name_to_id.erase(job_name);
			it = m_jobs.erase(it);
			changed = true;
			RML_DEBUG("Cleaned up destroyed job '{}'", job_name);
		}
		if (changed)
		{
			try
			{
				rebuild_dispatch_snapshots_locked();
			}
			catch (const std::exception& e)
			{
				RML_ERROR("Failed to rebuild job dispatch snapshots during maintenance: {}", e.what());
			}
		}
	}

	void JobRegistry::shutdown() noexcept
	{
		if (m_shutdown_requested.exchange(true, std::memory_order_acq_rel))
		{
			return;
		}

		std::vector<EntryHandle> entries;
		{
			std::unique_lock lock(m_jobs_mutex);
			entries.reserve(m_jobs.size());
			for (const auto& entry : m_jobs | std::views::values)
			{
				entry->removal_started.store(true, std::memory_order_release);
				entries.push_back(entry);
			}
			m_jobs.clear();
			m_name_to_id.clear();
			const auto empty_snapshot = std::make_shared<const DispatchSnapshot>();
			for (auto& snapshot : m_dispatch_snapshots)
			{
				snapshot.store(empty_snapshot, std::memory_order_release);
			}
		}
		if (executing_job_entry)
		{
			for (const auto& entry : entries)
			{
				entry->request_destroy_when_quiescent();
			}
			return;
		}
		for (const auto& entry : entries)
		{
			entry->wait_for_quiescence();
			entry->job->destroy();
		}
	}

	bool JobRegistry::is_shutdown() const noexcept
	{
		return m_shutdown_requested.load(std::memory_order_acquire);
	}

	void JobRegistry::register_job_kind_vtable(const JobKind kind, void** vtable) noexcept
	{
		if (!vtable)
		{
			return;
		}
		m_vtable_to_kind.insert_or_assign(vtable, kind);
		m_kind_to_vtable.insert_or_assign(kind, vtable);
	}

	std::optional<JobKind> JobRegistry::get_job_kind_from_vtable(void** vtable) const noexcept
	{
		if (const auto it = m_vtable_to_kind.find(vtable); it != m_vtable_to_kind.end())
		{
			return it->second;
		}
		return std::nullopt;
	}

	std::optional<void**> JobRegistry::get_vtable_for_job_kind(const JobKind kind) const noexcept
	{
		if (const auto it = m_kind_to_vtable.find(kind); it != m_kind_to_vtable.end())
		{
			return it->second;
		}
		return std::nullopt;
	}

	std::optional<std::size_t> JobRegistry::dispatch_index(const JobKind kind) noexcept
	{
		switch (kind)
		{
		case JobKind::Heartbeat: return 0;
		case JobKind::Physics: return 1;
		case JobKind::Render: return 2;
		case JobKind::WaitingHybridScripts: return 3;
		default: return std::nullopt;
		}
	}

	void JobRegistry::rebuild_dispatch_snapshots_locked()
	{
		std::array<std::shared_ptr<DispatchSnapshot>, 4> next;
		for (auto& snapshot : next)
		{
			snapshot = std::make_shared<DispatchSnapshot>();
			snapshot->reserve(m_jobs.size());
		}

		for (const auto& entry : m_jobs | std::views::values)
		{
			const auto target = entry->job->get_target_kind();
			for (std::size_t index = 0; index < next.size(); ++index)
			{
				constexpr std::array kinds{JobKind::Heartbeat, JobKind::Physics, JobKind::Render, JobKind::WaitingHybridScripts};
				if (target == JobKind::Custom || has_job_kind(target, kinds[index]))
				{
					next[index]->push_back(entry);
				}
			}
		}

		for (std::size_t index = 0; index < next.size(); ++index)
		{
			std::ranges::sort(*next[index], [](const EntryHandle& lhs, const EntryHandle& rhs) {
				return static_cast<std::int32_t>(lhs->job->get_priority()) < static_cast<std::int32_t>(rhs->job->get_priority());
			});
			m_dispatch_snapshots[index].store(std::shared_ptr<const DispatchSnapshot>{std::move(next[index])}, std::memory_order_release);
		}
	}

	void JobRegistry::try_execute_job(const EntryHandle& entry, const JobExecutionContext& context) noexcept
	{
		if (entry->removal_started.load(std::memory_order_acquire) || entry->job->get_state() == JobState::Destroyed)
		{
			return;
		}

		entry->active_executions.fetch_add(1, std::memory_order_acq_rel);
		struct ExecutionGuard
		{
			JobEntry& entry;
			const void* previous_entry;
			~ExecutionGuard()
			{
				executing_job_entry = previous_entry;
				entry.finish_execution();
			}
		} execution_guard{*entry, executing_job_entry};
		executing_job_entry = entry.get();

		if (entry->removal_started.load(std::memory_order_acquire) || entry->job->get_state() == JobState::Destroyed)
		{
			return;
		}

		if (!entry->job->is_thread_safe())
		{
			std::lock_guard lock(entry->execution_mutex);
			if (entry->removal_started.load(std::memory_order_acquire) || entry->job->get_state() == JobState::Destroyed
			    || !entry->job->should_execute(context) || entry->removal_started.load(std::memory_order_acquire))
			{
				return;
			}
			execute_job_with_stats(*entry, context);
			return;
		}

		if (entry->job->should_execute(context) && !entry->removal_started.load(std::memory_order_acquire))
		{
			execute_job_with_stats(*entry, context);
		}
	}

	void JobRegistry::execute_job_with_stats(JobEntry& entry, const JobExecutionContext& context) noexcept
	{
		const auto start_time = std::chrono::steady_clock::now();
		try
		{
			entry.job->execute(context);
			entry.executions.fetch_add(1, std::memory_order_relaxed);
		}
		catch (const std::exception& e)
		{
			entry.failures.fetch_add(1, std::memory_order_relaxed);
			RML_ERROR("Job '{}' execution failed: {}", entry.job->get_name(), e.what());
		}
		catch (...)
		{
			entry.failures.fetch_add(1, std::memory_order_relaxed);
			RML_ERROR("Job '{}' execution failed with unknown exception", entry.job->get_name());
		}

		const auto end_time = std::chrono::steady_clock::now();
		const auto execution_time = std::chrono::duration_cast<std::chrono::nanoseconds>(end_time - start_time);
		entry.total_execution_nanoseconds.fetch_add(execution_time.count(), std::memory_order_relaxed);
		entry.last_execution.store(end_time, std::memory_order_relaxed);
	}

	JobRegistry::JobId JobRegistry::generate_job_id() noexcept
	{
		return m_next_job_id.fetch_add(1, std::memory_order_relaxed);
	}
}
