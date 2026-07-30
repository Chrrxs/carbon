#pragma once

#include "RobloxModLoader/util/intrusive_weak_ptr.hpp"
#include "RobloxModLoader/util/layout_assert.hpp"

#include <cstdint>

namespace RBX::Signals
{
	struct Slot;
	struct Signal;

	class Connection
	{
	public:
		using Slot = Slot;

		Connection() noexcept = default;

		explicit Connection(Slot* slot) noexcept;
		Connection(const Connection& other) noexcept;
		Connection(Connection&& other) noexcept;
		Connection& operator=(const Connection& other) noexcept;
		Connection& operator=(Connection&& other) noexcept;
		~Connection();

		[[nodiscard]] static Connection observe(Slot* slot) noexcept;

		void disconnect() const;

		[[nodiscard]] Slot* raw_slot() const noexcept
		{
			return m_slot;
		}

		[[nodiscard]] bool connected() const;

		bool operator==(const Connection& other) const noexcept
		{
			return m_slot == other.m_slot;
		}
		bool operator!=(const Connection& other) const noexcept
		{
			return m_slot != other.m_slot;
		}

	private:
		Slot* m_slot{nullptr};
	};

	static_assert(sizeof(Connection) == sizeof(void*), "Connection must stay a single-pointer handle");
}
