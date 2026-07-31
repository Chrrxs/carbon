#pragma once
#include "RobloxModLoader/logger/logger.hpp"
#include "RobloxModLoader/roblox/reflection/event_descriptor.hpp"
#include "RobloxModLoader/roblox/signals.hpp"
#include "dotnet_variant.hpp"
#include "interop_registry.hpp"
#include "spdlog/spdlog.h"
#include "type_marshaler.hpp"


#include <cstdlib>
#include <cstdint>
#include <memory>
#include <vector>

namespace rml::dotnet
{
	class ManagedEventSlot final : public RBX::Reflection::GenericSlotWrapper
	{
	public:
		ManagedEventSlot(const ManagedEventCallback callback, void* state) noexcept :
		    m_callback(callback),
		    m_state(state)
		{
		}

		void deliver_owned(const RBX::Reflection::EventArguments& args) override
		{
			deliver_impl(args, "owned");
		}

		void deliver_view(const RBX::Reflection::EventArgumentsView& args) override
		{
			deliver_impl(args, "view");
		}

	private:

		struct EncodedArguments final
		{
			std::vector<InteropVariant> values;

			~EncodedArguments()
			{
				for (const auto& value : values)
					release_interop_value(value);
			}
		};

		static void append_encoded(std::vector<InteropVariant>& values, const InteropVariant value)
		{
			try
			{
				values.push_back(value);
			}
			catch (...)
			{
				release_interop_value(value);
				throw;
			}
		}

		template<typename Arguments>
		void deliver_impl(const Arguments& args, const char* kind)
		{
			if (!m_callback)
				return;

			try
			{
				EncodedArguments encoded;
				auto& interop_args = encoded.values;
				interop_args.reserve(args.size());

				for (const auto& arg : args)
				{
					if (!arg.is_void() && arg.type().type_id() == RBX::Reflection::TypeId::Tuple)
					{
						if (const auto* tuple = arg.try_cast<std::shared_ptr<const RBX::Reflection::Tuple>>()->get())
						{
							for (const auto& value : tuple->values)
								append_encoded(interop_args, TypeMarshaler::encode_variant(value));
						}
						continue;
					}

					append_encoded(interop_args, TypeMarshaler::encode_variant(arg));
				}

				m_callback(m_state, interop_args.data(), static_cast<uint32_t>(interop_args.size()));

			}
			catch (const std::exception& ex)
			{
				RML_ERROR_AT(
				    "ManagedEventSlot",
				    "Exception while dispatching {} {}-argument event: {}",
				    kind,
				    args.size(),
				    ex.what());
			}
			catch (...)
			{
				RML_ERROR_AT("ManagedEventSlot", "Unknown exception while dispatching {} event", kind);
			}
		}

		ManagedEventCallback m_callback;
		void* m_state;

	};

	struct ManagedEventConnection
	{
		std::shared_ptr<RBX::Reflection::GenericSlotWrapper> slot;
		RBX::Signals::Connection connection;
	};
}
