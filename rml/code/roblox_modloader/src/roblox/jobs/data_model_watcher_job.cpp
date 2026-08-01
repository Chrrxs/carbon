#include "data_model_watcher_job.hpp"

#include "../../mod/mod_manager.hpp"
#include "RobloxModLoader/internal/common.hpp"
#include "RobloxModLoader/luau/script_manager.hpp"
#include "RobloxModLoader/roblox/data_model.hpp"
#include "RobloxModLoader/roblox/internals_profile.hpp"
#include "RobloxModLoader/roblox/script_context.hpp"
#include "RobloxModLoader/roblox/task_scheduler.hpp"
#include "RobloxModLoader/roblox/waiting_hybrid_scripts_job.hpp"
#include "dotnet/dotnet_mod_loader.hpp"
#include "pointers.hpp"

#include <utility>
#include <vector>

namespace rml::jobs
{
	DataModelWatcherJob::DataModelWatcherJob() noexcept :
	    JobBase(JOB_NAME, JobPriority::High, JobKind::WaitingHybridScripts, false)
	{
	}

	bool DataModelWatcherJob::should_execute_impl(const JobExecutionContext& context) noexcept
	{
		const auto job = context.job_as<RBX::ScriptContextFacets::WaitingHybridScriptsJob>();
		if (!job)
		{
			return false;
		}

		const auto now = std::chrono::steady_clock::now();
		if (!m_job_cadence.should_check(job, now, CHANGE_CHECK_INTERVAL))
		{
			return false;
		}
		auto& resolution = m_job_resolutions[job];

		const auto data_model = RBX::DataModel::from_job(job);
		if (!data_model)
		{
			resolution.data_model = nullptr;
			resolution.type.reset();
			return false;
		}

		const auto marker_priority = studio_marker_priority(data_model);

		RBX::DataModelType type{};
		if (resolution.data_model == data_model && resolution.type)
		{
			type = *resolution.type;
		}
		else
		{
			const auto type_res = data_model->get_type();
			if (!type_res)
			{
				resolution.data_model = nullptr;
				resolution.type.reset();
				LOG_ERROR("DataModelWatcherJob: Failed to resolve DataModel type");
				return false;
			}
			type = *type_res;
			resolution.data_model = data_model;
			resolution.type = type;
			auto [sequence, inserted] = m_data_model_candidate_sequences.try_emplace(data_model, 0);
			if (inserted)
				sequence->second = ++m_next_candidate_sequence;
			resolution.candidate_sequence = sequence->second;
		}
		resolution.marker_priority = marker_priority;

		auto& scheduler = rml::task_scheduler();
		scheduler.run_maintenance();
		const auto current_data_model = scheduler.get_data_model_by_type(type);
		const auto current_priority = m_data_model_marker_priorities.find(type);
		const auto current_marker_priority =
		    current_priority != m_data_model_marker_priorities.end() ? current_priority->second : 0;
		const auto window_started_at = m_provisional_window_started_at.find(type);
		const auto current_sequence = m_current_candidate_sequences.find(type);
		const bool current_is_provisional =
		    current_marker_priority == 0 &&
		    window_started_at != m_provisional_window_started_at.end() &&
		    now - window_started_at->second <= PROVISIONAL_REPLACEMENT_WINDOW &&
		    current_sequence != m_current_candidate_sequences.end() &&
		    resolution.candidate_sequence > current_sequence->second;
		if (current_data_model == data_model)
		{
			m_data_model_last_time_stepped[type] = now;
			m_data_model_marker_priorities[type] = marker_priority;
		}
		check_and_cleanup_stale_data_models(now);

		if (!current_data_model)
		{
			return true;
		}
		if (current_data_model == data_model)
		{
			return false;
		}

		const auto last_step = m_data_model_last_time_stepped.find(type);
		const bool current_is_stale = last_step == m_data_model_last_time_stepped.end() || now - last_step->second > STALE_DATA_MODEL_THRESHOLD;
		const bool prefer_candidate = detail::should_prefer_data_model_candidate(current_is_stale,
		    marker_priority,
		    current_marker_priority,
		    current_is_provisional);
		return prefer_candidate;
	}

	void DataModelWatcherJob::execute_impl(const JobExecutionContext& context)
	{
		const auto job = context.job_as<RBX::ScriptContextFacets::WaitingHybridScriptsJob>();
		const auto resolution = m_job_resolutions.find(job);
		if (resolution == m_job_resolutions.end() || !resolution->second.data_model || !resolution->second.type)
		{
			return;
		}

		const auto now = std::chrono::steady_clock::now();
		auto* const new_data_model = resolution->second.data_model;
		const auto data_model_type = *resolution->second.type;
		const auto old_data_model = rml::task_scheduler().get_data_model_by_type(data_model_type);
		if (old_data_model == new_data_model)
		{
			return;
		}

		if (old_data_model)
		{
			for (auto& [cached_job, cached_resolution] : m_job_resolutions)
			{
				if (cached_resolution.data_model == old_data_model)
				{
					cached_resolution.data_model = nullptr;
					cached_resolution.type.reset();
					m_job_cadence.make_due(cached_job, now);
				}
			}
		}

		m_data_models[data_model_type] = new_data_model;
		m_data_model_last_time_stepped[data_model_type] = now;
		if (!old_data_model)
		{
			const auto absent_since = m_data_model_absent_since.find(data_model_type);
			if (m_current_candidate_sequences.find(data_model_type) == m_current_candidate_sequences.end() ||
			    (absent_since != m_data_model_absent_since.end() &&
			     now - absent_since->second > PROVISIONAL_ABSENCE_RESET))
			{
				m_provisional_window_started_at[data_model_type] = now;
			}
			m_data_model_absent_since.erase(data_model_type);
		}
		m_current_candidate_sequences[data_model_type] = resolution->second.candidate_sequence;
		m_data_model_marker_priorities[data_model_type] = resolution->second.marker_priority;
		on_data_model_changed(old_data_model, new_data_model, data_model_type, job->get_script_context());
	}

	void DataModelWatcherJob::destroy_impl() noexcept
	{
		m_job_resolutions.clear();
		m_job_cadence.clear();
		m_data_model_candidate_sequences.clear();
		m_data_models.clear();
		m_data_model_last_time_stepped.clear();
		m_provisional_window_started_at.clear();
		m_data_model_absent_since.clear();
		m_current_candidate_sequences.clear();
		m_data_model_marker_priorities.clear();
		m_next_candidate_sequence = 0;
	}

	std::uint8_t DataModelWatcherJob::studio_marker_priority(const RBX::DataModel* data_model) noexcept
	{
		constexpr std::string_view core_gui_name = "CoreGui";
		constexpr std::string_view studio_route_marker = "__CarbonStudioRoute";
		constexpr std::string_view managed_baseline_ready_marker = "__CarbonManagedBaselineReady";

		if (!data_model)
			return 0;
		const auto* services = data_model->get_children();
		if (!services)
			return 0;

		for (const auto& service_owner : *services)
		{
			auto* service = service_owner.get();
			if (!service || service->get_name() != core_gui_name)
				continue;
			const auto* children = service->get_children();
			if (!children)
				return 0;

			std::size_t route_markers = 0;
			bool baseline_ready = false;
			for (const auto& child_owner : *children)
			{
				const auto* child = child_owner.get();
				if (!child)
					continue;
				const auto name = child->get_name();
				if (name == managed_baseline_ready_marker)
					baseline_ready = true;
				route_markers += name == studio_route_marker ? 1u : 0u;
			}
			return detail::studio_marker_priority(route_markers, baseline_ready);
		}
		return 0;
	}

	void DataModelWatcherJob::on_data_model_changed(const RBX::DataModel* old_data_model, RBX::DataModel* new_data_model, const RBX::DataModelType data_model_type, RBX::ScriptContext* script_context)
	{
		if (!rml::has_task_scheduler())
		{
			LOG_ERROR("TaskScheduler is null, cannot set new DataModel.");
			return;
		}

		if (old_data_model == new_data_model)
		{
			return;
		}

		LOG_INFO("Resolved DataModel type {} from capability offset 0x{:X}",
		    static_cast<int>(data_model_type),
		    static_cast<std::uintptr_t>(get_roblox_internals_profile().datamodel().type_offset()));

		LOG_INFO("DataModel changed from 0x{:X} to 0x{:X} by {}", old_data_model ? reinterpret_cast<uintptr_t>(old_data_model) : 0, new_data_model ? reinterpret_cast<uintptr_t>(new_data_model) : 0, std::to_underlying(data_model_type));

		rml::task_scheduler().set_data_model(data_model_type, new_data_model, script_context);

		LOG_INFO("New DataModel type: {}, notifying mods and scripts", static_cast<int>(data_model_type));

		events::DataModelChangedEvent ev(reinterpret_cast<uint64_t>(old_data_model), reinterpret_cast<uint64_t>(new_data_model), static_cast<int>(data_model_type));
		events::event_manager().emit(ev);
		// Notify managed (.NET) mods about the change if the bridge is initialized
		if (rml::dotnet::g_dotnet_mod_loader)
		{
			try
			{
				rml::dotnet::g_dotnet_mod_loader->notify_data_model_changed(reinterpret_cast<uint64_t>(old_data_model), reinterpret_cast<uint64_t>(new_data_model), static_cast<int>(data_model_type));
			}
			catch (const std::exception& e)
			{
				LOG_WARN("Failed to notify managed mods of DataModel change: {}", e.what());
			}
		}

		// if (g_mod_manager)
		// {
		// 	try
		// 	{
		// 		g_mod_manager->notify_managed_datamodel_changed(old_data_model, new_data_model, data_model_type);
		// 	}
		// 	catch (const std::exception& e)
		// 	{
		// 		LOG_ERROR("Failed to notify managed mods of DataModel change: {}", e.what());
		// 	}
		// }

		// Start scripts only for a live context. Unloads retire the prior engine
		// instead of creating work against the cleared DataModel registry entry.
#if RML_ENABLE_LUAU
		try
		{
			if (new_data_model)
			{
				if (luau::g_script_manager)
				{
					luau::g_script_manager->execute_scripts_for_context(data_model_type);
				}
				LOG_INFO("Successfully triggered mod scripts for DataModel type: {}", static_cast<int>(data_model_type));
			}
			else
			{
				rml::task_scheduler().cleanup_script_engine(data_model_type);
			}
		}
		catch (const std::exception& e)
		{
			LOG_ERROR("Failed to update mod scripts for DataModel type {}: {}", static_cast<int>(data_model_type), e.what());
		}
#endif
	}

	void DataModelWatcherJob::check_and_cleanup_stale_data_models(const std::chrono::steady_clock::time_point now)
	{
		if (now < m_next_stale_cleanup)
		{
			return;
		}
		m_next_stale_cleanup = now + STALE_CLEANUP_INTERVAL;

		m_job_cadence.prune(now, JOB_CACHE_RETENTION, [this](const void* job) {
			m_job_resolutions.erase(job);
		});

		std::vector<std::pair<RBX::DataModelType, const RBX::DataModel*>> stale_data_models;
		for (auto it = m_data_model_last_time_stepped.begin(); it != m_data_model_last_time_stepped.end();)
		{
			const auto& [data_model_type, last_time] = *it;
			if (now - last_time <= STALE_DATA_MODEL_THRESHOLD)
			{
				++it;
				continue;
			}

			const auto current_data_model = rml::task_scheduler().get_data_model_by_type(data_model_type);
			const auto tracked_data_model = m_data_models.find(data_model_type);
			const auto* const tracked_data_model_ptr = tracked_data_model != m_data_models.end() ? tracked_data_model->second : nullptr;
			if (!detail::should_cleanup_stale_data_model(current_data_model, tracked_data_model_ptr))
			{
				++it;
				continue;
			}

			LOG_INFO("Detected stale DataModel type: {}, cleaning up", static_cast<int>(data_model_type));
			stale_data_models.emplace_back(data_model_type, tracked_data_model_ptr);
			m_data_models.erase(data_model_type);
			m_data_model_absent_since[data_model_type] = now;
			m_data_model_marker_priorities.erase(data_model_type);
			it = m_data_model_last_time_stepped.erase(it);
		}

		for (const auto& [stale_type, stale_data_model] : stale_data_models)
		{
			on_data_model_changed(stale_data_model, nullptr, stale_type, nullptr);
		}
	}
}
