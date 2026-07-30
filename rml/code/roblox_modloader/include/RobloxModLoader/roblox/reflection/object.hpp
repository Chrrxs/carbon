#pragma once
#include "RobloxModLoader/roblox/security/script_permissions.hpp"
#include "RobloxModLoader/util/layout_assert.hpp"
#include "callback_descriptor.hpp"
#include "descriptor.hpp"
#include "event.hpp"
#include "event_descriptor.hpp"
#include "function_descriptor.hpp"
#include "member.hpp"
#include "pointers.hpp"
#include "RobloxModLoader/roblox/internals_profile.hpp"
#include "property_descriptor.hpp"
#include "yield_function_descriptor.hpp"

#include <mutex>
#include <string_view>
#include <unordered_map>

namespace RBX::Reflection
{
	class ClassDescriptor : public Descriptor
	{
	public:
		enum Functionality
		{
			PERSISTENT = 0x1 + 0x2 + 0x8 + 0x10,
			PERSISTENT_PLAYER = 0x1 + 0x4 + 0x8 + 0x10,
			PERSISTENT_LOCAL = 0x1 + 0x0 + 0x8 + 0x10,
			RUNTIME = 0x1 + 0x2 + 0x0 + 0x10,
			RUNTIME_PLAYER = 0x1 + 0x4 + 0x0 + 0x10,
			RUNTIME_LOCAL = 0x1 + 0x0 + 0x0 + 0x10,
			INTERNAL = 0x1 + 0x2 + 0x0 + 0x0,
			INTERNAL_PLAYER = 0x1 + 0x4 + 0x0 + 0x0,
			INTERNAL_LOCAL = 0x1 + 0x0 + 0x0 + 0x0,
			PERSISTENT_HIDDEN = 0x1 + 0x2 + 0x8 + 0x0,
			PERSISTENT_LOCAL_INTERNAL = 0x1 + 0x0 + 0x8 + 0x0,
		};

		struct Attributes : Descriptor::Attributes
		{
			Functionality flags;

			explicit Attributes(const Functionality flags) :
			    flags(flags)
			{
			}

			static Attributes deprecated(const Functionality flags)
			{
				Attributes result(flags);
				result.is_deprecated = true;
				return result;
			}
		};

		enum ReplicationLevel
		{
			NEVER_REPLICATE = 0,
			STANDARD_REPLICATE = 1,
			PLAYER_REPLICATE = 2,
		};


		[[nodiscard]] std::expected<const ClassDescriptor*, rml::roblox::internals::CompatibilityError> get_base() const noexcept
		{
			return get_roblox_internals_profile().reflection().base_class(this);
		}

		bool is_serializable() const
		{
			return get_roblox_internals_profile().reflection().is_serializable(this);
		}

		bool is_base_of(const ClassDescriptor& child) const
		{
			return get_roblox_internals_profile().reflection().is_a(&child, this);
		}


		bool is_a(const ClassDescriptor& test) const
		{
			return get_roblox_internals_profile().reflection().is_a(this, &test);
		}

		bool is_a(const char* test_name) const
		{
			return get_roblox_internals_profile().reflection().is_a(this, test_name);
		}

		const ClassDescriptor& get_descriptor() const
		{
			return *this;
		}


		PropertyDescriptor* find_property_descriptor(const char* name) const
		{
			return find_property(name);
		}

		FunctionDescriptor* find_function_descriptor(const char* name) const
		{
			return find_function(name);
		}

		YieldFunctionDescriptor* find_yield_function_descriptor(const char* name) const
		{
			return find_yield_function(name);
		}

		EventDescriptor* find_event_descriptor(const char* name) const
		{
			return find_event(name);
		}

		CallbackDescriptor* find_callback_descriptor(const char* name) const
		{
			return find_callback(name);
		}

		PropertyDescriptor* find_property_descriptor(const char* name)
		{
			return find_property(name);
		}

		FunctionDescriptor* find_function_descriptor(const char* name)
		{
			return find_function(name);
		}

		YieldFunctionDescriptor* find_yield_function_descriptor(const char* name)
		{
			return find_yield_function(name);
		}

		EventDescriptor* find_event_descriptor(const char* name)
		{
			return find_event(name);
		}

		CallbackDescriptor* find_callback_descriptor(const char* name)
		{
			return find_callback(name);
		}


		PropertyDescriptor* find_property(const char* name) const
		{
			return get_roblox_internals_profile().reflection().find_property(this, name);
		}

		FunctionDescriptor* find_function(const char* name) const
		{
			return get_roblox_internals_profile().reflection().find_function(this, name);
		}

		YieldFunctionDescriptor* find_yield_function(const char* name) const
		{
			return get_roblox_internals_profile().reflection().find_yield_function(this, name);
		}

		EventDescriptor* find_event(const char* name) const
		{
			return get_roblox_internals_profile().reflection().find_event(this, name);
		}

		CallbackDescriptor* find_callback(const char* name) const
		{
			return get_roblox_internals_profile().reflection().find_callback(this, name);
		}

		bool operator==(const ClassDescriptor& other) const
		{
			return this == &other;
		}

		bool operator!=(const ClassDescriptor& other) const
		{
			return !(*this == other);
		}


	};

	class DescribedBase : public EventSource, public std::enable_shared_from_this<DescribedBase>
	{
	protected:
		const ClassDescriptor* descriptor;
		std::unique_ptr<std::string> xml_id;

	public:
		DescribedBase()
		{
		}

		virtual ~DescribedBase()
		{
		}

		inline const ClassDescriptor& get_descriptor() const
		{
			return *descriptor;
		};

		[[nodiscard]] inline const ClassDescriptor* try_get_descriptor() const noexcept
		{
			return descriptor;
		}

		template<class T>
		inline bool is_a() const
		{
			return get_descriptor().is_a(T::get_descriptor());
		}

		template<class T>
		static inline bool is_a(const DescribedBase* instance)
		{
			return instance ? instance->get_descriptor().is_a(T::get_descriptor()) : false;
		}

		bool is_a(std::string className)
		{
			return get_descriptor().is_a(className.c_str());
		}

		template<class T>
		inline T* fast_dynamic_cast()
		{
			return (get_descriptor().is_a(T::get_descriptor())) ? static_cast<T*>(this) : NULL;
		}

		template<class T>
		inline const T* fast_dynamic_cast() const
		{
			return (get_descriptor().is_a(T::get_descriptor())) ? static_cast<const T*>(this) : NULL;
		}

		template<class T>
		static inline T* fast_dynamic_cast(DescribedBase* instance)
		{
			return (instance && instance->get_descriptor().is_a(T::get_descriptor())) ? static_cast<T*>(instance) : NULL;
		}

		template<class T>
		static inline const T* fast_dynamic_cast(const DescribedBase* instance)
		{
			return (instance && instance->get_descriptor().is_a(T::get_descriptor())) ? static_cast<const T*>(instance) : NULL;
		}

		// This function replaces shared_dynamic_cast for classes that derives from DescribedCreatable or DescribedNonCreatable.
		template<class T, class U>
		static inline std::shared_ptr<T> fast_shared_dynamic_cast(const std::shared_ptr<U>& instance)
		{
			return is_a<T>(instance.get()) ? shared_static_cast<T>(instance) : std::shared_ptr<T>();
		}

		PropertyDescriptor* find_property_descriptor(const char* name)
		{
			return get_descriptor().find_property(name);
		}

		FunctionDescriptor* find_function_descriptor(const char* name)
		{
			return get_descriptor().find_function(name);
		}

		YieldFunctionDescriptor* find_yield_function_descriptor(const char* name) const
		{
			return get_descriptor().find_yield_function(name);
		}

		CallbackDescriptor* find_callback_descriptor(const char* name)
		{
			return get_descriptor().find_callback(name);
		}

		EventDescriptor* find_signal_descriptor(const char* name) const
		{
			return get_descriptor().find_event(name);
		}

		const std::string* get_xml_id() const
		{
			return xml_id.get();
		}

		void set_xml_id(const std::string& newId)
		{
			if (!xml_id)
			{
				xml_id.reset(new std::string(newId));
			}
			else
			{
				*xml_id = newId;
			}
		}

		virtual const RBX::Name& get_class_name() const = 0;
	};
}
