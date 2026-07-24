#pragma once

#include "dotnet/interop_registry.hpp"

#include <cstddef>
#include <cstdint>
#include <span>
#include <vector>

namespace RBX::Reflection
{
	class DescribedBase;
	class PropertyDescriptor;
}

namespace rml::carbon
{
	enum class ContentSourceType : std::uint32_t
	{
		None = 0,
		Uri = 1,
		Object = 2,
	};

	// The only elevated reflection surface exposed for Carbon sync. It is
	// intentionally narrower than the ordinary reflection interop: callers may
	// inspect descriptors, but reads and writes are limited to persisted,
	// non-scriptable binary values and the deny-list below is enforced natively.
	class SerializedPropertyAccess final
	{
	public:
		[[nodiscard]] static bool describe(
		    const RBX::Reflection::PropertyDescriptor& descriptor,
		    dotnet::SerializedPropertyInfo& out_info);

		[[nodiscard]] static bool read(
		    const RBX::Reflection::PropertyDescriptor& descriptor,
		    const RBX::Reflection::DescribedBase& instance,
		    std::vector<std::byte>& out_value);

		[[nodiscard]] static bool write(
		    const RBX::Reflection::PropertyDescriptor& descriptor,
		    RBX::Reflection::DescribedBase& instance,
		    std::span<const std::byte> value);

		[[nodiscard]] static bool copy(
		    const RBX::Reflection::PropertyDescriptor& source_descriptor,
		    const RBX::Reflection::DescribedBase& source,
		    const RBX::Reflection::PropertyDescriptor& destination_descriptor,
		    RBX::Reflection::DescribedBase& destination);

		// Reads only Content's inline SourceType discriminator. URI/object payload
		// pointers are deliberately never followed; capture only needs to reject
		// Object before Roblox's serializer silently turns it into None.
		[[nodiscard]] static bool read_content_source_type(
		    const RBX::Reflection::PropertyDescriptor& descriptor,
		    const RBX::Reflection::DescribedBase& instance,
		    ContentSourceType& out_source_type);

		[[nodiscard]] static bool is_binary_type(const RBX::Reflection::PropertyDescriptor& descriptor) noexcept;
		[[nodiscard]] static bool is_supported_type(const RBX::Reflection::PropertyDescriptor& descriptor) noexcept;
		[[nodiscard]] static bool is_explicitly_excluded(const RBX::Reflection::PropertyDescriptor& descriptor) noexcept;
		[[nodiscard]] static bool is_accessible(const RBX::Reflection::PropertyDescriptor& descriptor) noexcept;
		[[nodiscard]] static bool is_copyable(const RBX::Reflection::PropertyDescriptor& descriptor) noexcept;
	};
}
