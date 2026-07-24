#pragma once

#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <limits>
#include <memory>
#include <optional>
#include <string_view>
#include <type_traits>
#include <utility>
#include <vector>

namespace rml::dotnet::detail
{
	inline constexpr size_t hierarchy_node_fixed_bytes =
	    sizeof(uintptr_t) + sizeof(uint32_t) + sizeof(uint8_t) +
	    sizeof(uint16_t) + sizeof(uint32_t);

	[[nodiscard]] inline constexpr bool should_capture_reference(
	    const bool /*xml_serializable*/,
	    const bool /*direct_data_model_shell*/) noexcept
	{
		// Current Studio reports every XML capability bit and the virtual
		// serializer probe as false for persisted references such as
		// ObjectValue.Value. Parent and explicitly unsupported descriptors are
		// filtered before this policy; capture every remaining reference so a
		// mapping mask cannot silently erase its target.
		return true;
	}

	[[nodiscard]] inline std::optional<size_t> geometric_append_capacity(
	    const size_t size,
	    const size_t capacity,
	    const size_t max_size,
	    const size_t item_count,
	    const size_t units_per_item = 1) noexcept
	{
		if (size > max_size || capacity > max_size)
			return std::nullopt;
		if (units_per_item != 0 && item_count > (max_size - size) / units_per_item)
			return std::nullopt;

		const auto required = size + item_count * units_per_item;
		if (required <= capacity)
			return capacity;

		const auto doubled = capacity > max_size - capacity
		    ? max_size
		    : capacity * 2;
		return std::max(required, doubled);
	}

	template<typename T>
	[[nodiscard]] bool reserve_for_append(
	    std::vector<T>& values,
	    const size_t item_count,
	    const size_t units_per_item = 1)
	{
		const auto target = geometric_append_capacity(
		    values.size(),
		    values.capacity(),
		    values.max_size(),
		    item_count,
		    units_per_item);
		if (!target)
			return false;
		if (*target > values.capacity())
			values.reserve(*target);
		return true;
	}

	[[nodiscard]] inline bool append_hierarchy_node_record(
	    std::vector<std::byte>& bytes,
	    const uintptr_t handle,
	    const uint32_t parent_index,
	    const uint8_t persistence_flags,
	    const std::string_view class_name,
	    const std::string_view name)
	{
		if (class_name.size() > std::numeric_limits<uint16_t>::max() ||
		    name.size() > std::numeric_limits<uint32_t>::max())
		{
			return false;
		}
		const auto record_bytes = hierarchy_node_fixed_bytes + class_name.size() + name.size();
		if (!reserve_for_append(bytes, record_bytes))
			return false;

		const auto offset = bytes.size();
		bytes.resize(offset + record_bytes);
		auto* cursor = bytes.data() + offset;
		auto write = [&cursor](const auto& value) {
			std::memcpy(cursor, std::addressof(value), sizeof(value));
			cursor += sizeof(value);
		};
		const auto class_length = static_cast<uint16_t>(class_name.size());
		const auto name_length = static_cast<uint32_t>(name.size());
		write(handle);
		write(parent_index);
		write(persistence_flags);
		write(class_length);
		write(name_length);
		if (!class_name.empty())
		{
			std::memcpy(cursor, class_name.data(), class_name.size());
			cursor += class_name.size();
		}
		if (!name.empty())
			std::memcpy(cursor, name.data(), name.size());
		return true;
	}

	template<typename Key, typename Value>
	class ConsecutiveValueCache
	{
	public:
		[[nodiscard]] const Value* find(const Key& key) const noexcept
		{
			return m_populated && m_key == key ? std::addressof(m_value) : nullptr;
		}

		[[nodiscard]] const Value& remember(Key key, Value value) noexcept(
		    std::is_nothrow_move_assignable_v<Key> &&
		    std::is_nothrow_move_assignable_v<Value>)
		{
			m_key = std::move(key);
			m_value = std::move(value);
			m_populated = true;
			return m_value;
		}

	private:
		Key m_key{};
		Value m_value{};
		bool m_populated{};
	};
} // namespace rml::dotnet::detail
