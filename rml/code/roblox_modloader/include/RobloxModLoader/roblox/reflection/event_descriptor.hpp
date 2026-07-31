#pragma once

#include "RobloxModLoader/roblox/signals.hpp"
#include "RobloxModLoader/util/layout_assert.hpp"
#include "member.hpp"
#include "type.hpp"

#include <cstddef>
#include <type_traits>
#include <vector>

namespace RBX::Reflection
{
	typedef std::vector<Variant> EventArguments;

	class EventArgumentsView final
	{
	public:
		constexpr EventArgumentsView(const Variant* data, const std::size_t size) noexcept :
		    m_data(data),
		    m_size(size)
		{
		}

		[[nodiscard]] constexpr const Variant* begin() const noexcept
		{
			return m_data;
		}
		[[nodiscard]] constexpr const Variant* end() const noexcept
		{
			return m_size == 0 ? m_data : m_data + m_size;
		}
		[[nodiscard]] constexpr std::size_t size() const noexcept
		{
			return m_size;
		}
		[[nodiscard]] constexpr const Variant& operator[](const std::size_t index) const noexcept
		{
			return m_data[index];
		}

	private:
		const Variant* m_data;
		std::size_t m_size;
	};

	static_assert(sizeof(EventArgumentsView) == sizeof(void*) * 2);
	static_assert(std::is_trivially_copyable_v<EventArgumentsView>);

	class GenericSlotWrapper
	{
	public:
		virtual ~GenericSlotWrapper() = default;
		virtual bool use_submit_task()
		{
			return false;
		}
		virtual void deliver_owned(const EventArguments& args) = 0;
		virtual void deliver_view(const EventArgumentsView& args) = 0;
		virtual EventArguments* build_args(EventArguments& out)
		{
			return ::new (&out) EventArguments();
		}
	};

	class Event;
	class EventSource;

	class EventDescriptor : public MemberDescriptor
	{
	public:
		typedef Event ConstMember;
		typedef Event Member;

		bool operator==(const EventDescriptor& other) const
		{
			return this == &other;
		}
		bool operator!=(const EventDescriptor& other) const
		{
			return this != &other;
		}

	public:
		[[nodiscard]] const SignatureDescriptor* get_signature() const noexcept;

		virtual Signals::Connection connect(EventSource* source, std::shared_ptr<GenericSlotWrapper> wrapper) const = 0;

		[[nodiscard]] virtual bool is_scriptable() const
		{
			return true;
		}
		[[nodiscard]] bool is_public() const
		{
			return is_scriptable();
		}

		[[nodiscard]] virtual bool isBroadcast() const
		{
			return false;
		}
		[[nodiscard]] virtual bool unk_0x40() const
		{
			return false;
		}
		[[nodiscard]] virtual bool unk_0x80() const
		{
			return false;
		}
		virtual void fire_event(EventSource* source, const EventArguments& args) const = 0;
		virtual void send_event(EventSource* source, const EventArguments& args) const;
		virtual void disconnect_all(EventSource* source) const = 0;

		[[nodiscard]] Signals::Signal* get_signal(EventSource* source) const;
		[[nodiscard]] std::vector<Signals::Connection> snapshot_connections(EventSource* source) const;

	};
	
	class EventDesc : public EventDescriptor
	{
	};
}