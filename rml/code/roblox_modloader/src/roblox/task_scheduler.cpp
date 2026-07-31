#include "RobloxModLoader/roblox/task_scheduler.hpp"

#include "RobloxModLoader/internal/common.hpp"
#include "RobloxModLoader/internal/memory/rtti_scanner.hpp"
#include "data_model_registry.hpp"
#include "job_registry.hpp"

#include <array>
#include <cassert>

RML_LOG_SCOPE("TaskScheduler");

static RBX::TaskScheduler* s_active_task_scheduler{};

namespace RBX
{
	TaskScheduler::TaskScheduler() :
	    m_job_registry(std::make_unique<rml::JobRegistry>()),
	    m_data_model_registry(std::make_unique<rml::DataModelRegistry>()),
#if RML_ENABLE_LUAU
	    m_script_engine_registry(std::make_unique<rml::luau::ScriptEngineRegistry>()),
#endif
	    m_next_maintenance(std::chrono::steady_clock::now() + std::chrono::seconds{2})
	{
		s_active_task_scheduler = this;
		initialize_job_vtable_mappings();


		RML_INFO("TaskScheduler initialized successfully.");
	}

	TaskScheduler::~TaskScheduler()
	{
		shutdown();

		s_active_task_scheduler = nullptr;
	}

	std::expected<TaskScheduler::JobId, std::string> TaskScheduler::register_job(JobPtr job) noexcept
	{
		return m_job_registry->register_job(std::move(job));
	}

	bool TaskScheduler::unregister_job(const JobId job_id) noexcept
	{
		return m_job_registry->unregister_job(job_id);
	}

	bool TaskScheduler::unregister_job(const std::string_view job_name) noexcept
	{
		return m_job_registry->unregister_job(job_name);
	}

	void TaskScheduler::execute_jobs_for_kind(const rml::JobExecutionContext& context) noexcept
	{
		m_job_registry->execute_jobs_for_kind(context);
	}

	TaskScheduler::JobHandle TaskScheduler::get_job(const JobId job_id) const noexcept
	{
		return m_job_registry->get_job(job_id);
	}

	TaskScheduler::JobHandle TaskScheduler::get_job(const std::string_view job_name) const noexcept
	{
		return m_job_registry->get_job(job_name);
	}

	std::vector<TaskScheduler::JobId> TaskScheduler::get_jobs_by_kind(const rml::JobKind kind) const noexcept
	{
		return m_job_registry->get_jobs_by_kind(kind);
	}

	std::size_t TaskScheduler::get_job_count() const noexcept
	{
		return m_job_registry->get_job_count();
	}

	std::optional<TaskScheduler::JobStats> TaskScheduler::get_job_stats(const JobId job_id) const noexcept
	{
		return m_job_registry->get_job_stats(job_id);
	}

	void TaskScheduler::reset_stats() noexcept
	{
		m_job_registry->reset_stats();
	}

	void TaskScheduler::shutdown() noexcept
	{
		RML_INFO("Shutting down TaskScheduler...");

#if RML_ENABLE_LUAU
		m_script_engine_registry->shutdown();
#endif

		m_job_registry->shutdown();

		RML_INFO("Shutdown completed");
	}

	bool TaskScheduler::is_shutdown() const noexcept
	{
		return m_job_registry->is_shutdown();
	}

	std::optional<rml::JobKind> TaskScheduler::get_job_kind_from_vtable(void** vtable) const noexcept
	{
		return m_job_registry->get_job_kind_from_vtable(vtable);
	}

	std::optional<void**> TaskScheduler::get_vtable_for_job_kind(const rml::JobKind kind) const noexcept
	{
		return m_job_registry->get_vtable_for_job_kind(kind);
	}
	bool TaskScheduler::has_jobs_for_kind(const rml::JobKind kind) const noexcept
	{
		return m_job_registry->has_jobs_for_kind(kind);
	}

	void TaskScheduler::run_maintenance() noexcept
	{
		const auto now = std::chrono::steady_clock::now();
		auto next = m_next_maintenance.load(std::memory_order_acquire);
		if (now < next || !m_next_maintenance.compare_exchange_strong(next, now + std::chrono::seconds{2}, std::memory_order_acq_rel, std::memory_order_acquire))
		{
			return;
		}

		m_job_registry->cleanup_destroyed_jobs();
#if RML_ENABLE_LUAU
		cleanup_orphaned_script_engines();
#endif
	}


	void TaskScheduler::set_data_model(const DataModelType type, DataModel* data_model, ScriptContext* script_context)
	{
		m_data_model_registry->set_data_model(type, data_model, script_context);
	}

	const DataModel* TaskScheduler::get_data_model_by_type(const DataModelType type) noexcept
	{
		return m_data_model_registry->get_data_model_by_type(type);
	}

	void TaskScheduler::cleanup_data_model(const DataModelType data_model_type)
	{
		m_data_model_registry->cleanup_data_model(data_model_type);
	}

#if RML_ENABLE_LUAU
	std::shared_ptr<rml::luau::ScriptEngine> TaskScheduler::get_script_engine(const DataModelType data_model_type)
	{
		return m_script_engine_registry->get_script_engine(data_model_type);
	}

	std::shared_ptr<rml::luau::ScriptEngine> TaskScheduler::get_script_engine(lua_State* L)
	{
		return m_script_engine_registry->get_script_engine(L);
	}

	void TaskScheduler::cleanup_script_engine(const DataModelType data_model_type)
	{
		m_script_engine_registry->cleanup_script_engine(data_model_type);
	}

	void TaskScheduler::cleanup_orphaned_script_engines()
	{
		m_script_engine_registry->cleanup_orphaned_script_engines([this](const DataModelType data_model_type) {
			return m_data_model_registry->get_data_model_by_type(data_model_type);
		});
	}
#endif
	void TaskScheduler::initialize_job_vtable_mappings() noexcept
	{
		static constexpr std::array<std::pair<std::string_view, rml::JobKind>, 4> known_job_classes{{{"RBX::HeartbeatTask", rml::JobKind::Heartbeat}, {"RBX::PhysicsJob", rml::JobKind::Physics}, {"RBX::ScriptContextFacets::WaitingHybridScriptsJob", rml::JobKind::WaitingHybridScripts}, {"RBX::Studio::RenderJob", rml::JobKind::Render}}};

		std::size_t mapped_count = 0;
		for (const auto& [class_name, job_kind] : known_job_classes)
		{
			const auto rtti = rml::memory::rtti::RTTIManager::get_class_rtti(class_name);
			if (!rtti)
			{
				RML_WARN("RTTI for '{}' not found, skipping vtable mapping", class_name);
				continue;
			}
			const auto vtable = rtti->get_virtual_function_table();
			if (!vtable)
			{
				RML_WARN("Failed to get vtable for '{}'", class_name);
				continue;
			}
			m_job_registry->register_job_kind_vtable(job_kind, vtable);
			++mapped_count;
			RML_INFO("Mapped vtable for '{}' (kind: {}) -> 0x{:X}", class_name, std::to_underlying(job_kind), reinterpret_cast<std::uintptr_t>(vtable));
		}
		RML_INFO("Initialized vtable mappings: {}/{} job types mapped", mapped_count, known_job_classes.size());
	}

}

namespace rml
{
	RBX::TaskScheduler& task_scheduler()
	{
		assert(s_active_task_scheduler && "TaskScheduler accessed before Application initialized it");
		return *s_active_task_scheduler;
	}

	bool has_task_scheduler() noexcept
	{
		return s_active_task_scheduler != nullptr;
	}
}
