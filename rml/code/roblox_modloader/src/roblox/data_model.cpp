#include "RobloxModLoader/roblox/data_model.hpp"

#include "RobloxModLoader/internal/common.hpp"
#include "RobloxModLoader/roblox/data_model_job.hpp"
#include "RobloxModLoader/roblox/internals_profile.hpp"

namespace RBX
{
	std::expected<DataModelType, rml::roblox::internals::CompatibilityError> DataModel::get_type() const
	{
		return get_roblox_internals_profile().datamodel().resolve_type(this);
	}

	std::expected<void*, rml::roblox::internals::CompatibilityError> DataModel::get_task_context() const noexcept
	{
		const auto* profile = try_get_roblox_internals_profile();
		if (profile == nullptr)
		{
			return std::unexpected(rml::roblox::internals::CompatibilityError{
				.capability = "DataModel.RTTI",
				.failure = rml::roblox::internals::CompatibilityFailure::missing_signature,
			});
		}

		return profile->datamodel().data_model_to_task_context(this);
	}

	DataModel* DataModel::from_job(const DataModelJob* job, const rml::roblox::internals::RobloxInternalsProfile* profile)
	{
		if (job == nullptr)
		{
			return nullptr;
		}

		if (profile == nullptr)
		{
			profile = ::try_get_roblox_internals_profile();
		}
		if (profile == nullptr)
		{
			return nullptr;
		}

		const auto data_model = profile->datamodel().job_subobject_to_data_model(job);
		if (data_model)
		{
			return *data_model;
		}

		return nullptr;
	}
}
