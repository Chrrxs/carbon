#include "dotnet/hierarchy_capture_fast_path.hpp"

#include <cstddef>
#include <limits>
#include <string>
#include <utility>
#include <vector>

namespace
{
	using rml::dotnet::detail::ConsecutiveValueCache;
	using rml::dotnet::detail::append_hierarchy_node_record;
	using rml::dotnet::detail::geometric_append_capacity;
	using rml::dotnet::detail::should_capture_reference;
}

int main()
{
	constexpr auto maximum = std::numeric_limits<size_t>::max();
	const auto flat_pending = geometric_append_capacity(1024, 1024, maximum, 1'000'000);
	if (!flat_pending || *flat_pending != 1'001'024)
		return 1;

	const auto flat_bytes = geometric_append_capacity(
	    1024 * 1024,
	    1024 * 1024,
	    maximum,
	    1'000'000,
	    19);
	if (!flat_bytes || *flat_bytes != 20'048'576)
		return 2;

	const auto narrow_growth = geometric_append_capacity(1024, 1024, maximum, 1);
	if (!narrow_growth || *narrow_growth != 2048)
		return 3;
	if (geometric_append_capacity(10, 20, maximum, 5) != 20)
		return 4;
	if (geometric_append_capacity(maximum - 2, maximum - 1, maximum, 3))
		return 5;

	int folder_descriptor{};
	int part_descriptor{};
	using Selection = std::pair<const void*, const void*>;
	ConsecutiveValueCache<const int*, Selection> cache;
	if (cache.find(&folder_descriptor))
		return 6;

	size_t misses{};
	for (size_t index = 0; index < 1'000'000; ++index)
	{
		if (!cache.find(&folder_descriptor))
		{
			++misses;
			(void)cache.remember(&folder_descriptor, {&folder_descriptor, &folder_descriptor});
		}
	}
	if (misses != 1 || !cache.find(&folder_descriptor) || cache.find(&part_descriptor))
		return 7;

	(void)cache.remember(&part_descriptor, {&part_descriptor, &part_descriptor});
	if (cache.find(&folder_descriptor) || !cache.find(&part_descriptor))
		return 8;

	static_assert(sizeof(uintptr_t) == 8);
	std::vector<std::byte> record{std::byte{0xaa}};
	if (!append_hierarchy_node_record(
	        record,
	        uintptr_t{0x0102030405060708},
	        uint32_t{0x0a0b0c0d},
	        uint8_t{3},
	        "Folder",
	        "Node"))
	{
		return 9;
	}
	const std::vector<std::byte> expected{
	    std::byte{0xaa},
	    std::byte{0x08}, std::byte{0x07}, std::byte{0x06}, std::byte{0x05},
	    std::byte{0x04}, std::byte{0x03}, std::byte{0x02}, std::byte{0x01},
	    std::byte{0x0d}, std::byte{0x0c}, std::byte{0x0b}, std::byte{0x0a},
	    std::byte{0x03},
	    std::byte{0x06}, std::byte{0x00},
	    std::byte{0x04}, std::byte{0x00}, std::byte{0x00}, std::byte{0x00},
	    std::byte{'F'}, std::byte{'o'}, std::byte{'l'}, std::byte{'d'}, std::byte{'e'}, std::byte{'r'},
	    std::byte{'N'}, std::byte{'o'}, std::byte{'d'}, std::byte{'e'},
	};
	if (record != expected)
		return 10;
	if (append_hierarchy_node_record(record, 1, 0, 0, std::string(65'536, 'x'), ""))
		return 11;

	// Current Studio reports false XML flags even for persisted references such
	// as ObjectValue.Value, so every already-filtered descriptor is captured.
	if (!should_capture_reference(true, false))
		return 12;
	if (!should_capture_reference(false, false))
		return 13;
	if (!should_capture_reference(false, true))
		return 14;

	return 0;
}
