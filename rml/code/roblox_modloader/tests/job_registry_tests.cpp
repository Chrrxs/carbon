#include "RobloxModLoader/logger/logger.hpp"
#include "roblox/job_registry.hpp"
#include "roblox/jobs/data_model_watcher_policy.hpp"

#include <atomic>
#include <barrier>
#include <chrono>
#include <condition_variable>
#include <cstdlib>
#include <future>
#include <mutex>
#include <new>
#include <string>
#include <thread>
#include <vector>

namespace
{
	std::atomic<bool> count_allocations{false};
	std::atomic<std::uint64_t> allocation_count{0};
}

void* operator new(const std::size_t size)
{
	if (count_allocations.load(std::memory_order_relaxed))
		allocation_count.fetch_add(1, std::memory_order_relaxed);
	if (void* memory = std::malloc(size))
		return memory;
	throw std::bad_alloc{};
}

void operator delete(void* memory) noexcept
{
	std::free(memory);
}

void operator delete(void* memory, std::size_t) noexcept
{
	std::free(memory);
}

std::shared_ptr<spdlog::logger> global_logger()
{
	return spdlog::default_logger();
}

namespace rml
{
	std::shared_ptr<spdlog::logger> Logger::get_logger(const std::string&)
	{
		return spdlog::default_logger();
	}
}

namespace
{
	class ProbeJob final : public rml::IJob
	{
	public:
		ProbeJob(std::string name, const rml::JobPriority priority, const rml::JobKind target, std::vector<std::string>* order = nullptr) :
		    m_name(std::move(name)),
		    m_priority(priority),
		    m_target(target),
		    m_order(order)
		{
		}

		bool should_execute(const rml::JobExecutionContext&) noexcept override
		{
			should_calls.fetch_add(1, std::memory_order_relaxed);
			return true;
		}

		void execute(const rml::JobExecutionContext&) override
		{
			if (m_order)
				m_order->push_back(m_name);
			execute_calls.fetch_add(1, std::memory_order_relaxed);
			m_state.store(rml::JobState::Finished, std::memory_order_release);
		}

		void destroy() noexcept override
		{
			if (m_state.exchange(rml::JobState::Destroyed, std::memory_order_acq_rel) != rml::JobState::Destroyed)
			{
				destroy_calls.fetch_add(1, std::memory_order_relaxed);
			}
		}

		std::string_view get_name() const noexcept override
		{
			return m_name;
		}
		rml::JobPriority get_priority() const noexcept override
		{
			return m_priority;
		}
		rml::JobKind get_target_kind() const noexcept override
		{
			return m_target;
		}
		rml::JobState get_state() const noexcept override
		{
			return m_state.load(std::memory_order_acquire);
		}
		bool is_thread_safe() const noexcept override
		{
			return true;
		}

		std::atomic<std::uint64_t> should_calls{0};
		std::atomic<std::uint64_t> execute_calls{0};
		std::atomic<std::uint64_t> destroy_calls{0};

	private:
		const std::string m_name;
		const rml::JobPriority m_priority;
		const rml::JobKind m_target;
		std::vector<std::string>* const m_order;
		std::atomic<rml::JobState> m_state{rml::JobState::Running};
	};

	struct BlockingState
	{
		std::mutex mutex;
		std::condition_variable cv;
		bool entered{};
		bool release{};
		bool execute_finished{};
		bool destroyed_before_finish{};
	};

	class BlockingJob final : public rml::IJob
	{
	public:
		explicit BlockingJob(BlockingState& state) :
		    m_state(state)
		{
		}

		bool should_execute(const rml::JobExecutionContext&) noexcept override
		{
			return true;
		}

		void execute(const rml::JobExecutionContext&) override
		{
			std::unique_lock lock(m_state.mutex);
			m_state.entered = true;
			m_state.cv.notify_all();
			m_state.cv.wait(lock, [this] {
				return m_state.release;
			});
			m_state.execute_finished = true;
		}

		void destroy() noexcept override
		{
			std::lock_guard lock(m_state.mutex);
			m_state.destroyed_before_finish = !m_state.execute_finished;
			m_destroyed.store(true, std::memory_order_release);
		}

		std::string_view get_name() const noexcept override
		{
			return "blocking";
		}
		rml::JobPriority get_priority() const noexcept override
		{
			return rml::JobPriority::Normal;
		}
		rml::JobKind get_target_kind() const noexcept override
		{
			return rml::JobKind::WaitingHybridScripts;
		}
		rml::JobState get_state() const noexcept override
		{
			return m_destroyed.load(std::memory_order_acquire) ? rml::JobState::Destroyed : rml::JobState::Running;
		}
		bool is_thread_safe() const noexcept override
		{
			return false;
		}

	private:
		BlockingState& m_state;
		std::atomic<bool> m_destroyed{false};
	};

	enum class RegistryMutation
	{
		Unregister,
		Shutdown,
	};

	class RegistryMutatingJob final : public rml::IJob
	{
	public:
		RegistryMutatingJob(rml::JobRegistry& registry, std::string name, const RegistryMutation mutation) :
		    m_registry(registry),
		    m_name(std::move(name)),
		    m_mutation(mutation)
		{
		}

		void set_id(const rml::JobRegistry::JobId id) noexcept
		{
			m_id = id;
		}

		bool should_execute(const rml::JobExecutionContext&) noexcept override
		{
			return true;
		}

		void execute(const rml::JobExecutionContext&) override
		{
			if (m_mutation == RegistryMutation::Unregister)
			{
				mutation_succeeded.store(m_registry.unregister_job(m_id), std::memory_order_release);
				return;
			}
			m_registry.shutdown();
			mutation_succeeded.store(true, std::memory_order_release);
		}

		void destroy() noexcept override
		{
			m_state.store(rml::JobState::Destroyed, std::memory_order_release);
			destroy_calls.fetch_add(1, std::memory_order_relaxed);
		}

		std::string_view get_name() const noexcept override
		{
			return m_name;
		}

		rml::JobPriority get_priority() const noexcept override
		{
			return rml::JobPriority::Normal;
		}

		rml::JobKind get_target_kind() const noexcept override
		{
			return rml::JobKind::WaitingHybridScripts;
		}

		rml::JobState get_state() const noexcept override
		{
			return m_state.load(std::memory_order_acquire);
		}

		bool is_thread_safe() const noexcept override
		{
			return true;
		}

		std::atomic<bool> mutation_succeeded{false};
		std::atomic<std::uint64_t> destroy_calls{0};

	private:
		rml::JobRegistry& m_registry;
		const std::string m_name;
		const RegistryMutation m_mutation;
		rml::JobRegistry::JobId m_id{};
		std::atomic<rml::JobState> m_state{rml::JobState::Running};
	};

	class CrossUnregisterJob final : public rml::IJob
	{
	public:
		CrossUnregisterJob(rml::JobRegistry& registry, std::string name, const rml::JobKind target_kind, std::barrier<>& rendezvous) :
		    m_registry(registry),
		    m_name(std::move(name)),
		    m_target_kind(target_kind),
		    m_rendezvous(rendezvous)
		{
		}

		void set_target(const rml::JobRegistry::JobId target) noexcept
		{
			m_target = target;
		}

		bool should_execute(const rml::JobExecutionContext&) noexcept override
		{
			return true;
		}

		void execute(const rml::JobExecutionContext&) override
		{
			m_rendezvous.arrive_and_wait();
			unregister_succeeded.store(m_registry.unregister_job(m_target), std::memory_order_release);
		}

		void destroy() noexcept override
		{
			m_state.store(rml::JobState::Destroyed, std::memory_order_release);
			destroy_calls.fetch_add(1, std::memory_order_relaxed);
		}

		std::string_view get_name() const noexcept override
		{
			return m_name;
		}

		rml::JobPriority get_priority() const noexcept override
		{
			return rml::JobPriority::Normal;
		}

		rml::JobKind get_target_kind() const noexcept override
		{
			return m_target_kind;
		}

		rml::JobState get_state() const noexcept override
		{
			return m_state.load(std::memory_order_acquire);
		}

		bool is_thread_safe() const noexcept override
		{
			return true;
		}

		std::atomic<bool> unregister_succeeded{false};
		std::atomic<std::uint64_t> destroy_calls{0};

	private:
		rml::JobRegistry& m_registry;
		const std::string m_name;
		const rml::JobKind m_target_kind;
		std::barrier<>& m_rendezvous;
		rml::JobRegistry::JobId m_target{};
		std::atomic<rml::JobState> m_state{rml::JobState::Running};
	};

	constexpr rml::JobExecutionContext waiting_context{.kind = rml::JobKind::WaitingHybridScripts, .job = nullptr, .stats = nullptr, .delta_time = 0.0};

	constexpr rml::JobExecutionContext render_context{.kind = rml::JobKind::Render, .job = nullptr, .stats = nullptr, .delta_time = 0.0};
}

int main()
{
	{
		using namespace std::chrono_literals;
		rml::jobs::detail::PerJobCadence cadence;
		int first_job{};
		int second_job{};
		const auto start = std::chrono::steady_clock::time_point{};
		if (!cadence.should_check(&first_job, start, 250ms))
			return 101;
		if (cadence.should_check(&first_job, start + 249ms, 250ms))
			return 102;
		if (!cadence.should_check(&second_job, start + 1ms, 250ms))
			return 103;
		if (!cadence.should_check(&first_job, start + 250ms, 250ms))
			return 104;
		cadence.make_due(&second_job, start + 2ms);
		if (!cadence.should_check(&second_job, start + 2ms, 250ms))
			return 105;

		std::size_t pruned = 0;
		cadence.prune(start + 11s, 10s, [&](const void*) {
			++pruned;
		});
		if (pruned != 2 || !cadence.should_check(&first_job, start + 11s, 250ms))
			return 106;
		int current_data_model{};
		int replacement_data_model{};
		if (rml::jobs::detail::should_cleanup_stale_data_model(nullptr, nullptr))
			return 107;
		if (!rml::jobs::detail::should_cleanup_stale_data_model(nullptr, &current_data_model))
			return 108;
		if (!rml::jobs::detail::should_cleanup_stale_data_model(&current_data_model, &current_data_model))
			return 109;
		if (rml::jobs::detail::should_cleanup_stale_data_model(&current_data_model, nullptr) || rml::jobs::detail::should_cleanup_stale_data_model(&current_data_model, &replacement_data_model))
			return 110;
		if (!rml::jobs::detail::should_prefer_data_model_candidate(true, 0, 2))
			return 111;
		if (!rml::jobs::detail::should_prefer_data_model_candidate(false, 2, 1))
			return 112;
		if (rml::jobs::detail::should_prefer_data_model_candidate(false, 1, 1) || rml::jobs::detail::should_prefer_data_model_candidate(false, 0, 1))
			return 113;
		if (!rml::jobs::detail::should_prefer_data_model_candidate(false, 0, 0, true))
			return 114;
		if (rml::jobs::detail::should_prefer_data_model_candidate(false, 1, 1, true))
			return 115;
		if (rml::jobs::detail::studio_marker_priority(0, true) != 0)
			return 116;
		if (rml::jobs::detail::studio_marker_priority(1, false) != 1)
			return 117;
		if (rml::jobs::detail::studio_marker_priority(1, true) != 2)
			return 118;
		if (rml::jobs::detail::studio_marker_priority(2, true) != 0)
			return 119;
	}

	{
		rml::JobRegistry registry;
		if (registry.has_jobs_for_kind(rml::JobKind::WaitingHybridScripts))
			return 1;

		std::vector<std::string> order;
		order.reserve(2);
		auto low = std::make_unique<ProbeJob>("low", rml::JobPriority::Low, rml::JobKind::WaitingHybridScripts, &order);
		auto* low_ptr = low.get();
		const auto low_id = registry.register_job(std::move(low));
		if (!low_id)
			return 2;

		auto high = std::make_unique<ProbeJob>("high", rml::JobPriority::High, rml::JobKind::WaitingHybridScripts, &order);
		const auto high_id = registry.register_job(std::move(high));
		if (!high_id)
			return 3;
		if (!registry.has_jobs_for_kind(rml::JobKind::WaitingHybridScripts) || registry.has_jobs_for_kind(rml::JobKind::Render))
			return 4;

		registry.execute_jobs_for_kind(waiting_context);
		if (order.size() != 2 || order[0] != "high" || order[1] != "low")
			return 5;
		registry.execute_jobs_for_kind(render_context);
		if (low_ptr->execute_calls.load(std::memory_order_relaxed) != 1)
			return 6;

		registry.cleanup_destroyed_jobs();
		if (registry.get_job_count() != 2)
			return 7;

		auto retained = registry.get_job(*low_id);
		if (!retained || retained.get() != low_ptr || !registry.unregister_job(*low_id))
			return 8;
		if (retained->get_state() != rml::JobState::Destroyed || low_ptr->destroy_calls.load(std::memory_order_relaxed) != 1)
			return 9;
		if (!registry.unregister_job(*high_id) || registry.has_jobs_for_kind(rml::JobKind::WaitingHybridScripts))
			return 10;

		auto custom = std::make_unique<ProbeJob>("custom", rml::JobPriority::Normal, rml::JobKind::Custom);
		const auto custom_id = registry.register_job(std::move(custom));
		if (!custom_id || !registry.has_jobs_for_kind(rml::JobKind::Render) || !registry.has_jobs_for_kind(rml::JobKind::Heartbeat))
			return 11;
		if (!registry.unregister_job(*custom_id))
			return 12;
	}

	{
		rml::JobRegistry registry;
		BlockingState state;
		const auto id = registry.register_job(std::make_unique<BlockingJob>(state));
		if (!id)
			return 13;

		std::jthread dispatcher([&] {
			registry.execute_jobs_for_kind(waiting_context);
		});
		{
			std::unique_lock lock(state.mutex);
			if (!state.cv.wait_for(lock, std::chrono::seconds{2}, [&] {
				    return state.entered;
			    }))
				return 14;
		}

		auto unregister = std::async(std::launch::async, [&] {
			return registry.unregister_job(*id);
		});
		if (unregister.wait_for(std::chrono::milliseconds{20}) != std::future_status::timeout)
			return 15;
		{
			std::lock_guard lock(state.mutex);
			state.release = true;
		}
		state.cv.notify_all();
		if (!unregister.get())
			return 16;
		dispatcher.join();
		if (state.destroyed_before_finish)
			return 17;
	}

	{
		rml::JobRegistry registry;
		auto counter = std::make_unique<ProbeJob>("counter", rml::JobPriority::Normal, rml::JobKind::WaitingHybridScripts);
		auto* counter_ptr = counter.get();
		const auto id = registry.register_job(std::move(counter));
		if (!id)
			return 18;
		registry.execute_jobs_for_kind(waiting_context);

		allocation_count.store(0, std::memory_order_relaxed);
		count_allocations.store(true, std::memory_order_release);
		for (std::size_t i = 0; i < 10'000; ++i)
			registry.execute_jobs_for_kind(waiting_context);
		count_allocations.store(false, std::memory_order_release);
		if (allocation_count.load(std::memory_order_relaxed) != 0)
			return 19;
		if (counter_ptr->execute_calls.load(std::memory_order_relaxed) != 10'001)
			return 20;
		const auto stats = registry.get_job_stats(*id);
		if (!stats || stats->executions != 10'001 || stats->failures != 0)
			return 21;
		if (!registry.unregister_job(*id))
			return 22;
	}


	{
		rml::JobRegistry registry;
		auto job = std::make_unique<RegistryMutatingJob>(registry, "self-unregister", RegistryMutation::Unregister);
		auto* job_ptr = job.get();
		const auto id = registry.register_job(std::move(job));
		if (!id)
			return 23;
		job_ptr->set_id(*id);
		const auto retained = registry.get_job(*id);
		registry.execute_jobs_for_kind(waiting_context);
		if (!job_ptr->mutation_succeeded.load(std::memory_order_acquire) || registry.get_job_count() != 0
		    || job_ptr->destroy_calls.load(std::memory_order_relaxed) != 1 || retained->get_state() != rml::JobState::Destroyed)
			return 24;
	}

	{
		rml::JobRegistry registry;
		auto job = std::make_unique<RegistryMutatingJob>(registry, "self-shutdown", RegistryMutation::Shutdown);
		auto* job_ptr = job.get();
		const auto id = registry.register_job(std::move(job));
		if (!id)
			return 25;
		const auto retained = registry.get_job(*id);
		registry.execute_jobs_for_kind(waiting_context);
		if (!job_ptr->mutation_succeeded.load(std::memory_order_acquire) || !registry.is_shutdown()
		    || job_ptr->destroy_calls.load(std::memory_order_relaxed) != 1 || retained->get_state() != rml::JobState::Destroyed)
			return 26;
		if (registry.register_job(std::make_unique<ProbeJob>("late", rml::JobPriority::Normal, rml::JobKind::WaitingHybridScripts)))
			return 27;
	}

	{
		rml::JobRegistry registry;
		std::barrier rendezvous{2};
		auto heartbeat = std::make_unique<CrossUnregisterJob>(registry, "cross-heartbeat", rml::JobKind::Heartbeat, rendezvous);
		auto* heartbeat_ptr = heartbeat.get();
		auto physics = std::make_unique<CrossUnregisterJob>(registry, "cross-physics", rml::JobKind::Physics, rendezvous);
		auto* physics_ptr = physics.get();
		const auto heartbeat_id = registry.register_job(std::move(heartbeat));
		const auto physics_id = registry.register_job(std::move(physics));
		if (!heartbeat_id || !physics_id)
			return 28;
		heartbeat_ptr->set_target(*physics_id);
		physics_ptr->set_target(*heartbeat_id);
		const auto retained_heartbeat = registry.get_job(*heartbeat_id);
		const auto retained_physics = registry.get_job(*physics_id);

		constexpr rml::JobExecutionContext heartbeat_context{.kind = rml::JobKind::Heartbeat, .job = nullptr, .stats = nullptr, .delta_time = 0.0};
		constexpr rml::JobExecutionContext physics_context{.kind = rml::JobKind::Physics, .job = nullptr, .stats = nullptr, .delta_time = 0.0};
		auto heartbeat_dispatch = std::async(std::launch::async, [&] {
			registry.execute_jobs_for_kind(heartbeat_context);
		});
		auto physics_dispatch = std::async(std::launch::async, [&] {
			registry.execute_jobs_for_kind(physics_context);
		});
		if (heartbeat_dispatch.wait_for(std::chrono::seconds{2}) != std::future_status::ready || physics_dispatch.wait_for(std::chrono::seconds{2}) != std::future_status::ready)
		{
			std::_Exit(29);
		}
		heartbeat_dispatch.get();
		physics_dispatch.get();
		if (!heartbeat_ptr->unregister_succeeded.load(std::memory_order_acquire) || !physics_ptr->unregister_succeeded.load(std::memory_order_acquire)
		    || registry.get_job_count() != 0)
		{
			return 30;
		}
		if (heartbeat_ptr->destroy_calls.load(std::memory_order_relaxed) != 1 || physics_ptr->destroy_calls.load(std::memory_order_relaxed) != 1
		    || retained_heartbeat->get_state() != rml::JobState::Destroyed || retained_physics->get_state() != rml::JobState::Destroyed)
		{
			return 31;
		}
	}
	return 0;
}
