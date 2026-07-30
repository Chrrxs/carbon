#pragma once
// clang-format off
#include "RobloxModLoader/memory/batch.hpp"
#include "RobloxModLoader/memory/module.hpp"
#include "RobloxModLoader/util/compile_time_helpers.hpp"
#include "RobloxModLoader/internal/roblox_pointers.hpp"
#include "RobloxModLoader/roblox/internals_profile.hpp"
// clang-format on

namespace rml
{
	class Pointers
	{
	private:
		static constexpr auto get_roblox_batch();

		template<size_t N>
		void run_batch(const memory::batch<N>& batch, const memory::module& mem_region, const std::string_view batch_name)
		{
			if (!memory::batch_runner::run(batch, mem_region))
			{
				const std::string error_message =
				    std::string("Failed to find some patterns for ") + std::string(batch_name);
				throw std::runtime_error(error_message);
			}
		}

		std::unique_ptr<const roblox::internals::RobloxInternalsProfile> m_internals_profile;

	public:
		explicit Pointers();

		~Pointers();

		[[nodiscard]] const roblox::internals::RobloxInternalsProfile& internals_profile() const
		{
			if (!m_internals_profile)
				throw std::runtime_error("Roblox internals profile is not initialized");
			return *m_internals_profile;
		}
		[[nodiscard]] const roblox::internals::RobloxInternalsProfile* try_internals_profile() const noexcept
		{
			return m_internals_profile.get();
		}

	public:
		HWND m_hwnd{};

	public:
		RobloxPointers m_roblox_pointers;
	};
}

inline rml::Pointers* g_pointers{};
