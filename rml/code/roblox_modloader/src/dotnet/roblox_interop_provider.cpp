
#include "roblox_interop_provider.hpp"

#include "RobloxModLoader/logger/logger.hpp"
#include "RobloxModLoader/roblox/reflection/function_descriptor.hpp"
#include "RobloxModLoader/util/memory.hpp"
#include "carbon/serialized_property_access.hpp"
#include "dotnet_arguments.hpp"
#include "dotnet_event_descriptor.hpp"
#include "dotnet_variant.hpp"
#include "dotnet_yield.hpp"
#include "hierarchy_capture_fast_path.hpp"
#include "instance_child_reorder.hpp"
#include "pointers.hpp"
#include "serialized_property_event_gate.hpp"
#include "type_marshaler.hpp"

#include <RobloxModLoader/qt/mods_menu.hpp>
#include <RobloxModLoader/qt/qaction.hpp>
#include <RobloxModLoader/qt/qmenu.hpp>
#include <RobloxModLoader/qt/qmenubar.hpp>
#include <RobloxModLoader/qt/qt_integration.hpp>
#include <RobloxModLoader/roblox/data_model.hpp>
#include <RobloxModLoader/roblox/instance.hpp>
#include <RobloxModLoader/roblox/reflection/object.hpp>
#include <RobloxModLoader/roblox/reflection/property_descriptor.hpp>
#include <array>
#include <atomic>
#include <cassert>
#include <chrono>
#include <condition_variable>
#include <cstdlib>
#include <cstring>
#include <limits>
#include <memory>
#include <mutex>
#include <string>
#include <string_view>
#include <system_error>
#include <typeinfo>
#include <unordered_map>
#include <unordered_set>
#include <utility>

RML_LOG_SCOPE("Interop");

namespace rml::dotnet
{
	namespace
	{
		[[nodiscard]] rml::qt::QAction* find_studio_action(
		    rml::qt::QWidget* owner,
		    const std::string_view object_name,
		    const unsigned depth = 0)
		{
			if (!owner || depth > 16)
				return nullptr;
			for (auto* action : owner->actions())
			{
				if (!action)
					continue;
				if (action->object_name() == object_name)
					return action;
				if (auto* menu = action->menu())
				{
					if (auto* match = find_studio_action(menu, object_name, depth + 1))
						return match;
				}
			}
			return nullptr;
		}
	}

	class SerializedPropertyEventSlot final : public RBX::Reflection::GenericSlotWrapper
	{
	public:
		SerializedPropertyEventSlot(
		    const ManagedEventCallback callback,
		    void* state,
		    const RBX::Reflection::Type* property_descriptor_type) noexcept :
		    m_callback(callback),
		    m_state(state),
		    m_property_descriptor_type(property_descriptor_type)
		{
		}

		void deliver(const RBX::Reflection::EventArguments& args) override
		{
			if (!m_callback)
				return;

			try
			{
				const RBX::Reflection::PropertyDescriptor* descriptor = nullptr;
				if (!detail::visit_serialized_property_descriptor_argument(
				        args,
				        m_property_descriptor_type,
				        [&descriptor](const auto* value) { descriptor = value; }))
				{
					return;
				}

				const auto instance = TypeMarshaler::encode_variant(args[0]);
				if (instance.tag != InteropValueTag::Instance ||
				    !utils::memory::is_valid_pointer(reinterpret_cast<uintptr_t>(descriptor)))
				{
					return;
				}

				const auto* n = descriptor ? descriptor->name() : nullptr;
				if (!n) return;
				auto property_name = string_value(n->c_str());
				if (!property_name.as_string)
					return;

				const std::array interop_args{instance, property_name};
				m_callback(m_state, interop_args.data(), static_cast<uint32_t>(interop_args.size()));
				std::free(property_name.as_string);
			}
			catch (const std::exception& e)
			{
				RML_ERROR("serialized property observation dispatch failed: {}", e.what());
			}
			catch (...)
			{
				RML_ERROR("serialized property observation dispatch failed: unknown exception");
			}
		}

	private:
		ManagedEventCallback m_callback;
		void* m_state;
		const RBX::Reflection::Type* m_property_descriptor_type;
	};

	struct EngineThreadWakeState
	{
		std::mutex mutex;
		std::condition_variable idle;
		bool active{true};
		size_t in_flight{};
		ManagedEngineThreadCallback callback{};
		void* callback_state{};
		std::atomic_uint32_t manager_move_count{};
		std::atomic_bool manager_invoke_traced{};
	};

	thread_local EngineThreadWakeState* current_engine_thread_wake{};

	using PropertyDescriptorCollectionEntry =
	    rml::roblox::internals::PropertyDescriptorCollectionEntry;

	struct PropertyDescriptorSpan
	{
		const PropertyDescriptorCollectionEntry* entries{};
		size_t count{};
		const RBX::Reflection::ClassDescriptor* base{};
	};

	[[nodiscard]] bool try_read_property_descriptor_span(
	    const RBX::Reflection::ClassDescriptor* descriptor,
	    PropertyDescriptorSpan& out) noexcept
	{
		if (!descriptor)
			return false;

		const RBX::Reflection::ClassDescriptor* base{};
#if defined(_MSC_VER)
		__try
		{
			const auto base_res = descriptor->get_base();
			if (!base_res)
			{
				RML_ERROR(
				    "property descriptor span base read failed: descriptor=0x{:X}",
				    reinterpret_cast<std::uintptr_t>(descriptor));
				return false;
			}
			base = *base_res;
		}
		__except (EXCEPTION_EXECUTE_HANDLER)
		{
			return false;
		}
#else
		const auto base_res = descriptor->get_base();
		if (!base_res)
			return false;
		base = *base_res;
#endif

		const auto& reflection = get_roblox_internals_profile().reflection();
		const auto span_view = reflection.property_descriptors(descriptor);
		if (!span_view.has_value())
		{
#if defined(_MSC_VER)
			__try
			{
#endif
				std::array<std::uintptr_t, 3> collection_words{};
				std::array<std::uintptr_t, 2> base_words{};
				const auto collection_offset = reflection.descriptor_container_offsets()[0];
				const auto base_offset = reflection.base_class_offset();
				std::memcpy(
				    collection_words.data(),
				    reinterpret_cast<const std::byte*>(descriptor) + collection_offset,
				    sizeof(collection_words));
				std::memcpy(
				    base_words.data(),
				    reinterpret_cast<const std::byte*>(descriptor) + base_offset,
				    sizeof(base_words));
				RML_ERROR(
				    "property descriptor span read failed: descriptor=0x{:X}, collection=[0x{:X},0x{:X},0x{:X}], base=[0x{:X},0x{:X}]",
				    reinterpret_cast<std::uintptr_t>(descriptor),
				    collection_words[0],
				    collection_words[1],
				    collection_words[2],
				    base_words[0],
				    base_words[1]);
#if defined(_MSC_VER)
			}
			__except (EXCEPTION_EXECUTE_HANDLER)
			{
				RML_ERROR(
				    "property descriptor span diagnostics faulted: descriptor=0x{:X}",
				    reinterpret_cast<std::uintptr_t>(descriptor));
			}
#endif
			return false;
		}

		out = {span_view->entries, span_view->count, base};
		return true;
	}

	[[nodiscard]] bool try_probe_reference_descriptor(
	    const PropertyDescriptorCollectionEntry* entry,
	    const RBX::Reflection::RefPropertyDescriptor*& out_reference,
	    bool& out_xml_serializable) noexcept
	{
		out_reference = nullptr;
		out_xml_serializable = false;
#if defined(_MSC_VER)
		__try
		{
#endif
			const auto* property = entry->descriptor;
			if (!property)
				return true;
			const auto* reference = dynamic_cast<const RBX::Reflection::RefPropertyDescriptor*>(property);
			if (!reference || carbon::SerializedPropertyAccess::is_explicitly_excluded(*property))
			{
				return true;
			}
			out_reference = reference;
			// Preserve the available write signal for diagnostics and compatibility,
			// but the capture policy cannot trust it: current Studio reports every
			// probe false for persisted ObjectValue.Value.
			out_xml_serializable = property->is_xml_serializable() || property->can_xml_write();
			return true;
#if defined(_MSC_VER)
		}
		__except (EXCEPTION_EXECUTE_HANDLER)
		{
			return false;
		}
#endif
	}

	[[nodiscard]] bool try_probe_property_descriptor(
	    const PropertyDescriptorCollectionEntry* entry,
	    const RBX::Reflection::PropertyDescriptor*& out_property) noexcept
	{
		out_property = nullptr;
#if defined(_MSC_VER)
		__try
		{
#endif
			out_property = entry->descriptor;
			return true;
#if defined(_MSC_VER)
		}
		__except (EXCEPTION_EXECUTE_HANDLER)
		{
			return false;
		}
#endif
	}

	[[nodiscard]] bool try_probe_content_descriptor(
	    const PropertyDescriptorCollectionEntry* entry,
	    const RBX::Reflection::PropertyDescriptor*& out_content) noexcept
	{
		out_content = nullptr;
#if defined(_MSC_VER)
		__try
		{
#endif
			const auto* property = entry->descriptor;
			// Current Studio's XML bits are not authoritative for Content-backed
			// persisted fields (ImageContent is the concrete counterexample). Scan
			// every non-excluded Content descriptor and let capture ownership filter
			// the resulting blocker by persistent hierarchy ordinal.
			if (!property || carbon::SerializedPropertyAccess::is_explicitly_excluded(*property))
			{
				return true;
			}
			// Legacy ContentId descriptors use the "Content" tag too, but their
			// Variant storage is a string rather than the inline Content union. Only
			// the concrete Content type has the SourceType discriminator we need.
			const auto* t = property->type();
			const auto* n = t ? t->name() : nullptr;
			if (!n || n->to_string() != "Content")
				return true;
			out_content = property;
			return true;
#if defined(_MSC_VER)
		}
		__except (EXCEPTION_EXECUTE_HANDLER)
		{
			return false;
		}
#endif
	}


	[[nodiscard]] bool try_read_property_name(
	    const RBX::Reflection::PropertyDescriptor* descriptor,
	    const char*& out_data,
	    size_t& out_size) noexcept
	{
#if defined(_MSC_VER)
		__try
		{
#endif
			const auto* n = descriptor ? descriptor->name() : nullptr;
			if (!n) return false;
			const auto name = n->to_string();
			out_data = name.data();
			out_size = name.size();
			return out_size == 0 || utils::memory::is_valid_pointer(reinterpret_cast<uintptr_t>(out_data));
#if defined(_MSC_VER)
		}
		__except (EXCEPTION_EXECUTE_HANDLER)
		{
			return false;
		}
#endif
	}

	[[nodiscard]] bool try_read_reference_target(
	    const RBX::Reflection::RefPropertyDescriptor* descriptor,
	    const RBX::Instance* instance,
	    uintptr_t& out_target) noexcept
	{
#if defined(_MSC_VER)
		__try
		{
#endif
			out_target = reinterpret_cast<uintptr_t>(descriptor->get_ref_value(instance));
			return true;
#if defined(_MSC_VER)
		}
		__except (EXCEPTION_EXECUTE_HANDLER)
		{
			return false;
		}
#endif
	}

	void dispatch_engine_thread_wake(const std::shared_ptr<EngineThreadWakeState>& state) noexcept
	{
		ManagedEngineThreadCallback callback{};
		void* callback_state{};
		{
			const std::scoped_lock lock(state->mutex);
			if (!state->active || !state->callback)
				return;
			++state->in_flight;
			callback = state->callback;
			callback_state = state->callback_state;
		}

		auto* previous_wake = current_engine_thread_wake;
		current_engine_thread_wake = state.get();
		try
		{
			callback(callback_state);
		}
		catch (...)
		{
			RML_ERROR("managed engine-thread wake callback failed");
		}
		current_engine_thread_wake = previous_wake;

		{
			const std::scoped_lock lock(state->mutex);
			--state->in_flight;
			if (state->in_flight == 0)
				state->idle.notify_all();
		}
	}

	struct EngineCallableVtable
	{
		void*(__fastcall* clone)(void* source, void* destination);
		void*(__fastcall* move)(void* source, void* destination);
		void(__fastcall* invoke)(void* self, void* opaque_argument);
		const std::type_info*(__fastcall* target_type)(void* self);
		void(__fastcall* destroy)(void* self, bool deallocate);
		void*(__fastcall* target)(void* self);
	};

	struct EngineThreadWakeCallableManager
	{
		const EngineCallableVtable* vtable{};
		std::shared_ptr<EngineThreadWakeState> state;

		static void* __fastcall clone(void* source, void* destination) noexcept
		{
			const auto* manager = static_cast<const EngineThreadWakeCallableManager*>(source);
			return std::construct_at(
			    static_cast<EngineThreadWakeCallableManager*>(destination),
			    EngineThreadWakeCallableManager{manager->vtable, manager->state});
		}

		static void* __fastcall move(void* source, void* destination) noexcept
		{
			auto* manager = static_cast<EngineThreadWakeCallableManager*>(source);
			auto* moved = std::construct_at(
			    static_cast<EngineThreadWakeCallableManager*>(destination),
			    EngineThreadWakeCallableManager{manager->vtable, std::move(manager->state)});
			const auto move_count = moved->state
			    ? moved->state->manager_move_count.fetch_add(1) + 1
			    : 0;
			if (move_count != 0 && move_count <= 8)
			{
				try
				{
					RML_INFO("engine-thread pump callable move #{} entered", move_count);
				}
				catch (...)
				{
				}
			}
			return moved;
		}

		static void __fastcall invoke(void* self, void*) noexcept
		{
			auto& state = static_cast<EngineThreadWakeCallableManager*>(self)->state;
			if (state && !state->manager_invoke_traced.exchange(true))
			{
				try
				{
					RML_INFO("engine-thread pump callable invoke entered");
				}
				catch (...)
				{
				}
			}
			dispatch_engine_thread_wake(state);
		}

		static const std::type_info* __fastcall target_type(void*) noexcept
		{
			return &typeid(EngineThreadWakeCallableManager);
		}

		static void __fastcall destroy(void* self, const bool deallocate) noexcept
		{
			std::destroy_at(static_cast<EngineThreadWakeCallableManager*>(self));
			if (deallocate)
				::operator delete(self);
		}

		static void* __fastcall target(void* self) noexcept
		{
			return std::addressof(
			    static_cast<EngineThreadWakeCallableManager*>(self)->state);
		}
	};

	const EngineCallableVtable engine_thread_wake_callable_vtable{
	    &EngineThreadWakeCallableManager::clone,
	    &EngineThreadWakeCallableManager::move,
	    &EngineThreadWakeCallableManager::invoke,
	    &EngineThreadWakeCallableManager::target_type,
	    &EngineThreadWakeCallableManager::destroy,
	    &EngineThreadWakeCallableManager::target,
	};

	// Current Roblox Studio's DataModel submission API consumes this exact
	// boost::function-like small-object buffer. The manager pointer at +0x38
	// points back to the inline manager when no heap allocation is involved.
	struct alignas(8) EngineThreadWakeCallable
	{
		std::array<std::byte, 0x38> storage{};
		void* manager{};

		explicit EngineThreadWakeCallable(
		    std::shared_ptr<EngineThreadWakeState> state) noexcept
		{
			manager = std::construct_at(
			    reinterpret_cast<EngineThreadWakeCallableManager*>(storage.data()),
			    EngineThreadWakeCallableManager{
			        &engine_thread_wake_callable_vtable, std::move(state)});
		}

		~EngineThreadWakeCallable()
		{
			if (!manager)
				return;
			auto* callable_manager = static_cast<EngineThreadWakeCallableManager*>(manager);
			callable_manager->vtable->destroy(
			    manager, manager != static_cast<void*>(storage.data()));
		}

		EngineThreadWakeCallable(const EngineThreadWakeCallable&) = delete;
		EngineThreadWakeCallable& operator=(const EngineThreadWakeCallable&) = delete;
		EngineThreadWakeCallable(EngineThreadWakeCallable&&) = delete;
		EngineThreadWakeCallable& operator=(EngineThreadWakeCallable&&) = delete;
	};

	static_assert(sizeof(EngineCallableVtable) == 0x30);
	static_assert(sizeof(EngineThreadWakeCallableManager) <= 0x38);
	static_assert(alignof(EngineThreadWakeCallableManager) <= alignof(EngineThreadWakeCallable));
	static_assert(sizeof(EngineThreadWakeCallable) == 0x40);
	static_assert(offsetof(EngineThreadWakeCallable, manager) == 0x38);

	struct ManagedEngineThreadPump
	{
		RBX::DataModel* data_model_instance{};
		void* data_model_task_context{};
		std::shared_ptr<EngineThreadWakeState> state;
		std::atomic_bool submit_traced{};
	};

	void RobloxInteropProvider::verify_populated(const InteropTable& table)
	{
		const std::pair<const void*, std::string_view> members[]{
		    {reinterpret_cast<const void*>(table.reflection_invoke), "reflection_invoke"},
		    {reinterpret_cast<const void*>(table.reflection_invoke_async), "reflection_invoke_async"},
		    {reinterpret_cast<const void*>(table.reflection_get_property), "reflection_get_property"},
		    {reinterpret_cast<const void*>(table.reflection_set_property), "reflection_set_property"},
		    {reinterpret_cast<const void*>(table.reflection_event_connect), "reflection_event_connect"},
		    {reinterpret_cast<const void*>(table.reflection_event_disconnect), "reflection_event_disconnect"},
		    {reinterpret_cast<const void*>(table.object_create_by_name), "object_create_by_name"},
		    {reinterpret_cast<const void*>(table.managed_log), "managed_log"},
		    {reinterpret_cast<const void*>(table.free_string), "free_string"},
		    {reinterpret_cast<const void*>(table.free_native_ptr), "free_native_ptr"},
		    {reinterpret_cast<const void*>(table.mods_menu_add_action), "mods_menu_add_action"},
		    {reinterpret_cast<const void*>(table.mods_menu_add_submenu), "mods_menu_add_submenu"},
		    {reinterpret_cast<const void*>(table.mods_menu_add_separator), "mods_menu_add_separator"},
		    {reinterpret_cast<const void*>(table.mods_menu_add_checkable), "mods_menu_add_checkable"},
		    {reinterpret_cast<const void*>(table.mods_menu_set_item_icon), "mods_menu_set_item_icon"},
		    {reinterpret_cast<const void*>(table.mods_menu_remove), "mods_menu_remove"},
		    {reinterpret_cast<const void*>(table.reflection_event_fire), "reflection_event_fire"},
		    {reinterpret_cast<const void*>(table.reflection_event_disconnect_all), "reflection_event_disconnect_all"},
		    {reinterpret_cast<const void*>(table.reflection_event_slots), "reflection_event_slots"},
		    {reinterpret_cast<const void*>(table.event_slot_fire), "event_slot_fire"},
		    {reinterpret_cast<const void*>(table.event_slot_disconnect), "event_slot_disconnect"},
		    {reinterpret_cast<const void*>(table.event_slot_release), "event_slot_release"},
		    {reinterpret_cast<const void*>(table.serialized_property_describe), "serialized_property_describe"},
		    {reinterpret_cast<const void*>(table.serialized_property_read), "serialized_property_read"},
		    {reinterpret_cast<const void*>(table.serialized_property_write), "serialized_property_write"},
		    {reinterpret_cast<const void*>(table.serialized_property_copy), "serialized_property_copy"},
		    {reinterpret_cast<const void*>(table.serialized_property_observe), "serialized_property_observe"},
		    {reinterpret_cast<const void*>(table.serialized_property_snapshot), "serialized_property_snapshot"},
		    {reinterpret_cast<const void*>(table.engine_thread_pump_register), "engine_thread_pump_register"},
		    {reinterpret_cast<const void*>(table.engine_thread_pump_wake), "engine_thread_pump_wake"},
		    {reinterpret_cast<const void*>(table.engine_thread_pump_unregister), "engine_thread_pump_unregister"},
		    {reinterpret_cast<const void*>(table.instance_is_serializable), "instance_is_serializable"},
		    {reinterpret_cast<const void*>(table.instance_reorder_children), "instance_reorder_children"},
		    {reinterpret_cast<const void*>(table.instance_read_hierarchy), "instance_read_hierarchy"},
		    {reinterpret_cast<const void*>(table.studio_queue_local_place_save), "studio_queue_local_place_save"},
		};

		for (const auto& [pointer, name] : members)
		{
			if (!pointer)
				RML_ERROR("InteropTable::{} was never populated", name);

			assert(pointer && "InteropTable member left unpopulated after populate");
		}
	}

	[[nodiscard]] RBX::Instance* as_instance(const uintptr_t handle)
	{
		if (!utils::memory::is_valid_pointer(handle))
			return nullptr;

		auto* instance = reinterpret_cast<RBX::Instance*>(handle);
		// Managed wrappers intentionally carry raw engine handles. A removal event
		// can race a queued bridge request, leaving the allocation readable after
		// DescribedBase has cleared its descriptor during destruction. Never enter
		// Roblox's descriptor lookup with that tombstoned handle: the lookup treats
		// descriptor + 0x250 as its table and would dereference address 0x250.
		const auto raw_descriptor = instance->try_get_descriptor();
		if (!utils::memory::is_valid_pointer(reinterpret_cast<uintptr_t>(raw_descriptor)))
			return nullptr;

		return instance;
	}

	void invoke_reflection_function(RBX::Reflection::DescribedBase* instance, const RBX::Reflection::FunctionDescriptor& descriptor, const InteropVariant* args, const uint32_t arg_count, InteropVariant& out)
	{
		DotNetArguments arguments{args, arg_count};
		const auto function = RBX::Function(descriptor, instance);
		const auto ret = function.invoke(arguments);
		const auto* signature = descriptor.get_signature();
		const auto* type = signature ? signature->first_result_type() : nullptr;
		TypeMarshaler::encode_return_value(type, ret, reinterpret_cast<uintptr_t>(&arguments.return_value), out);
	}

	RBX::Reflection::EventArguments build_event_fire_args(const RBX::Reflection::EventDescriptor* descriptor, const InteropVariant* args, const uint32_t arg_count)
	{
		RBX::Reflection::EventArguments event_args;
		const auto* signature = descriptor->get_signature();
		if (!signature)
			return event_args;
		const auto sig_args = signature->arguments();

		if (sig_args.size() == 1 && sig_args[0].type && sig_args[0].type->type_id() == RBX::Reflection::TypeId::Tuple)
		{
			RBX::Reflection::Variant tuple_variant;
			if (TypeMarshaler::build_tuple_variant(args, arg_count, sig_args[0].type, tuple_variant))
				event_args.push_back(std::move(tuple_variant));
			return event_args;
		}

		const DotNetArguments arguments{args, arg_count, signature};
		event_args.reserve(arg_count);
		for (uint32_t i = 0; i < arg_count; ++i)
		{
			RBX::Reflection::Variant value;
			if (arguments.get_varint(static_cast<int>(i) + 1, value))
				event_args.push_back(std::move(value));
		}
		return event_args;
	}

	void release_fire_args(RBX::Reflection::EventArguments& event_args)
	{
		for (auto& value : event_args)
		{
			if (!value.is_void() && value.type().type_id() == RBX::Reflection::TypeId::Tuple)
				std::destroy_at(static_cast<std::shared_ptr<const RBX::Reflection::Tuple>*>(value.storage()));
		}
	}

	void RobloxInteropProvider::populate(InteropTable& table)
	{
		table.reflection_invoke = [](const uintptr_t instance_ptr, const char* function_name, const InteropVariant* args, const uint32_t arg_count, InteropVariant* out_result) {
			if (out_result)
			{
				out_result->tag = InteropValueTag::Null;
				out_result->as_uint64 = 0;
			}

			try
			{
				auto* instance = as_instance(instance_ptr);
				if (!instance || !function_name)
					return;

				const auto* class_descriptor = instance->try_get_descriptor();
				const auto* descriptor = class_descriptor->find_function(function_name);
				if (!descriptor)
					return;

				InteropVariant local_result{};
				invoke_reflection_function(instance, *descriptor, args, arg_count, out_result ? *out_result : local_result);
			}
			catch (const std::exception& e)
			{
				RML_ERROR("invoke('{}') failed: {}", function_name ? function_name : "?", e.what());
			}
			catch (...)
			{
				RML_ERROR("invoke('{}') failed: unknown exception", function_name ? function_name : "?");
			}
		};

		table.reflection_invoke_async = [](const uintptr_t instance_ptr, const char* function_name, const InteropVariant* args, const uint32_t arg_count, ManagedYieldCallback callback, void* state) {
			if (!callback)
				return;

			try
			{
				auto* instance = as_instance(instance_ptr);
				if (!instance || !function_name)
				{
					callback(state, nullptr, "invalid instance or function name");
					return;
				}

				auto& class_descriptor = instance->get_descriptor();

				if (const auto* yield_descriptor = class_descriptor.find_yield_function_descriptor(function_name))
				{
					const auto* signature = yield_descriptor->get_signature();
					if (!signature)
					{
						callback(state, nullptr, "invalid yield function signature");
						return;
					}
					DotNetArguments arguments{args, arg_count, signature};
					YieldInvocation::dispatch(*yield_descriptor, *instance, arguments, callback, state);
					return;
				}

				const auto* descriptor = class_descriptor.find_function(function_name);
				if (!descriptor)
				{
					callback(state, nullptr, "function not found");
					return;
				}

				InteropVariant result{};
				invoke_reflection_function(instance, *descriptor, args, arg_count, result);
				callback(state, &result, nullptr);
			}
			catch (const std::exception& e)
			{
				RML_ERROR("invoke_async('{}') failed: {}", function_name ? function_name : "?", e.what());
				callback(state, nullptr, e.what());
			}
			catch (...)
			{
				RML_ERROR("invoke_async('{}') failed: unknown exception", function_name ? function_name : "?");
				callback(state, nullptr, "unknown exception");
			}
		};

		table.reflection_get_property = [](const uintptr_t instance_ptr, const char* property_name, InteropVariant* out_value) {
			if (!out_value)
				return;
			*out_value = null_value();

			try
			{
				const auto* instance = as_instance(instance_ptr);
				if (!instance || !property_name)
					return;

				const auto property_descriptor = instance->get_descriptor().find_property(property_name);
				if (!property_descriptor)
					return;

				*out_value = TypeMarshaler::encode_property(property_descriptor, instance);
			}
			catch (const std::exception& e)
			{
				RML_ERROR("get_property('{}') failed: {}", property_name ? property_name : "?", e.what());
			}
			catch (...)
			{
				RML_ERROR("get_property('{}') failed: unknown exception", property_name ? property_name : "?");
			}
		};

		table.reflection_set_property = [](const uintptr_t instance_ptr, const char* property_name, const InteropVariant* value) {
			if (!value)
				return;

			try
			{
				auto* instance = as_instance(instance_ptr);
				if (!instance || !property_name)
					return;

				const auto property_descriptor = instance->get_descriptor().find_property(property_name);
				if (!property_descriptor)
					return;

				(void)TypeMarshaler::decode_property(property_descriptor, instance, *value);
			}
			catch (const std::exception& e)
			{
				RML_ERROR("set_property('{}') failed: {}", property_name ? property_name : "?", e.what());
			}
			catch (...)
			{
				RML_ERROR("set_property('{}') failed: unknown exception", property_name ? property_name : "?");
			}
		};

		table.reflection_event_connect = [](const uintptr_t instance_ptr, const char* event_name, const ManagedEventCallback callback, void* state) -> uintptr_t {
			try
			{
				if (!callback)
					return 0;

				auto* instance = as_instance(instance_ptr);
				if (!instance || !event_name)
				{
					RML_WARN(
						"event_connect('{}') rejected handle 0x{:X}; handle_valid={} raw_descriptor=0x{:X}",
						event_name ? event_name : "?",
						instance_ptr,
						utils::memory::is_valid_pointer(instance_ptr),
						utils::memory::is_valid_pointer(instance_ptr)
							? reinterpret_cast<std::uintptr_t>(
								reinterpret_cast<RBX::Instance*>(instance_ptr)->try_get_descriptor())
							: 0);
					return 0;
				}

				const auto* class_descriptor = instance->try_get_descriptor();
				const auto* event_descriptor = class_descriptor->find_event(event_name);
				if (!event_descriptor)
				{
					RML_WARN(
						"event_connect('{}') missed descriptor on class descriptor 0x{:X}",
						event_name,
						reinterpret_cast<std::uintptr_t>(class_descriptor));
					return 0;
				}

				const auto slot = std::make_shared<ManagedEventSlot>(callback, state);

				auto holder = std::make_unique<ManagedEventConnection>();
				holder->slot = slot;
				holder->connection = event_descriptor->connect(instance, slot);

				return reinterpret_cast<uintptr_t>(holder.release());
			}
			catch (const std::exception& e)
			{
				RML_ERROR("event_connect('{}') failed: {}", event_name ? event_name : "?", e.what());
				return 0;
			}
			catch (...)
			{
				RML_ERROR("event_connect('{}') failed: unknown exception", event_name ? event_name : "?");
				return 0;
			}
		};

		table.reflection_event_disconnect = [](const uintptr_t connection_handle) {
			const auto* holder = reinterpret_cast<ManagedEventConnection*>(connection_handle);
			if (!holder)
				return;

			try
			{
				holder->connection.disconnect();
			}
			catch (const std::exception& e)
			{
				RML_ERROR("event_disconnect failed: {}", e.what());
			}
			catch (...)
			{
				RML_ERROR("event_disconnect failed: unknown exception");
			}

			delete holder;
		};

		table.object_create_by_name = [](const char* class_name, const int creator_role) -> uintptr_t {
			if (!class_name)
				return 0;

			try
			{
				const auto atom = g_pointers->m_roblox_pointers.get_string_atom(class_name);

				uintptr_t out{};
				g_pointers->m_roblox_pointers.object_create_by_name(&out, 0, atom, creator_role);

				if (!out)
				{
					RML_ERROR("creator_create_by_name('{}') failed: null instance", class_name);
					return 0;
				}

				return out;
			}
			catch (const std::exception& e)
			{
				RML_ERROR("create_by_name('{}') failed: {}", class_name, e.what());
				return 0;
			}
			catch (...)
			{
				RML_ERROR("create_by_name('{}') failed: unknown exception", class_name);
				return 0;
			}
		};

		table.managed_log = [](const int32_t level, const char* utf8, const int32_t len) {
			if (!utf8 || len < 0)
				return;

			const std::string_view message{utf8, static_cast<size_t>(len)};

			switch (level)
			{
			case 0: rml_scoped_logger()->log(spdlog::level::trace, message); break;
			case 1: rml_scoped_logger()->log(spdlog::level::debug, message); break;
			case 2: rml_scoped_logger()->log(spdlog::level::info, message); break;
			case 3: rml_scoped_logger()->log(spdlog::level::warn, message); break;
			case 4: rml_scoped_logger()->log(spdlog::level::err, message); break;
			case 5: rml_scoped_logger()->log(spdlog::level::critical, message); break;
			default: rml_scoped_logger()->log(spdlog::level::info, message); break;
			}
		};

		table.free_string = [](const char* str) {
			free(const_cast<char*>(str));
		};

		table.free_native_ptr = [](const void* ptr) {
			free(const_cast<void*>(ptr));
		};

		table.mods_menu_add_action = [](const uintptr_t parent_id, const char* text, const ManagedEventCallback callback, void* state) -> uintptr_t {
			if (!text || !callback)
				return 0;

			auto* const integration = rml::qt::QtIntegration::instance();
			if (!integration)
				return 0;

			return integration->menu().add_action(parent_id, text, [callback, state] {
				callback(state, nullptr, 0);
			});
		};

		table.mods_menu_add_submenu = [](const uintptr_t parent_id, const char* text) -> uintptr_t {
			if (!text)
				return 0;

			auto* const integration = rml::qt::QtIntegration::instance();
			if (!integration)
				return 0;

			return integration->menu().add_submenu(parent_id, text);
		};

		table.mods_menu_add_separator = [](const uintptr_t parent_id) -> uintptr_t {
			auto* const integration = rml::qt::QtIntegration::instance();
			if (!integration)
				return 0;

			return integration->menu().add_separator(parent_id);
		};

		table.mods_menu_add_checkable = [](const uintptr_t parent_id, const char* text, const int initial, const ManagedEventCallback callback, void* state) -> uintptr_t {
			if (!text || !callback)
				return 0;

			auto* const integration = rml::qt::QtIntegration::instance();
			if (!integration)
				return 0;

			return integration->menu().add_checkable(parent_id, text, initial != 0, [callback, state](const bool value) {
				const InteropVariant arg = bool_value(value);
				callback(state, &arg, 1);
			});
		};

		table.mods_menu_set_item_icon = [](const uintptr_t id, const char* utf8_path) {
			if (!utf8_path)
				return;

			if (auto* const integration = rml::qt::QtIntegration::instance())
				integration->menu().set_item_icon(id, utf8_path);
		};

		table.mods_menu_remove = [](const uintptr_t id) {
			if (auto* const integration = rml::qt::QtIntegration::instance())
				integration->menu().remove(id);
		};

		table.reflection_event_fire = [](const uintptr_t instance_ptr, const char* event_name, const InteropVariant* args, const uint32_t arg_count) {
			try
			{
				auto* instance = as_instance(instance_ptr);
				if (!instance || !event_name)
					return;

				const auto* descriptor = instance->get_descriptor().find_event(event_name);
				if (!descriptor)
					return;

				auto event_args = build_event_fire_args(descriptor, args, arg_count);
				descriptor->fire_event(instance, event_args);
				release_fire_args(event_args);
			}
			catch (const std::exception& e)
			{
				RML_ERROR("event_fire('{}') failed: {}", event_name ? event_name : "?", e.what());
			}
			catch (...)
			{
				RML_ERROR("event_fire('{}') failed: unknown exception", event_name ? event_name : "?");
			}
		};

		table.reflection_event_disconnect_all = [](const uintptr_t instance_ptr, const char* event_name) {
			try
			{
				auto* instance = as_instance(instance_ptr);
				if (!instance || !event_name)
					return;

				if (const auto* descriptor = instance->get_descriptor().find_event(event_name))
					descriptor->disconnect_all(instance);
			}
			catch (const std::exception& e)
			{
				RML_ERROR("event_disconnect_all('{}') failed: {}", event_name ? event_name : "?", e.what());
			}
			catch (...)
			{
				RML_ERROR("event_disconnect_all('{}') failed: unknown exception", event_name ? event_name : "?");
			}
		};

		table.reflection_event_slots = [](const uintptr_t instance_ptr, const char* event_name, uint32_t* out_count) -> uintptr_t* {
			if (out_count)
				*out_count = 0;

			try
			{
				auto* instance = as_instance(instance_ptr);
				if (!instance || !event_name)
					return nullptr;

				const auto* descriptor = instance->get_descriptor().find_event(event_name);
				if (!descriptor)
					return nullptr;

				auto snapshot = descriptor->snapshot_connections(instance);
				if (snapshot.empty())
					return nullptr;

				auto* result = static_cast<uintptr_t*>(malloc(snapshot.size() * sizeof(uintptr_t)));
				if (!result)
					return nullptr;

				for (size_t i = 0; i < snapshot.size(); ++i)
					result[i] = reinterpret_cast<uintptr_t>(new RBX::Signals::Connection(std::move(snapshot[i])));

				if (out_count)
					*out_count = static_cast<uint32_t>(snapshot.size());
				return result;
			}
			catch (...)
			{
				return nullptr;
			}
		};

		table.event_slot_fire = [](const uintptr_t instance_ptr, const char* event_name, const uintptr_t slot_handle, const InteropVariant* args, const uint32_t arg_count) {
			try
			{
				const auto* connection = reinterpret_cast<RBX::Signals::Connection*>(slot_handle);
				if (!connection)
					return;

				const auto* slot = connection->raw_slot();
				if (!slot)
					return;

				const auto* profile = ::try_get_roblox_internals_profile();
				if (!profile)
					return;

				const auto& signal_caps = profile->signal();
				if (!signal_caps.get_source(slot))
					return;

				auto* wrapper = static_cast<RBX::Reflection::GenericSlotWrapper*>(signal_caps.get_wrapper_ptr(slot));
				if (!wrapper)
					return;

				RBX::Reflection::EventArguments event_args;
				if (const auto* instance = as_instance(instance_ptr); instance && event_name)
				{
					if (const auto* descriptor = instance->get_descriptor().find_event(event_name))
						event_args = build_event_fire_args(descriptor, args, arg_count);
				}

				wrapper->deliver(event_args);
				release_fire_args(event_args);
			}
			catch (const std::exception& e)
			{
				RML_ERROR("event_slot_fire failed: {}", e.what());
			}
			catch (...)
			{
				RML_ERROR("event_slot_fire failed: unknown exception");
			}
		};

		table.event_slot_disconnect = [](const uintptr_t slot_handle) {
			if (const auto* connection = reinterpret_cast<RBX::Signals::Connection*>(slot_handle))
				connection->disconnect();
		};

		table.event_slot_release = [](const uintptr_t slot_handle) {
			delete reinterpret_cast<RBX::Signals::Connection*>(slot_handle);
		};

		table.serialized_property_describe = [](const uintptr_t instance_ptr, const char* property_name, SerializedPropertyInfo* out_info) -> int32_t {
			if (!out_info)
				return false;
			*out_info = {};
			try
			{
				const auto* instance = as_instance(instance_ptr);
				if (!instance || !property_name)
					return false;
				const auto* descriptor = instance->get_descriptor().find_property(property_name);
				return descriptor && carbon::SerializedPropertyAccess::describe(*descriptor, *out_info);
			}
			catch (const std::exception& e)
			{
				RML_ERROR("serialized_property_describe('{}') failed: {}", property_name ? property_name : "?", e.what());
				return false;
			}
			catch (...)
			{
				RML_ERROR("serialized_property_describe('{}') failed: unknown exception", property_name ? property_name : "?");
				return false;
			}
		};

		table.serialized_property_read = [](const uintptr_t instance_ptr, const char* property_name, InteropVariant* out_value) -> int32_t {
			if (!out_value)
				return false;
			*out_value = null_value();
			try
			{
				const auto* instance = as_instance(instance_ptr);
				if (!instance || !property_name)
					return false;
				const auto* descriptor = instance->get_descriptor().find_property(property_name);
				if (!descriptor)
					return false;
				std::vector<std::byte> value;
				if (!carbon::SerializedPropertyAccess::read(*descriptor, *instance, value))
					return false;
				*out_value = bytes_value(value);
				return out_value->tag == InteropValueTag::Bytes;
			}
			catch (const std::exception& e)
			{
				RML_ERROR("serialized_property_read('{}') failed: {}", property_name ? property_name : "?", e.what());
				return false;
			}
			catch (...)
			{
				RML_ERROR("serialized_property_read('{}') failed: unknown exception", property_name ? property_name : "?");
				return false;
			}
		};

		table.serialized_property_write = [](const uintptr_t instance_ptr, const char* property_name, const InteropVariant* value) -> int32_t {
			try
			{
				auto* instance = as_instance(instance_ptr);
				if (!instance || !property_name || !value || value->tag != InteropValueTag::Bytes || !value->as_instance)
					return false;
				const auto* descriptor = instance->get_descriptor().find_property(property_name);
				const auto* bytes = reinterpret_cast<const InteropBytes*>(value->as_instance);
				if (!descriptor || !bytes || (bytes->size > 0 && !bytes->data))
					return false;
				return carbon::SerializedPropertyAccess::write(
				    *descriptor,
				    *instance,
				    {reinterpret_cast<const std::byte*>(bytes->data), static_cast<size_t>(bytes->size)});
			}
			catch (const std::exception& e)
			{
				RML_ERROR("serialized_property_write('{}') failed: {}", property_name ? property_name : "?", e.what());
				return false;
			}
			catch (...)
			{
				RML_ERROR("serialized_property_write('{}') failed: unknown exception", property_name ? property_name : "?");
				return false;
			}
		};

		table.serialized_property_copy = [](const uintptr_t source_ptr, const uintptr_t destination_ptr, const char* property_name) -> int32_t {
			try
			{
				const auto* source = as_instance(source_ptr);
				auto* destination = as_instance(destination_ptr);
				if (!source || !destination || !property_name)
					return false;
				const auto* source_descriptor = source->get_descriptor().find_property(property_name);
				const auto* destination_descriptor = destination->get_descriptor().find_property(property_name);
				if (!source_descriptor || !destination_descriptor ||
				    !carbon::SerializedPropertyAccess::is_copyable(*source_descriptor))
					return false;
				const auto* source_type = source_descriptor->type();
				const auto* dest_type = destination_descriptor->type();
				const auto* source_type_name = source_type ? source_type->name() : nullptr;
				const auto* dest_type_name = dest_type ? dest_type->name() : nullptr;
				if (!source_type_name || !dest_type_name || source_type_name->to_string() != dest_type_name->to_string())
					return false;
				return carbon::SerializedPropertyAccess::copy(
				    *source_descriptor,
				    *source,
				    *destination_descriptor,
				    *destination);
			}
			catch (const std::exception& e)
			{
				RML_ERROR("serialized_property_copy('{}') failed: {}", property_name ? property_name : "?", e.what());
				return false;
			}
			catch (...)
			{
				RML_ERROR("serialized_property_copy('{}') failed: unknown exception", property_name ? property_name : "?");
				return false;
			}
		};

		table.serialized_property_snapshot = [](const uintptr_t instance_ptr, InteropVariant* out_value) -> int32_t {
			if (!out_value)
				return 0;
			*out_value = null_value();
			try
			{
				const auto* instance = as_instance(instance_ptr);
				if (!instance)
					return 0;

				constexpr std::array<std::byte, 8> magic{
				    std::byte{'R'}, std::byte{'M'}, std::byte{'L'}, std::byte{'P'},
				    std::byte{'R'}, std::byte{'O'}, std::byte{'P'}, std::byte{'1'}};
				std::vector<std::byte> bytes(magic.begin(), magic.end());
				const auto count_offset = bytes.size();
				bytes.resize(bytes.size() + sizeof(uint32_t));
				uint32_t count{};
				std::unordered_set<std::string> captured_names;

				auto append = [&bytes](const auto& value) {
					const auto* begin = reinterpret_cast<const std::byte*>(std::addressof(value));
					bytes.insert(bytes.end(), begin, begin + sizeof(value));
				};
				auto append_text = [&bytes](const std::string_view value) {
					const auto* begin = reinterpret_cast<const std::byte*>(value.data());
					bytes.insert(bytes.end(), begin, begin + value.size());
				};

				auto* current = std::addressof(instance->get_descriptor());
				for (size_t base_depth = 0; current; ++base_depth)
				{
					if (base_depth > 128)
						return 0;
					PropertyDescriptorSpan span;
					if (!try_read_property_descriptor_span(current, span))
						return 0;
					for (size_t property_index = 0; property_index < span.count; ++property_index)
					{
						const auto* entry = span.entries + property_index;
						const RBX::Reflection::PropertyDescriptor* descriptor{};
						if (!try_probe_property_descriptor(entry, descriptor))
							return 0;
						if (!descriptor)
							continue;
						const char* property_data{};
						size_t property_size{};
						if (!try_read_property_name(descriptor, property_data, property_size))
							return 0;
						if (property_size == 0 || property_size > std::numeric_limits<uint16_t>::max())
							continue;
						std::string property_name(property_data, property_size);
						if (!captured_names.emplace(property_name).second)
							continue;

						SerializedPropertyInfo info{};
						if (!carbon::SerializedPropertyAccess::describe(*descriptor, info))
							continue;
						const std::unique_ptr<char, decltype(&std::free)> type_name_owner(info.type_name, &std::free);
						if (!info.type_name)
							continue;
						const std::string_view type_name(info.type_name);
						if (type_name.empty() || type_name.size() > std::numeric_limits<uint16_t>::max())
							continue;

						std::vector<std::byte> value;
						if ((info.flags & SerializedPropertyReference) != 0)
						{
							const RBX::Reflection::RefPropertyDescriptor* reference{};
							bool xml_serializable{};
							if (!try_probe_reference_descriptor(entry, reference, xml_serializable))
								return 0;
							if (!reference || property_name == "Parent")
								continue;
							uintptr_t target{};
							if (!try_read_reference_target(reference, instance, target))
								return 0;
							value.resize(sizeof(target));
							std::memcpy(value.data(), &target, sizeof(target));
						}
						else if ((info.flags & SerializedPropertyAccessible) == 0)
						{
							continue;
						}
						else
						{
							try
							{
								if (!carbon::SerializedPropertyAccess::read(*descriptor, *instance, value))
									continue;
							}
							catch (...)
							{
								// Enumeration has no canonical reflection-database request
								// to guarantee a readable property. Skip individual engine
								// getters; a missing requested baseline later retains the
								// service instead of guessing.
								continue;
							}
						}

						if (count == std::numeric_limits<uint32_t>::max())
							return 0;
						const auto property_length = static_cast<uint16_t>(property_name.size());
						const auto type_length = static_cast<uint16_t>(type_name.size());
						const auto value_length = static_cast<uint64_t>(value.size());
						append(property_length);
						append(type_length);
						append(info.flags);
						append(value_length);
						append_text(property_name);
						append_text(type_name);
						bytes.insert(bytes.end(), value.begin(), value.end());
						++count;
					}
					if (span.base == current)
						return 0;
					current = span.base;
				}

				std::memcpy(bytes.data() + count_offset, &count, sizeof(count));
				*out_value = bytes_value(bytes);
				return out_value->tag == InteropValueTag::Bytes ? 1 : 0;
			}
			catch (const std::exception& e)
			{
				RML_ERROR("serialized_property_snapshot failed: {}", e.what());
				return 0;
			}
			catch (...)
			{
				RML_ERROR("serialized_property_snapshot failed: unknown exception");
				return 0;
			}
		};

		table.serialized_property_observe = [](const uintptr_t instance_ptr, const ManagedEventCallback callback, void* state) -> uintptr_t {
			try
			{
				if (!callback)
					return 0;
				auto* instance = as_instance(instance_ptr);
				if (!instance)
					return 0;
				const auto* descriptor = instance->get_descriptor().find_event("ItemChanged");
				if (!descriptor)
					return 0;
				const auto* signature = descriptor->get_signature();
				const auto* property_descriptor_type = signature ? signature->argument_type(1) : nullptr;
				if (!property_descriptor_type)
					return 0;

				const auto slot = std::make_shared<SerializedPropertyEventSlot>(
				    callback,
				    state,
				    property_descriptor_type);
				auto holder = std::make_unique<ManagedEventConnection>();
				holder->slot = slot;
				holder->connection = descriptor->connect(instance, slot);
				return reinterpret_cast<uintptr_t>(holder.release());
			}
			catch (const std::exception& e)
			{
				RML_ERROR("serialized_property_observe failed: {}", e.what());
				return 0;
			}
			catch (...)
			{
				RML_ERROR("serialized_property_observe failed: unknown exception");
				return 0;
			}
		};

		table.engine_thread_pump_register = [](const uintptr_t data_model_ptr, const ManagedEngineThreadCallback callback, void* state) -> uintptr_t {
			try
			{
				auto* data_model_instance = as_instance(data_model_ptr);
				const auto* dm_descriptor_name = data_model_instance ? data_model_instance->get_descriptor().name() : nullptr;
				if (!callback || !data_model_instance ||
				    !dm_descriptor_name || dm_descriptor_name->to_string() != "DataModel" ||
				    !g_pointers || !g_pointers->m_roblox_pointers.data_model_submit_task)
				{
					return 0;
				}

				const auto task_context_result = data_model_instance->as<RBX::DataModel>()->get_task_context();
				if (!task_context_result)
				{
					const auto& err = task_context_result.error();
					RML_ERROR(
					    "engine_thread_pump_register task context resolution failed: capability={}, failure={}, matched_calls={}, decoded_candidates={}",
					    err.capability,
					    static_cast<int>(err.failure),
					    err.matched_calls,
					    err.decoded_candidates);
					return 0;
				}
				if (*task_context_result == nullptr)
				{
					RML_ERROR("engine_thread_pump_register task context resolution returned null context");
					return 0;
				}

				const auto wake_state = std::make_shared<EngineThreadWakeState>();
				wake_state->callback = callback;
				wake_state->callback_state = state;
				auto holder = std::make_unique<ManagedEngineThreadPump>();
				holder->data_model_instance = data_model_instance->as<RBX::DataModel>();
				holder->data_model_task_context = *task_context_result;
				holder->state = wake_state;
				RML_INFO(
				    "engine-thread pump registered: DataModel instance={}, task context={}",
				    static_cast<void*>(holder->data_model_instance),
				    holder->data_model_task_context);
				return reinterpret_cast<uintptr_t>(holder.release());
			}
			catch (const std::exception& e)
			{
				RML_ERROR("engine_thread_pump_register failed: {}", e.what());
				return 0;
			}
			catch (...)
			{
				RML_ERROR("engine_thread_pump_register failed: unknown exception");
				return 0;
			}
		};

		table.engine_thread_pump_wake = [](const uintptr_t pump_handle) -> int32_t {
			auto* holder = reinterpret_cast<ManagedEngineThreadPump*>(pump_handle);
			if (!holder ||
			    as_instance(reinterpret_cast<uintptr_t>(holder->data_model_instance)) != holder->data_model_instance ||
			    !utils::memory::is_valid_pointer(reinterpret_cast<uintptr_t>(holder->data_model_task_context)) ||
			    !g_pointers || !g_pointers->m_roblox_pointers.data_model_submit_task)
			{
				return 0;
			}
			const auto move_count_before = holder->state->manager_move_count.load();
			try
			{
				auto wake_state = holder->state;
				{
					const std::scoped_lock lock(wake_state->mutex);
					if (!wake_state->active)
						return 0;
				}

				EngineThreadWakeCallable callable(std::move(wake_state));
				constexpr int32_t write_marshalled_task = 1;
				const auto trace_submit = !holder->submit_traced.exchange(true);
				if (trace_submit)
					RML_INFO("engine-thread pump native submit entered");
				g_pointers->m_roblox_pointers.data_model_submit_task(
				    holder->data_model_task_context, std::addressof(callable), write_marshalled_task);
				if (trace_submit)
					RML_INFO("engine-thread pump native submit accepted");
				return 1;
			}
			catch (const std::system_error& e)
			{
				const auto move_count = holder->state->manager_move_count.load() - move_count_before;
				const auto retained_owners = holder->state.use_count();
				// DataModel submission enqueues the task before asking the global
				// scheduler to reschedule its job. The rescheduler reports EBUSY when
				// that job is already scheduled. In that case the input callable has
				// moved through the queue and the queue retains its own shared owner;
				// the wake is accepted even though the redundant reschedule failed.
				if (e.code() == std::errc::device_or_resource_busy &&
				    move_count > 2 && retained_owners > 1)
				{
					RML_INFO(
					    "engine-thread pump task retained after busy reschedule "
					    "(moves={}, owners={}); accepting queued wake",
					    move_count,
					    retained_owners);
					return 1;
				}

				RML_ERROR(
				    "engine_thread_pump_wake failed before task retention "
				    "(moves={}, owners={}): {}",
				    move_count,
				    retained_owners,
				    e.what());
				return 0;
			}
			catch (const std::exception& e)
			{
				RML_ERROR("engine_thread_pump_wake failed: {}", e.what());
				return 0;
			}
			catch (...)
			{
				RML_ERROR("engine_thread_pump_wake failed: unknown exception");
				return 0;
			}
		};

		table.engine_thread_pump_unregister = [](const uintptr_t pump_handle) -> int32_t {
			auto holder = std::unique_ptr<ManagedEngineThreadPump>(
			    reinterpret_cast<ManagedEngineThreadPump*>(pump_handle));
			if (!holder)
				return 0;
			{
				const std::scoped_lock lock(holder->state->mutex);
				holder->state->active = false;
			}
			if (current_engine_thread_wake != holder->state.get())
			{
				std::unique_lock lock(holder->state->mutex);
				holder->state->idle.wait(lock, [&holder] {
					return holder->state->in_flight == 0;
				});
			}
			return 1;
		};

		table.instance_is_serializable = [](const uintptr_t instance_ptr) -> int32_t {
			try
			{
				const auto* instance = as_instance(instance_ptr);
				if (!instance)
					return 0;
				const auto& descriptor = instance->get_descriptor();
				return descriptor.is_serializable() ? 1 : 0;
			}
			catch (...)
			{
				return 0;
			}
		};

		table.instance_reorder_children = [](const uintptr_t parent_ptr, const uintptr_t* children, const uint32_t count) -> int32_t {
			try
			{
				auto* parent = as_instance(parent_ptr);
				if (!parent || (count != 0 && !children))
					return 0;

				// Do not copy this outer shared_ptr: Roblox's embedded reference count
				// is not safe for us to retain. Its raw vector remains valid while this
				// engine-thread operation is running.
				auto* child_vector = parent->get_children();
				if (!child_vector)
					return count == 0 ? 1 : 0;
				if (child_vector->size() != count)
					return 0;

				std::vector<uintptr_t> current;
				current.reserve(child_vector->size());
				for (const auto& child_owner : *child_vector)
				{
					auto* child = child_owner.get();
					if (!child || as_instance(reinterpret_cast<uintptr_t>(child)) != child || child->get_parent() != parent)
						return 0;
					current.push_back(reinterpret_cast<uintptr_t>(child));
				}

				const auto plan = detail::plan_exact_child_reorder(
				    current,
				    std::span<const uintptr_t>{children, count});
				if (!plan)
					return 0;

				// All validation and all potentially-throwing allocations finish above.
				// shared_ptr swaps are noexcept and neither alter Parent nor refcounts.
				for (const auto [left, right] : *plan)
					std::swap((*child_vector)[left], (*child_vector)[right]);
				return 1;
			}
			catch (const std::exception& e)
			{
				RML_ERROR("instance_reorder_children failed: {}", e.what());
				return 0;
			}
			catch (...)
			{
				RML_ERROR("instance_reorder_children failed: unknown exception");
				return 0;
			}
		};

		table.instance_read_hierarchy = [](
		                                    const uintptr_t root_ptr,
		                                    const uintptr_t excluded_root_ptr,
		                                    const int32_t include_capture_metadata,
		                                    InteropVariant* out_value) -> int32_t {
			if (!out_value)
				return 0;
			*out_value = null_value();
			try
			{
				const auto started_at = std::chrono::steady_clock::now();
				auto reject = [](const std::string_view reason) -> int32_t {
					RML_ERROR("instance_read_hierarchy rejected the snapshot: {}", reason);
					return 0;
				};
				auto* root = as_instance(root_ptr);
				if (!root)
					return reject("root is unavailable");
				auto* excluded_root = excluded_root_ptr == 0
				    ? nullptr
				    : as_instance(excluded_root_ptr);
				if (excluded_root_ptr != 0 && !excluded_root)
					return reject("excluded root is unavailable");
				const auto* name_property = root->get_descriptor().find_property("Name");
				if (!name_property)
					return reject("Name is unavailable");
				const auto* archivable_property = root->get_descriptor().find_property("Archivable");
				if (!archivable_property || !archivable_property->type() ||
				    archivable_property->type()->type_id() != RBX::Reflection::TypeId::Bool)
					return reject("Archivable is unavailable");
				constexpr std::array<std::byte, 7> magic_prefix{
				    std::byte{'R'}, std::byte{'M'}, std::byte{'L'}, std::byte{'H'},
				    std::byte{'I'}, std::byte{'E'}, std::byte{'R'}};
				std::vector<std::byte> bytes;
				bytes.reserve(1024 * 1024);
				bytes.insert(bytes.end(), magic_prefix.begin(), magic_prefix.end());
				bytes.push_back(include_capture_metadata ? std::byte{'5'} : std::byte{'3'});
				const auto count_offset = bytes.size();
				bytes.resize(bytes.size() + sizeof(uint32_t));

				auto append = [&bytes](const auto& value) {
					const auto* begin = reinterpret_cast<const std::byte*>(std::addressof(value));
					bytes.insert(bytes.end(), begin, begin + sizeof(value));
				};
				auto append_text = [&bytes](const std::string_view value) {
					if (value.empty())
						return;
					const auto* begin = reinterpret_cast<const std::byte*>(value.data());
					bytes.insert(bytes.end(), begin, begin + value.size());
				};

				struct PendingNode
				{
					RBX::Instance* instance;
					uint32_t parent_index;
				};
				std::vector<PendingNode> pending;
				pending.reserve(1024);
				pending.push_back({root, std::numeric_limits<uint32_t>::max()});
				struct PendingReference
				{
					uint32_t owner_index;
					std::string property_name;
					uintptr_t target;
				};
				struct ReferenceDescriptor
				{
					const RBX::Reflection::RefPropertyDescriptor* descriptor;
					std::string property_name;
					bool xml_serializable;
				};
				struct ContentDescriptor
				{
					const RBX::Reflection::PropertyDescriptor* descriptor;
					std::string property_name;
				};
				struct CaptureDescriptorSelection
				{
					const std::vector<ReferenceDescriptor>* references;
					const std::vector<ContentDescriptor>* contents;
				};
				struct PendingContentObject
				{
					uint32_t owner_index;
					std::string property_name;
				};
				std::vector<PendingReference> references;
				std::vector<PendingContentObject> content_objects;
				std::unordered_map<
				    const RBX::Reflection::ClassDescriptor*,
				    std::vector<ReferenceDescriptor>> reference_descriptors;
				std::unordered_map<
				    const RBX::Reflection::ClassDescriptor*,
				    std::vector<ContentDescriptor>> content_descriptors;
				detail::ConsecutiveValueCache<
				    const RBX::Reflection::ClassDescriptor*,
				    CaptureDescriptorSelection> consecutive_capture_descriptors;
				uint32_t count{};
				for (size_t cursor = 0; cursor < pending.size(); ++cursor)
				{
					const auto [instance, parent_index] = pending[cursor];
					if (!instance || instance == excluded_root)
						continue;
					if (count == std::numeric_limits<uint32_t>::max())
						return reject("instance count exceeds the protocol limit");

					const auto& descriptor = instance->get_descriptor();
					const auto* class_name_ptr = descriptor.name();
					const std::string_view class_name = class_name_ptr ? class_name_ptr->to_string() : "";
					const auto reflected_name = name_property->get_string_value(instance);
					const std::string_view name = reflected_name.to_string();
					if (class_name.size() > std::numeric_limits<uint16_t>::max() ||
					    name.size() > std::numeric_limits<uint32_t>::max())
						return reject("class or name exceeds the protocol limit");

					RBX::Reflection::Variant archivable;
					archivable_property->get_variant(instance, archivable);
					if (archivable.is_void())
						return reject("Archivable value is unavailable");

					const auto handle = reinterpret_cast<uintptr_t>(instance);
					const uint8_t persistence_flags =
					    (descriptor.is_serializable() ? uint8_t{1} : uint8_t{0}) |
					    (*archivable.try_cast<bool>() ? uint8_t{2} : uint8_t{0});
					if (!detail::append_hierarchy_node_record(
					        bytes, handle, parent_index, persistence_flags, class_name, name))
						return reject("hierarchy node exceeds the snapshot allocation limit");

					const auto current_index = count++;
					if (include_capture_metadata)
					{
						const auto* selected = consecutive_capture_descriptors.find(&descriptor);
						if (!selected)
						{
							auto [reference_entry, inserted] = reference_descriptors.try_emplace(&descriptor);
							auto [content_entry, content_inserted] = content_descriptors.try_emplace(&descriptor);
							if (inserted != content_inserted)
								return reject("capture descriptor caches disagree");
							if (inserted)
							{
								auto* current = &descriptor;
								for (size_t base_depth = 0; current; ++base_depth)
								{
									if (base_depth > 128)
										return reject(std::string("reference descriptor base chain exceeds the limit for class ") +
										              std::string(class_name));
									PropertyDescriptorSpan span;
									if (!try_read_property_descriptor_span(current, span))
										return reject(std::string("reference descriptor span is unsafe for class ") +
										              std::string(class_name) + " at base depth " + std::to_string(base_depth));
									for (size_t property_index = 0; property_index < span.count; ++property_index)
									{
										const RBX::Reflection::RefPropertyDescriptor* reference{};
										bool xml_serializable{};
										if (!try_probe_reference_descriptor(
										        span.entries + property_index, reference, xml_serializable))
											return reject(std::string("reference descriptor probe faulted for class ") +
											              std::string(class_name) + " at base depth " +
											              std::to_string(base_depth) + ", property index " +
											              std::to_string(property_index));
										if (reference)
										{
											const char* property_data{};
											size_t property_size{};
											if (!try_read_property_name(reference, property_data, property_size) ||
											    property_size == 0 || property_size > std::numeric_limits<uint16_t>::max())
												return reject(std::string("reference descriptor name is unsafe for class ") +
												              std::string(class_name) + " at base depth " +
												              std::to_string(base_depth) + ", property index " +
												              std::to_string(property_index));
											std::string property_name(property_data, property_size);
											if (property_name != "Parent")
											{
												const auto duplicate = std::ranges::find_if(
												    reference_entry->second,
												    [&property_name](const auto& existing) {
													    return existing.property_name == property_name;
												    });
											if (duplicate == reference_entry->second.end())
												reference_entry->second.push_back({
												    reference, std::move(property_name), xml_serializable});
											}
										}

										const RBX::Reflection::PropertyDescriptor* content{};
										if (!try_probe_content_descriptor(span.entries + property_index, content))
											return reject(std::string("Content descriptor probe faulted for class ") +
											              std::string(class_name) + " at base depth " +
											              std::to_string(base_depth) + ", property index " +
											              std::to_string(property_index));
										if (content)
										{
											const char* property_data{};
											size_t property_size{};
											if (!try_read_property_name(content, property_data, property_size) ||
											    property_size == 0 || property_size > std::numeric_limits<uint16_t>::max())
												return reject(std::string("Content descriptor name is unsafe for class ") +
												              std::string(class_name) + " at base depth " +
												              std::to_string(base_depth) + ", property index " +
												              std::to_string(property_index));
											std::string property_name(property_data, property_size);
											const auto duplicate = std::ranges::find_if(
											    content_entry->second,
											    [&property_name](const auto& existing) {
												    return existing.property_name == property_name;
											    });
											if (duplicate == content_entry->second.end())
												content_entry->second.push_back({content, std::move(property_name)});
										}
									}
									if (span.base == current)
										return reject(std::string("reference descriptor base chain cycles for class ") +
										              std::string(class_name));
									current = span.base;
								}
							}
							selected = std::addressof(consecutive_capture_descriptors.remember(
							    &descriptor,
							    {std::addressof(reference_entry->second), std::addressof(content_entry->second)}));
						}
						for (const auto& reference : *selected->references)
						{
							// Current Studio's XML flags are not authoritative for
							// persistent service-shell references (for example,
							// Workspace.CurrentCamera). Broaden only direct DataModel
							// shells; descendants keep the bounded XML-readable set.
							if (!detail::should_capture_reference(
							        reference.xml_serializable, instance->get_parent() == root))
								continue;
							uintptr_t target{};
							if (!try_read_reference_target(reference.descriptor, instance, target))
								return reject(std::string("reference read faulted for ") + std::string(class_name) +
								              "." + reference.property_name);
								references.push_back({
								    current_index,
								    reference.property_name,
								    target});
						}
						for (const auto& content : *selected->contents)
						{
							carbon::ContentSourceType source_type{};
							if (!carbon::SerializedPropertyAccess::read_content_source_type(
							        *content.descriptor, *instance, source_type))
							{
								return reject(std::string("Content SourceType is invalid for ") +
								              std::string(class_name) + "." + content.property_name);
							}
							if (source_type == carbon::ContentSourceType::Object)
								content_objects.push_back({current_index, content.property_name});
						}
					}
					auto* child_vector = instance->get_children();
					if (!child_vector)
						continue;
					if (!detail::reserve_for_append(pending, child_vector->size()) ||
					    !detail::reserve_for_append(
					        bytes, child_vector->size(), detail::hierarchy_node_fixed_bytes))
						return reject("hierarchy child count exceeds the snapshot allocation limit");
					for (const auto& child_owner : *child_vector)
					{
						auto* child = child_owner.get();
						auto* child_parent = child ? child->get_parent() : nullptr;
						if (!child || child_parent != instance)
						{
							RML_ERROR(
							    "hierarchy child mismatch: parent=0x{:X}, child=0x{:X}, child_parent=0x{:X}",
							    reinterpret_cast<std::uintptr_t>(instance),
							    reinterpret_cast<std::uintptr_t>(child),
							    reinterpret_cast<std::uintptr_t>(child_parent));
							return reject("a child vector entry is inconsistent");
						}
						if (child != excluded_root)
							pending.push_back({child, current_index});
					}
				}

				if (count == 0)
					return reject("snapshot is empty");
				std::memcpy(bytes.data() + count_offset, &count, sizeof(count));
				if (include_capture_metadata)
				{
					if (references.size() > std::numeric_limits<uint32_t>::max())
						return reject("reference count exceeds the protocol limit");
					const auto reference_count = static_cast<uint32_t>(references.size());
					append(reference_count);
					for (const auto& reference : references)
					{
						const std::string_view property_name = reference.property_name;
						if (property_name.empty() || property_name.size() > std::numeric_limits<uint16_t>::max())
							return reject("reference property name exceeds the protocol limit");
						const auto property_length = static_cast<uint16_t>(property_name.size());
						append(reference.owner_index);
						append(reference.target);
						append(property_length);
						append_text(property_name);
					}
					if (content_objects.size() > std::numeric_limits<uint32_t>::max())
						return reject("Content.Object blocker count exceeds the protocol limit");
					const auto content_object_count = static_cast<uint32_t>(content_objects.size());
					append(content_object_count);
					for (const auto& content_object : content_objects)
					{
						const std::string_view property_name = content_object.property_name;
						if (property_name.empty() || property_name.size() > std::numeric_limits<uint16_t>::max())
							return reject("Content.Object property name exceeds the protocol limit");
						const auto property_length = static_cast<uint16_t>(property_name.size());
						append(content_object.owner_index);
						append(property_length);
						append_text(property_name);
					}
				}
				*out_value = bytes_value(bytes);
				const auto elapsed = std::chrono::duration_cast<std::chrono::milliseconds>(
				    std::chrono::steady_clock::now() - started_at);
				RML_INFO(
				    "instance_read_hierarchy captured {} nodes in {} ms",
				    count,
				    elapsed.count());
				return out_value->tag == InteropValueTag::Bytes ? 1 : 0;
			}
			catch (const std::exception& e)
			{
				RML_ERROR("instance_read_hierarchy failed: {}", e.what());
				return 0;
			}
			catch (...)
			{
				RML_ERROR("instance_read_hierarchy failed: unknown exception");
				return 0;
			}
		};

		table.studio_queue_local_place_save = []() -> int32_t {
			try
			{
				auto* integration = rml::qt::QtIntegration::instance();
				auto* menu_bar = integration ? integration->menu().system_menu_bar() : nullptr;
				auto* action = find_studio_action(menu_bar, "fileSaveAction");
				if (!action)
				{
					RML_ERROR("Studio local place save action is unavailable");
					return 0;
				}
				if (!action->queue_trigger())
				{
					RML_ERROR("Studio local place save action could not be queued");
					return 0;
				}
				RML_INFO("Queued Studio's local place save action for qualification");
				return 1;
			}
			catch (const std::exception& error)
			{
				RML_ERROR("studio_queue_local_place_save failed: {}", error.what());
				return 0;
			}
			catch (...)
			{
				RML_ERROR("studio_queue_local_place_save failed: unknown exception");
				return 0;
			}
		};

		verify_populated(table);
	}
} // namespace rml::dotnet
