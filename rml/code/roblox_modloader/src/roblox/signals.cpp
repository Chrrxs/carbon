#include "RobloxModLoader/roblox/signals.hpp"

#include "RobloxModLoader/internal/common.hpp"
#include "RobloxModLoader/roblox/internals_profile.hpp"
#include "pointers.hpp"

namespace RBX::Signals
{
	Connection::Connection(Slot* slot) noexcept :
		m_slot(slot)
	{
	}

	Connection::Connection(const Connection& other) noexcept :
		m_slot(other.m_slot)
	{
		if (m_slot)
			::get_roblox_internals_profile().signal().observe_slot(m_slot);
	}

	Connection::Connection(Connection&& other) noexcept :
		m_slot(other.m_slot)
	{
		other.m_slot = nullptr;
	}

	Connection& Connection::operator=(const Connection& other) noexcept
	{
		if (this != &other)
		{
			if (m_slot)
				::get_roblox_internals_profile().signal().release_slot(m_slot);
			m_slot = other.m_slot;
			if (m_slot)
				::get_roblox_internals_profile().signal().observe_slot(m_slot);
		}
		return *this;
	}

	Connection& Connection::operator=(Connection&& other) noexcept
	{
		if (this != &other)
		{
			if (m_slot)
				::get_roblox_internals_profile().signal().release_slot(m_slot);
			m_slot = other.m_slot;
			other.m_slot = nullptr;
		}
		return *this;
	}

	Connection::~Connection()
	{
		if (m_slot)
			::get_roblox_internals_profile().signal().release_slot(m_slot);
	}

	Connection Connection::observe(Slot* slot) noexcept
	{
		if (slot)
			::get_roblox_internals_profile().signal().observe_slot(slot);
		return Connection(slot);
	}

	bool Connection::connected() const
	{
		return m_slot && ::get_roblox_internals_profile().signal().is_connected(m_slot);
	}

	void Connection::disconnect() const
	{
		if (m_slot)
			::get_roblox_internals_profile().signal().disconnect_slot(m_slot);
	}
}
