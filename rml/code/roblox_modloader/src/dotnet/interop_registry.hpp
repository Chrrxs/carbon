#pragma once
#include <cstddef>
#include <cstdint>

#if defined(_WIN32)
	#define RML_INTEROP_CALL __cdecl
#else
	#define RML_INTEROP_CALL
#endif

namespace rml::dotnet
{
	enum class InteropValueTag : uint8_t
	{
		Null = 0,
		Bool = 1,
		Int64 = 2,
		Double = 3,
		Float = 4,
		String = 5,
		Instance = 6,
		InstanceArray = 7,
		Blittable = 8,
		Tuple = 9,
		Bytes = 10,
	};

	struct alignas(8) InteropBytes
	{
		uint8_t* data;
		uint64_t size;
	};

	enum SerializedPropertyFlags : uint32_t
	{
		SerializedPropertyNone = 0,
		SerializedPropertyXmlRead = 1u << 0,
		SerializedPropertyXmlWrite = 1u << 1,
		SerializedPropertyScriptable = 1u << 2,
		SerializedPropertyReadOnly = 1u << 3,
		SerializedPropertyWriteOnly = 1u << 4,
		SerializedPropertyBinary = 1u << 5,
		SerializedPropertyExcluded = 1u << 6,
		SerializedPropertyAccessible = 1u << 7,
		SerializedPropertyReference = 1u << 8,
	};

	struct alignas(8) SerializedPropertyInfo
	{
		uint32_t flags;
		uint32_t reserved;
		char* type_name;
	};

	struct alignas(8) InteropVariant
	{
		InteropValueTag tag;
		union {
			bool as_bool;
			int64_t as_int64;
			uint64_t as_uint64;
			double as_double;
			float as_float;
			uintptr_t as_instance;
			char* as_string;
		};
	};

	static_assert(sizeof(InteropVariant) == 16, "InteropVariant must be 16 bytes (managed mirror is Size = 16).");
	static_assert(alignof(InteropVariant) == 8, "InteropVariant must be 8-byte aligned.");
	static_assert(offsetof(InteropVariant, tag) == 0, "InteropVariant.tag must be at offset 0.");
	static_assert(offsetof(InteropVariant, as_uint64) == 8, "InteropVariant union must start at offset 8.");

	using ManagedEventCallback = void(RML_INTEROP_CALL*)(void* state, const InteropVariant* args, uint32_t arg_count);
	using ManagedYieldCallback = void(RML_INTEROP_CALL*)(void* state, const InteropVariant* result, const char* error_message);
	using ManagedEngineThreadCallback = void(RML_INTEROP_CALL*)(void* state);

	struct alignas(8) InteropTable
	{
		uint32_t version;
		uint32_t size;

		void(RML_INTEROP_CALL* reflection_invoke)(uintptr_t instance, const char* function_name, const InteropVariant* args, uint32_t arg_count, InteropVariant* out_result);
		void(RML_INTEROP_CALL* reflection_get_property)(uintptr_t instance, const char* property_name, InteropVariant* out_value);
		void(RML_INTEROP_CALL* reflection_set_property)(uintptr_t instance, const char* property_name, const InteropVariant* value);

		uintptr_t(RML_INTEROP_CALL* reflection_event_connect)(uintptr_t instance, const char* event_name, ManagedEventCallback callback, void* state);
		void(RML_INTEROP_CALL* reflection_event_disconnect)(uintptr_t connection_handle);

		uintptr_t(RML_INTEROP_CALL* object_create_by_name)(const char* class_name, int creator_role);

		void(RML_INTEROP_CALL* managed_log)(int32_t level, const char* utf8, int32_t len);

		void(RML_INTEROP_CALL* free_string)(const char* str);
		void(RML_INTEROP_CALL* free_native_ptr)(const void* ptr);
		
		uintptr_t(RML_INTEROP_CALL* mods_menu_add_action)(uintptr_t parent_id, const char* text, ManagedEventCallback callback, void* state);
		uintptr_t(RML_INTEROP_CALL* mods_menu_add_submenu)(uintptr_t parent_id, const char* text);
		uintptr_t(RML_INTEROP_CALL* mods_menu_add_separator)(uintptr_t parent_id);
		uintptr_t(RML_INTEROP_CALL* mods_menu_add_checkable)(uintptr_t parent_id, const char* text, int initial, ManagedEventCallback callback, void* state);
		void(RML_INTEROP_CALL* mods_menu_set_item_icon)(uintptr_t id, const char* utf8_path);
		void(RML_INTEROP_CALL* mods_menu_remove)(uintptr_t id);

		void(RML_INTEROP_CALL* reflection_invoke_async)(uintptr_t instance, const char* function_name, const InteropVariant* args, uint32_t arg_count, ManagedYieldCallback callback, void* state);

		void(RML_INTEROP_CALL* reflection_event_fire)(uintptr_t instance, const char* event_name, const InteropVariant* args, uint32_t arg_count);
		void(RML_INTEROP_CALL* reflection_event_disconnect_all)(uintptr_t instance, const char* event_name);

		uintptr_t*(RML_INTEROP_CALL* reflection_event_slots)(uintptr_t instance, const char* event_name, uint32_t* out_count);
		void(RML_INTEROP_CALL* event_slot_fire)(uintptr_t instance, const char* event_name, uintptr_t slot_handle, const InteropVariant* args, uint32_t arg_count);
		void(RML_INTEROP_CALL* event_slot_disconnect)(uintptr_t slot_handle);
		void(RML_INTEROP_CALL* event_slot_release)(uintptr_t slot_handle);

		// Carbon's narrow elevated seam. These calls only accept persisted,
		// non-scriptable binary/scalar properties; Carbon's reflection database
		// supplies canonical names. Identity, capabilities and other explicitly
		// excluded engine state are rejected again in native code.
		int32_t(RML_INTEROP_CALL* serialized_property_describe)(uintptr_t instance, const char* property_name, SerializedPropertyInfo* out_info);
		int32_t(RML_INTEROP_CALL* serialized_property_read)(uintptr_t instance, const char* property_name, InteropVariant* out_value);
		int32_t(RML_INTEROP_CALL* serialized_property_write)(uintptr_t instance, const char* property_name, const InteropVariant* value);
		int32_t(RML_INTEROP_CALL* serialized_property_copy)(uintptr_t source, uintptr_t destination, const char* property_name);
		uintptr_t(RML_INTEROP_CALL* serialized_property_observe)(uintptr_t instance, ManagedEventCallback callback, void* state);
		// Captures every transport-safe serialized property on one live instance.
		// Carbon uses this once at Studio's launch boundary so singleton services
		// can be compared with the defaults of the exact running Studio build.
		int32_t(RML_INTEROP_CALL* serialized_property_snapshot)(uintptr_t instance, InteropVariant* out_value);

		// Registers a callback on the scheduler job belonging to one exact
		// DataModel. This is the supported way for managed HTTP/background work
		// to marshal Roblox reflection back onto the engine thread in edit mode.
		uintptr_t(RML_INTEROP_CALL* engine_thread_pump_register)(uintptr_t data_model, ManagedEngineThreadCallback callback, void* state);
		int32_t(RML_INTEROP_CALL* engine_thread_pump_wake)(uintptr_t pump_handle);
		int32_t(RML_INTEROP_CALL* engine_thread_pump_unregister)(uintptr_t pump_handle);

		// Mirrors the engine class descriptor's persistence bit used by native
		// place/model serialization.
		int32_t(RML_INTEROP_CALL* instance_is_serializable)(uintptr_t instance);

		// Reorders one parent's complete existing child vector in place. The
		// implementation rejects partial/duplicate/foreign permutations before
		// mutating the engine-owned shared_ptr entries and never changes Parent.
		int32_t(RML_INTEROP_CALL* instance_reorder_children)(uintptr_t parent, const uintptr_t* children, uint32_t count);

		// Reads one complete hierarchy through the engine's in-memory child vectors
		// and returns a compact parent-before-child byte stream. This avoids one
		// managed reflection crossing per instance during Carbon verification.
		int32_t(RML_INTEROP_CALL* instance_read_hierarchy)(
		    uintptr_t root,
		    uintptr_t excluded_root,
		    int32_t include_capture_metadata,
		    InteropVariant* out_value);

		// Queues Studio's fixed File > Save action on the Qt UI thread. This has
		// no path parameter: it can only serialize the already-open local document.
		int32_t(RML_INTEROP_CALL* studio_queue_local_place_save)();

	};

	inline constexpr uint32_t RML_INTEROP_VERSION = 19;

	class InteropRegistry
	{
	public:
		InteropRegistry();

		[[nodiscard]] InteropTable* table() noexcept
		{
			return &m_table;
		}
		[[nodiscard]] const InteropTable* table() const noexcept
		{
			return &m_table;
		}

	private:
		InteropTable m_table{};
	};

} // namespace rml::dotnet
