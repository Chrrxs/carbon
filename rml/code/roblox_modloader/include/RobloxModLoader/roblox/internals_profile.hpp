#pragma once

#include "RobloxModLoader/internal/function_types.hpp"
#include "RobloxModLoader/roblox/reflection/runtime_layout_resolver.hpp"
#include "RobloxModLoader/roblox/security/script_permissions.hpp"
#include "RobloxModLoader/roblox/util/name.hpp"
#include "RobloxModLoader/rml_export.hpp"

#include <array>
#include <cstddef>
#include <cstdint>
#include <expected>
#include <optional>
#include <memory>
#include <string_view>
#include <vector>

namespace RBX
{
	class Instance;
	class DataModel;
	class ScriptContext;
	enum class DataModelType : std::int32_t;
}

namespace RBX::Signals
{
	struct Slot;
	struct Signal;
	class Connection;
}

namespace RBX::ScriptContextFacets
{
	class WaitingHybridScriptsJob;
}

namespace rml::memory
{
	class module;
}

namespace RBX::Reflection
{
	class CallbackDescriptor;
	class Descriptor;
	class MemberDescriptor;
	class SignatureDescriptor;
	class Type;
	class ClassDescriptor;
	class EventDescriptor;
	class FunctionDescriptor;
	class PropertyDescriptor;
	class YieldFunctionDescriptor;
}

namespace rml::roblox::internals
{

	struct PropertyDescriptorCollectionEntry
	{
		const RBX::Reflection::PropertyDescriptor* descriptor;
		std::uint64_t unk;
	};

	struct PropertyDescriptorSpanView
	{
		const PropertyDescriptorCollectionEntry* entries{nullptr};
		std::size_t count{0};

		[[nodiscard]] const PropertyDescriptorCollectionEntry* begin() const noexcept { return entries; }
		[[nodiscard]] const PropertyDescriptorCollectionEntry* end() const noexcept { return entries ? entries + count : nullptr; }
		[[nodiscard]] std::size_t size() const noexcept { return count; }
		[[nodiscard]] bool empty() const noexcept { return count == 0; }
	};

	class RML_EXPORT DataModelCapabilities final
	{
	public:
		DataModelCapabilities() noexcept = default;
		explicit DataModelCapabilities(
			std::uintptr_t module_base,
			std::size_t module_size,
			std::ptrdiff_t type_offset) noexcept;

		[[nodiscard]] std::expected<RBX::DataModel*, CompatibilityError> job_subobject_to_data_model(
			const void* job_subobject) const noexcept;

		[[nodiscard]] std::expected<void*, CompatibilityError> data_model_to_task_context(
			const RBX::DataModel* instance) const noexcept;

		[[nodiscard]] std::expected<RBX::DataModelType, CompatibilityError> resolve_type(
			const RBX::DataModel* instance) const noexcept;

		[[nodiscard]] std::uintptr_t module_base() const noexcept
		{
			return m_module_base;
		}

		[[nodiscard]] std::size_t module_size() const noexcept
		{
			return m_module_size;
		}

		[[nodiscard]] std::ptrdiff_t type_offset() const noexcept
		{
			return m_type_offset;
		}

	private:
		friend class RobloxInternalsProfile;

		std::uintptr_t m_module_base{0};
		std::size_t m_module_size{0};
		std::ptrdiff_t m_type_offset{0};
	};

	class RML_EXPORT ReflectionCapabilities final
	{
	public:
		[[nodiscard]] const RBX::Name* descriptor_name(
			const RBX::Reflection::Descriptor* descriptor) const noexcept;

		[[nodiscard]] std::expected<const RBX::Reflection::ClassDescriptor*, CompatibilityError> base_class(
			const RBX::Reflection::ClassDescriptor* descriptor) const noexcept;

		[[nodiscard]] bool is_a(
			const RBX::Reflection::ClassDescriptor* descriptor,
			const RBX::Reflection::ClassDescriptor* target) const noexcept;

		[[nodiscard]] bool is_a(
			const RBX::Reflection::ClassDescriptor* descriptor,
			const char* target_name) const noexcept;

		[[nodiscard]] bool is_serializable(
			const RBX::Reflection::ClassDescriptor* descriptor) const noexcept;

		[[nodiscard]] const RBX::Reflection::ClassDescriptor* member_owner(
			const RBX::Reflection::MemberDescriptor* member) const noexcept;

		[[nodiscard]] RBX::Security::Permissions member_security(
			const RBX::Reflection::MemberDescriptor* member) const noexcept;

		[[nodiscard]] const RBX::Reflection::Type* property_type(
			const RBX::Reflection::PropertyDescriptor* property) const noexcept;
		[[nodiscard]] bool property_always_clone(const RBX::Reflection::PropertyDescriptor* property) const noexcept;

		[[nodiscard]] std::uint32_t property_functionality(
			const RBX::Reflection::PropertyDescriptor* property) const noexcept;

		[[nodiscard]] bool property_is_public(const RBX::Reflection::PropertyDescriptor* property) const noexcept;
		[[nodiscard]] bool property_is_editable(const RBX::Reflection::PropertyDescriptor* property) const noexcept;
		[[nodiscard]] bool property_can_replicate(const RBX::Reflection::PropertyDescriptor* property) const noexcept;
		[[nodiscard]] bool property_can_xml_read(const RBX::Reflection::PropertyDescriptor* property) const noexcept;
		[[nodiscard]] bool property_can_xml_write(const RBX::Reflection::PropertyDescriptor* property) const noexcept;
		[[nodiscard]] bool property_is_scriptable(const RBX::Reflection::PropertyDescriptor* property) const noexcept;
		[[nodiscard]] const RBX::Reflection::SignatureDescriptor* function_signature(
			const RBX::Reflection::FunctionDescriptor* function) const noexcept;
		[[nodiscard]] std::uint32_t function_kind(
			const RBX::Reflection::FunctionDescriptor* function) const noexcept;
		[[nodiscard]] void* function_invoke_func_ptr(
			const RBX::Reflection::FunctionDescriptor* function) const noexcept;
		[[nodiscard]] std::intptr_t function_bound_this_delta(
			const RBX::Reflection::FunctionDescriptor* function) const noexcept;
		[[nodiscard]] const RBX::Reflection::SignatureDescriptor* yield_signature(
			const RBX::Reflection::YieldFunctionDescriptor* yield_func) const noexcept;
		[[nodiscard]] const RBX::Reflection::SignatureDescriptor* callback_signature(
			const RBX::Reflection::CallbackDescriptor* callback) const noexcept;
		[[nodiscard]] bool callback_is_async(
			const RBX::Reflection::CallbackDescriptor* callback) const noexcept;
		[[nodiscard]] const RBX::Reflection::SignatureDescriptor* event_signature(
			const RBX::Reflection::EventDescriptor* event) const noexcept;

		[[nodiscard]] RBX::Signals::Signal* event_signal(
			const RBX::Reflection::EventDescriptor* event,
			const void* event_source) const noexcept;

		[[nodiscard]] RBX::Reflection::PropertyDescriptor* find_property(
			const RBX::Reflection::ClassDescriptor* descriptor,
			const char* name) const noexcept;
		[[nodiscard]] RBX::Reflection::EventDescriptor* find_event(
			const RBX::Reflection::ClassDescriptor* descriptor,
			const char* name) const noexcept;
		[[nodiscard]] RBX::Reflection::FunctionDescriptor* find_function(
			const RBX::Reflection::ClassDescriptor* descriptor,
			const char* name) const noexcept;
		[[nodiscard]] RBX::Reflection::YieldFunctionDescriptor* find_yield_function(
			const RBX::Reflection::ClassDescriptor* descriptor,
			const char* name) const noexcept;
		[[nodiscard]] RBX::Reflection::CallbackDescriptor* find_callback(
			const RBX::Reflection::ClassDescriptor* descriptor,
			const char* name) const noexcept;
		[[nodiscard]] std::optional<PropertyDescriptorSpanView> property_descriptors(
			const RBX::Reflection::ClassDescriptor* descriptor) const noexcept;
		[[nodiscard]] std::array<std::ptrdiff_t, 5> descriptor_container_offsets() const noexcept
		{
			return m_descriptor_container_offsets;
		}
		[[nodiscard]] std::ptrdiff_t base_class_offset() const noexcept
		{
			return m_base_class_offset;
		}
		[[nodiscard]] std::ptrdiff_t functionality_offset() const noexcept
		{
			return m_functionality_offset;
		}

	public:
		ReflectionCapabilities() = default;

		ReflectionCapabilities(
			functions::get_string_atom get_string_atom,
			std::array<std::ptrdiff_t, 5> descriptor_container_offsets,
			std::ptrdiff_t base_class_offset,
			std::ptrdiff_t functionality_offset,
			std::ptrdiff_t name_offset,
			std::ptrdiff_t owner_offset,
			std::ptrdiff_t security_offset,
			std::ptrdiff_t property_type_offset,
			std::ptrdiff_t property_functionality_offset,
			std::ptrdiff_t signature_offset,
			std::ptrdiff_t function_kind_offset,
			std::ptrdiff_t function_invoke_func_ptr_offset,
			std::ptrdiff_t function_bound_this_delta_offset,
			std::ptrdiff_t callback_signature_offset,
			std::ptrdiff_t callback_async_flag_offset,
			std::ptrdiff_t event_signal_offset) noexcept;
	private:
		friend class RobloxInternalsProfile;
		[[nodiscard]] void* find_member_in_family(
			const RBX::Reflection::ClassDescriptor* descriptor,
			const char* name,
			std::size_t family_index) const noexcept;

		functions::get_string_atom m_get_string_atom{};
		std::array<std::ptrdiff_t, 5> m_descriptor_container_offsets{};
		std::ptrdiff_t m_base_class_offset{};
		std::ptrdiff_t m_functionality_offset{};
		std::ptrdiff_t m_name_offset{-1};
		std::ptrdiff_t m_owner_offset{-1};
		std::ptrdiff_t m_security_offset{-1};
		std::ptrdiff_t m_property_type_offset{-1};
		std::ptrdiff_t m_property_functionality_offset{-1};
		std::ptrdiff_t m_signature_offset{-1};
		std::ptrdiff_t m_function_kind_offset{-1};
		std::ptrdiff_t m_function_invoke_func_ptr_offset{-1};
		std::ptrdiff_t m_function_bound_this_delta_offset{-1};
		std::ptrdiff_t m_callback_signature_offset{-1};
		std::ptrdiff_t m_callback_async_flag_offset{-1};
		std::ptrdiff_t m_event_signal_offset{-1};
	};

	class RML_EXPORT InstanceCapabilities final
	{
	public:
		InstanceCapabilities() noexcept = default;
		explicit InstanceCapabilities(
			std::ptrdiff_t parent_offset,
			std::ptrdiff_t children_offset,
			std::ptrdiff_t name_offset) noexcept;

		[[nodiscard]] RBX::Instance* parent(const RBX::Instance* instance) const noexcept;
		[[nodiscard]] std::vector<std::shared_ptr<RBX::Instance>>* children(
			const RBX::Instance* instance) const noexcept;
		[[nodiscard]] std::string_view name(const RBX::Instance* instance) const noexcept;

	private:
		friend class RobloxInternalsProfile;

		std::ptrdiff_t m_parent_offset{-1};
		std::ptrdiff_t m_children_offset{-1};
		std::ptrdiff_t m_name_offset{-1};
	};

	class RML_EXPORT SignalCapabilities final
	{
	public:
		SignalCapabilities() noexcept = default;
		explicit SignalCapabilities(
			std::ptrdiff_t signal_head_offset,
			std::ptrdiff_t slot_strong_offset,
			std::ptrdiff_t slot_weak_offset,
			std::ptrdiff_t slot_next_offset,
			std::ptrdiff_t slot_source_offset,
			std::ptrdiff_t slot_wrapper_ptr_offset) noexcept;

		[[nodiscard]] RBX::Signals::Signal* get_signal(
			const RBX::Reflection::EventDescriptor* descriptor,
			const void* event_source) const noexcept;

		[[nodiscard]] RBX::Signals::Slot* get_head(const RBX::Signals::Signal* signal) const noexcept;
		[[nodiscard]] RBX::Signals::Slot* get_next(const RBX::Signals::Slot* slot) const noexcept;
		[[nodiscard]] void* get_source(const RBX::Signals::Slot* slot) const noexcept;
		[[nodiscard]] void* get_wrapper_ptr(const RBX::Signals::Slot* slot) const noexcept;
		[[nodiscard]] bool is_connected(const RBX::Signals::Slot* slot) const noexcept;

		void observe_slot(RBX::Signals::Slot* slot) const noexcept;
		void release_slot(RBX::Signals::Slot* slot) const noexcept;
		void disconnect_slot(RBX::Signals::Slot* slot) const noexcept;

		[[nodiscard]] std::vector<RBX::Signals::Connection> snapshot_connections(
			const RBX::Reflection::EventDescriptor* descriptor,
			void* event_source) const noexcept;


	private:
		friend class RobloxInternalsProfile;

		std::ptrdiff_t m_signal_head_offset{-1};
		std::ptrdiff_t m_slot_strong_offset{-1};
		std::ptrdiff_t m_slot_weak_offset{-1};
		std::ptrdiff_t m_slot_next_offset{-1};
		std::ptrdiff_t m_slot_source_offset{-1};
		std::ptrdiff_t m_slot_wrapper_ptr_offset{-1};
	};

	class RML_EXPORT JobCapabilities final
	{
	public:
		using CompleteDataModelAccessor = void* (*)(RBX::ScriptContext*);

		JobCapabilities() noexcept = default;
		JobCapabilities(
			std::ptrdiff_t waiting_scripts_job_script_context_offset,
			std::ptrdiff_t datamodel_instance_base_offset,
			CompleteDataModelAccessor waiting_scripts_job_data_model_accessor) noexcept;

		[[nodiscard]] RBX::ScriptContext* get_script_context(
			const RBX::ScriptContextFacets::WaitingHybridScriptsJob* job) const noexcept;
		[[nodiscard]] RBX::DataModel* get_data_model(
			const RBX::ScriptContextFacets::WaitingHybridScriptsJob* job) const noexcept;


	private:
		friend class RobloxInternalsProfile;

		std::ptrdiff_t m_waiting_scripts_job_script_context_offset{-1};
		std::ptrdiff_t m_datamodel_instance_base_offset{-1};
		CompleteDataModelAccessor m_waiting_scripts_job_data_model_accessor{};
	};

	class RML_EXPORT RobloxInternalsProfile final
	{
	public:
		[[nodiscard]] static std::expected<RobloxInternalsProfile, CompatibilityError> resolve_bootstrap(
			const memory::module& studio_module,
			functions::get_string_atom get_string_atom) noexcept;

		[[nodiscard]] const ReflectionCapabilities& reflection() const noexcept
		{
			return m_reflection;
		}

		[[nodiscard]] const DataModelCapabilities& datamodel() const noexcept
		{
			return m_datamodel;
		}

		[[nodiscard]] const InstanceCapabilities& instance() const noexcept
		{
			return m_instance;
		}

		[[nodiscard]] const SignalCapabilities& signal() const noexcept
		{
			return m_signal;
		}

		[[nodiscard]] const JobCapabilities& job() const noexcept
		{
			return m_job;
		}

	private:
		explicit RobloxInternalsProfile(
			ReflectionCapabilities reflection,
			DataModelCapabilities datamodel,
			InstanceCapabilities instance,
			SignalCapabilities signal,
			JobCapabilities job) noexcept;

		const ReflectionCapabilities m_reflection;
		const DataModelCapabilities m_datamodel;
		const InstanceCapabilities m_instance;
		const SignalCapabilities m_signal;
		const JobCapabilities m_job;
	};
}

[[nodiscard]] RML_EXPORT const rml::roblox::internals::RobloxInternalsProfile& get_roblox_internals_profile();
[[nodiscard]] RML_EXPORT const rml::roblox::internals::RobloxInternalsProfile* try_get_roblox_internals_profile() noexcept;
