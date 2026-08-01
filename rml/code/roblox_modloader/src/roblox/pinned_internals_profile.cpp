#include "RobloxModLoader/roblox/pinned_internals_profile.hpp"

#include "RobloxModLoader/memory/module.hpp"

#include <array>
#include <cstddef>
#include <cstdint>
#include <expected>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#if defined(RML_WINDOWS)
	#include <bcrypt.h>
#endif

namespace rml::roblox::internals
{
	namespace
	{
		constexpr std::string_view supported_studio_version = "0.732.0.7321040";
		constexpr std::uint64_t supported_studio_file_size = 219146704;
		constexpr std::array<unsigned char, 32> supported_studio_sha256{
			0xdd, 0xa0, 0x68, 0xed, 0x4a, 0xee, 0x56, 0x64,
			0xc5, 0x43, 0xbb, 0xcb, 0x75, 0x31, 0x44, 0x98,
			0xd6, 0xef, 0xc4, 0x95, 0xd7, 0x33, 0x1c, 0x57,
			0xb9, 0xc6, 0xe1, 0xf9, 0x3e, 0x09, 0x8d, 0x88,
		};
		constexpr std::uintptr_t waiting_scripts_job_data_model_accessor_rva = 0x68AB230;

#if defined(RML_WINDOWS)
		class WindowsHandle final
		{
		public:
			explicit WindowsHandle(HANDLE value) noexcept : m_value(value) {}
			~WindowsHandle()
			{
				if (m_value != INVALID_HANDLE_VALUE)
					CloseHandle(m_value);
			}

			WindowsHandle(const WindowsHandle&) = delete;
			WindowsHandle& operator=(const WindowsHandle&) = delete;

			[[nodiscard]] HANDLE get() const noexcept { return m_value; }

		private:
			HANDLE m_value{INVALID_HANDLE_VALUE};
		};

		class BCryptAlgorithm final
		{
		public:
			BCryptAlgorithm() noexcept = default;
			~BCryptAlgorithm()
			{
				if (m_value)
					BCryptCloseAlgorithmProvider(m_value, 0);
			}

			BCryptAlgorithm(const BCryptAlgorithm&) = delete;
			BCryptAlgorithm& operator=(const BCryptAlgorithm&) = delete;

			[[nodiscard]] BCRYPT_ALG_HANDLE* put() noexcept { return &m_value; }
			[[nodiscard]] BCRYPT_ALG_HANDLE get() const noexcept { return m_value; }

		private:
			BCRYPT_ALG_HANDLE m_value{};
		};

		class BCryptHash final
		{
		public:
			BCryptHash() noexcept = default;
			~BCryptHash()
			{
				if (m_value)
					BCryptDestroyHash(m_value);
			}

			BCryptHash(const BCryptHash&) = delete;
			BCryptHash& operator=(const BCryptHash&) = delete;

			[[nodiscard]] BCRYPT_HASH_HANDLE* put() noexcept { return &m_value; }
			[[nodiscard]] BCRYPT_HASH_HANDLE get() const noexcept { return m_value; }

		private:
			BCRYPT_HASH_HANDLE m_value{};
		};

		[[nodiscard]] std::expected<std::array<unsigned char, 32>, std::string> sha256_file(
			const wchar_t* path) noexcept
		{
			WindowsHandle file(CreateFileW(
				path,
				GENERIC_READ,
				FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
				nullptr,
				OPEN_EXISTING,
				FILE_ATTRIBUTE_NORMAL | FILE_FLAG_SEQUENTIAL_SCAN,
				nullptr));
			if (file.get() == INVALID_HANDLE_VALUE)
				return std::unexpected("could not open the Studio executable for ABI verification");

			LARGE_INTEGER size{};
			if (!GetFileSizeEx(file.get(), &size) || size.QuadPart < 0 ||
				static_cast<std::uint64_t>(size.QuadPart) != supported_studio_file_size)
			{
				return std::unexpected("Studio executable size does not match the pinned ABI profile");
			}

			BCryptAlgorithm algorithm;
			if (!BCRYPT_SUCCESS(BCryptOpenAlgorithmProvider(
					algorithm.put(), BCRYPT_SHA256_ALGORITHM, nullptr, 0)))
			{
				return std::unexpected("could not initialize Studio ABI verification");
			}

			ULONG object_size{};
			ULONG result_size{};
			if (!BCRYPT_SUCCESS(BCryptGetProperty(
					algorithm.get(),
					BCRYPT_OBJECT_LENGTH,
					reinterpret_cast<PUCHAR>(&object_size),
					sizeof(object_size),
					&result_size,
					0)) ||
				object_size == 0)
			{
				return std::unexpected("could not size Studio ABI verification state");
			}

			std::vector<unsigned char> hash_object(object_size);
			BCryptHash hash;
			if (!BCRYPT_SUCCESS(BCryptCreateHash(
					algorithm.get(),
					hash.put(),
					hash_object.data(),
					static_cast<ULONG>(hash_object.size()),
					nullptr,
					0,
					0)))
			{
				return std::unexpected("could not create Studio ABI verification state");
			}

			std::vector<unsigned char> buffer(1024 * 1024);
			for (;;)
			{
				DWORD bytes_read{};
				if (!ReadFile(file.get(), buffer.data(), static_cast<DWORD>(buffer.size()), &bytes_read, nullptr))
					return std::unexpected("could not read the Studio executable for ABI verification");
				if (bytes_read == 0)
					break;
				if (!BCRYPT_SUCCESS(BCryptHashData(hash.get(), buffer.data(), bytes_read, 0)))
					return std::unexpected("could not hash the Studio executable for ABI verification");
			}

			std::array<unsigned char, 32> digest{};
			if (!BCRYPT_SUCCESS(BCryptFinishHash(
					hash.get(), digest.data(), static_cast<ULONG>(digest.size()), 0)))
			{
				return std::unexpected("could not finish Studio ABI verification");
			}
			return digest;
		}
#endif

		[[nodiscard]] std::expected<void, std::string> verify_studio_identity(
			const memory::module& studio_module) noexcept
		{
#if defined(RML_WINDOWS)
			std::array<wchar_t, 32768> path{};
			const auto module = reinterpret_cast<HMODULE>(studio_module.begin().as<void*>());
			const auto path_length = GetModuleFileNameW(module, path.data(), static_cast<DWORD>(path.size()));
			if (path_length == 0 || path_length >= path.size())
				return std::unexpected("could not resolve the loaded Studio executable path");

			auto digest = sha256_file(path.data());
			if (!digest)
				return std::unexpected(std::move(digest.error()));
			if (*digest != supported_studio_sha256)
			{
				return std::unexpected(
					"Studio executable does not match pinned ABI profile " +
					std::string(supported_studio_version));
			}
			return {};
#else
			(void)studio_module;
			return std::unexpected("pinned Studio ABI verification is only available on Windows");
#endif
		}
	}

	RobloxInternalsProfile::RobloxInternalsProfile(
		ReflectionCapabilities reflection,
		DataModelCapabilities datamodel,
		InstanceCapabilities instance,
		SignalCapabilities signal,
		JobCapabilities job) noexcept :
		m_reflection(reflection),
		m_datamodel(datamodel),
		m_instance(instance),
		m_signal(signal),
		m_job(job)
	{
	}

	RobloxInternalsProfile RobloxInternalsProfile::from_pinned_profile(
		ReflectionCapabilities reflection,
		DataModelCapabilities datamodel,
		InstanceCapabilities instance,
		SignalCapabilities signal,
		JobCapabilities job) noexcept
	{
		return RobloxInternalsProfile(reflection, datamodel, instance, signal, job);
	}

	std::expected<RobloxInternalsProfile, std::string> load_pinned_internals_profile(
		const memory::module& studio_module,
		const functions::get_string_atom get_string_atom) noexcept
	{
		if (!get_string_atom)
			return std::unexpected("GET_STRING_ATOM was not resolved");
		if (auto identity = verify_studio_identity(studio_module); !identity)
			return std::unexpected(std::move(identity.error()));
		if (waiting_scripts_job_data_model_accessor_rva >= studio_module.size())
			return std::unexpected("pinned DataModel accessor RVA is outside the Studio image");

		const auto module_base = studio_module.begin().as<std::uintptr_t>();
		return RobloxInternalsProfile::from_pinned_profile(
			ReflectionCapabilities{
				get_string_atom,
				{0x40, 0x88, 0xD0, 0x118, 0x160},
				0x1B0,
				0x1BC,
				0x8,
				0x30,
				0x38,
				0x68,
				0x8C,
				0x28,
				0x30,
				0x34,
				0x35,
				0x36,
				0x48,
				0x78,
				0x80,
				0x88,
				0x48,
				0x78,
				0x78,
			},
			DataModelCapabilities(module_base, studio_module.size(), 0x908),
			InstanceCapabilities(0x68, 0x70, 0x98),
			SignalCapabilities(0x8, 0x0, 0x4, 0x10, 0x20, 0x30),
			JobCapabilities(
				0x1B0,
				0x1C8,
				reinterpret_cast<JobCapabilities::CompleteDataModelAccessor>(
					module_base + waiting_scripts_job_data_model_accessor_rva)));
	}
}
