#include "RobloxModLoader/roblox/datamodel_layout_resolver.hpp"

#include <Zydis/Zydis.h>

#include <algorithm>
#include <array>
#include <cstring>
#include <limits>
#include <optional>
#include <vector>

namespace rml::roblox::internals
{
	namespace
	{
		constexpr std::string_view capability_name = "DataModel.RuntimeIdentity";

		constexpr std::optional<std::size_t> gpr_index(const ZydisRegister reg) noexcept
		{
			switch (reg)
			{
			case ZYDIS_REGISTER_RAX: case ZYDIS_REGISTER_EAX: case ZYDIS_REGISTER_AX: case ZYDIS_REGISTER_AL: return 0;
			case ZYDIS_REGISTER_RCX: case ZYDIS_REGISTER_ECX: case ZYDIS_REGISTER_CX: case ZYDIS_REGISTER_CL: return 1;
			case ZYDIS_REGISTER_RDX: case ZYDIS_REGISTER_EDX: case ZYDIS_REGISTER_DX: case ZYDIS_REGISTER_DL: return 2;
			case ZYDIS_REGISTER_RBX: case ZYDIS_REGISTER_EBX: case ZYDIS_REGISTER_BX: case ZYDIS_REGISTER_BL: return 3;
			case ZYDIS_REGISTER_RSP: case ZYDIS_REGISTER_ESP: case ZYDIS_REGISTER_SP: case ZYDIS_REGISTER_SPL: return 4;
			case ZYDIS_REGISTER_RBP: case ZYDIS_REGISTER_EBP: case ZYDIS_REGISTER_BP: case ZYDIS_REGISTER_BPL: return 5;
			case ZYDIS_REGISTER_RSI: case ZYDIS_REGISTER_ESI: case ZYDIS_REGISTER_SI: case ZYDIS_REGISTER_SIL: return 6;
			case ZYDIS_REGISTER_RDI: case ZYDIS_REGISTER_EDI: case ZYDIS_REGISTER_DI: case ZYDIS_REGISTER_DIL: return 7;
			case ZYDIS_REGISTER_R8:  case ZYDIS_REGISTER_R8D:  case ZYDIS_REGISTER_R8W: case ZYDIS_REGISTER_R8B: return 8;
			case ZYDIS_REGISTER_R9:  case ZYDIS_REGISTER_R9D:  case ZYDIS_REGISTER_R9W: case ZYDIS_REGISTER_R9B: return 9;
			case ZYDIS_REGISTER_R10: case ZYDIS_REGISTER_R10D: case ZYDIS_REGISTER_R10W: case ZYDIS_REGISTER_R10B: return 10;
			case ZYDIS_REGISTER_R11: case ZYDIS_REGISTER_R11D: case ZYDIS_REGISTER_R11W: case ZYDIS_REGISTER_R11B: return 11;
			case ZYDIS_REGISTER_R12: case ZYDIS_REGISTER_R12D: case ZYDIS_REGISTER_R12W: case ZYDIS_REGISTER_R12B: return 12;
			case ZYDIS_REGISTER_R13: case ZYDIS_REGISTER_R13D: case ZYDIS_REGISTER_R13W: case ZYDIS_REGISTER_R13B: return 13;
			case ZYDIS_REGISTER_R14: case ZYDIS_REGISTER_R14D: case ZYDIS_REGISTER_R14W: case ZYDIS_REGISTER_R14B: return 14;
			case ZYDIS_REGISTER_R15: case ZYDIS_REGISTER_R15D: case ZYDIS_REGISTER_R15W: case ZYDIS_REGISTER_R15B: return 15;
			default: return std::nullopt;
			}
		}

		bool checked_add(
			const std::uintptr_t base,
			const std::int64_t displacement,
			std::uintptr_t& result) noexcept
		{
			if (displacement >= 0)
			{
				const auto positive = static_cast<std::uint64_t>(displacement);
				if (positive > std::numeric_limits<std::uintptr_t>::max() - base)
					return false;
				result = base + static_cast<std::uintptr_t>(positive);
				return true;
			}

			const auto magnitude = static_cast<std::uint64_t>(-(displacement + 1)) + 1;
			if (magnitude > base)
				return false;
			result = base - magnitude;
			return true;
		}
#pragma pack(push, 1)
		struct RuntimeFunctionEntry
		{
			std::uint32_t begin_address;
			std::uint32_t end_address;
			std::uint32_t unwind_info_address;
		};
#pragma pack(pop)

		static_assert(sizeof(RuntimeFunctionEntry) == 12);

		struct FunctionBounds
		{
			std::size_t begin;
			std::size_t end;

			constexpr auto operator<=>(const FunctionBounds&) const noexcept = default;
		};

		bool decode_at(
			const ZydisDecoder& decoder,
			const std::span<const std::byte> code,
			const std::size_t offset,
			const std::size_t limit,
			ZydisDecodedInstruction& instruction,
			ZydisDecodedOperand (&operands)[ZYDIS_MAX_OPERAND_COUNT]) noexcept
		{
			if (offset >= limit || limit > code.size())
				return false;
			return ZYAN_SUCCESS(ZydisDecoderDecodeFull(
				&decoder,
				reinterpret_cast<const std::uint8_t*>(code.data() + offset),
				limit - offset,
				&instruction,
				operands)) &&
				instruction.length != 0 &&
				instruction.length <= limit - offset;
		}

		std::expected<FunctionBounds, CompatibilityFailure> find_function_bounds(
			const std::span<const std::byte> code,
			const std::uintptr_t code_address,
			const std::span<const std::byte> runtime_function_table,
			const std::uintptr_t module_address,
			const std::size_t xref_offset) noexcept
		{
			if (runtime_function_table.empty() ||
				runtime_function_table.size() % sizeof(RuntimeFunctionEntry) != 0 ||
				code_address < module_address ||
				xref_offset >= code.size())
			{
				return std::unexpected(CompatibilityFailure::invalid_address_range);
			}

			if (code.size() > std::numeric_limits<std::uintptr_t>::max() - code_address ||
				xref_offset > std::numeric_limits<std::uintptr_t>::max() - code_address)
			{
				return std::unexpected(CompatibilityFailure::invalid_address_range);
			}
			const auto code_end = code_address + code.size();
			const auto xref_address = code_address + xref_offset;
			if (xref_address < module_address ||
				xref_address - module_address > std::numeric_limits<std::uint32_t>::max())
			{
				return std::unexpected(CompatibilityFailure::invalid_address_range);
			}
			const auto xref_rva = static_cast<std::uint32_t>(xref_address - module_address);

			std::optional<FunctionBounds> matched;
			for (std::size_t offset = 0;
				 offset < runtime_function_table.size();
				 offset += sizeof(RuntimeFunctionEntry))
			{
				RuntimeFunctionEntry entry{};
				std::memcpy(&entry, runtime_function_table.data() + offset, sizeof(entry));
				if (entry.begin_address == 0 && entry.end_address == 0 && entry.unwind_info_address == 0)
					continue;
				if (entry.begin_address >= entry.end_address)
					return std::unexpected(CompatibilityFailure::invalid_address_range);
				if (xref_rva < entry.begin_address || xref_rva >= entry.end_address)
					continue;

				std::uintptr_t function_begin = 0;
				std::uintptr_t function_end = 0;
				if (!checked_add(module_address, entry.begin_address, function_begin) ||
					!checked_add(module_address, entry.end_address, function_end) ||
					function_begin < code_address ||
					function_end > code_end ||
					function_begin >= function_end)
				{
					return std::unexpected(CompatibilityFailure::invalid_address_range);
				}

				const FunctionBounds candidate{
					.begin = static_cast<std::size_t>(function_begin - code_address),
					.end = static_cast<std::size_t>(function_end - code_address),
				};
				if (matched && *matched != candidate)
					return std::unexpected(CompatibilityFailure::ambiguous_evidence);
				matched = candidate;
			}

			if (!matched)
				return std::unexpected(CompatibilityFailure::missing_signature);
			return *matched;
		}

		bool immediate_equals(
			const ZydisDecodedOperand& operand,
			const std::uint64_t expected) noexcept
		{
			return operand.type == ZYDIS_OPERAND_TYPE_IMMEDIATE &&
				operand.imm.value.u == expected;
		}

		bool matches_datamodel_type_guard(
			const ZydisDecoder& decoder,
			const std::span<const std::byte> code,
			const std::uintptr_t code_address,
			const FunctionBounds bounds,
			const std::size_t read_end,
			const ZydisRegister read_register) noexcept
		{
			const auto read_index = gpr_index(read_register);
			if (!read_index)
				return false;

			std::size_t cursor = read_end;
			ZydisDecodedInstruction sub{};
			ZydisDecodedOperand sub_operands[ZYDIS_MAX_OPERAND_COUNT]{};
			if (!decode_at(decoder, code, cursor, bounds.end, sub, sub_operands) ||
				sub.mnemonic != ZYDIS_MNEMONIC_SUB ||
				sub_operands[0].type != ZYDIS_OPERAND_TYPE_REGISTER ||
				gpr_index(sub_operands[0].reg.value) != read_index ||
				sub_operands[0].size != 32 ||
				!immediate_equals(sub_operands[1], 2))
			{
				return false;
			}
			cursor += sub.length;

			ZydisDecodedInstruction cmp{};
			ZydisDecodedOperand cmp_operands[ZYDIS_MAX_OPERAND_COUNT]{};
			if (!decode_at(decoder, code, cursor, bounds.end, cmp, cmp_operands) ||
				cmp.mnemonic != ZYDIS_MNEMONIC_CMP ||
				cmp_operands[0].type != ZYDIS_OPERAND_TYPE_REGISTER ||
				gpr_index(cmp_operands[0].reg.value) != read_index ||
				cmp_operands[0].size != 32 ||
				!immediate_equals(cmp_operands[1], 1))
			{
				return false;
			}
			cursor += cmp.length;

			ZydisDecodedInstruction branch{};
			ZydisDecodedOperand branch_operands[ZYDIS_MAX_OPERAND_COUNT]{};
			if (!decode_at(decoder, code, cursor, bounds.end, branch, branch_operands) ||
				branch.mnemonic != ZYDIS_MNEMONIC_JNBE ||
				branch_operands[0].type != ZYDIS_OPERAND_TYPE_IMMEDIATE ||
				!branch_operands[0].imm.is_relative)
			{
				return false;
			}

			std::uintptr_t branch_target = 0;
			const auto branch_address = code_address + cursor;
			if (!checked_add(
					branch_address + branch.length,
					branch_operands[0].imm.value.s,
					branch_target))
			{
				return false;
			}
			return branch_target >= code_address + bounds.begin &&
				branch_target < code_address + bounds.end;
		}

		template<std::size_t Size>
		void clear_volatile_registers(std::array<bool, Size>& state) noexcept
		{
			static_assert(Size >= 12);
			state[0] = false;
			state[1] = false;
			state[2] = false;
			for (std::size_t index = 8; index <= 11; ++index)
				state[index] = false;
		}

		struct FieldAccess
		{
			std::ptrdiff_t offset;
			std::size_t instruction;
		};

	}

	std::expected<DataModelLayoutEvidence, CompatibilityError> resolve_datamodel_layout(
		const std::span<const std::byte> executable_code,
		const std::uintptr_t code_address,
		const std::span<const std::byte> runtime_function_table,
		const std::uintptr_t module_address,
		const std::span<const std::uintptr_t> datamodel_vft_addresses,
		std::vector<CompatibilityError>* diagnostics) noexcept
	{
		auto emit = [diagnostics](CompatibilityError err) {
			if (diagnostics)
				diagnostics->push_back(err);
			return err;
		};

		if (executable_code.empty() ||
			code_address == 0 ||
			runtime_function_table.empty() ||
			module_address == 0 ||
			code_address < module_address ||
			datamodel_vft_addresses.empty())
		{
			return std::unexpected(emit(CompatibilityError{
				.capability = capability_name,
				.failure = CompatibilityFailure::invalid_address_range,
			}));
		}
		for (const auto datamodel_vft_address : datamodel_vft_addresses)
		{
			if (datamodel_vft_address == 0 || datamodel_vft_address < module_address)
			{
				return std::unexpected(emit(CompatibilityError{
					.capability = capability_name,
					.failure = CompatibilityFailure::invalid_address_range,
				}));
			}
		}
		ZydisDecoder decoder;
		if (!ZYAN_SUCCESS(ZydisDecoderInit(&decoder, ZYDIS_MACHINE_MODE_LONG_64, ZYDIS_STACK_WIDTH_64)))
		{
			return std::unexpected(emit(CompatibilityError{
				.capability = capability_name,
				.failure = CompatibilityFailure::unsupported_instruction_form,
			}));
		}

		std::vector<std::size_t> vft_xrefs;
		std::size_t cursor = 0;
		while (cursor < executable_code.size())
		{
			ZydisDecodedInstruction instruction{};
			ZydisDecodedOperand operands[ZYDIS_MAX_OPERAND_COUNT]{};
			if (!decode_at(
					decoder,
					executable_code,
					cursor,
					executable_code.size(),
					instruction,
					operands))
			{
				++cursor;
				continue;
			}

			if ((instruction.mnemonic == ZYDIS_MNEMONIC_LEA ||
				 instruction.mnemonic == ZYDIS_MNEMONIC_MOV) &&
				operands[0].type == ZYDIS_OPERAND_TYPE_REGISTER &&
				operands[1].type == ZYDIS_OPERAND_TYPE_MEMORY &&
				operands[1].mem.base == ZYDIS_REGISTER_RIP &&
				operands[1].mem.disp.has_displacement)
			{
				std::uintptr_t target_address = 0;
				const auto instruction_address = code_address + cursor;
				if (checked_add(
						instruction_address + instruction.length,
						operands[1].mem.disp.value,
						target_address) &&
					std::find(
						datamodel_vft_addresses.begin(),
						datamodel_vft_addresses.end(),
						target_address) != datamodel_vft_addresses.end())
				{
					vft_xrefs.push_back(cursor);
				}
			}

			cursor += instruction.length;
		}

		if (vft_xrefs.empty())
		{
			return std::unexpected(emit(CompatibilityError{
				.capability = capability_name,
				.failure = CompatibilityFailure::missing_signature,
			}));
		}

		bool unsupported_flow_detected = false;
		std::vector<FunctionBounds> processed_functions;
		std::vector<std::ptrdiff_t> candidate_offsets;

		for (const auto vft_position : vft_xrefs)
		{
			const auto bounds_result = find_function_bounds(
				executable_code,
				code_address,
				runtime_function_table,
				module_address,
				vft_position);
			if (!bounds_result)
			{
				if (bounds_result.error() == CompatibilityFailure::missing_signature)
					continue;
				return std::unexpected(CompatibilityError{
					.capability = capability_name,
					.failure = bounds_result.error(),
				});
			}
			const auto bounds = *bounds_result;
			if (std::find(processed_functions.begin(), processed_functions.end(), bounds) !=
				processed_functions.end())
			{
				continue;
			}
			processed_functions.push_back(bounds);

			std::array<bool, 16> is_complete_owner{};
			is_complete_owner[*gpr_index(ZYDIS_REGISTER_RCX)] = true;
			std::array<bool, 16> is_instance_subobject{};
			std::array<bool, 16> is_vft{};
			std::array<bool, 16> is_type{};
			is_type[*gpr_index(ZYDIS_REGISTER_R9)] = true;
			std::array<bool, 16> is_corrupted_type{};

			bool vft_store_verified = false;
			std::vector<FieldAccess> type_stores;
			std::vector<FieldAccess> semantic_reads;

			std::size_t current = bounds.begin;
			while (current < bounds.end)
			{
				ZydisDecodedInstruction instruction{};
				ZydisDecodedOperand operands[ZYDIS_MAX_OPERAND_COUNT]{};
				if (!decode_at(
						decoder,
						executable_code,
						current,
						bounds.end,
						instruction,
						operands))
				{
					unsupported_flow_detected = true;
					break;
				}

				const auto previous_owner = is_complete_owner;
				const auto previous_subobject = is_instance_subobject;
				const auto previous_vft = is_vft;
				const auto previous_type = is_type;
				const auto previous_corrupted_type = is_corrupted_type;

				const bool provenance_copy =
					instruction.mnemonic == ZYDIS_MNEMONIC_MOV ||
					instruction.mnemonic == ZYDIS_MNEMONIC_MOVZX ||
					instruction.mnemonic == ZYDIS_MNEMONIC_MOVSX;
				for (std::size_t index = 0; index < instruction.operand_count; ++index)
				{
					if (operands[index].type != ZYDIS_OPERAND_TYPE_REGISTER ||
						(operands[index].actions & ZYDIS_OPERAND_ACTION_MASK_WRITE) == 0)
					{
						continue;
					}
					const auto destination = gpr_index(operands[index].reg.value);
					if (!destination)
						continue;

					const bool transformed_type =
						!provenance_copy &&
						instruction.mnemonic != ZYDIS_MNEMONIC_LEA &&
						previous_type[*destination];
					is_complete_owner[*destination] = false;
					is_instance_subobject[*destination] = false;
					is_vft[*destination] = false;
					is_type[*destination] = transformed_type;
					is_corrupted_type[*destination] = transformed_type;
				}

				if (instruction.mnemonic == ZYDIS_MNEMONIC_LEA &&
					operands[0].type == ZYDIS_OPERAND_TYPE_REGISTER &&
					operands[1].type == ZYDIS_OPERAND_TYPE_MEMORY)
				{
					const auto destination = gpr_index(operands[0].reg.value);
					const auto base = gpr_index(operands[1].mem.base);
					if (destination && base && previous_owner[*base] &&
						operands[1].mem.disp.has_displacement &&
						operands[1].mem.disp.value >= 0)
					{
						if (operands[1].mem.disp.value == 0)
							is_complete_owner[*destination] = true;
						else
							is_instance_subobject[*destination] = true;
					}
				}
				else if (provenance_copy &&
					operands[0].type == ZYDIS_OPERAND_TYPE_REGISTER &&
					operands[1].type == ZYDIS_OPERAND_TYPE_REGISTER)
				{
					const auto destination = gpr_index(operands[0].reg.value);
					const auto source = gpr_index(operands[1].reg.value);
					if (destination && source)
					{
						if (instruction.mnemonic == ZYDIS_MNEMONIC_MOV)
						{
							is_complete_owner[*destination] = previous_owner[*source];
							is_instance_subobject[*destination] = previous_subobject[*source];
							is_vft[*destination] = previous_vft[*source];
						}
						is_type[*destination] = previous_type[*source];
						is_corrupted_type[*destination] =
							previous_type[*source] && previous_corrupted_type[*source];
					}
				}

				if (current == vft_position &&
					operands[0].type == ZYDIS_OPERAND_TYPE_REGISTER)
				{
					const auto destination = gpr_index(operands[0].reg.value);
					if (destination)
						is_vft[*destination] = true;
				}

				if (instruction.mnemonic == ZYDIS_MNEMONIC_MOV &&
					operands[0].type == ZYDIS_OPERAND_TYPE_MEMORY &&
					operands[1].type == ZYDIS_OPERAND_TYPE_REGISTER)
				{
					const auto base = gpr_index(operands[0].mem.base);
					const auto source = gpr_index(operands[1].reg.value);
					if (base && source)
					{
						if (is_vft[*source] &&
							(is_complete_owner[*base] || is_instance_subobject[*base]) &&
							operands[0].size == 64 &&
							operands[1].size == 64)
						{
							vft_store_verified = true;
						}
						else if (is_complete_owner[*base] &&
							is_type[*source] &&
							operands[0].size == 32 &&
							operands[1].size == 32)
						{
							if (is_corrupted_type[*source])
							{
								unsupported_flow_detected = true;
							}
							else if (operands[0].mem.disp.has_displacement &&
								operands[0].mem.disp.value > 0 &&
								operands[0].mem.disp.value <= 0x10000)
							{
								type_stores.push_back(FieldAccess{
									.offset = static_cast<std::ptrdiff_t>(
										operands[0].mem.disp.value),
									.instruction = current,
								});
							}
						}
					}
				}
				else if (instruction.mnemonic == ZYDIS_MNEMONIC_MOV &&
					operands[0].type == ZYDIS_OPERAND_TYPE_REGISTER &&
					operands[1].type == ZYDIS_OPERAND_TYPE_MEMORY)
				{
					const auto destination = gpr_index(operands[0].reg.value);
					const auto base = gpr_index(operands[1].mem.base);
					if (destination &&
						base &&
						previous_owner[*base] &&
						operands[0].size == 32 &&
						operands[1].size == 32 &&
						operands[1].mem.disp.has_displacement &&
						operands[1].mem.disp.value > 0 &&
						operands[1].mem.disp.value <= 0x10000 &&
						matches_datamodel_type_guard(
							decoder,
							executable_code,
							code_address,
							bounds,
							current + instruction.length,
							operands[0].reg.value))
					{
						semantic_reads.push_back(FieldAccess{
							.offset = static_cast<std::ptrdiff_t>(
								operands[1].mem.disp.value),
							.instruction = current,
						});
					}
				}

				if (instruction.mnemonic == ZYDIS_MNEMONIC_CALL)
				{
					clear_volatile_registers(is_complete_owner);
					clear_volatile_registers(is_instance_subobject);
					clear_volatile_registers(is_vft);
					clear_volatile_registers(is_type);
					clear_volatile_registers(is_corrupted_type);
				}

				current += instruction.length;
			}

			if (!vft_store_verified)
				continue;
			for (const auto& store : type_stores)
			{
				for (const auto& read : semantic_reads)
				{
					if (store.offset == read.offset && store.instruction < read.instruction)
						candidate_offsets.push_back(store.offset);
				}
			}
		}

		if (candidate_offsets.empty())
		{
			return std::unexpected(emit(CompatibilityError{
				.capability = capability_name,
				.failure = unsupported_flow_detected
					? CompatibilityFailure::unsupported_instruction_form
					: CompatibilityFailure::missing_signature,
			}));
		}

		std::vector<std::ptrdiff_t> unique_offsets = candidate_offsets;
		std::sort(unique_offsets.begin(), unique_offsets.end());
		unique_offsets.erase(std::unique(unique_offsets.begin(), unique_offsets.end()), unique_offsets.end());
		if (unique_offsets.size() != 1)
		{
			return std::unexpected(emit(CompatibilityError{
				.capability = capability_name,
				.failure = CompatibilityFailure::ambiguous_evidence,
			}));
		}

		return DataModelLayoutEvidence{
			.type_offset = unique_offsets.front(),
			.supporting_calls = candidate_offsets.size(),
			.matched_calls = candidate_offsets.size(),
		};
	}
}
