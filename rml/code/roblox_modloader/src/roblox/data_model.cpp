#include "RobloxModLoader/roblox/data_model.hpp"

#include "RobloxModLoader/internal/common.hpp"
#include "RobloxModLoader/roblox/data_model_job.hpp"

namespace RBX
{
	namespace
	{
		// TaskSchedulerJob::data_model points at a DataModel base subobject
		// eight bytes into the owning task context. The reflection-visible
		// Instance lives at +0x1C8 from that owner.
		constexpr std::uintptr_t job_subobject_to_instance_offset = 0x1C0;
		constexpr std::uintptr_t task_context_to_instance_offset = 0x1C8;

		constexpr std::uintptr_t task_context_from_instance_address(const std::uintptr_t instance_address)
		{
			return instance_address - task_context_to_instance_offset;
		}

		constexpr std::uintptr_t instance_from_job_subobject_address(const std::uintptr_t job_subobject_address)
		{
			return job_subobject_address + job_subobject_to_instance_offset;
		}

		// Keep the two conversions asymmetric: the job holds the +0x8 base
		// subobject, while submit_task expects the owning task context.
		constexpr std::uintptr_t test_task_context_address = 0x1000;
		constexpr std::uintptr_t test_job_subobject_address = test_task_context_address + sizeof(void*);
		constexpr std::uintptr_t test_instance_address = test_task_context_address + task_context_to_instance_offset;
		static_assert(instance_from_job_subobject_address(test_job_subobject_address) == test_instance_address);
		static_assert(task_context_from_instance_address(test_instance_address) == test_task_context_address);
	}

	DataModelType DataModel::get_type() const
	{
		return m_type;
	}

	bool DataModel::is_initialized() const
	{
		return m_initialized;
	}

	void* DataModel::get_task_context() noexcept
	{
		return reinterpret_cast<void*>(task_context_from_instance_address(reinterpret_cast<std::uintptr_t>(this)));
	}

	DataModel* DataModel::from_job(const DataModelJob* job)
	{
		if (job == nullptr)
		{
			return nullptr;
		}

		const auto fake_data_model = job->data_model;
		if (fake_data_model == nullptr)
		{
			return nullptr;
		}

		const auto data_model = instance_from_job_subobject_address(reinterpret_cast<std::uintptr_t>(fake_data_model.get()));

		return reinterpret_cast<DataModel*>(data_model);
	}
}
